use crate::net::connect_and_auth_relay;
use crate::net::direct::connect_direct_channel;
use crate::transfer::{
    compute_bytes_sha256, compute_file_sha256, part_and_meta_paths, ResumeMeta,
    TransferDirection, TransferRecord, TransferState,
};
use anyhow::{Context, Result};
use protocol::auth::{DeviceBundle, RelayNodeConfig};
use protocol::noise::{NoiseReader, NoiseStream, NoiseWriter};
use protocol::relay::{ChannelKind, ConnectionPurpose};
use protocol::session::ControlMsg;
use protocol::transfer::{FileEntry, FileMsg, FILE_CHUNK_SIZE};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, SeekFrom};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone)]
pub enum FileTransportMode {
    Relay {
        bundle: DeviceBundle,
        revoked_ids: Vec<Uuid>,
        peer_device_id: Uuid,
        fallback_relay: RelayNodeConfig,
    },
    Direct {
        bundle: DeviceBundle,
        revoked_ids: Vec<Uuid>,
        direct_addr: String,
    },
}

pub enum FileCommand {
    ListRemoteDir(String),
    UploadFile {
        local_path: PathBuf,
        remote_dir: String,
    },
    DownloadFile {
        remote_path: String,
        local_dir: PathBuf,
    },
    SetRelay(Option<RelayNodeConfig>),
    Cancel(Uuid),
    Shutdown,
}

#[derive(Clone)]
pub struct FileClientHandle {
    pub cmd_tx: mpsc::UnboundedSender<FileCommand>,
    pub remote_dir: Arc<Mutex<String>>,
    pub remote_entries: Arc<Mutex<Vec<FileEntry>>>,
    pub remote_roots: Arc<Mutex<Vec<FileEntry>>>,
    pub remote_error: Arc<Mutex<Option<String>>>,
    pub remote_loading: Arc<AtomicBool>,
    pub transfers: Arc<Mutex<Vec<TransferRecord>>>,
    pub active_via_label: Arc<Mutex<String>>,
    pub cancel_flags: Arc<Mutex<std::collections::HashMap<Uuid, Arc<AtomicBool>>>>,
}

impl FileClientHandle {
    pub fn send(&self, cmd: FileCommand) {
        if let FileCommand::Cancel(id) = &cmd {
            if let Some(flag) = self.cancel_flags.lock().unwrap().get(id) {
                flag.store(true, Ordering::Relaxed);
            }
        }
        let _ = self.cmd_tx.send(cmd);
    }
}

