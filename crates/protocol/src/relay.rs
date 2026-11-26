use crate::auth::{verify_challenge, DeviceCert};
use crate::session::VideoCodec;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChannelKind {
    Control,
    Video,
    File,
    AudioReserved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionPurpose {
    Presence,
    OpenSession {
        target_device_id: Uuid,
        session_id: Uuid,
        channel: ChannelKind,
    },
    AcceptSession {
        session_id: Uuid,
        channel: ChannelKind,
    },
    /// Symmetric rendezvous on any relay node without requiring Presence on that node.
    PairTunnel {
        session_id: Uuid,
        channel: ChannelKind,
        peer_device_id: Uuid,
    },
    /// Authenticated update manifest query, artifact download, or root-signed release push.
    UpdateChannel,
}

impl ConnectionPurpose {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        bincode::serialize(self).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayHello {
    pub server_version: u16,
    pub server_nonce: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayAuthRequest {
    pub cert: DeviceCert,
    pub purpose: ConnectionPurpose,
    pub challenge_sig: Vec<u8>,
}

impl RelayAuthRequest {
    pub fn verify(
        &self,
        root_pubkey: &[u8; 32],
        nonce: &[u8; 32],
        revoked_ids: &[Uuid],
        now_unix: u64,
    ) -> crate::Result<()> {
        if revoked_ids.contains(&self.cert.device_id) {
            return Err(crate::ProtocolError::Auth(format!(
                "device {} is revoked",
                self.cert.device_id
            )));
        }
        self.cert.verify(root_pubkey, now_unix)?;
        verify_challenge(
            &self.cert.device_pubkey,
            nonce,
            &self.purpose.canonical_bytes(),
            &self.challenge_sig,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelayAuthResponse {
    Ok,
    Rejected(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OnlineDevice {
    pub device_id: Uuid,
    pub device_name: String,
    pub monitor_count: u32,
    pub encode_codecs: Vec<VideoCodec>,
    pub decode_codecs: Vec<VideoCodec>,
    pub os: String,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PresenceClientMsg {
    UpdateInfo {
        monitor_count: u32,
        encode_codecs: Vec<VideoCodec>,
        decode_codecs: Vec<VideoCodec>,
        os: String,
        #[serde(default = "default_true")]
        accept_incoming: bool,
    },
    ListDevices,
    Heartbeat(u64),
    SyncRevocations(crate::auth::RevocationCommand),
    SetDisplayName(String),
    RequestPeerUpdateCheck {
        target_device_id: Uuid,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PresenceServerMsg {
    DeviceList(Vec<OnlineDevice>),
    IncomingChannel {
        session_id: Uuid,
        channel: ChannelKind,
        from_device_id: Uuid,
        from_device_name: String,
    },
    HeartbeatAck(u64),
    Error(String),
    RevocationsSynced {
        count: usize,
        issued_at: u64,
    },
    CheckUpdateNow {
        from_device_id: Uuid,
        from_device_name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enum_variant_index<T: Serialize>(v: &T) -> u32 {
        let bytes = bincode::serialize(v).unwrap();
        u32::from_le_bytes(bytes[..4].try_into().unwrap())
    }

    /// Mixed-version relay clients and servers depend on bincode variant indices never shifting.
    #[test]
    fn relay_enum_variant_indices_are_stable() {
        assert_eq!(enum_variant_index(&ConnectionPurpose::Presence), 0);
        assert_eq!(
            enum_variant_index(&ConnectionPurpose::OpenSession {
                target_device_id: Uuid::nil(),
                session_id: Uuid::nil(),
                channel: ChannelKind::Control,
            }),
            1
        );
        assert_eq!(
            enum_variant_index(&ConnectionPurpose::AcceptSession {
                session_id: Uuid::nil(),
                channel: ChannelKind::Video,
            }),
            2
        );
        assert_eq!(
            enum_variant_index(&ConnectionPurpose::PairTunnel {
                session_id: Uuid::nil(),
                channel: ChannelKind::File,
                peer_device_id: Uuid::nil(),
            }),
            3
        );
        assert_eq!(enum_variant_index(&ConnectionPurpose::UpdateChannel), 4);

        assert_eq!(enum_variant_index(&PresenceClientMsg::ListDevices), 1);
        assert_eq!(enum_variant_index(&PresenceClientMsg::Heartbeat(0)), 2);
        assert_eq!(
            enum_variant_index(&PresenceClientMsg::SetDisplayName(String::new())),
            4
        );
        assert_eq!(
            enum_variant_index(&PresenceClientMsg::RequestPeerUpdateCheck {
                target_device_id: Uuid::nil(),
            }),
            5
        );

        assert_eq!(enum_variant_index(&PresenceServerMsg::DeviceList(vec![])), 0);
        assert_eq!(enum_variant_index(&PresenceServerMsg::HeartbeatAck(0)), 2);
        assert_eq!(
            enum_variant_index(&PresenceServerMsg::RevocationsSynced {
                count: 0,
                issued_at: 0,
            }),
            4
        );
        assert_eq!(
            enum_variant_index(&PresenceServerMsg::CheckUpdateNow {
                from_device_id: Uuid::nil(),
                from_device_name: String::new(),
            }),
            5
        );
    }
}
