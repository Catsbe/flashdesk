//! `flashdesk.exe --link-bench`: measures the *real* streaming pipeline end to end over a
//! simulated bandwidth-limited relay. Only the screen is synthetic.
//!
//! ```text
//! synthetic 60 Hz desktop -> host session (BGRA->NV12, encoder, flow control/ABR, Noise)
//!   -> shaped TCP proxy (shared FIFO bottleneck @ N Mbps, one-way delay, ~1 s deep buffer like a
//!      small cloud relay with big socket buffers)
//!   -> viewer session (Noise, decoder, RGBA) -> marker read-back
//! ```
//!
//! Every changed picture carries a 16-bit marker (12-bit counter + 4-bit checksum drawn as 16x16
//! black/white blocks in the top-left corner). Reading it back from the decoded picture gives the
//! exact "screen changed on the host" -> "picture decoded on the viewer" latency.

use crate::capture::dxgi::CaptureOutcome;
use crate::capture::{FrameSource, FrameSourceFactory};
use crate::codec::{bgra_to_nv12, probe_capabilities, RawFrame};
use crate::net::direct::serve_direct_connection;
use crate::net::host_session::{HostSessionOptions, PendingHostChannels};
use crate::net::viewer_session::{start_direct_viewer_session, ViewerCommand};
use anyhow::Result;
use eframe::egui::ColorImage;
use protocol::auth::{DeviceBundle, RootKeypair};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener as StdListener, TcpStream as StdStream};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct BenchConfig {
    pub link_mbps: f64,
    pub one_way_ms: u64,
    /// Bottleneck buffer depth before the proxy stops reading (relay-like bufferbloat).
    pub queue_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_kbps: u32,
    /// Resolution cap by height (0 = native).
    pub res_cap_h: u32,
    pub scroll_secs: u64,
    /// Seconds with a large full-motion "video" region (saturates a 5 Mbps link).
    pub video_secs: u64,
    pub typing_secs: u64,
    /// Viewer sends no frame acks: emulates the pre-flow-control transport for comparison.
    pub no_ack: bool,
}

const PHASE_SCROLL: u8 = 0;
const PHASE_TYPING: u8 = 1;
const PHASE_VIDEO: u8 = 2;
const MARKER_BLOCK: usize = 16;
const MARKER_BITS: usize = 16;
const SCROLL_PX_PER_REFRESH: u32 = 4;
const TYPING_INTERVAL: Duration = Duration::from_millis(150);
const SCROLL_WARMUP: Duration = Duration::from_secs(3);
const TYPING_WARMUP: Duration = Duration::from_secs(1);
const VIDEO_WARMUP: Duration = Duration::from_secs(2);

struct Shared {
    phase: AtomicU8,
    /// 12-bit marker id -> instant the synthetic display presented that picture.
    presented: Mutex<HashMap<u16, Instant>>,
}

fn marker_word(id: u16) -> u16 {
    let v = id & 0x0FFF;
    let sum = (v & 0xF) + ((v >> 4) & 0xF) + ((v >> 8) & 0xF);
    (v << 4) | (sum & 0xF)
}

fn marker_id(word: u16) -> Option<u16> {
    let v = word >> 4;
    let sum = (v & 0xF) + ((v >> 4) & 0xF) + ((v >> 8) & 0xF);
    ((sum & 0xF) == (word & 0xF)).then_some(v)
}

/// `a` is newer than `b` on the 12-bit wrapping counter.
fn marker_newer(a: u16, b: u16) -> bool {
    let d = a.wrapping_sub(b) & 0x0FFF;
    d != 0 && d < 0x0800
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u32
    }
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.next() % (hi - lo).max(1)
    }
}

fn fill_rect(buf: &mut [u8], stride_px: usize, x0: usize, y0: usize, w: usize, h: usize, bgra: [u8; 4]) {
    let rows = buf.len() / (stride_px * 4);
    for y in y0..(y0 + h).min(rows) {
        let row = &mut buf[y * stride_px * 4..(y + 1) * stride_px * 4];
        for x in x0..(x0 + w).min(stride_px) {
            row[x * 4..x * 4 + 4].copy_from_slice(&bgra);
        }
    }
}

