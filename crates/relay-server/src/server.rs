use crate::state::{PairRole, RelayState};
use protocol::framing::{read_frame, write_frame};
use protocol::relay::{
    ChannelKind, ConnectionPurpose, OnlineDevice, PresenceClientMsg, PresenceServerMsg,
    RelayAuthRequest, RelayAuthResponse, RelayHello,
};
use protocol::unix_timestamp_secs;
use rand::rngs::OsRng;
use rand::RngCore;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::copy_bidirectional_with_sizes;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const PRESENCE_IDLE_TIMEOUT: Duration = Duration::from_secs(25);
/// Bulk file tunnels: large buffers for throughput on high-BDP (overseas) links.
const FILE_COPY_BUF_SIZE: usize = 512 * 1024;
/// Interactive tunnels (control / video): keep what the relay itself buffers small. On a 5 Mbps
/// egress the old 512 KB buffer alone held ~0.8 s of video; the host's flow control can only
/// bound latency if the path doesn't hide a deep queue.
const INTERACTIVE_COPY_BUF_SIZE: usize = 32 * 1024;
/// Linux `TCP_NOTSENT_LOWAT` for interactive sockets: a socket only reports writable while less
/// than this much *unsent* data is queued, so backlog stays in the sender instead of the kernel.
#[cfg(target_os = "linux")]
const INTERACTIVE_NOTSENT_LOWAT: i32 = 16 * 1024;

fn copy_buf_size(channel: ChannelKind) -> usize {
    if channel == ChannelKind::File {
        FILE_COPY_BUF_SIZE
    } else {
        INTERACTIVE_COPY_BUF_SIZE
    }
}

