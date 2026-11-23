use crate::{unix_timestamp_secs, ProtocolError, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const CERT_SIGN_DOMAIN: &[u8] = b"FLASHDESK-DEVICE-CERT-V1";
const CHALLENGE_SIGN_DOMAIN: &[u8] = b"FLASHDESK-RELAY-CHALLENGE-V1";

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
        let raw = B64
            .decode(token.trim())
            .map_err(|e| ProtocolError::Serialization(format!("invalid bundle base64: {e}")))?;
        serde_json::from_slice(&raw).map_err(|e| ProtocolError::Serialization(e.to_string()))
    }
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
}
