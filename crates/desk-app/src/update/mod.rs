//! Core auto-update client: fetches Ed25519-signed manifests via `ConnectionPurpose::UpdateChannel`,
//! downloads binary deltas (`FLDP`) with automatic fallback to full deflate packages, stages into
//! `<InstallDir>\updates\staging`, and coordinates with `flashdesk-svc.exe` via `switch_request.json`
//! and `C:\ProgramData\FlashDesk\update.status.json`.

use crate::config::AppConfig;
use crate::net::connect_and_auth_relay;
use anyhow::{Context, Result};
use base_common::{
    core_exe_for_version, detect_install_layout, is_newer_version, meets_min_base, pid_alive,
    prune_old_versions, read_json_opt, replace_file_atomic, sha256_bytes, sha256_file, unix_now,
    write_json_atomic, InstallLayout, InstallState, SwitchRequest, UpdateIpcAction,
    UpdateIpcRequest, UpdatePhase, UpdateStatusSnapshot, BASE_VERSION, CORE_EXE_NAME,
    LAUNCHER_EXE_NAME, STATE_FILE_NAME, SWITCH_REQUEST_REL, UPDATE_IPC_REQUEST_PATH,
    UPDATE_STATUS_PATH,
};
use protocol::auth::{DeviceIdentity, RelayNodeConfig};
use protocol::framing::{read_frame, write_frame};
use protocol::patch::{apply_delta_patch, decompress_full, MAX_UNCOMPRESSED_SIZE};
use protocol::relay::ConnectionPurpose;
use protocol::update::{SignedUpdateManifest, UpdateClientMsg, UpdateServerMsg};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const UPDATE_CHECK_INTERVAL_SECS: u64 = 86_400; // 24 hours
const DOWNLOAD_CHUNK_SIZE: u32 = 256 * 1024;
const UPDATE_MSG_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct StagedArtifact {
    pub version: String,
    pub staged_exe: PathBuf,
    pub sha256: String,
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub snapshot: UpdateStatusSnapshot,
    pub staged_for_gui: Option<StagedArtifact>,
}

pub fn current_install_layout() -> InstallLayout {
    std::env::current_exe()
        .map(|p| detect_install_layout(&p))
        .unwrap_or(InstallLayout::Portable)
}

pub fn dir_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".write-probe-{}", std::process::id()));
    if std::fs::write(&probe, b"ok").is_ok() {
        let _ = std::fs::remove_file(&probe);
        true
    } else {
        false
    }
}

/// Orders configured relay nodes for update download: `FilePreferred` first, then `active_relay`,
/// then any remaining unique nodes.
pub fn relay_nodes_for_update(cfg: &AppConfig) -> Vec<RelayNodeConfig> {
    let Some(bundle) = &cfg.bundle else {
        return Vec::new();
    };
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();

    if let Some(file_node) = cfg.preferred_file_relay() {
        if seen.insert(file_node.addr.trim().to_string()) {
            ordered.push(file_node.clone());
        }
    }
    if let Some(ctrl_node) = cfg.active_relay() {
        if seen.insert(ctrl_node.addr.trim().to_string()) {
            ordered.push(ctrl_node.clone());
        }
    }
    for n in &bundle.relay_nodes {
        if seen.insert(n.addr.trim().to_string()) {
            ordered.push(n.clone());
        }
    }
    ordered
}

async fn exchange_update_msg(
    stream: &mut TcpStream,
    req: &UpdateClientMsg,
) -> Result<UpdateServerMsg> {
    let bytes = bincode::serialize(req)?;
    tokio::time::timeout(UPDATE_MSG_TIMEOUT, write_frame(stream, &bytes))
        .await
        .map_err(|_| anyhow::anyhow!("发送更新请求超时"))??;
    let raw = tokio::time::timeout(UPDATE_MSG_TIMEOUT, read_frame(stream))
        .await
        .map_err(|_| anyhow::anyhow!("等待更新响应超时"))??;
    let resp: UpdateServerMsg = bincode::deserialize(&raw)?;
    Ok(resp)
}

