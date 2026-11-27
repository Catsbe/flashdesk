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
/// Larger text is not synced: a single control message that size holds up input for seconds on
/// a small relay, and anything over the 16 MiB Noise frame limit would end the session.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// Cap for the one-off push of text already on the clipboard when a session starts.
pub const MAX_INITIAL_TEXT_BYTES: usize = 256 * 1024;

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

    /// Short `(kind, bytes)` description for logs (never the content).
    pub fn describe(data: &ClipboardData) -> (&'static str, usize) {
        match data {
            ClipboardData::Text(t) => ("text", t.len()),
            ClipboardData::ImagePng(b) => ("image", b.len()),
        }
    }

    /// Local text worth sending to the peer: non-empty, at most `limit` bytes, and different from
    /// what was last sent or applied (then recorded as the latest hash).
    pub fn local_text_update(&self, text: String, limit: usize) -> Option<ClipboardData> {
        if text.is_empty() {
            return None;
        }
        if text.len() > limit {
            tracing::warn!(
                "clipboard text too large to sync ({} bytes > {limit})",
                text.len()
            );
            return None;
        }
        let data = ClipboardData::Text(text);
        self.record_if_new(&data).then_some(data)
    }

    fn local_image_update(&self, img: &ImageData) -> Option<ClipboardData> {
        let png = encode_rgba_to_png(img.width as u32, img.height as u32, img.bytes.as_ref()).ok()?;
        if png.len() > MAX_CLIPBOARD_IMAGE_BYTES {
            tracing::warn!(
                "clipboard image too large ({} bytes > 8 MiB), skipping sync",
                png.len()
            );
            return None;
        }
        let data = ClipboardData::ImagePng(png);
        self.record_if_new(&data).then_some(data)
    }

    /// Writes remote clipboard data locally while preventing echo loops back to the peer.
    pub fn apply_remote(&self, data: &ClipboardData) -> Result<()> {
        if !self.enabled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let hash = Self::hash_data(data);
        let prev_hash = {
            let mut guard = self.last_hash.lock().unwrap();
            if *guard == hash {
                return Ok(());
            }
            std::mem::replace(&mut *guard, hash)
        };

        // Another program may hold the clipboard for a moment; this runs on its own thread.
        let mut attempt = 0;
        while let Err(e) = Self::write_local(data) {
            let busy = matches!(
                e.downcast_ref::<arboard::Error>(),
                Some(arboard::Error::ClipboardOccupied)
            );
            if busy && attempt < 4 {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            // Not applied: forget the hash so the same content is not deduplicated away later.
            let mut guard = self.last_hash.lock().unwrap();
            if *guard == hash {
                *guard = prev_hash;
            }
            return Err(e);
        }

        let seq = unsafe { GetClipboardSequenceNumber() };
        self.last_seq.store(seq, Ordering::Relaxed);
        let (kind, bytes) = Self::describe(data);
        tracing::info!("clipboard: applied {kind} from peer ({bytes} bytes)");
        Ok(())
    }

    fn write_local(data: &ClipboardData) -> Result<()> {
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
        Ok(())
    }

    /// Spawns a background clipboard watcher thread that fires `on_local_change`
    /// whenever local text or image content changes.
    ///
    /// With `push_initial_text`, text already on the clipboard when the session starts is sent
    /// once right away, so "copy here, connect, paste there" works (the viewer side uses this;
    /// both sides doing it would swap the two clipboards). Images are left to the next change:
    /// a multi-megabyte PNG would hold up input on a small relay right after connecting.
    pub fn spawn_watcher<F>(&self, stop: Arc<AtomicBool>, push_initial_text: bool, on_local_change: F)
    where
        F: Fn(ClipboardData) + Send + 'static,
    {
        let sync = self.clone();
        let notify = move |data: ClipboardData| {
            let (kind, bytes) = Self::describe(&data);
            tracing::info!("clipboard: sending local {kind} to peer ({bytes} bytes)");
            on_local_change(data);
        };
        let _ = std::thread::Builder::new()
            .name("flashdesk-clipboard".into())
            .spawn(move || {
                if push_initial_text && sync.enabled.load(Ordering::Relaxed) {
                    if let Ok(text) = Clipboard::new().and_then(|mut cb| cb.get_text()) {
                        if let Some(data) = sync.local_text_update(text, MAX_INITIAL_TEXT_BYTES) {
                            notify(data);
                        }
                    }
                }
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

                    // `continue` before recording `seq` = read again next tick (clipboard held
                    // by another program); the change is not lost.
                    let Ok(mut cb) = Clipboard::new() else {
                        continue;
                    };
                    let update = match cb.get_text() {
                        Ok(text) if !text.is_empty() => {
                            sync.local_text_update(text, MAX_CLIPBOARD_TEXT_BYTES)
                        }
                        Err(arboard::Error::ClipboardOccupied) => continue,
                        _ => match cb.get_image() {
                            Ok(img) => sync.local_image_update(&img),
                            Err(arboard::Error::ClipboardOccupied) => continue,
                            Err(_) => None,
                        },
                    };
                    sync.last_seq.store(seq, Ordering::Relaxed);
                    if let Some(data) = update {
                        notify(data);
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
    fn text_updates_respect_size_limit_and_dedup() {
        let sync = ClipboardSync::new(true);
        assert!(sync.local_text_update(String::new(), 10).is_none(), "empty");
        assert!(sync.local_text_update("x".repeat(11), 10).is_none(), "over the limit");
        assert!(sync.local_text_update("x".repeat(10), 10).is_some(), "at the limit");
        assert!(sync.local_text_update("x".repeat(10), 10).is_none(), "duplicate");
        // A rejected text is not recorded: the same text still goes out under a larger limit.
        assert!(sync.local_text_update("y".repeat(20), 10).is_none());
        assert!(sync.local_text_update("y".repeat(20), 100).is_some());
    }

    #[test]
    fn text_limits_stay_well_below_the_frame_limit() {
        assert!(MAX_INITIAL_TEXT_BYTES < MAX_CLIPBOARD_TEXT_BYTES);
        // bincode adds a few bytes of enum tags and length prefixes around the text.
        assert!(MAX_CLIPBOARD_TEXT_BYTES + 1024 < protocol::framing::MAX_FRAME_LEN);
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
