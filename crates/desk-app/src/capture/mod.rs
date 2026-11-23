pub mod dxgi;
pub mod monitors;

use anyhow::Result;
use dxgi::{CaptureOutcome, ScreenCapturer};
use std::sync::Arc;

/// Anything the host capture loop can pull frames from: the real DXGI/GDI capturer, or a
/// synthetic desktop for the `--link-bench` end-to-end benchmark.
///
/// Not `Send` on purpose: sources are created *inside* the capture thread (DXGI/COM objects stay
/// on the thread that created them) through a [`FrameSourceFactory`].
pub trait FrameSource {
    /// Native (pre-resolution-cap) even frame size.
    fn dimensions(&self) -> (u32, u32);
    /// Waits up to `timeout_ms` for a new frame; with `force_frame` returns a repeat of the last
    /// picture (`content_changed == false`) instead of `Unchanged`.
    fn capture_next(&mut self, timeout_ms: u32, force_frame: bool) -> Result<CaptureOutcome>;
    fn is_gdi_fallback(&self) -> bool {
        false
    }
}

impl FrameSource for ScreenCapturer {
    fn dimensions(&self) -> (u32, u32) {
        ScreenCapturer::dimensions(self)
    }

    fn capture_next(&mut self, timeout_ms: u32, force_frame: bool) -> Result<CaptureOutcome> {
        ScreenCapturer::capture_next(self, timeout_ms, force_frame)
    }

    fn is_gdi_fallback(&self) -> bool {
        ScreenCapturer::is_gdi_fallback(self)
    }
}

/// Opens a frame source for `(monitor_index, agent_mode)` on the calling (capture) thread.
pub type FrameSourceFactory =
    Arc<dyn Fn(u32, bool) -> Result<Box<dyn FrameSource>> + Send + Sync>;

/// Opens the real desktop capturer, or the injected source when a factory is provided.
pub fn open_frame_source(
    factory: Option<&FrameSourceFactory>,
    monitor_index: u32,
    agent_mode: bool,
) -> Result<Box<dyn FrameSource>> {
    match factory {
        Some(f) => f(monitor_index, agent_mode),
        None => Ok(Box::new(ScreenCapturer::open(monitor_index, agent_mode)?)),
    }
}
