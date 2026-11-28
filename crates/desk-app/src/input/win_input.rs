use protocol::session::{InputEvent, MonitorInfo, MouseBtn};
use windows::Win32::System::StationsAndDesktops::{GetThreadDesktop, SetThreadDesktop, HDESK};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD,
    INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const XBUTTON1_DATA: u32 = 1;
const XBUTTON2_DATA: u32 = 2;
const VK_CAPITAL: u16 = 0x14;
const VK_NUMLOCK: u16 = 0x90;
const VK_SCROLL: u16 = 0x91;

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
            let input = make_key_input(*vk, *scan_code, *extended, *pressed);
            let inserted = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
            inserted > 0
        }
        InputEvent::SendSas => {
            if let Err(e) = crate::service::desktop::send_sas_ctrl_alt_del() {
                tracing::warn!("SendSAS failed (requires SYSTEM service mode): {e}");
            }
            true
        }
        InputEvent::SyncLockKeys {
            caps_lock,
            num_lock,
            scroll_lock,
        } => sync_lock_keys_with(
            (*caps_lock, *num_lock, *scroll_lock),
            current_lock_states,
            |inputs| {
                let inserted = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
                inserted == inputs.len() as u32
            },
        ),
    }
}

fn make_key_input(vk: u16, scan_code: u16, extended: bool, pressed: bool) -> INPUT {
    let mut flags = Default::default();
    if vk == 0 && scan_code != 0 {
        flags |= KEYEVENTF_SCANCODE;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan_code,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn sync_lock_keys_with<R, S>(desired: (bool, bool, bool), read_state: R, send_inputs: S) -> bool
where
    R: FnOnce() -> (bool, bool, bool),
    S: FnOnce(&[INPUT]) -> bool,
{
    let flips = lock_keys_to_toggle(read_state(), desired);
    if flips.is_empty() {
        return true;
    }
    let inputs = build_lock_toggle_inputs(&flips);
    send_inputs(&inputs)
}

fn read_lock_toggles_on_thread() -> (bool, bool, bool) {
    let mut keys = [0u8; 256];
    // GetKeyboardState issues NtUserGetKeyboardState directly rather than hitting
    // user32!GetKeyState's Win32ClientInfo+0x874 cache (which covers vk < 0x20, incl. VK_CAPITAL).
    if unsafe { GetKeyboardState(&mut keys) }.is_ok() {
        return (
            (keys[usize::from(VK_CAPITAL)] & 1) != 0,
            (keys[usize::from(VK_NUMLOCK)] & 1) != 0,
            (keys[usize::from(VK_SCROLL)] & 1) != 0,
        );
    }
    unsafe {
        (
            (GetKeyState(i32::from(VK_CAPITAL)) & 1) != 0,
            (GetKeyState(i32::from(VK_NUMLOCK)) & 1) != 0,
            (GetKeyState(i32::from(VK_SCROLL)) & 1) != 0,
        )
    }
}

/// Reads the active desktop's lock-key toggle states without binding the long-lived, pump-less
/// `flashdesk-host-input` thread to a stale `THREADINFO`/`pq->afKeyState` or blocking future
/// `SetThreadDesktop` switches across `Default` <-> `Winlogon` in agent mode.
fn current_lock_states() -> (bool, bool, bool) {
    let desk_raw = unsafe {
        GetThreadDesktop(GetCurrentThreadId())
            .ok()
            .map(|h| h.0 as usize)
            .unwrap_or(0)
    };
    let handle = std::thread::Builder::new()
        .name("flashdesk-lock-probe".into())
        .spawn(move || {
            if desk_raw != 0 {
                let _ = unsafe { SetThreadDesktop(HDESK(desk_raw as *mut _)) };
            }
            read_lock_toggles_on_thread()
        });
    match handle {
        Ok(h) => h.join().unwrap_or_else(|_| read_lock_toggles_on_thread()),
        Err(_) => read_lock_toggles_on_thread(),
    }
}

fn lock_keys_to_toggle(
    current: (bool, bool, bool),
    desired: (bool, bool, bool),
) -> Vec<(u16, bool)> {
    let mut flips = Vec::new();
    if current.0 != desired.0 {
        flips.push((VK_CAPITAL, false));
    }
    if current.1 != desired.1 {
        flips.push((VK_NUMLOCK, true));
    }
    if current.2 != desired.2 {
        flips.push((VK_SCROLL, false));
    }
    flips
}

fn lock_key_scan_code(vk: u16) -> u16 {
    let mapped = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) as u16 };
    if mapped != 0 {
        return mapped;
    }
    match vk {
        VK_CAPITAL => 0x3A,
        VK_NUMLOCK => 0x45,
        VK_SCROLL => 0x46,
        _ => 0,
    }
}

