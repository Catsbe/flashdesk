use crate::transfer::{
    compute_bytes_sha256, compute_file_sha256, default_receive_dir, list_local_dir,
    part_and_meta_paths, ResumeMeta,
};
use anyhow::Result;
use protocol::noise::{NoiseStream, NoiseWriter};
use protocol::transfer::{FileMsg, FILE_CHUNK_SIZE};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, SeekFrom};
use tokio::sync::Mutex;
use uuid::Uuid;

struct ActiveUpload {
    final_path: PathBuf,
    part_path: PathBuf,
    meta_path: PathBuf,
    file: tokio::fs::File,
    expected_sha256: [u8; 32],
}

pub async fn run_host_file_session<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: NoiseStream<S>,
) -> Result<()> {
    let (mut reader, writer, peer_cert) = stream.into_split();
    tracing::info!("host file channel started for '{}'", peer_cert.device_name);

    let writer = Arc::new(Mutex::new(writer));
    let active_cancels: Arc<Mutex<HashMap<Uuid, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut uploads: HashMap<Uuid, ActiveUpload> = HashMap::new();

    while let Ok(raw) = reader.recv_msg().await {
        let Ok(msg) = bincode::deserialize::<FileMsg>(&raw) else {
            continue;
        };

        match msg {
            FileMsg::ListDirReq { path } => {
                let p_clone = path.clone();
                let res = tokio::task::spawn_blocking(move || list_local_dir(&p_clone)).await;
                let resp = match res {
                    Ok(Ok(entries)) => FileMsg::ListDirResp {
                        path,
                        entries,
                        error: None,
                    },
                    Ok(Err(e)) => FileMsg::ListDirResp {
                        path,
                        entries: Vec::new(),
                        error: Some(e.to_string()),
                    },
                    Err(e) => FileMsg::ListDirResp {
                        path,
                        entries: Vec::new(),
                        error: Some(e.to_string()),
                    },
                };
                if send_file_msg(&writer, &resp).await.is_err() {
                    break;
                }
            }
            FileMsg::UploadInit {
                transfer_id,
                remote_dir,
                file_name,
                total_size,
                file_sha256,
            } => {
                let ack = handle_upload_init(
                    transfer_id,
                    &remote_dir,
                    &file_name,
                    total_size,
                    file_sha256,
                    &mut uploads,
                )
                .await;
                if send_file_msg(&writer, &ack).await.is_err() {
                    break;
                }
            }
            FileMsg::FileChunk {
                transfer_id,
                offset,
                chunk_sha256,
                data,
            } => {
                if compute_bytes_sha256(&data) != chunk_sha256 {
                    uploads.remove(&transfer_id);
                    let _ = send_file_msg(
                        &writer,
                        &FileMsg::TransferResult {
                            transfer_id,
                            ok: false,
                            saved_path: String::new(),
                            error: Some("数据块 SHA-256 校验失败".into()),
                        },
                    )
                    .await;
                    continue;
                }
                if let Some(up) = uploads.get_mut(&transfer_id) {
                    if let Err(e) = async {
                        up.file.seek(SeekFrom::Start(offset)).await?;
                        up.file.write_all(&data).await?;
                        Ok::<_, std::io::Error>(())
                    }
                    .await
                    {
                        uploads.remove(&transfer_id);
                        let _ = send_file_msg(
                            &writer,
                            &FileMsg::TransferResult {
                                transfer_id,
                                ok: false,
                                saved_path: String::new(),
                                error: Some(format!("写入磁盘失败: {e}")),
                            },
                        )
                        .await;
                    }
                }
            }
            FileMsg::UploadComplete { transfer_id } => {
                let res_msg = handle_upload_complete(transfer_id, &mut uploads).await;
                if send_file_msg(&writer, &res_msg).await.is_err() {
                    break;
                }
            }
            FileMsg::DownloadReq {
                transfer_id,
                remote_path,
                resume_offset,
            } => {
                let cancel = Arc::new(AtomicBool::new(false));
                active_cancels
                    .lock()
                    .await
                    .insert(transfer_id, Arc::clone(&cancel));
                let w_clone = Arc::clone(&writer);
                let cancels_clone = Arc::clone(&active_cancels);
                tokio::spawn(async move {
                    let _ = stream_download_to_peer(
                        w_clone,
                        transfer_id,
                        PathBuf::from(remote_path),
                        resume_offset,
                        cancel,
                    )
                    .await;
                    cancels_clone.lock().await.remove(&transfer_id);
                });
            }
            FileMsg::CancelTransfer { transfer_id } => {
                uploads.remove(&transfer_id);
                if let Some(flag) = active_cancels.lock().await.remove(&transfer_id) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }

    tracing::info!("host file channel closed for '{}'", peer_cert.device_name);
    Ok(())
}

async fn send_file_msg<W: AsyncWrite + Unpin>(
    writer: &Arc<Mutex<NoiseWriter<W>>>,
    msg: &FileMsg,
) -> Result<()> {
    let bytes = bincode::serialize(msg)?;
    let mut guard = writer.lock().await;
    guard.send_msg(&bytes).await?;
    Ok(())
}

async fn handle_upload_init(
    transfer_id: Uuid,
    remote_dir: &str,
    file_name: &str,
    total_size: u64,
    file_sha256: [u8; 32],
    uploads: &mut HashMap<Uuid, ActiveUpload>,
) -> FileMsg {
    let safe_name = Path::new(file_name)
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| "uploaded_file.bin".into());

    let dir = if remote_dir.trim().is_empty() {
        default_receive_dir()
    } else {
        PathBuf::from(remote_dir.trim())
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        return FileMsg::UploadAck {
            transfer_id,
            resume_offset: 0,
            already_complete: false,
            saved_path: String::new(),
            error: Some(format!("无法创建目标目录: {e}")),
        };
    }

    let final_path = dir.join(safe_name);
    let saved_str = final_path.to_string_lossy().into_owned();

    if final_path.exists() {
        if let Ok(meta) = std::fs::metadata(&final_path) {
            if meta.len() == total_size {
                let fp_clone = final_path.clone();
                if let Ok(Ok(existing_sha)) =
                    tokio::task::spawn_blocking(move || compute_file_sha256(&fp_clone)).await
                {
                    if existing_sha == file_sha256 {
                        return FileMsg::UploadAck {
                            transfer_id,
                            resume_offset: total_size,
                            already_complete: true,
                            saved_path: saved_str,
                            error: None,
                        };
                    }
                }
            }
        }
    }

    let (part_path, meta_path) = part_and_meta_paths(&final_path);
    let mut resume_offset = 0u64;

    if part_path.exists() && meta_path.exists() {
        if let Ok(meta_bytes) = std::fs::read(&meta_path) {
            if let Ok(rm) = serde_json::from_slice::<ResumeMeta>(&meta_bytes) {
                if rm.total_size == total_size && rm.file_sha256 == file_sha256 {
                    if let Ok(part_meta) = std::fs::metadata(&part_path) {
                        if part_meta.len() <= total_size {
                            resume_offset = part_meta.len();
                        }
                    }
                }
            }
        }
    }

    let file_res = if resume_offset > 0 {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&part_path)
            .await
    } else {
        let rm = ResumeMeta {
            total_size,
            file_sha256,
        };
        if let Ok(json) = serde_json::to_vec(&rm) {
            let _ = std::fs::write(&meta_path, json);
        }
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&part_path)
            .await
    };

    match file_res {
        Ok(file) => {
            uploads.insert(
                transfer_id,
                ActiveUpload {
                    final_path,
                    part_path,
                    meta_path,
                    file,
                    expected_sha256: file_sha256,
                },
            );
            FileMsg::UploadAck {
                transfer_id,
                resume_offset,
                already_complete: false,
                saved_path: saved_str,
                error: None,
            }
        }
        Err(e) => FileMsg::UploadAck {
            transfer_id,
            resume_offset: 0,
            already_complete: false,
            saved_path: saved_str,
            error: Some(format!("打开临时文件失败: {e}")),
        },
    }
}

