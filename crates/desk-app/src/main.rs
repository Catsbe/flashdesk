#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod clipboard;
mod codec;
mod config;
mod input;
mod net;
mod service;
mod timing;
mod transfer;
mod ui;
mod vdd;

use anyhow::Result;
use capture::dxgi::{CaptureOutcome, ScreenCapturer};
use capture::monitors::enumerate_monitors;
use clap::Parser;
use clipboard::{decode_png_to_rgba, encode_rgba_to_png};
use codec::{
    bgra_to_nv12, create_decoder, create_encoder,
    openh264_codec::{OpenH264Decoder, OpenH264Encoder},
    probe_capabilities, RawFrame, VideoDecoder, VideoEncoder,
};
use config::AppConfig;
use net::direct::spawn_direct_listener;
use net::relay_client::{spawn_relay_manager, RelayCommand};
use net::RelayStatus;
use protocol::session::VideoCodec;
use service::install::{
    install_and_start_service, query_service_state, stop_and_uninstall_service,
    sync_config_file_to_program_data, SHARED_CONFIG_PATH,
};
use service::win_service::run_windows_service;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing_subscriber::EnvFilter;
use ui::app::FlashDeskApp;
use vdd::detect_vdd_status;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

#[derive(Parser)]
#[command(name = "flashdesk", about = "FlashDesk self-hosted 60fps remote desktop")]
struct Cli {
    /// Run automated diagnostics: monitor/DPI enum, codec probe, real/synthetic 1080p encode->decode, service & VDD check
    #[arg(long)]
    self_test: bool,
    /// Run as Windows SYSTEM service supervisor (Session 0)
    #[arg(long)]
    service: bool,
    /// Run as headless Session 1 SYSTEM host agent
    #[arg(long)]
    agent: bool,
    /// Install & start Windows SYSTEM service (requires Administrator)
    #[arg(long)]
    install_service: bool,
    /// Stop & uninstall Windows SYSTEM service (requires Administrator)
    #[arg(long)]
    uninstall_service: bool,
    /// Stop & start Windows SYSTEM service again, e.g. after updating flashdesk.exe (requires Administrator)
    #[arg(long)]
    restart_service: bool,
    /// Copy config file to C:\ProgramData\FlashDesk\config.json (requires Administrator if protected)
    #[arg(long)]
    sync_service_config: bool,
    /// Explicit config path for service / agent / install commands
    #[arg(long)]
    config: Option<PathBuf>,
    /// Benchmark the real streaming pipeline (encode -> flow control -> Noise -> decode) over a
    /// simulated bandwidth-limited relay, using a synthetic 60 Hz desktop. Exit code 0 = PASS.
    #[arg(long)]
    link_bench: bool,
    /// --link-bench: bottleneck bandwidth in Mbps
    #[arg(long, default_value_t = 5.0)]
    bench_mbps: f64,
    /// --link-bench: one-way delay in ms (RTT = 2x)
    #[arg(long, default_value_t = 15)]
    bench_delay_ms: u64,
    /// --link-bench: relay buffer depth in ms before the link pushes back
    #[arg(long, default_value_t = 1000)]
    bench_queue_ms: u64,
    /// --link-bench: synthetic desktop size, e.g. 1920x1080 or 2560x1440
    #[arg(long, default_value = "1920x1080")]
    bench_res: String,
    /// --link-bench: frame rate
    #[arg(long, default_value_t = 60)]
    bench_fps: u32,
    /// --link-bench: bitrate ceiling in kbps (the toolbar slider)
    #[arg(long, default_value_t = 4000)]
    bench_max_kbps: u32,
    /// --link-bench: resolution cap by height (0 = native, 1080, 720)
    #[arg(long, default_value_t = 0)]
    bench_cap: u32,
    /// --link-bench: seconds of continuous scrolling
    #[arg(long, default_value_t = 12)]
    bench_scroll_secs: u64,
    /// --link-bench: seconds with a large full-motion video region (saturates the link)
    #[arg(long, default_value_t = 10)]
    bench_video_secs: u64,
    /// --link-bench: seconds of sparse "typing" updates
    #[arg(long, default_value_t = 8)]
    bench_typing_secs: u64,
    /// --link-bench: disable frame acks (emulates the legacy transport for comparison)
    #[arg(long)]
    bench_no_ack: bool,
}