fn build_lock_toggle_inputs(flips: &[(u16, bool)]) -> Vec<INPUT> {
    let mut inputs = Vec::with_capacity(flips.len() * 2);
    for &(vk, extended) in flips {
        let scan_code = lock_key_scan_code(vk);
        inputs.push(make_key_input(vk, scan_code, extended, true));
        inputs.push(make_key_input(vk, scan_code, extended, false));
    }
    inputs
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_keys_to_toggle_only_flips_mismatched_keys() {
        assert!(lock_keys_to_toggle((true, false, true), (true, false, true)).is_empty());
        assert_eq!(
            lock_keys_to_toggle((false, true, false), (true, false, true)),
            vec![(VK_CAPITAL, false), (VK_NUMLOCK, true), (VK_SCROLL, false)]
        );
        assert_eq!(
            lock_keys_to_toggle((false, true, false), (true, true, false)),
            vec![(VK_CAPITAL, false)]
        );
    }

    #[test]
    fn build_lock_toggle_inputs_sets_extended_and_keyup_flags() {
        let inputs = build_lock_toggle_inputs(&[
            (VK_CAPITAL, false),
            (VK_NUMLOCK, true),
            (VK_SCROLL, false),
        ]);
        assert_eq!(inputs.len(), 6);
        unsafe {
            assert_eq!(inputs[0].Anonymous.ki.wVk.0, VK_CAPITAL);
            assert_eq!(inputs[0].Anonymous.ki.wScan, 0x3A);
            assert_eq!(inputs[0].Anonymous.ki.dwFlags, Default::default());
            assert_eq!(inputs[1].Anonymous.ki.wVk.0, VK_CAPITAL);
            assert_eq!(inputs[1].Anonymous.ki.wScan, 0x3A);
            assert_eq!(inputs[1].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP);

            assert_eq!(inputs[2].Anonymous.ki.wVk.0, VK_NUMLOCK);
            assert_eq!(inputs[2].Anonymous.ki.wScan, 0x45);
            assert_eq!(inputs[2].Anonymous.ki.dwFlags, KEYEVENTF_EXTENDEDKEY);
            assert_eq!(inputs[3].Anonymous.ki.wVk.0, VK_NUMLOCK);
            assert_eq!(inputs[3].Anonymous.ki.wScan, 0x45);
            assert_eq!(
                inputs[3].Anonymous.ki.dwFlags,
                KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
            );

            assert_eq!(inputs[4].Anonymous.ki.wVk.0, VK_SCROLL);
            assert_eq!(inputs[4].Anonymous.ki.wScan, 0x46);
            assert_eq!(inputs[4].Anonymous.ki.dwFlags, Default::default());
            assert_eq!(inputs[5].Anonymous.ki.wVk.0, VK_SCROLL);
            assert_eq!(inputs[5].Anonymous.ki.wScan, 0x46);
            assert_eq!(inputs[5].Anonymous.ki.dwFlags, KEYEVENTF_KEYUP);
        }
    }

    #[test]
    fn sync_lock_keys_with_handles_noop_and_consecutive_flips() {
        // Matching desired state sends zero inputs.
        let mut called = false;
        assert!(sync_lock_keys_with(
            (true, false, true),
            || (true, false, true),
            |_| {
                called = true;
                true
            }
        ));
        assert!(!called);

        // Caps Lock off -> on toggles VK_CAPITAL once (down + up with scan 0x3A).
        let mut sent_on = Vec::new();
        assert!(sync_lock_keys_with(
            (true, true, false),
            || (false, true, false),
            |inputs| {
                sent_on = inputs
                    .iter()
                    .map(|i| unsafe { (i.Anonymous.ki.wVk.0, i.Anonymous.ki.wScan, i.Anonymous.ki.dwFlags.0) })
                    .collect();
                true
            }
        ));
        assert_eq!(
            sent_on,
            vec![
                (VK_CAPITAL, 0x3A, 0),
                (VK_CAPITAL, 0x3A, KEYEVENTF_KEYUP.0),
            ]
        );

        // Consecutive flip Caps Lock on -> off toggles VK_CAPITAL back cleanly.
        let mut sent_off = Vec::new();
        assert!(sync_lock_keys_with(
            (false, true, false),
            || (true, true, false),
            |inputs| {
                sent_off = inputs
                    .iter()
                    .map(|i| unsafe { (i.Anonymous.ki.wVk.0, i.Anonymous.ki.wScan, i.Anonymous.ki.dwFlags.0) })
                    .collect();
                true
            }
        ));
        assert_eq!(
            sent_off,
            vec![
                (VK_CAPITAL, 0x3A, 0),
                (VK_CAPITAL, 0x3A, KEYEVENTF_KEYUP.0),
            ]
        );
    }

    #[test]
    fn current_lock_states_from_pumpless_thread_matches_live_and_preserves_set_thread_desktop() {
        let baseline = std::thread::spawn(read_lock_toggles_on_thread)
            .join()
            .expect("baseline probe panicked");

        let (reads, set_desk_ok) = std::thread::Builder::new()
            .name("flashdesk-pumpless-test".into())
            .spawn(|| {
                let mut results = Vec::new();
                for _ in 0..3 {
                    results.push(current_lock_states());
                }
                let set_ok = unsafe {
                    GetThreadDesktop(GetCurrentThreadId())
                        .and_then(|d| SetThreadDesktop(d))
                        .is_ok()
                };
                (results, set_ok)
            })
            .expect("spawn failed")
            .join()
            .expect("pumpless test thread panicked");

        assert!(
            set_desk_ok,
            "current_lock_states must leave calling thread able to call SetThreadDesktop"
        );
        assert_eq!(reads, vec![baseline, baseline, baseline]);
    }

    #[test]
    fn sync_lock_keys_noop_when_matching_live_state() {
        let (caps, num, scroll) = current_lock_states();
        assert!(inject_input(
            &InputEvent::SyncLockKeys {
                caps_lock: caps,
                num_lock: num,
                scroll_lock: scroll,
            },
            None,
            false
        ));
    }
}
