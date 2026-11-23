use crate::codec::{nv12_to_i420, DecodedPicture, EncodedPacket, RawFrame, VideoDecoder, VideoEncoder};
use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType,
};
use openh264::formats::{YUVBuffer, YUVSource};
use openh264::OpenH264API;
use openh264_sys2::{
    SBitrateInfo, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_ALL,
};
use protocol::session::VideoCodec;

pub struct OpenH264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    /// OpenH264 initialises lazily on the first `encode`; options can only be changed after that.
    initialized: bool,
}

impl OpenH264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let encoder = build_openh264_encoder(fps, bitrate_kbps)?;
        Ok(Self {
            encoder,
            width,
            height,
            fps,
            bitrate_kbps,
            initialized: false,
        })
    }

    /// Changes the bitrate of the running encoder via `SetOption` (no re-init, hence no IDR).
    fn set_bitrate_live(&mut self, bitrate_kbps: u32) -> Result<()> {
        let mut info = SBitrateInfo {
            iLayer: SPATIAL_LAYER_ALL,
            iBitrate: bitrate_kbps.saturating_mul(1000).min(i32::MAX as u32) as i32,
        };
        // Keep target <= max at every step: raise the max first, or lower the target first.
        let order = if bitrate_kbps > self.bitrate_kbps {
            [ENCODER_OPTION_MAX_BITRATE, ENCODER_OPTION_BITRATE]
        } else {
            [ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE]
        };
        for opt in order {
            let rc = unsafe {
                self.encoder
                    .raw_api()
                    .set_option(opt, std::ptr::addr_of_mut!(info).cast())
            };
            anyhow::ensure!(rc == 0, "OpenH264 SetOption({opt}) failed: {rc}");
        }
        Ok(())
    }
}

fn build_openh264_encoder(fps: u32, bitrate_kbps: u32) -> Result<Encoder> {
    // Screen-content real-time mode (text/UI friendly, cheap static regions), lowest complexity
    // for speed, no scene-change IDRs and no periodic intra frames (intra_frame_period stays at
    // 0): over a reliable transport keyframes are only produced on demand.
    // Frame skipping stays OFF even though OpenH264 warns RC_BITRATE_MODE can't hold the bitrate
    // without it: `--link-bench` showed its skipper dropping scrolling to ~38 fps while using a
    // sixth of the budget. Overshoot on saturating content is instead bounded by the host's
    // latency gate (measured: 1080p video region on 5 Mbps -> ~52 fps, p95 85 ms).
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        .complexity(Complexity::Low)
        .scene_change_detect(false)
        .skip_frames(false);

    let api = OpenH264API::from_source();
    Encoder::with_api_config(api, config).context("failed to initialize OpenH264 encoder")
}

impl VideoEncoder for OpenH264Encoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn backend_name(&self) -> &str {
        "OpenH264-SW"
    }

    fn encode(&mut self, frame: &RawFrame, force_keyframe: bool) -> Result<EncodedPacket> {
        if frame.width != self.width || frame.height != self.height {
            self.width = frame.width;
            self.height = frame.height;
            self.encoder = build_openh264_encoder(self.fps, self.bitrate_kbps)?;
            self.initialized = false;
        }
        if force_keyframe && self.initialized {
            self.encoder.force_intra_frame();
        }
        let i420 = nv12_to_i420(&frame.nv12, frame.width, frame.height);
        let yuv = YUVBuffer::from_vec(i420, frame.width as usize, frame.height as usize);
        // Deliberately no real timestamps (`encode_at`): with them OpenH264's input frame-rate
        // limiter drops frames whenever capture jitter puts two frames < 1/fps apart
        // (`--link-bench`: scrolling fell from ~59 to ~39 fps).
        let bitstream = self
            .encoder
            .encode(&yuv)
            .context("OpenH264 encode failed")?;
        self.initialized = true;
        let is_keyframe = matches!(
            bitstream.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        Ok(EncodedPacket {
            data: bitstream.to_vec(),
            is_keyframe,
        })
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        if self.bitrate_kbps == bitrate_kbps {
            return Ok(());
        }
        if self.initialized {
            if let Err(e) = self.set_bitrate_live(bitrate_kbps) {
                // Last resort: re-create (costs one IDR). Should not happen with OpenH264 2.x.
                tracing::warn!("{e:#}; re-creating OpenH264 encoder");
                self.encoder = build_openh264_encoder(self.fps, bitrate_kbps)?;
                self.initialized = false;
            }
        } else {
            self.encoder = build_openh264_encoder(self.fps, bitrate_kbps)?;
        }
        self.bitrate_kbps = bitrate_kbps;
        Ok(())
    }
}

pub struct OpenH264Decoder {
    decoder: Decoder,
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::new().context("failed to initialize OpenH264 decoder")?;
        Ok(Self { decoder })
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::H264
    }

    fn backend_name(&self) -> &str {
        "OpenH264-SW"
    }

    fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedPicture>> {
        let maybe_yuv = self
            .decoder
            .decode(packet)
            .context("OpenH264 decode failed")?;
        let Some(yuv) = maybe_yuv else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        let (y_s, u_s, v_s) = yuv.strides();
        // The planes borrow decoder-owned memory; copying them (~1 ms at 2K) is far cheaper than
        // converting pictures that a newer one will supersede before they are shown.
        Ok(Some(DecodedPicture::I420 {
            width: w as u32,
            height: h as u32,
            y_stride: y_s,
            uv_stride: u_s.min(v_s),
            y: yuv.y().to_vec(),
            u: yuv.u().to_vec(),
            v: yuv.v().to_vec(),
        }))
    }
}
