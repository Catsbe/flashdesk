use crate::capture::dxgi::CaptureOutcome;
use crate::capture::monitors::enumerate_monitors;
use crate::capture::{open_frame_source, FrameSourceFactory};
use crate::clipboard::ClipboardSync;
use crate::codec::{
    create_encoder, fit_within, openh264_codec::OpenH264Encoder, scale_nv12, CodecCapabilities,
    RawFrame, VideoEncoder,
};
use crate::input::win_input::inject_input;
use crate::net::connect_and_auth_relay;
use crate::net::flow::FlowController;
use crate::transfer::host_handler::run_host_file_session;
use crate::vdd::{detect_vdd_status, handle_vdd_request};
use anyhow::Result;
use protocol::auth::DeviceBundle;
use protocol::noise::NoiseStream;
use protocol::relay::{ChannelKind, ConnectionPurpose};
use protocol::session::{
    ControlMsg, HostStreamStats, InputEvent, MonitorInfo, VddRequest, VideoCodec, VideoFrame,
};
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
    /// Frame source override (synthetic desktop for `--link-bench`); `None` = real capture.
    pub frame_source: Option<FrameSourceFactory>,
}

#[derive(Debug, Clone)]
struct StreamParams {
    target_fps: u32,
    monitor_index: u32,
    /// Resolution cap `(max_long, max_short)`, `(0, 0)` = native.
    res_cap: (u32, u32),
}

