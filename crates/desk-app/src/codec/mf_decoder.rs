use crate::codec::mf_common::{
    create_compressed_sample, ensure_com_mta, ensure_mf_startup, guid_eq, pack_u32_pair,
    poll_event_until, read_sample_bytes, unpack_u32_pair,
};
use crate::codec::{DecodedPicture, VideoDecoder};
use anyhow::{Context, Result};
use protocol::session::VideoCodec;
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID, PWSTR, VARIANT};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFTransform, METransformHaveOutput, METransformNeedInput, MFCreateMediaType,
    MFCreateSample, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK,
};
use windows::Win32::System::Com::CoTaskMemFree;

/// How long an async decoder may take to deliver the picture for the packet just submitted
/// before `decode` returns without it. Hardware decoders typically need 1-5 ms; returning early
/// instead leaves the picture stuck until the *next* packet arrives, which on a static desktop
/// used to mean up to the 1 s keep-alive refresh.
const OUTPUT_WAIT: Duration = Duration::from_millis(8);
/// After this many consecutive waits that time out (decoder genuinely holds a frame until the
/// next input), stop waiting so we don't burn `OUTPUT_WAIT` on every packet.
const MAX_OUTPUT_WAIT_MISSES: u32 = 8;

pub struct MfVideoDecoder {
    codec: VideoCodec,
    backend_name: String,
    transform: IMFTransform,
    event_gen: Option<IMFMediaEventGenerator>,
    is_async: bool,
    pending_need_input: usize,
    /// Packets submitted whose picture hasn't come out yet (async path bookkeeping).
    inputs_in_flight: u32,
    output_wait_misses: u32,
    width: u32,
    height: u32,
    stride: u32,
    ts_counter: u64,
}

unsafe impl Send for MfVideoDecoder {}

impl MfVideoDecoder {
    pub fn new(codec: VideoCodec) -> Result<Self> {
        ensure_mf_startup()?;
        ensure_com_mta();

        let in_subtype = match codec {
            VideoCodec::Hevc => MFVideoFormat_HEVC,
            VideoCodec::H264 => MFVideoFormat_H264,
        };
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: in_subtype,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };

        let flags = MFT_ENUM_FLAG(
            (MFT_ENUM_FLAG_HARDWARE.0
                | MFT_ENUM_FLAG_ASYNCMFT.0
                | MFT_ENUM_FLAG_SYNCMFT.0
                | MFT_ENUM_FLAG_SORTANDFILTER.0) as _,
        );

