//! Signed update manifest and UpdateChannel wire protocol.

use crate::{ProtocolError, Result};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

pub const MANIFEST_SIGN_DOMAIN: &[u8] = b"FLASHDESK-UPDATE-MANIFEST-V1";
pub const MAX_UPDATE_ARTIFACT_SIZE: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FullPackageInfo {
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub target_size: u64,
    pub target_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeltaPatchInfo {
    pub from_version: String,
    pub from_sha256: String,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateManifest {
    pub format_version: u16,
    pub version: String,
    pub min_base_version: String,
    pub released_at: u64,
    pub notes: String,
    pub full: FullPackageInfo,
    pub deltas: Vec<DeltaPatchInfo>,
}

fn push_len_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

impl UpdateManifest {
    /// Deterministic canonical bytes covered by `SignedUpdateManifest::root_signature`.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        out.extend_from_slice(MANIFEST_SIGN_DOMAIN);
        out.extend_from_slice(&self.format_version.to_be_bytes());
        push_len_str(&mut out, &self.version);
        push_len_str(&mut out, &self.min_base_version);
        out.extend_from_slice(&self.released_at.to_be_bytes());
        push_len_str(&mut out, &self.notes);

        push_len_str(&mut out, &self.full.filename);
        out.extend_from_slice(&self.full.size.to_be_bytes());
        push_len_str(&mut out, &self.full.sha256);
        out.extend_from_slice(&self.full.target_size.to_be_bytes());
        push_len_str(&mut out, &self.full.target_sha256);

        out.extend_from_slice(&(self.deltas.len() as u32).to_be_bytes());
        for d in &self.deltas {
            push_len_str(&mut out, &d.from_version);
            push_len_str(&mut out, &d.from_sha256);
            push_len_str(&mut out, &d.filename);
            out.extend_from_slice(&d.size.to_be_bytes());
            push_len_str(&mut out, &d.sha256);
        }
        out
    }

    /// Returns every artifact filename referenced by this manifest (full package + all deltas).
    pub fn referenced_filenames(&self) -> Vec<&str> {
        let mut names = Vec::with_capacity(1 + self.deltas.len());
        names.push(self.full.filename.as_str());
        for d in &self.deltas {
            names.push(d.filename.as_str());
        }
        names
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedUpdateManifest {
    pub manifest: UpdateManifest,
    pub root_signature: Vec<u8>,
}

impl SignedUpdateManifest {
    pub fn verify(&self, root_pubkey: &[u8; 32], now_unix: u64) -> Result<()> {
        let m = &self.manifest;
        if m.format_version != 1 {
            return Err(ProtocolError::Auth(format!(
                "unsupported manifest format version: {}",
                m.format_version
            )));
        }
        Version::parse(m.version.trim()).map_err(|e| {
            ProtocolError::Auth(format!("invalid manifest version '{}': {e}", m.version))
        })?;
        Version::parse(m.min_base_version.trim()).map_err(|e| {
            ProtocolError::Auth(format!(
                "invalid min_base_version '{}': {e}",
                m.min_base_version
            ))
        })?;
        if m.released_at > now_unix.saturating_add(300) {
            return Err(ProtocolError::Auth(
                "update manifest released_at is in the future".into(),
            ));
        }
        if !is_safe_artifact_name(&m.full.filename)
            || !is_valid_sha256_hex(&m.full.sha256)
            || !is_valid_sha256_hex(&m.full.target_sha256)
            || m.full.size == 0
            || m.full.size > MAX_UPDATE_ARTIFACT_SIZE
            || m.full.target_size == 0
        {
            return Err(ProtocolError::Auth("invalid full package metadata".into()));
        }
        for d in &m.deltas {
            if !is_safe_artifact_name(&d.filename)
                || !is_valid_sha256_hex(&d.from_sha256)
                || !is_valid_sha256_hex(&d.sha256)
                || d.size == 0
                || d.size > MAX_UPDATE_ARTIFACT_SIZE
            {
                return Err(ProtocolError::Auth(format!(
                    "invalid delta metadata for {}",
                    d.filename
                )));
            }
        }

        let verifying_key = VerifyingKey::from_bytes(root_pubkey)
            .map_err(|e| ProtocolError::Auth(format!("invalid root pubkey: {e}")))?;
        let signature = Signature::from_slice(&self.root_signature)
            .map_err(|e| ProtocolError::Auth(format!("invalid manifest signature: {e}")))?;
        verifying_key
            .verify(&m.canonical_bytes(), &signature)
            .map_err(|_| ProtocolError::Auth("update manifest signature verification failed".into()))
    }
}

pub fn is_safe_artifact_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn is_newer_version(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate.trim()), Version::parse(current.trim())) {
        (Ok(c), Ok(cur)) => c > cur,
        _ => false,
    }
}

pub fn meets_min_base(base_ver: &str, min_base: &str) -> bool {
    match (Version::parse(base_ver.trim()), Version::parse(min_base.trim())) {
        (Ok(b), Ok(m)) => b >= m,
        _ => false,
    }
}

pub fn sha256_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishFileSpec {
    pub filename: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UpdateClientMsg {
    GetManifest,
    DownloadChunk {
        filename: String,
        offset: u64,
        max_len: u32,
    },
    BeginPublish {
        manifest: SignedUpdateManifest,
        files: Vec<PublishFileSpec>,
    },
    UploadChunk {
        filename: String,
        offset: u64,
        data: Vec<u8>,
        eof: bool,
    },
    CommitPublish,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UpdateServerMsg {
    Manifest(Option<SignedUpdateManifest>),
    ChunkData {
        offset: u64,
        data: Vec<u8>,
        total_size: u64,
        eof: bool,
    },
    PublishReady,
    UploadAck {
        filename: String,
        received: u64,
    },
    PublishCommitted {
        version: String,
        files: usize,
    },
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::RootKeypair;
    use crate::unix_timestamp_secs;

    fn sample_manifest(released_at: u64) -> UpdateManifest {
        let hash = "a".repeat(64);
        UpdateManifest {
            format_version: 1,
            version: "0.2.0".into(),
            min_base_version: "1.0.0".into(),
            released_at,
            notes: "delta update test".into(),
            full: FullPackageInfo {
                filename: "flashdesk-core-0.2.0.exe.deflate".into(),
                size: 1024,
                sha256: hash.clone(),
                target_size: 2048,
                target_sha256: hash.clone(),
            },
            deltas: vec![DeltaPatchInfo {
                from_version: "0.1.0".into(),
                from_sha256: "b".repeat(64),
                filename: "flashdesk-0.1.0-to-0.2.0.fldp".into(),
                size: 256,
                sha256: hash,
            }],
        }
    }

    #[test]
    fn manifest_sign_verify_and_tamper_checks() {
        let root = RootKeypair::generate();
        let other = RootKeypair::generate();
        let now = unix_timestamp_secs();

        let signed = root.sign_update_manifest(sample_manifest(now));
        assert!(signed.verify(&root.pubkey_bytes(), now).is_ok());
        assert!(signed.verify(&other.pubkey_bytes(), now).is_err());

        let mut tampered = signed.clone();
        tampered.manifest.notes.push('!');
        assert!(tampered.verify(&root.pubkey_bytes(), now).is_err());

        let future = root.sign_update_manifest(sample_manifest(now + 600));
        assert!(future.verify(&root.pubkey_bytes(), now).is_err());
    }

    #[test]
    fn safe_artifact_names_and_semver_checks() {
        assert!(is_safe_artifact_name("flashdesk-core-0.2.0.exe.deflate"));
        assert!(is_safe_artifact_name("patch_0.1-0.2.fldp"));
        assert!(!is_safe_artifact_name(""));
        assert!(!is_safe_artifact_name("../secret"));
        assert!(!is_safe_artifact_name("a/b"));
        assert!(!is_safe_artifact_name(r"a\b"));
        assert!(!is_safe_artifact_name(".hidden"));

        assert!(is_newer_version("0.2.0", "0.1.0"));
        assert!(is_newer_version("0.2.0", "0.2.0-rc.1"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.0.9", "0.1.0"));

        assert!(meets_min_base("1.0.0", "1.0.0"));
        assert!(meets_min_base("1.1.0", "1.0.0"));
        assert!(!meets_min_base("1.0.0", "1.0.1"));
    }
}
