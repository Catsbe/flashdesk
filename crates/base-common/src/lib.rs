//! Shared minimal contract between FlashDesk's frozen base binaries (`flashdesk-svc.exe`,
//! `FlashDesk.exe`, `flashdesk-uninstall.exe`) and the updatable core (`flashdesk-core.exe`).
//!
//! Architectural red line: this crate must NEVER depend on `tokio`, `snow`, `ed25519-dalek`,
//! `eframe`, `openh264`, or `protocol`.

#[cfg(feature = "setup-payload")]
pub mod setup_payload;

use semver::Version;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const BASE_VERSION: &str = "1.0.0";
pub const SERVICE_NAME: &str = "FlashDeskService";
pub const SERVICE_DISPLAY_NAME: &str = "FlashDesk 远程桌面后台服务";

pub const SHARED_PROGRAM_DATA_DIR: &str = r"C:\ProgramData\FlashDesk";
pub const SHARED_CONFIG_PATH: &str = r"C:\ProgramData\FlashDesk\config.json";
pub const AGENT_STATUS_PATH: &str = r"C:\ProgramData\FlashDesk\agent.status.json";
pub const UPDATE_STATUS_PATH: &str = r"C:\ProgramData\FlashDesk\update.status.json";
pub const UPDATE_IPC_DIR: &str = r"C:\ProgramData\FlashDesk\ipc";
pub const UPDATE_IPC_REQUEST_PATH: &str = r"C:\ProgramData\FlashDesk\ipc\request.json";

pub const WATCHDOG_HEALTH_WINDOW_SECS: u64 = 15;
pub const IDLE_WAIT_BEFORE_SWITCH_SECS: u64 = 10;
pub const AGENT_HEARTBEAT_TIMEOUT_SECS: u64 = 12;

