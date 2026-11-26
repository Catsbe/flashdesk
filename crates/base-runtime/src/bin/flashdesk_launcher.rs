#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base_common::resolve_launch_core_exe;
use std::ffi::OsString;
use std::process::Command;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Console::{
    AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

fn attach_parent_console_if_needed(has_args: bool) {
    if !has_args {
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

fn show_error_dialog(msg: &str) {
    let msg_w: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let title_w: Vec<u16> = "FlashDesk 启动失败"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(msg_w.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let has_args = !args.is_empty();
    attach_parent_console_if_needed(has_args);

    let Ok(self_exe) = std::env::current_exe() else {
        if !has_args {
            show_error_dialog("无法定位 FlashDesk 启动器路径。");
        }
        std::process::exit(1);
    };
    let Some(install_dir) = self_exe.parent() else {
        if !has_args {
            show_error_dialog("无法确定 FlashDesk 安装目录。");
        }
        std::process::exit(1);
    };

    let Some((_ver, core_exe)) = resolve_launch_core_exe(install_dir) else {
        let detail = format!(
            "在 {} 下未找到可用的 FlashDesk 核心程序 (versions\\*\\flashdesk-core.exe)。\n请重新运行 FlashDesk-Setup.exe 修复安装。",
            install_dir.display()
        );
        if has_args {
            eprintln!("{detail}");
        } else {
            show_error_dialog(&detail);
        }
        std::process::exit(1);
    };

    let mut cmd = Command::new(&core_exe);
    cmd.args(&args);

    match cmd.spawn() {
        Ok(mut child) => {
            if has_args {
                match child.wait() {
                    Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                    Err(e) => {
                        eprintln!("等待核心进程失败: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Err(e) => {
            let detail = format!("启动 {} 失败：{e}", core_exe.display());
            if has_args {
                eprintln!("{detail}");
            } else {
                show_error_dialog(&detail);
            }
            std::process::exit(1);
        }
    }
}