/// A text-heavy "document" page (high-frequency content, the hard case for screen encoders).
fn make_document(w: usize, h: usize) -> Vec<u8> {
    let mut doc = vec![0u8; w * h * 4];
    fill_rect(&mut doc, w, 0, 0, w, h, [250, 250, 250, 255]);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut y = 24usize;
    let mut line = 0u32;
    while y + 14 < h {
        if line % 18 == 17 && y + 140 < h {
            // An "image" block with a gradient.
            for yy in 0..120 {
                let c = (60 + yy * 3 / 2) as u8;
                fill_rect(&mut doc, w, 60, y + yy, w / 3, 1, [c, 120, 255 - c, 255]);
            }
            y += 140;
            line += 1;
            continue;
        }
        let line_end = rng.range((w as u32 * 3) / 5, (w as u32) - 40) as usize;
        let mut x = 40usize;
        while x + 12 < line_end {
            let glyphs = rng.range(2, 9);
            for _ in 0..glyphs {
                let gw = rng.range(4, 9) as usize;
                let gh = rng.range(8, 13) as usize;
                fill_rect(&mut doc, w, x, y + (12 - gh), gw, gh, [40, 40, 40, 255]);
                x += gw + 2;
            }
            x += 7;
        }
        y += 22;
        line += 1;
    }
    doc
}

struct SyntheticDesktop {
    w: usize,
    h: usize,
    /// The document twice, stacked, so any `h`-row window at offset < h is valid.
    doc2: Vec<u8>,
    frame: Vec<u8>,
    scroll: usize,
    counter: u16,
    typed: usize,
    in_typing: bool,
    video_t: u32,
    noise: Rng,
    refresh: Duration,
    next_present: Instant,
    last_typing: Instant,
    last_nv12: Option<Vec<u8>>,
    epoch: Instant,
    shared: Arc<Shared>,
}

impl SyntheticDesktop {
    fn new(w: u32, h: u32, hz: u32, epoch: Instant, shared: Arc<Shared>) -> Self {
        let (w, h) = (w as usize, h as usize);
        let doc = make_document(w, h);
        let mut doc2 = doc.clone();
        doc2.extend_from_slice(&doc);
        let now = Instant::now();
        Self {
            w,
            h,
            doc2,
            frame: vec![0u8; w * h * 4],
            scroll: 0,
            counter: 0,
            typed: 0,
            in_typing: false,
            video_t: 0,
            noise: Rng(0x2545_F491_4F6C_DD1D),
            refresh: Duration::from_micros(1_000_000 / hz.max(1) as u64),
            next_present: now,
            last_typing: now,
            last_nv12: None,
            epoch,
            shared,
        }
    }

    fn compose_scrolled(&mut self) {
        let row = self.w * 4;
        self.frame
            .copy_from_slice(&self.doc2[self.scroll * row..(self.scroll + self.h) * row]);
    }

    fn type_glyph(&mut self) {
        // An "editor" box near the bottom; glyphs appear one by one like typing.
        let (box_x, box_y) = (40usize, self.h.saturating_sub(220));
        let per_line = (self.w - 2 * box_x) / 12;
        let (col, line) = (self.typed % per_line, (self.typed / per_line) % 10);
        if col == 0 && line == 0 {
            fill_rect(&mut self.frame, self.w, box_x - 8, box_y - 8, self.w - 2 * box_x + 16, 10 * 18 + 16, [255, 255, 255, 255]);
        }
        let h = 9 + (self.typed % 5);
        fill_rect(&mut self.frame, self.w, box_x + col * 12, box_y + line * 18 + (14 - h), 9, h, [30, 30, 30, 255]);
        self.typed += 1;
    }