/// Re-attaches stdout/stderr to the caller's terminal for CLI invocations when built with
/// `windows_subsystem = "windows"`, leaving piped handles untouched and avoiding any console
/// flash on normal GUI double-click.
fn attach_parent_console_if_needed() {
    if std::env::args_os().len() <= 1 {
        return;
    }
    let has_stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
        .map(|h| !h.is_invalid() && !h.0.is_null())
        .unwrap_or(false);
    if !has_stdout {
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// Persists fatal GUI startup diagnostics alongside the config file and alerts the user via a
/// native Win32 dialog when running without a console window.
fn report_gui_fatal(cfg_path: &std::path::Path, detail: &str) {
    let log_path = cfg_path.with_file_name("flashdesk-error.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&log_path, detail);

    let msg = format!("{detail}\n\n详情已写入日志:\n{}", log_path.display());
    let msg_w: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let title_w: Vec<u16> = "FlashDesk 启动失败"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(msg_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

struct DualWriter {
    file: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
}

impl std::io::Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(buf);
        }
        let _ = std::io::Write::write_all(&mut std::io::stdout(), buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
        Ok(())
    }
}

fn init_logging(log_path: Option<&std::path::Path>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if let Some(path) = log_path {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if let Some(file) = open_log_file(path) {
            let shared = std::sync::Arc::new(std::sync::Mutex::new(file));
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(move || DualWriter {
                    file: std::sync::Arc::clone(&shared),
                })
                .init();
            return;
        }
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Opens the log for APPEND with full sharing so (a) history survives restarts instead of being
/// truncated by `File::create`, and (b) the file can be read while the service/agent holds it open.
/// Rotates to `<name>.old` once the log exceeds 8 MiB.
fn open_log_file(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > 8 * 1024 * 1024 {
            let old = std::path::PathBuf::from(format!("{}.old", path.display()));
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(path, &old);
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .ok()
}

/// Requests 1 ms timer resolution for this process. Without it `Sleep` / `std::thread::sleep`
/// round up to the 15.6 ms system tick on many machines (measured on a Win11 VM: a 1 ms sleep
/// took ~12 ms), which capped the capture loop around 40 fps and slowed the 1 ms MF event
/// polling. Since Windows 11 the request is per-process and may be dropped for occluded /
/// minimised window-owning processes unless timer-resolution power throttling is opted out of.
fn enable_high_resolution_timers() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE,
    };
    unsafe {
        let _ = windows::Win32::Media::timeBeginPeriod(1);
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            std::ptr::addr_of!(state).cast(),
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn main() -> Result<()> {
    attach_parent_console_if_needed();
    enable_high_resolution_timers();

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cli = Cli::parse();

    if cli.link_bench {
        if std::env::var_os("RUST_LOG").is_none() {
            std::env::set_var("RUST_LOG", "warn");
        }
        init_logging(None);
    } else if cli.install_service
        || cli.uninstall_service
        || cli.restart_service
        || cli.sync_service_config
        || cli.self_test
    {
        init_logging(None);
    } else if cli.service {
        let cfg_path = cli
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from(SHARED_CONFIG_PATH));
        init_logging(Some(&cfg_path.with_file_name("flashdesk-service.log")));
    } else if cli.agent {
        let cfg_path = cli
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from(SHARED_CONFIG_PATH));
        init_logging(Some(&cfg_path.with_file_name("flashdesk-agent.log")));
    } else {
        let cfg_path = cli.config.clone().unwrap_or_else(AppConfig::resolve_path);
        init_logging(Some(&cfg_path.with_file_name("flashdesk.log")));
    }

    if cli.install_service {
        return install_and_start_service(cli.config.as_deref());
    }
    if cli.uninstall_service {
        return stop_and_uninstall_service();
    }
    if cli.restart_service {
        return service::install::restart_service();
    }
    if cli.sync_service_config {
        let src = cli.config.unwrap_or_else(AppConfig::resolve_path);
        sync_config_file_to_program_data(&src)?;
        return Ok(());
    }
    if cli.service {
        let cfg_path = cli
            .config
            .unwrap_or_else(|| PathBuf::from(SHARED_CONFIG_PATH));
        return run_windows_service(cfg_path);
    }
    if cli.agent {
        let cfg_path = cli
            .config
            .unwrap_or_else(|| PathBuf::from(SHARED_CONFIG_PATH));
        return run_headless_agent(cfg_path);
    }
    if cli.self_test {
        return run_self_test();
    }
    if cli.link_bench {
        let (bw, bh) = cli
            .bench_res
            .split_once(['x', 'X'])
            .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
            .ok_or_else(|| anyhow::anyhow!("--bench-res must look like 1920x1080"))?;
        let pass = net::bench::run_link_bench(net::bench::BenchConfig {
            link_mbps: cli.bench_mbps,
            one_way_ms: cli.bench_delay_ms,
            queue_ms: cli.bench_queue_ms,
            width: bw,
            height: bh,
            fps: cli.bench_fps,
            max_kbps: cli.bench_max_kbps,
            res_cap_h: cli.bench_cap,
            scroll_secs: cli.bench_scroll_secs,
            video_secs: cli.bench_video_secs,
            typing_secs: cli.bench_typing_secs,
            no_ack: cli.bench_no_ack,
        })?;
        if !pass {
            std::process::exit(1);
        }
        return Ok(());
    }

    let cfg_path = cli.config.unwrap_or_else(AppConfig::resolve_path);
    let cfg_path_panic = cfg_path.clone();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev_hook(info);
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".to_string()
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());
        let detail = format!("程序异常终止 ({loc}):\n{payload}");
        report_gui_fatal(&cfg_path_panic, &detail);
    }));

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            report_gui_fatal(&cfg_path, &format!("异步运行时初始化失败: {e}"));
            return Err(e.into());
        }
    };
    let _rt_guard = rt.enter();

    let caps = probe_capabilities();
    let monitors = enumerate_monitors().unwrap_or_default();
    let cfg = AppConfig::load_or_default(&cfg_path);

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([980.0, 700.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("FlashDesk — 自建高速远程桌面"),
        ..Default::default()
    };

    let cfg_path_err = cfg_path.clone();
    if let Err(e) = eframe::run_native(
        "FlashDesk",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(FlashDeskApp::new(
                cc, cfg_path, cfg, caps, monitors,
            )))
        }),
    ) {
        let detail = format!("界面引擎启动失败: {e}");
        report_gui_fatal(&cfg_path_err, &detail);
        return Err(anyhow::anyhow!("eframe exited with error: {e}"));
    }

    Ok(())
}

