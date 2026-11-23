pub mod mf_common;
pub mod mf_decoder;
pub mod mf_encoder;
pub mod openh264_codec;

use anyhow::Result;
use protocol::session::VideoCodec;

#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub nv12: Vec<u8>,
    pub timestamp_us: u64,
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CodecCapabilities {
    pub encode_codecs: Vec<VideoCodec>,
    pub decode_codecs: Vec<VideoCodec>,
    pub encode_backends: Vec<String>,
    pub decode_backends: Vec<String>,
}

pub trait VideoEncoder: Send {
    fn codec(&self) -> VideoCodec;
    fn backend_name(&self) -> &str;
    fn encode(&mut self, frame: &RawFrame, force_keyframe: bool) -> Result<EncodedPacket>;
    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()>;
}

pub trait VideoDecoder: Send {
    fn codec(&self) -> VideoCodec;
    fn backend_name(&self) -> &str;
    fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedFrame>>;
}

/// Probes real hardware & software encoder/decoder support by instantiating 640x360 test MFTs
/// on an isolated worker thread so COM MTA does not pollute the GUI main thread's STA (`OleInitialize`).
pub fn probe_capabilities() -> CodecCapabilities {
    std::thread::Builder::new()
        .name("flashdesk-codec-probe".into())
        .spawn(probe_capabilities_inner)
        .ok()
        .and_then(|h| h.join().ok())
        .unwrap_or_default()
}

