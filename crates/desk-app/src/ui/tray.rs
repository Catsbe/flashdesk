//! Windows system tray (notification area) integration.
//!
//! Closing the main window (`X`) hides it to the tray instead of exiting so remote hosting
//! and relay presence keep running. Clicking/double-clicking the tray icon or picking
//! "显示主窗口" restores the window; picking "退出 FlashDesk" cleanly terminates the app.

use eframe::egui;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS, FALSE, HANDLE, HWND, LPARAM, LRESULT,
    POINT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPV5HEADER, BI_BITFIELDS,
    DIB_RGB_COLORS, HBITMAP,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, EnumThreadWindows,
    FindWindowW, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowTextW, LoadIconW,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetMenuDefaultItem, SetWindowLongPtrW, ShowWindow, TrackPopupMenu, TranslateMessage, ASFW_ANY,
    CREATESTRUCTW, GWLP_USERDATA, HICON, ICONINFO, IDI_APPLICATION, IsWindow, IsWindowVisible,
    MF_SEPARATOR, MF_STRING, MSG, SW_RESTORE, SW_SHOW, SW_SHOWNA, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

const TRAY_ICON_UID: u32 = 1;
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_TRAY_SHUTDOWN: u32 = WM_APP + 2;
const WM_TRAY_SHOW_MAIN: u32 = WM_APP + 3;
const MENU_ID_SHOW: u32 = 1001;
const MENU_ID_QUIT: u32 = 1002;
const TRAY_TIP: &str = "FlashDesk — 远程桌面（点击打开窗口）";
const SINGLE_INSTANCE_MUTEX: &str = "Local\\FlashDeskGuiSingleInstance";
pub const MAIN_WINDOW_TITLE: &str = "FlashDesk — 自建高速远程桌面";

/// Holds the session-scoped named mutex preventing duplicate GUI instances.
pub struct SingleInstanceGuard(HANDLE);

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn try_acquire_named_mutex(name: &str) -> Option<SingleInstanceGuard> {
    let mut wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let Ok(handle) = CreateMutexW(None, false, PCWSTR(wide.as_mut_ptr())) else {
            return Some(SingleInstanceGuard(HANDLE::default()));
        };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return None;
        }
        Some(SingleInstanceGuard(handle))
    }
}

/// Acquires the single-instance GUI mutex. If another GUI instance is already running in this
/// Windows session, asks it to restore & focus its window and returns `None`.
pub fn try_acquire_gui_instance() -> Option<SingleInstanceGuard> {
    if let Some(guard) = try_acquire_named_mutex(SINGLE_INSTANCE_MUTEX) {
        return Some(guard);
    }
    activate_existing_gui_instance();
    None
}

fn activate_existing_gui_instance() {
    let title_w: Vec<u16> = MAIN_WINDOW_TITLE
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        for attempt in 0..3 {
            if let Ok(tray_hwnd) = FindWindowW(w!("FlashDeskTrayWindowClass"), w!("FlashDeskTray"))
            {
                if !tray_hwnd.0.is_null() {
                    let _ = PostMessageW(tray_hwnd, WM_TRAY_SHOW_MAIN, WPARAM(0), LPARAM(0));
                    return;
                }
            }
            if let Ok(main_hwnd) = FindWindowW(PCWSTR::null(), PCWSTR(title_w.as_ptr())) {
                if !main_hwnd.0.is_null() {
                    let _ = ShowWindow(main_hwnd, SW_SHOW);
                    let _ = ShowWindow(main_hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(main_hwnd);
                    return;
                }
            }
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}

/// Generates a 32x32 RGBA FlashDesk icon (dark rounded badge + teal lightning bolt).
pub fn build_tray_icon_rgba() -> Vec<u8> {
    base_common::build_app_icon_rgba(32)
}

/// Encodes a tooltip string into the 128-UTF-16-unit `NOTIFYICONDATAW::szTip` buffer.
pub fn encode_tip(text: &str) -> [u16; 128] {
    let mut buf = [0u16; 128];
    for (dst, unit) in buf.iter_mut().take(127).zip(text.encode_utf16()) {
        *dst = unit;
    }
    buf
}

/// Finds the root `FlashDesk` window `HWND` on the calling GUI thread.
pub fn find_main_window_hwnd() -> isize {
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let mut title = [0u16; 128];
        let len = GetWindowTextW(hwnd, &mut title);
        if len > 0 {
            let text = String::from_utf16_lossy(&title[..len as usize]);
            if text.contains("FlashDesk") {
                let out = &mut *(lparam.0 as *mut isize);
                *out = hwnd.0 as isize;
                return FALSE;
            }
        }
        TRUE
    }

    let mut found = 0isize;
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(enum_proc),
            LPARAM(&mut found as *mut isize as isize),
        );
    }
    found
}