        let mut activates_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;

        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                flags,
                Some(&input_info),
                Some(&output_info),
                &mut activates_ptr,
                &mut count,
            )?;
        }

        if count == 0 || activates_ptr.is_null() {
            anyhow::bail!("未找到 {:?} 解码 MFT", codec);
        }

        let activates = unsafe { std::slice::from_raw_parts(activates_ptr, count as usize) };
        let mut last_err = anyhow::anyhow!("no decoder MFT succeeded");
        let mut chosen = None;

        for maybe_act in activates {
            let Some(act) = maybe_act.as_ref() else {
                continue;
            };
            let friendly = read_friendly_name(act);
            match try_init_decoder_mft(act, in_subtype) {
                Ok((transform, event_gen, is_async, w, h, s)) => {
                    chosen = Some((friendly, transform, event_gen, is_async, w, h, s));
                    break;
                }
                Err(e) => {
                    tracing::debug!("skipping decoder MFT '{}': {e}", friendly);
                    last_err = e;
                }
            }
        }

        unsafe {
            for item in activates {
                std::ptr::drop_in_place(item as *const _ as *mut Option<IMFActivate>);
            }
            CoTaskMemFree(Some(activates_ptr as *const _));
        }

        let (friendly, transform, event_gen, is_async, width, height, stride) =
            chosen.ok_or(last_err)?;

        let label = if friendly.is_empty() {
            format!("MF-{:?}", codec)
        } else {
            format!("MF-{:?} ({})", codec, friendly)
        };

        Ok(Self {
            codec,
            backend_name: label,
            transform,
            event_gen,
            is_async,
            pending_need_input: 0,
            inputs_in_flight: 0,
            output_wait_misses: 0,
            width,
            height,
            stride,
            ts_counter: 0,
        })
    }

    /// Handles one async MFT event: counts input requests and harvests ready pictures.
    fn handle_event(
        &mut self,
        ev: &windows::Win32::Media::MediaFoundation::IMFMediaEvent,
        latest: &mut Option<DecodedPicture>,
    ) -> Result<()> {
        let ty = unsafe { ev.GetType()? };
        if ty == METransformNeedInput.0 as u32 {
            self.pending_need_input += 1;
        } else if ty == METransformHaveOutput.0 as u32 {
            if let Some(pic) = self.pull_output()? {
                *latest = Some(pic);
            }
        }
        Ok(())
    }

    fn refresh_output_type(&mut self) -> Result<()> {
        let (w, h, s) = select_nv12_output(&self.transform)?;
        if w > 0 && h > 0 {
            self.width = w;
            self.height = h;
            self.stride = s.max(w);
        }
        Ok(())
    }

    fn pull_output(&mut self) -> Result<Option<DecodedPicture>> {
        let mut latest: Option<DecodedPicture> = None;
        loop {
            let step = unsafe {
                let stream_info: MFT_OUTPUT_STREAM_INFO = self.transform.GetOutputStreamInfo(0)?;
                let mft_provides =
                    (stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

                let sample_holder: Option<IMFSample> = if mft_provides {
                    None
                } else {
                    let s: IMFSample = MFCreateSample()?;
                    let buf_size = stream_info
                        .cbSize
                        .max(self.stride.max(1920) * self.height.max(1080) * 3 / 2);
                    let buf =
                        windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer(buf_size)?;
                    s.AddBuffer(&buf)?;
                    Some(s)
                };

                let mut out_buf = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(sample_holder),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                };
                let mut status = 0u32;
                let res = self.transform.ProcessOutput(
                    0,
                    std::slice::from_mut(&mut out_buf),
                    &mut status,
                );
                let out_sample = std::mem::ManuallyDrop::take(&mut out_buf.pSample);
                let _events = std::mem::ManuallyDrop::take(&mut out_buf.pEvents);
                (res, out_sample)
            };

            match step.0 {
                Ok(()) => {
                    if let Some(sample) = step.1 {
                        self.inputs_in_flight = self.inputs_in_flight.saturating_sub(1);
                        if let Ok(nv12) = read_sample_bytes(&sample) {
                            let need = self.stride as usize * self.height as usize * 3 / 2;
                            if self.width > 0 && self.height > 0 && nv12.len() >= need {
                                // Keep the raw NV12; the caller converts only the newest picture.
                                latest = Some(DecodedPicture::Nv12 {
                                    width: self.width,
                                    height: self.height,
                                    stride: self.stride,
                                    uv_offset: self.stride as usize * self.height as usize,
                                    data: nv12,
                                });
                            }
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.refresh_output_type()?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(latest)
    }
}

impl Drop for MfVideoDecoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

impl VideoDecoder for MfVideoDecoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn backend_name(&self) -> &str {
        &self.backend_name
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedPicture>> {
        ensure_com_mta();
        if packet.is_empty() {
            return Ok(None);
        }
        let ts = self.ts_counter;
        self.ts_counter += 16_667;
        let sample = create_compressed_sample(packet, ts)?;

        if self.is_async {
            let event_gen = self
                .event_gen
                .as_ref()
                .context("async decoder missing event generator")?
                .clone();
            let mut latest: Option<DecodedPicture> = None;

            // 1) Bounded wait for an input slot, harvesting pictures that show up meanwhile.
            let input_deadline = Instant::now() + Duration::from_millis(120);
            while self.pending_need_input == 0 {
                let Some(ev) = poll_event_until(&event_gen, input_deadline)? else {
                    anyhow::bail!("MF decoder timed out waiting for input");
                };
                self.handle_event(&ev, &mut latest)?;
            }

            unsafe {
                self.transform.ProcessInput(0, &sample, 0)?;
            }
            self.pending_need_input -= 1;
            self.inputs_in_flight = (self.inputs_in_flight + 1).min(8);

            // 2) Drain *every* ready event (a single HaveOutput per call used to let a backlog of
            //    pictures build up), then give the decoder a short, adaptive window to deliver the
            //    picture for this packet even if it already asked for more input.
            let wait = if self.output_wait_misses >= MAX_OUTPUT_WAIT_MISSES {
                Duration::ZERO
            } else {
                OUTPUT_WAIT
            };
            let soft_deadline = Instant::now() + wait;
            let hard_deadline = Instant::now() + Duration::from_millis(120);
            loop {
                while let Ok(ev) = unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    self.handle_event(&ev, &mut latest)?;
                }
                if self.inputs_in_flight == 0 {
                    break;
                }
                // Once the decoder wants more input it may legitimately hold this picture until
                // the next packet: only wait for the short soft window in that case.
                let deadline = if self.pending_need_input > 0 {
                    soft_deadline
                } else {
                    hard_deadline
                };
                if Instant::now() >= deadline {
                    break;
                }
                match poll_event_until(&event_gen, deadline)? {
                    Some(ev) => self.handle_event(&ev, &mut latest)?,
                    None => break,
                }
            }
            if !wait.is_zero() {
                if self.inputs_in_flight == 0 {
                    self.output_wait_misses = 0;
                } else {
                    self.output_wait_misses += 1;
                    if self.output_wait_misses == MAX_OUTPUT_WAIT_MISSES {
                        tracing::info!(
                            "{}: decoder holds pictures until the next packet; disabling output wait",
                            self.backend_name
                        );
                    }
                }
            }
            Ok(latest)
        } else {
            let mut drained: Option<DecodedPicture> = None;
            unsafe {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {
                        // Keep (don't drop) the picture that was blocking the input queue.
                        drained = self.pull_output()?;
                        self.transform.ProcessInput(0, &sample, 0)?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(self.pull_output()?.or(drained))
        }
    }
}

fn read_friendly_name(act: &IMFActivate) -> String {
    unsafe {
        let mut ptr = PWSTR::null();
        let mut len = 0u32;
        if act
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut ptr, &mut len)
            .is_ok()
        {
            crate::codec::mf_common::pwstr_to_string_and_free(ptr)
        } else {
            String::new()
        }
    }
}

fn try_init_decoder_mft(
    act: &IMFActivate,
    in_subtype: GUID,
) -> Result<(
    IMFTransform,
    Option<IMFMediaEventGenerator>,
    bool,
    u32,
    u32,
    u32,
)> {
    unsafe {
        let transform: IMFTransform = act.ActivateObject()?;
        let attrs = transform.GetAttributes()?;
        let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
        if is_async {
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
        }
        let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

        if let Ok(api) = transform.cast::<ICodecAPI>() {
            let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
        }

        let in_mt: IMFMediaType = MFCreateMediaType()?;
        in_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        in_mt.SetGUID(&MF_MT_SUBTYPE, &in_subtype)?;
        in_mt.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(1920, 1080))?;
        in_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        transform
            .SetInputType(0, &in_mt, 0)
            .context("decoder SetInputType failed")?;

        let (w, h, s) = select_nv12_output(&transform)?;

        let event_gen = if is_async {
            Some(transform.cast::<IMFMediaEventGenerator>()?)
        } else {
            None
        };

        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

        Ok((transform, event_gen, is_async, w, h, s.max(w)))
    }
}

fn select_nv12_output(transform: &IMFTransform) -> Result<(u32, u32, u32)> {
    unsafe {
        for idx in 0..32u32 {
            let Ok(mt) = transform.GetOutputAvailableType(0, idx) else {
                break;
            };
            let Ok(sub) = mt.GetGUID(&MF_MT_SUBTYPE) else {
                continue;
            };
            if guid_eq(&sub, &MFVideoFormat_NV12) {
                transform.SetOutputType(0, &mt, 0)?;
                let (w, h) = mt
                    .GetUINT64(&MF_MT_FRAME_SIZE)
                    .map(unpack_u32_pair)
                    .unwrap_or((1920, 1080));
                let stride = mt
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .map(|v| (v as i32).unsigned_abs())
                    .unwrap_or(w);
                return Ok((w, h, stride.max(w)));
            }
        }
        anyhow::bail!("decoder does not expose NV12 output")
    }
}