pub async fn fetch_signed_manifest(
    cfg: &AppConfig,
) -> Result<(RelayNodeConfig, Option<SignedUpdateManifest>)> {
    let bundle = cfg
        .bundle
        .as_ref()
        .context("尚未绑定设备身份，无法连接中转服务器检查更新")?;
    let nodes = relay_nodes_for_update(cfg);
    anyhow::ensure!(!nodes.is_empty(), "尚未配置任何中转服务器节点");

    let mut last_err = None;
    for node in nodes {
        match connect_and_auth_relay(&node.addr, &bundle.identity, ConnectionPurpose::UpdateChannel)
            .await
        {
            Ok(mut stream) => match exchange_update_msg(&mut stream, &UpdateClientMsg::GetManifest).await {
                Ok(UpdateServerMsg::Manifest(Some(signed))) => {
                    signed
                        .verify(&bundle.root_pubkey, unix_now())
                        .context("更新清单 Ed25519 验签失败")?;
                    return Ok((node, Some(signed)));
                }
                Ok(UpdateServerMsg::Manifest(None)) => {
                    return Ok((node, None));
                }
                Ok(UpdateServerMsg::Error(e)) => {
                    last_err = Some(anyhow::anyhow!("中转节点 {} 返回错误: {e}", node.name));
                }
                Ok(other) => {
                    last_err = Some(anyhow::anyhow!("意外的更新响应: {other:?}"));
                }
                Err(e) => {
                    last_err = Some(anyhow::anyhow!("读取节点 {} 清单失败: {e:#}", node.name));
                }
            },
            Err(e) => {
                last_err = Some(anyhow::anyhow!("连接节点 {} ({}) 失败: {e:#}", node.name, node.addr));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("检查更新失败")))
}

async fn download_artifact(
    node: &RelayNodeConfig,
    identity: &DeviceIdentity,
    filename: &str,
    dst_path: &Path,
    expected_sha256: &str,
    mut on_progress: impl FnMut(u8),
) -> Result<()> {
    if let Some(parent) = dst_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part_path = dst_path.with_extension("part");
    let mut stream = connect_and_auth_relay(&node.addr, identity, ConnectionPurpose::UpdateChannel).await?;
    let mut file = tokio::fs::File::create(&part_path).await?;
    let mut offset = 0u64;

    loop {
        let resp = exchange_update_msg(
            &mut stream,
            &UpdateClientMsg::DownloadChunk {
                filename: filename.to_string(),
                offset,
                max_len: DOWNLOAD_CHUNK_SIZE,
            },
        )
        .await?;
        match resp {
            UpdateServerMsg::ChunkData {
                offset: got_off,
                data,
                total_size,
                eof,
            } => {
                anyhow::ensure!(got_off == offset, "下载偏移量不匹配");
                if !data.is_empty() {
                    file.write_all(&data).await?;
                    offset = offset.saturating_add(data.len() as u64);
                }
                if total_size > 0 {
                    let pct = ((offset.saturating_mul(100)) / total_size).min(100) as u8;
                    on_progress(pct);
                }
                if eof || data.is_empty() {
                    break;
                }
            }
            UpdateServerMsg::Error(e) => anyhow::bail!("下载 {filename} 失败: {e}"),
            other => anyhow::bail!("意外的下载响应: {other:?}"),
        }
    }
    file.flush().await?;
    drop(file);

    let got_sha = sha256_file(&part_path)?;
    if !got_sha.eq_ignore_ascii_case(expected_sha256) {
        let _ = std::fs::remove_file(&part_path);
        anyhow::bail!("下载文件 {filename} SHA-256 校验失败");
    }
    replace_file_atomic(&part_path, dst_path)?;
    Ok(())
}

fn update_last_check_timestamp(layout: &InstallLayout, ts: u64) {
    let Some(install_dir) = layout.install_dir() else {
        return;
    };
    let state_path = install_dir.join(STATE_FILE_NAME);
    if let Some(mut st) = read_json_opt::<InstallState>(&state_path) {
        st.last_check_at = ts;
        let _ = write_json_atomic(&state_path, &st);
    }
}

fn make_snapshot(
    state: UpdatePhase,
    base_version: &str,
    available_version: Option<String>,
    notes: Option<String>,
    progress_pct: Option<u8>,
    last_check_at: u64,
    message: Option<String>,
) -> UpdateStatusSnapshot {
    UpdateStatusSnapshot {
        state,
        current_version: CORE_VERSION.to_string(),
        base_version: base_version.to_string(),
        available_version,
        notes,
        progress_pct,
        last_check_at,
        updated_at: unix_now(),
        message,
    }
}

pub async fn check_and_stage_update<F>(
    cfg: &AppConfig,
    layout: &InstallLayout,
    agent_mode: bool,
    mut on_status: F,
) -> Result<CheckOutcome>
where
    F: FnMut(&UpdateStatusSnapshot),
{
    let install_state = layout
        .install_dir()
        .and_then(|d| read_json_opt::<InstallState>(&d.join(STATE_FILE_NAME)));
    let base_version = install_state
        .as_ref()
        .map(|s| s.base_version.clone())
        .unwrap_or_else(|| BASE_VERSION.to_string());
    let prev_check = install_state.as_ref().map_or(0, |s| s.last_check_at);

    let checking = make_snapshot(
        UpdatePhase::Checking,
        &base_version,
        None,
        None,
        None,
        prev_check,
        Some("正在检查更新…".into()),
    );
    on_status(&checking);

    let (node, opt_signed) = fetch_signed_manifest(cfg).await?;
    let now = unix_now();
    update_last_check_timestamp(layout, now);

    let Some(signed) = opt_signed else {
        let snap = make_snapshot(
            UpdatePhase::UpToDate,
            &base_version,
            None,
            None,
            None,
            now,
            Some("服务器尚未发布任何更新包（当前已是最新）".into()),
        );
        on_status(&snap);
        return Ok(CheckOutcome {
            snapshot: snap,
            staged_for_gui: None,
        });
    };

    let m = &signed.manifest;
    if !is_newer_version(&m.version, CORE_VERSION) {
        let snap = make_snapshot(
            UpdatePhase::UpToDate,
            &base_version,
            None,
            Some(m.notes.clone()),
            None,
            now,
            Some(format!("当前已是最新版本 v{CORE_VERSION}")),
        );
        on_status(&snap);
        return Ok(CheckOutcome {
            snapshot: snap,
            staged_for_gui: None,
        });
    }

    if install_state
        .as_ref()
        .is_some_and(|st| st.blacklisted_versions.iter().any(|v| v == &m.version))
    {
        let snap = make_snapshot(
            UpdatePhase::Error,
            &base_version,
            Some(m.version.clone()),
            Some(m.notes.clone()),
            None,
            now,
            Some(format!(
                "版本 v{} 曾在本机启动异常并被自动拉黑，已跳过",
                m.version
            )),
        );
        on_status(&snap);
        return Ok(CheckOutcome {
            snapshot: snap,
            staged_for_gui: None,
        });
    }

    if !meets_min_base(&base_version, &m.min_base_version) {
        let snap = make_snapshot(
            UpdatePhase::NeedsBaseUpgrade {
                min_base_version: m.min_base_version.clone(),
            },
            &base_version,
            Some(m.version.clone()),
            Some(m.notes.clone()),
            None,
            now,
            Some(format!(
                "新版本 v{} 需要基座 >= v{}（当前 v{}），请运行新版 FlashDesk-Setup.exe 覆盖安装",
                m.version, m.min_base_version, base_version
            )),
        );
        on_status(&snap);
        return Ok(CheckOutcome {
            snapshot: snap,
            staged_for_gui: None,
        });
    }

    // Portable mode cannot hot-swap versioned slots; notify user cleanly.
    if matches!(layout, InstallLayout::Portable) {
        let snap = make_snapshot(
            UpdatePhase::StagedPendingIdle,
            &base_version,
            Some(m.version.clone()),
            Some(m.notes.clone()),
            None,
            now,
            Some(format!(
                "发现新版本 v{}（当前为便携单文件模式，建议使用 FlashDesk-Setup.exe 安装以支持自动补丁更新）",
                m.version
            )),
        );
        on_status(&snap);
        return Ok(CheckOutcome {
            snapshot: snap,
            staged_for_gui: None,
        });
    }

    let install_dir = layout.install_dir().unwrap();
    let install_writable = dir_writable(install_dir);
    let (cache_dir, staging_exe) = if install_writable {
        (
            install_dir.join("updates").join("cache"),
            install_dir
                .join("updates")
                .join("staging")
                .join(&m.version)
                .join(CORE_EXE_NAME),
        )
    } else {
        let tmp_base = std::env::temp_dir().join("flashdesk-update");
        (
            tmp_base.join("cache"),
            tmp_base.join("staging").join(&m.version).join(CORE_EXE_NAME),
        )
    };

    let dl_init = make_snapshot(
        UpdatePhase::Downloading,
        &base_version,
        Some(m.version.clone()),
        Some(m.notes.clone()),
        Some(0),
        now,
        Some(format!("正在从 {} 下载更新 v{}…", node.name, m.version)),
    );
    on_status(&dl_init);

    let bundle = cfg.bundle.as_ref().unwrap();
    let cur_exe = std::env::current_exe()?;
    let cur_sha = sha256_file(&cur_exe)?;

    let mut applied_via_delta = false;
    let mut synthesized_bytes: Option<Vec<u8>> = None;

    if let Some(delta) = m
        .deltas
        .iter()
        .find(|d| d.from_sha256.eq_ignore_ascii_case(&cur_sha))
    {
        let patch_path = cache_dir.join(&delta.filename);
        let dl_res = download_artifact(
            &node,
            &bundle.identity,
            &delta.filename,
            &patch_path,
            &delta.sha256,
            |pct| {
                let s = make_snapshot(
                    UpdatePhase::Downloading,
                    &base_version,
                    Some(m.version.clone()),
                    Some(m.notes.clone()),
                    Some(pct),
                    now,
                    Some(format!("正在下载差量补丁 ({pct}%)…")),
                );
                on_status(&s);
            },
        )
        .await;
        match dl_res {
            Ok(()) => {
                let old_bytes = std::fs::read(&cur_exe)?;
                let patch_bytes = std::fs::read(&patch_path)?;
                let _ = std::fs::remove_file(&patch_path);
                match apply_delta_patch(&old_bytes, &patch_bytes, MAX_UNCOMPRESSED_SIZE) {
                    Ok(new_bytes) => {
                        let got = sha256_bytes(&new_bytes);
                        if got.eq_ignore_ascii_case(&m.full.target_sha256) {
                            tracing::info!(
                                "delta patch applied successfully ({} -> {} bytes)",
                                patch_bytes.len(),
                                new_bytes.len()
                            );
                            synthesized_bytes = Some(new_bytes);
                            applied_via_delta = true;
                        } else {
                            tracing::warn!("delta output SHA-256 mismatch; falling back to full");
                        }
                    }
                    Err(e) => {
                        tracing::warn!("delta patch apply failed ({e}); falling back to full");
                    }
                }
            }
            Err(e) => {
                tracing::warn!("delta download failed ({e:#}); falling back to full");
                let _ = std::fs::remove_file(&patch_path);
            }
        }
    }

    if !applied_via_delta {
        let full_path = cache_dir.join(&m.full.filename);
        download_artifact(
            &node,
            &bundle.identity,
            &m.full.filename,
            &full_path,
            &m.full.sha256,
            |pct| {
                let s = make_snapshot(
                    UpdatePhase::Downloading,
                    &base_version,
                    Some(m.version.clone()),
                    Some(m.notes.clone()),
                    Some(pct),
                    now,
                    Some(format!("正在下载完整更新包 ({pct}%)…")),
                );
                on_status(&s);
            },
        )
        .await?;
        let compressed = std::fs::read(&full_path)?;
        let _ = std::fs::remove_file(&full_path);
        let new_bytes = decompress_full(&compressed, MAX_UNCOMPRESSED_SIZE)?;
        let got = sha256_bytes(&new_bytes);
        anyhow::ensure!(
            got.eq_ignore_ascii_case(&m.full.target_sha256),
            "解压后核心程序 SHA-256 校验失败"
        );
        synthesized_bytes = Some(new_bytes);
    }

    let final_bytes = synthesized_bytes.context("missing synthesized update binary")?;
    if let Some(parent) = staging_exe.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_stage = staging_exe.with_extension("exe.tmp");
    std::fs::write(&tmp_stage, &final_bytes)?;
    replace_file_atomic(&tmp_stage, &staging_exe)?;

    if agent_mode && install_writable {
        let req = SwitchRequest {
            target_version: m.version.clone(),
            staged_exe_rel: format!(r"updates\staging\{}\{}", m.version, CORE_EXE_NAME),
            target_sha256: m.full.target_sha256.clone(),
            min_base_version: m.min_base_version.clone(),
            requested_at: unix_now(),
            force_immediate: false,
        };
        write_json_atomic(&install_dir.join(SWITCH_REQUEST_REL), &req)?;
        let snap = make_snapshot(
            UpdatePhase::StagedPendingIdle,
            &base_version,
            Some(m.version.clone()),
            Some(m.notes.clone()),
            Some(100),
            now,
            Some(format!(
                "新版本 v{} 已校验就绪（{}），空闲后由基座静默切换",
                m.version,
                if applied_via_delta { "差量补丁" } else { "全量包" }
            )),
        );
        on_status(&snap);
        return Ok(CheckOutcome {
            snapshot: snap,
            staged_for_gui: None,
        });
    }

    let snap = make_snapshot(
        UpdatePhase::StagedPendingIdle,
        &base_version,
        Some(m.version.clone()),
        Some(m.notes.clone()),
        Some(100),
        now,
        Some(format!(
            "新版本 v{} 已准备就绪（{}），点击「立即重启更新」完成切换",
            m.version,
            if applied_via_delta { "差量补丁" } else { "全量包" }
        )),
    );
    on_status(&snap);
    Ok(CheckOutcome {
        snapshot: snap,
        staged_for_gui: Some(StagedArtifact {
            version: m.version.clone(),
            staged_exe: staging_exe,
            sha256: m.full.target_sha256.clone(),
            notes: m.notes.clone(),
        }),
    })
}

/// Applies a verified staged core binary directly into `<install_dir>\versions\<ver>\` and updates
/// `state.json` (used for GUI-only Managed installs without `FlashDeskService`).
pub fn apply_staged_update_cli(
    install_dir: &Path,
    staged: &Path,
    version: &str,
    expected_sha256: &str,
) -> Result<()> {
    let got_sha = sha256_file(staged)?;
    anyhow::ensure!(
        got_sha.eq_ignore_ascii_case(expected_sha256),
        "staged binary SHA-256 mismatch"
    );
    let dst_exe = core_exe_for_version(install_dir, version);
    if let Some(parent) = dst_exe.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dst_exe.with_extension("exe.tmp");
    std::fs::copy(staged, &tmp)?;
    replace_file_atomic(&tmp, &dst_exe)?;

    let state_path = install_dir.join(STATE_FILE_NAME);
    let mut state = read_json_opt::<InstallState>(&state_path).unwrap_or(InstallState {
        base_version: BASE_VERSION.to_string(),
        active_version: CORE_VERSION.to_string(),
        previous_good_version: None,
        candidate: None,
        blacklisted_versions: Vec::new(),
        last_check_at: unix_now(),
        installed_at: unix_now(),
    });
    if state.active_version != version {
        state.previous_good_version = Some(state.active_version.clone());
    }
    state.active_version = version.to_string();
    state.candidate = None;
    write_json_atomic(&state_path, &state)?;
    let _ = prune_old_versions(&install_dir.join("versions"), &state);
    let _ = std::fs::remove_file(staged);
    Ok(())
}

/// Applies a staged update for a GUI-only Managed install, elevating via UAC once if `install_dir`
/// is protected (e.g. `Program Files`).
pub fn apply_staged_for_gui(install_dir: &Path, art: &StagedArtifact) -> Result<()> {
    if dir_writable(install_dir) {
        return apply_staged_update_cli(install_dir, &art.staged_exe, &art.version, &art.sha256);
    }
    let exe = std::env::current_exe().context("无法获取当前 exe 路径")?;
    let args = format!(
        "--apply-staged-update --install-dir \"{}\" --staged \"{}\" --version \"{}\" --sha256 \"{}\"",
        install_dir.display(),
        art.staged_exe.display(),
        art.version,
        art.sha256
    );
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let args_w: Vec<u16> = OsStr::new(&args).encode_wide().chain(Some(0)).collect();
    let ret = unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("runas"),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        )
    };
    if (ret.0 as isize) <= 32 {
        anyhow::bail!("提权应用更新被取消或失败 (code={})", ret.0 as isize);
    }
    // Wait up to 8s for elevated child to update state.json.
    let state_path = install_dir.join(STATE_FILE_NAME);
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(st) = read_json_opt::<InstallState>(&state_path) {
            if st.active_version == art.version {
                return Ok(());
            }
        }
    }
    anyhow::bail!("等待提权应用更新完成超时");
}

