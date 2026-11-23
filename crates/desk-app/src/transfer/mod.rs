pub mod client_manager;
pub mod host_handler;

use anyhow::{Context, Result};
use protocol::transfer::FileEntry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use uuid::Uuid;
use windows::Win32::Storage::FileSystem::GetLogicalDrives;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeMeta {
    pub total_size: u64,
    pub file_sha256: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Active,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct TransferRecord {
    pub transfer_id: Uuid,
    pub file_name: String,
    pub local_path: String,
    pub remote_path: String,
    pub direction: TransferDirection,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub speed_mbps: f32,
    pub via_label: String,
    pub resumed: bool,
    pub sha256_verified: bool,
    pub state: TransferState,
}

pub fn part_and_meta_paths(final_path: &Path) -> (PathBuf, PathBuf) {
    let mut part_os = OsString::from(final_path.as_os_str());
    part_os.push(".fdesk.part");
    let mut meta_os = OsString::from(final_path.as_os_str());
    meta_os.push(".fdesk.meta");
    (PathBuf::from(part_os), PathBuf::from(meta_os))
}

pub fn compute_bytes_sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

pub fn compute_file_sha256(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open file for SHA-256: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

pub fn default_receive_dir() -> PathBuf {
    dirs::desktop_dir()
        .or_else(dirs::download_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn list_local_dir(path: &str) -> Result<Vec<FileEntry>> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        let mut entries = Vec::new();
        if let Some(desktop) = dirs::desktop_dir() {
            entries.push(FileEntry {
                name: "桌面 (Desktop)".into(),
                path: desktop.to_string_lossy().into_owned(),
                is_dir: true,
                size: 0,
                modified_unix: 0,
            });
        }
        if let Some(dl) = dirs::download_dir() {
            entries.push(FileEntry {
                name: "下载 (Downloads)".into(),
                path: dl.to_string_lossy().into_owned(),
                is_dir: true,
                size: 0,
                modified_unix: 0,
            });
        }
        let mask = unsafe { GetLogicalDrives() };
        for bit in 0..26u32 {
            if (mask & (1 << bit)) != 0 {
                let letter = (b'A' + bit as u8) as char;
                let root = format!("{letter}:\\");
                entries.push(FileEntry {
                    name: format!("本地磁盘 ({letter}:)"),
                    path: root,
                    is_dir: true,
                    size: 0,
                    modified_unix: 0,
                });
            }
        }
        return Ok(entries);
    }

    let dir_path = Path::new(trimmed);
    let mut dirs_vec = Vec::new();
    let mut files_vec = Vec::new();

    for item in std::fs::read_dir(dir_path)
        .with_context(|| format!("无法读取目录: {}", dir_path.display()))?
    {
        let Ok(entry) = item else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = if is_dir {
            0
        } else {
            meta.as_ref().map(|m| m.len()).unwrap_or(0)
        };
        let modified_unix = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let fe = FileEntry {
            name,
            path: entry.path().to_string_lossy().into_owned(),
            is_dir,
            size,
            modified_unix,
        };
        if is_dir {
            dirs_vec.push(fe);
        } else {
            files_vec.push(fe);
        }
    }

    dirs_vec.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files_vec.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    dirs_vec.extend(files_vec);
    Ok(dirs_vec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::client_manager::{run_download_over_stream, run_upload_over_stream};
    use crate::transfer::host_handler::run_host_file_session;
    use protocol::auth::RootKeypair;
    use protocol::noise::NoiseStream;
    use std::sync::atomic::AtomicBool;

    async fn make_paired_noise_streams() -> (
        NoiseStream<tokio::io::DuplexStream>,
        NoiseStream<tokio::io::DuplexStream>,
    ) {
        let root = RootKeypair::generate();
        let id_a = root.sign_device_cert(Uuid::new_v4(), "Viewer", 3600);
        let id_b = root.sign_device_cert(Uuid::new_v4(), "Host", 3600);
        let root_pub = root.pubkey_bytes();
        let (sa, sb) = tokio::io::duplex(256 * 1024);
        let (ra, rb) = tokio::join!(
            NoiseStream::handshake_initiator(sa, &root_pub, &id_a, &[]),
            NoiseStream::handshake_responder(sb, &root_pub, &id_b, &[])
        );
        (ra.unwrap(), rb.unwrap())
    }

    #[tokio::test]
    async fn file_upload_and_sha256_verify() {
        let tmp = std::env::temp_dir().join(format!("fdesk_ul_{}", Uuid::new_v4()));
        let src_dir = tmp.join("src");
        let dst_dir = tmp.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();

        let src_file = src_dir.join("test_payload.bin");
        let data: Vec<u8> = (0..(700 * 1024)).map(|i| ((i * 31) % 253) as u8).collect();
        std::fs::write(&src_file, &data).unwrap();

        let (viewer_ns, host_ns) = make_paired_noise_streams().await;
        let host_task = tokio::spawn(run_host_file_session(host_ns));

        let (mut r, mut w, _) = viewer_ns.into_split();
        let records = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let tid = Uuid::new_v4();
        let res = run_upload_over_stream(
            &mut r,
            &mut w,
            tid,
            &src_file,
            &dst_dir.to_string_lossy(),
            "test-relay",
            &records,
            &cancel,
            &|| {},
        )
        .await
        .unwrap();

        assert!(res.sha256_verified);
        assert!(!res.resumed);
        let uploaded = std::fs::read(dst_dir.join("test_payload.bin")).unwrap();
        assert_eq!(uploaded, data);

        drop(r);
        drop(w);
        host_task.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn file_upload_resumes_from_partial_file() {
        let tmp = std::env::temp_dir().join(format!("fdesk_resume_{}", Uuid::new_v4()));
        let src_dir = tmp.join("src");
        let dst_dir = tmp.join("dst");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();

        let src_file = src_dir.join("resume_payload.bin");
        let data: Vec<u8> = (0..(600 * 1024)).map(|i| ((i * 17) % 251) as u8).collect();
        std::fs::write(&src_file, &data).unwrap();
        let expected_sha = compute_bytes_sha256(&data);

        let final_dst = dst_dir.join("resume_payload.bin");
        let (part_path, meta_path) = part_and_meta_paths(&final_dst);
        let half = 256 * 1024;
        std::fs::write(&part_path, &data[..half]).unwrap();
        let meta = ResumeMeta {
            total_size: data.len() as u64,
            file_sha256: expected_sha,
        };
        std::fs::write(&meta_path, serde_json::to_vec(&meta).unwrap()).unwrap();

        let (viewer_ns, host_ns) = make_paired_noise_streams().await;
        let host_task = tokio::spawn(run_host_file_session(host_ns));

        let (mut r, mut w, _) = viewer_ns.into_split();
        let records = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let tid = Uuid::new_v4();
        let res = run_upload_over_stream(
            &mut r,
            &mut w,
            tid,
            &src_file,
            &dst_dir.to_string_lossy(),
            "test-relay",
            &records,
            &cancel,
            &|| {},
        )
        .await
        .unwrap();

        assert!(res.resumed);
        assert!(res.sha256_verified);
        assert!(!part_path.exists());
        assert!(!meta_path.exists());
        assert_eq!(std::fs::read(&final_dst).unwrap(), data);

        drop(r);
        drop(w);
        host_task.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn file_download_and_sha256_verify() {
        let tmp = std::env::temp_dir().join(format!("fdesk_dl_{}", Uuid::new_v4()));
        let remote_dir = tmp.join("remote");
        let local_dir = tmp.join("local");
        std::fs::create_dir_all(&remote_dir).unwrap();
        std::fs::create_dir_all(&local_dir).unwrap();

        let remote_file = remote_dir.join("movie_clip.dat");
        let data: Vec<u8> = (0..(650 * 1024)).map(|i| ((i * 13) % 241) as u8).collect();
        std::fs::write(&remote_file, &data).unwrap();

        let (viewer_ns, host_ns) = make_paired_noise_streams().await;
        let host_task = tokio::spawn(run_host_file_session(host_ns));

        let (mut r, mut w, _) = viewer_ns.into_split();
        let records = Arc::new(Mutex::new(Vec::new()));
        let cancel = Arc::new(AtomicBool::new(false));
        let tid = Uuid::new_v4();
        let res = run_download_over_stream(
            &mut r,
            &mut w,
            tid,
            &remote_file.to_string_lossy(),
            &local_dir,
            "test-relay",
            &records,
            &cancel,
            &|| {},
        )
        .await
        .unwrap();

        assert!(res.sha256_verified);
        assert_eq!(
            std::fs::read(local_dir.join("movie_clip.dat")).unwrap(),
            data
        );

        drop(r);
        drop(w);
        host_task.abort();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    use std::sync::{Arc, Mutex};
}
