pub mod mf_common;
pub mod mf_decoder;
pub mod mf_encoder;
pub mod openh264_codec;

use anyhow::Result;
use protocol::session::VideoCodec;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub nv12: Vec<u8>,
    pub timestamp_us: u64,
    /// `true` when the screen content changed since the previous capture; `false` for a forced
    /// repeat of the last picture (keep-alive refresh, pipeline flush, keyframe request).
    pub content_changed: bool,
}

/// A decoded picture still in the decoder's native YUV layout.
///
/// Colour conversion is deferred so that when the viewer falls behind it decodes every packet
/// (needed to keep the reference chain intact) but only converts the newest picture.
#[derive(Debug, Clone)]
pub enum DecodedPicture {
    Nv12 {
        width: u32,
        height: u32,
        stride: u32,
        /// Byte offset of the interleaved UV plane (`stride * coded_height`).
        uv_offset: usize,
        data: Vec<u8>,
    },
    I420 {
        width: u32,
        height: u32,
        y_stride: usize,
        uv_stride: usize,
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
    },
}

impl DecodedPicture {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            DecodedPicture::Nv12 { width, height, .. } | DecodedPicture::I420 { width, height, .. } => {
                (*width, *height)
            }
        }
    }

    /// Restricts the visible area to at most `max_w x max_h`. Decoders frequently report the
    /// 16-aligned coded size (e.g. 1920x1088 for a 1080p stream); the extra rows are padding.
    pub fn crop_to(&mut self, max_w: u32, max_h: u32) {
        let (w, h) = match self {
            DecodedPicture::Nv12 { width, height, .. } | DecodedPicture::I420 { width, height, .. } => {
                (width, height)
            }
        };
        if max_w > 0 && *w > max_w {
            *w = max_w;
        }
        if max_h > 0 && *h > max_h {
            *h = max_h;
        }
    }

    /// Converts into tightly packed opaque RGBA8; `dst.len()` must be `width * height * 4`.
    pub fn write_rgba(&self, dst: &mut [u8]) {
        match self {
            DecodedPicture::Nv12 { width, height, stride, uv_offset, data } => {
                nv12_to_rgba_into(data, *width, *height, *stride, *uv_offset, dst)
            }
            DecodedPicture::I420 { width, height, y_stride, uv_stride, y, u, v } => {
                i420_to_rgba_into(y, u, v, *y_stride, *uv_stride, *width, *height, dst)
            }
        }
    }

    pub fn to_rgba(&self) -> Vec<u8> {
        let (w, h) = self.dimensions();
        let mut out = vec![0u8; w as usize * h as usize * 4];
        self.write_rgba(&mut out);
        out
    }
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
    /// Changes the target bitrate of a running encoder. Must not force a keyframe: the ABR
    /// controller calls this several times per second.
    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()>;
}

pub trait VideoDecoder: Send {
    fn codec(&self) -> VideoCodec;
    fn backend_name(&self) -> &str;
    /// Feeds one access unit. Returns the newest picture that became available during this call
    /// (which may belong to an earlier packet for decoders with output delay).
    fn decode(&mut self, packet: &[u8]) -> Result<Option<DecodedPicture>>;
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
            content_changed: i == 0,
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

// ---------------------------------------------------------------------------------------------
// Pixel conversion. Every per-frame conversion is split into horizontal bands processed on
// scoped threads: a 2560x1440 frame is ~3.7 M pixels and the single-threaded loops alone cost
// ~10-20 ms per frame on each side, which by itself made 2K@60 unreachable.
// ---------------------------------------------------------------------------------------------

/// Worker count for a conversion over `pixels` pixels (1 for small frames: spawning isn't free).
fn worker_threads(pixels: usize) -> usize {
    static CPUS: OnceLock<usize> = OnceLock::new();
    if pixels < 640 * 360 {
        return 1;
    }
    let cpus = *CPUS.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });
    cpus.clamp(1, 8)
}

