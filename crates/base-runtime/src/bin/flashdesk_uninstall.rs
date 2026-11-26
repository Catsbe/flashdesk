#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use base_common::{SHARED_PROGRAM_DATA_DIR, SERVICE_NAME};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SUCCESS, HANDLE, HWND};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    RegDeleteTreeW, RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, DeleteService, OpenSCManagerW, OpenServiceW,
    QueryServiceConfigW, QueryServiceStatusEx, QUERY_SERVICE_CONFIGW, SC_HANDLE,
    SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_ALL_ACCESS,
    SERVICE_CONTROL_STOP, SERVICE_QUERY_CONFIG, SERVICE_STATUS, SERVICE_STATUS_PROCESS,
    SERVICE_STOPPED,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows::Win32::UI::Shell::{
    SHGetFolderPathW, ShellExecuteW, CSIDL_COMMON_DESKTOPDIRECTORY, CSIDL_COMMON_PROGRAMS,
    CSIDL_DESKTOPDIRECTORY, CSIDL_PROGRAMS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK,
    MB_YESNO, SW_HIDE,
};

struct Options {
    silent: bool,
    purge_data: bool,
    elevated: bool,
    dir: Option<PathBuf>,
}

fn parse_args() -> Options {
    let mut opts = Options {
        silent: false,
        purge_data: false,
        elevated: false,
        dir: None,
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        if a == "--silent" {
            opts.silent = true;
        } else if a == "--purge-data" {
            opts.purge_data = true;
        } else if a == "--elevated-uninstall" {
            opts.elevated = true;
        } else if a == "--dir" {
            if let Some(v) = args.next() {
                opts.dir = Some(PathBuf::from(v));
            }
        }
    }
    opts
}

fn to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn msgbox(title: &str, text: &str, flags: u32) -> i32 {
    let t: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let m: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(m.as_ptr()),
            PCWSTR(t.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_STYLE(flags),
        )
        .0
    }
}

fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

struct ScGuard(SC_HANDLE);
impl Drop for ScGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseServiceHandle(self.0);
            }
        }
    }
}

fn service_belongs_to_dir(install_dir: &Path) -> bool {
    let norm_dir = install_dir
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase();
    if norm_dir.is_empty() {
        return false;
    }
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else {
            return false;
        };
        let _g = ScGuard(scm);
        let svc_w = to_wide_null(OsStr::new(SERVICE_NAME));
        let Ok(svc) = OpenServiceW(scm, PCWSTR(svc_w.as_ptr()), SERVICE_QUERY_CONFIG) else {
            return false;
        };
        let _sg = ScGuard(svc);
        let mut needed = 0u32;
        let _ = QueryServiceConfigW(svc, None, 0, &mut needed);
        if needed == 0 {
            return false;
        }
        let mut buf = vec![0u8; needed as usize];
        let cfg_ptr = buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        if QueryServiceConfigW(svc, Some(cfg_ptr), needed, &mut needed).is_err() {
            return false;
        }
        let bin_pwstr = (*cfg_ptr).lpBinaryPathName;
        if bin_pwstr.is_null() {
            return false;
        }
        let bin_path = bin_pwstr.to_string().unwrap_or_default().to_ascii_lowercase();
        bin_path.contains(&norm_dir)
    }
}

fn stop_and_delete_service() -> Result<()> {
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
            .context("打开服务管理器失败，需要管理员权限")?;
        let _g = ScGuard(scm);
        let svc_w = to_wide_null(OsStr::new(SERVICE_NAME));
        match OpenServiceW(scm, PCWSTR(svc_w.as_ptr()), SERVICE_ALL_ACCESS) {
            Ok(svc) => {
                let _sg = ScGuard(svc);
                let mut status = SERVICE_STATUS::default();
                let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut status);
                for _ in 0..30 {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    let mut buf = [0u8; std::mem::size_of::<SERVICE_STATUS_PROCESS>()];
                    let mut needed = 0u32;
                    if QueryServiceStatusEx(
                        svc,
                        SC_STATUS_PROCESS_INFO,
                        Some(&mut buf),
                        &mut needed,
                    )
                    .is_ok()
                    {
                        let st = &*(buf.as_ptr() as *const SERVICE_STATUS_PROCESS);
                        if st.dwCurrentState == SERVICE_STOPPED {
                            break;
                        }
                    }
                }
                DeleteService(svc).context("DeleteService 失败")?;
            }
            Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn dir_is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".uninstall-probe-{}", std::process::id()));
    if std::fs::write(&probe, b"ok").is_ok() {
        let _ = std::fs::remove_file(&probe);
        true
    } else {
        false
    }
}