pub const CORE_EXE_NAME: &str = "flashdesk-core.exe";
pub const LAUNCHER_EXE_NAME: &str = "FlashDesk.exe";
pub const SVC_EXE_NAME: &str = "flashdesk-svc.exe";
pub const UNINSTALL_EXE_NAME: &str = "flashdesk-uninstall.exe";
pub const STATE_FILE_NAME: &str = "state.json";
pub const SWITCH_REQUEST_REL: &str = r"updates\switch_request.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateState {
    pub version: String,
    pub attempts: u32,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallState {
    pub base_version: String,
    pub active_version: String,
    #[serde(default)]
    pub previous_good_version: Option<String>,
    #[serde(default)]
    pub candidate: Option<CandidateState>,
    #[serde(default)]
    pub blacklisted_versions: Vec<String>,
    #[serde(default)]
    pub last_check_at: u64,
    #[serde(default)]
    pub installed_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SwitchRequest {
    pub target_version: String,
    pub staged_exe_rel: String,
    pub target_sha256: String,
    pub min_base_version: String,
    pub requested_at: u64,
    #[serde(default)]
    pub force_immediate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentStatus {
    pub pid: u32,
    pub session_id: u32,
    /// True when the agent token is elevated (SYSTEM service agent).
    pub elevated: bool,
    pub started_at: u64,
    pub heartbeat_at: u64,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub active_sessions: u32,
}

impl AgentStatus {
    pub fn is_fresh_at(&self, now: u64) -> bool {
        now >= self.heartbeat_at && now - self.heartbeat_at <= AGENT_HEARTBEAT_TIMEOUT_SECS
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum UpdateIpcAction {
    CheckNow,
    ApplyStaged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateIpcRequest {
    pub action: UpdateIpcAction,
    pub requested_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum UpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Downloading,
    StagedPendingIdle,
    SwitchedRestartGui,
    NeedsBaseUpgrade { min_base_version: String },
    RolledBack { bad_version: String },
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateStatusSnapshot {
    pub state: UpdatePhase,
    pub current_version: String,
    pub base_version: String,
    #[serde(default)]
    pub available_version: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub progress_pct: Option<u8>,
    #[serde(default)]
    pub last_check_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallLayout {
    ManagedCore {
        install_dir: PathBuf,
        running_version: String,
    },
    ManagedRoot {
        install_dir: PathBuf,
    },
    Portable,
}

impl InstallLayout {
    pub fn install_dir(&self) -> Option<&Path> {
        match self {
            InstallLayout::ManagedCore { install_dir, .. }
            | InstallLayout::ManagedRoot { install_dir } => Some(install_dir.as_path()),
            InstallLayout::Portable => None,
        }
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn detect_install_layout(exe_path: &Path) -> InstallLayout {
    if let Some(ver_dir) = exe_path.parent() {
        if let Some(versions_dir) = ver_dir.parent() {
            let is_versions = versions_dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("versions"));
            if is_versions {
                if let Some(install_dir) = versions_dir.parent() {
                    if install_dir.join(STATE_FILE_NAME).is_file() {
                        let running_version = ver_dir
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        return InstallLayout::ManagedCore {
                            install_dir: install_dir.to_path_buf(),
                            running_version,
                        };
                    }
                }
            }
        }
        if ver_dir.join(STATE_FILE_NAME).is_file() && ver_dir.join("versions").is_dir() {
            return InstallLayout::ManagedRoot {
                install_dir: ver_dir.to_path_buf(),
            };
        }
    }
    InstallLayout::Portable
}

pub fn read_json_opt<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp_name = format!(
        "{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("data"),
        std::process::id()
    );
    let tmp = path.with_file_name(tmp_name);
    let data = serde_json::to_vec_pretty(value)?;
    std::fs::write(&tmp, data)?;
    replace_file_atomic(&tmp, path)
}

pub fn replace_file_atomic(src_tmp: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};

        let src_w: Vec<u16> = src_tmp
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let dst_w: Vec<u16> = dst
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let res = unsafe {
            MoveFileExW(
                PCWSTR(src_w.as_ptr()),
                PCWSTR(dst_w.as_ptr()),
                MOVEFILE_REPLACE_EXISTING,
            )
        };
        if res.is_ok() {
            return Ok(());
        }
    }
    let _ = std::fs::remove_file(dst);
    std::fs::rename(src_tmp, dst)
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

pub fn sha256_bytes(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Renders the FlashDesk icon (dark rounded squircle badge + teal border + electric-teal lightning
/// bolt) at `size x size` straight RGBA pixels using 4x4 supersampling.
pub fn build_app_icon_rgba(size: usize) -> Vec<u8> {
    if size == 0 {
        return Vec::new();
    }
    let mut rgba = vec![0u8; size * size * 4];
    let scale = 32.0 / size as f32;
    let aa = (0.35 * scale).max(0.04);
    let bolt_top: [(f32, f32); 3] = [(17.8, 5.0), (10.0, 17.4), (16.4, 17.4)];
    let bolt_bot: [(f32, f32); 3] = [(14.2, 27.0), (22.0, 14.6), (15.6, 14.6)];

    for y in 0..size {
        for x in 0..size {
            let mut r_acc = 0.0f32;
            let mut g_acc = 0.0f32;
            let mut b_acc = 0.0f32;
            let mut a_acc = 0.0f32;

            for sy in 0..4 {
                let ny = (y as f32 + (sy as f32 + 0.5) * 0.25) * scale;
                for sx in 0..4 {
                    let nx = (x as f32 + (sx as f32 + 0.5) * 0.25) * scale;
                    let dist = rounded_box_sdf(nx - 16.0, ny - 16.0, 13.8, 13.8, 6.2);
                    if dist > aa {
                        continue;
                    }
                    let alpha = ((-dist + aa) / (2.0 * aa)).clamp(0.0, 1.0);
                    if alpha <= 0.0 {
                        continue;
                    }

                    let in_bolt =
                        point_in_tri(nx, ny, bolt_top) || point_in_tri(nx, ny, bolt_bot);
                    let (r, g, b) = if in_bolt {
                        let bt = ((ny - 5.0) / 22.0).clamp(0.0, 1.0);
                        (
                            lerp_f32(82.0, 38.0, bt),
                            lerp_f32(238.0, 191.0, bt),
                            lerp_f32(208.0, 160.0, bt),
                        )
                    } else if dist > -1.35 {
                        (45.0, 200.0, 170.0)
                    } else {
                        let vt = ((ny - 2.2) / 27.6).clamp(0.0, 1.0);
                        let mut r = lerp_f32(24.0, 13.0, vt);
                        let mut g = lerp_f32(36.0, 21.0, vt);
                        let mut b = lerp_f32(48.0, 29.0, vt);

                        let radial = ((nx - 16.0).hypot(ny - 16.0) / 13.0).clamp(0.0, 1.0);
                        let center_glow = (1.0 - radial).powi(2) * 0.16;
                        let rim_glow = if dist > -2.8 {
                            ((dist + 2.8) / 1.45).clamp(0.0, 1.0) * 0.14
                        } else {
                            0.0
                        };
                        let tint = (center_glow + rim_glow).clamp(0.0, 0.3);
                        r = lerp_f32(r, 45.0, tint);
                        g = lerp_f32(g, 200.0, tint);
                        b = lerp_f32(b, 170.0, tint);
                        (r, g, b)
                    };

                    r_acc += r * alpha;
                    g_acc += g * alpha;
                    b_acc += b * alpha;
                    a_acc += alpha;
                }
            }

            if a_acc > 0.0 {
                let off = (y * size + x) * 4;
                rgba[off] = (r_acc / a_acc).round().clamp(0.0, 255.0) as u8;
                rgba[off + 1] = (g_acc / a_acc).round().clamp(0.0, 255.0) as u8;
                rgba[off + 2] = (b_acc / a_acc).round().clamp(0.0, 255.0) as u8;
                rgba[off + 3] = ((a_acc / 16.0) * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    rgba
}

fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn rounded_box_sdf(px: f32, py: f32, half_w: f32, half_h: f32, radius: f32) -> f32 {
    let qx = px.abs() - half_w + radius;
    let qy = py.abs() - half_h + radius;
    let out = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    out + inside - radius
}

fn point_in_tri(px: f32, py: f32, tri: [(f32, f32); 3]) -> bool {
    let sign = |a: (f32, f32), b: (f32, f32)| (px - b.0) * (a.1 - b.1) - (a.0 - b.0) * (py - b.1);
    let d1 = sign(tri[0], tri[1]);
    let d2 = sign(tri[1], tri[2]);
    let d3 = sign(tri[2], tri[0]);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
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

/// Checks process existence using a ToolHelp snapshot (never OpenProcess, per Pitfall #15 DACL).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return false;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut alive = false;
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    if entry.th32ProcessID == pid {
                        alive = true;
                        break;
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            alive
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

/// Default Windows service DACL plus `RP` (SERVICE_START) and `WP` (SERVICE_STOP) for
/// Interactive Users (`IU`) so the logged-in desktop GUI can stop/start `FlashDeskService`
/// without a UAC prompt, while still blocking config changes, deletion, or ACL edits.
pub const SERVICE_INTERACTIVE_CONTROL_SDDL: &str =
    "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPWPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";

#[cfg(windows)]
fn apply_sddl_dacl(
    object_name: &std::ffi::OsStr,
    object_type: windows::Win32::Security::Authorization::SE_OBJECT_TYPE,
    sddl: &str,
) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{BOOL, ERROR_SUCCESS, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, ACL, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let sddl_w: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
        let mut dacl_present = BOOL::default();
        let mut dacl_defaulted = BOOL::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let dacl_res =
            GetSecurityDescriptorDacl(sd, &mut dacl_present, &mut dacl, &mut dacl_defaulted);
        let obj_w: Vec<u16> = object_name
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let set_err = if dacl_res.is_ok() && dacl_present.as_bool() && !dacl.is_null() {
            SetNamedSecurityInfoW(
                PCWSTR(obj_w.as_ptr()),
                object_type,
                DACL_SECURITY_INFORMATION,
                PSID::default(),
                PSID::default(),
                Some(dacl),
                None,
            )
        } else {
            let _ = LocalFree(HLOCAL(sd.0));
            anyhow::bail!("SDDL did not yield a valid DACL");
        };
        let _ = LocalFree(HLOCAL(sd.0));
        if set_err != ERROR_SUCCESS {
            anyhow::bail!("SetNamedSecurityInfoW failed: {:?}", set_err);
        }
    }
    Ok(())
}

/// Ensures `dir` exists and grants Authenticated Users Modify access (`0x1301bf`) alongside full
/// SYSTEM/Administrators access so a standard-user GUI can drop IPC triggers for the SYSTEM agent.
pub fn ensure_ipc_dir_writable(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(windows)]
    {
        apply_sddl_dacl(
            dir.as_os_str(),
            windows::Win32::Security::Authorization::SE_FILE_OBJECT,
            "D:(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1301bf;;;AU)",
        )?;
    }
    Ok(())
}

/// Grants Interactive Users (`IU`) permission to start (`RP`) and stop (`WP`) the named Windows
/// service without UAC elevation, while keeping configuration, deletion, and ACL edits admin-only.
pub fn grant_interactive_user_service_control(service_name: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        apply_sddl_dacl(
            std::ffi::OsStr::new(service_name),
            windows::Win32::Security::Authorization::SE_SERVICE,
            SERVICE_INTERACTIVE_CONTROL_SDDL,
        )?;
    }
    #[cfg(not(windows))]
    {
        let _ = service_name;
    }
    Ok(())
}

/// Validates a relative path from `SwitchRequest::staged_exe_rel` stays strictly inside `updates/staging`.
pub fn resolve_staged_exe_path(install_dir: &Path, staged_exe_rel: &str) -> Option<PathBuf> {
    let rel = Path::new(staged_exe_rel);
    if rel.is_absolute() {
        return None;
    }
    let mut comps = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(seg) => comps.push(seg.to_string_lossy().to_ascii_lowercase()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if comps.len() < 3 || comps[0] != "updates" || comps[1] != "staging" {
        return None;
    }
    Some(install_dir.join(rel))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchDecision {
    DeferActiveSession,
    Reject(String),
    Apply { staged_abs: PathBuf },
}

pub fn evaluate_switch_request(
    install_dir: &Path,
    state: &InstallState,
    req: &SwitchRequest,
    idle_secs: u64,
) -> SwitchDecision {
    if Version::parse(req.target_version.trim()).is_err() {
        return SwitchDecision::Reject(format!("无效的目标版本号: {}", req.target_version));
    }
    if state.blacklisted_versions.contains(&req.target_version) {
        return SwitchDecision::Reject(format!(
            "版本 {} 曾启动失败并已被拉黑，跳过切换",
            req.target_version
        ));
    }
    if !meets_min_base(&state.base_version, &req.min_base_version) {
        return SwitchDecision::Reject(format!(
            "当前基座版本 {} 低于所需最低基座版本 {}",
            state.base_version, req.min_base_version
        ));
    }
    let Some(staged_abs) = resolve_staged_exe_path(install_dir, &req.staged_exe_rel) else {
        return SwitchDecision::Reject(format!("非法暂存路径: {}", req.staged_exe_rel));
    };
    if !staged_abs.is_file() {
        return SwitchDecision::Reject(format!("暂存文件不存在: {}", staged_abs.display()));
    }
    match sha256_file(&staged_abs) {
        Ok(got) if got.eq_ignore_ascii_case(&req.target_sha256) => {}
        Ok(got) => {
            return SwitchDecision::Reject(format!(
                "暂存文件 SHA-256 校验不匹配 (got {got}, expected {})",
                req.target_sha256
            ));
        }
        Err(e) => {
            return SwitchDecision::Reject(format!("计算暂存文件哈希失败: {e}"));
        }
    }
    if !req.force_immediate && idle_secs < IDLE_WAIT_BEFORE_SWITCH_SECS {
        return SwitchDecision::DeferActiveSession;
    }
    SwitchDecision::Apply { staged_abs }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogDecision {
    NoCandidate,
    Waiting,
    Promote { version: String },
    RetryCandidate { attempt: u32 },
    Rollback {
        bad_version: String,
        fallback_version: String,
    },
}

pub fn evaluate_watchdog(
    state: &InstallState,
    agent_status: Option<&AgentStatus>,
    now_unix: u64,
    agent_exited: bool,
) -> WatchdogDecision {
    let Some(cand) = &state.candidate else {
        return WatchdogDecision::NoCandidate;
    };

    if agent_exited {
        if cand.attempts < 2 {
            return WatchdogDecision::RetryCandidate {
                attempt: cand.attempts.saturating_add(1),
            };
        }
        let fallback = state
            .previous_good_version
            .clone()
            .filter(|v| v != &cand.version)
            .unwrap_or_else(|| state.active_version.clone());
        return WatchdogDecision::Rollback {
            bad_version: cand.version.clone(),
            fallback_version: fallback,
        };
    }

    let elapsed_ok =
        now_unix >= cand.started_at && now_unix - cand.started_at >= WATCHDOG_HEALTH_WINDOW_SECS;
    let heartbeat_ok = agent_status.is_some_and(|st| {
        st.is_fresh_at(now_unix) && st.version.as_deref() == Some(cand.version.as_str())
    });

    if elapsed_ok && heartbeat_ok {
        WatchdogDecision::Promote {
            version: cand.version.clone(),
        }
    } else {
        WatchdogDecision::Waiting
    }
}

pub fn apply_watchdog_promote(state: &mut InstallState, promoted_ver: &str) {
    state.active_version = promoted_ver.to_string();
    state.candidate = None;
}

pub fn apply_watchdog_rollback(state: &mut InstallState, bad_ver: &str, fallback_ver: &str) {
    state.active_version = fallback_ver.to_string();
    if state.previous_good_version.as_deref() == Some(bad_ver) {
        state.previous_good_version = None;
    }
    state.candidate = None;
    if !state.blacklisted_versions.iter().any(|v| v == bad_ver) {
        state.blacklisted_versions.push(bad_ver.to_string());
    }
}

pub fn core_exe_for_version(install_dir: &Path, version: &str) -> PathBuf {
    install_dir
        .join("versions")
        .join(version)
        .join(CORE_EXE_NAME)
}

/// Resolves the best available `flashdesk-core.exe` from `state.json` or highest semver subdir.
pub fn resolve_launch_core_exe(install_dir: &Path) -> Option<(String, PathBuf)> {
    if let Some(st) = read_json_opt::<InstallState>(&install_dir.join(STATE_FILE_NAME)) {
        let active_p = core_exe_for_version(install_dir, &st.active_version);
        if active_p.is_file() {
            return Some((st.active_version, active_p));
        }
        if let Some(prev) = &st.previous_good_version {
            let prev_p = core_exe_for_version(install_dir, prev);
            if prev_p.is_file() {
                return Some((prev.clone(), prev_p));
            }
        }
    }

    let versions_dir = install_dir.join("versions");
    let mut found: Vec<(Version, String, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&versions_dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let candidate_exe = entry.path().join(CORE_EXE_NAME);
            if candidate_exe.is_file() {
                if let Ok(ver) = Version::parse(&name) {
                    found.push((ver, name, candidate_exe));
                }
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().next().map(|(_, v, p)| (v, p))
}

/// Prunes older version slot folders under `versions/`, keeping only `active_version`,
/// `previous_good_version`, and any active `candidate`.
pub fn prune_old_versions(versions_dir: &Path, state: &InstallState) -> std::io::Result<Vec<String>> {
    let mut removed = Vec::new();
    if !versions_dir.is_dir() {
        return Ok(removed);
    }
    for entry in std::fs::read_dir(versions_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let keep = name == state.active_version
            || state.previous_good_version.as_deref() == Some(name.as_str())
            || state
                .candidate
                .as_ref()
                .is_some_and(|c| c.version == name);
        if !keep {
            std::fs::remove_dir_all(entry.path())?;
            removed.push(name);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("flashdesk-base-test-{tag}-{}", unix_now()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn detect_layout_identifies_managed_core_root_and_portable() {
        let root = temp_dir("layout");
        let core_exe = root.join("versions").join("0.2.0").join(CORE_EXE_NAME);
        std::fs::create_dir_all(core_exe.parent().unwrap()).unwrap();
        std::fs::write(&core_exe, b"exe").unwrap();

        // Without state.json it stays Portable.
        assert_eq!(detect_install_layout(&core_exe), InstallLayout::Portable);

        let st = InstallState {
            base_version: BASE_VERSION.into(),
            active_version: "0.2.0".into(),
            previous_good_version: None,
            candidate: None,
            blacklisted_versions: vec![],
            last_check_at: 0,
            installed_at: 1,
        };
        write_json_atomic(&root.join(STATE_FILE_NAME), &st).unwrap();

        assert_eq!(
            detect_install_layout(&core_exe),
            InstallLayout::ManagedCore {
                install_dir: root.clone(),
                running_version: "0.2.0".into()
            }
        );
        assert_eq!(
            detect_install_layout(&root.join(LAUNCHER_EXE_NAME)),
            InstallLayout::ManagedRoot {
                install_dir: root.clone()
            }
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn switch_request_validation_deferral_and_apply() {
        let root = temp_dir("switch");
        let staged = root
            .join("updates")
            .join("staging")
            .join("0.2.0")
            .join(CORE_EXE_NAME);
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"new-core-binary").unwrap();
        let good_hash = sha256_file(&staged).unwrap();

        let mut state = InstallState {
            base_version: "1.0.0".into(),
            active_version: "0.1.0".into(),
            previous_good_version: None,
            candidate: None,
            blacklisted_versions: vec!["0.1.9".into()],
            last_check_at: 0,
            installed_at: 1,
        };

        let req = SwitchRequest {
            target_version: "0.2.0".into(),
            staged_exe_rel: r"updates\staging\0.2.0\flashdesk-core.exe".into(),
            target_sha256: good_hash.clone(),
            min_base_version: "1.0.0".into(),
            requested_at: 100,
            force_immediate: false,
        };

        assert_eq!(
            evaluate_switch_request(&root, &state, &req, 5),
            SwitchDecision::DeferActiveSession
        );
        assert_eq!(
            evaluate_switch_request(&root, &state, &req, IDLE_WAIT_BEFORE_SWITCH_SECS),
            SwitchDecision::Apply {
                staged_abs: staged.clone()
            }
        );

        let mut bad_hash = req.clone();
        bad_hash.target_sha256 = "0".repeat(64);
        assert!(matches!(
            evaluate_switch_request(&root, &state, &bad_hash, 20),
            SwitchDecision::Reject(_)
        ));

        let mut bad_base = req.clone();
        bad_base.min_base_version = "2.0.0".into();
        assert!(matches!(
            evaluate_switch_request(&root, &state, &bad_base, 20),
            SwitchDecision::Reject(_)
        ));

        let mut traversal = req.clone();
        traversal.staged_exe_rel = r"..\..\evil.exe".into();
        assert!(matches!(
            evaluate_switch_request(&root, &state, &traversal, 20),
            SwitchDecision::Reject(_)
        ));

        state.blacklisted_versions.push("0.2.0".into());
        assert!(matches!(
            evaluate_switch_request(&root, &state, &req, 20),
            SwitchDecision::Reject(_)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn watchdog_promotes_healthy_candidate_and_rolls_back_crashing_one() {
        let mut state = InstallState {
            base_version: "1.0.0".into(),
            active_version: "0.2.0".into(),
            previous_good_version: Some("0.1.0".into()),
            candidate: Some(CandidateState {
                version: "0.2.0".into(),
                attempts: 1,
                started_at: 1000,
            }),
            blacklisted_versions: vec![],
            last_check_at: 0,
            installed_at: 1,
        };

        let stale_or_old_ver = AgentStatus {
            pid: 123,
            session_id: 1,
            elevated: true,
            started_at: 1000,
            heartbeat_at: 1016,
            version: Some("0.1.0".into()),
            active_sessions: 0,
        };
        assert_eq!(
            evaluate_watchdog(&state, Some(&stale_or_old_ver), 1016, false),
            WatchdogDecision::Waiting
        );

        let healthy = AgentStatus {
            pid: 123,
            session_id: 1,
            elevated: true,
            started_at: 1000,
            heartbeat_at: 1015,
            version: Some("0.2.0".into()),
            active_sessions: 0,
        };
        assert_eq!(
            evaluate_watchdog(&state, Some(&healthy), 1015, false),
            WatchdogDecision::Promote {
                version: "0.2.0".into()
            }
        );
        apply_watchdog_promote(&mut state, "0.2.0");
        assert!(state.candidate.is_none());
        assert_eq!(state.active_version, "0.2.0");

        // Crash test: 1st crash retries, 2nd crash rolls back + blacklists.
        state.candidate = Some(CandidateState {
            version: "0.3.0".into(),
            attempts: 1,
            started_at: 2000,
        });
        state.previous_good_version = Some("0.2.0".into());
        state.active_version = "0.3.0".into();

        assert_eq!(
            evaluate_watchdog(&state, None, 2002, true),
            WatchdogDecision::RetryCandidate { attempt: 2 }
        );
        state.candidate.as_mut().unwrap().attempts = 2;
        assert_eq!(
            evaluate_watchdog(&state, None, 2004, true),
            WatchdogDecision::Rollback {
                bad_version: "0.3.0".into(),
                fallback_version: "0.2.0".into()
            }
        );
        apply_watchdog_rollback(&mut state, "0.3.0", "0.2.0");
        assert_eq!(state.active_version, "0.2.0");
        assert!(state.candidate.is_none());
        assert!(state.blacklisted_versions.contains(&"0.3.0".to_string()));
    }

    #[test]
    fn prune_old_versions_keeps_only_active_and_previous_good() {
        let root = temp_dir("prune");
        let versions = root.join("versions");
        for v in ["0.1.0", "0.2.0", "0.3.0"] {
            let p = versions.join(v).join(CORE_EXE_NAME);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"bin").unwrap();
        }
        let state = InstallState {
            base_version: "1.0.0".into(),
            active_version: "0.3.0".into(),
            previous_good_version: Some("0.2.0".into()),
            candidate: None,
            blacklisted_versions: vec![],
            last_check_at: 0,
            installed_at: 1,
        };
        let removed = prune_old_versions(&versions, &state).unwrap();
        assert_eq!(removed, vec!["0.1.0".to_string()]);
        assert!(!versions.join("0.1.0").exists());
        assert!(versions.join("0.2.0").exists());
        assert!(versions.join("0.3.0").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn service_interactive_sddl_is_valid() {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{BOOL, HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{GetSecurityDescriptorDacl, ACL, PSECURITY_DESCRIPTOR};

        let sddl_w: Vec<u16> = SERVICE_INTERACTIVE_CONTROL_SDDL
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl_w.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .expect("SDDL should parse");
            let mut present = BOOL::default();
            let mut defaulted = BOOL::default();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted)
                .expect("GetSecurityDescriptorDacl should succeed");
            assert!(present.as_bool());
            assert!(!dacl.is_null());
            let _ = LocalFree(HLOCAL(sd.0));
        }
    }

    #[test]
    fn app_icon_rgba_dimensions_and_alpha() {
        let rgba = build_app_icon_rgba(32);
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert_eq!(rgba[3], 0);
        let center_alpha = rgba[(16 * 32 + 16) * 4 + 3];
        assert_eq!(center_alpha, 255);
    }
}
