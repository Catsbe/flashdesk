use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, HWND};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_DWORD,
    REG_OPTION_NON_VOLATILE,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx,
    StartServiceW, SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS,
    SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_START_TYPE, SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOPPED,
    SERVICE_WIN32_OWN_PROCESS,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

pub const SERVICE_NAME: &str = "FlashDeskService";
pub const SERVICE_DISPLAY_NAME: &str = "FlashDesk 远程桌面后台服务";
pub const SHARED_CONFIG_PATH: &str = r"C:\ProgramData\FlashDesk\config.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Running,
    Stopped,
    Unknown,
}

impl ServiceState {
    pub fn label(&self) -> &'static str {
        match self {
            ServiceState::NotInstalled => "未安装（当前为绿色便携模式）",
            ServiceState::Running => "系统服务运行中（支持锁屏/UAC）",
            ServiceState::Stopped => "已安装但未运行",
            ServiceState::Unknown => "状态未知",
        }
    }
}

fn to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

struct ScHandleGuard(SC_HANDLE);
impl Drop for ScHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseServiceHandle(self.0);
            }
        }
    }
}

pub fn query_service_state() -> ServiceState {
    unsafe {
        let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else {
            return ServiceState::Unknown;
        };
        let _scm_guard = ScHandleGuard(scm);
        let svc_name = to_wide_null(OsStr::new(SERVICE_NAME));
        match OpenServiceW(scm, PCWSTR(svc_name.as_ptr()), SERVICE_QUERY_STATUS) {
            Ok(svc) => {
                let _svc_guard = ScHandleGuard(svc);
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
                    let status = &*(buf.as_ptr() as *const SERVICE_STATUS_PROCESS);
                    if status.dwCurrentState == SERVICE_RUNNING {
                        ServiceState::Running
                    } else if status.dwCurrentState == SERVICE_STOPPED {
                        ServiceState::Stopped
                    } else {
                        ServiceState::Unknown
                    }
                } else {
                    ServiceState::Unknown
                }
            }
            Err(e) => {
                if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() {
                    ServiceState::NotInstalled
                } else {
                    ServiceState::Unknown
                }
            }
        }
    }
}

pub fn sync_config_file_to_program_data(source_cfg: &Path) -> Result<PathBuf> {
    let text = std::fs::read_to_string(source_cfg).with_context(|| {
        format!("读取配置 {} 失败", source_cfg.display())
    })?;
    let parsed: crate::config::AppConfig = serde_json::from_str(&text).with_context(|| {
        format!("解析配置 {} 失败", source_cfg.display())
    })?;
    anyhow::ensure!(
        parsed.bundle.is_some(),
        "拒绝同步空白配置到系统服务：请先初始化根密钥或导入设备配置包"
    );

    let dst = PathBuf::from(SHARED_CONFIG_PATH);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if source_cfg != dst {
        std::fs::copy(source_cfg, &dst).with_context(|| {
            format!(
                "复制配置 {} -> {} 失败",
                source_cfg.display(),
                dst.display()
            )
        })?;
    }
    Ok(dst)
}

fn enable_software_sas_policy() -> Result<()> {
    unsafe {
        let mut hkey = HKEY::default();
        let subkey = w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System");
        let status = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            subkey,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if status.is_err() {
            anyhow::bail!("RegCreateKeyExW failed: {:?}", status);
        }
        let val: u32 = 1; // 1 = Services allowed to call SendSAS
        let res = RegSetValueExW(
            hkey,
            w!("SoftwareSASGeneration"),
            0,
            REG_DWORD,
            Some(&val.to_le_bytes()),
        );
        let _ = RegCloseKey(hkey);
        if res.is_err() {
            anyhow::bail!("RegSetValueExW(SoftwareSASGeneration) failed: {:?}", res);
        }
        Ok(())
    }
}

pub fn install_and_start_service(source_cfg: Option<&Path>) -> Result<()> {
    if let Some(src) = source_cfg {
        let _ = sync_config_file_to_program_data(src)?;
    } else if let Some(parent) = Path::new(SHARED_CONFIG_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }

    if let Err(e) = enable_software_sas_policy() {
        tracing::warn!("failed to set SoftwareSASGeneration policy: {e}");
    }

    let exe = std::env::current_exe().context("无法获取当前 exe 路径")?;
    let bin_cmd = format!(
        "\"{}\" --service --config \"{}\"",
        exe.display(),
        SHARED_CONFIG_PATH
    );
    let bin_w = to_wide_null(OsStr::new(&bin_cmd));
    let svc_name_w = to_wide_null(OsStr::new(SERVICE_NAME));
    let disp_name_w = to_wide_null(OsStr::new(SERVICE_DISPLAY_NAME));

    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
            .context("OpenSCManagerW 失败，请以管理员权限运行")?;
        let _scm_guard = ScHandleGuard(scm);

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
                .context("更新已有服务配置失败")?;
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
            .context("创建 Windows 系统服务失败")?,
        };
        let _svc_guard = ScHandleGuard(svc);

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

        let _ = SERVICE_NO_CHANGE;
        let _ = SERVICE_START_TYPE(0);

        if let Err(e) = StartServiceW(svc, None) {
            if e.code() != ERROR_SERVICE_ALREADY_RUNNING.to_hresult() {
                return Err(e.into());
            }
        }
    }

    Ok(())
}

/// Stops (if running) and starts the service again, e.g. after replacing flashdesk.exe or when the
/// desktop agent failed to spawn. Requires Administrator.
pub fn restart_service() -> Result<()> {
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
            .context("OpenSCManagerW 失败，请以管理员权限运行")?;
        let _scm_guard = ScHandleGuard(scm);
        let svc_name_w = to_wide_null(OsStr::new(SERVICE_NAME));
        let svc = OpenServiceW(scm, PCWSTR(svc_name_w.as_ptr()), SERVICE_ALL_ACCESS)
            .context("打开 FlashDeskService 失败（服务未安装？）")?;
        let _svc_guard = ScHandleGuard(svc);

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
        StartServiceW(svc, None).context("重新启动 FlashDeskService 失败")?;
    }
    Ok(())
}

pub fn stop_and_uninstall_service() -> Result<()> {
    unsafe {
        let scm = OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS)
            .context("OpenSCManagerW 失败，请以管理员权限运行")?;
        let _scm_guard = ScHandleGuard(scm);
        let svc_name_w = to_wide_null(OsStr::new(SERVICE_NAME));
        let svc = OpenServiceW(scm, PCWSTR(svc_name_w.as_ptr()), SERVICE_ALL_ACCESS)
            .context("打开 FlashDeskService 失败")?;
        let _svc_guard = ScHandleGuard(svc);

        let mut status = SERVICE_STATUS::default();
        let _ = ControlService(svc, SERVICE_CONTROL_STOP, &mut status);
        for _ in 0..25 {
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

        DeleteService(svc).context("DeleteService 失败")?;
    }
    Ok(())
}

/// Launches an elevated child `flashdesk.exe` via UAC `runas` to install, uninstall,
/// or sync config to `FlashDeskService`.
pub fn launch_elevated_service_cmd(args: &str) -> Result<()> {
    let exe = std::env::current_exe().context("无法获取当前 exe 路径")?;
    let exe_w = to_wide_null(exe.as_os_str());
    let args_w = to_wide_null(OsStr::new(args));

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
        anyhow::bail!("提权操作被取消或启动失败 (code={})", ret.0 as isize);
    }
    Ok(())
}
