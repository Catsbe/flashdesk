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

pub fn spawn_direct_listener(
    cfg: Arc<Mutex<AppConfig>>,
    caps: CodecCapabilities,
    agent_mode: bool,
) {
    tokio::spawn(async move {
        let pending = Arc::new(PendingHostChannels::new());
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
                        let Ok((mut stream, _peer)) = acc else { break };
                        let _ = stream.set_nodelay(true);
                        let cfg_snap = cfg.lock().unwrap().clone();
                        let pending = Arc::clone(&pending);
                        let caps = caps.clone();

                        tokio::spawn(async move {
                            if !cfg_snap.allow_incoming {
                                return;
                            }
                            let Some(bundle) = cfg_snap.bundle else {
                                return;
                            };
                            let Ok(Ok(raw)) =
                                tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stream)).await
                            else {
                                return;
                            };
                            let Ok(hdr) = bincode::deserialize::<DirectChannelHeader>(&raw) else {
                                return;
                            };
                            let Ok(ns) = NoiseStream::handshake_responder(
                                stream,
                                &bundle.root_pubkey,
                                &bundle.identity,
                                &cfg_snap.revoked_device_ids,
                            )
                            .await
                            else {
                                return;
                            };

                            if hdr.channel == ChannelKind::File {
                                pending.fulfill_direct_file(hdr.session_id, ns);
                                return;
                            }

                            if let Some((ctrl, video)) =
                                pending.insert_channel(hdr.session_id, hdr.channel, ns)
                            {
                                let opts = HostSessionOptions {
                                    caps,
                                    bundle,
                                    revoked_ids: cfg_snap.revoked_device_ids,
                                    agent_mode,
                                    auto_vdd_on_headless: cfg_snap.auto_vdd_on_headless,
                                    pending_channels: Arc::clone(&pending),
                                };
                                let _ = run_host_session(ctrl, video, opts).await;
                            }
                        });
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
