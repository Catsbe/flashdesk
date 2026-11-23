use crate::clipboard::ClipboardSync;
use crate::codec::{create_decoder, CodecCapabilities, DecodedFrame};
use crate::net::connect_and_auth_relay;
use crate::net::direct::connect_direct_channel;
use crate::transfer::client_manager::{spawn_file_client, FileClientHandle, FileCommand, FileTransportMode};
use anyhow::Result;
use protocol::auth::{DeviceBundle, RelayNodeConfig};
use protocol::noise::NoiseStream;
use protocol::relay::{ChannelKind, ConnectionPurpose};
use protocol::session::{ControlMsg, InputEvent, MonitorInfo, VddInfo, VddRequest, VideoCodec, VideoFrame};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct HudStats {
    pub fps: f32,
    pub rtt_ms: u32,
    pub bitrate_mbps: f32,
    pub codec_label: String,
    pub encode_us: u32,
    pub remote_res: String,
    pub relay_name: String,
    pub connecting: bool,
    pub status_notice: Option<String>,
}

pub enum ViewerCommand {
    Input(InputEvent),
    SetMonitor(u32),
    SetBitrate(u32),
    SetFps(u32),
    SetClipboardEnabled(bool),
    VddAction(VddRequest),
    Disconnect,
}

#[derive(Clone)]
pub enum ReconnectTarget {
    Relay {
        target_device_id: Uuid,
        target_device_name: String,
    },
    Direct,
}

#[derive(Clone)]
pub struct ViewerSessionHandle {
    pub peer_name: String,
    pub via_label: String,
    pub reconnect_target: ReconnectTarget,
    pub latest_frame: Arc<Mutex<Option<DecodedFrame>>>,
    pub hud: Arc<Mutex<HudStats>>,
    pub remote_monitors: Arc<Mutex<Vec<MonitorInfo>>>,
    pub active_monitor: Arc<Mutex<u32>>,
    pub vdd_info: Arc<Mutex<Option<VddInfo>>>,
    pub disconnected_reason: Arc<Mutex<Option<String>>>,
    pub cmd_tx: mpsc::UnboundedSender<ViewerCommand>,
    pub file_client: FileClientHandle,
    pub clipboard_enabled: Arc<AtomicBool>,
    pub available_relays: Vec<RelayNodeConfig>,
    pub current_ctrl_relay_id: Option<String>,
}