/// Runs `jobs` concurrently: all but the last on scoped threads, the last on the calling thread.
fn run_jobs<F: FnOnce() + Send>(mut jobs: Vec<F>) {
    let Some(last) = jobs.pop() else {
        return;
    };
    if jobs.is_empty() {
        last();
        return;
    }
    std::thread::scope(|s| {
        for job in jobs {
            s.spawn(job);
        }
        last();
    });
}

/// Converts BGRA8 (tight stride = `width * 4`) into tightly packed NV12 using BT.709 studio swing.
/// Odd trailing columns/rows are dropped (output is `(width & !1) x (height & !1)`).
pub fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = (width & !1) as usize;
    let h = (height & !1) as usize;
    let mut out = vec![0u8; w * h + w * h / 2];
    if w == 0 || h == 0 {
        return out;
    }
    let stride = width as usize * 4;
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let pair_rows = h / 2;
    let threads = worker_threads(w * h).min(pair_rows).max(1);
    let pairs_per_band = pair_rows.div_ceil(threads);
    let jobs: Vec<_> = y_plane
        .chunks_mut(pairs_per_band * 2 * w)
        .zip(uv_plane.chunks_mut(pairs_per_band * w))
        .enumerate()
        .map(|(i, (y_band, uv_band))| {
            move || bgra_pairs_to_nv12(bgra, stride, w, i * pairs_per_band, y_band, uv_band)
        })
        .collect();
    run_jobs(jobs);
    out
}

