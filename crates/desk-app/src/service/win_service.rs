use crate::service::install::SERVICE_NAME;
use anyhow::{Context, Result};
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID, NO_ERROR,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, LookupPrivilegeValueW,
    SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, LUID_AND_ATTRIBUTES,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
    SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
};

static CONFIG_PATH_CELL: OnceLock<PathBuf> = OnceLock::new();
static STATUS_HANDLE: AtomicU32 = AtomicU32::new(0);
static STOP_FLAG: AtomicBool = AtomicBool::new(false);
static SESSION_GEN: AtomicU32 = AtomicU32::new(0);
static STATUS_HANDLE_PTR: std::sync::Mutex<Option<isize>> = std::sync::Mutex::new(None);

fn to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn report_status(state: u32, controls_accepted: u32, wait_hint: u32) {
    let Some(raw) = *STATUS_HANDLE_PTR.lock().unwrap() else {
        return;
    };
    let handle = SERVICE_STATUS_HANDLE(raw as *mut c_void);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE(state),
        dwControlsAccepted: controls_accepted,
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_hint,
    };
    unsafe {
        let _ = SetServiceStatus(handle, &status);
    }
}

unsafe extern "system" fn service_ctrl_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report_status(SERVICE_STOP_PENDING.0, 0, 5000);
            STOP_FLAG.store(true, Ordering::SeqCst);
            NO_ERROR.0
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            SESSION_GEN.fetch_add(1, Ordering::SeqCst);
            NO_ERROR.0
        }
        _ => NO_ERROR.0,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let svc_name_w = to_wide_null(OsStr::new(SERVICE_NAME));
    let Ok(handle) =
        RegisterServiceCtrlHandlerExW(PCWSTR(svc_name_w.as_ptr()), Some(service_ctrl_handler), None)
    else {
        return;
    };
    *STATUS_HANDLE_PTR.lock().unwrap() = Some(handle.0 as isize);
    let _ = STATUS_HANDLE.load(Ordering::Relaxed);

    report_status(SERVICE_START_PENDING.0, 0, 3000);
    report_status(
        SERVICE_RUNNING.0,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE,
        0,
    );

    let cfg_path = CONFIG_PATH_CELL
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(crate::service::install::SHARED_CONFIG_PATH));

    let Ok(exe_path) = std::env::current_exe() else {
        report_status(SERVICE_STOPPED.0, 0, 0);
        return;
    };

    supervisor_loop(&exe_path, &cfg_path);

    report_status(SERVICE_STOPPED.0, 0, 0);
}

/// Enables a single privilege in the given token. Returns false when the privilege is absent from
/// the token (e.g. stripped by domain policy) or the adjustment was denied.
unsafe fn enable_privilege(token: HANDLE, name: PCWSTR) -> bool {
    let mut luid = LUID::default();
    if LookupPrivilegeValueW(None, name, &mut luid).is_err() {
        return false;
    }
    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    if AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None).is_err() {
        return false;
    }
    // AdjustTokenPrivileges succeeds even when the privilege was NOT assigned; only GetLastError
    // distinguishes "enabled" from "missing from token".
    GetLastError() != ERROR_NOT_ALL_ASSIGNED
}

/// SCM service tokens do not guarantee `SeTcbPrivilege` in an enabled state, and
/// `SetTokenInformation(TokenSessionId)` (cross-session agent spawn) hard-requires it enabled.
/// Enable it explicitly, plus the two privileges `CreateProcessAsUserW` needs.
fn enable_service_privileges() {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            tracing::error!("OpenProcessToken for privilege adjustment failed");
            return;
        }
        for (name, label) in [
            (
                windows::core::w!("SeTcbPrivilege"),
                "SeTcbPrivilege (跨会话拉起桌面代理必需)",
            ),
            (
                windows::core::w!("SeAssignPrimaryTokenPrivilege"),
                "SeAssignPrimaryTokenPrivilege (CreateProcessAsUserW 必需)",
            ),
            (
                windows::core::w!("SeIncreaseQuotaPrivilege"),
                "SeIncreaseQuotaPrivilege (CreateProcessAsUserW 必需)",
            ),
            (
                windows::core::w!("SeDebugPrivilege"),
                "SeDebugPrivilege (winlogon 令牌回退路径必需)",
            ),
        ] {
            if enable_privilege(token, name) {
                tracing::info!("enabled service privilege: {label}");
            } else {
                tracing::error!(
                    "cannot enable privilege {label}: 令牌中缺失或被域策略移除，桌面代理可能无法拉起"
                );
            }
        }
        let _ = CloseHandle(token);
    }
}

