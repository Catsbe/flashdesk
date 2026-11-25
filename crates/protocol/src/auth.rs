use crate::{unix_timestamp_secs, ProtocolError, Result};
use base64::{
    engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL},
    Engine as _,
};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const CERT_SIGN_DOMAIN: &[u8] = b"FLASHDESK-DEVICE-CERT-V1";
const CHALLENGE_SIGN_DOMAIN: &[u8] = b"FLASHDESK-RELAY-CHALLENGE-V1";
const REVOKE_SIGN_DOMAIN: &[u8] = b"FLASHDESK-REVOKE-LIST-V1";
pub const COMPACT_TOKEN_PREFIX: &str = "fdesk://v2/";
const COMPACT_V2_VERSION: u8 = 2;

#[derive(Debug, Clone)]
pub struct RootKeypair {
    signing_key: SigningKey,
}

impl RootKeypair {
    /// Generates a new personal Ed25519 root keypair.
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
        }
    }

    /// Restores a root keypair from a 32-byte Ed25519 secret seed.
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(bytes),
        }
    }

    pub fn from_b64(encoded: &str) -> Result<Self> {
        let bytes = B64
            .decode(encoded.trim())
            .map_err(|e| ProtocolError::Auth(format!("invalid base64 root key: {e}")))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| ProtocolError::Auth("root secret key must be 32 bytes".into()))?;
        Ok(Self::from_bytes(&arr))
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    pub fn to_b64(&self) -> String {
        B64.encode(self.to_bytes())
    }

    pub fn pubkey_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn pubkey_b64(&self) -> String {
        B64.encode(self.pubkey_bytes())
    }

    /// Signs a new device identity certificate valid from now for `validity_secs`.
    pub fn sign_device_cert(
        &self,
        device_id: Uuid,
        device_name: impl Into<String>,
        validity_secs: u64,
    ) -> DeviceIdentity {
        let device_signing = SigningKey::generate(&mut OsRng);
        let now = unix_timestamp_secs();
        let mut cert = DeviceCert {
            version: 1,
            device_id,
            device_name: device_name.into(),
            device_pubkey: device_signing.verifying_key().to_bytes(),
            issued_at: now,
            expires_at: now.saturating_add(validity_secs),
            root_signature: Vec::new(),
        };
        let payload = cert.canonical_bytes();
        let sig: Signature = self.signing_key.sign(&payload);
        cert.root_signature = sig.to_bytes().to_vec();

        DeviceIdentity {
            cert,
            secret_key: device_signing.to_bytes(),
        }
    }

    /// Signs a full device revocation list so the relay server can apply and persist updates
    /// without manual SSH edits to `relay.toml`.
    pub fn sign_revocation_list(&self, mut revoked_device_ids: Vec<Uuid>) -> RevocationCommand {
        revoked_device_ids.sort_unstable();
        revoked_device_ids.dedup();
        let mut cmd = RevocationCommand {
            version: 1,
            issued_at: unix_timestamp_secs(),
            revoked_device_ids,
            root_signature: Vec::new(),
        };
        let payload = cmd.canonical_bytes();
        let sig: Signature = self.signing_key.sign(&payload);
        cmd.root_signature = sig.to_bytes().to_vec();
        cmd
    }
}

/// Root-CA-signed revocation list pushed to the relay server over an authenticated Presence link.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevocationCommand {
    pub version: u16,
    pub issued_at: u64,
    pub revoked_device_ids: Vec<Uuid>,
    pub root_signature: Vec<u8>,
}

impl RevocationCommand {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(REVOKE_SIGN_DOMAIN.len() + 2 + 8 + 4 + self.revoked_device_ids.len() * 16);
        out.extend_from_slice(REVOKE_SIGN_DOMAIN);
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&self.issued_at.to_be_bytes());
        out.extend_from_slice(&(self.revoked_device_ids.len() as u32).to_be_bytes());
        for id in &self.revoked_device_ids {
            out.extend_from_slice(id.as_bytes());
        }
        out
    }

    pub fn verify(&self, root_pubkey: &[u8; 32], now_unix: u64) -> Result<()> {
        if self.version != 1 {
            return Err(ProtocolError::Auth(format!(
                "unsupported revocation command version: {}",
                self.version
            )));
        }
        if self.issued_at > now_unix.saturating_add(300) {
            return Err(ProtocolError::Auth(
                "revocation command issued_at is in the future".into(),
            ));
        }
        let verifying_key = VerifyingKey::from_bytes(root_pubkey)
            .map_err(|e| ProtocolError::Auth(format!("invalid root pubkey: {e}")))?;
        let signature = Signature::from_slice(&self.root_signature)
            .map_err(|e| ProtocolError::Auth(format!("invalid revocation signature: {e}")))?;
        verifying_key
            .verify(&self.canonical_bytes(), &signature)
            .map_err(|_| ProtocolError::Auth("revocation signature verification failed".into()))
    }
}

