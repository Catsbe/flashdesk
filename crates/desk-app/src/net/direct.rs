use crate::codec::CodecCapabilities;
use crate::config::AppConfig;
use crate::net::host_session::{run_host_session, HostSessionOptions, PendingHostChannels};
use anyhow::Result;
use protocol::framing::{read_frame, write_frame};
use protocol::noise::NoiseStream;
use protocol::relay::ChannelKind;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize)]
struct DirectChannelHeader {
    session_id: Uuid,
    channel: ChannelKind,
}

pub async fn connect_direct_channel(
    addr: &str,
    session_id: Uuid,
    channel: ChannelKind,
) -> Result<TcpStream> {
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .map_err(|_| anyhow::anyhow!("直连超时: {addr}"))??;
    if channel != ChannelKind::File {
        let _ = stream.set_nodelay(true);
    }
    let hdr = DirectChannelHeader {
        session_id,
        channel,
    };
    write_frame(&mut stream, &bincode::serialize(&hdr)?).await?;
    Ok(stream)
}

/// Serves one accepted direct (LAN / IP) connection: channel header, Noise responder handshake,
/// then file-channel hand-off or Control+Video pairing into a host session.
pub async fn serve_direct_connection(mut stream: TcpStream, opts: HostSessionOptions) {
    let _ = stream.set_nodelay(true);
    let Ok(Ok(raw)) = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream)).await
    else {
        return;
    };
    let Ok(hdr) = bincode::deserialize::<DirectChannelHeader>(&raw) else {
        return;
    };
    let Ok(ns) = NoiseStream::handshake_responder(
        stream,
        &opts.bundle.root_pubkey,
        &opts.bundle.identity,
        &opts.revoked_ids,
    )
    .await
    else {
        return;
    };

    if hdr.channel == ChannelKind::File {
        opts.pending_channels.fulfill_direct_file(hdr.session_id, ns);
        return;
    }

    if let Some((ctrl, video)) = opts
        .pending_channels
        .insert_channel(hdr.session_id, hdr.channel, ns)
    {
        if let Err(e) = run_host_session(ctrl, video, opts).await {
            tracing::warn!("direct host session ended with error: {e:#}");
        }
    }
}

pub fn spawn_direct_listener(
    cfg: Arc<Mutex<AppConfig>>,
    caps: CodecCapabilities,
    agent_mode: bool,
    active_sessions: Arc<std::sync::atomic::AtomicU32>,
) {
    tokio::spawn(async move {
        let pending = Arc::new(PendingHostChannels::with_session_counter(active_sessions));
        let mut bind_warned = false;

        // Bind supervisor: the port must always be owned by exactly one live host.
        // - agent: retry binding forever (the GUI may hold the port from an earlier takeover).
        // - GUI: stay out of the way while a healthy SYSTEM agent exists; otherwise bind/retry so
        //   LAN direct keeps working when the agent is dead (degraded non-SYSTEM hosting).
        loop {
            let port = cfg.lock().unwrap().direct_listen_port;
            if !agent_mode && crate::service::health::agent_healthy() {
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            let bind_addr = format!("0.0.0.0:{port}");
            let listener = match TcpListener::bind(&bind_addr).await {
                Ok(l) => {
                    bind_warned = false;
                    tracing::info!(
                        "direct LAN listener active on {} (agent={})",
                        bind_addr,
                        agent_mode
                    );
                    l
                }
                Err(e) => {
                    if !bind_warned {
                        tracing::warn!(
                            "failed to bind direct LAN listener on {}: {e}; retrying every 3s",
                            bind_addr
                        );
                        bind_warned = true;
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            let mut health_tick = tokio::time::interval(Duration::from_secs(3));
            health_tick.tick().await; // consume the immediate first tick

            // `TcpListener::accept` is cancel-safe; no frame reads happen inside this select!.
            loop {
                tokio::select! {
                    acc = listener.accept() => {
                        let Ok((stream, _peer)) = acc else { break };
                        let cfg_snap = cfg.lock().unwrap().clone();
                        if !cfg_snap.allow_incoming {
                            continue;
                        }
                        let Some(bundle) = cfg_snap.bundle else {
                            continue;
                        };
                        let opts = HostSessionOptions {
                            caps: caps.clone(),
                            bundle,
                            revoked_ids: cfg_snap.revoked_device_ids,
                            agent_mode,
                            auto_vdd_on_headless: cfg_snap.auto_vdd_on_headless,
                            pending_channels: Arc::clone(&pending),
                            frame_source: None,
                        };
                        tokio::spawn(serve_direct_connection(stream, opts));
                    }
                    _ = health_tick.tick() => {
                        let cur_port = cfg.lock().unwrap().direct_listen_port;
                        if cur_port != port {
                            tracing::info!("direct listen port changed to {cur_port}; rebinding");
                            break;
                        }
                        if !agent_mode && crate::service::health::agent_healthy() {
                            tracing::info!(
                                "direct LAN listener yielding port {port} to healthy SYSTEM agent"
                            );
                            break;
                        }
                    }
                }
            }
            drop(listener);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
}
