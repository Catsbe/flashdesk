use anyhow::Result;
use protocol::session::MonitorInfo;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ERROR_NOT_FOUND};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

#[derive(Debug, Clone)]
pub struct MonitorTarget {
    pub adapter_idx: u32,
    pub output_idx: u32,
    pub info: MonitorInfo,
}

pub fn enumerate_monitors() -> Result<Vec<MonitorTarget>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut targets = Vec::new();
    let mut display_index = 0u32;

    for adapter_idx in 0..16 {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_idx) } {
            Ok(a) => a,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };

        for output_idx in 0..16 {
            let output = match unsafe { adapter.EnumOutputs(output_idx) } {
                Ok(o) => o,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };

            let desc = unsafe { output.GetDesc()? };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }

            let rect = desc.DesktopCoordinates;
            let width = (rect.right - rect.left).max(0) as u32;
            let height = (rect.bottom - rect.top).max(0) as u32;
            if width == 0 || height == 0 {
                continue;
            }

            let mut dpi_x = 96u32;
            let mut dpi_y = 96u32;
            unsafe {
                let _ = GetDpiForMonitor(desc.Monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            }

            let raw_name = &desc.DeviceName;
            let end = raw_name
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(raw_name.len());
            let name = String::from_utf16_lossy(&raw_name[..end]);

            targets.push(MonitorTarget {
                adapter_idx,
                output_idx,
                info: MonitorInfo {
                    index: display_index,
                    name,
                    left: rect.left,
                    top: rect.top,
                    width,
                    height,
                    dpi: dpi_x,
                    is_primary: rect.left == 0 && rect.top == 0,
                },
            });
            display_index += 1;
        }
    }

    Ok(targets)
}
