use crate::updates::UpdateStore;
use protocol::auth::RevocationCommand;
use protocol::relay::{OnlineDevice, PresenceServerMsg};
use protocol::ChannelKind;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex, Notify, RwLock};
use tracing::{info, warn};
use uuid::Uuid;

pub struct PeerEntry {
    pub conn_seq: u64,
    pub accept_incoming: bool,
    pub info: OnlineDevice,
    pub tx: mpsc::Sender<PresenceServerMsg>,
    pub kick_notify: Arc<Notify>,
}

pub struct PendingTunnel {
    pub initiator_id: Uuid,
    pub target_id: Uuid,
    pub tx: oneshot::Sender<TcpStream>,
}

pub enum PairRole {
    Waiter(oneshot::Receiver<TcpStream>),
    Joiner(PendingTunnel),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedRevocations {
    issued_at: u64,
    revoked_ids: Vec<Uuid>,
}

struct DynamicRevocations {
    issued_at: u64,
    revoked_ids: Vec<Uuid>,
    state_path: Option<PathBuf>,
}

pub struct RelayState {
    pub root_pubkey: [u8; 32],
    static_revoked_ids: Vec<Uuid>,
    dynamic_revocations: RwLock<DynamicRevocations>,
    pub pending_timeout: Duration,
    pub updates: Arc<UpdateStore>,
    peers: Mutex<HashMap<Uuid, Vec<PeerEntry>>>,
    pending_tunnels: Mutex<HashMap<(Uuid, ChannelKind), PendingTunnel>>,
    next_conn_seq: AtomicU64,
}

impl RelayState {
    pub fn new(
        root_pubkey: [u8; 32],
        revoked_ids: Vec<Uuid>,
        pending_timeout: Duration,
    ) -> Self {
        Self::new_with_state_dir(root_pubkey, revoked_ids, pending_timeout, None)
    }

    pub fn new_with_state_dir(
        root_pubkey: [u8; 32],
        static_revoked_ids: Vec<Uuid>,
        pending_timeout: Duration,
        state_dir: Option<PathBuf>,
    ) -> Self {
        let updates = Arc::new(UpdateStore::new(state_dir.as_deref(), &root_pubkey));
        let state_path = state_dir.map(|d| d.join("revoked.json"));
        let mut loaded = PersistedRevocations::default();
        if let Some(path) = &state_path {
            if let Ok(text) = std::fs::read_to_string(path) {
                match serde_json::from_str::<PersistedRevocations>(&text) {
                    Ok(p) => {
                        info!(
                            "loaded {} persisted revocation(s) from {}",
                            p.revoked_ids.len(),
                            path.display()
                        );
                        loaded = p;
                    }
                    Err(e) => {
                        warn!("failed to parse {}: {e}", path.display());
                    }
                }
            }
        }
        Self {
            root_pubkey,
            static_revoked_ids,
            dynamic_revocations: RwLock::new(DynamicRevocations {
                issued_at: loaded.issued_at,
                revoked_ids: loaded.revoked_ids,
                state_path,
            }),
            pending_timeout,
            updates,
            peers: Mutex::new(HashMap::new()),
            pending_tunnels: Mutex::new(HashMap::new()),
            next_conn_seq: AtomicU64::new(1),
        }
    }

    pub async fn effective_revoked_ids(&self) -> Vec<Uuid> {
        let dyn_rev = self.dynamic_revocations.read().await;
        let mut set: HashSet<Uuid> = self.static_revoked_ids.iter().copied().collect();
        set.extend(dyn_rev.revoked_ids.iter().copied());
        set.into_iter().collect()
    }

    pub async fn is_revoked(&self, device_id: &Uuid) -> bool {
        if self.static_revoked_ids.contains(device_id) {
            return true;
        }
        self.dynamic_revocations
            .read()
            .await
            .revoked_ids
            .contains(device_id)
    }

