use crate::codec::{i420_to_rgba, nv12_to_i420, DecodedFrame, EncodedPacket, RawFrame, VideoDecoder, VideoEncoder};
use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode};
use openh264::formats::{YUVBuffer, YUVSource};
use openh264::OpenH264API;
use protocol::session::VideoCodec;

pub struct OpenH264Encoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
}

impl OpenH264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let encoder = build_openh264_encoder(width, height, fps, bitrate_kbps)?;
        Ok(Self {
            encoder,
            width,
            height,
            fps,
            bitrate_kbps,
        })
    }
}

fn build_openh264_encoder(
    _width: u32,
    _height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<Encoder> {
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps.max(1) * 2))
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
            self.encoder =
                build_openh264_encoder(self.width, self.height, self.fps, self.bitrate_kbps)?;
        }
        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        let i420 = nv12_to_i420(&frame.nv12, frame.width, frame.height);
        let yuv = YUVBuffer::from_vec(i420, frame.width as usize, frame.height as usize);
        let bitstream = self
            .encoder
            .encode(&yuv)
            .context("OpenH264 encode failed")?;
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
        if self.bitrate_kbps != bitrate_kbps {
            self.bitrate_kbps = bitrate_kbps;
            self.encoder =
                build_openh264_encoder(self.width, self.height, self.fps, self.bitrate_kbps)?;
        }
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

    fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedFrame>> {
        let maybe_yuv = self
            .decoder
            .decode(packet)
            .context("OpenH264 decode failed")?;
        let Some(yuv) = maybe_yuv else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        let (y_s, u_s, v_s) = yuv.strides();
        let rgba = i420_to_rgba(yuv.y(), yuv.u(), yuv.v(), y_s, u_s.min(v_s), w as u32, h as u32);
        Ok(Some(DecodedFrame {
            width: w as u32,
            height: h as u32,
            rgba,
        }))
    }
}
