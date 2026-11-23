use anyhow::{Context, Result};
use protocol::session::{VddDriverKind, VddInfo, VddRequest};
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsExW, CDS_UPDATEREGISTRY,
    DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ACTIVE, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT,
    DM_PELSWIDTH, DISP_CHANGE_SUCCESSFUL, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
    ENUM_REGISTRY_SETTINGS,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub const PARSEC_VDD_DOWNLOAD_URL: &str =
    "https://github.com/nomi-san/parsec-vdd/releases";
pub const MTT_VDD_DOWNLOAD_URL: &str =
    "https://github.com/VirtualDrivers/Virtual-Display-Driver/releases";

pub const PARSEC_VDD_INTERFACE_GUID: GUID =
    GUID::from_u128(0x00b41627_04c4_429e_a26e_0265cf50c8fa);

const IOCTL_VDD_ADD: u32 = 0x0022_e004;
const IOCTL_VDD_REMOVE: u32 = 0x0022_a008;
const IOCTL_VDD_UPDATE: u32 = 0x0022_a00c;

static GLOBAL_VDD: OnceLock<Mutex<Option<ParsecVddController>>> = OnceLock::new();

fn global_vdd_slot() -> &'static Mutex<Option<ParsecVddController>> {
    GLOBAL_VDD.get_or_init(|| Mutex::new(None))
}

fn to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn utf16_slice_to_string(slice: &[u16]) -> String {
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..end])
}

fn has_parsec_vdd_device() -> bool {
    open_parsec_vdd_path().is_ok()
}

fn open_parsec_vdd_path() -> Result<Vec<u16>> {
    unsafe {
        let dev_info = SetupDiGetClassDevsW(
            Some(&PARSEC_VDD_INTERFACE_GUID),
            PCWSTR::null(),
            HWND::default(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .context("SetupDiGetClassDevsW failed")?;

        struct InfoGuard(windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO);
        impl Drop for InfoGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = SetupDiDestroyDeviceInfoList(self.0);
                }
            }
        }
        let _guard = InfoGuard(dev_info);

        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        SetupDiEnumDeviceInterfaces(dev_info, None, &PARSEC_VDD_INTERFACE_GUID, 0, &mut iface)
            .context("未检测到 Parsec VDD 设备接口")?;

        let mut required = 0u32;
        let _ = SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            &iface,
            None,
            0,
            Some(&mut required),
            None,
        );
        anyhow::ensure!(
            required >= std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32,
            "invalid SetupDiGetDeviceInterfaceDetailW buffer size"
        );

        let mut buf = vec![0u8; required as usize];
        let detail = &mut *(buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W);
        detail.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

        SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            &iface,
            Some(detail),
            required,
            None,
            None,
        )
        .context("SetupDiGetDeviceInterfaceDetailW failed")?;

        let path_ptr = detail.DevicePath.as_ptr();
        let mut len = 0usize;
        while *path_ptr.add(len) != 0 {
            len += 1;
        }
        let mut path = std::slice::from_raw_parts(path_ptr, len).to_vec();
        path.push(0);
        Ok(path)
    }
}

