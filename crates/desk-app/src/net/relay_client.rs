use crate::codec::CodecCapabilities;
use crate::config::AppConfig;
use crate::net::host_session::{run_host_session, HostSessionOptions, PendingHostChannels};
use crate::net::{connect_and_auth_relay, RelayStatus};
use crate::service::install::{query_service_state, ServiceState};
use crate::service::health::agent_healthy;
use protocol::auth::RelayNodeConfig;
use protocol::framing::{read_frame, write_frame};
use protocol::noise::NoiseStream;
use protocol::relay::{ConnectionPurpose, PresenceClientMsg, PresenceServerMsg};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub enum RelayCommand {
    ReloadConfig(AppConfig),
    RefreshDevices,
}

pub fn spawn_relay_manager(
    initial_cfg: AppConfig,
    caps: CodecCapabilities,
    monitor_count: u32,
    agent_mode: bool,
    status: Arc<Mutex<RelayStatus>>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> mpsc::UnboundedSender<RelayCommand> {
    let (tx, mut rx) = mpsc::unbounded_channel::<RelayCommand>();

    tokio::spawn(async move {
        let mut cfg = initial_cfg;
        let pending_channels = Arc::new(PendingHostChannels::new());

        loop {
            while let Ok(cmd) = rx.try_recv() {
                if let RelayCommand::ReloadConfig(new_cfg) = cmd {
                    cfg = new_cfg;
                }
            }

            let Some(bundle) = cfg.bundle.clone() else {
                update_status_offline(&status, None, None);
                repaint();
                match rx.recv().await {
                    Some(RelayCommand::ReloadConfig(new_cfg)) => {
                        cfg = new_cfg;
                        continue;
                    }
                    Some(RelayCommand::RefreshDevices) => continue,
                    None => break,
                }
            };

            let Some(node) = cfg.active_relay().cloned() else {
                update_status_offline(&status, None, Some("尚未配置任何中转节点".into()));
                repaint();
                match rx.recv().await {
                    Some(RelayCommand::ReloadConfig(new_cfg)) => {
                        cfg = new_cfg;
                        continue;
                    }
                    Some(RelayCommand::RefreshDevices) => continue,
                    None => break,
                }
            };

            let res = run_presence_once(
                &cfg,
                &bundle,
                &node,
                &caps,
                monitor_count,
                agent_mode,
                &pending_channels,
                &status,
                &repaint,
                &mut rx,
            )
            .await;

            match res {
                PresenceOutcome::Reload(new_cfg) => {
                    cfg = new_cfg;
                    continue;
                }
                PresenceOutcome::Disconnected(err) => {
                    update_status_offline(&status, Some(&node), Some(err));
                    repaint();
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                        cmd = rx.recv() => {
                            match cmd {
                                Some(RelayCommand::ReloadConfig(new_cfg)) => cfg = new_cfg,
                                Some(RelayCommand::RefreshDevices) => {}
                                None => break,
                            }
                        }
                    }
                }
                PresenceOutcome::Exit => break,
            }
        }
    });

    tx
}

enum PresenceOutcome {
    Reload(AppConfig),
    Disconnected(String),
    Exit,
}

/// The desktop GUI must defer incoming sessions to the SYSTEM agent ONLY while that agent is
/// actually alive. SCM "service running" alone is not enough: the Session-0 supervisor can be up
/// while its interactive agent failed to spawn or crashed — deferring in that state leaves nobody
/// accepting sessions ("connected once, then never again").
fn host_defers_to_agent(agent_mode: bool) -> bool {
    !agent_mode && query_service_state() == ServiceState::Running && agent_healthy()
}