fn run_headless_agent(cfg_path: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let caps = probe_capabilities();
        let monitors = enumerate_monitors().unwrap_or_default();
        let cfg = AppConfig::load_or_default(&cfg_path);
        let cfg_arc = Arc::new(Mutex::new(cfg.clone()));
        let status = Arc::new(Mutex::new(RelayStatus::default()));
        let noop_repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});

        let relay_tx = spawn_relay_manager(
            cfg.clone(),
            caps.clone(),
            monitors.len() as u32,
            true,
            status,
            noop_repaint,
        );
        spawn_direct_listener(Arc::clone(&cfg_arc), caps, true);

        // Publish liveness heartbeat so the desktop GUI can tell "service running + agent alive"
        // apart from "service running but agent dead" and take over hosting when needed.
        let pid = std::process::id();
        let session_id = service::health::current_session_id();
        let elevated = service::health::current_process_elevated();
        let started_at = service::health::unix_now();
        tracing::info!(
            "agent started: pid={pid} session={session_id} elevated={elevated} config={}",
            cfg_path.display()
        );
        if let Err(e) = service::health::write_agent_status(pid, session_id, elevated, started_at) {
            tracing::warn!("failed to publish agent heartbeat: {e}");
        }

        let mut last_mtime = std::fs::metadata(&cfg_path)
            .and_then(|m| m.modified())
            .ok();

        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            if let Err(e) =
                service::health::write_agent_status(pid, session_id, elevated, started_at)
            {
                tracing::warn!("failed to refresh agent heartbeat: {e}");
            }
            let cur_mtime = std::fs::metadata(&cfg_path)
                .and_then(|m| m.modified())
                .ok();
            if cur_mtime != last_mtime {
                last_mtime = cur_mtime;
                let new_cfg = AppConfig::load_or_default(&cfg_path);
                *cfg_arc.lock().unwrap() = new_cfg.clone();
                let _ = relay_tx.send(RelayCommand::ReloadConfig(new_cfg));
                tracing::info!("reloaded agent config from {}", cfg_path.display());
            }
        }
    })
}

