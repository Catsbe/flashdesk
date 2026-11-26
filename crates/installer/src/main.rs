#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use base_common::setup_payload::{read_self_extracting_setup, ExtractedSetupPayload};
use base_common::{
    core_exe_for_version, ensure_ipc_dir_writable, read_json_opt, replace_file_atomic, unix_now,
    write_json_atomic, InstallState, LAUNCHER_EXE_NAME, SERVICE_DISPLAY_NAME,
    SERVICE_NAME, SHARED_CONFIG_PATH, SHARED_PROGRAM_DATA_DIR, STATE_FILE_NAME, SVC_EXE_NAME,
    UNINSTALL_EXE_NAME, UPDATE_IPC_DIR,
};
use clap::Parser;
use eframe::egui::{
    self, vec2, Align, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame,
    Layout, Margin, RichText, Stroke, TextStyle, Theme,
};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use windows::core::{w, Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_SERVICE_ALREADY_RUNNING, HANDLE, HWND};
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, StartServiceW, SC_ACTION,
    SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_STATUS_PROCESS_INFO,
    SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP,
    SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_STATUS, SERVICE_STATUS_PROCESS,
    SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject, INFINITE,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows::Win32::UI::Shell::{
    IShellLinkW, SHChangeNotify, SHGetFolderPathW, ShellExecuteExW, ShellLink,
    CSIDL_COMMON_DESKTOPDIRECTORY, CSIDL_COMMON_PROGRAMS, CSIDL_DESKTOPDIRECTORY, CSIDL_PROGRAMS,
    SEE_MASK_NOCLOSEPROCESS, SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, SW_HIDE};

#[derive(Parser)]
#[command(name = "FlashDesk-Setup", about = "FlashDesk Windows 安装程序")]
struct Cli {
    #[arg(long)]
    silent: bool,
    #[arg(long)]
    elevated_install: bool,
    #[arg(long)]
    dir: Option<PathBuf>,
    #[arg(long)]
    no_service: bool,
    #[arg(long)]
    no_shortcuts: bool,
    #[arg(long)]
    launch: bool,
    #[arg(long)]
    source_config: Option<PathBuf>,
}

#[derive(Clone)]
struct InstallConfig {
    install_dir: PathBuf,
    install_service: bool,
    create_shortcuts: bool,
    source_config: Option<PathBuf>,
}

fn default_install_dir() -> PathBuf {
    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("FlashDesk")
}

fn default_user_config_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|p| p.join(".flashdesk").join("config.json"))
        .filter(|p| p.is_file())
}