fn terminate_processes_in_dir(install_dir: &Path) {
    let prefix = install_dir.to_string_lossy().to_ascii_lowercase();
    let self_pid = std::process::id();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let pid = entry.th32ProcessID;
                if pid != 0 && pid != self_pid {
                    if let Ok(proc) = OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                        false,
                        pid,
                    ) {
                        let mut buf = [0u16; 1024];
                        let mut len = buf.len() as u32;
                        if QueryFullProcessImageNameW(
                            proc,
                            PROCESS_NAME_WIN32,
                            windows::core::PWSTR(buf.as_mut_ptr()),
                            &mut len,
                        )
                        .is_ok()
                        {
                            let path_str =
                                String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase();
                            if path_str.starts_with(&prefix) {
                                let _ = TerminateProcess(proc, 0);
                                let _ = WaitForSingleObject(proc, 2000);
                            }
                        }
                        let _ = CloseHandle(proc);
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
}

fn folder_path(csidl: u32) -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    let ok = unsafe { SHGetFolderPathW(HWND::default(), csidl as i32, HANDLE::default(), 0, &mut buf) };
    if ok.is_ok() {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
    } else {
        None
    }
}

fn remove_shortcuts() {
    for csidl in [CSIDL_COMMON_DESKTOPDIRECTORY, CSIDL_DESKTOPDIRECTORY] {
        if let Some(dir) = folder_path(csidl) {
            let _ = std::fs::remove_file(dir.join("FlashDesk.lnk"));
        }
    }
    for csidl in [CSIDL_COMMON_PROGRAMS, CSIDL_PROGRAMS] {
        if let Some(dir) = folder_path(csidl) {
            let _ = std::fs::remove_dir_all(dir.join("FlashDesk"));
        }
    }
}

fn read_uninstall_install_location(root: HKEY) -> Option<String> {
    let subkey = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\FlashDesk");
    let mut byte_len = 0u32;
    let rc = unsafe {
        RegGetValueW(
            root,
            subkey,
            w!("InstallLocation"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut byte_len),
        )
    };
    if rc != ERROR_SUCCESS || byte_len < 2 {
        return None;
    }
    let mut wide = vec![0u16; (byte_len as usize) / 2 + 1];
    let rc2 = unsafe {
        RegGetValueW(
            root,
            subkey,
            w!("InstallLocation"),
            RRF_RT_REG_SZ,
            None,
            Some(wide.as_mut_ptr().cast()),
            Some(&mut byte_len),
        )
    };
    if rc2 != ERROR_SUCCESS {
        return None;
    }
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    Some(String::from_utf16_lossy(&wide[..end]))
}

fn remove_uninstall_registry(install_dir: &Path, force: bool) -> bool {
    let norm_dir = install_dir
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase();
    let key = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\FlashDesk");
    let mut removed_any = false;
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let matches_dir = read_uninstall_install_location(root).is_some_and(|loc| {
            loc.trim_end_matches(['\\', '/'])
                .to_ascii_lowercase()
                == norm_dir
        });
        if force || matches_dir {
            unsafe {
                if RegDeleteTreeW(root, key) == ERROR_SUCCESS {
                    removed_any = true;
                }
            }
        }
    }
    removed_any
}