fn supervisor_loop(exe_path: &Path, cfg_path: &Path) {
    enable_service_privileges();

    let mut child_handle: Option<HANDLE> = None;
    let mut child_session_id: u32 = u32::MAX;
    let mut child_spawned_at = std::time::Instant::now();
    let mut spawn_failures: u32 = 0;
    let mut crash_count: u32 = 0;

    while !STOP_FLAG.load(Ordering::SeqCst) {
        let active_session = unsafe { WTSGetActiveConsoleSessionId() };

        // Check if current child exited
        if let Some(h) = child_handle {
            let wait_res = unsafe { WaitForSingleObject(h, 0) };
            if wait_res.0 == 0 {
                let mut exit_code = 0u32;
                unsafe {
                    let _ = GetExitCodeProcess(h, &mut exit_code);
                }
                let uptime = child_spawned_at.elapsed();
                tracing::warn!(
                    "Session {} agent exited (code={}, uptime={:.1}s)",
                    child_session_id,
                    exit_code,
                    uptime.as_secs_f64()
                );
                if uptime < std::time::Duration::from_secs(3) {
                    crash_count = crash_count.saturating_add(1);
                } else {
                    crash_count = 0;
                }
                unsafe {
                    let _ = CloseHandle(h);
                }
                child_handle = None;
                child_session_id = u32::MAX;
            }
        }

        // Only restart child if active console session changed to a valid console session
        if active_session != u32::MAX && active_session != child_session_id {
            if let Some(h) = child_handle.take() {
                unsafe {
                    let _ = TerminateProcess(h, 0);
                    let _ = WaitForSingleObject(h, 2000);
                    let _ = CloseHandle(h);
                }
            }
            match spawn_agent_in_session(exe_path, cfg_path, active_session) {
                Ok(h) => {
                    tracing::info!("spawned Session {} agent process", active_session);
                    child_handle = Some(h);
                    child_session_id = active_session;
                    child_spawned_at = std::time::Instant::now();
                    spawn_failures = 0;
                }
                Err(e) => {
                    spawn_failures = spawn_failures.saturating_add(1);
                    // Log the first failure and then at most every ~10s to avoid flooding the log.
                    if spawn_failures == 1 || spawn_failures % 20 == 0 {
                        tracing::error!(
                            "failed to spawn Session {} agent (attempt {}): {e:#}",
                            active_session,
                            spawn_failures
                        );
                    }
                }
            }
        }

        // Backoff: spawn failures grow the poll interval up to ~5s; agent crash-loops up to ~10s.
        let failure_delay = 500u64 * (spawn_failures.min(9) as u64);
        let crash_delay = 2000u64 * (crash_count.min(5) as u64);
        std::thread::sleep(std::time::Duration::from_millis(
            500 + failure_delay.min(4500) + crash_delay.min(10_000),
        ));
    }

    if let Some(h) = child_handle.take() {
        unsafe {
            let _ = TerminateProcess(h, 0);
            let _ = WaitForSingleObject(h, 2000);
            let _ = CloseHandle(h);
        }
    }
}

fn spawn_agent_in_session(exe_path: &Path, cfg_path: &Path, session_id: u32) -> Result<HANDLE> {
    match spawn_agent_via_session_token(exe_path, cfg_path, session_id) {
        Ok(h) => Ok(h),
        Err(e) => {
            tracing::warn!(
                "session-token spawn path failed ({e:#}); falling back to winlogon token of session {session_id}"
            );
            spawn_agent_via_winlogon_token(exe_path, cfg_path, session_id)
        }
    }
}