fn run_self_test() -> Result<()> {
    println!("=== FlashDesk Self-Test ===");

    let monitors = enumerate_monitors()?;
    println!("[1/5] Detected {} active monitor(s):", monitors.len());
    for m in &monitors {
        println!(
            "  - Monitor #{}: {} | {}x{} at ({}, {}) | DPI={} | primary={}",
            m.info.index,
            m.info.name,
            m.info.width,
            m.info.height,
            m.info.left,
            m.info.top,
            m.info.dpi,
            m.info.is_primary
        );
    }
    anyhow::ensure!(!monitors.is_empty(), "No monitors found");

    let svc_state = query_service_state();
    let vdd_info = detect_vdd_status();
    println!("[2/5] Service & Virtual Display Probe:");
    println!("  - Windows Service: {:?}", svc_state);
    println!("  - Virtual Display: {}", vdd_info.details);

    // Clipboard PNG encode/decode test
    let sample_rgba = vec![40u8, 180, 220, 255].repeat(16 * 16);
    let png = encode_rgba_to_png(16, 16, &sample_rgba)?;
    let (pw, ph, decoded_rgba) = decode_png_to_rgba(&png)?;
    anyhow::ensure!((pw, ph) == (16, 16) && decoded_rgba == sample_rgba);
    println!(
        "[3/5] Clipboard PNG Round-Trip: 16x16 RGBA -> {} bytes PNG -> verified",
        png.len()
    );

    let caps = probe_capabilities();
    println!("[4/5] Probed Codec Capabilities:");
    println!("  - Encode codecs: {:?}", caps.encode_codecs);
    println!("  - Encode backends: {:?}", caps.encode_backends);
    println!("  - Decode codecs: {:?}", caps.decode_codecs);
    println!("  - Decode backends: {:?}", caps.decode_backends);
    anyhow::ensure!(
        !caps.encode_codecs.is_empty() && !caps.decode_codecs.is_empty(),
        "No working encoder/decoder found"
    );

    println!("[5/5] Testing capture/NV12 -> encode -> decode round-trip...");
    let live_res: Result<(u32, u32, Vec<RawFrame>)> = (|| {
        let mut capturer = ScreenCapturer::open(0, false)?;
        let (w, h) = capturer.dimensions();
        println!(
            "  - Opened live ScreenCapturer(0) [{}]: {}x{}",
            capturer.backend_name(),
            w,
            h
        );
        let mut frames = Vec::new();
        for attempt in 0..12 {
            match capturer.capture_next(100, true)? {
                CaptureOutcome::Frame(f) => {
                    frames.push(f);
                    if frames.len() >= 8 {
                        break;
                    }
                }
                CaptureOutcome::Unchanged | CaptureOutcome::Reacquired => {
                    if attempt == 11 && frames.is_empty() {
                        anyhow::bail!("Capture failed to yield a desktop frame in 12 attempts");
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        Ok((w, h, frames))
    })();

    let (w, h, raw_frames) = match live_res {
        Ok(res) => res,
        Err(e)
            if {
                let msg = format!("{e:#}");
                msg.contains("0x80070005") || msg.contains("0x80070006")
            } =>
        {
            println!(
                "[WARN] Live desktop capture unavailable in isolated desktop ({e:#}); falling back to 1920x1080 synthetic BGRA->NV12 frames to verify full 1080p HW/SW encode->decode pipeline. Run --self-test from your normal Windows desktop to verify live DXGI/GDI capture."
            );
            let (w, h) = (1920u32, 1080u32);
            let (frames, avg_conv_ms) = generate_synthetic_nv12_frames(w, h, 8);
            println!(
                "  - Generated {} synthetic {}x{} NV12 frames (avg bgra_to_nv12 {:.2}ms)",
                frames.len(),
                w,
                h,
                avg_conv_ms
            );
            (w, h, frames)
        }
        Err(e) => return Err(e),
    };

    let mut pipelines: Vec<(Box<dyn VideoEncoder>, Box<dyn VideoDecoder>)> = Vec::new();
    if caps.encode_codecs.contains(&VideoCodec::Hevc)
        && caps.decode_codecs.contains(&VideoCodec::Hevc)
    {
        pipelines.push((
            create_encoder(VideoCodec::Hevc, w, h, 60, 4500)?,
            create_decoder(VideoCodec::Hevc)?,
        ));
    }
    if caps.encode_codecs.contains(&VideoCodec::H264)
        && caps.decode_codecs.contains(&VideoCodec::H264)
    {
        pipelines.push((
            create_encoder(VideoCodec::H264, w, h, 60, 4500)?,
            create_decoder(VideoCodec::H264)?,
        ));
    }
    pipelines.push((
        Box::new(OpenH264Encoder::new(w, h, 60, 4500)?),
        Box::new(OpenH264Decoder::new()?),
    ));

    for (mut enc, mut dec) in pipelines {
        verify_codec_pipeline(enc.as_mut(), dec.as_mut(), &raw_frames)?;
    }

    println!("=== All Self-Tests PASSED ===");
    Ok(())
}

fn generate_synthetic_nv12_frames(w: u32, h: u32, count: usize) -> (Vec<RawFrame>, f64) {
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let mut frames = Vec::with_capacity(count);
    let mut conv_us_sum = 0u128;

    for idx in 0..count {
        let shift = (idx as u32) * 32;
        for y in 0..h {
            let row_off = (y * w * 4) as usize;
            let bar = ((y + shift) / 64) % 4;
            let (b, g, r) = match bar {
                0 => (40u8, 180u8, 60u8),
                1 => (220u8, 130u8, 40u8),
                2 => (50u8, 70u8, 210u8),
                _ => (210u8, 210u8, 210u8),
            };
            for x in 0..w {
                let p = row_off + (x as usize) * 4;
                let grad = ((x + shift) & 0x3F) as u8;
                bgra[p] = b.saturating_add(grad);
                bgra[p + 1] = g;
                bgra[p + 2] = r;
                bgra[p + 3] = 255;
            }
        }
        let t0 = Instant::now();
        let nv12 = bgra_to_nv12(&bgra, w, h);
        conv_us_sum += t0.elapsed().as_micros();
        frames.push(RawFrame {
            width: w,
            height: h,
            nv12,
            timestamp_us: (idx as u64) * 16_667,
            content_changed: true,
        });
    }

    (frames, conv_us_sum as f64 / count as f64 / 1000.0)
}

fn verify_codec_pipeline(
    enc: &mut dyn VideoEncoder,
    dec: &mut dyn VideoDecoder,
    raw_frames: &[RawFrame],
) -> Result<()> {
    let mut encoded_bytes = 0usize;
    let mut decoded_ok = 0usize;
    let mut enc_us_sum = 0u128;
    let mut dec_us_sum = 0u128;

    for (i, frame) in raw_frames.iter().enumerate() {
        let t0 = Instant::now();
        let pkt = enc.encode(frame, i == 0)?;
        enc_us_sum += t0.elapsed().as_micros();
        encoded_bytes += pkt.data.len();

        if !pkt.data.is_empty() {
            let t1 = Instant::now();
            if let Some(pic) = dec.decode(&pkt.data)? {
                let (pw, ph) = pic.dimensions();
                let rgba = pic.to_rgba();
                dec_us_sum += t1.elapsed().as_micros();
                anyhow::ensure!(pw > 0 && ph > 0);
                anyhow::ensure!(rgba.len() == (pw * ph * 4) as usize);
                decoded_ok += 1;
            }
        }
    }

    println!(
        "  - {:?} -> {:?} ({} -> {}): encoded={} bytes across {} frames (avg enc {:.2}ms), decoded={} frames (avg dec {:.2}ms)",
        enc.codec(),
        dec.codec(),
        enc.backend_name(),
        dec.backend_name(),
        encoded_bytes,
        raw_frames.len(),
        enc_us_sum as f64 / raw_frames.len() as f64 / 1000.0,
        decoded_ok,
        if decoded_ok > 0 { dec_us_sum as f64 / decoded_ok as f64 / 1000.0 } else { 0.0 }
    );
    anyhow::ensure!(encoded_bytes > 0, "{} produced 0 encoded bytes", enc.backend_name());
    anyhow::ensure!(decoded_ok > 0, "{} produced 0 decoded frames", dec.backend_name());
    Ok(())
}