pub fn write_ipc_check_now() -> Result<()> {
    let req = UpdateIpcRequest {
        action: UpdateIpcAction::CheckNow,
        requested_at: unix_now(),
    };
    write_json_atomic(Path::new(UPDATE_IPC_REQUEST_PATH), &req)?;
    Ok(())
}

/// Spawns the stable `<InstallDir>\FlashDesk.exe --wait-pid <self_pid>` so the new version waits
/// for the current GUI process to release the single-instance mutex before starting.
pub fn restart_via_launcher(install_dir: &Path) -> Result<()> {
    let launcher = install_dir.join(LAUNCHER_EXE_NAME);
    anyhow::ensure!(launcher.is_file(), "未找到启动器 {}", launcher.display());
    let pid = std::process::id().to_string();
    std::process::Command::new(&launcher)
        .args(["--wait-pid", &pid])
        .spawn()
        .with_context(|| format!("启动 {} 失败", launcher.display()))?;
    Ok(())
}

/// Waits up to `max_wait` for `pid` to exit (used by `--wait-pid` during self-restart).
pub fn wait_for_pid_exit(pid: u32, max_wait: Duration) {
    let deadline = std::time::Instant::now() + max_wait;
    while std::time::Instant::now() < deadline && pid_alive(pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(120));
}