/// Low-latency socket options for control / video tunnels.
fn tune_interactive_socket(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // Value from include/uapi/linux/tcp.h (stable since Linux 3.12).
        const TCP_NOTSENT_LOWAT: libc::c_int = 25;
        let val: libc::c_int = INTERACTIVE_NOTSENT_LOWAT;
        // SAFETY: valid socket fd for the lifetime of `stream`; option value is a c_int.
        let rc = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::IPPROTO_TCP,
                TCP_NOTSENT_LOWAT,
                std::ptr::addr_of!(val).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            debug!(
                "TCP_NOTSENT_LOWAT not applied: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

pub async fn run_server(listener: TcpListener, state: Arc<RelayState>) -> anyhow::Result<()> {
    info!("FlashDesk relay listening on {}", listener.local_addr()?);
    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer_addr, state).await {
                debug!("connection {} ended: {}", peer_addr, e);
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    state: Arc<RelayState>,
) -> anyhow::Result<()> {
    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);

    let auth_req = tokio::time::timeout(AUTH_TIMEOUT, async {
        let hello = RelayHello {
            server_version: 1,
            server_nonce: nonce,
        };
        let hello_bytes = bincode::serialize(&hello)?;
        write_frame(&mut stream, &hello_bytes).await?;
        let raw_req = read_frame(&mut stream).await?;
        let req: RelayAuthRequest = bincode::deserialize(&raw_req)?;
        Ok::<RelayAuthRequest, anyhow::Error>(req)
    })
    .await
    .map_err(|_| anyhow::anyhow!("auth handshake timeout from {peer_addr}"))??;

    let revoked = state.revoked_ids.read().await.clone();
    if let Err(e) = auth_req.verify(&state.root_pubkey, &nonce, &revoked, unix_timestamp_secs()) {
        warn!("rejected unauthenticated client {}: {}", peer_addr, e);
        let resp = bincode::serialize(&RelayAuthResponse::Rejected(e.to_string()))?;
        let _ = tokio::time::timeout(Duration::from_secs(2), write_frame(&mut stream, &resp)).await;
        return Err(anyhow::anyhow!("auth failed: {e}"));
    }

    match &auth_req.purpose {
        ConnectionPurpose::Presence => {
            let _ = stream.set_nodelay(true);
            handle_presence(stream, auth_req.cert, state).await
        }
        ConnectionPurpose::OpenSession {
            target_device_id,
            session_id,
            channel,
        } => {
            if *channel != ChannelKind::File {
                tune_interactive_socket(&stream);
            }
            handle_open_session(
                stream,
                auth_req.cert,
                *target_device_id,
                *session_id,
                *channel,
                state,
            )
            .await
        }
        ConnectionPurpose::AcceptSession {
            session_id,
            channel,
        } => {
            if *channel != ChannelKind::File {
                tune_interactive_socket(&stream);
            }
            handle_accept_session(stream, auth_req.cert, *session_id, *channel, state).await
        }
        ConnectionPurpose::PairTunnel {
            session_id,
            channel,
            peer_device_id,
        } => {
            if *channel != ChannelKind::File {
                tune_interactive_socket(&stream);
            }
            handle_pair_tunnel(
                stream,
                auth_req.cert,
                *session_id,
                *channel,
                *peer_device_id,
                state,
            )
            .await
        }
    }
}

async fn send_auth_ok(stream: &mut TcpStream) -> anyhow::Result<()> {
    let resp = bincode::serialize(&RelayAuthResponse::Ok)?;
    write_frame(stream, &resp).await?;
    Ok(())
}

async fn send_auth_rejected(stream: &mut TcpStream, msg: impl Into<String>) {
    if let Ok(resp) = bincode::serialize(&RelayAuthResponse::Rejected(msg.into())) {
        let _ = tokio::time::timeout(Duration::from_secs(2), write_frame(stream, &resp)).await;
    }
}

async fn handle_presence(
    mut stream: TcpStream,
    cert: protocol::DeviceCert,
    state: Arc<RelayState>,
) -> anyhow::Result<()> {
    send_auth_ok(&mut stream).await?;

    let device_id = cert.device_id;
    let device_name = cert.device_name.clone();
    info!("device online: {} ({})", device_name, device_id);

    let (tx, mut rx) = mpsc::channel::<PresenceServerMsg>(128);
    let initial_info = OnlineDevice {
        device_id,
        device_name: device_name.clone(),
        monitor_count: 1,
        encode_codecs: Vec::new(),
        decode_codecs: Vec::new(),
        os: "unknown".into(),
    };
    let conn_seq = state.register_peer(initial_info, tx.clone()).await;

    let (mut reader, mut writer) = stream.into_split();

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let bytes = match bincode::serialize(&msg) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if write_frame(&mut writer, &bytes).await.is_err() {
                break;
            }
        }
    });

    let res = loop {
        match tokio::time::timeout(PRESENCE_IDLE_TIMEOUT, read_frame(&mut reader)).await {
            Ok(Ok(raw)) => {
                let msg: PresenceClientMsg = match bincode::deserialize(&raw) {
                    Ok(m) => m,
                    Err(e) => {
                        break Err(anyhow::anyhow!("invalid presence msg: {e}"));
                    }
                };
                match msg {
                    PresenceClientMsg::UpdateInfo {
                        monitor_count,
                        encode_codecs,
                        decode_codecs,
                        os,
                        accept_incoming,
                    } => {
                        state
                            .update_peer_info(
                                &device_id,
                                conn_seq,
                                monitor_count,
                                encode_codecs,
                                decode_codecs,
                                os,
                                accept_incoming,
                            )
                            .await;
                    }
                    PresenceClientMsg::ListDevices => {
                        let list = state.snapshot_devices().await;
                        let _ = tx.try_send(PresenceServerMsg::DeviceList(list));
                    }
                    PresenceClientMsg::Heartbeat(seq) => {
                        let _ = tx.try_send(PresenceServerMsg::HeartbeatAck(seq));
                    }
                }
            }
            Ok(Err(e)) => break Err(anyhow::anyhow!("presence read closed: {e}")),
            Err(_) => break Err(anyhow::anyhow!("presence idle timeout")),
        }
    };

    writer_task.abort();
    state.unregister_peer(&device_id, conn_seq).await;
    info!("device offline: {} ({})", device_name, device_id);
    res
}