/// Standard path: duplicate our own SYSTEM primary token and move it into the target session.
fn spawn_agent_via_session_token(exe_path: &Path, cfg_path: &Path, session_id: u32) -> Result<HANDLE> {
    unsafe {
        let mut self_token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_SESSIONID,
            &mut self_token,
        )
        .context("OpenProcessToken failed")?;

        let mut dup_token = HANDLE::default();
        let dup_res = DuplicateTokenEx(
            self_token,
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_SESSIONID,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup_token,
        );
        let _ = CloseHandle(self_token);
        dup_res.context("DuplicateTokenEx failed")?;

        let set_res = SetTokenInformation(
            dup_token,
            TokenSessionId,
            &session_id as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        );
        if set_res.is_err() {
            let gle = GetLastError();
            let _ = CloseHandle(dup_token);
            anyhow::bail!(
                "SetTokenInformation(TokenSessionId) failed: {:?} (GetLastError={})",
                set_res,
                gle.0
            );
        }

        let h = create_agent_process(dup_token, exe_path, cfg_path);
        let _ = CloseHandle(dup_token);
        h
    }
}

/// Fallback path for machines where `NtSetInformationToken(TokenSessionId)` is denied even with
/// `SeTcbPrivilege` enabled (hardened/EDR-hooked kernels): borrow the primary SYSTEM token of
/// `winlogon.exe` living in the target session — it already carries the right session id, so no
/// session adjustment is needed at all.
fn spawn_agent_via_winlogon_token(exe_path: &Path, cfg_path: &Path, session_id: u32) -> Result<HANDLE> {
    unsafe {
        let pid = find_winlogon_pid(session_id)
            .with_context(|| format!("no winlogon.exe found in session {session_id}"))?;
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .context("OpenProcess(winlogon) failed")?;
        let mut tok = HANDLE::default();
        let open_res = OpenProcessToken(
            proc,
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY,
            &mut tok,
        );
        let _ = CloseHandle(proc);
        open_res.context("OpenProcessToken(winlogon) failed")?;

        let mut dup_token = HANDLE::default();
        let dup_res = DuplicateTokenEx(
            tok,
            TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_SESSIONID,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut dup_token,
        );
        let _ = CloseHandle(tok);
        dup_res.context("DuplicateTokenEx(winlogon) failed")?;

        let mut tok_session = u32::MAX;
        let mut ret_len = 0u32;
        let _ = GetTokenInformation(
            dup_token,
            TokenSessionId,
            Some(&mut tok_session as *mut u32 as *mut c_void),
            std::mem::size_of::<u32>() as u32,
            &mut ret_len,
        );
        tracing::info!(
            "winlogon fallback: pid={pid} token session={tok_session} (target {session_id})"
        );

        let h = create_agent_process(dup_token, exe_path, cfg_path);
        let _ = CloseHandle(dup_token);
        h
    }
}

fn find_winlogon_pid(session_id: u32) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if name.eq_ignore_ascii_case("winlogon.exe") {
                    let mut sid = u32::MAX;
                    if ProcessIdToSessionId(entry.th32ProcessID, &mut sid).is_ok()
                        && sid == session_id
                    {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// Shared CreateProcessAsUserW tail: launches `--agent` on the interactive default desktop.
unsafe fn create_agent_process(token: HANDLE, exe_path: &Path, cfg_path: &Path) -> Result<HANDLE> {
    let mut desktop_w = to_wide_null(OsStr::new(r"winsta0\default"));
    let cmd = format!(
        "\"{}\" --agent --config \"{}\"",
        exe_path.display(),
        cfg_path.display()
    );
    let mut cmd_w = to_wide_null(OsStr::new(&cmd));

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop_w.as_mut_ptr()),
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    let create_res = CreateProcessAsUserW(
        token,
        None,
        PWSTR(cmd_w.as_mut_ptr()),
        None,
        None,
        false,
        CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
        None,
        None,
        &si,
        &mut pi,
    );
    create_res.context("CreateProcessAsUserW failed")?;

    let _ = CloseHandle(pi.hThread);
    Ok(pi.hProcess)
}

pub fn run_windows_service(config_path: PathBuf) -> Result<()> {
    let _ = CONFIG_PATH_CELL.set(config_path);
    let mut svc_name_w = to_wide_null(OsStr::new(SERVICE_NAME));
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(svc_name_w.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::null(),
            lpServiceProc: None,
        },
    ];
    unsafe {
        StartServiceCtrlDispatcherW(table.as_ptr())
            .context("StartServiceCtrlDispatcherW 失败（仅可由 Windows 服务管理器启动）")?;
    }
    Ok(())
}