    /// A large full-motion region (moving interference pattern + film grain): the "watching a
    /// video / playing a game remotely" case that saturates a 5 Mbps link.
    fn draw_video(&mut self, refreshes: u32) {
        self.video_t += refreshes;
        let t = self.video_t as i32;
        let (x0, y0) = (self.w / 4, self.h / 4);
        let (vw, vh) = (self.w / 2, self.h / 2);
        for y in 0..vh {
            let row = &mut self.frame[((y0 + y) * self.w + x0) * 4..((y0 + y) * self.w + x0 + vw) * 4];
            let yy = y as i32;
            for (x, px) in row.chunks_exact_mut(4).enumerate() {
                let xx = x as i32;
                let a = ((xx * 3 + t * 5) ^ (yy * 2 - t * 3)) & 0xFF;
                let b = ((xx + yy + t * 7) >> 1) & 0xFF;
                let grain = (self.noise.next() & 0x1F) as i32 - 16;
                px[0] = (a + grain).clamp(0, 255) as u8;
                px[1] = ((a + b) / 2 + grain).clamp(0, 255) as u8;
                px[2] = (b + grain).clamp(0, 255) as u8;
                px[3] = 255;
            }
        }
    }

    fn draw_marker(&mut self) {
        let word = marker_word(self.counter);
        for bit in 0..MARKER_BITS {
            let on = (word >> (MARKER_BITS - 1 - bit)) & 1 == 1;
            let c = if on { 255 } else { 0 };
            fill_rect(&mut self.frame, self.w, bit * MARKER_BLOCK, 0, MARKER_BLOCK, MARKER_BLOCK, [c, c, c, 255]);
        }
    }

    /// Advances the synthetic screen to display refresh `present_at` (`elapsed_refreshes` >= 1
    /// refreshes since the last call). Returns the new picture if the content changed.
    fn advance(&mut self, present_at: Instant, elapsed_refreshes: u32) -> Option<RawFrame> {
        let phase = self.shared.phase.load(Ordering::Relaxed);
        let changed = if phase == PHASE_SCROLL {
            self.in_typing = false;
            self.scroll = (self.scroll + (SCROLL_PX_PER_REFRESH * elapsed_refreshes) as usize) % self.h;
            self.compose_scrolled();
            true
        } else if phase == PHASE_VIDEO {
            self.in_typing = false;
            self.draw_video(elapsed_refreshes);
            true
        } else if !self.in_typing {
            self.in_typing = true;
            self.compose_scrolled();
            self.last_typing = present_at;
            true
        } else if present_at.duration_since(self.last_typing) >= TYPING_INTERVAL {
            while present_at.duration_since(self.last_typing) >= TYPING_INTERVAL {
                self.type_glyph();
                self.last_typing += TYPING_INTERVAL;
            }
            true
        } else {
            false
        };
        if !changed {
            return None;
        }
        self.counter = (self.counter + 1) & 0x0FFF;
        self.draw_marker();
        self.shared
            .presented
            .lock()
            .unwrap()
            .insert(self.counter, present_at);
        let nv12 = bgra_to_nv12(&self.frame, self.w as u32, self.h as u32);
        self.last_nv12 = Some(nv12.clone());
        Some(RawFrame {
            width: self.w as u32,
            height: self.h as u32,
            nv12,
            timestamp_us: present_at.duration_since(self.epoch).as_micros() as u64,
            content_changed: true,
        })
    }
}

impl FrameSource for SyntheticDesktop {
    fn dimensions(&self) -> (u32, u32) {
        (self.w as u32, self.h as u32)
    }

    /// DXGI-like semantics: wait up to `timeout_ms` for a display refresh that changed the
    /// content; otherwise return a repeat when forced.
    fn capture_next(&mut self, timeout_ms: u32, force_frame: bool) -> Result<CaptureOutcome> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            let now = Instant::now();
            if now >= self.next_present {
                let mut refreshes = 0u32;
                let mut present_at = self.next_present;
                while self.next_present <= now {
                    present_at = self.next_present;
                    self.next_present += self.refresh;
                    refreshes += 1;
                }
                if let Some(frame) = self.advance(present_at, refreshes) {
                    return Ok(CaptureOutcome::Frame(frame));
                }
                continue;
            }
            if now >= deadline {
                break;
            }
            crate::timing::sleep_until(self.next_present.min(deadline));
        }
        if force_frame {
            if let Some(nv12) = &self.last_nv12 {
                return Ok(CaptureOutcome::Frame(RawFrame {
                    width: self.w as u32,
                    height: self.h as u32,
                    nv12: nv12.clone(),
                    timestamp_us: self.epoch.elapsed().as_micros() as u64,
                    content_changed: false,
                }));
            }
        }
        Ok(CaptureOutcome::Unchanged)
    }
}