#[allow(clippy::too_many_arguments)]
async fn run_presence_once(
    cfg: &AppConfig,
    bundle: &protocol::auth::DeviceBundle,
    node: &RelayNodeConfig,
    caps: &CodecCapabilities,
    monitor_count: u32,
    agent_mode: bool,
    pending: &Arc<PendingHostChannels>,
    status: &Arc<Mutex<RelayStatus>>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
    cmd_rx: &mut mpsc::UnboundedReceiver<RelayCommand>,
) -> PresenceOutcome {
    let stream = match connect_and_auth_relay(
        &node.addr,
        &bundle.identity,
        ConnectionPurpose::Presence,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return PresenceOutcome::Disconnected(e.to_string()),
    };

    {
        let mut st = status.lock().unwrap();
        st.connected = true;
        st.relay_id = node.id.clone();
        st.relay_name = node.name.clone();
        st.relay_addr = node.addr.clone();
        st.error = None;
    }
    repaint();

    let (mut reader, mut writer) = stream.into_split();

    let mut effective_accept = cfg.allow_incoming && !host_defers_to_agent(agent_mode);

    let os_label = if agent_mode {
        "Windows 11 (系统服务)"
    } else {
        "Windows 11"
    };

    let update_msg = PresenceClientMsg::UpdateInfo {
        monitor_count,
        encode_codecs: caps.encode_codecs.clone(),
        decode_codecs: caps.decode_codecs.clone(),
        os: os_label.into(),
        accept_incoming: effective_accept,
    };
    if let Ok(bytes) = bincode::serialize(&update_msg) {
        if write_frame(&mut writer, &bytes).await.is_err() {
            return PresenceOutcome::Disconnected("发送设备信息失败".into());
        }
    }
    if let Ok(bytes) = bincode::serialize(&PresenceClientMsg::ListDevices) {
        let _ = write_frame(&mut writer, &bytes).await;
    }

    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    let (srv_tx, mut srv_rx) = mpsc::channel::<Result<Vec<u8>, String>>(128);
    let _reader_guard = AbortOnDrop(tokio::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok(raw) => {
                    if srv_tx.send(Ok(raw)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = srv_tx.send(Err(e.to_string())).await;
                    break;
                }
            }
        }
    }));

    let mut hb_interval = tokio::time::interval(Duration::from_secs(5));
    let mut hb_seq = 0u64;
    let mut hb_sent: HashMap<u64, Instant> = HashMap::new();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(RelayCommand::ReloadConfig(new_cfg)) => {
                        return PresenceOutcome::Reload(new_cfg);
                    }
                    Some(RelayCommand::RefreshDevices) => {
                        if let Ok(bytes) = bincode::serialize(&PresenceClientMsg::ListDevices) {
                            let _ = write_frame(&mut writer, &bytes).await;
                        }
                    }
                    None => return PresenceOutcome::Exit,
                }
            }
            _ = hb_interval.tick() => {
                hb_seq += 1;
                hb_sent.insert(hb_seq, Instant::now());
                if hb_sent.len() > 16 {
                    let min_keep = hb_seq.saturating_sub(8);
                    hb_sent.retain(|&k, _| k >= min_keep);
                }
                if let Ok(bytes) = bincode::serialize(&PresenceClientMsg::Heartbeat(hb_seq)) {
                    if write_frame(&mut writer, &bytes).await.is_err() {
                        return PresenceOutcome::Disconnected("心跳发送失败".into());
                    }
                }
                // Service/agent health can change while this presence connection stays up
                // (agent crash, service restart, takeover recovery). Re-evaluate ownership and
                // push UpdateInfo immediately so the relay routes to whoever can actually host.
                let new_accept = cfg.allow_incoming && !host_defers_to_agent(agent_mode);
                if new_accept != effective_accept {
                    effective_accept = new_accept;
                    tracing::info!(
                        "accept_incoming -> {new_accept} (service/agent health changed)"
                    );
                    let update = PresenceClientMsg::UpdateInfo {
                        monitor_count,
                        encode_codecs: caps.encode_codecs.clone(),
                        decode_codecs: caps.decode_codecs.clone(),
                        os: os_label.into(),
                        accept_incoming: new_accept,
                    };
                    if let Ok(bytes) = bincode::serialize(&update) {
                        if write_frame(&mut writer, &bytes).await.is_err() {
                            return PresenceOutcome::Disconnected("发送设备信息失败".into());
                        }
                    }
                }
            }
            srv_opt = srv_rx.recv() => {
                let raw = match srv_opt {
                    Some(Ok(r)) => r,
                    Some(Err(e)) => return PresenceOutcome::Disconnected(format!("连接断开: {e}")),
                    None => return PresenceOutcome::Disconnected("中转连接已关闭".into()),
                };
                let Ok(msg) = bincode::deserialize::<PresenceServerMsg>(&raw) else {
                    continue;
                };
                match msg {
                    PresenceServerMsg::DeviceList(devices) => {
                        let mut st = status.lock().unwrap();
                        st.devices = devices;
                        drop(st);
                        repaint();
                    }
                    PresenceServerMsg::HeartbeatAck(seq) => {
                        if let Some(t0) = hb_sent.remove(&seq) {
                            let rtt = t0.elapsed().as_millis() as u32;
                            status.lock().unwrap().rtt_ms = Some(rtt);
                            repaint();
                        }
                    }
                    PresenceServerMsg::IncomingChannel {
                        session_id,
                        channel,
                        from_device_id,
                        from_device_name,
                    } => {
                        if !effective_accept {
                            tracing::info!(
                                "ignored incoming session from {} ({}): SYSTEM agent owns incoming",
                                from_device_name, from_device_id
                            );
                            continue;
                        }
                        if cfg.revoked_device_ids.contains(&from_device_id) {
                            tracing::warn!("blocked revoked device {}", from_device_id);
                            continue;
                        }
                        let addr = node.addr.clone();
                        let ident = bundle.identity.clone();
                        let root_pub = bundle.root_pubkey;
                        let revoked = cfg.revoked_device_ids.clone();
                        let pending = Arc::clone(pending);
                        let caps = caps.clone();
                        let auto_vdd = cfg.auto_vdd_on_headless;
                        let bundle_full = bundle.clone();

                        tokio::spawn(async move {
                            let tcp = match connect_and_auth_relay(
                                &addr,
                                &ident,
                                ConnectionPurpose::AcceptSession { session_id, channel },
                            )
                            .await
                            {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::warn!(
                                        "AcceptSession ({channel:?}, session {session_id}) failed: {e}"
                                    );
                                    return;
                                }
                            };
                            let ns = match tokio::time::timeout(
                                Duration::from_secs(10),
                                NoiseStream::handshake_responder(tcp, &root_pub, &ident, &revoked),
                            )
                            .await
                            {
                                Ok(Ok(ns)) => ns,
                                Ok(Err(e)) => {
                                    tracing::warn!(
                                        "Noise handshake_responder ({channel:?}, session {session_id}) failed: {e}"
                                    );
                                    return;
                                }
                                Err(_) => {
                                    tracing::warn!(
                                        "Noise handshake_responder ({channel:?}, session {session_id}) timed out"
                                    );
                                    return;
                                }
                            };
                            if let Some((ctrl, video)) = pending.insert_channel(session_id, channel, ns) {
                                let opts = HostSessionOptions {
                                    caps,
                                    bundle: bundle_full,
                                    revoked_ids: revoked,
                                    agent_mode,
                                    auto_vdd_on_headless: auto_vdd,
                                    pending_channels: Arc::clone(&pending),
                                    frame_source: None,
                                };
                                if let Err(e) = run_host_session(ctrl, video, opts).await {
                                    tracing::warn!("host session ended with error: {e:#}");
                                }
                            }
                        });
                    }
                    PresenceServerMsg::Error(e) => {
                        status.lock().unwrap().error = Some(e);
                        repaint();
                    }
                }
            }
        }
    }
}

fn update_status_offline(
    status: &Arc<Mutex<RelayStatus>>,
    node: Option<&RelayNodeConfig>,
    error: Option<String>,
) {
    let mut st = status.lock().unwrap();
    st.connected = false;
    st.rtt_ms = None;
    st.devices.clear();
    if let Some(n) = node {
        st.relay_id = n.id.clone();
        st.relay_name = n.name.clone();
        st.relay_addr = n.addr.clone();
    }
    st.error = error;
}
