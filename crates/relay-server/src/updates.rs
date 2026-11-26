use crate::server::send_auth_ok;
use crate::state::RelayState;
use protocol::framing::{read_frame, write_frame};
use protocol::update::{
    is_safe_artifact_name, is_valid_sha256_hex, PublishFileSpec, SignedUpdateManifest,
    UpdateClientMsg, UpdateServerMsg, MAX_UPDATE_ARTIFACT_SIZE,
};
use protocol::{unix_timestamp_secs, DeviceCert};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};
use tracing::{info, warn};
use uuid::Uuid;

const UPDATE_IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DOWNLOAD_CHUNK: usize = 512 * 1024;
const MAX_BATCH_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;

pub struct UpdateStore {
    updates_dir: Option<PathBuf>,
    manifest: RwLock<Option<SignedUpdateManifest>>,
    publish_lock: Arc<Mutex<()>>,
}

impl UpdateStore {
    pub fn new(state_dir: Option<&Path>, root_pubkey: &[u8; 32]) -> Self {
        let updates_dir = state_dir.map(|d| d.join("updates"));
        let mut initial_manifest = None;
        if let Some(dir) = &updates_dir {
            if let Err(e) = std::fs::create_dir_all(dir) {
                warn!("failed to create updates dir {}: {e}", dir.display());
            } else {
                cleanup_stale_incoming(dir);
                let manifest_path = dir.join("manifest.json");
                if let Ok(text) = std::fs::read_to_string(&manifest_path) {
                    match serde_json::from_str::<SignedUpdateManifest>(&text) {
                        Ok(signed) => match signed.verify(root_pubkey, unix_timestamp_secs()) {
                            Ok(()) => {
                                info!(
                                    "loaded signed update manifest v{} from {}",
                                    signed.manifest.version,
                                    manifest_path.display()
                                );
                                initial_manifest = Some(signed);
                            }
                            Err(e) => {
                                warn!("ignoring invalid manifest {}: {e}", manifest_path.display());
                            }
                        },
                        Err(e) => {
                            warn!("failed to parse {}: {e}", manifest_path.display());
                        }
                    }
                }
            }
        }
        Self {
            updates_dir,
            manifest: RwLock::new(initial_manifest),
            publish_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn updates_dir(&self) -> Option<&Path> {
        self.updates_dir.as_deref()
    }
}

fn cleanup_stale_incoming(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with(".incoming-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

struct PendingFileState {
    spec: PublishFileSpec,
    received: u64,
    eof_seen: bool,
    hasher: Sha256,
    file: Option<tokio::fs::File>,
}

struct PendingPublish {
    manifest: SignedUpdateManifest,
    files: HashMap<String, PendingFileState>,
    staging_dir: PathBuf,
    _guard: OwnedMutexGuard<()>,
}

impl Drop for PendingPublish {
    fn drop(&mut self) {
        for st in self.files.values_mut() {
            st.file.take();
        }
        let _ = std::fs::remove_dir_all(&self.staging_dir);
    }
}

async fn send_msg(stream: &mut TcpStream, msg: &UpdateServerMsg) -> anyhow::Result<()> {
    let bytes = bincode::serialize(msg)?;
    tokio::time::timeout(UPDATE_IO_TIMEOUT, write_frame(stream, &bytes))
        .await
        .map_err(|_| anyhow::anyhow!("update write timeout"))??;
    Ok(())
}

pub async fn handle_update_channel(
    mut stream: TcpStream,
    cert: DeviceCert,
    state: Arc<RelayState>,
) -> anyhow::Result<()> {
    send_auth_ok(&mut stream).await?;
    let mut pending: Option<PendingPublish> = None;

    loop {
        let raw = match tokio::time::timeout(UPDATE_IO_TIMEOUT, read_frame(&mut stream)).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(_)) => break,
            Err(_) => break,
        };
        let req: UpdateClientMsg = match bincode::deserialize(&raw) {
            Ok(m) => m,
            Err(e) => {
                let _ = send_msg(&mut stream, &UpdateServerMsg::Error(format!("invalid update msg: {e}"))).await;
                break;
            }
        };

        match req {
            UpdateClientMsg::GetManifest => {
                let m = state.updates.manifest.read().await.clone();
                send_msg(&mut stream, &UpdateServerMsg::Manifest(m)).await?;
            }
            UpdateClientMsg::DownloadChunk {
                filename,
                offset,
                max_len,
            } => {
                let resp = handle_download_chunk(&state, &filename, offset, max_len).await;
                send_msg(&mut stream, &resp).await?;
            }
            UpdateClientMsg::BeginPublish { manifest, files } => {
                match handle_begin_publish(&state, &cert, &mut pending, manifest, files).await {
                    Ok(()) => send_msg(&mut stream, &UpdateServerMsg::PublishReady).await?,
                    Err(e) => {
                        warn!(
                            "rejected BeginPublish from {} ({}): {e}",
                            cert.device_name, cert.device_id
                        );
                        send_msg(&mut stream, &UpdateServerMsg::Error(e.to_string())).await?;
                    }
                }
            }
            UpdateClientMsg::UploadChunk {
                filename,
                offset,
                data,
                eof,
            } => {
                match handle_upload_chunk(&mut pending, &filename, offset, &data, eof).await {
                    Ok(received) => {
                        send_msg(
                            &mut stream,
                            &UpdateServerMsg::UploadAck { filename, received },
                        )
                        .await?;
                    }
                    Err(e) => {
                        pending = None;
                        send_msg(&mut stream, &UpdateServerMsg::Error(e.to_string())).await?;
                    }
                }
            }
            UpdateClientMsg::CommitPublish => {
                let committed = handle_commit_publish(&state, &cert, &mut pending).await;
                match committed {
                    Ok((version, files)) => {
                        send_msg(
                            &mut stream,
                            &UpdateServerMsg::PublishCommitted { version, files },
                        )
                        .await?;
                    }
                    Err(e) => {
                        pending = None;
                        send_msg(&mut stream, &UpdateServerMsg::Error(e.to_string())).await?;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn handle_download_chunk(
    state: &RelayState,
    filename: &str,
    offset: u64,
    max_len: u32,
) -> UpdateServerMsg {
    if !is_safe_artifact_name(filename) {
        return UpdateServerMsg::Error("invalid artifact filename".into());
    }
    let Some(dir) = state.updates.updates_dir() else {
        return UpdateServerMsg::Error("relay server updates store is not enabled".into());
    };

    let allowed = {
        let guard = state.updates.manifest.read().await;
        guard
            .as_ref()
            .is_some_and(|m| m.manifest.referenced_filenames().contains(&filename))
    };
    if !allowed {
        return UpdateServerMsg::Error(format!("artifact '{filename}' is not in active manifest"));
    }

    let path = dir.join(filename);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return UpdateServerMsg::Error(format!("failed to open artifact: {e}")),
    };
    let total_size = match file.metadata().await {
        Ok(md) => md.len(),
        Err(e) => return UpdateServerMsg::Error(format!("failed to stat artifact: {e}")),
    };
    if offset > total_size {
        return UpdateServerMsg::Error("download offset beyond EOF".into());
    }
    if offset > 0 {
        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            return UpdateServerMsg::Error(format!("seek failed: {e}"));
        }
    }

    let want = (max_len as usize).clamp(1, MAX_DOWNLOAD_CHUNK);
    let mut buf = vec![0u8; want];
    let mut filled = 0usize;
    while filled < want {
        match file.read(&mut buf[filled..]).await {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return UpdateServerMsg::Error(format!("read failed: {e}")),
        }
    }
    buf.truncate(filled);
    let eof = offset.saturating_add(filled as u64) >= total_size;
    UpdateServerMsg::ChunkData {
        offset,
        data: buf,
        total_size,
        eof,
    }
}

async fn handle_begin_publish(
    state: &RelayState,
    cert: &DeviceCert,
    pending: &mut Option<PendingPublish>,
    manifest: SignedUpdateManifest,
    files: Vec<PublishFileSpec>,
) -> anyhow::Result<()> {
    let Some(dir) = state.updates.updates_dir() else {
        anyhow::bail!("relay server started without --state-dir; update storage disabled");
    };
    if pending.is_some() {
        anyhow::bail!("publish transaction already in progress on this connection");
    }

    manifest.verify(&state.root_pubkey, unix_timestamp_secs())?;

    let guard = state
        .updates
        .publish_lock
        .clone()
        .try_lock_owned()
        .map_err(|_| anyhow::anyhow!("another update publish is currently in progress"))?;

    if let Some(current) = state.updates.manifest.read().await.as_ref() {
        if manifest.manifest.released_at < current.manifest.released_at {
            anyhow::bail!(
                "stale update manifest (released_at {} < {})",
                manifest.manifest.released_at,
                current.manifest.released_at
            );
        }
    }

    let expected_names: HashSet<&str> = manifest
        .manifest
        .referenced_filenames()
        .into_iter()
        .collect();
    if files.len() != expected_names.len() {
        anyhow::bail!(
            "file spec count {} does not match manifest artifacts {}",
            files.len(),
            expected_names.len()
        );
    }

    let mut total_bytes = 0u64;
    let mut file_map = HashMap::with_capacity(files.len());
    for spec in files {
        if !is_safe_artifact_name(&spec.filename) || !is_valid_sha256_hex(&spec.sha256) {
            anyhow::bail!("invalid publish file spec '{}'", spec.filename);
        }
        if !expected_names.contains(spec.filename.as_str()) {
            anyhow::bail!("unexpected artifact '{}' not in manifest", spec.filename);
        }
        if spec.size == 0 || spec.size > MAX_UPDATE_ARTIFACT_SIZE {
            anyhow::bail!("artifact '{}' size {} out of range", spec.filename, spec.size);
        }
        // Cross-check declared spec against manifest metadata.
        let manifest_matches = if spec.filename == manifest.manifest.full.filename {
            spec.size == manifest.manifest.full.size && spec.sha256 == manifest.manifest.full.sha256
        } else {
            manifest
                .manifest
                .deltas
                .iter()
                .any(|d| d.filename == spec.filename && d.size == spec.size && d.sha256 == spec.sha256)
        };
        if !manifest_matches {
            anyhow::bail!(
                "artifact '{}' size/sha256 does not match signed manifest",
                spec.filename
            );
        }

        total_bytes = total_bytes.saturating_add(spec.size);
        if total_bytes > MAX_BATCH_UPLOAD_BYTES {
            anyhow::bail!("total publish size exceeds {} bytes", MAX_BATCH_UPLOAD_BYTES);
        }
        if file_map.contains_key(&spec.filename) {
            anyhow::bail!("duplicate artifact '{}'", spec.filename);
        }
        file_map.insert(
            spec.filename.clone(),
            PendingFileState {
                spec,
                received: 0,
                eof_seen: false,
                hasher: Sha256::new(),
                file: None,
            },
        );
    }

    let staging_dir = dir.join(format!(".incoming-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&staging_dir).await?;
    info!(
        "accepted BeginPublish v{} ({} files, {} bytes) from {} ({})",
        manifest.manifest.version,
        file_map.len(),
        total_bytes,
        cert.device_name,
        cert.device_id
    );

    *pending = Some(PendingPublish {
        manifest,
        files: file_map,
        staging_dir,
        _guard: guard,
    });
    Ok(())
}

async fn handle_upload_chunk(
    pending: &mut Option<PendingPublish>,
    filename: &str,
    offset: u64,
    data: &[u8],
    eof: bool,
) -> anyhow::Result<u64> {
    let Some(tx) = pending.as_mut() else {
        anyhow::bail!("no active publish transaction");
    };
    let Some(st) = tx.files.get_mut(filename) else {
        anyhow::bail!("unknown artifact '{filename}'");
    };
    if offset != st.received {
        anyhow::bail!(
            "non-sequential upload offset for '{filename}': got {offset}, expected {}",
            st.received
        );
    }
    let next_received = offset.saturating_add(data.len() as u64);
    if next_received > st.spec.size {
        anyhow::bail!("upload exceeds declared size for '{filename}'");
    }
    if st.file.is_none() {
        let path = tx.staging_dir.join(filename);
        st.file = Some(tokio::fs::File::create(&path).await?);
    }
    if !data.is_empty() {
        let f = st.file.as_mut().unwrap();
        f.write_all(data).await?;
        st.hasher.update(data);
    }
    st.received = next_received;
    if eof {
        if st.received != st.spec.size {
            anyhow::bail!(
                "premature EOF for '{filename}': received {} of {} bytes",
                st.received,
                st.spec.size
            );
        }
        if let Some(f) = st.file.as_mut() {
            f.flush().await?;
        }
        st.eof_seen = true;
    }
    Ok(st.received)
}

async fn handle_commit_publish(
    state: &RelayState,
    cert: &DeviceCert,
    pending: &mut Option<PendingPublish>,
) -> anyhow::Result<(String, usize)> {
    let Some(mut tx) = pending.take() else {
        anyhow::bail!("no active publish transaction");
    };
    let Some(updates_dir) = state.updates.updates_dir() else {
        anyhow::bail!("updates directory unavailable");
    };

    for (name, st) in &mut tx.files {
        if let Some(mut f) = st.file.take() {
            f.flush().await?;
        }
        if st.received != st.spec.size || !st.eof_seen {
            anyhow::bail!("artifact '{name}' upload incomplete");
        }
        let got_sha = hex::encode(st.hasher.clone().finalize());
        if got_sha != st.spec.sha256 {
            anyhow::bail!(
                "artifact '{name}' SHA-256 mismatch: got {got_sha}, expected {}",
                st.spec.sha256
            );
        }
    }

    tokio::fs::create_dir_all(updates_dir).await?;
    for name in tx.files.keys() {
        let src = tx.staging_dir.join(name);
        let dst = updates_dir.join(name);
        if tokio::fs::rename(&src, &dst).await.is_err() {
            tokio::fs::copy(&src, &dst).await?;
            let _ = tokio::fs::remove_file(&src).await;
        }
    }

    let manifest_tmp = updates_dir.join("manifest.json.tmp");
    let manifest_dst = updates_dir.join("manifest.json");
    tokio::fs::write(&manifest_tmp, serde_json::to_vec_pretty(&tx.manifest)?).await?;
    tokio::fs::rename(&manifest_tmp, &manifest_dst).await?;

    let keep: HashSet<String> = tx
        .manifest
        .manifest
        .referenced_filenames()
        .into_iter()
        .map(str::to_string)
        .collect();
    let version = tx.manifest.manifest.version.clone();
    let file_count = tx.files.len();

    *state.updates.manifest.write().await = Some(tx.manifest.clone());

    // Prune artifacts not referenced by the newly committed manifest.
    if let Ok(mut rd) = tokio::fs::read_dir(updates_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname == "manifest.json" || fname.starts_with(".incoming-") || keep.contains(&fname) {
                continue;
            }
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }

    info!(
        "committed update release v{} ({} artifacts) by {} ({})",
        version, file_count, cert.device_name, cert.device_id
    );
    Ok((version, file_count))
}