/// Shared FIFO bottleneck (the relay's egress) with a finite but deep buffer.
struct Link {
    bytes_per_sec: f64,
    one_way: Duration,
    max_queue: Duration,
    free_at: Mutex<Instant>,
}

impl Link {
    /// Reserves bottleneck time for `bytes`; waits while the queue is deeper than `max_queue`
    /// (that back-pressures the sender's TCP like a full relay buffer). Returns the instant the
    /// chunk is fully delivered at the far end.
    fn transmit(&self, bytes: usize) -> Instant {
        loop {
            let now = Instant::now();
            let mut free = self.free_at.lock().unwrap();
            let start = (*free).max(now);
            if start.duration_since(now) <= self.max_queue {
                *free = start + Duration::from_secs_f64(bytes as f64 / self.bytes_per_sec);
                return *free + self.one_way;
            }
            drop(free);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Forwards `src` -> `dst`: through the shaped `link`, or with a plain `delay`.
fn spawn_pump(mut src: StdStream, mut dst: StdStream, link: Option<Arc<Link>>, delay: Duration) {
    let (tx, rx) = std_mpsc::channel::<(Instant, Vec<u8>)>();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let at = match &link {
                Some(l) => l.transmit(n),
                None => Instant::now() + delay,
            };
            if tx.send((at, buf[..n].to_vec())).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        while let Ok((at, data)) = rx.recv() {
            let now = Instant::now();
            if at > now {
                std::thread::sleep(at - now);
            }
            if dst.write_all(&data).is_err() {
                break;
            }
        }
        let _ = dst.shutdown(Shutdown::Both);
    });
}

fn read_marker(image: &ColorImage, src_w: usize, src_h: usize) -> Option<u16> {
    let [w, h] = image.size;
    if src_w < MARKER_BITS * MARKER_BLOCK || h < 8 {
        return None;
    }
    let (sx, sy) = (w as f64 / src_w as f64, h as f64 / src_h as f64);
    let y = ((MARKER_BLOCK as f64 / 2.0) * sy) as usize;
    let mut word = 0u16;
    for bit in 0..MARKER_BITS {
        let x = (((bit * MARKER_BLOCK) as f64 + MARKER_BLOCK as f64 / 2.0) * sx) as usize;
        let c = image.pixels.get(y * w + x)?;
        let luma = c.r() as u32 + c.g() as u32 + c.b() as u32;
        word = (word << 1) | u16::from(luma > 3 * 128);
    }
    marker_id(word)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn summarize(label: &str, mut lat: Vec<f64>, secs: f64) -> (f64, f64, f64) {
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let fps = lat.len() as f64 / secs.max(0.001);
    let avg = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
    let p95 = percentile(&lat, 0.95);
    println!(
        "  {label}: {} pictures in {:.1}s = {:.1} fps | capture->decoded latency avg {:.0} ms, p50 {:.0} ms, p95 {:.0} ms, max {:.0} ms",
        lat.len(),
        secs,
        fps,
        avg,
        percentile(&lat, 0.5),
        p95,
        lat.last().copied().unwrap_or(f64::NAN)
    );
    (fps, avg, p95)
}

/// Runs the benchmark; returns `Ok(true)` when the pass criteria are met.
pub fn run_link_bench(cfg: BenchConfig) -> Result<bool> {
    let (w, h) = (cfg.width & !1, cfg.height & !1);
    anyhow::ensure!(w >= 320 && h >= 240, "bench resolution too small");
    println!("=== FlashDesk link benchmark ===");
    println!(
        "  link: {:.1} Mbps bottleneck, {} ms one-way ({} ms RTT), {} ms relay buffer | desktop {}x{} @ {} Hz | bitrate cap {} kbps | resolution cap {} | frame acks: {}",
        cfg.link_mbps,
        cfg.one_way_ms,
        cfg.one_way_ms * 2,
        cfg.queue_ms,
        w,
        h,
        cfg.fps,
        cfg.max_kbps,
        if cfg.res_cap_h == 0 { "native".to_string() } else { format!("{}p", cfg.res_cap_h) },
        if cfg.no_ack { "OFF (legacy transport)" } else { "on" }
    );

    let caps = probe_capabilities();
    println!("  encoders: {:?}", caps.encode_backends);
    println!("  decoders: {:?}", caps.decode_backends);
    anyhow::ensure!(!caps.encode_codecs.is_empty(), "no working video encoder");

    let root = RootKeypair::generate();
    let host_bundle = DeviceBundle {
        root_pubkey: root.pubkey_bytes(),
        identity: root.sign_device_cert(Uuid::new_v4(), "BenchHost", 3600),
        relay_nodes: Vec::new(),
    };
    let viewer_bundle = DeviceBundle {
        root_pubkey: root.pubkey_bytes(),
        identity: root.sign_device_cert(Uuid::new_v4(), "BenchViewer", 3600),
        relay_nodes: Vec::new(),
    };

    let shared = Arc::new(Shared {
        phase: AtomicU8::new(PHASE_SCROLL),
        presented: Mutex::new(HashMap::new()),
    });
    let epoch = Instant::now();
    let factory: FrameSourceFactory = {
        let shared = Arc::clone(&shared);
        let hz = cfg.fps;
        Arc::new(move |_monitor, _agent| {
            Ok(Box::new(SyntheticDesktop::new(w, h, hz, epoch, Arc::clone(&shared)))
                as Box<dyn FrameSource>)
        })
    };

    crate::net::viewer_session::set_frame_acks_disabled(cfg.no_ack);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = rt.enter();

    // Host: same accept path as LAN direct connections, with the synthetic desktop injected.
    let host_listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
    let host_addr = host_listener.local_addr()?;
    let host_opts = HostSessionOptions {
        caps: caps.clone(),
        bundle: host_bundle,
        revoked_ids: Vec::new(),
        agent_mode: false,
        auto_vdd_on_headless: false,
        pending_channels: Arc::new(PendingHostChannels::new()),
        frame_source: Some(factory),
    };
    rt.spawn(async move {
        while let Ok((stream, _)) = host_listener.accept().await {
            tokio::spawn(serve_direct_connection(stream, host_opts.clone()));
        }
    });

    // Shaped "relay": host->viewer shares one bottleneck; viewer->host only gets the delay.
    let link = Arc::new(Link {
        bytes_per_sec: cfg.link_mbps * 1_000_000.0 / 8.0,
        one_way: Duration::from_millis(cfg.one_way_ms),
        max_queue: Duration::from_millis(cfg.queue_ms),
        free_at: Mutex::new(Instant::now()),
    });
    let proxy = StdListener::bind("127.0.0.1:0")?;
    let proxy_addr = proxy.local_addr()?;
    let one_way = Duration::from_millis(cfg.one_way_ms);
    std::thread::spawn(move || {
        for viewer_side in proxy.incoming().flatten() {
            let Ok(host_side) = StdStream::connect(host_addr) else {
                continue;
            };
            let _ = viewer_side.set_nodelay(true);
            let _ = host_side.set_nodelay(true);
            let (Ok(v2), Ok(h2)) = (viewer_side.try_clone(), host_side.try_clone()) else {
                continue;
            };
            spawn_pump(host_side, viewer_side, Some(Arc::clone(&link)), one_way);
            spawn_pump(v2, h2, None, one_way);
        }
    });

    // The viewer "repaints" after every decoded picture: use that as an event-driven wake-up so
    // the measurement doesn't depend on sleep granularity (coarse on some machines/VMs).
    let (picture_tx, picture_rx) = std_mpsc::channel::<()>();
    let viewer = start_direct_viewer_session(
        viewer_bundle,
        Vec::new(),
        proxy_addr.to_string(),
        caps,
        cfg.fps,
        cfg.max_kbps,
        crate::config::res_cap_for_height(cfg.res_cap_h),
        Arc::new(move || {
            let _ = picture_tx.send(());
        }),
    );

    println!(
        "\n  {:>4} {:<6} {:>5} {:>8} {:>8} {:>13} {:>13} {:>9} {:>7} {:>7} {:>7} {:>15}",
        "t(s)", "phase", "fps", "cap/sent", "recv", "ABR sent/tgt", "frame delay", "gate/busy", "capture", "encode", "decode", "latency avg/p95"
    );
    let scroll_end = Duration::from_secs(cfg.scroll_secs);
    let video_end = scroll_end + Duration::from_secs(cfg.video_secs);
    let total = video_end + Duration::from_secs(cfg.typing_secs);
    let start = Instant::now();
    let mut last_id: Option<u16> = None;
    let mut sec_lat: Vec<f64> = Vec::new();
    let mut scroll_lat: Vec<f64> = Vec::new();
    let mut video_lat: Vec<f64> = Vec::new();
    let mut typing_lat: Vec<f64> = Vec::new();
    let mut next_report = Duration::from_secs(1);
    let mut first_picture: Option<Duration> = None;

    while start.elapsed() < total {
        let t = start.elapsed();
        let phase = if t < scroll_end {
            PHASE_SCROLL
        } else if t < video_end {
            PHASE_VIDEO
        } else {
            PHASE_TYPING
        };
        shared.phase.store(phase, Ordering::Relaxed);

        if let Some(frame) = viewer.latest_frame.lock().unwrap().take() {
            first_picture.get_or_insert(t);
            if let Some(id) = read_marker(&frame.image, w as usize, h as usize) {
                if last_id.is_none_or(|l| marker_newer(id, l)) {
                    last_id = Some(id);
                    let presented = shared.presented.lock().unwrap().get(&id).copied();
                    if let Some(presented) = presented {
                        let ms = presented.elapsed().as_secs_f64() * 1000.0;
                        if ms < 10_000.0 {
                            sec_lat.push(ms);
                            if phase == PHASE_SCROLL && t >= SCROLL_WARMUP {
                                scroll_lat.push(ms);
                            } else if phase == PHASE_VIDEO && t >= scroll_end + VIDEO_WARMUP {
                                video_lat.push(ms);
                            } else if phase == PHASE_TYPING && t >= video_end + TYPING_WARMUP {
                                typing_lat.push(ms);
                            }
                        }
                    }
                }
            }
        }

        if t >= next_report {
            next_report += Duration::from_secs(1);
            let hud = viewer.hud.lock().unwrap().clone();
            let host = hud.host.clone().unwrap_or_default();
            sec_lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "  {:>4} {:<6} {:>5.1} {:>3}/{:<4.0} {:>5.2}Mbps {:>5.2}/{:<5.2}M {:>6}/{:<4}ms {:>4}/{:<4} {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>7.0}/{:<5.0}ms",
                next_report.as_secs() - 1,
                match phase {
                    PHASE_SCROLL => "scroll",
                    PHASE_VIDEO => "video",
                    _ => "typing",
                },
                hud.fps,
                host.captured_frames,
                host.sent_fps,
                hud.bitrate_mbps,
                host.sent_kbps as f64 / 1000.0,
                host.target_kbps as f64 / 1000.0,
                host.avg_delay_ms,
                host.max_delay_ms,
                host.paused_ticks,
                host.sender_busy_ticks,
                host.capture_ms,
                host.encode_ms,
                hud.decode_ms,
                sec_lat.iter().sum::<f64>() / sec_lat.len().max(1) as f64,
                percentile(&sec_lat, 0.95),
            );
            sec_lat.clear();
        }

        if let Some(reason) = viewer.disconnected_reason.lock().unwrap().clone() {
            anyhow::bail!("session ended early: {reason}");
        }
        let _ = picture_rx.recv_timeout(Duration::from_millis(20));
    }

    let codec = viewer.hud.lock().unwrap().codec_label.clone();
    viewer.send(ViewerCommand::Disconnect);
    std::thread::sleep(Duration::from_millis(300));

    println!("\n=== Results ({codec}) ===");
    println!(
        "  first picture after {} ms",
        first_picture.map_or(-1, |d| d.as_millis() as i64)
    );
    let scroll_secs = cfg.scroll_secs as f64 - SCROLL_WARMUP.as_secs_f64();
    let video_secs = cfg.video_secs as f64 - VIDEO_WARMUP.as_secs_f64();
    let typing_secs = cfg.typing_secs as f64 - TYPING_WARMUP.as_secs_f64();
    let (scroll_fps, _, scroll_p95) =
        summarize("scrolling text (continuous motion)", scroll_lat, scroll_secs);
    let video = (cfg.video_secs > 0).then(|| {
        summarize(
            "full-motion video region (saturates the link)",
            video_lat,
            video_secs,
        )
    });
    let (_, typing_avg, typing_p95) =
        summarize("typing (sparse updates every 150 ms)", typing_lat, typing_secs);

    // Fixed before the first run: fluid = >= 90 % of the target frame rate while scrolling and
    // >= 75 % while a big video region saturates the link; responsive = p95 capture->decoded
    // latency within one round trip + 120 ms (+80 ms more under saturation).
    let latency_ceiling = (cfg.one_way_ms * 2 + 120) as f64;
    let video_ceiling = latency_ceiling + 80.0;
    let scroll_ok = scroll_fps >= cfg.fps as f64 * 0.9 && scroll_p95 <= latency_ceiling;
    let video_ok = video.is_none_or(|(fps, _, p95)| fps >= cfg.fps as f64 * 0.75 && p95 <= video_ceiling);
    let typing_ok = typing_avg.is_finite() && typing_p95 <= latency_ceiling;
    let pass = scroll_ok && video_ok && typing_ok;
    println!(
        "  criteria: scroll >= {:.0} fps & p95 <= {latency_ceiling:.0} ms [{}] | video >= {:.0} fps & p95 <= {video_ceiling:.0} ms [{}] | typing p95 <= {latency_ceiling:.0} ms [{}] -> {}",
        cfg.fps as f64 * 0.9,
        if scroll_ok { "ok" } else { "FAIL" },
        cfg.fps as f64 * 0.75,
        if video_ok { "ok" } else { "FAIL" },
        if typing_ok { "ok" } else { "FAIL" },
        if pass { "PASS" } else { "FAIL" }
    );
    Ok(pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_roundtrip_and_ordering() {
        for id in [0u16, 1, 0x0ABC, 0x0FFF] {
            assert_eq!(marker_id(marker_word(id)), Some(id));
        }
        assert_eq!(marker_id(marker_word(0x0123) ^ 0x0010), None);
        assert!(marker_newer(5, 4));
        assert!(marker_newer(0, 0x0FFF)); // wrap
        assert!(!marker_newer(4, 5));
    }

    #[test]
    fn synthetic_marker_survives_nv12_and_rgba_roundtrip() {
        let shared = Arc::new(Shared {
            phase: AtomicU8::new(PHASE_SCROLL),
            presented: Mutex::new(HashMap::new()),
        });
        let mut desk = SyntheticDesktop::new(640, 360, 60, Instant::now(), shared);
        let frame = desk.advance(Instant::now(), 1).expect("scrolling always changes");
        let pic = crate::codec::DecodedPicture::Nv12 {
            width: 640,
            height: 360,
            stride: 640,
            uv_offset: 640 * 360,
            data: frame.nv12,
        };
        let mut image = ColorImage::new([640, 360], eframe::egui::Color32::BLACK);
        pic.write_rgba(bytemuck::cast_slice_mut(&mut image.pixels));
        assert_eq!(read_marker(&image, 640, 360), Some(desk.counter));
    }
}