/// Handle to the background Windows notification-area icon thread.
pub struct TrayHandle {
    tray_hwnd: Arc<AtomicIsize>,
    main_hwnd: Arc<AtomicIsize>,
    quit_flag: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl TrayHandle {
    pub fn spawn(main_hwnd: isize, egui_ctx: egui::Context) -> Self {
        let tray_hwnd = Arc::new(AtomicIsize::new(0));
        let main_hwnd_arc = Arc::new(AtomicIsize::new(main_hwnd));
        let quit_flag = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(false));

        let (ready_tx, ready_rx) = mpsc::sync_channel::<bool>(1);
        let t_tray_hwnd = Arc::clone(&tray_hwnd);
        let t_main_hwnd = Arc::clone(&main_hwnd_arc);
        let t_quit = Arc::clone(&quit_flag);

        let thread = std::thread::Builder::new()
            .name("flashdesk-tray".into())
            .spawn(move || {
                run_tray_thread(egui_ctx, t_tray_hwnd, t_main_hwnd, t_quit, ready_tx);
            })
            .ok();

        let is_ok = if thread.is_some() {
            ready_rx.recv().unwrap_or(false)
        } else {
            false
        };
        active.store(is_ok, Ordering::Release);

        Self {
            tray_hwnd,
            main_hwnd: main_hwnd_arc,
            quit_flag,
            active,
            thread,
        }
    }