pub fn spawn_file_client(
    mode: FileTransportMode,
    initial_file_relay: Option<RelayNodeConfig>,
    ctrl_out_tx: mpsc::Sender<ControlMsg>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> FileClientHandle {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<FileCommand>();
    let remote_dir = Arc::new(Mutex::new(String::new()));
    let remote_entries = Arc::new(Mutex::new(Vec::new()));
    let remote_roots = Arc::new(Mutex::new(Vec::new()));
    let remote_error = Arc::new(Mutex::new(None));
    let remote_loading = Arc::new(AtomicBool::new(false));
    let transfers = Arc::new(Mutex::new(Vec::new()));
    let cancel_flags = Arc::new(Mutex::new(std::collections::HashMap::new()));

    let init_label = compute_via_label(&mode, &initial_file_relay);
    let active_via_label = Arc::new(Mutex::new(init_label));

    let handle = FileClientHandle {
        cmd_tx,
        remote_dir: Arc::clone(&remote_dir),
        remote_entries: Arc::clone(&remote_entries),
        remote_roots: Arc::clone(&remote_roots),
        remote_error: Arc::clone(&remote_error),
        remote_loading: Arc::clone(&remote_loading),
        transfers: Arc::clone(&transfers),
        active_via_label: Arc::clone(&active_via_label),
        cancel_flags: Arc::clone(&cancel_flags),
    };

    tokio::spawn(async move {
        let mut selected_relay = initial_file_relay;
        let mut conn: Option<(
            NoiseReader<tokio::io::ReadHalf<TcpStream>>,
            NoiseWriter<tokio::io::WriteHalf<TcpStream>>,
        )> = None;

        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                FileCommand::Shutdown => break,
                FileCommand::SetRelay(new_relay) => {
                    selected_relay = new_relay;
                    *active_via_label.lock().unwrap() = compute_via_label(&mode, &selected_relay);
                    conn = None; // Drop old tunnel so next operation opens on new relay node
                    repaint();
                }
                FileCommand::Cancel(tid) => {
                    if let Some((_, w)) = &mut conn {
                        if let Ok(bytes) = bincode::serialize(&FileMsg::CancelTransfer { transfer_id: tid }) {
                            let _ = w.send_msg(&bytes).await;
                        }
                    }
                    update_record_state(&transfers, tid, TransferState::Cancelled);
                    repaint();
                }
                FileCommand::ListRemoteDir(path) => {
                    remote_loading.store(true, Ordering::Relaxed);
                    *remote_error.lock().unwrap() = None;
                    repaint();

                    if conn.is_none() {
                        match open_file_tunnel(&mode, &selected_relay, &ctrl_out_tx).await {
                            Ok(ns) => {
                                let (r, w, _) = ns.into_split();
                                conn = Some((r, w));
                            }
                            Err(e) => {
                                *remote_error.lock().unwrap() = Some(format!("连接文件通道失败: {e}"));
                                remote_loading.store(false, Ordering::Relaxed);
                                repaint();
                                continue;
                            }
                        }
                    }

                    let (r, w) = conn.as_mut().unwrap();
                    let req = FileMsg::ListDirReq { path: path.clone() };
                    let res = async {
                        w.send_msg(&bincode::serialize(&req)?).await?;
                        let raw = r.recv_msg().await?;
                        let resp: FileMsg = bincode::deserialize(&raw)?;
                        Ok::<FileMsg, anyhow::Error>(resp)
                    }
                    .await;

                    match res {
                        Ok(FileMsg::ListDirResp {
                            path: p,
                            entries,
                            error,
                        }) => {
                            if p.trim().is_empty() && error.is_none() {
                                *remote_roots.lock().unwrap() = entries.clone();
                            }
                            *remote_dir.lock().unwrap() = p;
                            *remote_entries.lock().unwrap() = entries;
                            *remote_error.lock().unwrap() = error;
                        }
                        Ok(other) => {
                            *remote_error.lock().unwrap() =
                                Some(format!("意外的文件目录响应: {other:?}"));
                        }
                        Err(e) => {
                            conn = None;
                            *remote_error.lock().unwrap() = Some(format!("目录读取失败: {e}"));
                        }
                    }
                    remote_loading.store(false, Ordering::Relaxed);
                    repaint();
                }
                FileCommand::UploadFile {
                    local_path,
                    remote_dir: rdir,
                } => {
                    let via = active_via_label.lock().unwrap().clone();
                    let tid = Uuid::new_v4();
                    let cancel = Arc::new(AtomicBool::new(false));
                    cancel_flags.lock().unwrap().insert(tid, Arc::clone(&cancel));

                    if conn.is_none() {
                        match open_file_tunnel(&mode, &selected_relay, &ctrl_out_tx).await {
                            Ok(ns) => {
                                let (r, w, _) = ns.into_split();
                                conn = Some((r, w));
                            }
                            Err(e) => {
                                push_failed_record(
                                    &transfers,
                                    tid,
                                    &local_path,
                                    &rdir,
                                    TransferDirection::Upload,
                                    &via,
                                    format!("打开文件通道失败: {e}"),
                                );
                                cancel_flags.lock().unwrap().remove(&tid);
                                repaint();
                                continue;
                            }
                        }
                    }

                    let (r, w) = conn.as_mut().unwrap();
                    let repaint_cb = || repaint();
                    if let Err(e) = run_upload_over_stream(
                        r,
                        w,
                        tid,
                        &local_path,
                        &rdir,
                        &via,
                        &transfers,
                        &cancel,
                        &repaint_cb,
                    )
                    .await
                    {
                        conn = None;
                        update_record_state(&transfers, tid, TransferState::Failed(e.to_string()));
                    }
                    cancel_flags.lock().unwrap().remove(&tid);
                    repaint();
                }
                FileCommand::DownloadFile {
                    remote_path,
                    local_dir,
                } => {
                    let via = active_via_label.lock().unwrap().clone();
                    let tid = Uuid::new_v4();
                    let cancel = Arc::new(AtomicBool::new(false));
                    cancel_flags.lock().unwrap().insert(tid, Arc::clone(&cancel));

                    if conn.is_none() {
                        match open_file_tunnel(&mode, &selected_relay, &ctrl_out_tx).await {
                            Ok(ns) => {
                                let (r, w, _) = ns.into_split();
                                conn = Some((r, w));
                            }
                            Err(e) => {
                                push_failed_record(
                                    &transfers,
                                    tid,
                                    &local_dir,
                                    &remote_path,
                                    TransferDirection::Download,
                                    &via,
                                    format!("打开文件通道失败: {e}"),
                                );
                                cancel_flags.lock().unwrap().remove(&tid);
                                repaint();
                                continue;
                            }
                        }
                    }

                    let (r, w) = conn.as_mut().unwrap();
                    let repaint_cb = || repaint();
                    if let Err(e) = run_download_over_stream(
                        r,
                        w,
                        tid,
                        &remote_path,
                        &local_dir,
                        &via,
                        &transfers,
                        &cancel,
                        &repaint_cb,
                    )
                    .await
                    {
                        conn = None;
                        update_record_state(&transfers, tid, TransferState::Failed(e.to_string()));
                    }
                    cancel_flags.lock().unwrap().remove(&tid);
                    repaint();
                }
            }
        }
    });

    handle
}

