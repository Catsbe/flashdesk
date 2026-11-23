//! Agent liveness heartbeat shared between the Session-N SYSTEM agent and the desktop GUI.
//!
//! The SCM service state alone is NOT proof that remote control with lockscreen/UAC support is
//! available: the Session-0 supervisor may be running while its interactive `--agent` child failed
//! to spawn (e.g. missing `SeTcbPrivilege`) or crashed. The agent therefore publishes a heartbeat
//! file every few seconds; the GUI uses it to decide whether to defer incoming sessions / the LAN
//! direct port to the agent, or to take over hosting itself (degraded, non-SYSTEM mode).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};

pub const AGENT_STATUS_PATH: &str = r"C:\ProgramData\FlashDesk\agent.status.json";

/// Agent writes every 3s; tolerate up to 4 missed beats before declaring it dead.
const HEARTBEAT_TIMEOUT_SECS: u64 = 12;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub pid: u32,
    pub session_id: u32,
    /// True when the agent token is elevated (SYSTEM service agent).
    pub elevated: bool,
    pub started_at: u64,
    pub heartbeat_at: u64,
}

impl AgentStatus {
    pub fn is_fresh_at(&self, now: u64) -> bool {
        now >= self.heartbeat_at && now - self.heartbeat_at <= HEARTBEAT_TIMEOUT_SECS
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn read_agent_status() -> Option<AgentStatus> {
    let text = std::fs::read_to_string(AGENT_STATUS_PATH).ok()?;
    serde_json::from_str(&text).ok()
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Snapshot enumeration instead of OpenProcess: agents spawned from a borrowed winlogon token
    // inherit its restrictive DACL, so a standard-user GUI cannot OpenProcess them (access denied)
    // even with PROCESS_QUERY_LIMITED_INFORMATION. ToolHelp snapshots need no per-process rights.
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

/// True only when a live agent published a heartbeat recently. This is the single source of truth
/// for "the SYSTEM agent can host sessions (lockscreen/UAC capable)".
pub fn agent_healthy() -> bool {
    let Some(st) = read_agent_status() else {
        return false;
    };
    st.is_fresh_at(unix_now()) && pid_alive(st.pid)
}

fn to_wide_null(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Atomically publishes the agent heartbeat (temp file + replace) so readers never see partial JSON.
pub fn write_agent_status(
    pid: u32,
    session_id: u32,
    elevated: bool,
    started_at: u64,
) -> std::io::Result<()> {
    let st = AgentStatus {
        pid,
        session_id,
        elevated,
        started_at,
        heartbeat_at: unix_now(),
    };
    let path = PathBuf::from(AGENT_STATUS_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name("agent.status.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&st)?)?;
    let tmp_w = to_wide_null(tmp.as_os_str());
    let dst_w = to_wide_null(path.as_os_str());
    let moved = unsafe {
        MoveFileExW(
            PCWSTR(tmp_w.as_ptr()),
            PCWSTR(dst_w.as_ptr()),
            MOVEFILE_REPLACE_EXISTING,
        )
    };
    if moved.is_err() {
        // Fallback for exotic filesystems: remove + rename (tiny race, acceptable).
        let _ = std::fs::remove_file(&path);
        std::fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Whether the current process token is elevated (SYSTEM / admin). Best effort.
pub fn current_process_elevated() -> bool {
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut TOKEN_ELEVATION as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

/// Session id of the current process (the agent runs inside the interactive console session).
pub fn current_session_id() -> u32 {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut sid = u32::MAX;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut sid);
    }
    sid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(heartbeat_at: u64) -> AgentStatus {
        AgentStatus {
            pid: 1,
            session_id: 1,
            elevated: true,
            started_at: 0,
            heartbeat_at,
        }
    }

    #[test]
    fn heartbeat_freshness_window() {
        let st = status(1000);
        assert!(st.is_fresh_at(1000));
        assert!(st.is_fresh_at(1012));
        assert!(!st.is_fresh_at(1013));
        assert!(!st.is_fresh_at(999)); // clock went backwards -> treat as stale
    }
}