pub fn detect_vdd_status() -> VddInfo {
    let mut parsec_present = has_parsec_vdd_device();
    let mut mtt_present = false;
    let mut third_party: Option<String> = None;
    let mut active_virtual = 0u32;
    let mut virtual_devices = Vec::new();

    unsafe {
        for dev_idx in 0..32u32 {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), dev_idx, &mut dd, 0).as_bool() {
                break;
            }
            let dev_name = utf16_slice_to_string(&dd.DeviceName);
            let dev_str = utf16_slice_to_string(&dd.DeviceString);
            let lower = dev_str.to_ascii_lowercase();
            let active = (dd.StateFlags & DISPLAY_DEVICE_ACTIVE) != 0;

            if lower.contains("parsec") {
                parsec_present = true;
                if active {
                    active_virtual += 1;
                    virtual_devices.push(format!("{dev_name} ({dev_str})"));
                }
            } else if lower.contains("virtual display driver")
                || lower.contains("iddsampledriver")
                || lower.contains("mtt")
                || lower.contains("virtual display")
            {
                mtt_present = true;
                if active {
                    active_virtual += 1;
                    virtual_devices.push(format!("{dev_name} ({dev_str})"));
                }
            } else if lower.contains("orayidddriver") {
                third_party = Some(dev_str.clone());
                if active {
                    active_virtual += 1;
                    virtual_devices.push(format!("{dev_name} ({dev_str})"));
                }
            }
        }
    }

    let driver = if parsec_present {
        VddDriverKind::ParsecVdd
    } else if mtt_present {
        VddDriverKind::VirtualDisplayDriver
    } else if let Some(tp) = third_party {
        VddDriverKind::ThirdParty(tp)
    } else {
        VddDriverKind::None
    };

    let details = if virtual_devices.is_empty() {
        driver.label()
    } else {
        format!("{} | 活动虚拟屏: {}", driver.label(), virtual_devices.join(", "))
    };

    VddInfo {
        driver,
        active_virtual_displays: active_virtual,
        details,
    }
}

pub struct ParsecVddController {
    handle: isize,
    added_indices: Vec<u32>,
    stop_hb: Arc<AtomicBool>,
    hb_thread: Option<std::thread::JoinHandle<()>>,
}

impl ParsecVddController {
    pub fn open() -> Result<Self> {
        let path_w = open_parsec_vdd_path()?;
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path_w.as_ptr()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .context("打开 Parsec VDD 设备句柄失败")?
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
            anyhow::bail!("Parsec VDD 句柄无效");
        }