fn compute_via_label(mode: &FileTransportMode, selected: &Option<RelayNodeConfig>) -> String {
    match mode {
        FileTransportMode::Direct { direct_addr, .. } => format!("直连 ({direct_addr})"),
        FileTransportMode::Relay { fallback_relay, .. } => {
            let n = selected.as_ref().unwrap_or(fallback_relay);
            format!("{} [{}]", n.name, n.addr)
        }
    }
}

async fn open_file_tunnel(
    mode: &FileTransportMode,
    selected_relay: &Option<RelayNodeConfig>,
    ctrl_out_tx: &mpsc::Sender<ControlMsg>,
) -> Result<NoiseStream<TcpStream>> {
    let transfer_session_id = Uuid::new_v4();
    match mode {
        FileTransportMode::Relay {
            bundle,
            revoked_ids,
            peer_device_id,
            fallback_relay,
        } => {
            let node = selected_relay.as_ref().unwrap_or(fallback_relay);
            ctrl_out_tx
                .send(ControlMsg::OpenFileChannel {
                    transfer_session_id,
                    relay_addr: Some(node.addr.clone()),
                })
                .await
                .context("控制通道已关闭")?;

            let tcp = connect_and_auth_relay(
                &node.addr,
                &bundle.identity,
                ConnectionPurpose::PairTunnel {
                    session_id: transfer_session_id,
                    channel: ChannelKind::File,
                    peer_device_id: *peer_device_id,
                },
            )
            .await?;

            Ok(NoiseStream::handshake_initiator(
                tcp,
                &bundle.root_pubkey,
                &bundle.identity,
                revoked_ids,
            )
            .await?)
        }
        FileTransportMode::Direct {
            bundle,
            revoked_ids,
            direct_addr,
        } => {
            ctrl_out_tx
                .send(ControlMsg::OpenFileChannel {
                    transfer_session_id,
                    relay_addr: None,
                })
                .await
                .context("控制通道已关闭")?;

            let tcp =
                connect_direct_channel(direct_addr, transfer_session_id, ChannelKind::File).await?;
            Ok(NoiseStream::handshake_initiator(
                tcp,
                &bundle.root_pubkey,
                &bundle.identity,
                revoked_ids,
            )
            .await?)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_upload_over_stream<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut NoiseReader<R>,
    writer: &mut NoiseWriter<W>,
    transfer_id: Uuid,
    local_path: &Path,
    remote_dir: &str,
    via_label: &str,
    transfers: &Arc<Mutex<Vec<TransferRecord>>>,
    cancel: &Arc<AtomicBool>,
    repaint: &(dyn Fn() + Send + Sync),
) -> Result<TransferRecord> {
    let file_name = local_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file.bin".into());
    let meta = tokio::fs::metadata(local_path).await?;
    anyhow::ensure!(meta.is_file(), "只支持传输普通文件: {}", local_path.display());
    let total_size = meta.len();

    upsert_record(
        transfers,
        TransferRecord {
            transfer_id,
            file_name: file_name.clone(),
            local_path: local_path.to_string_lossy().into_owned(),
            remote_path: remote_dir.to_string(),
            direction: TransferDirection::Upload,
            total_bytes: total_size,
            transferred_bytes: 0,
            speed_mbps: 0.0,
            via_label: via_label.to_string(),
            resumed: false,
            sha256_verified: false,
            state: TransferState::Queued,
        },
    );
    repaint();

    let lp_clone = local_path.to_path_buf();
    let file_sha256 = tokio::task::spawn_blocking(move || compute_file_sha256(&lp_clone)).await??;

    let init = FileMsg::UploadInit {
        transfer_id,
        remote_dir: remote_dir.to_string(),
        file_name,
        total_size,
        file_sha256,
    };
    writer.send_msg(&bincode::serialize(&init)?).await?;

    let ack: FileMsg = bincode::deserialize(&reader.recv_msg().await?)?;
    let (resume_offset, already_complete, saved_path) = match ack {
        FileMsg::UploadAck {
            resume_offset,
            already_complete,
            saved_path,
            error,
            ..
        } => {
            if let Some(err) = error {
                anyhow::bail!("{err}");
            }
            (resume_offset, already_complete, saved_path)
        }
        other => anyhow::bail!("expected UploadAck, got {other:?}"),
    };

    if already_complete {
        let mut done = get_record(transfers, transfer_id).unwrap();
        done.transferred_bytes = total_size;
        done.remote_path = saved_path;
        done.resumed = true;
        done.sha256_verified = true;
        done.state = TransferState::Done;
        upsert_record(transfers, done.clone());
        repaint();
        return Ok(done);
    }

    let resumed = resume_offset > 0;
    {
        let mut guard = transfers.lock().unwrap();
        if let Some(r) = guard.iter_mut().find(|r| r.transfer_id == transfer_id) {
            r.remote_path = saved_path;
            r.resumed = resumed;
            r.transferred_bytes = resume_offset;
            r.state = TransferState::Active;
        }
    }
    repaint();

    let mut file = tokio::fs::File::open(local_path).await?;
    file.seek(SeekFrom::Start(resume_offset)).await?;

    let mut offset = resume_offset;
    let mut sent_since_start = 0u64;
    let start_time = Instant::now();
    let mut last_ui_tick = Instant::now();
    let mut buf = vec![0u8; FILE_CHUNK_SIZE];

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = writer
                .send_msg(&bincode::serialize(&FileMsg::CancelTransfer { transfer_id })?)
                .await;
            update_record_state(transfers, transfer_id, TransferState::Cancelled);
            repaint();
            return Ok(get_record(transfers, transfer_id).unwrap());
        }
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk_data = buf[..n].to_vec();
        let chunk_sha256 = compute_bytes_sha256(&chunk_data);
        let msg = FileMsg::FileChunk {
            transfer_id,
            offset,
            chunk_sha256,
            data: chunk_data,
        };
        writer.send_msg(&bincode::serialize(&msg)?).await?;

        offset += n as u64;
        sent_since_start += n as u64;
        if last_ui_tick.elapsed().as_millis() >= 100 || offset == total_size {
            let secs = start_time.elapsed().as_secs_f32().max(0.001);
            let mbps = (sent_since_start as f32 * 8.0) / (secs * 1_000_000.0);
            update_record_progress(transfers, transfer_id, offset, mbps);
            repaint();
            last_ui_tick = Instant::now();
        }
    }

    writer
        .send_msg(&bincode::serialize(&FileMsg::UploadComplete { transfer_id })?)
        .await?;

    let res: FileMsg = bincode::deserialize(&reader.recv_msg().await?)?;
    match res {
        FileMsg::TransferResult {
            ok,
            saved_path,
            error,
            ..
        } => {
            if !ok {
                anyhow::bail!("{}", error.unwrap_or_else(|| "未知上传错误".into()));
            }
            let mut guard = transfers.lock().unwrap();
            let rec = guard
                .iter_mut()
                .find(|r| r.transfer_id == transfer_id)
                .unwrap();
            rec.remote_path = saved_path;
            rec.transferred_bytes = total_size;
            rec.sha256_verified = true;
            rec.state = TransferState::Done;
            let out = rec.clone();
            drop(guard);
            repaint();
            Ok(out)
        }
        other => anyhow::bail!("expected TransferResult, got {other:?}"),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_download_over_stream<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut NoiseReader<R>,
    writer: &mut NoiseWriter<W>,
    transfer_id: Uuid,
    remote_path: &str,
    local_dir: &Path,
    via_label: &str,
    transfers: &Arc<Mutex<Vec<TransferRecord>>>,
    cancel: &Arc<AtomicBool>,
    repaint: &(dyn Fn() + Send + Sync),
) -> Result<TransferRecord> {
    std::fs::create_dir_all(local_dir)?;
    let remote_name = Path::new(remote_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download.bin".into());

    let local_final = local_dir.join(&remote_name);
    let (part_path, meta_path) = part_and_meta_paths(&local_final);
    let mut initial_resume = 0u64;
    if part_path.exists() && meta_path.exists() {
        if let Ok(part_meta) = std::fs::metadata(&part_path) {
            initial_resume = part_meta.len();
        }
    }

    upsert_record(
        transfers,
        TransferRecord {
            transfer_id,
            file_name: remote_name.clone(),
            local_path: local_final.to_string_lossy().into_owned(),
            remote_path: remote_path.to_string(),
            direction: TransferDirection::Download,
            total_bytes: 0,
            transferred_bytes: initial_resume,
            speed_mbps: 0.0,
            via_label: via_label.to_string(),
            resumed: initial_resume > 0,
            sha256_verified: false,
            state: TransferState::Queued,
        },
    );
    repaint();

    let req = FileMsg::DownloadReq {
        transfer_id,
        remote_path: remote_path.to_string(),
        resume_offset: initial_resume,
    };
    writer.send_msg(&bincode::serialize(&req)?).await?;

    let meta_msg: FileMsg = bincode::deserialize(&reader.recv_msg().await?)?;
    let (total_size, expected_sha256, valid_offset) = match meta_msg {
        FileMsg::DownloadMeta {
            total_size,
            file_sha256,
            resume_offset,
            error,
            ..
        } => {
            if let Some(err) = error {
                anyhow::bail!("{err}");
            }
            (total_size, file_sha256, resume_offset)
        }
        other => anyhow::bail!("expected DownloadMeta, got {other:?}"),
    };

    // Validate local resume meta matches remote SHA-256; if not, restart cleanly from 0
    let mut actual_resume = 0u64;
    if valid_offset > 0 && part_path.exists() && meta_path.exists() {
        if let Ok(bytes) = std::fs::read(&meta_path) {
            if let Ok(rm) = serde_json::from_slice::<ResumeMeta>(&bytes) {
                if rm.total_size == total_size && rm.file_sha256 == expected_sha256 {
                    actual_resume = valid_offset;
                }
            }
        }
    }

    if actual_resume == 0 && valid_offset > 0 {
        // Remote started from valid_offset, cancel & re-request from 0 cleanly
        let _ = writer
            .send_msg(&bincode::serialize(&FileMsg::CancelTransfer { transfer_id })?)
            .await;
        anyhow::bail!("本地断点元数据不匹配，请重试以从头下载");
    }

    let mut out_file = if actual_resume > 0 {
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&part_path)
            .await?
    } else {
        let rm = ResumeMeta {
            total_size,
            file_sha256: expected_sha256,
        };
        std::fs::write(&meta_path, serde_json::to_vec(&rm)?)?;
        tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&part_path)
            .await?
    };
    out_file.seek(SeekFrom::Start(actual_resume)).await?;

    {
        let mut guard = transfers.lock().unwrap();
        if let Some(r) = guard.iter_mut().find(|r| r.transfer_id == transfer_id) {
            r.total_bytes = total_size;
            r.transferred_bytes = actual_resume;
            r.resumed = actual_resume > 0;
            r.state = TransferState::Active;
        }
    }
    repaint();

    let start_time = Instant::now();
    let mut last_ui_tick = Instant::now();
    let mut received_since_start = 0u64;

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = writer
                .send_msg(&bincode::serialize(&FileMsg::CancelTransfer { transfer_id })?)
                .await;
            update_record_state(transfers, transfer_id, TransferState::Cancelled);
            repaint();
            return Ok(get_record(transfers, transfer_id).unwrap());
        }

        let msg: FileMsg = bincode::deserialize(&reader.recv_msg().await?)?;
        match msg {
            FileMsg::FileChunk {
                offset,
                chunk_sha256,
                data,
                ..
            } => {
                anyhow::ensure!(
                    compute_bytes_sha256(&data) == chunk_sha256,
                    "下载分片 SHA-256 校验失败"
                );
                out_file.seek(SeekFrom::Start(offset)).await?;
                out_file.write_all(&data).await?;
                let current_offset = offset + data.len() as u64;
                received_since_start += data.len() as u64;

                if last_ui_tick.elapsed().as_millis() >= 100 || current_offset == total_size {
                    let secs = start_time.elapsed().as_secs_f32().max(0.001);
                    let mbps = (received_since_start as f32 * 8.0) / (secs * 1_000_000.0);
                    update_record_progress(transfers, transfer_id, current_offset, mbps);
                    repaint();
                    last_ui_tick = Instant::now();
                }
            }
            FileMsg::DownloadComplete { .. } => {
                break;
            }
            other => anyhow::bail!("unexpected download msg: {other:?}"),
        }
    }

    out_file.flush().await?;
    drop(out_file);

    let part_clone = part_path.clone();
    let actual_sha = tokio::task::spawn_blocking(move || compute_file_sha256(&part_clone)).await??;
    anyhow::ensure!(actual_sha == expected_sha256, "下载文件完整 SHA-256 校验失败");

    if local_final.exists() {
        let _ = std::fs::remove_file(&local_final);
    }
    std::fs::rename(&part_path, &local_final)?;
    let _ = std::fs::remove_file(&meta_path);

    let mut guard = transfers.lock().unwrap();
    let rec = guard
        .iter_mut()
        .find(|r| r.transfer_id == transfer_id)
        .unwrap();
    rec.transferred_bytes = total_size;
    rec.sha256_verified = true;
    rec.state = TransferState::Done;
    let out = rec.clone();
    drop(guard);
    repaint();
    Ok(out)
}