impl ViewerSessionHandle {
    pub fn send(&self, cmd: ViewerCommand) {
        if let ViewerCommand::SetClipboardEnabled(en) = cmd {
            self.clipboard_enabled.store(en, Ordering::Relaxed);
        }
        let _ = self.cmd_tx.send(cmd);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn start_relay_viewer_session(
    bundle: DeviceBundle,
    revoked_ids: Vec<Uuid>,
    ctrl_relay: RelayNodeConfig,
    preferred_file_relay: Option<RelayNodeConfig>,
    target_device_id: Uuid,
    target_device_name: String,
    local_caps: CodecCapabilities,
    initial_fps: u32,
    initial_bitrate_kbps: u32,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> ViewerSessionHandle {
    let via_label = format!("中转: {}", ctrl_relay.name);
    let available_relays = bundle.relay_nodes.clone();
    let current_ctrl_relay_id = Some(ctrl_relay.id.clone());

    let (ctrl_out_tx, ctrl_out_rx) = mpsc::channel::<ControlMsg>(256);
    let file_client = spawn_file_client(
        FileTransportMode::Relay {
            bundle: bundle.clone(),
            revoked_ids: revoked_ids.clone(),
            peer_device_id: target_device_id,
            fallback_relay: ctrl_relay.clone(),
        },
        preferred_file_relay,
        ctrl_out_tx.clone(),
        Arc::clone(&repaint),
    );

    let (handle, cmd_rx) = build_handle(
        target_device_name.clone(),
        via_label,
        &ctrl_relay.name,
        ReconnectTarget::Relay {
            target_device_id,
            target_device_name,
        },
        file_client,
        available_relays,
        current_ctrl_relay_id,
    );
    let inner = handle.clone();

    tokio::spawn(async move {
        let session_id = Uuid::new_v4();
        let ident = bundle.identity.clone();
        let root_pub = bundle.root_pubkey;
        let relay_addr = ctrl_relay.addr.clone();

        let open_ctrl = async {
            let s = connect_and_auth_relay(
                &relay_addr,
                &ident,
                ConnectionPurpose::OpenSession {
                    target_device_id,
                    session_id,
                    channel: ChannelKind::Control,
                },
            )
            .await?;
            tokio::time::timeout(
                Duration::from_secs(10),
                NoiseStream::handshake_initiator(s, &root_pub, &ident, &revoked_ids),
            )
            .await
            .map_err(|_| anyhow::anyhow!("控制通道加密握手超时"))?
            .map_err(anyhow::Error::from)
        };

        let open_video = async {
            let s = connect_and_auth_relay(
                &relay_addr,
                &ident,
                ConnectionPurpose::OpenSession {
                    target_device_id,
                    session_id,
                    channel: ChannelKind::Video,
                },
            )
            .await?;
            tokio::time::timeout(
                Duration::from_secs(10),
                NoiseStream::handshake_initiator(s, &root_pub, &ident, &revoked_ids),
            )
            .await
            .map_err(|_| anyhow::anyhow!("视频通道加密握手超时"))?
            .map_err(anyhow::Error::from)
        };

        match tokio::try_join!(open_ctrl, open_video) {
            Ok((ctrl, video)) => {
                if let Err(e) = run_viewer_loop(
                    ctrl,
                    video,
                    local_caps,
                    initial_fps,
                    initial_bitrate_kbps,
                    inner.clone(),
                    ctrl_out_tx,
                    ctrl_out_rx,
                    cmd_rx,
                    repaint.clone(),
                )
                .await
                {
                    *inner.disconnected_reason.lock().unwrap() = Some(e.to_string());
                    repaint();
                }
            }
            Err(e) => {
                *inner.disconnected_reason.lock().unwrap() = Some(format!("建立会话失败: {e}"));
                inner.hud.lock().unwrap().connecting = false;
                inner.file_client.send(FileCommand::Shutdown);
                repaint();
            }
        }
    });

    handle
}

#[allow(clippy::too_many_arguments)]
pub fn start_direct_viewer_session(
    bundle: DeviceBundle,
    revoked_ids: Vec<Uuid>,
    direct_addr: String,
    local_caps: CodecCapabilities,
    initial_fps: u32,
    initial_bitrate_kbps: u32,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> ViewerSessionHandle {
    let via_label = format!("直连: {direct_addr}");
    let available_relays = bundle.relay_nodes.clone();

    let (ctrl_out_tx, ctrl_out_rx) = mpsc::channel::<ControlMsg>(256);
    let file_client = spawn_file_client(
        FileTransportMode::Direct {
            bundle: bundle.clone(),
            revoked_ids: revoked_ids.clone(),
            direct_addr: direct_addr.clone(),
        },
        None,
        ctrl_out_tx.clone(),
        Arc::clone(&repaint),
    );

    let (handle, cmd_rx) = build_handle(
        direct_addr.clone(),
        via_label.clone(),
        &via_label,
        ReconnectTarget::Direct,
        file_client,
        available_relays,
        None,
    );
    let inner = handle.clone();

    tokio::spawn(async move {
        let session_id = Uuid::new_v4();
        let ident = bundle.identity.clone();
        let root_pub = bundle.root_pubkey;

        let open_ctrl = async {
            let s = connect_direct_channel(&direct_addr, session_id, ChannelKind::Control).await?;
            tokio::time::timeout(
                Duration::from_secs(10),
                NoiseStream::handshake_initiator(s, &root_pub, &ident, &revoked_ids),
            )
            .await
            .map_err(|_| anyhow::anyhow!("直连控制握手超时"))?
            .map_err(anyhow::Error::from)
        };
        let open_video = async {
            let s = connect_direct_channel(&direct_addr, session_id, ChannelKind::Video).await?;
            tokio::time::timeout(
                Duration::from_secs(10),
                NoiseStream::handshake_initiator(s, &root_pub, &ident, &revoked_ids),
            )
            .await
            .map_err(|_| anyhow::anyhow!("直连视频握手超时"))?
            .map_err(anyhow::Error::from)
        };

        match tokio::try_join!(open_ctrl, open_video) {
            Ok((ctrl, video)) => {
                if let Err(e) = run_viewer_loop(
                    ctrl,
                    video,
                    local_caps,
                    initial_fps,
                    initial_bitrate_kbps,
                    inner.clone(),
                    ctrl_out_tx,
                    ctrl_out_rx,
                    cmd_rx,
                    repaint.clone(),
                )
                .await
                {
                    *inner.disconnected_reason.lock().unwrap() = Some(e.to_string());
                    repaint();
                }
            }
            Err(e) => {
                *inner.disconnected_reason.lock().unwrap() = Some(format!("直连失败: {e}"));
                inner.hud.lock().unwrap().connecting = false;
                inner.file_client.send(FileCommand::Shutdown);
                repaint();
            }
        }
    });

    handle
}

#[allow(clippy::too_many_arguments)]
fn build_handle(
    peer_name: String,
    via_label: String,
    relay_name: &str,
    reconnect_target: ReconnectTarget,
    file_client: FileClientHandle,
    available_relays: Vec<RelayNodeConfig>,
    current_ctrl_relay_id: Option<String>,
) -> (ViewerSessionHandle, mpsc::UnboundedReceiver<ViewerCommand>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let handle = ViewerSessionHandle {
        peer_name,
        via_label,
        reconnect_target,
        latest_frame: Arc::new(Mutex::new(None)),
        hud: Arc::new(Mutex::new(HudStats {
            relay_name: relay_name.to_string(),
            connecting: true,
            ..Default::default()
        })),
        remote_monitors: Arc::new(Mutex::new(Vec::new())),
        active_monitor: Arc::new(Mutex::new(0)),
        vdd_info: Arc::new(Mutex::new(None)),
        disconnected_reason: Arc::new(Mutex::new(None)),
        cmd_tx,
        file_client,
        clipboard_enabled: Arc::new(AtomicBool::new(true)),
        available_relays,
        current_ctrl_relay_id,
    };
    (handle, cmd_rx)
}

#[allow(clippy::too_many_arguments)]
async fn run_viewer_loop<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    mut ctrl: NoiseStream<S>,
    video: NoiseStream<S>,
    local_caps: CodecCapabilities,
    mut target_fps: u32,
    mut bitrate_kbps: u32,
    handle: ViewerSessionHandle,
    ctrl_out_tx: mpsc::Sender<ControlMsg>,
    mut ctrl_out_rx: mpsc::Receiver<ControlMsg>,
    mut cmd_rx: mpsc::UnboundedReceiver<ViewerCommand>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> Result<()> {
    let hello_raw = tokio::time::timeout(Duration::from_secs(10), ctrl.recv_msg())
        .await
        .map_err(|_| anyhow::anyhow!("等待远程主机 Hello 响应超时"))??;
    let hello: ControlMsg = bincode::deserialize(&hello_raw)?;
    let (host_enc, monitors, active_mon) = match hello {
        ControlMsg::Hello {
            encode_codecs,
            monitors,
            active_monitor,
            ..
        } => (encode_codecs, monitors, active_monitor),
        other => anyhow::bail!("expected Hello, got {other:?}"),
    };

    *handle.remote_monitors.lock().unwrap() = monitors;
    *handle.active_monitor.lock().unwrap() = active_mon;

    let chosen_codec = if host_enc.contains(&VideoCodec::Hevc)
        && local_caps.decode_codecs.contains(&VideoCodec::Hevc)
    {
        VideoCodec::Hevc
    } else {
        VideoCodec::H264
    };

    let mut cur_monitor = active_mon;
    let start_msg = ControlMsg::StartStream {
        codec: chosen_codec,
        target_fps,
        bitrate_kbps,
        monitor_index: cur_monitor,
    };
    ctrl.send_msg(&bincode::serialize(&start_msg)?).await?;
    handle.hud.lock().unwrap().codec_label = format!("{} (等待首帧...)", chosen_codec.label());
    repaint();

    let (mut ctrl_reader, mut ctrl_writer, _) = ctrl.into_split();
    let (mut video_reader, _video_writer, _) = video.into_split();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let (decode_tx, mut decode_rx) = mpsc::channel::<VideoFrame>(8);

    // Bidirectional clipboard sync
    let clipboard_sync = ClipboardSync::with_enabled_flag(Arc::clone(&handle.clipboard_enabled));
    {
        let cb_tx = ctrl_out_tx.clone();
        clipboard_sync.spawn_watcher(Arc::clone(&stop_flag), move |data| {
            let _ = cb_tx.try_send(ControlMsg::Clipboard(data));
        });
    }

    // Dedicated decode thread
    let dec_handle = handle.clone();
    let dec_stop = Arc::clone(&stop_flag);
    let dec_repaint = Arc::clone(&repaint);
    let dec_ctrl_tx = ctrl_out_tx.clone();

    let decode_thread = std::thread::Builder::new()
        .name("flashdesk-viewer-decode".into())
        .spawn(move || {
            let mut decoder = match create_decoder(chosen_codec) {
                Ok(d) => d,
                Err(e) => {
                    *dec_handle.disconnected_reason.lock().unwrap() =
                        Some(format!("解码器启动失败: {e}"));
                    dec_stop.store(true, Ordering::Relaxed);
                    dec_repaint();
                    return;
                }
            };

            let mut frames_in_window = 0u32;
            let mut bytes_in_window = 0usize;
            let mut window_start = Instant::now();
            let mut has_decoded_any = false;
            let mut stall_kf_sent = false;

            while let Some(vf) = decode_rx.blocking_recv() {
                if dec_stop.load(Ordering::Relaxed) {
                    break;
                }
                bytes_in_window += vf.data.len();

                match decoder.decode(&vf.data) {
                    Ok(Some(decoded)) => {
                        has_decoded_any = true;
                        frames_in_window += 1;
                        *dec_handle.latest_frame.lock().unwrap() = Some(decoded);
                        {
                            let mut hud = dec_handle.hud.lock().unwrap();
                            hud.connecting = false;
                            hud.encode_us = vf.encode_us;
                            hud.remote_res = format!("{}x{}", vf.width, vf.height);
                            hud.codec_label =
                                format!("{} | {}", vf.codec.label(), vf.encoder_name);
                        }
                        dec_repaint();
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("decode failed: {e}, requesting keyframe");
                        let _ = dec_ctrl_tx.try_send(ControlMsg::RequestKeyframe);
                    }
                }

                let elapsed = window_start.elapsed();
                if !has_decoded_any && !stall_kf_sent && elapsed >= Duration::from_secs(2) {
                    stall_kf_sent = true;
                    let _ = dec_ctrl_tx.try_send(ControlMsg::RequestKeyframe);
                }
                if elapsed >= Duration::from_secs(1) {
                    let secs = elapsed.as_secs_f32();
                    let mut hud = dec_handle.hud.lock().unwrap();
                    hud.fps = frames_in_window as f32 / secs;
                    hud.bitrate_mbps = (bytes_in_window as f32 * 8.0) / (secs * 1_000_000.0);
                    frames_in_window = 0;
                    bytes_in_window = 0;
                    window_start = Instant::now();
                    drop(hud);
                    dec_repaint();
                }
            }
        })?;

    let ended_notify = Arc::new(tokio::sync::Notify::new());

    let vr_stop = Arc::clone(&stop_flag);
    let vr_notify = Arc::clone(&ended_notify);
    let vr_ctrl = ctrl_out_tx.clone();
    let video_recv_task = tokio::spawn(async move {
        loop {
            let raw = match video_reader.recv_msg().await {
                Ok(b) => b,
                Err(_) => break,
            };
            let Ok(vf) = bincode::deserialize::<VideoFrame>(&raw) else {
                continue;
            };
            match decode_tx.try_send(vf) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let _ = vr_ctrl.try_send(ControlMsg::RequestKeyframe);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
        vr_stop.store(true, Ordering::Relaxed);
        vr_notify.notify_waiters();
    });

    let cw_stop = Arc::clone(&stop_flag);
    let cw_notify = Arc::clone(&ended_notify);
    let ctrl_write_task = tokio::spawn(async move {
        while let Some(msg) = ctrl_out_rx.recv().await {
            let Ok(bytes) = bincode::serialize(&msg) else {
                continue;
            };
            if ctrl_writer.send_msg(&bytes).await.is_err() {
                break;
            }
        }
        cw_stop.store(true, Ordering::Relaxed);
        cw_notify.notify_waiters();
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

    let mut ping_interval = tokio::time::interval(Duration::from_secs(1));
    let session_start = Instant::now();
    let mut ping_seq = 0u64;
    let mut user_disconnected = false;
    let mut unexpected_reason: Option<String> = None;

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            _ = ended_notify.notified() => {
                if !user_disconnected {
                    unexpected_reason = Some("视频或控制发送通道已断开".into());
                }
                break;
            }
            _ = ping_interval.tick() => {
                ping_seq += 1;
                let sent_us = session_start.elapsed().as_micros() as u64;
                let _ = ctrl_out_tx.try_send(ControlMsg::Ping { seq: ping_seq, sent_us });
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ViewerCommand::Input(ev)) => {
                        let _ = ctrl_out_tx.try_send(ControlMsg::Input(ev));
                    }
                    Some(ViewerCommand::SetMonitor(idx)) => {
                        cur_monitor = idx;
                        *handle.active_monitor.lock().unwrap() = idx;
                        let _ = ctrl_out_tx.try_send(ControlMsg::UpdateStream {
                            target_fps,
                            bitrate_kbps,
                            monitor_index: cur_monitor,
                        });
                    }
                    Some(ViewerCommand::SetBitrate(kbps)) => {
                        bitrate_kbps = kbps;
                        let _ = ctrl_out_tx.try_send(ControlMsg::UpdateStream {
                            target_fps,
                            bitrate_kbps,
                            monitor_index: cur_monitor,
                        });
                    }
                    Some(ViewerCommand::SetFps(fps)) => {
                        target_fps = fps;
                        let _ = ctrl_out_tx.try_send(ControlMsg::UpdateStream {
                            target_fps,
                            bitrate_kbps,
                            monitor_index: cur_monitor,
                        });
                    }
                    Some(ViewerCommand::SetClipboardEnabled(_)) => {}
                    Some(ViewerCommand::VddAction(req)) => {
                        let _ = ctrl_out_tx.try_send(ControlMsg::VddAction(req));
                    }
                    Some(ViewerCommand::Disconnect) | None => {
                        user_disconnected = true;
                        break;
                    }
                }
            }
            ctrl_opt = ctrl_in_rx.recv() => {
                match ctrl_opt {
                    Some(Ok(raw)) => {
                        if let Ok(msg) = bincode::deserialize::<ControlMsg>(&raw) {
                            match msg {
                                ControlMsg::Pong { sent_us, encode_us, .. } => {
                                    let now_us = session_start.elapsed().as_micros() as u64;
                                    let rtt = (now_us.saturating_sub(sent_us) / 1000) as u32;
                                    let mut hud = handle.hud.lock().unwrap();
                                    hud.rtt_ms = rtt;
                                    hud.encode_us = encode_us;
                                    drop(hud);
                                    repaint();
                                }
                                ControlMsg::Notice(txt) => {
                                    handle.hud.lock().unwrap().status_notice = Some(txt);
                                    repaint();
                                }
                                ControlMsg::Clipboard(data) => {
                                    let sync = clipboard_sync.clone();
                                    std::thread::spawn(move || {
                                        let _ = sync.apply_remote(&data);
                                    });
                                }
                                ControlMsg::VddStatus(info) => {
                                    *handle.vdd_info.lock().unwrap() = Some(info);
                                    repaint();
                                }
                                ControlMsg::MonitorsUpdated(mons) => {
                                    *handle.remote_monitors.lock().unwrap() = mons;
                                    repaint();
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Err(e)) => {
                        unexpected_reason = Some(format!("控制通道断开: {e}"));
                        break;
                    }
                    None => {
                        unexpected_reason = Some("远程主机已关闭控制通道".into());
                        break;
                    }
                }
            }
        }
    }

    stop_flag.store(true, Ordering::Relaxed);
    if !user_disconnected {
        let mut dr = handle.disconnected_reason.lock().unwrap();
        if dr.is_none() {
            *dr = Some(unexpected_reason.unwrap_or_else(|| "远程会话已断开".into()));
        }
        drop(dr);
        handle.hud.lock().unwrap().connecting = false;
        repaint();
    }

    handle.file_client.send(FileCommand::Shutdown);
    ctrl_read_task.abort();
    video_recv_task.abort();
    ctrl_write_task.abort();
    let _ = video_recv_task.await;
    drop(ctrl_in_rx);
    drop(ctrl_out_tx);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = decode_thread.join();
    })
    .await;
    Ok(())
}
