use protocol::session::{InputEvent, MonitorInfo, MouseBtn};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const XBUTTON1_DATA: u32 = 1;
const XBUTTON2_DATA: u32 = 2;

/// Injects a remote input event onto the local Windows desktop.
/// When `target_monitor` is `Some`, normalized `[0, 1]` coordinates map inside that monitor's
/// physical rectangle; when `None` (composite all-monitors view), they map across the entire
/// virtual desktop bounding box.
pub fn inject_input(event: &InputEvent, target_monitor: Option<&MonitorInfo>, agent_mode: bool) -> bool {
    if agent_mode {
        let _ = crate::service::desktop::sync_thread_to_input_desktop();
    }
    match event {
        InputEvent::MouseMove { norm_x, norm_y } => {
            let (vx, vy, vw, vh) = virtual_screen_rect();
            if vw <= 1 || vh <= 1 {
                return true;
            }
            let nx = norm_x.clamp(0.0, 1.0);
            let ny = norm_y.clamp(0.0, 1.0);

            let (px, py) = match target_monitor {
                Some(m) => (
                    m.left as f64 + nx as f64 * (m.width.saturating_sub(1)) as f64,
                    m.top as f64 + ny as f64 * (m.height.saturating_sub(1)) as f64,
                ),
                None => (
                    vx as f64 + nx as f64 * (vw - 1) as f64,
                    vy as f64 + ny as f64 * (vh - 1) as f64,
                ),
            };

            let abs_x = (((px - vx as f64) * 65535.0) / (vw - 1) as f64)
                .round()
                .clamp(0.0, 65535.0) as i32;
            let abs_y = (((py - vy as f64) * 65535.0) / (vh - 1) as f64)
                .round()
                .clamp(0.0, 65535.0) as i32;

            send_mouse(
                abs_x,
                abs_y,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            )
        }
        InputEvent::MouseButton { button, pressed } => {
            let (flags, mouse_data) = match (button, pressed) {
                (MouseBtn::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseBtn::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseBtn::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseBtn::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseBtn::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseBtn::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseBtn::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1_DATA),
                (MouseBtn::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1_DATA),
                (MouseBtn::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2_DATA),
                (MouseBtn::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2_DATA),
            };
            send_mouse(0, 0, mouse_data, flags)
        }
        InputEvent::MouseWheel { delta_x, delta_y } => {
            let mut ok = true;
            if *delta_y != 0 {
                ok &= send_mouse(0, 0, (*delta_y as i32) as u32, MOUSEEVENTF_WHEEL);
            }
            if *delta_x != 0 {
                ok &= send_mouse(0, 0, (*delta_x as i32) as u32, MOUSEEVENTF_HWHEEL);
            }
            ok
        }
        InputEvent::Key {
            vk,
            scan_code,
            extended,
            pressed,
        } => {
            let mut flags = Default::default();
            if *vk == 0 && *scan_code != 0 {
                flags |= KEYEVENTF_SCANCODE;
            }
            if *extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !*pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            let input = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(*vk),
                        wScan: *scan_code,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let inserted = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
            inserted > 0
        }
        InputEvent::SendSas => {
            if let Err(e) = crate::service::desktop::send_sas_ctrl_alt_del() {
                tracing::warn!("SendSAS failed (requires SYSTEM service mode): {e}");
            }
            true
        }
    }
}

fn virtual_screen_rect() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

fn send_mouse(
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) -> bool {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inserted = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    inserted > 0
}