fn verify_encoder_produces_output(enc: &mut dyn VideoEncoder) -> bool {
    let nv12 = vec![128u8; 640 * 360 * 3 / 2];
    for i in 0..3 {
        let frame = RawFrame {
            width: 640,
            height: 360,
            nv12: nv12.clone(),
            timestamp_us: (i as u64) * 16_667,
        };
        match enc.encode(&frame, i == 0) {
            Ok(pkt) if !pkt.data.is_empty() => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
    false
}

fn probe_capabilities_inner() -> CodecCapabilities {
    let mut caps = CodecCapabilities::default();

    // Probe Encoders: HEVC HW -> H264 HW -> OpenH264 SW (verifying real output packets)
    if let Ok(mut enc) = mf_encoder::MfVideoEncoder::new(VideoCodec::Hevc, 640, 360, 60, 2000) {
        if verify_encoder_produces_output(&mut enc) {
            caps.encode_codecs.push(VideoCodec::Hevc);
            caps.encode_backends.push(enc.backend_name().to_string());
        } else {
            tracing::warn!("skipping {} (failed 640x360 output probe)", enc.backend_name());
        }
    }

    if let Ok(mut enc) = mf_encoder::MfVideoEncoder::new(VideoCodec::H264, 640, 360, 60, 2000) {
        if verify_encoder_produces_output(&mut enc) {
            caps.encode_codecs.push(VideoCodec::H264);
            caps.encode_backends.push(enc.backend_name().to_string());
        } else {
            tracing::warn!("skipping {} (failed 640x360 output probe)", enc.backend_name());
            if let Ok(mut sw) = openh264_codec::OpenH264Encoder::new(640, 360, 60, 2000) {
                if verify_encoder_produces_output(&mut sw) {
                    caps.encode_codecs.push(VideoCodec::H264);
                    caps.encode_backends.push(sw.backend_name().to_string());
                }
            }
        }
    } else if let Ok(mut enc) = openh264_codec::OpenH264Encoder::new(640, 360, 60, 2000) {
        if verify_encoder_produces_output(&mut enc) {
            caps.encode_codecs.push(VideoCodec::H264);
            caps.encode_backends.push(enc.backend_name().to_string());
        }
    }

    // Ensure OpenH264 SW fallback is noted even if HW H264 works
    if !caps.encode_backends.iter().any(|b| b.contains("OpenH264")) {
        if openh264_codec::OpenH264Encoder::new(640, 360, 60, 2000).is_ok() {
            if !caps.encode_codecs.contains(&VideoCodec::H264) {
                caps.encode_codecs.push(VideoCodec::H264);
            }
            caps.encode_backends.push("OpenH264-SW (fallback)".into());
        }
    }

    // Probe Decoders: HEVC -> H264 (MF) -> OpenH264 SW
    if let Ok(dec) = mf_decoder::MfVideoDecoder::new(VideoCodec::Hevc) {
        caps.decode_codecs.push(VideoCodec::Hevc);
        caps.decode_backends.push(dec.backend_name().to_string());
    }

    if let Ok(dec) = mf_decoder::MfVideoDecoder::new(VideoCodec::H264) {
        caps.decode_codecs.push(VideoCodec::H264);
        caps.decode_backends.push(dec.backend_name().to_string());
    } else if let Ok(dec) = openh264_codec::OpenH264Decoder::new() {
        caps.decode_codecs.push(VideoCodec::H264);
        caps.decode_backends.push(dec.backend_name().to_string());
    }

    if !caps.decode_backends.iter().any(|b| b.contains("OpenH264")) {
        if openh264_codec::OpenH264Decoder::new().is_ok() {
            if !caps.decode_codecs.contains(&VideoCodec::H264) {
                caps.decode_codecs.push(VideoCodec::H264);
            }
            caps.decode_backends.push("OpenH264-SW (fallback)".into());
        }
    }

    caps
}

pub fn create_encoder(
    codec: VideoCodec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<Box<dyn VideoEncoder>> {
    match codec {
        VideoCodec::Hevc => {
            let enc = mf_encoder::MfVideoEncoder::new(VideoCodec::Hevc, width, height, fps, bitrate_kbps)?;
            Ok(Box::new(enc))
        }
        VideoCodec::H264 => {
            match mf_encoder::MfVideoEncoder::new(VideoCodec::H264, width, height, fps, bitrate_kbps) {
                Ok(enc) => Ok(Box::new(enc)),
                Err(hw_err) => {
                    tracing::warn!("MF H.264 encoder unavailable ({hw_err}), falling back to OpenH264");
                    let sw = openh264_codec::OpenH264Encoder::new(width, height, fps, bitrate_kbps)?;
                    Ok(Box::new(sw))
                }
            }
        }
    }
}

pub fn create_decoder(codec: VideoCodec) -> Result<Box<dyn VideoDecoder>> {
    match codec {
        VideoCodec::Hevc => {
            let dec = mf_decoder::MfVideoDecoder::new(VideoCodec::Hevc)?;
            Ok(Box::new(dec))
        }
        VideoCodec::H264 => match mf_decoder::MfVideoDecoder::new(VideoCodec::H264) {
            Ok(dec) => Ok(Box::new(dec)),
            Err(hw_err) => {
                tracing::warn!("MF H.264 decoder unavailable ({hw_err}), falling back to OpenH264");
                let sw = openh264_codec::OpenH264Decoder::new()?;
                Ok(Box::new(sw))
            }
        },
    }
}

/// Converts BGRA8 (tight stride = `width * 4`) into tightly packed NV12 using BT.709 studio swing.
/// `width` and `height` must be even.
pub fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = (width & !1) as usize;
    let h = (height & !1) as usize;
    let y_size = w * h;
    let uv_size = y_size / 2;
    let mut out = vec![0u8; y_size + uv_size];
    let (y_plane, uv_plane) = out.split_at_mut(y_size);
    let stride = width as usize * 4;

    for by in 0..(h / 2) {
        let row0_idx = by * 2;
        let row1_idx = row0_idx + 1;
        let src0 = &bgra[row0_idx * stride..row0_idx * stride + w * 4];
        let src1 = &bgra[row1_idx * stride..row1_idx * stride + w * 4];
        let (dst_y0, dst_y1) = y_plane[row0_idx * w..row1_idx * w + w].split_at_mut(w);
        let dst_uv = &mut uv_plane[by * w..by * w + w];

        for bx in 0..(w / 2) {
            let x0 = bx * 2;
            let x1 = x0 + 1;
            let p00 = &src0[x0 * 4..x0 * 4 + 4];
            let p01 = &src0[x1 * 4..x1 * 4 + 4];
            let p10 = &src1[x0 * 4..x0 * 4 + 4];
            let p11 = &src1[x1 * 4..x1 * 4 + 4];

            dst_y0[x0] = rgb_to_y709(p00[2] as i32, p00[1] as i32, p00[0] as i32);
            dst_y0[x1] = rgb_to_y709(p01[2] as i32, p01[1] as i32, p01[0] as i32);
            dst_y1[x0] = rgb_to_y709(p10[2] as i32, p10[1] as i32, p10[0] as i32);
            dst_y1[x1] = rgb_to_y709(p11[2] as i32, p11[1] as i32, p11[0] as i32);

            let r_avg = (p00[2] as i32 + p01[2] as i32 + p10[2] as i32 + p11[2] as i32 + 2) >> 2;
            let g_avg = (p00[1] as i32 + p01[1] as i32 + p10[1] as i32 + p11[1] as i32 + 2) >> 2;
            let b_avg = (p00[0] as i32 + p01[0] as i32 + p10[0] as i32 + p11[0] as i32 + 2) >> 2;

            dst_uv[x0] = (((-26 * r_avg - 87 * g_avg + 112 * b_avg + 128) >> 8) + 128)
                .clamp(16, 240) as u8;
            dst_uv[x1] = (((112 * r_avg - 102 * g_avg - 10 * b_avg + 128) >> 8) + 128)
                .clamp(16, 240) as u8;
        }
    }

    out
}

#[inline(always)]
fn rgb_to_y709(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(16, 235) as u8
}

/// Converts tightly packed NV12 to planar I420 (for OpenH264).
pub fn nv12_to_i420(nv12: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = (width & !1) as usize;
    let h = (height & !1) as usize;
    let y_size = w * h;
    let uv_plane_size = y_size / 4;
    let mut out = vec![0u8; y_size + uv_plane_size * 2];
    out[..y_size].copy_from_slice(&nv12[..y_size]);

    let uv_src = &nv12[y_size..y_size + y_size / 2];
    let (u_dst, v_dst) = out[y_size..].split_at_mut(uv_plane_size);
    for (i, pair) in uv_src.chunks_exact(2).enumerate() {
        u_dst[i] = pair[0];
        v_dst[i] = pair[1];
    }
    out
}

/// Converts NV12 (optionally with stride >= width) into tightly packed RGBA8 using BT.709.
pub fn nv12_to_rgba(nv12: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let s = stride.max(width) as usize;
    let y_plane = &nv12[..s * h];
    let uv_plane = &nv12[s * h..];
    let mut rgba = vec![255u8; w * h * 4];

    for y in 0..h {
        let y_row = &y_plane[y * s..y * s + w];
        let uv_row = &uv_plane[(y / 2) * s..(y / 2) * s + (w & !1)];
        let dst_row = &mut rgba[y * w * 4..(y + 1) * w * 4];

        for x in 0..w {
            let y_val = y_row[x] as i32;
            let uv_x = (x & !1).min(uv_row.len().saturating_sub(2));
            let u_val = uv_row[uv_x] as i32;
            let v_val = uv_row[uv_x + 1] as i32;
            write_yuv709_rgba(y_val, u_val, v_val, &mut dst_row[x * 4..x * 4 + 4]);
        }
    }
    rgba
}

/// Converts planar I420 with explicit Y/UV strides into tightly packed RGBA8.
pub fn i420_to_rgba(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_stride: usize,
    uv_stride: usize,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut rgba = vec![255u8; w * h * 4];

    for y in 0..h {
        let y_row = &y_plane[y * y_stride..y * y_stride + w];
        let uv_row_idx = y / 2;
        let u_row = &u_plane[uv_row_idx * uv_stride..];
        let v_row = &v_plane[uv_row_idx * uv_stride..];
        let dst_row = &mut rgba[y * w * 4..(y + 1) * w * 4];

        for x in 0..w {
            let uv_x = x / 2;
            write_yuv709_rgba(
                y_row[x] as i32,
                u_row[uv_x] as i32,
                v_row[uv_x] as i32,
                &mut dst_row[x * 4..x * 4 + 4],
            );
        }
    }
    rgba
}

#[inline(always)]
fn write_yuv709_rgba(y: i32, u: i32, v: i32, dst: &mut [u8]) {
    let c = y - 16;
    let d = u - 128;
    let e = v - 128;
    let c1192 = 1192 * c;
    dst[0] = ((c1192 + 1836 * e + 512) >> 10).clamp(0, 255) as u8;
    dst[1] = ((c1192 - 218 * d - 546 * e + 512) >> 10).clamp(0, 255) as u8;
    dst[2] = ((c1192 + 2163 * d + 512) >> 10).clamp(0, 255) as u8;
    dst[3] = 255;
}