fn upsert_record(list: &Arc<Mutex<Vec<TransferRecord>>>, rec: TransferRecord) {
    let mut guard = list.lock().unwrap();
    if let Some(pos) = guard.iter().position(|r| r.transfer_id == rec.transfer_id) {
        guard[pos] = rec;
    } else {
        guard.insert(0, rec);
    }
}

fn get_record(list: &Arc<Mutex<Vec<TransferRecord>>>, id: Uuid) -> Option<TransferRecord> {
    list.lock()
        .unwrap()
        .iter()
        .find(|r| r.transfer_id == id)
        .cloned()
}

fn update_record_progress(
    list: &Arc<Mutex<Vec<TransferRecord>>>,
    id: Uuid,
    bytes: u64,
    mbps: f32,
) {
    let mut guard = list.lock().unwrap();
    if let Some(r) = guard.iter_mut().find(|r| r.transfer_id == id) {
        r.transferred_bytes = bytes;
        r.speed_mbps = mbps;
    }
}

fn update_record_state(list: &Arc<Mutex<Vec<TransferRecord>>>, id: Uuid, state: TransferState) {
    let mut guard = list.lock().unwrap();
    if let Some(r) = guard.iter_mut().find(|r| r.transfer_id == id) {
        r.state = state;
    }
}

fn push_failed_record(
    list: &Arc<Mutex<Vec<TransferRecord>>>,
    tid: Uuid,
    local_path: &Path,
    remote_path: &str,
    direction: TransferDirection,
    via_label: &str,
    err: String,
) {
    let file_name = local_path
        .file_name()
        .or_else(|| Path::new(remote_path).file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    upsert_record(
        list,
        TransferRecord {
            transfer_id: tid,
            file_name,
            local_path: local_path.to_string_lossy().into_owned(),
            remote_path: remote_path.to_string(),
            direction,
            total_bytes: 0,
            transferred_bytes: 0,
            speed_mbps: 0.0,
            via_label: via_label.to_string(),
            resumed: false,
            sha256_verified: false,
            state: TransferState::Failed(err),
        },
    );
}