pub fn publish_update_snapshot(snap: &UpdateStatusSnapshot) {
    let _ = write_json_atomic(Path::new(UPDATE_STATUS_PATH), snap);
}

/// Background updater loop inside `--agent`: zero frequent polling; only checks on
/// (a) local GUI IPC trigger, (b) remote `CheckUpdateNow` trigger, or (c) once every 24h online.
pub fn spawn_agent_update_task(
    cfg: Arc<Mutex<AppConfig>>,
    mut remote_trigger_rx: mpsc::UnboundedReceiver<()>,
) {
    tokio::spawn(async move {
        let layout = current_install_layout();
        let ipc_path = PathBuf::from(UPDATE_IPC_REQUEST_PATH);
        let is_running = Arc::new(AtomicBool::new(false));
        let mut local_tick = tokio::time::interval(Duration::from_secs(2));
        let mut last_daily_check_poll = 0u64;

        loop {
            let mut should_check = false;
            tokio::select! {
                msg = remote_trigger_rx.recv() => {
                    if msg.is_none() {
                        break;
                    }
                    should_check = true;
                }
                _ = local_tick.tick() => {
                    if ipc_path.is_file() {
                        if let Some(req) = read_json_opt::<UpdateIpcRequest>(&ipc_path) {
                            let _ = std::fs::remove_file(&ipc_path);
                            if req.action == UpdateIpcAction::CheckNow {
                                should_check = true;
                            }
                        } else {
                            let _ = std::fs::remove_file(&ipc_path);
                        }
                    } else if let Some(install_dir) = layout.install_dir() {
                        let now = unix_now();
                        if now.saturating_sub(last_daily_check_poll) >= 60 {
                            last_daily_check_poll = now;
                            if let Some(st) = read_json_opt::<InstallState>(&install_dir.join(STATE_FILE_NAME)) {
                                if now.saturating_sub(st.last_check_at) >= UPDATE_CHECK_INTERVAL_SECS {
                                    should_check = true;
                                }
                            }
                        }
                    }
                }
            }

            if should_check && !is_running.swap(true, Ordering::AcqRel) {
                let cfg_snap = cfg.lock().unwrap().clone();
                if cfg_snap.bundle.is_none() {
                    is_running.store(false, Ordering::Release);
                    continue;
                }
                let layout_clone = layout.clone();
                let flag = Arc::clone(&is_running);
                tokio::spawn(async move {
                    let res = check_and_stage_update(&cfg_snap, &layout_clone, true, |snap| {
                        publish_update_snapshot(snap);
                    })
                    .await;
                    if let Err(e) = res {
                        let base_ver = layout_clone
                            .install_dir()
                            .and_then(|d| read_json_opt::<InstallState>(&d.join(STATE_FILE_NAME)))
                            .map(|s| s.base_version)
                            .unwrap_or_else(|| BASE_VERSION.to_string());
                        let err_snap = make_snapshot(
                            UpdatePhase::Error,
                            &base_ver,
                            None,
                            None,
                            None,
                            unix_now(),
                            Some(format!("自动检查更新失败: {e:#}")),
                        );
                        publish_update_snapshot(&err_snap);
                        tracing::warn!("agent update check failed: {e:#}");
                    }
                    flag.store(false, Ordering::Release);
                });
            }
        }
    });
}
