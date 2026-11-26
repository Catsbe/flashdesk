//! Self-extracting `FLDSKSTP` trailer payload format shared by `release-tool` and `installer`.

use crate::sha256_bytes;
use anyhow::{Context, Result};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const SETUP_TRAILER_MAGIC: &[u8; 8] = b"FLDSKSTP";
pub const SETUP_TRAILER_LEN: u64 = 8 + 8 + 8; // 24 bytes
const MAX_SETUP_BLOB_BYTES: u64 = 96 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupBinaryEntry {
    pub role: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SetupPayloadManifest {
    pub format_version: u16,
    pub core_version: String,
    pub base_version: String,
    pub entries: Vec<SetupBinaryEntry>,
}

pub struct SetupInputBinary<'a> {
    pub role: &'a str,
    pub bytes: &'a [u8],
}

/// Packs `stub_bytes` + compressed payload (`manifest` + binary blobs) + 24-byte `FLDSKSTP` trailer.
pub fn build_self_extracting_setup(
    stub_bytes: &[u8],
    core_version: &str,
    base_version: &str,
    inputs: &[SetupInputBinary<'_>],
) -> Result<Vec<u8>> {
    let mut entries = Vec::with_capacity(inputs.len());
    let mut raw_blob = Vec::new();
    for inp in inputs {
        entries.push(SetupBinaryEntry {
            role: inp.role.to_string(),
            size: inp.bytes.len() as u64,
            sha256: sha256_bytes(inp.bytes),
        });
        raw_blob.extend_from_slice(inp.bytes);
    }
    let manifest = SetupPayloadManifest {
        format_version: 1,
        core_version: core_version.to_string(),
        base_version: base_version.to_string(),
        entries,
    };
    let manifest_bytes = serde_json::to_vec(&manifest)?;

    let mut plain = Vec::with_capacity(4 + manifest_bytes.len() + raw_blob.len());
    plain.extend_from_slice(&(manifest_bytes.len() as u32).to_be_bytes());
    plain.extend_from_slice(&manifest_bytes);
    plain.extend_from_slice(&raw_blob);

    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(6));
    encoder.write_all(&plain)?;
    let compressed = encoder.finish()?;

    let payload_offset = stub_bytes.len() as u64;
    let payload_len = compressed.len() as u64;

    let mut out = Vec::with_capacity(stub_bytes.len() + compressed.len() + SETUP_TRAILER_LEN as usize);
    out.extend_from_slice(stub_bytes);
    out.extend_from_slice(&compressed);
    out.extend_from_slice(SETUP_TRAILER_MAGIC);
    out.extend_from_slice(&payload_offset.to_be_bytes());
    out.extend_from_slice(&payload_len.to_be_bytes());
    Ok(out)
}

pub struct ExtractedSetupPayload {
    pub manifest: SetupPayloadManifest,
    pub binaries: Vec<(String, Vec<u8>)>,
}

/// Reads and verifies the embedded `FLDSKSTP` payload from the end of `exe_path`.
pub fn read_self_extracting_setup(exe_path: &Path) -> Result<ExtractedSetupPayload> {
    let mut file = std::fs::File::open(exe_path)
        .with_context(|| format!("failed to open setup executable {}", exe_path.display()))?;
    let total_len = file.metadata()?.len();
    anyhow::ensure!(
        total_len >= SETUP_TRAILER_LEN,
        "安装包体积过小，未找到内嵌载荷"
    );

    file.seek(SeekFrom::End(-(SETUP_TRAILER_LEN as i64)))?;
    let mut trailer = [0u8; SETUP_TRAILER_LEN as usize];
    file.read_exact(&mut trailer)?;
    anyhow::ensure!(
        &trailer[..8] == SETUP_TRAILER_MAGIC,
        "当前程序仅为空安装包壳体（未附加 FLDSKSTP 载荷），请使用 flashdesk-release build-setup 打包"
    );
    let offset = u64::from_be_bytes(trailer[8..16].try_into().unwrap());
    let len = u64::from_be_bytes(trailer[16..24].try_into().unwrap());
    anyhow::ensure!(
        len > 0
            && len <= MAX_SETUP_BLOB_BYTES
            && offset.saturating_add(len).saturating_add(SETUP_TRAILER_LEN) == total_len,
        "安装包内嵌载荷边界校验失败"
    );

    file.seek(SeekFrom::Start(offset))?;
    let mut compressed = vec![0u8; len as usize];
    file.read_exact(&mut compressed)?;

    let mut decoder = DeflateDecoder::new(compressed.as_slice());
    let mut plain = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        anyhow::ensure!(
            (plain.len() + n) as u64 <= MAX_SETUP_BLOB_BYTES * 2,
            "安装包解压体积超出安全限制"
        );
        plain.extend_from_slice(&buf[..n]);
    }

    anyhow::ensure!(plain.len() >= 4, "安装包载荷头损坏");
    let mlen = u32::from_be_bytes(plain[..4].try_into().unwrap()) as usize;
    anyhow::ensure!(plain.len() >= 4 + mlen, "安装包清单截断");
    let manifest: SetupPayloadManifest = serde_json::from_slice(&plain[4..4 + mlen])?;
    anyhow::ensure!(manifest.format_version == 1, "不支持的安装包格式版本");

    let mut cursor = 4 + mlen;
    let mut binaries = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let sz = entry.size as usize;
        anyhow::ensure!(
            cursor.saturating_add(sz) <= plain.len(),
            "安装包组件 {} 数据截断",
            entry.role
        );
        let bytes = plain[cursor..cursor + sz].to_vec();
        cursor += sz;
        let got_sha = sha256_bytes(&bytes);
        anyhow::ensure!(
            got_sha == entry.sha256,
            "安装包组件 {} SHA-256 校验失败",
            entry.role
        );
        binaries.push((entry.role.clone(), bytes));
    }
    anyhow::ensure!(cursor == plain.len(), "安装包尾部存在冗余字节");

    Ok(ExtractedSetupPayload { manifest, binaries })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_payload_roundtrip_and_tamper_detection() {
        let dir = std::env::temp_dir().join(format!("flashdesk-stp-{}", crate::unix_now()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe_path = dir.join("FlashDesk-Setup.exe");

        let stub = b"MZ-FAKE-PE-STUB";
        let svc = b"svc-binary".repeat(200);
        let launcher = b"launcher-binary".repeat(100);
        let packed = build_self_extracting_setup(
            stub,
            "0.1.0",
            "1.0.0",
            &[
                SetupInputBinary {
                    role: "svc",
                    bytes: &svc,
                },
                SetupInputBinary {
                    role: "launcher",
                    bytes: &launcher,
                },
            ],
        )
        .unwrap();
        std::fs::write(&exe_path, &packed).unwrap();

        let extracted = read_self_extracting_setup(&exe_path).unwrap();
        assert_eq!(extracted.manifest.core_version, "0.1.0");
        assert_eq!(extracted.binaries.len(), 2);
        assert_eq!(extracted.binaries[0].0, "svc");
        assert_eq!(extracted.binaries[0].1, svc);
        assert_eq!(extracted.binaries[1].1, launcher);

        // Corrupting the payload triggers checksum / deflate error.
        let mut bad = packed;
        let mid = stub.len() + 10;
        bad[mid] ^= 0xFF;
        std::fs::write(&exe_path, &bad).unwrap();
        assert!(read_self_extracting_setup(&exe_path).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
