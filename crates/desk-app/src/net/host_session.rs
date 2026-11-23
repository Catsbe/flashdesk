use crate::capture::dxgi::{CaptureOutcome, ScreenCapturer};
use crate::capture::monitors::enumerate_monitors;
use crate::clipboard::ClipboardSync;
use crate::codec::{create_encoder, CodecCapabilities};
use crate::input::win_input::inject_input;
use crate::net::connect_and_auth_relay;
use crate::transfer::host_handler::run_host_file_session;
use crate::vdd::{detect_vdd_status, handle_vdd_request};
use anyhow::Result;
use protocol::auth::DeviceBundle;
use protocol::noise::NoiseStream;
use protocol::relay::{ChannelKind, ConnectionPurpose};
use protocol::session::{ControlMsg, InputEvent, MonitorInfo, VddRequest, VideoCodec, VideoFrame};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

#[derive(Default)]
pub struct PendingHostChannels {
    sessions: Mutex<HashMap<Uuid, PendingSlot>>,
    direct_files: Mutex<HashMap<Uuid, PendingDirectFile>>,
    active_session: Mutex<Option<(Arc<AtomicBool>, Arc<tokio::sync::Notify>)>>,
    session_mutex: Arc<tokio::sync::Mutex<()>>,
    next_seq: AtomicU64,
    latest_seq: AtomicU64,
}

struct PendingSlot {
    ctrl: Option<NoiseStream<TcpStream>>,
    video: Option<NoiseStream<TcpStream>>,
    created_at: Instant,
}

enum PendingDirectFile {
    Arrived(NoiseStream<TcpStream>),
    Waiting(oneshot::Sender<NoiseStream<TcpStream>>),
}

impl PendingHostChannels {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_channel(
        &self,
        session_id: Uuid,
        channel: ChannelKind,
        stream: NoiseStream<TcpStream>,
    ) -> Option<(NoiseStream<TcpStream>, NoiseStream<TcpStream>)> {
        let mut map = self.sessions.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, v| now.duration_since(v.created_at) < Duration::from_secs(20));

        let slot = map.entry(session_id).or_insert_with(|| PendingSlot {
            ctrl: None,
            video: None,
            created_at: now,
        });
        match channel {
            ChannelKind::Control => slot.ctrl = Some(stream),
            ChannelKind::Video => slot.video = Some(stream),
            _ => {}
        }
        if slot.ctrl.is_some() && slot.video.is_some() {
            let mut done = map.remove(&session_id).unwrap();
            Some((done.ctrl.take().unwrap(), done.video.take().unwrap()))
        } else {
            None
        }
    }

    pub fn fulfill_direct_file(&self, transfer_id: Uuid, stream: NoiseStream<TcpStream>) {
        let mut map = self.direct_files.lock().unwrap();
        match map.remove(&transfer_id) {
            Some(PendingDirectFile::Waiting(tx)) => {
                let _ = tx.send(stream);
            }
            _ => {
                map.insert(transfer_id, PendingDirectFile::Arrived(stream));
            }
        }
    }

    pub fn wait_direct_file(&self, transfer_id: Uuid) -> Result<NoiseStream<TcpStream>, oneshot::Receiver<NoiseStream<TcpStream>>> {
        let mut map = self.direct_files.lock().unwrap();
        if let Some(PendingDirectFile::Arrived(s)) = map.remove(&transfer_id) {
            return Ok(s);
        }
        let (tx, rx) = oneshot::channel();
        map.insert(transfer_id, PendingDirectFile::Waiting(tx));
        Err(rx)
    }

    pub fn claim_next_session(&self) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        self.latest_seq.store(seq, Ordering::SeqCst);
        self.cancel_active_session();
        seq
    }

    pub fn is_superseded(&self, seq: u64) -> bool {
        self.latest_seq.load(Ordering::SeqCst) != seq
    }

    pub fn cancel_active_session(&self) {
        if let Some((stop, notify)) = self.active_session.lock().unwrap().take() {
            stop.store(true, Ordering::Relaxed);
            notify.notify_waiters();
        }
    }

    pub fn set_active_session(
        &self,
        stop_flag: Arc<AtomicBool>,
        stop_notify: Arc<tokio::sync::Notify>,
    ) {
        *self.active_session.lock().unwrap() = Some((stop_flag, stop_notify));
    }

    pub fn clear_active_session_if(&self, stop_flag: &Arc<AtomicBool>) {
        let mut guard = self.active_session.lock().unwrap();
        if let Some((cur, _)) = guard.as_ref() {
            if Arc::ptr_eq(cur, stop_flag) {
                *guard = None;
            }
        }
    }
}