fn attach_parent_console_if_needed() {
    if std::env::args_os().len() <= 1 {
        return;
    }
    let has_stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
        .map(|h| !h.is_invalid() && !h.0.is_null())
        .unwrap_or(false);
    if !has_stdout {
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

fn to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn str_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn show_error_dialog(msg: &str) {
    let msg_w = str_wide_null(msg);
    let title_w = str_wide_null("FlashDesk 安装错误");
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(msg_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
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

fn dir_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(format!(".setup-probe-{}", std::process::id()));
    if std::fs::write(&probe, b"ok").is_ok() {
        let _ = std::fs::remove_file(&probe);
        true
    } else {
        false
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

fn stop_existing_service_if_any() -> Result<()> {
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS) else {
            return Ok(());
        };
        let _g = ScGuard(scm);
        let svc_w = to_wide_null(OsStr::new(SERVICE_NAME));
        let Ok(svc) = OpenServiceW(scm, PCWSTR(svc_w.as_ptr()), SERVICE_ALL_ACCESS) else {
            return Ok(());
        };
        let _sg = ScGuard(svc);
        let mut status = SERVICE_STATUS::default();
        let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut status);
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let mut buf = [0u8; std::mem::size_of::<SERVICE_STATUS_PROCESS>()];
            let mut needed = 0u32;
            if QueryServiceStatusEx(svc, SC_STATUS_PROCESS_INFO, Some(&mut buf), &mut needed)
                .is_ok()
            {
                let st = &*(buf.as_ptr() as *const SERVICE_STATUS_PROCESS);
                if st.dwCurrentState == SERVICE_STOPPED {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn enable_software_sas_policy() {
    unsafe {
        let mut hkey = HKEY::default();
        let subkey = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
        if RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .is_err()
        {
            return;
        }
        let val: u32 = 1;
        let _ = RegSetValueExW(
            hkey,
            w!("SoftwareSASGeneration"),
            0,
            REG_DWORD,
            Some(&val.to_le_bytes()),
        );
        let _ = RegCloseKey(hkey);
    }
}

fn install_and_start_base_service(svc_exe: &Path) -> Result<()> {
    enable_software_sas_policy();
    let bin_cmd = format!("\"{}\" --config \"{}\"", svc_exe.display(), SHARED_CONFIG_PATH);
    let bin_w = to_wide_null(OsStr::new(&bin_cmd));
    let svc_name_w = to_wide_null(OsStr::new(SERVICE_NAME));
    let disp_name_w = to_wide_null(OsStr::new(SERVICE_DISPLAY_NAME));

    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
            .context("OpenSCManagerW 失败，需要管理员权限")?;
        let _g = ScGuard(scm);
        let svc = match OpenServiceW(scm, PCWSTR(svc_name_w.as_ptr()), SERVICE_ALL_ACCESS) {
            Ok(existing) => {
                ChangeServiceConfigW(
                    existing,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    PCWSTR(bin_w.as_ptr()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    PCWSTR(disp_name_w.as_ptr()),
                )
                .context("更新已有 FlashDeskService 配置失败")?;
                existing
            }
            Err(_) => CreateServiceW(
                scm,
                PCWSTR(svc_name_w.as_ptr()),
                PCWSTR(disp_name_w.as_ptr()),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                PCWSTR(bin_w.as_ptr()),
                None,
                None,
                None,
                None,
                None,
            )
            .context("注册 FlashDeskService 失败")?,
        };
        let _sg = ScGuard(svc);

        let mut actions = [
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 3000,
            },
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 5000,
            },
            SC_ACTION {
                Type: SC_ACTION_RESTART,
                Delay: 10000,
            },
        ];
        let failure_actions = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: 86400,
            lpRebootMsg: PWSTR::null(),
            lpCommand: PWSTR::null(),
            cActions: actions.len() as u32,
            lpsaActions: actions.as_mut_ptr(),
        };
        let _ = ChangeServiceConfig2W(
            svc,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&failure_actions as *const _ as *const _),
        );

        let _ = base_common::grant_interactive_user_service_control(SERVICE_NAME);

        if let Err(e) = StartServiceW(svc, None) {
            if e.code() != ERROR_SERVICE_ALREADY_RUNNING.to_hresult() {
                return Err(e.into());
            }
        }
    }
    Ok(())
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
                            PWSTR(buf.as_mut_ptr()),
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

fn json_has_bundle(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    val.get("bundle").is_some_and(|b| !b.is_null())
}

fn migrate_shared_config_if_needed(source_cfg: Option<&Path>) {
    let dst = Path::new(SHARED_CONFIG_PATH);
    if dst.is_file() && json_has_bundle(dst) {
        return;
    }
    if let Some(src) = source_cfg {
        if src.is_file() && json_has_bundle(src) {
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::copy(src, dst);
        }
    }
}

fn folder_path(csidl: u32) -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    let ok =
        unsafe { SHGetFolderPathW(HWND::default(), csidl as i32, HANDLE::default(), 0, &mut buf) };
    if ok.is_ok() {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
    } else {
        None
    }
}

fn create_shortcut_sta(launcher: &Path, install_dir: &Path, elevated: bool) -> Result<()> {
    let launcher = launcher.to_path_buf();
    let install_dir = install_dir.to_path_buf();
    std::thread::Builder::new()
        .name("flashdesk-shortcut".into())
        .spawn(move || -> Result<()> {
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
            }
            let res = (|| -> Result<()> {
                let desktop_csidl = if elevated {
                    CSIDL_COMMON_DESKTOPDIRECTORY
                } else {
                    CSIDL_DESKTOPDIRECTORY
                };
                let programs_csidl = if elevated {
                    CSIDL_COMMON_PROGRAMS
                } else {
                    CSIDL_PROGRAMS
                };

                let mut targets = Vec::new();
                if let Some(d) = folder_path(desktop_csidl) {
                    targets.push(d.join("FlashDesk.lnk"));
                }
                if let Some(p) = folder_path(programs_csidl) {
                    let dir = p.join("FlashDesk");
                    let _ = std::fs::create_dir_all(&dir);
                    targets.push(dir.join("FlashDesk.lnk"));
                }

                let target_w = to_wide_null(launcher.as_os_str());
                let workdir_w = to_wide_null(install_dir.as_os_str());
                let desc_w = str_wide_null("FlashDesk 自建高速远程桌面");

                for lnk_path in targets {
                    let link: IShellLinkW = unsafe {
                        CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?
                    };
                    unsafe {
                        link.SetPath(PCWSTR(target_w.as_ptr()))?;
                        link.SetWorkingDirectory(PCWSTR(workdir_w.as_ptr()))?;
                        link.SetDescription(PCWSTR(desc_w.as_ptr()))?;
                        link.SetIconLocation(PCWSTR(target_w.as_ptr()), 0)?;
                        let pf: IPersistFile = link.cast()?;
                        let lnk_w = to_wide_null(lnk_path.as_os_str());
                        pf.Save(PCWSTR(lnk_w.as_ptr()), true)?;
                    }
                }
                unsafe {
                    SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
                }
                Ok(())
            })();
            unsafe {
                CoUninitialize();
            }
            res
        })?
        .join()
        .map_err(|_| anyhow::anyhow!("快捷方式线程异常退出"))?
}

fn reg_set_sz(hkey: HKEY, name: PCWSTR, val: &str) {
    let wide = str_wide_null(val);
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    unsafe {
        let _ = RegSetValueExW(hkey, name, 0, REG_SZ, Some(bytes));
    }
}

fn write_uninstall_registry(
    install_dir: &Path,
    uninstall_exe: &Path,
    launcher_exe: &Path,
    version: &str,
    elevated: bool,
) {
    let root = if elevated {
        HKEY_LOCAL_MACHINE
    } else {
        HKEY_CURRENT_USER
    };
    let subkey = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\FlashDesk");
    unsafe {
        let mut hkey = HKEY::default();
        if RegCreateKeyExW(
            root,
            subkey,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .is_err()
        {
            return;
        }
        reg_set_sz(hkey, w!("DisplayName"), "FlashDesk 远程桌面");
        reg_set_sz(hkey, w!("DisplayVersion"), version);
        reg_set_sz(hkey, w!("Publisher"), "FlashDesk");
        reg_set_sz(hkey, w!("InstallLocation"), &install_dir.display().to_string());
        reg_set_sz(
            hkey,
            w!("UninstallString"),
            &format!("\"{}\"", uninstall_exe.display()),
        );
        reg_set_sz(hkey, w!("DisplayIcon"), &launcher_exe.display().to_string());
        let one: u32 = 1;
        let _ = RegSetValueExW(hkey, w!("NoModify"), 0, REG_DWORD, Some(&one.to_le_bytes()));
        let _ = RegSetValueExW(hkey, w!("NoRepair"), 0, REG_DWORD, Some(&one.to_le_bytes()));
        let _ = RegCloseKey(hkey);
    }
}

fn write_binary_atomic(dst: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_extension("exe.tmp");
    std::fs::write(&tmp, bytes)?;
    replace_file_atomic(&tmp, dst)?;
    Ok(())
}

fn run_elevated_install_child(cfg: &InstallConfig) -> Result<()> {
    let exe = std::env::current_exe().context("无法获取安装程序路径")?;
    let mut args = format!(
        "--silent --elevated-install --dir \"{}\"",
        cfg.install_dir.display()
    );
    if !cfg.install_service {
        args.push_str(" --no-service");
    }
    if !cfg.create_shortcuts {
        args.push_str(" --no-shortcuts");
    }
    if let Some(src) = &cfg.source_config {
        args.push_str(&format!(" --source-config \"{}\"", src.display()));
    }

    let verb_w = w!("runas");
    let exe_w = to_wide_null(exe.as_os_str());
    let args_w = to_wide_null(OsStr::new(&args));
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND::default(),
        lpVerb: verb_w,
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(args_w.as_ptr()),
        lpDirectory: PCWSTR::null(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut sei).context("提权启动安装进程失败或已取消")?;
    }
    if !sei.hProcess.is_invalid() && !sei.hProcess.0.is_null() {
        let mut exit_code = 1u32;
        unsafe {
            WaitForSingleObject(sei.hProcess, INFINITE);
            let _ = GetExitCodeProcess(sei.hProcess, &mut exit_code);
            let _ = CloseHandle(sei.hProcess);
        }
        anyhow::ensure!(
            exit_code == 0,
            "提权安装进程返回错误码 {exit_code}，请检查目标路径与权限"
        );
    }
    Ok(())
}

fn perform_install(
    payload: &ExtractedSetupPayload,
    cfg: &InstallConfig,
    mut on_step: impl FnMut(&str, f32),
) -> Result<()> {
    anyhow::ensure!(
        cfg.install_dir.parent().is_some(),
        "不能直接安装到磁盘根目录，请选择子目录"
    );

    let need_uac = (cfg.install_service || !dir_writable(&cfg.install_dir)) && !is_elevated();
    if need_uac {
        on_step("正在请求管理员权限…", 0.15);
        return run_elevated_install_child(cfg);
    }

    let core_ver = &payload.manifest.core_version;
    let base_ver = &payload.manifest.base_version;

    let find_bin = |role: &str| -> Result<&[u8]> {
        payload
            .binaries
            .iter()
            .find(|(r, _)| r == role)
            .map(|(_, b)| b.as_slice())
            .with_context(|| format!("安装包缺失组件: {role}"))
    };
    let svc_bytes = find_bin("svc")?;
    let launcher_bytes = find_bin("launcher")?;
    let uninstall_bytes = find_bin("uninstall")?;
    let core_bytes = find_bin("core")?;

    on_step("正在停止已有进程…", 0.25);
    if cfg.install_service {
        let _ = stop_existing_service_if_any();
    }
    terminate_processes_in_dir(&cfg.install_dir);

    on_step("正在创建安装目录结构…", 0.40);
    std::fs::create_dir_all(cfg.install_dir.join("bin"))?;
    std::fs::create_dir_all(cfg.install_dir.join("versions").join(core_ver))?;
    std::fs::create_dir_all(cfg.install_dir.join("updates").join("cache"))?;
    std::fs::create_dir_all(cfg.install_dir.join("updates").join("staging"))?;
    std::fs::create_dir_all(cfg.install_dir.join("logs"))?;

    on_step("正在释放程序文件…", 0.55);
    let launcher_exe = cfg.install_dir.join(LAUNCHER_EXE_NAME);
    let svc_exe = cfg.install_dir.join("bin").join(SVC_EXE_NAME);
    let uninstall_exe = cfg.install_dir.join("bin").join(UNINSTALL_EXE_NAME);
    let core_exe = core_exe_for_version(&cfg.install_dir, core_ver);

    write_binary_atomic(&launcher_exe, launcher_bytes)?;
    write_binary_atomic(&svc_exe, svc_bytes)?;
    write_binary_atomic(&uninstall_exe, uninstall_bytes)?;
    write_binary_atomic(&core_exe, core_bytes)?;

    let state_path = cfg.install_dir.join(STATE_FILE_NAME);
    let mut state = read_json_opt::<InstallState>(&state_path).unwrap_or(InstallState {
        base_version: base_ver.clone(),
        active_version: core_ver.clone(),
        previous_good_version: None,
        candidate: None,
        blacklisted_versions: Vec::new(),
        last_check_at: 0,
        installed_at: unix_now(),
    });
    if state.active_version != *core_ver {
        state.previous_good_version = Some(state.active_version.clone());
    }
    state.base_version = base_ver.clone();
    state.active_version = core_ver.clone();
    state.candidate = None;
    write_json_atomic(&state_path, &state)?;

    if cfg.install_service {
        on_step("正在注册并启动 FlashDeskService 系统服务…", 0.75);
        std::fs::create_dir_all(SHARED_PROGRAM_DATA_DIR)?;
        let _ = ensure_ipc_dir_writable(Path::new(UPDATE_IPC_DIR));
        migrate_shared_config_if_needed(cfg.source_config.as_deref());
        install_and_start_base_service(&svc_exe)?;
    }

    if cfg.create_shortcuts {
        on_step("正在创建桌面与开始菜单快捷方式…", 0.88);
        if let Err(e) = create_shortcut_sta(&launcher_exe, &cfg.install_dir, is_elevated()) {
            eprintln!("warn: create shortcut failed: {e:#}");
        }
    }

    on_step("正在写入卸载信息…", 0.95);
    write_uninstall_registry(
        &cfg.install_dir,
        &uninstall_exe,
        &launcher_exe,
        core_ver,
        is_elevated(),
    );
    on_step("安装完成！", 1.0);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Dark Theme GUI Wizard (eframe glow)
// ---------------------------------------------------------------------------------------------

mod c {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(13, 16, 22);
    pub const SURFACE: Color32 = Color32::from_rgb(21, 25, 33);
    pub const SURFACE_2: Color32 = Color32::from_rgb(28, 33, 43);
    pub const FIELD: Color32 = Color32::from_rgb(12, 15, 20);
    pub const BORDER: Color32 = Color32::from_rgb(36, 42, 54);
    pub const BORDER_STRONG: Color32 = Color32::from_rgb(52, 60, 76);
    pub const TEXT: Color32 = Color32::from_rgb(230, 234, 241);
    pub const TEXT_2: Color32 = Color32::from_rgb(156, 166, 182);
    pub const TEXT_3: Color32 = Color32::from_rgb(108, 118, 135);
    pub const ACCENT: Color32 = Color32::from_rgb(45, 200, 170);
    pub const ACCENT_TEXT: Color32 = Color32::from_rgb(6, 28, 24);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(20, 56, 52);
    pub const OK: Color32 = Color32::from_rgb(72, 212, 142);
    pub const DANGER: Color32 = Color32::from_rgb(240, 104, 104);
}

fn install_dark_theme(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    for path in [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\Deng.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("cjk".into(), Arc::new(FontData::from_owned(bytes)));
            if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
                list.insert(0, "cjk".into());
            }
            if let Some(list) = fonts.families.get_mut(&FontFamily::Monospace) {
                list.push("cjk".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut style = (*ctx.style_of(Theme::Dark)).clone();
    style.text_styles = [
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = c::BG;
    style.visuals.window_fill = c::SURFACE;
    style.visuals.extreme_bg_color = c::FIELD;
    style.visuals.faint_bg_color = c::SURFACE_2;
    ctx.set_style_of(Theme::Dark, style.clone());
    ctx.set_style_of(Theme::Light, style);
}

enum WizardStage {
    Configure,
    Installing { step: String, progress: f32 },
    Done,
    Failed(String),
}

struct SharedWorkerState {
    step: String,
    progress: f32,
    done: Option<Result<()>>,
}

struct InstallerApp {
    payload: Arc<ExtractedSetupPayload>,
    path_input: String,
    install_service: bool,
    create_shortcuts: bool,
    source_config: Option<PathBuf>,
    stage: WizardStage,
    worker: Option<Arc<Mutex<SharedWorkerState>>>,
}

impl InstallerApp {
    fn new(cc: &eframe::CreationContext<'_>, payload: ExtractedSetupPayload) -> Self {
        install_dark_theme(&cc.egui_ctx);
        Self {
            payload: Arc::new(payload),
            path_input: default_install_dir().display().to_string(),
            install_service: true,
            create_shortcuts: true,
            source_config: default_user_config_path(),
            stage: WizardStage::Configure,
            worker: None,
        }
    }

    fn start_install(&mut self, ctx: &egui::Context) {
        let dir_trimmed = self.path_input.trim();
        if dir_trimmed.is_empty() {
            self.stage = WizardStage::Failed("安装路径不能为空".into());
            return;
        }
        let cfg = InstallConfig {
            install_dir: PathBuf::from(dir_trimmed),
            install_service: self.install_service,
            create_shortcuts: self.create_shortcuts,
            source_config: self.source_config.clone(),
        };
        let state = Arc::new(Mutex::new(SharedWorkerState {
            step: "准备安装…".into(),
            progress: 0.05,
            done: None,
        }));
        self.worker = Some(Arc::clone(&state));
        self.stage = WizardStage::Installing {
            step: "准备安装…".into(),
            progress: 0.05,
        };

        let payload = Arc::clone(&self.payload);
        let egui_ctx = ctx.clone();
        std::thread::spawn(move || {
            let st_prog = Arc::clone(&state);
            let ctx_prog = egui_ctx.clone();
            let res = perform_install(&payload, &cfg, move |step, progress| {
                let mut g = st_prog.lock().unwrap();
                g.step = step.to_string();
                g.progress = progress;
                ctx_prog.request_repaint();
            });
            state.lock().unwrap().done = Some(res);
            egui_ctx.request_repaint();
        });
    }
}

impl eframe::App for InstallerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(w) = &self.worker {
            let g = w.lock().unwrap();
            if let Some(res) = &g.done {
                let next = match res {
                    Ok(()) => WizardStage::Done,
                    Err(e) => WizardStage::Failed(format!("{e:#}")),
                };
                drop(g);
                self.worker = None;
                self.stage = next;
            } else {
                self.stage = WizardStage::Installing {
                    step: g.step.clone(),
                    progress: g.progress,
                };
            }
        }

        egui::CentralPanel::default()
            .frame(Frame::new().fill(c::BG).inner_margin(Margin::same(24)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⚡").size(26.0).color(c::ACCENT));
                    ui.vertical(|ui| {
                        ui.label(RichText::new("FlashDesk 安装向导").size(20.0).strong().color(c::TEXT));
                        ui.label(
                            RichText::new(format!(
                                "核心版本 v{} · 基座版本 v{}",
                                self.payload.manifest.core_version,
                                self.payload.manifest.base_version
                            ))
                            .size(13.0)
                            .color(c::TEXT_2),
                        );
                    });
                });

                ui.add_space(16.0);

                Frame::new()
                    .fill(c::SURFACE)
                    .stroke(Stroke::new(1.0_f32, c::BORDER))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(18))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match &self.stage {
                            WizardStage::Configure => {
                                ui.label(RichText::new("安装位置").strong().color(c::TEXT));
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    let w = (ui.available_width() - 90.0).max(160.0);
                                    ui.add_sized(
                                        vec2(w, 30.0),
                                        egui::TextEdit::singleline(&mut self.path_input),
                                    );
                                    if ui
                                        .add_sized(vec2(78.0, 30.0), egui::Button::new("浏览…"))
                                        .clicked()
                                    {
                                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                                            let chosen = if folder
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .is_some_and(|n| {
                                                    n.eq_ignore_ascii_case("FlashDesk")
                                                }) {
                                                folder
                                            } else {
                                                folder.join("FlashDesk")
                                            };
                                            self.path_input = chosen.display().to_string();
                                        }
                                    }
                                });

                                ui.add_space(12.0);
                                ui.checkbox(
                                    &mut self.install_service,
                                    "安装后台系统服务 FlashDeskService（开机自启 · 支持锁屏/UAC 穿透及免 UAC 自动更新）",
                                );
                                ui.add_space(4.0);
                                ui.checkbox(
                                    &mut self.create_shortcuts,
                                    "创建桌面与开始菜单快捷方式",
                                );

                                ui.add_space(12.0);
                                Frame::new()
                                    .fill(c::ACCENT_SOFT)
                                    .stroke(Stroke::new(1.0_f32, c::BORDER_STRONG))
                                    .corner_radius(CornerRadius::same(8))
                                    .inner_margin(Margin::same(10))
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(
                                                    "提示：本安装程序不会自动安装虚拟显示器驱动（VDD）；\
                                                     如需无显示器虚拟屏，请在安装后于软件「设置」中自行选择安装。",
                                                )
                                                .size(12.5)
                                                .color(c::TEXT_2),
                                            )
                                            .wrap(),
                                        );
                                    });
                            }
                            WizardStage::Installing { step, progress } => {
                                ui.label(RichText::new("正在安装 FlashDesk…").strong().color(c::TEXT));
                                ui.add_space(10.0);
                                ui.add(egui::ProgressBar::new(*progress).show_percentage());
                                ui.add_space(8.0);
                                ui.label(RichText::new(step).color(c::TEXT_2));
                            }
                            WizardStage::Done => {
                                ui.label(RichText::new("✓ FlashDesk 已安装完成！").size(16.0).strong().color(c::OK));
                                ui.add_space(8.0);
                                ui.label(
                                    RichText::new(format!("安装目录：{}", self.path_input))
                                        .color(c::TEXT_2),
                                );
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new("后续版本升级将自动通过基座版本槽完成，无需再次运行安装向导。")
                                        .size(12.5)
                                        .color(c::TEXT_3),
                                );
                            }
                            WizardStage::Failed(err) => {
                                ui.label(RichText::new("安装未完成").size(16.0).strong().color(c::DANGER));
                                ui.add_space(8.0);
                                ui.add(
                                    egui::Label::new(RichText::new(err).color(c::TEXT_2)).wrap(),
                                );
                            }
                        }
                    });

                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        match &self.stage {
                            WizardStage::Configure => {
                                if ui
                                    .add_sized(vec2(120.0, 34.0), egui::Button::new("取消"))
                                    .clicked()
                                {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                let btn = egui::Button::new(
                                    RichText::new("立即安装").strong().color(c::ACCENT_TEXT),
                                )
                                .fill(c::ACCENT);
                                if ui.add_sized(vec2(140.0, 34.0), btn).clicked() {
                                    self.start_install(ctx);
                                }
                            }
                            WizardStage::Installing { .. } => {
                                ui.add_enabled(
                                    false,
                                    egui::Button::new("正在安装…"),
                                );
                            }
                            WizardStage::Done => {
                                if ui
                                    .add_sized(vec2(100.0, 34.0), egui::Button::new("完成"))
                                    .clicked()
                                {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                let launch_btn = egui::Button::new(
                                    RichText::new("立即启动 FlashDesk")
                                        .strong()
                                        .color(c::ACCENT_TEXT),
                                )
                                .fill(c::ACCENT);
                                if ui.add_sized(vec2(160.0, 34.0), launch_btn).clicked() {
                                    let launcher =
                                        PathBuf::from(self.path_input.trim()).join(LAUNCHER_EXE_NAME);
                                    let _ = std::process::Command::new(&launcher).spawn();
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            }
                            WizardStage::Failed(_) => {
                                if ui
                                    .add_sized(vec2(100.0, 34.0), egui::Button::new("关闭"))
                                    .clicked()
                                {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                if ui
                                    .add_sized(vec2(120.0, 34.0), egui::Button::new("返回重试"))
                                    .clicked()
                                {
                                    self.stage = WizardStage::Configure;
                                }
                            }
                        }
                    });
                });
            });
    }
}