        let stop_hb = Arc::new(AtomicBool::new(false));
        let raw_handle = handle.0 as isize;
        let stop_clone = Arc::clone(&stop_hb);
        let hb_thread = std::thread::Builder::new()
            .name("flashdesk-vdd-hb".into())
            .spawn(move || {
                while !stop_clone.load(Ordering::Relaxed) {
                    let input: [u8; 4] = [0; 4];
                    let mut bytes = 0u32;
                    unsafe {
                        let _ = DeviceIoControl(
                            HANDLE(raw_handle as *mut c_void),
                            IOCTL_VDD_UPDATE,
                            Some(input.as_ptr() as *const c_void),
                            4,
                            None,
                            0,
                            Some(&mut bytes),
                            None,
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            })
            .ok();

        Ok(Self {
            handle: raw_handle,
            added_indices: Vec::new(),
            stop_hb,
            hb_thread,
        })
    }

    pub fn add_display(&mut self) -> Result<u32> {
        let input: [u8; 4] = [0; 4];
        let mut out_index: u32 = 0;
        let mut bytes_returned = 0u32;
        unsafe {
            DeviceIoControl(
                HANDLE(self.handle as *mut c_void),
                IOCTL_VDD_ADD,
                Some(input.as_ptr() as *const c_void),
                4,
                Some(&mut out_index as *mut u32 as *mut c_void),
                4,
                Some(&mut bytes_returned),
                None,
            )
            .context("Parsec VDD IOCTL_VDD_ADD 失败")?;
        }
        self.added_indices.push(out_index);
        Ok(out_index)
    }

    pub fn remove_all(&mut self) -> Result<()> {
        let indices = std::mem::take(&mut self.added_indices);
        for idx in indices {
            let mut bytes_returned = 0u32;
            let in_bytes = idx.to_le_bytes();
            unsafe {
                let _ = DeviceIoControl(
                    HANDLE(self.handle as *mut c_void),
                    IOCTL_VDD_REMOVE,
                    Some(in_bytes.as_ptr() as *const c_void),
                    4,
                    None,
                    0,
                    Some(&mut bytes_returned),
                    None,
                );
            }
        }
        Ok(())
    }
}

impl Drop for ParsecVddController {
    fn drop(&mut self) {
        let _ = self.remove_all();
        self.stop_hb.store(true, Ordering::Relaxed);
        if let Some(t) = self.hb_thread.take() {
            let _ = t.join();
        }
        unsafe {
            let _ = CloseHandle(HANDLE(self.handle as *mut c_void));
        }
    }
}

pub fn handle_vdd_request(req: &VddRequest) -> Result<VddInfo> {
    match req {
        VddRequest::Query => Ok(detect_vdd_status()),
        VddRequest::AddDisplay {
            width,
            height,
            refresh_hz,
        } => {
            let mut slot = global_vdd_slot().lock().unwrap();
            if slot.is_none() {
                *slot = Some(ParsecVddController::open()?);
            }
            if let Some(ctrl) = slot.as_mut() {
                let _idx = ctrl.add_display()?;
                std::thread::sleep(std::time::Duration::from_millis(350));
                if let Some(dev_name) = find_first_virtual_display_name() {
                    let _ = set_display_mode(&dev_name, *width, *height, *refresh_hz);
                }
            }
            Ok(detect_vdd_status())
        }
        VddRequest::RemoveDisplay => {
            let mut slot = global_vdd_slot().lock().unwrap();
            *slot = None; // Drop triggers remove_all() + handle close
            std::thread::sleep(std::time::Duration::from_millis(250));
            Ok(detect_vdd_status())
        }
        VddRequest::SetMode {
            device_name,
            width,
            height,
            refresh_hz,
        } => {
            let target = if device_name.trim().is_empty() {
                find_first_virtual_display_name().context("未找到活动的虚拟显示器设备")?
            } else {
                device_name.clone()
            };
            set_display_mode(&target, *width, *height, *refresh_hz)?;
            Ok(detect_vdd_status())
        }
    }
}

pub fn find_first_virtual_display_name() -> Option<String> {
    unsafe {
        for dev_idx in 0..32u32 {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), dev_idx, &mut dd, 0).as_bool() {
                break;
            }
            if (dd.StateFlags & DISPLAY_DEVICE_ACTIVE) == 0 {
                continue;
            }
            let dev_str = utf16_slice_to_string(&dd.DeviceString).to_ascii_lowercase();
            if dev_str.contains("parsec")
                || dev_str.contains("virtual display")
                || dev_str.contains("iddsampledriver")
                || dev_str.contains("orayidddriver")
            {
                return Some(utf16_slice_to_string(&dd.DeviceName));
            }
        }
    }
    None
}

pub fn set_display_mode(
    device_name: &str,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Result<()> {
    let dev_w = to_wide_null(OsStr::new(device_name));
    unsafe {
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let mut ok = EnumDisplaySettingsExW(
            PCWSTR(dev_w.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
            Default::default(),
        )
        .as_bool();
        if !ok {
            ok = EnumDisplaySettingsExW(
                PCWSTR(dev_w.as_ptr()),
                ENUM_REGISTRY_SETTINGS,
                &mut dm,
                Default::default(),
            )
            .as_bool();
        }
        if !ok {
            let _ = ENUM_DISPLAY_SETTINGS_MODE(0);
            anyhow::bail!("EnumDisplaySettingsExW failed for {}", device_name);
        }

        dm.dmPelsWidth = width;
        dm.dmPelsHeight = height;
        dm.dmDisplayFrequency = refresh_hz;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;

        let res = ChangeDisplaySettingsExW(
            PCWSTR(dev_w.as_ptr()),
            Some(&dm),
            HWND::default(),
            CDS_UPDATEREGISTRY,
            None,
        );
        anyhow::ensure!(
            res == DISP_CHANGE_SUCCESSFUL,
            "ChangeDisplaySettingsExW({}) failed with code {}",
            device_name,
            res.0
        );
    }
    Ok(())
}

pub fn open_url(url: &str) {
    let url_w = to_wide_null(OsStr::new(url));
    let verb_w = to_wide_null(OsStr::new("open"));
    unsafe {
        let _ = ShellExecuteW(
            HWND::default(),
            PCWSTR(verb_w.as_ptr()),
            PCWSTR(url_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