#[derive(Clone)]
pub struct HostSessionOptions {
    pub caps: CodecCapabilities,
    pub bundle: DeviceBundle,
    pub revoked_ids: Vec<Uuid>,
    pub agent_mode: bool,
    pub auto_vdd_on_headless: bool,
    pub pending_channels: Arc<PendingHostChannels>,
}

#[derive(Debug, Clone)]
struct StreamParams {
    target_fps: u32,
    bitrate_kbps: u32,
    monitor_index: u32,
}

pub async fn run_host_session<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    mut ctrl: NoiseStream<S>,
    video: NoiseStream<S>,
    opts: HostSessionOptions,
) -> Result<()> {
    let session_seq = opts.pending_channels.claim_next_session();
    let _session_guard = opts.pending_channels.session_mutex.lock().await;
    if opts.pending_channels.is_superseded(session_seq) {
        tracing::info!("host session #{session_seq} superseded before start");
        return Ok(());
    }

    let peer_cert = ctrl.peer_cert().clone();
    let peer_name = peer_cert.device_name.clone();
    let peer_device_id = peer_cert.device_id;
    tracing::info!(
        "host session started for viewer '{}' (agent_mode={})",
        peer_name,
        opts.agent_mode
    );

    let mut targets = enumerate_monitors().unwrap_or_default();
    if targets.is_empty() && opts.auto_vdd_on_headless {
        tracing::info!("no physical monitors found; attempting headless Parsec VDD attach");
        let _ = tokio::task::spawn_blocking(|| {
            handle_vdd_request(&VddRequest::AddDisplay {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
        })
        .await;
        targets = enumerate_monitors().unwrap_or_default();
    }
    let monitors: Vec<MonitorInfo> = targets.into_iter().map(|t| t.info).collect();

    let hello = ControlMsg::Hello {
        encode_codecs: opts.caps.encode_codecs.clone(),
        decode_codecs: opts.caps.decode_codecs.clone(),
        monitors: monitors.clone(),
        active_monitor: 0,
    };
    ctrl.send_msg(&bincode::serialize(&hello)?).await?;

    let start_raw = tokio::time::timeout(Duration::from_secs(10), ctrl.recv_msg())
        .await
        .map_err(|_| anyhow::anyhow!("等待 StartStream 超时"))??;
    let start_msg: ControlMsg = bincode::deserialize(&start_raw)?;
    let (codec, target_fps, bitrate_kbps, initial_monitor) = match start_msg {
        ControlMsg::StartStream {
            codec,
            target_fps,
            bitrate_kbps,
            monitor_index,
        } => (codec, target_fps, bitrate_kbps, monitor_index),
        other => anyhow::bail!("expected StartStream, got {other:?}"),
    };

    let (mut ctrl_reader, mut ctrl_writer, _) = ctrl.into_split();
    let (_video_reader, mut video_writer, _) = video.into_split();

    let (ctrl_out_tx, mut ctrl_out_rx) = mpsc::channel::<ControlMsg>(128);
    let (video_tx, mut video_rx) = mpsc::channel::<VideoFrame>(2);

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_notify = Arc::new(tokio::sync::Notify::new());
    opts.pending_channels
        .set_active_session(Arc::clone(&stop_flag), Arc::clone(&stop_notify));
    let force_keyframe = Arc::new(AtomicBool::new(true));
    let last_encode_us = Arc::new(AtomicU32::new(0));
    let stream_params = Arc::new(Mutex::new(StreamParams {
        target_fps: target_fps.clamp(10, 120),
        bitrate_kbps: bitrate_kbps.clamp(300, 100_000),
        monitor_index: initial_monitor,
    }));
    let monitors_map = Arc::new(Mutex::new(
        monitors
            .iter()
            .map(|m| (m.index, m.clone()))
            .collect::<HashMap<u32, MonitorInfo>>(),
    ));

    // Send initial VDD status proactively
    if let Ok(Ok(vdd_info)) = tokio::task::spawn_blocking(|| Ok::<_, anyhow::Error>(detect_vdd_status())).await {
        let _ = ctrl_out_tx.try_send(ControlMsg::VddStatus(vdd_info));
    }

    // Clipboard bidirectional sync
    let clipboard_sync = ClipboardSync::new(true);
    {
        let cb_ctrl_tx = ctrl_out_tx.clone();
        clipboard_sync.spawn_watcher(Arc::clone(&stop_flag), move |data| {
            let _ = cb_ctrl_tx.try_send(ControlMsg::Clipboard(data));
        });
    }

    // Dedicated input thread (isolates SetThreadDesktop in agent_mode from Tokio workers)
    let (input_tx, input_rx) = std::sync::mpsc::channel::<(InputEvent, Option<MonitorInfo>)>();
    let input_ctrl_tx = ctrl_out_tx.clone();
    let agent_mode = opts.agent_mode;
    let input_handle = std::thread::Builder::new()
        .name("flashdesk-host-input".into())
        .spawn(move || {
            let mut uipi_warned = false;
            while let Ok((ev, target_mon)) = input_rx.recv() {
                if !inject_input(&ev, target_mon.as_ref(), agent_mode) && !uipi_warned {
                    uipi_warned = true;
                    tracing::warn!("SendInput returned 0 (possibly blocked by UIPI)");
                    let _ = input_ctrl_tx.try_send(ControlMsg::Notice(
                        "部分键鼠输入被目标系统拦截(UIPI)：当前会话可能运行在桌面接管模式，或目标窗口权限更高。请在被控端主页确认「运行模式」为系统服务运行中".into(),
                    ));
                }
            }
        })?;

    // Dedicated capture + encode thread
    let cap_stop = Arc::clone(&stop_flag);
    let cap_stop_notify = Arc::clone(&stop_notify);
    let cap_kf = Arc::clone(&force_keyframe);
    let cap_enc_us = Arc::clone(&last_encode_us);
    let cap_params = Arc::clone(&stream_params);
    let cap_ctrl_tx = ctrl_out_tx.clone();

    let capture_handle = std::thread::Builder::new()
        .name("flashdesk-host-capture".into())
        .spawn(move || {
            run_capture_encode_loop(
                codec,
                agent_mode,
                cap_params,
                cap_stop,
                cap_stop_notify,
                cap_kf,
                cap_enc_us,
                video_tx,
                cap_ctrl_tx,
            );
        })?;

    let stop_video = Arc::clone(&stop_flag);
    let stop_video_notify = Arc::clone(&stop_notify);
    let video_task = tokio::spawn(async move {
        while let Some(frame) = video_rx.recv().await {
            let Ok(bytes) = bincode::serialize(&frame) else {
                continue;
            };
            if video_writer.send_msg(&bytes).await.is_err() {
                break;
            }
        }
        stop_video.store(true, Ordering::Relaxed);
        stop_video_notify.notify_waiters();
    });

    let stop_cw = Arc::clone(&stop_flag);
    let stop_cw_notify = Arc::clone(&stop_notify);
    let ctrl_write_task = tokio::spawn(async move {
        while let Some(msg) = ctrl_out_rx.recv().await {
            let Ok(bytes) = bincode::serialize(&msg) else {
                continue;
            };
            if ctrl_writer.send_msg(&bytes).await.is_err() {
                break;
            }
        }
        stop_cw.store(true, Ordering::Relaxed);
        stop_cw_notify.notify_waiters();
    });

    // Dedicated cancel-safe control reader task so select! never cancels NoiseReader::recv_msg mid-frame.
    let (ctrl_in_tx, mut ctrl_in_rx) = mpsc::channel::<Result<Vec<u8>, String>>(256);
    let ctrl_read_task = tokio::spawn(async move {
        loop {
            match ctrl_reader.recv_msg().await {
                Ok(raw) => {
                    if ctrl_in_tx.send(Ok(raw)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = ctrl_in_tx.send(Err(e.to_string())).await;
                    break;
                }
            }
        }
    });

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let raw = tokio::select! {
            _ = stop_notify.notified() => break,
            opt = ctrl_in_rx.recv() => match opt {
                Some(Ok(b)) => b,
                _ => break,
            },
        };
        let Ok(msg) = bincode::deserialize::<ControlMsg>(&raw) else {
            continue;
        };
        match msg {
            ControlMsg::Input(ev) => {
                let mon_idx = stream_params.lock().unwrap().monitor_index;
                let target_mon = if mon_idx == u32::MAX {
                    None
                } else {
                    monitors_map.lock().unwrap().get(&mon_idx).cloned()
                };
                let _ = input_tx.send((ev, target_mon));
            }
            ControlMsg::UpdateStream {
                target_fps,
                bitrate_kbps,
                monitor_index,
            } => {
                let mut p = stream_params.lock().unwrap();
                if p.monitor_index != monitor_index {
                    force_keyframe.store(true, Ordering::Relaxed);
                }
                p.target_fps = target_fps.clamp(10, 120);
                p.bitrate_kbps = bitrate_kbps.clamp(300, 100_000);
                p.monitor_index = monitor_index;
            }
            ControlMsg::RequestKeyframe => {
                force_keyframe.store(true, Ordering::Relaxed);
            }
            ControlMsg::Ping { seq, sent_us } => {
                let encode_us = last_encode_us.load(Ordering::Relaxed);
                let _ = ctrl_out_tx.try_send(ControlMsg::Pong {
                    seq,
                    sent_us,
                    encode_us,
                });
            }
            ControlMsg::Clipboard(data) => {
                let sync = clipboard_sync.clone();
                std::thread::spawn(move || {
                    if let Err(e) = sync.apply_remote(&data) {
                        tracing::debug!("apply remote clipboard failed: {e}");
                    }
                });
            }
            ControlMsg::OpenFileChannel {
                transfer_session_id,
                relay_addr,
            } => {
                let bundle = opts.bundle.clone();
                let revoked = opts.revoked_ids.clone();
                let pending = Arc::clone(&opts.pending_channels);
                let out_tx = ctrl_out_tx.clone();
                tokio::spawn(async move {
                    let res = match relay_addr {
                        Some(addr) => async {
                            let tcp = connect_and_auth_relay(
                                &addr,
                                &bundle.identity,
                                ConnectionPurpose::PairTunnel {
                                    session_id: transfer_session_id,
                                    channel: ChannelKind::File,
                                    peer_device_id,
                                },
                            )
                            .await?;
                            let ns = NoiseStream::handshake_responder(
                                tcp,
                                &bundle.root_pubkey,
                                &bundle.identity,
                                &revoked,
                            )
                            .await?;
                            run_host_file_session(ns).await
                        }
                        .await,
                        None => async {
                            let ns = match pending.wait_direct_file(transfer_session_id) {
                                Ok(s) => s,
                                Err(rx) => tokio::time::timeout(Duration::from_secs(10), rx)
                                    .await
                                    .map_err(|_| anyhow::anyhow!("直连文件通道等待超时"))??,
                            };
                            run_host_file_session(ns).await
                        }
                        .await,
                    };
                    if let Err(e) = res {
                        let _ = out_tx
                            .try_send(ControlMsg::Notice(format!("文件传输通道异常: {e}")));
                    }
                });
            }
            ControlMsg::VddAction(req) => {
                let out_tx = ctrl_out_tx.clone();
                let mmap = Arc::clone(&monitors_map);
                let kf = Arc::clone(&force_keyframe);
                tokio::spawn(async move {
                    let res = tokio::task::spawn_blocking(move || handle_vdd_request(&req)).await;
                    match res {
                        Ok(Ok(info)) => {
                            let _ = out_tx.send(ControlMsg::VddStatus(info)).await;
                            if let Ok(new_targets) = enumerate_monitors() {
                                let new_mons: Vec<MonitorInfo> =
                                    new_targets.into_iter().map(|t| t.info).collect();
                                *mmap.lock().unwrap() =
                                    new_mons.iter().map(|m| (m.index, m.clone())).collect();
                                let _ = out_tx.send(ControlMsg::MonitorsUpdated(new_mons)).await;
                                kf.store(true, Ordering::Relaxed);
                            }
                        }
                        Ok(Err(e)) => {
                            let _ = out_tx
                                .send(ControlMsg::Notice(format!("虚拟屏操作失败: {e}")))
                                .await;
                        }
                        Err(e) => {
                            let _ = out_tx
                                .send(ControlMsg::Notice(format!("虚拟屏任务异常: {e}")))
                                .await;
                        }
                    }
                });
            }
            _ => {}
        }
    }

    stop_flag.store(true, Ordering::Relaxed);
    stop_notify.notify_waiters();
    opts.pending_channels.clear_active_session_if(&stop_flag);
    drop(input_tx);
    ctrl_read_task.abort();
    video_task.abort();
    ctrl_write_task.abort();
    let _ = video_task.await;
    drop(ctrl_in_rx);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = capture_handle.join();
        let _ = input_handle.join();
    })
    .await;
    tracing::info!("host session ended for viewer '{}'", peer_name);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_capture_encode_loop(
    codec: VideoCodec,
    agent_mode: bool,
    params: Arc<Mutex<StreamParams>>,
    stop: Arc<AtomicBool>,
    stop_notify: Arc<tokio::sync::Notify>,
    force_keyframe: Arc<AtomicBool>,
    last_encode_us: Arc<AtomicU32>,
    video_tx: mpsc::Sender<VideoFrame>,
    ctrl_tx: mpsc::Sender<ControlMsg>,
) {
    let init = params.lock().unwrap().clone();
    let mut cur_monitor = init.monitor_index;
    let mut cur_bitrate = init.bitrate_kbps;

    let mut capturer = match ScreenCapturer::open(cur_monitor, agent_mode) {
        Ok(c) => c,
        Err(e) => {
            let _ = ctrl_tx.try_send(ControlMsg::Notice(format!("抓屏初始化失败: {e:#}")));
            stop.store(true, Ordering::Relaxed);
            stop_notify.notify_waiters();
            return;
        }
    };
    if capturer.is_gdi_fallback() {
        let _ = ctrl_tx.try_send(ControlMsg::Notice("已自动切换至 GDI 兼容抓屏模式".into()));
    }

    let (mut cur_w, mut cur_h) = capturer.dimensions();
    let mut encoder = match create_encoder(codec, cur_w, cur_h, init.target_fps, cur_bitrate) {
        Ok(enc) => enc,
        Err(e) => {
            let _ = ctrl_tx.try_send(ControlMsg::Notice(format!("编码器初始化失败: {e:#}")));
            stop.store(true, Ordering::Relaxed);
            stop_notify.notify_waiters();
            return;
        }
    };

    let mut frame_id = 0u64;
    let mut next_tick = Instant::now();
    let mut need_kf_after_drop = true;
    let mut prime_frames_left: u32 = 6;
    let mut last_sent_at = Instant::now();
    let mut consecutive_enc_errors: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        let p = params.lock().unwrap().clone();

        if p.monitor_index != cur_monitor {
            match ScreenCapturer::open(p.monitor_index, agent_mode) {
                Ok(new_cap) => {
                    capturer = new_cap;
                    cur_monitor = p.monitor_index;
                    need_kf_after_drop = true;
                    prime_frames_left = prime_frames_left.max(4);
                    let (nw, nh) = capturer.dimensions();
                    if (nw, nh) != (cur_w, cur_h) {
                        if let Ok(new_enc) =
                            create_encoder(codec, nw, nh, p.target_fps, cur_bitrate)
                        {
                            encoder = new_enc;
                            cur_w = nw;
                            cur_h = nh;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("monitor switch failed: {e:#}");
                }
            }
        }

        if p.bitrate_kbps != cur_bitrate {
            if encoder.set_bitrate(p.bitrate_kbps).is_ok() {
                cur_bitrate = p.bitrate_kbps;
            }
        }

        let interval = Duration::from_micros(1_000_000 / p.target_fps.max(1) as u64);
        let now = Instant::now();
        if next_tick > now {
            std::thread::sleep(next_tick - now);
        }
        next_tick = Instant::now() + interval;

        let kf_signal = force_keyframe.swap(false, Ordering::Relaxed);
        if kf_signal {
            prime_frames_left = prime_frames_left.max(4);
        }
        let req_kf = kf_signal || need_kf_after_drop;
        let periodic_refresh = last_sent_at.elapsed() >= Duration::from_secs(1);
        let force_capture = req_kf || prime_frames_left > 0 || periodic_refresh;
        let wait_ms = (interval.as_millis() as u32).clamp(4, 30);

        match capturer.capture_next(wait_ms, force_capture) {
            Ok(CaptureOutcome::Frame(raw)) => {
                let t0 = Instant::now();
                match encoder.encode(&raw, req_kf) {
                    Ok(pkt) => {
                        let enc_us = t0.elapsed().as_micros() as u32;
                        last_encode_us.store(enc_us, Ordering::Relaxed);
                        consecutive_enc_errors = 0;
                        if pkt.data.is_empty() {
                            continue;
                        }
                        frame_id += 1;
                        let vf = VideoFrame {
                            frame_id,
                            timestamp_us: raw.timestamp_us,
                            codec: encoder.codec(),
                            width: raw.width,
                            height: raw.height,
                            monitor_index: cur_monitor,
                            is_keyframe: pkt.is_keyframe,
                            encode_us: enc_us,
                            encoder_name: encoder.backend_name().to_string(),
                            data: pkt.data,
                        };
                        match video_tx.try_send(vf) {
                            Ok(()) => {
                                need_kf_after_drop = false;
                                prime_frames_left = prime_frames_left.saturating_sub(1);
                                last_sent_at = Instant::now();
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                need_kf_after_drop = true;
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("encode error: {e:#}");
                        need_kf_after_drop = true;
                        consecutive_enc_errors += 1;
                        if consecutive_enc_errors >= 3
                            && codec == VideoCodec::H264
                            && encoder.backend_name() != "OpenH264-SW"
                        {
                            if let Ok(sw_enc) = crate::codec::openh264_codec::OpenH264Encoder::new(
                                cur_w,
                                cur_h,
                                p.target_fps,
                                cur_bitrate,
                            ) {
                                encoder = Box::new(sw_enc);
                                consecutive_enc_errors = 0;
                                prime_frames_left = 4;
                                let _ = ctrl_tx.try_send(ControlMsg::Notice(
                                    "已自动切换至 OpenH264 软件编码".into(),
                                ));
                            }
                        }
                    }
                }
            }
            Ok(CaptureOutcome::Unchanged) => {}
            Ok(CaptureOutcome::Reacquired) => {
                need_kf_after_drop = true;
                prime_frames_left = prime_frames_left.max(4);
            }
            Err(e) => {
                tracing::warn!("capture error: {e:#}");
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(new_cap) = ScreenCapturer::open(cur_monitor, agent_mode) {
                    capturer = new_cap;
                    need_kf_after_drop = true;
                    prime_frames_left = prime_frames_left.max(4);
                    let (nw, nh) = capturer.dimensions();
                    if (nw, nh) != (cur_w, cur_h) {
                        if let Ok(new_enc) =
                            create_encoder(codec, nw, nh, p.target_fps, cur_bitrate)
                        {
                            encoder = new_enc;
                            cur_w = nw;
                            cur_h = nh;
                        }
                    }
                }
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    stop_notify.notify_waiters();
}