async fn handle_upload_complete(
    transfer_id: Uuid,
    uploads: &mut HashMap<Uuid, ActiveUpload>,
) -> FileMsg {
    let Some(mut up) = uploads.remove(&transfer_id) else {
        return FileMsg::TransferResult {
            transfer_id,
            ok: false,
            saved_path: String::new(),
            error: Some("上传会话不存在".into()),
        };
    };

    if let Err(e) = up.file.flush().await {
        return FileMsg::TransferResult {
            transfer_id,
            ok: false,
            saved_path: String::new(),
            error: Some(format!("刷新文件缓冲失败: {e}")),
        };
    }
    drop(up.file);

    let part_clone = up.part_path.clone();
    let sha_res = tokio::task::spawn_blocking(move || compute_file_sha256(&part_clone)).await;
    match sha_res {
        Ok(Ok(actual)) if actual == up.expected_sha256 => {
            if up.final_path.exists() {
                let _ = std::fs::remove_file(&up.final_path);
            }
            if let Err(e) = std::fs::rename(&up.part_path, &up.final_path) {
                return FileMsg::TransferResult {
                    transfer_id,
                    ok: false,
                    saved_path: String::new(),
                    error: Some(format!("重命名目标文件失败: {e}")),
                };
            }
            let _ = std::fs::remove_file(&up.meta_path);
            FileMsg::TransferResult {
                transfer_id,
                ok: true,
                saved_path: up.final_path.to_string_lossy().into_owned(),
                error: None,
            }
        }
        Ok(Ok(_)) => FileMsg::TransferResult {
            transfer_id,
            ok: false,
            saved_path: String::new(),
            error: Some("文件完整 SHA-256 校验失败".into()),
        },
        Ok(Err(e)) => FileMsg::TransferResult {
            transfer_id,
            ok: false,
            saved_path: String::new(),
            error: Some(format!("校验计算失败: {e}")),
        },
        Err(e) => FileMsg::TransferResult {
            transfer_id,
            ok: false,
            saved_path: String::new(),
            error: Some(e.to_string()),
        },
    }
}