/// Viewer keyframe requests are honoured at most this often. Every request used to force an
/// IDR immediately; on a congested link that turned into an IDR storm (each 2K IDR is 100-300 KB).
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(1000);
/// After a real screen change (or a keyframe) keep feeding this many repeat frames so pictures
/// held inside async encoders/decoders come out within ~50 ms instead of waiting for the next
/// change or the 1 s keep-alive. Repeats of a static picture encode to a few hundred bytes.
const TAIL_FLUSH_FRAMES: u32 = 3;
/// Re-send the last picture after this much silence (primes decoders, refines static quality).
const KEEPALIVE_REFRESH: Duration = Duration::from_secs(1);
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// ABR target changes smaller than this fraction are not pushed to the encoder.
const BITRATE_CHANGE_MIN_FRAC: f64 = 0.04;

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
    let (mut video_reader, mut video_writer, _) = video.into_split();

    let (ctrl_out_tx, mut ctrl_out_rx) = mpsc::channel::<ControlMsg>(128);
    let (video_tx, mut video_rx) = mpsc::channel::<VideoFrame>(2);

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_notify = Arc::new(tokio::sync::Notify::new());
    opts.pending_channels
        .set_active_session(Arc::clone(&stop_flag), Arc::clone(&stop_notify));
    // Session start needs a keyframe, but the capture loop already forces one locally.
    let force_keyframe = Arc::new(AtomicBool::new(false));
    let last_encode_us = Arc::new(AtomicU32::new(0));
    let stream_params = Arc::new(Mutex::new(StreamParams {
        target_fps: target_fps.clamp(10, 120),
        monitor_index: initial_monitor,
        res_cap: (0, 0),
    }));
    let flow = Arc::new(Mutex::new(FlowController::new(
        bitrate_kbps.clamp(300, 100_000),
        Instant::now(),
    )));

    // Advertise flow-control support: viewers start sending FrameAcks once they see HostStats.
    let _ = ctrl_out_tx.try_send(ControlMsg::HostStats(HostStreamStats::default()));

    // FrameAcks arrive on the otherwise idle reverse direction of the video channel, so they never
    // queue behind clipboard/input traffic. Dedicated reader task (cancel-safety red line #7).
    let ack_flow = Arc::clone(&flow);
    let video_ack_task = tokio::spawn(async move {
        while let Ok(raw) = video_reader.recv_msg().await {
            if let Ok(ControlMsg::FrameAck { frame_id, viewer_us }) =
                bincode::deserialize::<ControlMsg>(&raw)
            {
                ack_flow
                    .lock()
                    .unwrap()
                    .on_ack(frame_id, viewer_us, Instant::now());
            }
        }
    });
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
    let cap_flow = Arc::clone(&flow);
    let cap_source = opts.frame_source.clone();

    let capture_handle = std::thread::Builder::new()
        .name("flashdesk-host-capture".into())
        .spawn(move || {
            run_capture_encode_loop(CaptureLoop {
                codec,
                agent_mode,
                params: cap_params,
                stop: cap_stop,
                stop_notify: cap_stop_notify,
                force_keyframe: cap_kf,
                last_encode_us: cap_enc_us,
                video_tx,
                ctrl_tx: cap_ctrl_tx,
                flow: cap_flow,
                frame_source: cap_source,
            });
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
                // Monitor switches force their own keyframe inside the capture loop.
                let mut p = stream_params.lock().unwrap();
                p.target_fps = target_fps.clamp(10, 120);
                p.monitor_index = monitor_index;
                drop(p);
                // The slider is the ABR ceiling; the controller picks the actual bitrate.
                flow.lock()
                    .unwrap()
                    .set_max_kbps(bitrate_kbps.clamp(300, 100_000));
            }
            ControlMsg::SetResolutionCap {
                max_long,
                max_short,
            } => {
                stream_params.lock().unwrap().res_cap = (max_long, max_short);
            }
            ControlMsg::FrameAck {
                frame_id,
                viewer_us,
            } => {
                // Normally on the video channel; accepted here too for robustness.
                flow.lock()
                    .unwrap()
                    .on_ack(frame_id, viewer_us, Instant::now());
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
    video_ack_task.abort();
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

struct CaptureLoop {
    codec: VideoCodec,
    agent_mode: bool,
    params: Arc<Mutex<StreamParams>>,
    stop: Arc<AtomicBool>,
    stop_notify: Arc<tokio::sync::Notify>,
    /// Viewer keyframe requests (throttled to `KEYFRAME_MIN_INTERVAL`).
    force_keyframe: Arc<AtomicBool>,
    last_encode_us: Arc<AtomicU32>,
    video_tx: mpsc::Sender<VideoFrame>,
    ctrl_tx: mpsc::Sender<ControlMsg>,
    flow: Arc<Mutex<FlowController>>,
    frame_source: Option<FrameSourceFactory>,
}

fn make_encoder(
    codec: VideoCodec,
    software: bool,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<Box<dyn VideoEncoder>> {
    if software {
        Ok(Box::new(OpenH264Encoder::new(width, height, fps, bitrate_kbps)?))
    } else {
        create_encoder(codec, width, height, fps, bitrate_kbps)
    }
}

/// Downscales a captured frame to fit the viewer's resolution cap (no-op when uncapped/smaller).
fn apply_resolution_cap(raw: RawFrame, cap: (u32, u32)) -> RawFrame {
    if cap == (0, 0) {
        return raw;
    }
    let (w, h) = fit_within(raw.width, raw.height, cap.0, cap.1);
    if (w, h) == (raw.width, raw.height) {
        return raw;
    }
    RawFrame {
        nv12: scale_nv12(&raw.nv12, raw.width, raw.height, w, h),
        width: w,
        height: h,
        ..raw
    }
}

fn bitrate_differs(target_kbps: u32, cur_kbps: u32) -> bool {
    let (t, c) = (target_kbps as f64, cur_kbps.max(1) as f64);
    (t - c).abs() / c >= BITRATE_CHANGE_MIN_FRAC
}

/// One captured frame handed from the capture stage to the encode stage.
struct EncodeJob {
    raw: RawFrame,
    force_keyframe: bool,
    target_kbps: u32,
    fps: u32,
    monitor_index: u32,
}

/// State the two pipeline stages share.
struct PipelineShared {
    /// Set by the encode stage after an encoder error: next frame must be a keyframe.
    need_keyframe: AtomicBool,
    last_keyframe_at: Mutex<Option<Instant>>,
    last_sent_at: Mutex<Instant>,
    /// Encoder call time accumulated for the current stats window.
    encode_time: Mutex<(Duration, u32)>,
}

/// Capture stage (this thread) + encode stage (its own thread), connected by a one-slot queue.
///
/// Capturing/colour-converting frame N+1 overlaps with encoding frame N, so throughput is
/// `max(capture, encode)` instead of `capture + encode` (2K capture + convert + downscale is
/// ~10 ms, a software encode another ~10 ms: sequential that is ~45 fps, pipelined ~60 fps).
/// Frames are still never dropped after encoding: the encode stage *waits* for the network
/// writer, and the capture stage stops producing while the writer or the flow gate is busy.
fn run_capture_encode_loop(cl: CaptureLoop) {
    let CaptureLoop {
        codec,
        agent_mode,
        params,
        stop,
        stop_notify,
        force_keyframe,
        last_encode_us,
        video_tx,
        ctrl_tx,
        flow,
        frame_source,
    } = cl;
    let init = params.lock().unwrap().clone();
    let mut cur_monitor = init.monitor_index;
    let mut cur_res_cap = init.res_cap;

    let mut capturer = match open_frame_source(frame_source.as_ref(), cur_monitor, agent_mode) {
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

    let (native_w, native_h) = capturer.dimensions();
    let (mut cur_w, mut cur_h) = fit_within(native_w, native_h, cur_res_cap.0, cur_res_cap.1);
    let shared = Arc::new(PipelineShared {
        need_keyframe: AtomicBool::new(false),
        last_keyframe_at: Mutex::new(None),
        last_sent_at: Mutex::new(Instant::now()),
        encode_time: Mutex::new((Duration::ZERO, 0)),
    });
    let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<EncodeJob>(1);

    let encode_handle = {
        let stage = EncodeStage {
            codec,
            jobs: job_rx,
            video_tx: video_tx.clone(),
            ctrl_tx: ctrl_tx.clone(),
            flow: Arc::clone(&flow),
            shared: Arc::clone(&shared),
            stop: Arc::clone(&stop),
            stop_notify: Arc::clone(&stop_notify),
            last_encode_us,
            init_dims: (cur_w, cur_h),
            init_fps: init.target_fps,
            init_kbps: flow.lock().unwrap().target_kbps(),
        };
        match std::thread::Builder::new()
            .name("flashdesk-host-encode".into())
            .spawn(move || run_encode_stage(stage))
        {
            Ok(h) => h,
            Err(e) => {
                let _ = ctrl_tx.try_send(ControlMsg::Notice(format!("编码线程启动失败: {e}")));
                stop.store(true, Ordering::Relaxed);
                stop_notify.notify_waiters();
                return;
            }
        }
    };

    let mut next_tick = Instant::now();
    // Local keyframe reasons (start, monitor switch, desktop re-acquire) are honoured
    // immediately; viewer requests go through the KEYFRAME_MIN_INTERVAL throttle.
    let mut need_keyframe = true;
    let mut prime_frames_left: u32 = 6;
    let mut tail_frames_left: u32 = 0;
    let mut next_stats_at = Instant::now() + STATS_INTERVAL;
    // Per-stats-window counters (HUD / --link-bench diagnostics).
    let mut win_sender_busy = 0u32;
    let mut win_capture = (Duration::ZERO, 0u32);

    while !stop.load(Ordering::Relaxed) {
        let p = params.lock().unwrap().clone();

        if p.monitor_index != cur_monitor {
            match open_frame_source(frame_source.as_ref(), p.monitor_index, agent_mode) {
                Ok(new_cap) => {
                    capturer = new_cap;
                    cur_monitor = p.monitor_index;
                    need_keyframe = true;
                    prime_frames_left = prime_frames_left.max(4);
                }
                Err(e) => {
                    tracing::warn!("monitor switch failed: {e:#}");
                }
            }
        }
        if p.res_cap != cur_res_cap {
            // New size -> the encode stage re-creates its encoder (starts with an IDR).
            cur_res_cap = p.res_cap;
            prime_frames_left = prime_frames_left.max(2);
        }

        let interval = Duration::from_micros(1_000_000 / p.target_fps.max(1) as u64);
        // Precise pacing: a plain sleep of a few ms can take a full 15 ms OS tick and cap the
        // stream around 40 fps (see timing.rs).
        crate::timing::sleep_until(next_tick);
        next_tick = Instant::now() + interval;

        let now = Instant::now();
        if now >= next_stats_at {
            let avg_ms = |(sum, n): (Duration, u32)| {
                if n == 0 {
                    0.0
                } else {
                    sum.as_secs_f32() * 1000.0 / n as f32
                }
            };
            let mut stats = flow.lock().unwrap().take_stats(now);
            stats.stream_width = cur_w;
            stats.stream_height = cur_h;
            stats.sender_busy_ticks = std::mem::take(&mut win_sender_busy);
            stats.captured_frames = win_capture.1;
            stats.capture_ms = avg_ms(std::mem::take(&mut win_capture));
            stats.encode_ms = avg_ms(std::mem::take(&mut *shared.encode_time.lock().unwrap()));
            let _ = ctrl_tx.try_send(ControlMsg::HostStats(stats));
            next_stats_at = now + STATS_INTERVAL;
        }

        // Backpressure *before* capture + encode: a frame that is never captured never has to
        // be dropped, so the reference chain stays intact and no recovery keyframe is needed
        // (the old drop-after-encode path forced an IDR on every congestion event).
        if video_tx.is_closed() {
            break;
        }
        if video_tx.capacity() == 0 {
            win_sender_busy += 1;
            continue;
        }
        let (may_send, target_kbps) = {
            let mut f = flow.lock().unwrap();
            (f.can_send(now), f.target_kbps())
        };
        if !may_send {
            // Network or viewer decoder behind: skip this tick (screen changes accumulate).
            continue;
        }

        let last_kf = *shared.last_keyframe_at.lock().unwrap();
        let remote_kf_allowed = last_kf.is_none_or(|t| t.elapsed() >= KEYFRAME_MIN_INTERVAL);
        let remote_kf = remote_kf_allowed && force_keyframe.swap(false, Ordering::Relaxed);
        if remote_kf {
            prime_frames_left = prime_frames_left.max(2);
        }
        if shared.need_keyframe.swap(false, Ordering::Relaxed) {
            need_keyframe = true;
        }
        let req_kf = need_keyframe || remote_kf;
        let periodic_refresh = shared.last_sent_at.lock().unwrap().elapsed() >= KEEPALIVE_REFRESH;
        let force_capture =
            req_kf || prime_frames_left > 0 || tail_frames_left > 0 || periodic_refresh;
        let wait_ms = (interval.as_millis() as u32).clamp(4, 30);

        let cap_t0 = Instant::now();
        match capturer.capture_next(wait_ms, force_capture) {
            Ok(CaptureOutcome::Frame(raw)) => {
                let changed = raw.content_changed;
                let raw = apply_resolution_cap(raw, cur_res_cap);
                // Includes any wait for the next desktop update plus BGRA->NV12 (+ downscale).
                win_capture.0 += cap_t0.elapsed();
                win_capture.1 += 1;
                cur_w = raw.width;
                cur_h = raw.height;
                let job = EncodeJob {
                    raw,
                    force_keyframe: req_kf,
                    target_kbps,
                    fps: p.target_fps,
                    monitor_index: cur_monitor,
                };
                // Blocks only while the encoder is still busy with the previous frame *and* one
                // job is already queued: capture stays at most one frame ahead.
                if job_tx.send(job).is_err() {
                    break;
                }
                need_keyframe = false;
                // Pipeline flush: after a real change (or a keyframe) keep feeding a few repeats
                // so pictures held inside async encoders/decoders come out within ~50 ms
                // instead of waiting for the next change or the 1 s keep-alive.
                if changed || req_kf {
                    tail_frames_left = TAIL_FLUSH_FRAMES;
                } else {
                    tail_frames_left = tail_frames_left.saturating_sub(1);
                }
                prime_frames_left = prime_frames_left.saturating_sub(1);
            }
            Ok(CaptureOutcome::Unchanged) => {}
            Ok(CaptureOutcome::Reacquired) => {
                need_keyframe = true;
                prime_frames_left = prime_frames_left.max(4);
            }
            Err(e) => {
                tracing::warn!("capture error: {e:#}");
                std::thread::sleep(Duration::from_millis(50));
                if let Ok(new_cap) =
                    open_frame_source(frame_source.as_ref(), cur_monitor, agent_mode)
                {
                    capturer = new_cap;
                    need_keyframe = true;
                    prime_frames_left = prime_frames_left.max(4);
                }
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    drop(job_tx);
    let _ = encode_handle.join();
    stop_notify.notify_waiters();
}

struct EncodeStage {
    codec: VideoCodec,
    jobs: std::sync::mpsc::Receiver<EncodeJob>,
    video_tx: mpsc::Sender<VideoFrame>,
    ctrl_tx: mpsc::Sender<ControlMsg>,
    flow: Arc<Mutex<FlowController>>,
    shared: Arc<PipelineShared>,
    stop: Arc<AtomicBool>,
    stop_notify: Arc<tokio::sync::Notify>,
    last_encode_us: Arc<AtomicU32>,
    init_dims: (u32, u32),
    init_fps: u32,
    init_kbps: u32,
}

fn run_encode_stage(stage: EncodeStage) {
    let EncodeStage {
        codec,
        jobs,
        video_tx,
        ctrl_tx,
        flow,
        shared,
        stop,
        stop_notify,
        last_encode_us,
        init_dims,
        init_fps,
        init_kbps,
    } = stage;
    let (mut cur_w, mut cur_h) = init_dims;
    let mut cur_bitrate = init_kbps;
    let mut encoder = match make_encoder(codec, false, cur_w, cur_h, init_fps, cur_bitrate) {
        Ok(enc) => enc,
        Err(e) => {
            let _ = ctrl_tx.try_send(ControlMsg::Notice(format!("编码器初始化失败: {e:#}")));
            stop.store(true, Ordering::Relaxed);
            stop_notify.notify_waiters();
            return;
        }
    };
    // create_encoder may already have fallen back to OpenH264; don't re-probe MF on resizes.
    let mut software_encoder = encoder.backend_name() == "OpenH264-SW";
    let mut frame_id = 0u64;
    let mut consecutive_enc_errors: u32 = 0;

    while let Ok(job) = jobs.recv() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let raw = job.raw;
        if (raw.width, raw.height) != (cur_w, cur_h) {
            match make_encoder(codec, software_encoder, raw.width, raw.height, job.fps, cur_bitrate) {
                Ok(enc) => {
                    encoder = enc;
                    cur_w = raw.width;
                    cur_h = raw.height;
                }
                Err(e) => {
                    tracing::warn!(
                        "encoder re-create for {}x{} failed: {e:#}",
                        raw.width,
                        raw.height
                    );
                    continue;
                }
            }
        }
        if bitrate_differs(job.target_kbps, cur_bitrate) && encoder.set_bitrate(job.target_kbps).is_ok() {
            cur_bitrate = job.target_kbps;
        }

        let t0 = Instant::now();
        match encoder.encode(&raw, job.force_keyframe) {
            Ok(pkt) => {
                let took = t0.elapsed();
                {
                    let mut w = shared.encode_time.lock().unwrap();
                    w.0 += took;
                    w.1 += 1;
                }
                let enc_us = took.as_micros() as u32;
                last_encode_us.store(enc_us, Ordering::Relaxed);
                consecutive_enc_errors = 0;
                if pkt.data.is_empty() {
                    continue;
                }
                frame_id += 1;
                let bytes = pkt.data.len();
                let vf = VideoFrame {
                    frame_id,
                    timestamp_us: raw.timestamp_us,
                    codec: encoder.codec(),
                    width: raw.width,
                    height: raw.height,
                    monitor_index: job.monitor_index,
                    is_keyframe: pkt.is_keyframe,
                    encode_us: enc_us,
                    encoder_name: encoder.backend_name().to_string(),
                    data: pkt.data,
                };
                let sent_at = Instant::now();
                flow.lock()
                    .unwrap()
                    .on_frame_sent(frame_id, bytes, vf.is_keyframe, sent_at);
                if vf.is_keyframe {
                    *shared.last_keyframe_at.lock().unwrap() = Some(sent_at);
                }
                *shared.last_sent_at.lock().unwrap() = sent_at;
                // Never drop an encoded frame (it would break the reference chain): wait for
                // the network writer instead; the capture stage pauses meanwhile.
                if video_tx.blocking_send(vf).is_err() {
                    break;
                }
            }
            Err(e) => {
                tracing::error!("encode error: {e:#}");
                shared.need_keyframe.store(true, Ordering::Relaxed);
                consecutive_enc_errors += 1;
                if consecutive_enc_errors >= 3 && codec == VideoCodec::H264 && !software_encoder {
                    if let Ok(sw_enc) = OpenH264Encoder::new(cur_w, cur_h, job.fps, cur_bitrate) {
                        encoder = Box::new(sw_enc);
                        software_encoder = true;
                        consecutive_enc_errors = 0;
                        let _ = ctrl_tx.try_send(ControlMsg::Notice(
                            "已自动切换至 OpenH264 软件编码".into(),
                        ));
                    }
                }
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    stop_notify.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_cap_downscales_and_keeps_flags() {
        let raw = RawFrame {
            width: 2560,
            height: 1440,
            nv12: vec![100u8; 2560 * 1440 * 3 / 2],
            timestamp_us: 42,
            content_changed: true,
        };
        let out = apply_resolution_cap(raw.clone(), (1920, 1080));
        assert_eq!((out.width, out.height), (1920, 1080));
        assert_eq!(out.nv12.len(), 1920 * 1080 * 3 / 2);
        assert_eq!((out.timestamp_us, out.content_changed), (42, true));
        let same = apply_resolution_cap(raw, (0, 0));
        assert_eq!((same.width, same.height), (2560, 1440));
    }

    #[test]
    fn small_bitrate_wiggles_are_ignored() {
        assert!(!bitrate_differs(4050, 4000));
        assert!(bitrate_differs(4200, 4000));
        assert!(bitrate_differs(3000, 4000));
    }
}

