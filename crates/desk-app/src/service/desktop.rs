use anyhow::{Context, Result};
use std::ffi::c_void;
use windows::core::{s, w};
use windows::Win32::Foundation::{BOOL, HANDLE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

fn desktop_name(hdesk: HDESK) -> Option<String> {
    if hdesk.0.is_null() {
        return None;
    }
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(
            HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut c_void),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Ensures the calling thread is attached to the current active input desktop (`Default` or
/// `Winlogon` on lockscreen/UAC). Returns `Ok(true)` if the desktop changed.
pub fn sync_thread_to_input_desktop() -> Result<bool> {
    unsafe {
        // DESKTOP_ALL_ACCESS (0x01FF) covers read/write/switch/hooks for capture & SendInput
        let input_desk = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x01FF),
        )
        .context("OpenInputDesktop failed")?;

        let cur_desk = GetThreadDesktop(GetCurrentThreadId()).ok();
        let cur_name = cur_desk.and_then(desktop_name);
        let target_name = desktop_name(input_desk);

        if cur_name.is_some() && cur_name == target_name {
            let _ = CloseDesktop(input_desk);
            return Ok(false);
        }

        let res = SetThreadDesktop(input_desk);
        let _ = CloseDesktop(input_desk);
        res.context("SetThreadDesktop failed")?;
        if let Some(name) = target_name {
            tracing::info!("switched thread desktop to '{}'", name);
        }
        Ok(true)
    }
}

/// Sends a real Windows Secure Attention Sequence (Ctrl+Alt+Del) via `sas.dll`.
/// Requires running as SYSTEM / elevated service with `SoftwareSASGeneration` enabled.
pub fn send_sas_ctrl_alt_del() -> Result<()> {
    type SendSasFn = unsafe extern "system" fn(BOOL);
    unsafe {
        let hmod = LoadLibraryW(w!("sas.dll")).context("无法加载 sas.dll")?;
        let proc = GetProcAddress(hmod, s!("SendSAS")).context("sas.dll 未导出 SendSAS")?;
        let func: SendSasFn = std::mem::transmute(proc);
        func(BOOL(0)); // AsUser = FALSE (service / SYSTEM mode)
        Ok(())
    }
}
