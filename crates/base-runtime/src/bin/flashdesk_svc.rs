#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use base_common::{
    apply_watchdog_promote, apply_watchdog_rollback, core_exe_for_version, ensure_ipc_dir_writable,
    evaluate_switch_request, evaluate_watchdog, prune_old_versions, read_json_opt,
    replace_file_atomic, resolve_launch_core_exe, unix_now, write_json_atomic, AgentStatus,
    CandidateState, InstallState, SwitchDecision, SwitchRequest, UpdatePhase,
    UpdateStatusSnapshot, WatchdogDecision, AGENT_STATUS_PATH, BASE_VERSION, SHARED_CONFIG_PATH,
    SERVICE_NAME, STATE_FILE_NAME, SWITCH_REQUEST_REL, UPDATE_IPC_DIR, UPDATE_STATUS_PATH,
};
use base_runtime::{log_error, log_info, log_warn, svc_log};
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR, PWSTR};
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
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
};

static CONFIG_PATH_CELL: OnceLock<PathBuf> = OnceLock::new();
static STOP_FLAG: AtomicBool = AtomicBool::new(false);
static SESSION_GEN: AtomicU32 = AtomicU32::new(0);
static STATUS_HANDLE_PTR: Mutex<Option<isize>> = Mutex::new(None);

fn to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn parse_config_arg() -> PathBuf {
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            if let Some(val) = args.next() {
                return PathBuf::from(val);
            }
        }
    }
    PathBuf::from(SHARED_CONFIG_PATH)
}

fn report_status(state: u32, controls_accepted: u32, wait_hint: u32) {
    let Some(raw) = *STATUS_HANDLE_PTR.lock().unwrap() else {
        return;
    };
    let handle = SERVICE_STATUS_HANDLE(raw as *mut c_void);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_STATUS_CURRENT_STATE(state),
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

    report_status(SERVICE_START_PENDING.0, 0, 3000);
    report_status(
        SERVICE_RUNNING.0,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE,
        0,
    );

    let cfg_path = CONFIG_PATH_CELL
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(SHARED_CONFIG_PATH));

    let Ok(svc_exe) = std::env::current_exe() else {
        report_status(SERVICE_STOPPED.0, 0, 0);
        return;
    };

    let install_dir = svc_exe
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    svc_log::init(&install_dir.join("logs").join("base-service.log"));
    log_info!(
        "flashdesk-svc v{} started: install_dir={}, config={}",
        BASE_VERSION,
        install_dir.display(),
        cfg_path.display()
    );

    let _ = ensure_ipc_dir_writable(Path::new(UPDATE_IPC_DIR));
    supervisor_loop(&install_dir, &cfg_path);

    log_info!("flashdesk-svc stopping");
    report_status(SERVICE_STOPPED.0, 0, 0);
}

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
    GetLastError() != ERROR_NOT_ALL_ASSIGNED
}

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
            log_error!("OpenProcessToken for privilege adjustment failed");
            return;
        }
        for (name, label) in [
            (w!("SeTcbPrivilege"), "SeTcbPrivilege"),
            (w!("SeAssignPrimaryTokenPrivilege"), "SeAssignPrimaryTokenPrivilege"),
            (w!("SeIncreaseQuotaPrivilege"), "SeIncreaseQuotaPrivilege"),
            (w!("SeDebugPrivilege"), "SeDebugPrivilege"),
        ] {
            if enable_privilege(token, name) {
                log_info!("enabled service privilege: {label}");
            } else {
                log_error!("cannot enable privilege {label}; winlogon fallback may be needed");
            }
        }
        let _ = CloseHandle(token);
    }
}

fn load_or_recover_state(install_dir: &Path) -> InstallState {
    let state_path = install_dir.join(STATE_FILE_NAME);
    if let Some(mut st) = read_json_opt::<InstallState>(&state_path) {
        if st.base_version != BASE_VERSION {
            st.base_version = BASE_VERSION.to_string();
            let _ = write_json_atomic(&state_path, &st);
        }
        return st;
    }
    let active = resolve_launch_core_exe(install_dir)
        .map(|(v, _)| v)
        .unwrap_or_else(|| "0.1.0".to_string());
    let st = InstallState {
        base_version: BASE_VERSION.to_string(),
        active_version: active,
        previous_good_version: None,
        candidate: None,
        blacklisted_versions: Vec::new(),
        last_check_at: 0,
        installed_at: unix_now(),
    };
    let _ = write_json_atomic(&state_path, &st);
    st
}