fn remove_contents_except_self(install_dir: &Path, self_exe: &Path) {
    let self_name = self_exe.file_name().map(|n| n.to_os_string());
    if let Ok(rd) = std::fs::read_dir(install_dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().eq_ignore_ascii_case("bin") && p.is_dir() {
                if let Ok(brd) = std::fs::read_dir(&p) {
                    for bentry in brd.flatten() {
                        if self_name.as_deref() == Some(bentry.file_name().as_os_str()) {
                            continue;
                        }
                        let bp = bentry.path();
                        if bp.is_dir() {
                            let _ = std::fs::remove_dir_all(&bp);
                        } else {
                            let _ = std::fs::remove_file(&bp);
                        }
                    }
                }
            } else if p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

fn schedule_dir_removal(install_dir: &Path) {
    let temp = std::env::temp_dir();
    let _ = std::env::set_current_dir(&temp);
    let dir_str = install_dir.display();
    let raw = format!(
        "/C \"ping 127.0.0.1 -n 3 >nul & rmdir /s /q \"{dir_str}\" & if exist \"{dir_str}\" (ping 127.0.0.1 -n 3 >nul & rmdir /s /q \"{dir_str}\")\""
    );
    let _ = Command::new("cmd.exe")
        .raw_arg(&raw)
        .current_dir(&temp)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn();
}

fn main() {
    let opts = parse_args();
    let Ok(self_exe) = std::env::current_exe() else {
        return;
    };
    let install_dir = opts.dir.clone().unwrap_or_else(|| {
        self_exe
            .parent()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    });

    if install_dir.parent().is_none() {
        if !opts.silent {
            msgbox("FlashDesk 卸载", "拒绝卸载根目录。", MB_OK.0 | MB_ICONERROR.0);
        }
        std::process::exit(1);
    }

    if !opts.silent && !opts.elevated {
        let answer = msgbox(
            "卸载 FlashDesk",
            &format!("确定要从以下目录卸载 FlashDesk 吗？\n\n{}", install_dir.display()),
            MB_YESNO.0 | MB_ICONQUESTION.0,
        );
        if answer != IDYES.0 {
            return;
        }
    }

    let mut purge_data = opts.purge_data;
    if !opts.silent && !opts.elevated {
        let ans = msgbox(
            "保留用户配置与根密钥？",
            "是否同时删除个人配置、根密钥 (root.key) 和服务配置？\n\n选择「否」将保留身份与节点设置，重装后无需重新配对。",
            MB_YESNO.0 | MB_ICONQUESTION.0 | MB_DEFBUTTON2.0,
        );
        purge_data = ans == IDYES.0;
    }

    let owns_service = service_belongs_to_dir(&install_dir);
    let needs_admin = owns_service || !dir_is_writable(&install_dir);
    if needs_admin && !is_elevated() && !opts.elevated {
        let mut arg_str = format!("--elevated-uninstall --dir \"{}\"", install_dir.display());
        if opts.silent {
            arg_str.push_str(" --silent");
        }
        if purge_data {
            arg_str.push_str(" --purge-data");
        }
        let exe_w = to_wide_null(self_exe.as_os_str());
        let args_w = to_wide_null(OsStr::new(&arg_str));
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
        if (ret.0 as isize) <= 32 && !opts.silent {
            msgbox(
                "FlashDesk 卸载",
                "需要管理员权限才能停止系统服务并清理安装目录。",
                MB_OK.0 | MB_ICONERROR.0,
            );
        }
        return;
    }

    if owns_service {
        if let Err(e) = stop_and_delete_service() {
            if !opts.silent {
                msgbox(
                    "FlashDesk 卸载",
                    &format!("停止系统服务失败：{e:#}"),
                    MB_OK.0 | MB_ICONERROR.0,
                );
            }
            std::process::exit(1);
        }
    }

    terminate_processes_in_dir(&install_dir);
    let _ = remove_uninstall_registry(&install_dir, !opts.silent || owns_service);
    if !opts.silent || owns_service {
        remove_shortcuts();
    }

    if purge_data {
        if let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
            let _ = std::fs::remove_dir_all(home.join(".flashdesk"));
        }
        let _ = std::fs::remove_dir_all(SHARED_PROGRAM_DATA_DIR);
    }

    remove_contents_except_self(&install_dir, &self_exe);
    schedule_dir_removal(&install_dir);

    if !opts.silent {
        msgbox(
            "FlashDesk 卸载完成",
            "FlashDesk 已成功卸载。",
            MB_OK.0 | MB_ICONINFORMATION.0,
        );
    }
}
