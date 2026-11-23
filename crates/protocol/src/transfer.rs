use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const FILE_CHUNK_SIZE: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileMsg {
    ListDirReq {
        path: String,
    },
    ListDirResp {
        path: String,
        entries: Vec<FileEntry>,
        error: Option<String>,
    },
    UploadInit {
        transfer_id: Uuid,
        remote_dir: String,
        file_name: String,
        total_size: u64,
        file_sha256: [u8; 32],
    },
    UploadAck {
        transfer_id: Uuid,
        resume_offset: u64,
        already_complete: bool,
        saved_path: String,
        error: Option<String>,
    },
    FileChunk {
        transfer_id: Uuid,
        offset: u64,
        chunk_sha256: [u8; 32],
        data: Vec<u8>,
    },
    UploadComplete {
        transfer_id: Uuid,
    },
    DownloadReq {
        transfer_id: Uuid,
        remote_path: String,
        resume_offset: u64,
    },
    DownloadMeta {
        transfer_id: Uuid,
        file_name: String,
        total_size: u64,
        file_sha256: [u8; 32],
        resume_offset: u64,
        error: Option<String>,
    },
    DownloadComplete {
        transfer_id: Uuid,
    },
    TransferResult {
        transfer_id: Uuid,
        ok: bool,
        saved_path: String,
        error: Option<String>,
    },
    CancelTransfer {
        transfer_id: Uuid,
    },
}