fn publish_update_status(
    phase: UpdatePhase,
    current_version: &str,
    available_version: Option<String>,
    last_check_at: u64,
    message: Option<String>,
) {
    let prev = read_json_opt::<UpdateStatusSnapshot>(Path::new(UPDATE_STATUS_PATH));
    let snap = UpdateStatusSnapshot {
        state: phase,
        current_version: current_version.to_string(),
        base_version: BASE_VERSION.to_string(),
        available_version,
        notes: prev.as_ref().and_then(|p| p.notes.clone()),
        progress_pct: None,
        last_check_at: last_check_at.max(prev.as_ref().map_or(0, |p| p.last_check_at)),
        updated_at: unix_now(),
        message,
    };
    let _ = write_json_atomic(Path::new(UPDATE_STATUS_PATH), &snap);
}

fn terminate_child(child_handle: &mut Option<HANDLE>, child_session_id: &mut u32) {
    if let Some(h) = child_handle.take() {
        unsafe {
            let _ = TerminateProcess(h, 0);
            let _ = WaitForSingleObject(h, 3000);
            let _ = CloseHandle(h);
        }
    }
    *child_session_id = u32::MAX;
}

fn supervisor_loop(install_dir: &Path, cfg_path: &Path) {
    enable_service_privileges();
    let _ = std::fs::create_dir_all(install_dir.join("versions"));
    let _ = std::fs::create_dir_all(install_dir.join("updates").join("cache"));
    let _ = std::fs::create_dir_all(install_dir.join("updates").join("staging"));

    let state_path = install_dir.join(STATE_FILE_NAME);
    let switch_path = install_dir.join(SWITCH_REQUEST_REL);

    let mut state = load_or_recover_state(install_dir);
    if let Some(cand) = &mut state.candidate {
        // Give any in-flight candidate a fresh watchdog window across service restarts.
        cand.started_at = unix_now();
        let _ = write_json_atomic(&state_path, &state);
    }

    let mut child_handle: Option<HANDLE> = None;
    let mut child_session_id: u32 = u32::MAX;
    let mut child_version = String::new();
    let mut child_spawned_at = Instant::now();
    let mut spawn_failures: u32 = 0;
    let mut crash_count: u32 = 0;
    let mut idle_since: Option<Instant> = None;
    let mut waiting_switch_logged: Option<String> = None;

    while !STOP_FLAG.load(Ordering::SeqCst) {
        let now = unix_now();
        let active_session = unsafe { WTSGetActiveConsoleSessionId() };
        let agent_status = read_json_opt::<AgentStatus>(Path::new(AGENT_STATUS_PATH));

        // Update idle timer based on active remote sessions reported by agent.
        let busy = agent_status
            .as_ref()
            .is_some_and(|st| st.is_fresh_at(now) && st.active_sessions > 0);
        if busy {
            idle_since = None;
        } else if idle_since.is_none() {
            idle_since = Some(Instant::now());
        }
        let idle_secs = idle_since.map_or(0, |t| t.elapsed().as_secs());

        // Check if current child exited.
        let mut child_exited = false;
        if let Some(h) = child_handle {
            let wait_res = unsafe { WaitForSingleObject(h, 0) };
            if wait_res.0 == 0 {
                let mut exit_code = 0u32;
                unsafe {
                    let _ = GetExitCodeProcess(h, &mut exit_code);
                    let _ = CloseHandle(h);
                }
                let uptime = child_spawned_at.elapsed();
                log_warn!(
                    "Session {} agent (v{}) exited (code={}, uptime={:.1}s)",
                    child_session_id,
                    child_version,
                    exit_code,
                    uptime.as_secs_f64()
                );
                if uptime < Duration::from_secs(3) {
                    crash_count = crash_count.saturating_add(1);
                } else {
                    crash_count = 0;
                }
                child_handle = None;
                child_session_id = u32::MAX;
                child_exited = true;
            }
        }

        // Reload state if core updated non-base fields (e.g. last_check_at).
        if let Some(disk_state) = read_json_opt::<InstallState>(&state_path) {
            state.last_check_at = state.last_check_at.max(disk_state.last_check_at);
        }

        // 1. Evaluate 15s watchdog for any pending candidate version.
        if state.candidate.is_some() {
            match evaluate_watchdog(&state, agent_status.as_ref(), now, child_exited) {
                WatchdogDecision::NoCandidate | WatchdogDecision::Waiting => {}
                WatchdogDecision::Promote { version } => {
                    log_info!("candidate v{version} passed 15s health watchdog; promoting");
                    apply_watchdog_promote(&mut state, &version);
                    let _ = write_json_atomic(&state_path, &state);
                    let _ = prune_old_versions(&install_dir.join("versions"), &state);
                    publish_update_status(
                        UpdatePhase::SwitchedRestartGui,
                        &version,
                        Some(version.clone()),
                        state.last_check_at,
                        Some(format!("后台服务已切换至 v{version}，可重启界面生效")),
                    );
                }
                WatchdogDecision::RetryCandidate { attempt } => {
                    if let Some(cand) = &mut state.candidate {
                        cand.attempts = attempt;
                        cand.started_at = unix_now();
                        log_warn!(
                            "candidate v{} exited early; retrying (attempt {attempt}/2)",
                            cand.version
                        );
                        let _ = write_json_atomic(&state_path, &state);
                    }
                }
                WatchdogDecision::Rollback {
                    bad_version,
                    fallback_version,
                } => {
                    log_error!(
                        "candidate v{bad_version} failed twice within 15s; rolling back to v{fallback_version}"
                    );
                    terminate_child(&mut child_handle, &mut child_session_id);
                    apply_watchdog_rollback(&mut state, &bad_version, &fallback_version);
                    let _ = write_json_atomic(&state_path, &state);
                    let bad_dir = install_dir.join("versions").join(&bad_version);
                    let _ = std::fs::remove_dir_all(&bad_dir);
                    publish_update_status(
                        UpdatePhase::RolledBack {
                            bad_version: bad_version.clone(),
                        },
                        &fallback_version,
                        None,
                        state.last_check_at,
                        Some(format!(
                            "新版本 v{bad_version} 启动异常，已自动回滚至 v{fallback_version}"
                        )),
                    );
                }
            }
        }

        // 2. Process pending switch_request.json when no candidate is currently in trial.
        if state.candidate.is_none() && switch_path.is_file() {
            match read_json_opt::<SwitchRequest>(&switch_path) {
                Some(req) => match evaluate_switch_request(install_dir, &state, &req, idle_secs) {
                    SwitchDecision::DeferActiveSession => {
                        if waiting_switch_logged.as_deref() != Some(&req.target_version) {
                            log_info!(
                                "switch_request v{} deferred until session idle >= 10s",
                                req.target_version
                            );
                            waiting_switch_logged = Some(req.target_version.clone());
                        }
                    }
                    SwitchDecision::Reject(reason) => {
                        log_error!("rejected switch_request v{}: {reason}", req.target_version);
                        let _ = std::fs::remove_file(&switch_path);
                        waiting_switch_logged = None;
                        publish_update_status(
                            UpdatePhase::Error,
                            &state.active_version,
                            None,
                            state.last_check_at,
                            Some(reason),
                        );
                    }
                    SwitchDecision::Apply { staged_abs } => {
                        waiting_switch_logged = None;
                        let target_ver = req.target_version.clone();
                        let dst_exe = core_exe_for_version(install_dir, &target_ver);
                        if let Some(parent) = dst_exe.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let tmp_dst = dst_exe.with_extension("exe.tmp");
                        let move_res = std::fs::copy(&staged_abs, &tmp_dst)
                            .and_then(|_| replace_file_atomic(&tmp_dst, &dst_exe));
                        match move_res {
                            Ok(()) => {
                                log_info!(
                                    "staged core v{target_ver} installed to {}; switching agent",
                                    dst_exe.display()
                                );
                                terminate_child(&mut child_handle, &mut child_session_id);
                                if state.active_version != target_ver {
                                    state.previous_good_version = Some(state.active_version.clone());
                                }
                                state.active_version = target_ver.clone();
                                state.candidate = Some(CandidateState {
                                    version: target_ver.clone(),
                                    attempts: 1,
                                    started_at: unix_now(),
                                });
                                let _ = write_json_atomic(&state_path, &state);
                                let _ = std::fs::remove_file(&switch_path);
                                let _ = std::fs::remove_file(&staged_abs);
                                crash_count = 0;
                                spawn_failures = 0;
                            }
                            Err(e) => {
                                log_error!("failed to deploy staged core v{target_ver}: {e}");
                                let _ = std::fs::remove_file(&tmp_dst);
                            }
                        }
                    }
                },
                None => {
                    log_error!("corrupt switch_request.json; removing");
                    let _ = std::fs::remove_file(&switch_path);
                }
            }
        }

        // 3. Ensure active core --agent is running in the active console session.
        let resolved = resolve_launch_core_exe(install_dir);
        let needs_respawn = if active_session == u32::MAX {
            false
        } else if child_handle.is_none() || child_session_id != active_session {
            true
        } else if let Some((ref target_ver, _)) = resolved {
            target_ver != &child_version
        } else {
            false
        };

        if needs_respawn {
            if let Some((ver, exe_path)) = resolved {
                terminate_child(&mut child_handle, &mut child_session_id);
                match spawn_agent_in_session(&exe_path, cfg_path, active_session) {
                    Ok(h) => {
                        log_info!(
                            "spawned Session {} agent (v{}) from {}",
                            active_session,
                            ver,
                            exe_path.display()
                        );
                        child_handle = Some(h);
                        child_session_id = active_session;
                        child_version = ver;
                        child_spawned_at = Instant::now();
                        spawn_failures = 0;
                    }
                    Err(e) => {
                        spawn_failures = spawn_failures.saturating_add(1);
                        if spawn_failures == 1 || spawn_failures % 20 == 0 {
                            log_error!(
                                "failed to spawn Session {} agent (attempt {}): {e:#}",
                                active_session,
                                spawn_failures
                            );
                        }
                    }
                }
            } else {
                spawn_failures = spawn_failures.saturating_add(1);
                if spawn_failures == 1 || spawn_failures % 20 == 0 {
                    log_error!(
                        "no flashdesk-core.exe found under {}\\versions",
                        install_dir.display()
                    );
                }
            }
        }

        let failure_delay = 500u64 * (spawn_failures.min(9) as u64);
        let crash_delay = 2000u64 * (crash_count.min(5) as u64);
        sleep_interruptible(500 + failure_delay.min(4500) + crash_delay.min(10_000));
    }

    terminate_child(&mut child_handle, &mut child_session_id);
    let _ = std::fs::remove_file(AGENT_STATUS_PATH);
}

fn sleep_interruptible(total_ms: u64) {
    let mut elapsed = 0u64;
    while elapsed < total_ms && !STOP_FLAG.load(Ordering::SeqCst) {
        let step = (total_ms - elapsed).min(100);
        std::thread::sleep(Duration::from_millis(step));
        elapsed += step;
    }
}

fn spawn_agent_in_session(exe_path: &Path, cfg_path: &Path, session_id: u32) -> Result<HANDLE> {
    match spawn_agent_via_session_token(exe_path, cfg_path, session_id) {
        Ok(h) => Ok(h),
        Err(e) => {
            log_warn!(
                "session-token spawn path failed ({e:#}); falling back to winlogon token of session {session_id}"
            );
            spawn_agent_via_winlogon_token(exe_path, cfg_path, session_id)
        }
    }
}

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
        log_info!("winlogon fallback: pid={pid} token session={tok_session} (target {session_id})");

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

    CreateProcessAsUserW(
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
    )
    .context("CreateProcessAsUserW failed")?;

    let _ = CloseHandle(pi.hThread);
    Ok(pi.hProcess)
}

fn main() -> Result<()> {
    let cfg_path = parse_config_arg();
    let _ = CONFIG_PATH_CELL.set(cfg_path);
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