async fn stream_download_to_peer<W: AsyncWrite + Unpin>(
    writer: Arc<Mutex<NoiseWriter<W>>>,
    transfer_id: Uuid,
    remote_path: PathBuf,
    resume_offset: u64,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let file_name = remote_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download.bin".into());

    let meta = match tokio::fs::metadata(&remote_path).await {
        Ok(m) if m.is_file() => m,
        Ok(_) => {
            let _ = send_file_msg(
                &writer,
                &FileMsg::DownloadMeta {
                    transfer_id,
                    file_name,
                    total_size: 0,
                    file_sha256: [0u8; 32],
                    resume_offset: 0,
                    error: Some("目标不是普通文件".into()),
                },
            )
            .await;
            return Ok(());
        }
        Err(e) => {
            let _ = send_file_msg(
                &writer,
                &FileMsg::DownloadMeta {
                    transfer_id,
                    file_name,
                    total_size: 0,
                    file_sha256: [0u8; 32],
                    resume_offset: 0,
                    error: Some(format!("无法访问文件: {e}")),
                },
            )
            .await;
            return Ok(());
        }
    };

    let total_size = meta.len();
    let rp_clone = remote_path.clone();
    let file_sha256 = tokio::task::spawn_blocking(move || compute_file_sha256(&rp_clone)).await??;
    let valid_offset = resume_offset.min(total_size);

    send_file_msg(
        &writer,
        &FileMsg::DownloadMeta {
            transfer_id,
            file_name,
            total_size,
            file_sha256,
            resume_offset: valid_offset,
            error: None,
        },
    )
    .await?;

    let mut file = tokio::fs::File::open(&remote_path).await?;
    file.seek(SeekFrom::Start(valid_offset)).await?;

    let mut offset = valid_offset;
    let mut buf = vec![0u8; FILE_CHUNK_SIZE];

    while !cancel.load(Ordering::Relaxed) {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk_data = buf[..n].to_vec();
        let chunk_sha256 = compute_bytes_sha256(&chunk_data);
        send_file_msg(
            &writer,
            &FileMsg::FileChunk {
                transfer_id,
                offset,
                chunk_sha256,
                data: chunk_data,
            },
        )
        .await?;
        offset += n as u64;
    }

    if !cancel.load(Ordering::Relaxed) {
        send_file_msg(&writer, &FileMsg::DownloadComplete { transfer_id }).await?;
    }

    Ok(())
}