async fn handle_open_session(
    mut stream: TcpStream,
    cert: protocol::DeviceCert,
    target_device_id: uuid::Uuid,
    session_id: uuid::Uuid,
    channel: ChannelKind,
    state: Arc<RelayState>,
) -> anyhow::Result<()> {
    let Some(target_tx) = state.get_peer_sender(&target_device_id).await else {
        send_auth_rejected(&mut stream, "target device offline").await;
        return Err(anyhow::anyhow!("target {} offline", target_device_id));
    };

    let Some(rx) = state
        .insert_pending_tunnel(session_id, channel, cert.device_id, target_device_id)
        .await
    else {
        send_auth_rejected(&mut stream, "duplicate session/channel id").await;
        return Err(anyhow::anyhow!(
            "duplicate pending tunnel {:?}/{:?}",
            session_id,
            channel
        ));
    };

    send_auth_ok(&mut stream).await?;

    let invite = PresenceServerMsg::IncomingChannel {
        session_id,
        channel,
        from_device_id: cert.device_id,
        from_device_name: cert.device_name.clone(),
    };
    if target_tx.send(invite).await.is_err() {
        let _ = state.take_pending_tunnel(session_id, channel).await;
        return Err(anyhow::anyhow!("target presence channel closed"));
    }

    let mut target_stream = match tokio::time::timeout(state.pending_timeout, rx).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            let _ = state.take_pending_tunnel(session_id, channel).await;
            return Err(anyhow::anyhow!("pending tunnel cancelled"));
        }
        Err(_) => {
            let _ = state.take_pending_tunnel(session_id, channel).await;
            return Err(anyhow::anyhow!(
                "timed out waiting for target {} to accept {:?}",
                target_device_id,
                channel
            ));
        }
    };

    info!(
        "tunnel paired {:?}: {} -> {} (session {})",
        channel, cert.device_id, target_device_id, session_id
    );

    let buf = copy_buf_size(channel);
    let _ = copy_bidirectional_with_sizes(&mut stream, &mut target_stream, buf, buf).await;

    Ok(())
}

async fn handle_accept_session(
    mut stream: TcpStream,
    cert: protocol::DeviceCert,
    session_id: uuid::Uuid,
    channel: ChannelKind,
    state: Arc<RelayState>,
) -> anyhow::Result<()> {
    let Some(pending) = state.take_pending_tunnel(session_id, channel).await else {
        send_auth_rejected(&mut stream, "pending session expired or missing").await;
        return Err(anyhow::anyhow!(
            "missing pending tunnel {:?}/{:?}",
            session_id,
            channel
        ));
    };

    if pending.target_id != cert.device_id {
        send_auth_rejected(&mut stream, "device mismatch for target session").await;
        return Err(anyhow::anyhow!(
            "device {} tried to accept tunnel meant for {}",
            cert.device_id,
            pending.target_id
        ));
    }

    send_auth_ok(&mut stream).await?;

    if pending.tx.send(stream).is_err() {
        return Err(anyhow::anyhow!("initiator disconnected before pairing"));
    }

    Ok(())
}

async fn handle_pair_tunnel(
    mut stream: TcpStream,
    cert: protocol::DeviceCert,
    session_id: uuid::Uuid,
    channel: ChannelKind,
    peer_device_id: uuid::Uuid,
    state: Arc<RelayState>,
) -> anyhow::Result<()> {
    match state
        .pair_or_insert_tunnel(session_id, channel, cert.device_id, peer_device_id)
        .await
    {
        PairRole::Joiner(pending) => {
            if pending.target_id != cert.device_id || pending.initiator_id != peer_device_id {
                send_auth_rejected(&mut stream, "pair tunnel device mismatch").await;
                return Err(anyhow::anyhow!(
                    "pair mismatch: got {}->{} expected {}->{}",
                    cert.device_id,
                    peer_device_id,
                    pending.target_id,
                    pending.initiator_id
                ));
            }
            send_auth_ok(&mut stream).await?;
            if pending.tx.send(stream).is_err() {
                return Err(anyhow::anyhow!("pair waiter disconnected before handoff"));
            }
            Ok(())
        }
        PairRole::Waiter(rx) => {
            send_auth_ok(&mut stream).await?;
            let mut partner_stream = match tokio::time::timeout(state.pending_timeout, rx).await {
                Ok(Ok(s)) => s,
                Ok(Err(_)) => {
                    let _ = state.take_pending_tunnel(session_id, channel).await;
                    return Err(anyhow::anyhow!("pair tunnel cancelled"));
                }
                Err(_) => {
                    let _ = state.take_pending_tunnel(session_id, channel).await;
                    return Err(anyhow::anyhow!(
                        "pair tunnel {:?}/{} timed out waiting for {}",
                        channel,
                        session_id,
                        peer_device_id
                    ));
                }
            };
            info!(
                "direct pair tunnel {:?}: {} <-> {} (session {})",
                channel, cert.device_id, peer_device_id, session_id
            );
            let buf = copy_buf_size(channel);
            let _ = copy_bidirectional_with_sizes(&mut stream, &mut partner_stream, buf, buf).await;
            Ok(())
        }
    }
}