    pub async fn apply_revocation_command(
        &self,
        cmd: &RevocationCommand,
        now_unix: u64,
    ) -> anyhow::Result<(usize, u64)> {
        cmd.verify(&self.root_pubkey, now_unix)?;
        let (count, issued_at, effective_set) = {
            let mut dyn_rev = self.dynamic_revocations.write().await;
            if cmd.issued_at < dyn_rev.issued_at {
                anyhow::bail!(
                    "stale revocation command ({} < {})",
                    cmd.issued_at,
                    dyn_rev.issued_at
                );
            }
            dyn_rev.issued_at = cmd.issued_at;
            dyn_rev.revoked_ids = cmd.revoked_device_ids.clone();
            if let Some(path) = &dyn_rev.state_path {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)?;
                    }
                }
                let payload = PersistedRevocations {
                    issued_at: dyn_rev.issued_at,
                    revoked_ids: dyn_rev.revoked_ids.clone(),
                };
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, serde_json::to_vec_pretty(&payload)?)?;
                std::fs::rename(&tmp, path)?;
            }
            let mut set: HashSet<Uuid> = self.static_revoked_ids.iter().copied().collect();
            set.extend(dyn_rev.revoked_ids.iter().copied());
            (dyn_rev.revoked_ids.len(), dyn_rev.issued_at, set)
        };

        // Kick any currently online connections belonging to newly revoked devices.
        let kicks: Vec<Arc<Notify>> = {
            let peers = self.peers.lock().await;
            peers
                .iter()
                .filter(|(id, _)| effective_set.contains(id))
                .flat_map(|(_, vec)| vec.iter().map(|p| Arc::clone(&p.kick_notify)))
                .collect()
        };
        for notify in kicks {
            notify.notify_waiters();
        }

        Ok((count, issued_at))
    }

    pub async fn register_peer(
        &self,
        info: OnlineDevice,
        tx: mpsc::Sender<PresenceServerMsg>,
    ) -> (u64, Arc<Notify>) {
        let seq = self.next_conn_seq.fetch_add(1, Ordering::Relaxed);
        let kick_notify = Arc::new(Notify::new());
        let id = info.device_id;
        {
            let mut peers = self.peers.lock().await;
            peers.entry(id).or_default().push(PeerEntry {
                conn_seq: seq,
                accept_incoming: true,
                info,
                tx,
                kick_notify: Arc::clone(&kick_notify),
            });
        }
        self.broadcast_device_list().await;
        (seq, kick_notify)
    }

    pub async fn unregister_peer(&self, device_id: &Uuid, conn_seq: u64) {
        let removed = {
            let mut peers = self.peers.lock().await;
            if let Some(vec) = peers.get_mut(device_id) {
                let before = vec.len();
                vec.retain(|e| e.conn_seq != conn_seq);
                let changed = vec.len() != before;
                if vec.is_empty() {
                    peers.remove(device_id);
                }
                changed
            } else {
                false
            }
        };
        if removed {
            self.broadcast_device_list().await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_peer_info(
        &self,
        device_id: &Uuid,
        conn_seq: u64,
        monitor_count: u32,
        encode_codecs: Vec<protocol::VideoCodec>,
        decode_codecs: Vec<protocol::VideoCodec>,
        os: String,
        accept_incoming: bool,
    ) {
        let updated = {
            let mut peers = self.peers.lock().await;
            if let Some(vec) = peers.get_mut(device_id) {
                if let Some(entry) = vec.iter_mut().find(|e| e.conn_seq == conn_seq) {
                    entry.accept_incoming = accept_incoming;
                    entry.info.monitor_count = monitor_count;
                    entry.info.encode_codecs = encode_codecs;
                    entry.info.decode_codecs = decode_codecs;
                    entry.info.os = os;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if updated {
            self.broadcast_device_list().await;
        }
    }

    pub async fn update_peer_display_name(&self, device_id: &Uuid, conn_seq: u64, name: String) {
        let clean: String = name.trim().chars().take(64).collect();
        if clean.is_empty() {
            return;
        }
        let updated = {
            let mut peers = self.peers.lock().await;
            if let Some(vec) = peers.get_mut(device_id) {
                if let Some(entry) = vec.iter_mut().find(|e| e.conn_seq == conn_seq) {
                    if entry.info.device_name != clean {
                        entry.info.device_name = clean;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        };
        if updated {
            self.broadcast_device_list().await;
        }
    }

    fn deduplicated_devices(peers: &HashMap<Uuid, Vec<PeerEntry>>) -> Vec<OnlineDevice> {
        let mut list = Vec::with_capacity(peers.len());
        for vec in peers.values() {
            if let Some(chosen) = vec
                .iter()
                .rev()
                .find(|e| e.accept_incoming)
                .or_else(|| vec.last())
            {
                list.push(chosen.info.clone());
            }
        }
        list.sort_by(|a, b| a.device_name.cmp(&b.device_name));
        list
    }

    pub async fn snapshot_devices(&self) -> Vec<OnlineDevice> {
        let peers = self.peers.lock().await;
        Self::deduplicated_devices(&peers)
    }

    pub async fn broadcast_device_list(&self) {
        let (list, senders): (Vec<OnlineDevice>, Vec<mpsc::Sender<PresenceServerMsg>>) = {
            let peers = self.peers.lock().await;
            let list = Self::deduplicated_devices(&peers);
            let senders = peers
                .values()
                .flat_map(|vec| vec.iter().map(|p| p.tx.clone()))
                .collect();
            (list, senders)
        };
        let msg = PresenceServerMsg::DeviceList(list);
        for tx in senders {
            let _ = tx.try_send(msg.clone());
        }
    }

    pub async fn get_peer_sender(
        &self,
        device_id: &Uuid,
    ) -> Option<mpsc::Sender<PresenceServerMsg>> {
        let peers = self.peers.lock().await;
        peers.get(device_id).and_then(|vec| {
            vec.iter()
                .rev()
                .find(|e| e.accept_incoming)
                .or_else(|| vec.last())
                .map(|p| p.tx.clone())
        })
    }

    pub async fn insert_pending_tunnel(
        &self,
        session_id: Uuid,
        channel: ChannelKind,
        initiator_id: Uuid,
        target_id: Uuid,
    ) -> Option<oneshot::Receiver<TcpStream>> {
        let key = (session_id, channel);
        let mut pending = self.pending_tunnels.lock().await;
        if pending.contains_key(&key) {
            return None;
        }
        let (tx, rx) = oneshot::channel();
        pending.insert(
            key,
            PendingTunnel {
                initiator_id,
                target_id,
                tx,
            },
        );
        Some(rx)
    }

    pub async fn take_pending_tunnel(
        &self,
        session_id: Uuid,
        channel: ChannelKind,
    ) -> Option<PendingTunnel> {
        let mut pending = self.pending_tunnels.lock().await;
        pending.remove(&(session_id, channel))
    }

    pub async fn pair_or_insert_tunnel(
        &self,
        session_id: Uuid,
        channel: ChannelKind,
        self_id: Uuid,
        peer_id: Uuid,
    ) -> PairRole {
        let key = (session_id, channel);
        let mut pending = self.pending_tunnels.lock().await;
        if let Some(existing) = pending.remove(&key) {
            PairRole::Joiner(existing)
        } else {
            let (tx, rx) = oneshot::channel();
            pending.insert(
                key,
                PendingTunnel {
                    initiator_id: self_id,
                    target_id: peer_id,
                    tx,
                },
            );
            PairRole::Waiter(rx)
        }
    }
}