    pub fn set_main_hwnd(&self, hwnd: isize) {
        if hwnd != 0 {
            self.main_hwnd.store(hwnd, Ordering::Release);
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn quit_requested(&self) -> bool {
        self.quit_flag.load(Ordering::Acquire)
    }

    pub fn shutdown(&mut self) {
        self.active.store(false, Ordering::Release);
        let raw = self.tray_hwnd.swap(0, Ordering::AcqRel);
        if raw != 0 {
            unsafe {
                let _ = PostMessageW(
                    HWND(raw as *mut c_void),
                    WM_TRAY_SHUTDOWN,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct TrayWndState {
    egui_ctx: egui::Context,
    main_hwnd: Arc<AtomicIsize>,
    quit_flag: Arc<AtomicBool>,
    taskbar_created_msg: u32,
    hicon: HICON,
    icon_added: bool,
}

fn run_tray_thread(
    egui_ctx: egui::Context,
    tray_hwnd_out: Arc<AtomicIsize>,
    main_hwnd: Arc<AtomicIsize>,
    quit_flag: Arc<AtomicBool>,
    ready_tx: mpsc::SyncSender<bool>,
) {
    unsafe {
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class_name = w!("FlashDeskTrayWindowClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        let hicon = create_win32_tray_icon();
        let taskbar_created_msg = RegisterWindowMessageW(w!("TaskbarCreated"));
        let state = Box::new(TrayWndState {
            egui_ctx,
            main_hwnd,
            quit_flag,
            taskbar_created_msg,
            hicon,
            icon_added: false,
        });
        let state_ptr = Box::into_raw(state);

        // Top-level hidden tool window (not HWND_MESSAGE) so `TaskbarCreated` is delivered if
        // Explorer restarts.
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!("FlashDeskTray"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            Some(state_ptr.cast()),
        ) else {
            drop(Box::from_raw(state_ptr));
            if !hicon.is_invalid() {
                let _ = DestroyIcon(hicon);
            }
            let _ = ready_tx.send(false);
            return;
        };

        let added = add_tray_icon(hwnd, hicon);
        (*state_ptr).icon_added = added;
        if !added {
            let _ = DestroyWindow(hwnd);
            let _ = ready_tx.send(false);
            return;
        }

        tray_hwnd_out.store(hwnd.0 as isize, Ordering::Release);
        let _ = ready_tx.send(true);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        tray_hwnd_out.store(0, Ordering::Release);
    }
}

unsafe fn add_tray_icon(hwnd: HWND, hicon: HICON) -> bool {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_UID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: hicon,
        szTip: encode_tip(TRAY_TIP),
        ..Default::default()
    };
    Shell_NotifyIconW(NIM_ADD, &nid).as_bool()
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_UID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayWndState;
    if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let state = &mut *state_ptr;

    if state.taskbar_created_msg != 0 && msg == state.taskbar_created_msg {
        state.icon_added = add_tray_icon(hwnd, state.hicon);
        return LRESULT(0);
    }

    match msg {
        WM_TRAY_CALLBACK => {
            let ev = (lparam.0 as u32) & 0xFFFF;
            match ev {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                    let main_raw = state.main_hwnd.load(Ordering::Acquire);
                    restore_main_window(main_raw, &state.egui_ctx);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    show_context_menu(hwnd, state);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TRAY_SHUTDOWN => {
            if state.icon_added {
                remove_tray_icon(hwnd);
                state.icon_added = false;
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_TRAY_SHOW_MAIN => {
            let main_raw = state.main_hwnd.load(Ordering::Acquire);
            restore_main_window(main_raw, &state.egui_ctx);
            LRESULT(0)
        }
        WM_DESTROY => {
            if state.icon_added {
                remove_tray_icon(hwnd);
                state.icon_added = false;
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let boxed = Box::from_raw(state_ptr);
            if !boxed.hicon.is_invalid() {
                let _ = DestroyIcon(boxed.hicon);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_context_menu(hwnd: HWND, state: &TrayWndState) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        MENU_ID_SHOW as usize,
        w!("显示主窗口"),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        MENU_ID_QUIT as usize,
        w!("退出 FlashDesk"),
    );
    let _ = SetMenuDefaultItem(menu, MENU_ID_SHOW, 0);

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Microsoft KB135788: SetForegroundWindow + WM_NULL so clicking outside closes the menu.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    )
    .0 as u32;
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    match cmd {
        MENU_ID_SHOW => {
            let main_raw = state.main_hwnd.load(Ordering::Acquire);
            restore_main_window(main_raw, &state.egui_ctx);
        }
        MENU_ID_QUIT => {
            let main_raw = state.main_hwnd.load(Ordering::Acquire);
            request_app_quit(&state.quit_flag, main_raw, &state.egui_ctx);
        }
        _ => {}
    }
}

unsafe fn restore_main_window(main_hwnd: isize, egui_ctx: &egui::Context) {
    if main_hwnd != 0 {
        let hwnd = HWND(main_hwnd as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
    egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    egui_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    egui_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    egui_ctx.request_repaint();
}

unsafe fn request_app_quit(quit_flag: &AtomicBool, main_hwnd: isize, egui_ctx: &egui::Context) {
    quit_flag.store(true, Ordering::Release);
    if main_hwnd != 0 {
        let hwnd = HWND(main_hwnd as *mut c_void);
        if IsWindow(hwnd).as_bool() {
            // Hidden Win32 windows do not receive `WM_PAINT`, which winit/eframe needs to pump
            // `App::update` and process `CloseRequested`; show without activating first.
            if !IsWindowVisible(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_SHOWNA);
            }
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    egui_ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    egui_ctx.request_repaint();
}

unsafe fn create_win32_tray_icon() -> HICON {
    let rgba = build_tray_icon_rgba();
    let mut bgra = vec![0u8; rgba.len()];
    for (src, dst) in rgba.chunks_exact(4).zip(bgra.chunks_exact_mut(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }

    let hdr = BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: 32,
        bV5Height: -32,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        ..Default::default()
    };

    let hdc = GetDC(None);
    let mut bits_ptr: *mut c_void = std::ptr::null_mut();
    let color_bmp = CreateDIBSection(
        hdc,
        (&hdr as *const BITMAPV5HEADER).cast(),
        DIB_RGB_COLORS,
        &mut bits_ptr,
        None,
        0,
    );
    let _ = ReleaseDC(None, hdc);

    if let Ok(color_bmp) = color_bmp {
        if !bits_ptr.is_null() {
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits_ptr.cast::<u8>(), bgra.len());
            let mask_bmp: HBITMAP = CreateBitmap(32, 32, 1, 1, None);
            let info = ICONINFO {
                fIcon: TRUE,
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask_bmp,
                hbmColor: color_bmp,
            };
            let icon = CreateIconIndirect(&info);
            if !mask_bmp.is_invalid() {
                let _ = DeleteObject(mask_bmp);
            }
            let _ = DeleteObject(color_bmp);
            if let Ok(hicon) = icon {
                return hicon;
            }
        } else {
            let _ = DeleteObject(color_bmp);
        }
    }

    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_icon_rgba_dimensions_and_alpha() {
        let rgba = build_tray_icon_rgba();
        assert_eq!(rgba.len(), 32 * 32 * 4);
        // Outer corner (0,0) lies outside the rounded badge and must be transparent.
        assert_eq!(rgba[3], 0);
        // Center pixel (16,16) lies inside the badge and must be opaque.
        let center_alpha = rgba[(16 * 32 + 16) * 4 + 3];
        assert_eq!(center_alpha, 255);
    }

    #[test]
    fn encode_tip_handles_unicode_and_truncates_safely() {
        let short = encode_tip(TRAY_TIP);
        let end = short.iter().position(|&c| c == 0).expect("null terminated");
        assert_eq!(String::from_utf16_lossy(&short[..end]), TRAY_TIP);

        let long = "测".repeat(200);
        let truncated = encode_tip(&long);
        assert_eq!(truncated[127], 0);
        assert_ne!(truncated[126], 0);
    }

    #[test]
    fn single_instance_mutex_blocks_second_acquire_until_dropped() {
        let name = format!("Local\\FlashDeskTest-{}", uuid::Uuid::new_v4());
        let first = try_acquire_named_mutex(&name);
        assert!(first.is_some());
        assert!(try_acquire_named_mutex(&name).is_none());
        drop(first);
        assert!(try_acquire_named_mutex(&name).is_some());
    }
}
