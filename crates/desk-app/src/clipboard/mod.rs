use anyhow::{Context, Result};
use arboard::{Clipboard, ImageData};
use protocol::session::ClipboardData;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;

pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct ClipboardSync {
    last_hash: Arc<Mutex<[u8; 32]>>,
    last_seq: Arc<AtomicU32>,
    pub enabled: Arc<AtomicBool>,
}

impl Default for ClipboardSync {
    fn default() -> Self {
        Self::new(true)
    }
}

impl ClipboardSync {
    pub fn new(enabled: bool) -> Self {
        Self::with_enabled_flag(Arc::new(AtomicBool::new(enabled)))
    }

    pub fn with_enabled_flag(enabled: Arc<AtomicBool>) -> Self {
        let seq = unsafe { GetClipboardSequenceNumber() };
        Self {
            last_hash: Arc::new(Mutex::new([0u8; 32])),
            last_seq: Arc::new(AtomicU32::new(seq)),
            enabled,
        }
    }

    /// Computes a deterministic SHA-256 hash for deduping text/image clipboard updates.
    pub fn hash_data(data: &ClipboardData) -> [u8; 32] {
        let mut h = Sha256::new();
        match data {
            ClipboardData::Text(t) => {
                h.update([0x01]);
                h.update(t.as_bytes());
            }
            ClipboardData::ImagePng(bytes) => {
                h.update([0x02]);
                h.update(bytes);
            }
        }
        h.finalize().into()
    }

    /// Checks whether `data` differs from the most recent applied/observed clipboard hash,
    /// and if so records it as the latest hash.
    pub fn record_if_new(&self, data: &ClipboardData) -> bool {
        let hash = Self::hash_data(data);
        let mut guard = self.last_hash.lock().unwrap();
        if *guard == hash {
            false
        } else {
            *guard = hash;
            true
        }
    }

    /// Writes remote clipboard data locally while preventing echo loops back to the peer.
    pub fn apply_remote(&self, data: &ClipboardData) -> Result<()> {
        if !self.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let hash = Self::hash_data(data);
        {
            let mut guard = self.last_hash.lock().unwrap();
            if *guard == hash {
                return Ok(());
            }
            *guard = hash;
        }

        let mut cb = Clipboard::new().context("failed to open Windows clipboard")?;
        match data {
            ClipboardData::Text(text) => {
                cb.set_text(text.clone())
                    .context("failed to set clipboard text")?;
            }
            ClipboardData::ImagePng(png_bytes) => {
                if png_bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
                    anyhow::bail!("remote clipboard image exceeds 8 MiB limit");
                }
                let (w, h, rgba) = decode_png_to_rgba(png_bytes)?;
                cb.set_image(ImageData {
                    width: w as usize,
                    height: h as usize,
                    bytes: Cow::Owned(rgba),
                })
                .context("failed to set clipboard image")?;
            }
        }

        let seq = unsafe { GetClipboardSequenceNumber() };
        self.last_seq.store(seq, Ordering::Relaxed);
        Ok(())
    }

    /// Spawns a background clipboard watcher thread that fires `on_local_change`
    /// whenever local text or image content changes.
    pub fn spawn_watcher<F>(&self, stop: Arc<AtomicBool>, on_local_change: F)
    where
        F: Fn(ClipboardData) + Send + 'static,
    {
        let sync = self.clone();
        let _ = std::thread::Builder::new()
            .name("flashdesk-clipboard".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    if !sync.enabled.load(Ordering::Relaxed) {
                        continue;
                    }
                    let seq = unsafe { GetClipboardSequenceNumber() };
                    let prev = sync.last_seq.load(Ordering::Relaxed);
                    if seq == 0 || seq == prev {
                        continue;
                    }
                    sync.last_seq.store(seq, Ordering::Relaxed);

                    let Ok(mut cb) = Clipboard::new() else {
                        continue;
                    };
                    if let Ok(text) = cb.get_text() {
                        if !text.is_empty() {
                            let data = ClipboardData::Text(text);
                            if sync.record_if_new(&data) {
                                on_local_change(data);
                            }
                            continue;
                        }
                    }
                    if let Ok(img) = cb.get_image() {
                        if let Ok(png_bytes) = encode_rgba_to_png(
                            img.width as u32,
                            img.height as u32,
                            img.bytes.as_ref(),
                        ) {
                            if png_bytes.len() <= MAX_CLIPBOARD_IMAGE_BYTES {
                                let data = ClipboardData::ImagePng(png_bytes);
                                if sync.record_if_new(&data) {
                                    on_local_change(data);
                                }
                            } else {
                                tracing::warn!(
                                    "clipboard image too large ({} bytes > 8 MiB), skipping sync",
                                    png_bytes.len()
                                );
                            }
                        }
                    }
                }
            });
    }
}

pub fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(width > 0 && height > 0, "invalid image dimensions");
    anyhow::ensure!(
        rgba.len() == (width as usize) * (height as usize) * 4,
        "RGBA buffer size mismatch"
    );
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().context("PNG write_header failed")?;
        writer
            .write_image_data(rgba)
            .context("PNG write_image_data failed")?;
    }
    Ok(out)
}

pub fn decode_png_to_rgba(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().context("PNG read_info failed")?;
    let buf_size = reader
        .output_buffer_size()
        .context("PNG decoder reported empty output buffer")?;
    let mut buf = vec![0u8; buf_size];
    let info = reader
        .next_frame(&mut buf)
        .context("PNG next_frame failed")?;
    buf.truncate(info.buffer_size());

    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for rgb in buf.chunks_exact(3) {
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for ga in buf.chunks_exact(2) {
                out.extend_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity((info.width * info.height * 4) as usize);
            for &g in &buf {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        other => anyhow::bail!("unsupported PNG color type: {other:?}"),
    };

    Ok((info.width, info.height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_rgba_encode_decode_roundtrip() {
        let (w, h) = (8u32, 6u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 30) as u8, (y * 40) as u8, 180, 255]);
            }
        }
        let png = encode_rgba_to_png(w, h, &rgba).unwrap();
        let (dw, dh, drgba) = decode_png_to_rgba(&png).unwrap();
        assert_eq!((dw, dh), (w, h));
        assert_eq!(drgba, rgba);
    }

    #[test]
    fn clipboard_dedup_prevents_echo() {
        let sync = ClipboardSync::new(true);
        let msg = ClipboardData::Text("hello FlashDesk 剪切板".into());
        assert!(sync.record_if_new(&msg));
        assert!(!sync.record_if_new(&msg));
        let msg2 = ClipboardData::Text("changed".into());
        assert!(sync.record_if_new(&msg2));
    }
}