pub fn decode_pubkey_b64(encoded: &str) -> Result<[u8; 32]> {
    let bytes = B64
        .decode(encoded.trim())
        .map_err(|e| ProtocolError::Auth(format!("invalid base64 pubkey: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| ProtocolError::Auth("public key must be 32 bytes".into()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceCert {
    pub version: u16,
    pub device_id: Uuid,
    pub device_name: String,
    pub device_pubkey: [u8; 32],
    pub issued_at: u64,
    pub expires_at: u64,
    pub root_signature: Vec<u8>,
}

impl DeviceCert {
    /// Returns deterministic canonical bytes covered by the root signature.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let name_bytes = self.device_name.as_bytes();
        let mut out = Vec::with_capacity(
            CERT_SIGN_DOMAIN.len() + 2 + 16 + 4 + name_bytes.len() + 32 + 8 + 8,
        );
        out.extend_from_slice(CERT_SIGN_DOMAIN);
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(self.device_id.as_bytes());
        out.extend_from_slice(&(name_bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&self.device_pubkey);
        out.extend_from_slice(&self.issued_at.to_be_bytes());
        out.extend_from_slice(&self.expires_at.to_be_bytes());
        out
    }

    /// Verifies the root CA signature and expiration against `now_unix`.
    pub fn verify(&self, root_pubkey: &[u8; 32], now_unix: u64) -> Result<()> {
        if self.version != 1 {
            return Err(ProtocolError::Auth(format!(
                "unsupported cert version: {}",
                self.version
            )));
        }
        if self.device_name.trim().is_empty() {
            return Err(ProtocolError::Auth("device_name cannot be empty".into()));
        }
        if now_unix > self.expires_at {
            return Err(ProtocolError::Auth(format!(
                "device cert for {} expired at {}",
                self.device_id, self.expires_at
            )));
        }
        if self.issued_at > now_unix.saturating_add(300) {
            return Err(ProtocolError::Auth(
                "device cert issued_at is in the future".into(),
            ));
        }

        let verifying_key = VerifyingKey::from_bytes(root_pubkey)
            .map_err(|e| ProtocolError::Auth(format!("invalid root pubkey: {e}")))?;
        let signature = Signature::from_slice(&self.root_signature)
            .map_err(|e| ProtocolError::Auth(format!("invalid cert signature format: {e}")))?;
        let payload = self.canonical_bytes();
        verifying_key
            .verify(&payload, &signature)
            .map_err(|_| ProtocolError::Auth("root signature verification failed".into()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub cert: DeviceCert,
    pub secret_key: [u8; 32],
}

impl DeviceIdentity {
    pub fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.secret_key)
    }

    /// Signs a domain-prefixed arbitrary payload using this device's Ed25519 private key.
    pub fn sign_domain_payload(&self, domain: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut msg = Vec::with_capacity(domain.len() + payload.len());
        msg.extend_from_slice(domain);
        msg.extend_from_slice(payload);
        self.signing_key().sign(&msg).to_bytes().to_vec()
    }

    /// Signs a relay authentication challenge binding nonce + purpose bytes.
    pub fn sign_challenge(&self, nonce: &[u8; 32], purpose_bytes: &[u8]) -> Vec<u8> {
        let mut payload = Vec::with_capacity(32 + purpose_bytes.len());
        payload.extend_from_slice(nonce);
        payload.extend_from_slice(purpose_bytes);
        self.sign_domain_payload(CHALLENGE_SIGN_DOMAIN, &payload)
    }
}

pub fn verify_domain_signature(
    device_pubkey: &[u8; 32],
    domain: &[u8],
    payload: &[u8],
    signature_bytes: &[u8],
) -> Result<()> {
    let vk = VerifyingKey::from_bytes(device_pubkey)
        .map_err(|e| ProtocolError::Auth(format!("invalid device pubkey: {e}")))?;
    let sig = Signature::from_slice(signature_bytes)
        .map_err(|e| ProtocolError::Auth(format!("invalid device signature: {e}")))?;
    let mut msg = Vec::with_capacity(domain.len() + payload.len());
    msg.extend_from_slice(domain);
    msg.extend_from_slice(payload);
    vk.verify(&msg, &sig)
        .map_err(|_| ProtocolError::Auth("device challenge signature mismatch".into()))
}

pub fn verify_challenge(
    device_pubkey: &[u8; 32],
    nonce: &[u8; 32],
    purpose_bytes: &[u8],
    signature_bytes: &[u8],
) -> Result<()> {
    let mut payload = Vec::with_capacity(32 + purpose_bytes.len());
    payload.extend_from_slice(nonce);
    payload.extend_from_slice(purpose_bytes);
    verify_domain_signature(device_pubkey, CHALLENGE_SIGN_DOMAIN, &payload, signature_bytes)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RelayNodeRole {
    PrimaryControl,
    FilePreferred,
    Backup,
}

impl RelayNodeRole {
    pub fn label(&self) -> &'static str {
        match self {
            RelayNodeRole::PrimaryControl => "主控低延迟",
            RelayNodeRole::FilePreferred => "大文件高速",
            RelayNodeRole::Backup => "备用节点",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayNodeConfig {
    pub id: String,
    pub name: String,
    pub addr: String,
    pub role: RelayNodeRole,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceBundle {
    pub root_pubkey: [u8; 32],
    pub identity: DeviceIdentity,
    pub relay_nodes: Vec<RelayNodeConfig>,
}

impl DeviceBundle {
    pub fn to_json_pretty(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| ProtocolError::Serialization(e.to_string()))
    }

    pub fn from_json_str(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| ProtocolError::Serialization(e.to_string()))
    }

    pub fn to_b64_token(&self) -> Result<String> {
        let json = serde_json::to_vec(self)
            .map_err(|e| ProtocolError::Serialization(e.to_string()))?;
        Ok(B64.encode(json))
    }

    pub fn from_b64_token(token: &str) -> Result<Self> {
        let clean: String = token
            .trim()
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let raw = B64
            .decode(&clean)
            .map_err(|e| ProtocolError::Serialization(format!("invalid bundle base64: {e}")))?;
        serde_json::from_slice(&raw).map_err(|e| ProtocolError::Serialization(e.to_string()))
    }

    /// Encodes the bundle into a compact binary `fdesk://v2/<base64url>` string suitable for QR
    /// codes and short copy/paste onboarding.
    pub fn to_compact_token(&self) -> Result<String> {
        let sig: [u8; 64] = self
            .identity
            .cert
            .root_signature
            .as_slice()
            .try_into()
            .map_err(|_| ProtocolError::Serialization("root_signature must be 64 bytes".into()))?;
        let mut raw = Vec::with_capacity(240);
        raw.push(COMPACT_V2_VERSION);
        raw.extend_from_slice(&self.root_pubkey);
        raw.extend_from_slice(self.identity.cert.device_id.as_bytes());
        raw.extend_from_slice(&self.identity.secret_key);
        raw.extend_from_slice(&self.identity.cert.issued_at.to_be_bytes());
        raw.extend_from_slice(&self.identity.cert.expires_at.to_be_bytes());
        raw.extend_from_slice(&sig);
        write_str_u16(&mut raw, &self.identity.cert.device_name)?;

        let relay_count = u8::try_from(self.relay_nodes.len())
            .map_err(|_| ProtocolError::Serialization("too many relay nodes".into()))?;
        raw.push(relay_count);
        for node in &self.relay_nodes {
            write_str_u16(&mut raw, &node.id)?;
            write_str_u16(&mut raw, &node.name)?;
            write_str_u16(&mut raw, &node.addr)?;
            raw.push(match node.role {
                RelayNodeRole::PrimaryControl => 0,
                RelayNodeRole::FilePreferred => 1,
                RelayNodeRole::Backup => 2,
            });
        }

        Ok(format!("{COMPACT_TOKEN_PREFIX}{}", B64URL.encode(raw)))
    }

    pub fn from_compact_token(token: &str) -> Result<Self> {
        let clean: String = token
            .trim()
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let body = clean
            .strip_prefix(COMPACT_TOKEN_PREFIX)
            .ok_or_else(|| ProtocolError::Serialization("missing fdesk://v2/ prefix".into()))?;
        let raw = B64URL
            .decode(body)
            .map_err(|e| ProtocolError::Serialization(format!("invalid compact base64url: {e}")))?;

        let mut cur = raw.as_slice();
        let ver = read_u8(&mut cur)?;
        if ver != COMPACT_V2_VERSION {
            return Err(ProtocolError::Serialization(format!(
                "unsupported compact bundle version: {ver}"
            )));
        }
        let root_pubkey: [u8; 32] = read_exact_arr(&mut cur)?;
        let device_id = Uuid::from_bytes(read_exact_arr::<16>(&mut cur)?);
        let secret_key: [u8; 32] = read_exact_arr(&mut cur)?;
        let issued_at = u64::from_be_bytes(read_exact_arr::<8>(&mut cur)?);
        let expires_at = u64::from_be_bytes(read_exact_arr::<8>(&mut cur)?);
        let sig: [u8; 64] = read_exact_arr(&mut cur)?;
        let device_name = read_str_u16(&mut cur)?;

        let relay_count = read_u8(&mut cur)? as usize;
        let mut relay_nodes = Vec::with_capacity(relay_count);
        for _ in 0..relay_count {
            let id = read_str_u16(&mut cur)?;
            let name = read_str_u16(&mut cur)?;
            let addr = read_str_u16(&mut cur)?;
            let role = match read_u8(&mut cur)? {
                0 => RelayNodeRole::PrimaryControl,
                1 => RelayNodeRole::FilePreferred,
                2 => RelayNodeRole::Backup,
                other => {
                    return Err(ProtocolError::Serialization(format!(
                        "invalid relay role byte: {other}"
                    )))
                }
            };
            relay_nodes.push(RelayNodeConfig {
                id,
                name,
                addr,
                role,
            });
        }
        if !cur.is_empty() {
            return Err(ProtocolError::Serialization(
                "trailing bytes in compact bundle".into(),
            ));
        }

        let signing = SigningKey::from_bytes(&secret_key);
        let device_pubkey = signing.verifying_key().to_bytes();

        Ok(Self {
            root_pubkey,
            identity: DeviceIdentity {
                cert: DeviceCert {
                    version: 1,
                    device_id,
                    device_name,
                    device_pubkey,
                    issued_at,
                    expires_at,
                    root_signature: sig.to_vec(),
                },
                secret_key,
            },
            relay_nodes,
        })
    }

    /// Parses any supported credential representation: `fdesk://v2/...`, `.fdesk` JSON, or legacy
    /// Base64-encoded JSON.
    pub fn from_any_str(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ProtocolError::Serialization("导入内容为空".into()));
        }
        if trimmed.starts_with('{') {
            return Self::from_json_str(trimmed);
        }
        let no_ws: String = trimmed.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        if no_ws.starts_with(COMPACT_TOKEN_PREFIX) {
            return Self::from_compact_token(&no_ws);
        }
        Self::from_b64_token(&no_ws)
    }
}

fn write_str_u16(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    let len = u16::try_from(bytes.len())
        .map_err(|_| ProtocolError::Serialization("string exceeds u16 length".into()))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn read_u8(cur: &mut &[u8]) -> Result<u8> {
    let (&first, rest) = cur
        .split_first()
        .ok_or_else(|| ProtocolError::Serialization("unexpected EOF in compact bundle".into()))?;
    *cur = rest;
    Ok(first)
}

fn read_exact_arr<const N: usize>(cur: &mut &[u8]) -> Result<[u8; N]> {
    if cur.len() < N {
        return Err(ProtocolError::Serialization(
            "unexpected EOF in compact bundle".into(),
        ));
    }
    let (head, rest) = cur.split_at(N);
    *cur = rest;
    head.try_into()
        .map_err(|_| ProtocolError::Serialization("slice length mismatch".into()))
}

fn read_str_u16(cur: &mut &[u8]) -> Result<String> {
    let len = u16::from_be_bytes(read_exact_arr::<2>(cur)?) as usize;
    if cur.len() < len {
        return Err(ProtocolError::Serialization(
            "truncated string in compact bundle".into(),
        ));
    }
    let (head, rest) = cur.split_at(len);
    *cur = rest;
    std::str::from_utf8(head)
        .map(|s| s.to_string())
        .map_err(|e| ProtocolError::Serialization(format!("invalid utf-8 string: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_sign_and_verify_roundtrip() {
        let root = RootKeypair::generate();
        let id = Uuid::new_v4();
        let identity = root.sign_device_cert(id, "HomePC", 3600);
        let now = unix_timestamp_secs();
        assert!(identity.cert.verify(&root.pubkey_bytes(), now).is_ok());

        // Wrong root key fails
        let other_root = RootKeypair::generate();
        assert!(identity.cert.verify(&other_root.pubkey_bytes(), now).is_err());

        // Tampered device name fails
        let mut tampered = identity.cert.clone();
        tampered.device_name = "EvilPC".into();
        assert!(tampered.verify(&root.pubkey_bytes(), now).is_err());

        // Expired fails
        assert!(identity
            .cert
            .verify(&root.pubkey_bytes(), now + 7200)
            .is_err());
    }

    #[test]
    fn bundle_json_and_token_roundtrip() {
        let root = RootKeypair::generate();
        let identity = root.sign_device_cert(Uuid::new_v4(), "OfficePC", 86400);
        let bundle = DeviceBundle {
            root_pubkey: root.pubkey_bytes(),
            identity,
            relay_nodes: vec![RelayNodeConfig {
                id: "cn".into(),
                name: "国内 5M".into(),
                addr: "1.2.3.4:19443".into(),
                role: RelayNodeRole::PrimaryControl,
            }],
        };

        let token = bundle.to_b64_token().unwrap();
        let decoded = DeviceBundle::from_b64_token(&token).unwrap();
        assert_eq!(bundle, decoded);
    }

    #[test]
    fn compact_token_roundtrip_and_size() {
        let root = RootKeypair::generate();
        let identity = root.sign_device_cert(Uuid::new_v4(), "Laptop", 3650 * 86400);
        let bundle = DeviceBundle {
            root_pubkey: root.pubkey_bytes(),
            identity,
            relay_nodes: vec![RelayNodeConfig {
                id: "cn-5m".into(),
                name: "国内 5M".into(),
                addr: "203.0.113.10:9442".into(),
                role: RelayNodeRole::PrimaryControl,
            }],
        };

        let compact = bundle.to_compact_token().unwrap();
        assert!(compact.starts_with(COMPACT_TOKEN_PREFIX));
        assert!(compact.len() < 360, "compact token unexpectedly long: {}", compact.len());

        let spaced = format!("  {}\n{}  ", &compact[..40], &compact[40..]);
        let decoded = DeviceBundle::from_any_str(&spaced).unwrap();
        assert_eq!(decoded, bundle);

        let now = unix_timestamp_secs();
        assert!(decoded.identity.cert.verify(&bundle.root_pubkey, now).is_ok());

        let legacy = bundle.to_b64_token().unwrap();
        assert_eq!(DeviceBundle::from_any_str(&legacy).unwrap(), bundle);
        let json = bundle.to_json_pretty().unwrap();
        assert_eq!(DeviceBundle::from_any_str(&json).unwrap(), bundle);
    }

    #[test]
    fn revocation_command_sign_and_verify() {
        let root = RootKeypair::generate();
        let other = RootKeypair::generate();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let cmd = root.sign_revocation_list(vec![id2, id1, id2]);
        assert_eq!(cmd.revoked_device_ids.len(), 2);

        let now = unix_timestamp_secs();
        assert!(cmd.verify(&root.pubkey_bytes(), now).is_ok());
        assert!(cmd.verify(&other.pubkey_bytes(), now).is_err());

        let mut tampered = cmd.clone();
        tampered.revoked_device_ids.push(Uuid::new_v4());
        assert!(tampered.verify(&root.pubkey_bytes(), now).is_err());
    }
}