fn bgra_pairs_to_nv12(
    bgra: &[u8],
    stride: usize,
    w: usize,
    first_pair: usize,
    y_band: &mut [u8],
    uv_band: &mut [u8],
) {
    for (lp, (y_rows, dst_uv)) in y_band
        .chunks_exact_mut(2 * w)
        .zip(uv_band.chunks_exact_mut(w))
        .enumerate()
    {
        let row0 = (first_pair + lp) * 2;
        let src0 = &bgra[row0 * stride..row0 * stride + w * 4];
        let src1 = &bgra[(row0 + 1) * stride..(row0 + 1) * stride + w * 4];
        let (dst_y0, dst_y1) = y_rows.split_at_mut(w);
        for ((((s0, s1), y0), y1), uv) in src0
            .chunks_exact(8)
            .zip(src1.chunks_exact(8))
            .zip(dst_y0.chunks_exact_mut(2))
            .zip(dst_y1.chunks_exact_mut(2))
            .zip(dst_uv.chunks_exact_mut(2))
        {
            y0[0] = rgb_to_y709(s0[2] as i32, s0[1] as i32, s0[0] as i32);
            y0[1] = rgb_to_y709(s0[6] as i32, s0[5] as i32, s0[4] as i32);
            y1[0] = rgb_to_y709(s1[2] as i32, s1[1] as i32, s1[0] as i32);
            y1[1] = rgb_to_y709(s1[6] as i32, s1[5] as i32, s1[4] as i32);

            let r = (s0[2] as i32 + s0[6] as i32 + s1[2] as i32 + s1[6] as i32 + 2) >> 2;
            let g = (s0[1] as i32 + s0[5] as i32 + s1[1] as i32 + s1[5] as i32 + 2) >> 2;
            let b = (s0[0] as i32 + s0[4] as i32 + s1[0] as i32 + s1[4] as i32 + 2) >> 2;
            uv[0] = (((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
            uv[1] = (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
        }
    }
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
#[cfg_attr(not(test), allow(dead_code))]
pub fn nv12_to_rgba(nv12: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let s = stride.max(width) as usize;
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    nv12_to_rgba_into(nv12, width, height, stride, s * height as usize, &mut rgba);
    rgba
}

/// NV12 -> RGBA8 into `dst` with an explicit UV plane offset (coded height may exceed `height`).
pub fn nv12_to_rgba_into(
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    uv_offset: usize,
    dst: &mut [u8],
) {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return;
    }
    let s = stride.max(width) as usize;
    let threads = worker_threads(w * h).min(h).max(1);
    let rows_per = h.div_ceil(threads);
    let jobs: Vec<_> = dst[..w * h * 4]
        .chunks_mut(rows_per * w * 4)
        .enumerate()
        .map(|(i, band)| move || nv12_rows_into(data, s, uv_offset, w, i * rows_per, band))
        .collect();
    run_jobs(jobs);
}

fn nv12_rows_into(data: &[u8], s: usize, uv_offset: usize, w: usize, first_row: usize, band: &mut [u8]) {
    let w_even = w & !1;
    for (r, dst) in band.chunks_exact_mut(w * 4).enumerate() {
        let y = first_row + r;
        let y_row = &data[y * s..y * s + w];
        let uv_start = uv_offset + (y / 2) * s;
        let uv_row = &data[uv_start..uv_start + w_even];
        let (pairs_dst, tail_dst) = dst.split_at_mut(w_even * 4);
        for ((ys, uv), px) in y_row[..w_even]
            .chunks_exact(2)
            .zip(uv_row.chunks_exact(2))
            .zip(pairs_dst.chunks_exact_mut(8))
        {
            yuv_pair_to_rgba(ys[0], ys[1], uv[0], uv[1], px);
        }
        if w_even < w {
            // Odd width: the last pixel reuses the last available chroma pair (legacy behaviour).
            let (u, v) = if w_even >= 2 {
                (uv_row[w_even - 2], uv_row[w_even - 1])
            } else {
                (128, 128)
            };
            yuv_to_rgba_px(y_row[w - 1], u, v, tail_dst);
        }
    }
}

/// Converts planar I420 with explicit Y/UV strides into tightly packed RGBA8.
#[cfg_attr(not(test), allow(dead_code))]
pub fn i420_to_rgba(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_stride: usize,
    uv_stride: usize,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    i420_to_rgba_into(
        y_plane, u_plane, v_plane, y_stride, uv_stride, width, height, &mut rgba,
    );
    rgba
}

#[allow(clippy::too_many_arguments)]
pub fn i420_to_rgba_into(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_stride: usize,
    uv_stride: usize,
    width: u32,
    height: u32,
    dst: &mut [u8],
) {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 {
        return;
    }
    let threads = worker_threads(w * h).min(h).max(1);
    let rows_per = h.div_ceil(threads);
    let jobs: Vec<_> = dst[..w * h * 4]
        .chunks_mut(rows_per * w * 4)
        .enumerate()
        .map(|(i, band)| {
            move || {
                i420_rows_into(
                    y_plane, u_plane, v_plane, y_stride, uv_stride, w, i * rows_per, band,
                )
            }
        })
        .collect();
    run_jobs(jobs);
}

#[allow(clippy::too_many_arguments)]
fn i420_rows_into(
    y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    y_stride: usize,
    uv_stride: usize,
    w: usize,
    first_row: usize,
    band: &mut [u8],
) {
    let w_even = w & !1;
    for (r, dst) in band.chunks_exact_mut(w * 4).enumerate() {
        let row = first_row + r;
        let y_row = &y_plane[row * y_stride..row * y_stride + w];
        let uv_row = (row / 2) * uv_stride;
        let u_row = &u_plane[uv_row..];
        let v_row = &v_plane[uv_row..];
        let (pairs_dst, tail_dst) = dst.split_at_mut(w_even * 4);
        for (((ys, &u), &v), px) in y_row[..w_even]
            .chunks_exact(2)
            .zip(&u_row[..w_even / 2])
            .zip(&v_row[..w_even / 2])
            .zip(pairs_dst.chunks_exact_mut(8))
        {
            yuv_pair_to_rgba(ys[0], ys[1], u, v, px);
        }
        if w_even < w {
            let uv_x = (w - 1) / 2;
            yuv_to_rgba_px(y_row[w - 1], u_row[uv_x], v_row[uv_x], tail_dst);
        }
    }
}

/// BT.709 limited-range YUV -> RGBA for two horizontally adjacent pixels sharing chroma.
#[inline(always)]
fn yuv_pair_to_rgba(y0: u8, y1: u8, u: u8, v: u8, out: &mut [u8]) {
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let r_off = 1836 * e + 512;
    let g_off = -218 * d - 546 * e + 512;
    let b_off = 2163 * d + 512;
    let c0 = 1192 * (y0 as i32 - 16);
    let c1 = 1192 * (y1 as i32 - 16);
    out[0] = ((c0 + r_off) >> 10).clamp(0, 255) as u8;
    out[1] = ((c0 + g_off) >> 10).clamp(0, 255) as u8;
    out[2] = ((c0 + b_off) >> 10).clamp(0, 255) as u8;
    out[3] = 255;
    out[4] = ((c1 + r_off) >> 10).clamp(0, 255) as u8;
    out[5] = ((c1 + g_off) >> 10).clamp(0, 255) as u8;
    out[6] = ((c1 + b_off) >> 10).clamp(0, 255) as u8;
    out[7] = 255;
}

#[inline(always)]
fn yuv_to_rgba_px(y: u8, u: u8, v: u8, out: &mut [u8]) {
    let c = 1192 * (y as i32 - 16);
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    out[0] = ((c + 1836 * e + 512) >> 10).clamp(0, 255) as u8;
    out[1] = ((c - 218 * d - 546 * e + 512) >> 10).clamp(0, 255) as u8;
    out[2] = ((c + 2163 * d + 512) >> 10).clamp(0, 255) as u8;
    out[3] = 255;
}

// ---------------------------------------------------------------------------------------------
// Resolution cap: orientation-agnostic fit + area-averaging (box filter) NV12 downscale.
// ---------------------------------------------------------------------------------------------

/// Fits `w x h` inside a cap of `max_long` (longer side) x `max_short` (shorter side), keeping
/// the aspect ratio. `0` disables that bound. Never upscales; the result is even and >= 2.
pub fn fit_within(w: u32, h: u32, max_long: u32, max_short: u32) -> (u32, u32) {
    let even = |v: u32| (v & !1).max(2);
    if w == 0 || h == 0 {
        return (w, h);
    }
    let (long, short) = if w >= h { (w, h) } else { (h, w) };
    let mut scale = 1.0f64;
    if max_long > 0 {
        scale = scale.min(max_long as f64 / long as f64);
    }
    if max_short > 0 {
        scale = scale.min(max_short as f64 / short as f64);
    }
    if scale >= 1.0 {
        return (even(w), even(h));
    }
    (
        even((w as f64 * scale).round() as u32),
        even((h as f64 * scale).round() as u32),
    )
}

/// Downscales tightly packed NV12 from `sw x sh` to `dw x dh` (all even) with an area filter,
/// which keeps small text legible far better than point/bilinear sampling at 0.75x / 0.5x.
pub fn scale_nv12(nv12: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw, sh) = ((sw & !1) as usize, (sh & !1) as usize);
    let (dw, dh) = ((dw & !1) as usize, (dh & !1) as usize);
    let mut out = vec![128u8; dw * dh + dw * dh / 2];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    let (y_src, uv_src) = nv12.split_at(sw * sh);
    let (y_dst, uv_dst) = out.split_at_mut(dw * dh);
    resample_area(y_src, sw, sh, sw, 1, y_dst, dw, dh);
    resample_area(uv_src, sw / 2, sh / 2, sw, 2, uv_dst, dw / 2, dh / 2);
    out
}

const AREA_BITS: u32 = 14;
const AREA_ONE: u32 = 1 << AREA_BITS;

struct AreaTap {
    start: usize,
    weights: Vec<u32>,
}

/// Fixed-point area weights mapping `src_len` samples onto `dst_len` samples (weights per output
/// sample sum to exactly `AREA_ONE`).
fn area_taps(src_len: usize, dst_len: usize) -> Vec<AreaTap> {
    let scale = src_len as f64 / dst_len as f64;
    (0..dst_len)
        .map(|i| {
            let a = i as f64 * scale;
            let b = ((i + 1) as f64 * scale).min(src_len as f64);
            let start = (a.floor() as usize).min(src_len - 1);
            let end = (b.ceil() as usize).clamp(start + 1, src_len);
            let span = (b - a).max(f64::EPSILON);
            let mut weights: Vec<u32> = (start..end)
                .map(|j| {
                    let lo = (j as f64).max(a);
                    let hi = ((j + 1) as f64).min(b);
                    (((hi - lo).max(0.0) / span) * AREA_ONE as f64).round() as u32
                })
                .collect();
            let sum: i64 = weights.iter().map(|&w| w as i64).sum();
            if let Some(max_w) = weights.iter_mut().max() {
                *max_w = (*max_w as i64 + (AREA_ONE as i64 - sum)).max(0) as u32;
            }
            AreaTap { start, weights }
        })
        .collect()
}

/// Separable area resample of an interleaved 8-bit image with `ch` bytes per pixel.
#[allow(clippy::too_many_arguments)]
fn resample_area(
    src: &[u8],
    sw: usize,
    sh: usize,
    src_stride: usize,
    ch: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
) {
    let row_len = dw * ch;
    let htaps = area_taps(sw, dw);
    let vtaps = area_taps(sh, dh);

    // Horizontal pass: sh rows of `sw` pixels -> sh rows of `dw` pixels.
    let mut tmp = vec![0u8; sh * row_len];
    {
        let threads = worker_threads(sw * sh).min(sh).max(1);
        let rows_per = sh.div_ceil(threads);
        let htaps = &htaps;
        let jobs: Vec<_> = tmp
            .chunks_mut(rows_per * row_len)
            .enumerate()
            .map(|(bi, band)| {
                move || {
                    for (r, out_row) in band.chunks_exact_mut(row_len).enumerate() {
                        let y = bi * rows_per + r;
                        let src_row = &src[y * src_stride..y * src_stride + sw * ch];
                        for (dx, tap) in htaps.iter().enumerate() {
                            for c in 0..ch {
                                let mut acc = 0u32;
                                for (k, &w) in tap.weights.iter().enumerate() {
                                    acc += w * src_row[(tap.start + k) * ch + c] as u32;
                                }
                                out_row[dx * ch + c] =
                                    ((acc + AREA_ONE / 2) >> AREA_BITS).min(255) as u8;
                            }
                        }
                    }
                }
            })
            .collect();
        run_jobs(jobs);
    }

    // Vertical pass: sh rows -> dh rows.
    let threads = worker_threads(dw * dh).min(dh).max(1);
    let rows_per = dh.div_ceil(threads);
    let vtaps = &vtaps;
    let tmp = &tmp;
    let jobs: Vec<_> = dst[..dh * row_len]
        .chunks_mut(rows_per * row_len)
        .enumerate()
        .map(|(bi, band)| {
            move || {
                let mut acc = vec![0u32; row_len];
                for (r, out_row) in band.chunks_exact_mut(row_len).enumerate() {
                    let tap = &vtaps[bi * rows_per + r];
                    acc.fill(0);
                    for (k, &w) in tap.weights.iter().enumerate() {
                        let row = &tmp[(tap.start + k) * row_len..(tap.start + k + 1) * row_len];
                        for (a, &s) in acc.iter_mut().zip(row) {
                            *a += w * s as u32;
                        }
                    }
                    for (o, &a) in out_row.iter_mut().zip(&acc) {
                        *o = ((a + AREA_ONE / 2) >> AREA_BITS).min(255) as u8;
                    }
                }
            }
        })
        .collect();
    run_jobs(jobs);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct XorShift(u64);
    impl XorShift {
        fn next_u8(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 24) as u8
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next_u8()).collect()
        }
    }

    // ---- Reference implementations: the original single-threaded scalar code (pre-2026-11). ----

    fn ref_bgra_to_nv12(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
        let w = (width & !1) as usize;
        let h = (height & !1) as usize;
        let y_size = w * h;
        let mut out = vec![0u8; y_size + y_size / 2];
        let (y_plane, uv_plane) = out.split_at_mut(y_size);
        let stride = width as usize * 4;
        for by in 0..(h / 2) {
            let row0 = by * 2;
            let src0 = &bgra[row0 * stride..row0 * stride + w * 4];
            let src1 = &bgra[(row0 + 1) * stride..(row0 + 1) * stride + w * 4];
            for bx in 0..(w / 2) {
                let (x0, x1) = (bx * 2, bx * 2 + 1);
                let p00 = &src0[x0 * 4..x0 * 4 + 4];
                let p01 = &src0[x1 * 4..x1 * 4 + 4];
                let p10 = &src1[x0 * 4..x0 * 4 + 4];
                let p11 = &src1[x1 * 4..x1 * 4 + 4];
                y_plane[row0 * w + x0] = rgb_to_y709(p00[2] as i32, p00[1] as i32, p00[0] as i32);
                y_plane[row0 * w + x1] = rgb_to_y709(p01[2] as i32, p01[1] as i32, p01[0] as i32);
                y_plane[(row0 + 1) * w + x0] =
                    rgb_to_y709(p10[2] as i32, p10[1] as i32, p10[0] as i32);
                y_plane[(row0 + 1) * w + x1] =
                    rgb_to_y709(p11[2] as i32, p11[1] as i32, p11[0] as i32);
                let r = (p00[2] as i32 + p01[2] as i32 + p10[2] as i32 + p11[2] as i32 + 2) >> 2;
                let g = (p00[1] as i32 + p01[1] as i32 + p10[1] as i32 + p11[1] as i32 + 2) >> 2;
                let b = (p00[0] as i32 + p01[0] as i32 + p10[0] as i32 + p11[0] as i32 + 2) >> 2;
                uv_plane[by * w + x0] =
                    (((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
                uv_plane[by * w + x1] =
                    (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(16, 240) as u8;
            }
        }
        out
    }

    fn ref_px(y: i32, u: i32, v: i32) -> [u8; 4] {
        let c1192 = 1192 * (y - 16);
        let d = u - 128;
        let e = v - 128;
        [
            ((c1192 + 1836 * e + 512) >> 10).clamp(0, 255) as u8,
            ((c1192 - 218 * d - 546 * e + 512) >> 10).clamp(0, 255) as u8,
            ((c1192 + 2163 * d + 512) >> 10).clamp(0, 255) as u8,
            255,
        ]
    }

    fn ref_nv12_to_rgba(nv12: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
        let (w, h, s) = (width as usize, height as usize, stride.max(width) as usize);
        let (y_plane, uv_plane) = nv12.split_at(s * h);
        let mut rgba = vec![255u8; w * h * 4];
        for y in 0..h {
            let uv_row = &uv_plane[(y / 2) * s..(y / 2) * s + (w & !1)];
            for x in 0..w {
                let uv_x = (x & !1).min(uv_row.len().saturating_sub(2));
                let px = ref_px(
                    y_plane[y * s + x] as i32,
                    uv_row[uv_x] as i32,
                    uv_row[uv_x + 1] as i32,
                );
                rgba[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&px);
            }
        }
        rgba
    }

    #[test]
    fn parallel_bgra_to_nv12_matches_reference() {
        let mut rng = XorShift(0x1234_5678_9abc_def1);
        for &(w, h) in &[(64u32, 48u32), (333, 201), (1280, 720), (2560, 1440)] {
            let bgra = rng.bytes((w * h * 4) as usize);
            assert_eq!(bgra_to_nv12(&bgra, w, h), ref_bgra_to_nv12(&bgra, w, h), "{w}x{h}");
        }
    }

    #[test]
    fn parallel_nv12_to_rgba_matches_reference() {
        let mut rng = XorShift(0xdead_beef_cafe_f00d);
        for &(w, h, s) in &[(62u32, 34u32, 64u32), (1280, 720, 1344), (1920, 1080, 1920), (7, 5, 8)] {
            let nv12 = rng.bytes((s * h + s * h.div_ceil(2)) as usize);
            assert_eq!(
                nv12_to_rgba(&nv12, w, h, s),
                ref_nv12_to_rgba(&nv12, w, h, s),
                "{w}x{h} stride {s}"
            );
        }
    }

    #[test]
    fn parallel_i420_to_rgba_matches_reference() {
        let mut rng = XorShift(0x0bad_c0de_1357_2468);
        for &(w, h) in &[(1280u32, 720u32), (65, 33)] {
            let (ys, uvs) = (w as usize + 32, (w as usize).div_ceil(2) + 16);
            let rows_uv = (h as usize).div_ceil(2);
            let y = rng.bytes(ys * h as usize);
            let u = rng.bytes(uvs * rows_uv);
            let v = rng.bytes(uvs * rows_uv);
            let got = i420_to_rgba(&y, &u, &v, ys, uvs, w, h);
            let mut want = vec![0u8; (w * h * 4) as usize];
            for row in 0..h as usize {
                for x in 0..w as usize {
                    let px = ref_px(
                        y[row * ys + x] as i32,
                        u[(row / 2) * uvs + x / 2] as i32,
                        v[(row / 2) * uvs + x / 2] as i32,
                    );
                    want[(row * w as usize + x) * 4..][..4].copy_from_slice(&px);
                }
            }
            assert_eq!(got, want, "{w}x{h}");
        }
    }

    #[test]
    fn decoded_picture_crop_skips_coded_padding_rows() {
        // 4x4 visible inside a 4x6 coded NV12 buffer (2 padding rows, as for 1080 -> 1088).
        let (w, coded_h, stride) = (4u32, 6u32, 4u32);
        let mut data = vec![0u8; (stride * coded_h + stride * coded_h / 2) as usize];
        data[..16].fill(200); // visible luma
        data[16..24].fill(16); // padding luma (would show up as black rows)
        let uv_offset = (stride * coded_h) as usize;
        data[uv_offset..].fill(128);
        let mut pic = DecodedPicture::Nv12 { width: w, height: coded_h, stride, uv_offset, data };
        pic.crop_to(4, 4);
        assert_eq!(pic.dimensions(), (4, 4));
        let rgba = pic.to_rgba();
        assert_eq!(rgba.len(), 4 * 4 * 4);
        assert!(rgba.chunks_exact(4).all(|p| p[0] > 200 && p[3] == 255));
    }

    #[test]
    fn fit_within_respects_long_and_short_side() {
        assert_eq!(fit_within(2560, 1440, 1920, 1080), (1920, 1080));
        assert_eq!(fit_within(1440, 2560, 1920, 1080), (1080, 1920)); // portrait monitor
        assert_eq!(fit_within(1920, 1080, 1920, 1080), (1920, 1080));
        assert_eq!(fit_within(1920, 1080, 0, 0), (1920, 1080));
        assert_eq!(fit_within(5120, 1440, 1920, 1080), (1920, 540)); // panorama
        assert_eq!(fit_within(3840, 2160, 1280, 720), (1280, 720));
        assert_eq!(fit_within(1366, 768, 1280, 720), (1280, 720));
        assert_eq!(fit_within(1280, 720, 1920, 1080), (1280, 720)); // never upscales
    }

    #[test]
    fn scale_nv12_area_filter() {
        // Flat colour stays flat at an arbitrary 0.75x ratio.
        let (sw, sh) = (64u32, 48u32);
        let mut flat = vec![77u8; (sw * sh) as usize];
        flat.extend(std::iter::repeat_n([140u8, 90u8], (sw * sh / 4) as usize).flatten());
        let out = scale_nv12(&flat, sw, sh, 48, 36);
        assert_eq!(out.len(), 48 * 36 * 3 / 2);
        assert!(out[..48 * 36].iter().all(|&v| v == 77));
        assert!(out[48 * 36..].chunks_exact(2).all(|p| p == [140, 90]));

        // 2x2 checkerboard averages to mid grey at exactly 0.5x.
        let mut checker = vec![0u8; (sw * sh) as usize];
        for y in 0..sh as usize {
            for x in 0..sw as usize {
                checker[y * sw as usize + x] = if (x + y) % 2 == 0 { 200 } else { 100 };
            }
        }
        checker.extend(std::iter::repeat_n(128u8, (sw * sh / 2) as usize));
        let half = scale_nv12(&checker, sw, sh, 32, 24);
        assert!(half[..32 * 24].iter().all(|&v| v == 150), "{:?}", &half[..8]);

        // Large frame exercises the multi-threaded bands.
        let big = vec![60u8; 2560 * 1440 * 3 / 2];
        let scaled = scale_nv12(&big, 2560, 1440, 1920, 1080);
        assert_eq!(scaled.len(), 1920 * 1080 * 3 / 2);
        assert!(scaled.iter().all(|&v| v == 60));
    }
}
