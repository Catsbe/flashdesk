use protocol::relay::{OnlineDevice, PresenceServerMsg};
use protocol::ChannelKind;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use uuid::Uuid;

pub struct PeerEntry {
    pub conn_seq: u64,
    pub accept_incoming: bool,
    pub info: OnlineDevice,
    pub tx: mpsc::Sender<PresenceServerMsg>,
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

pub struct RelayState {
    pub root_pubkey: [u8; 32],
    pub revoked_ids: RwLock<Vec<Uuid>>,
    pub pending_timeout: Duration,
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
        Self {
            root_pubkey,
            revoked_ids: RwLock::new(revoked_ids),
            pending_timeout,
            peers: Mutex::new(HashMap::new()),
            pending_tunnels: Mutex::new(HashMap::new()),
            next_conn_seq: AtomicU64::new(1),
        }
    }

    pub async fn is_revoked(&self, device_id: &Uuid) -> bool {
        self.revoked_ids.read().await.contains(device_id)
    }

    pub async fn register_peer(
        &self,
        info: OnlineDevice,
        tx: mpsc::Sender<PresenceServerMsg>,
    ) -> u64 {
        let seq = self.next_conn_seq.fetch_add(1, Ordering::Relaxed);
        let id = info.device_id;
        {
            let mut peers = self.peers.lock().await;
            peers.entry(id).or_default().push(PeerEntry {
                conn_seq: seq,
                accept_incoming: true,
                info,
                tx,
            });
        }
        self.broadcast_device_list().await;
        seq
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
