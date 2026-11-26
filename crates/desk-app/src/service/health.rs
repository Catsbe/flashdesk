//! Agent liveness heartbeat shared between the Session-N SYSTEM agent and the desktop GUI.
//!
//! The SCM service state alone is NOT proof that remote control with lockscreen/UAC support is
//! available: the Session-0 supervisor may be running while its interactive `--agent` child failed
//! to spawn (e.g. missing `SeTcbPrivilege`) or crashed. The agent therefore publishes a heartbeat
//! file every few seconds; the GUI uses it to decide whether to defer incoming sessions / the LAN
//! direct port to the agent, or to take over hosting itself (degraded, non-SYSTEM mode).

pub use base_common::{pid_alive, unix_now, write_json_atomic, AgentStatus, AGENT_STATUS_PATH};
use std::path::PathBuf;
use windows::Win32::Foundation::{CloseHandle, HANDLE};

pub fn read_agent_status() -> Option<AgentStatus> {
    let text = std::fs::read_to_string(AGENT_STATUS_PATH).ok()?;
    serde_json::from_str(&text).ok()
}

/// True only when a live agent published a heartbeat recently. This is the single source of truth
/// for "the SYSTEM agent can host sessions (lockscreen/UAC capable)".
pub fn agent_healthy() -> bool {
    let Some(st) = read_agent_status() else {
        return false;
    };
    st.is_fresh_at(unix_now()) && pid_alive(st.pid)
}

/// Atomically publishes the agent heartbeat (temp file + replace) so readers never see partial JSON.
pub fn write_agent_status(
    pid: u32,
    session_id: u32,
    elevated: bool,
    started_at: u64,
    version: &str,
    active_sessions: u32,
) -> std::io::Result<()> {
    let st = AgentStatus {
        pid,
        session_id,
        elevated,
        started_at,
        heartbeat_at: unix_now(),
        version: Some(version.to_string()),
        active_sessions,
    };
    write_json_atomic(&PathBuf::from(AGENT_STATUS_PATH), &st)
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
            version: Some("0.1.0".into()),
            active_sessions: 2,
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