fn main() -> Result<()> {
    attach_parent_console_if_needed();
    let cli = Cli::parse();

    let self_exe = std::env::current_exe().context("无法定位安装程序路径")?;
    let payload = match read_self_extracting_setup(&self_exe) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("{e:#}");
            if cli.silent {
                eprintln!("{msg}");
                std::process::exit(1);
            } else {
                show_error_dialog(&msg);
                return Err(e);
            }
        }
    };

    if cli.silent || cli.elevated_install {
        let install_dir = cli.dir.unwrap_or_else(default_install_dir);
        let cfg = InstallConfig {
            install_dir: install_dir.clone(),
            install_service: !cli.no_service,
            create_shortcuts: !cli.no_shortcuts,
            source_config: cli.source_config.or_else(default_user_config_path),
        };
        if let Err(e) = perform_install(&payload, &cfg, |step, _| {
            println!("{step}");
        }) {
            eprintln!("安装失败: {e:#}");
            std::process::exit(1);
        }
        if cli.launch {
            let _ = std::process::Command::new(install_dir.join(LAUNCHER_EXE_NAME)).spawn();
        }
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 410.0])
            .with_min_inner_size([520.0, 380.0])
            .with_resizable(false)
            .with_icon(egui::IconData {
                rgba: base_common::build_app_icon_rgba(32),
                width: 32,
                height: 32,
            })
            .with_title("FlashDesk 安装向导"),
        centered: true,
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "FlashDesk Setup",
        options,
        Box::new(move |cc| Ok(Box::new(InstallerApp::new(cc, payload)))),
    ) {
        let msg = format!("安装向导图形界面启动失败: {e}");
        show_error_dialog(&msg);
        return Err(anyhow::anyhow!(msg));
    }

    Ok(())
}
