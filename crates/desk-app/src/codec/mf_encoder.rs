use crate::codec::mf_common::{
    annexb_has_param_sets, create_nv12_sample, ensure_com_mta, ensure_mf_startup,
    extract_annexb_param_sets, pack_u32_pair, poll_event_until, read_sample_bytes,
};
use crate::codec::{EncodedPacket, RawFrame, VideoEncoder};
use anyhow::{Context, Result};
use protocol::session::VideoCodec;
use windows::core::{Interface, GUID, PWSTR, VARIANT};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main, eAVEncH265VProfile_Main_420_8,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncVideoForceKeyFrame,
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate, IMFMediaEventGenerator, IMFMediaType,
    IMFSample, IMFTransform, METransformHaveOutput, METransformNeedInput, MFCreateMediaType,
    MFCreateSample, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_HEVC,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_ASYNCMFT, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_INFO,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFSampleExtension_CleanPoint,
};
use windows::Win32::System::Com::CoTaskMemFree;

pub struct MfVideoEncoder {
    codec: VideoCodec,
    backend_name: String,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    transform: IMFTransform,
    event_gen: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    is_async: bool,
    pending_need_input: usize,
    cached_param_sets: Vec<u8>,
}

unsafe impl Send for MfVideoEncoder {}

impl MfVideoEncoder {
    pub fn new(
        codec: VideoCodec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<Self> {
        ensure_mf_startup()?;
        ensure_com_mta();

        let out_subtype = match codec {
            VideoCodec::Hevc => MFVideoFormat_HEVC,
            VideoCodec::H264 => MFVideoFormat_H264,
        };
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: out_subtype,
        };

        let flags = MFT_ENUM_FLAG(
            (MFT_ENUM_FLAG_HARDWARE.0
                | MFT_ENUM_FLAG_ASYNCMFT.0
                | MFT_ENUM_FLAG_SORTANDFILTER.0) as _,
        );

        let mut activates_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;

        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&input_info),
                Some(&output_info),
                &mut activates_ptr,
                &mut count,
            )?;
        }

        if count == 0 || activates_ptr.is_null() {
            anyhow::bail!("未找到 {:?} 硬件编码 MFT", codec);
        }

        let activates = unsafe { std::slice::from_raw_parts(activates_ptr, count as usize) };
        let mut last_err = anyhow::anyhow!("no hardware encoder MFT succeeded");
        let mut chosen = None;

        for maybe_act in activates {
            let Some(act) = maybe_act.as_ref() else {
                continue;
            };
            let friendly = read_friendly_name(act);
            match try_init_encoder_mft(act, codec, out_subtype, width, height, fps, bitrate_kbps) {
                Ok((transform, event_gen, codec_api, is_async)) => {
                    chosen = Some((friendly, transform, event_gen, codec_api, is_async));
                    break;
                }
                Err(e) => {
                    tracing::debug!("skipping encoder MFT '{}': {e}", friendly);
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

        let (friendly, transform, event_gen, codec_api, is_async) =
            chosen.ok_or(last_err)?;

        let label = if friendly.is_empty() {
            format!("MF-{:?}-HW", codec)
        } else {
            format!("MF-{:?}-HW ({})", codec, friendly)
        };

        Ok(Self {
            codec,
            backend_name: label,
            width,
            height,
            fps,
            bitrate_kbps,
            transform,
            event_gen,
            codec_api,
            is_async,
            pending_need_input: 0,
            cached_param_sets: Vec::new(),
        })
    }

    fn drain_available_output(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        unsafe {
            let stream_info: MFT_OUTPUT_STREAM_INFO = self.transform.GetOutputStreamInfo(0)?;
            let mft_provides = (stream_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

            let sample_holder: Option<IMFSample> = if mft_provides {
                None
            } else {
                let s: IMFSample = MFCreateSample()?;
                let buf_size = stream_info.cbSize.max(512 * 1024);
                let buf = windows::Win32::Media::MediaFoundation::MFCreateMemoryBuffer(buf_size)?;
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
            let res = self.transform.ProcessOutput(0, std::slice::from_mut(&mut out_buf), &mut status);

            let out_sample = std::mem::ManuallyDrop::take(&mut out_buf.pSample);
            let _events = std::mem::ManuallyDrop::take(&mut out_buf.pEvents);

            match res {
                Ok(()) => {
                    let Some(sample) = out_sample else {
                        return Ok(None);
                    };
                    let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                    let bytes = read_sample_bytes(&sample)?;
                    Ok(Some((bytes, clean_point)))
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    configure_output_type(
                        &self.transform,
                        self.codec,
                        match self.codec {
                            VideoCodec::Hevc => MFVideoFormat_HEVC,
                            VideoCodec::H264 => MFVideoFormat_H264,
                        },
                        self.width,
                        self.height,
                        self.fps,
                        self.bitrate_kbps,
                    )?;
                    Ok(None)
                }
                Err(e) => Err(e.into()),
            }
        }
    }
}

impl Drop for MfVideoEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

impl VideoEncoder for MfVideoEncoder {
    fn codec(&self) -> VideoCodec {
        self.codec
    }

    fn backend_name(&self) -> &str {
        &self.backend_name
    }

    fn encode(&mut self, frame: &RawFrame, force_keyframe: bool) -> Result<EncodedPacket> {
        ensure_com_mta();
        if frame.width != self.width || frame.height != self.height {
            *self = Self::new(
                self.codec,
                frame.width,
                frame.height,
                self.fps,
                self.bitrate_kbps,
            )?;
        }

        if force_keyframe {
            if let Some(api) = &self.codec_api {
                unsafe {
                    let v = VARIANT::from(1u32);
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v);
                }
            }
        }

        let duration_100ns = (10_000_000i64) / (self.fps.max(1) as i64);
        let sample = create_nv12_sample(&frame.nv12, frame.timestamp_us, duration_100ns)?;

        let mut collected_data = Vec::new();
        let mut is_keyframe = false;
        let is_hevc = self.codec == VideoCodec::Hevc;

        if self.is_async {
            let event_gen = self
                .event_gen
                .as_ref()
                .context("async MFT missing event generator")?
                .clone();

            let input_deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(250);
            while self.pending_need_input == 0 {
                let Some(ev) = poll_event_until(&event_gen, input_deadline)? else {
                    anyhow::bail!("MF encoder timed out waiting for input");
                };
                let ty = unsafe { ev.GetType()? };
                if ty == METransformNeedInput.0 as u32 {
                    self.pending_need_input += 1;
                } else if ty == METransformHaveOutput.0 as u32 {
                    if let Some((bytes, clean)) = self.drain_available_output()? {
                        collected_data.extend_from_slice(&bytes);
                        is_keyframe |= clean;
                    }
                }
            }

            unsafe {
                self.transform.ProcessInput(0, &sample, 0)?;
            }
            self.pending_need_input -= 1;

            // Pump events until we obtain output for this frame (or non-blocking if buffered)
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
            while collected_data.is_empty() && std::time::Instant::now() < deadline {
                let Some(ev) = poll_event_until(&event_gen, deadline)? else {
                    break;
                };
                let ty = unsafe { ev.GetType()? };
                if ty == METransformNeedInput.0 as u32 {
                    self.pending_need_input += 1;
                    // Drain any non-blocking HaveOutput right after
                    while let Ok(ev2) = unsafe { event_gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                        let ty2 = unsafe { ev2.GetType()? };
                        if ty2 == METransformNeedInput.0 as u32 {
                            self.pending_need_input += 1;
                        } else if ty2 == METransformHaveOutput.0 as u32 {
                            if let Some((bytes, clean)) = self.drain_available_output()? {
                                collected_data.extend_from_slice(&bytes);
                                is_keyframe |= clean;
                            }
                        }
                    }
                    if !collected_data.is_empty() {
                        break;
                    }
                    // If encoder genuinely wants 1 more primed input frame before first output,
                    // return empty packet this turn so pipeline keeps feeding.
                    if self.pending_need_input > 0 {
                        break;
                    }
                } else if ty == METransformHaveOutput.0 as u32 {
                    if let Some((bytes, clean)) = self.drain_available_output()? {
                        collected_data.extend_from_slice(&bytes);
                        is_keyframe |= clean;
                    }
                }
            }
        } else {
            unsafe {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_NOTACCEPTING => {}
                    Err(e) => return Err(e.into()),
                }
            }
            while let Some((bytes, clean)) = self.drain_available_output()? {
                collected_data.extend_from_slice(&bytes);
                is_keyframe |= clean;
            }
        }

        if !collected_data.is_empty() {
            if annexb_has_param_sets(&collected_data, is_hevc) {
                let ps = extract_annexb_param_sets(&collected_data, is_hevc);
                if !ps.is_empty() {
                    self.cached_param_sets = ps;
                }
                is_keyframe = true;
            } else if is_keyframe && !self.cached_param_sets.is_empty() {
                let mut prefixed = Vec::with_capacity(self.cached_param_sets.len() + collected_data.len());
                prefixed.extend_from_slice(&self.cached_param_sets);
                prefixed.extend_from_slice(&collected_data);
                collected_data = prefixed;
            }
        }

        Ok(EncodedPacket {
            data: collected_data,
            is_keyframe,
        })
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        self.bitrate_kbps = bitrate_kbps;
        if let Some(api) = &self.codec_api {
            unsafe {
                let v = VARIANT::from(bitrate_kbps.saturating_mul(1000));
                let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &v);
            }
        }
        Ok(())
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

fn try_init_encoder_mft(
    act: &IMFActivate,
    codec: VideoCodec,
    out_subtype: GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<(
    IMFTransform,
    Option<IMFMediaEventGenerator>,
    Option<ICodecAPI>,
    bool,
)> {
    unsafe {
        let transform: IMFTransform = act.ActivateObject()?;
        let attrs = transform.GetAttributes()?;
        let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
        if is_async {
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
        }

        let event_gen = if is_async {
            Some(transform.cast::<IMFMediaEventGenerator>()?)
        } else {
            None
        };

        let codec_api = transform.cast::<ICodecAPI>().ok();
        if let Some(api) = &codec_api {
            let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            let _ = api.SetValue(
                &CODECAPI_AVEncCommonRateControlMode,
                &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            let _ = api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
            let _ = api.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(bitrate_kbps.saturating_mul(1000)),
            );
        }

        configure_output_type(&transform, codec, out_subtype, width, height, fps, bitrate_kbps)?;
        configure_input_nv12(&transform, width, height, fps)?;

        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

        Ok((transform, event_gen, codec_api, is_async))
    }
}

fn configure_output_type(
    transform: &IMFTransform,
    codec: VideoCodec,
    out_subtype: GUID,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<()> {
    unsafe {
        let mt: IMFMediaType = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &out_subtype)?;
        mt.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000))?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps.max(1), 1))?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        let profile = match codec {
            VideoCodec::Hevc => eAVEncH265VProfile_Main_420_8.0 as u32,
            VideoCodec::H264 => eAVEncH264VProfile_Main.0 as u32,
        };
        let _ = mt.SetUINT32(&MF_MT_MPEG2_PROFILE, profile);

        if let Err(e) = transform.SetOutputType(0, &mt, 0) {
            // Retry without explicit profile if vendor MFT rejects profile hint
            let mt2: IMFMediaType = MFCreateMediaType()?;
            mt2.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            mt2.SetGUID(&MF_MT_SUBTYPE, &out_subtype)?;
            mt2.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000))?;
            mt2.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))?;
            mt2.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps.max(1), 1))?;
            mt2.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
            mt2.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform
                .SetOutputType(0, &mt2, 0)
                .with_context(|| format!("SetOutputType failed: {e}"))?;
        }
        Ok(())
    }
}

fn configure_input_nv12(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<()> {
    unsafe {
        let mt: IMFMediaType = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps.max(1), 1))?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        transform
            .SetInputType(0, &mt, 0)
            .context("SetInputType(NV12) failed")?;
        Ok(())
    }
}
