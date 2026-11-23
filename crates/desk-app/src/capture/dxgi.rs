use crate::capture::monitors::{enumerate_monitors, MonitorTarget};
use crate::codec::{bgra_to_nv12, RawFrame};
use anyhow::{Context, Result};
use std::time::Instant;
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP,
    HBRUSH, HDC, HGDIOBJ, ROP_CODE, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, HICON,
    ICONINFO,
};

#[derive(Debug)]
pub enum CaptureOutcome {
    Frame(RawFrame),
    Unchanged,
    Reacquired,
}

struct ReleaseGuard<'a>(&'a IDXGIOutputDuplication);

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.ReleaseFrame();
        }
    }
}

pub struct DxgiSingleCapturer {
    pub target: MonitorTarget,
    agent_mode: bool,
    width: u32,
    height: u32,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    has_first_frame: bool,
    last_bgra: Vec<u8>,
    start_time: Instant,
}

impl DxgiSingleCapturer {
    pub fn new(target: MonitorTarget, agent_mode: bool) -> Result<Self> {
        if agent_mode {
            let _ = crate::service::desktop::sync_thread_to_input_desktop();
        }
        let (width, height, device, context, duplication, staging) =
            init_duplication(target.adapter_idx, target.output_idx)?;
        Ok(Self {
            target,
            agent_mode,
            width,
            height,
            device,
            context,
            duplication,
            staging,
            has_first_frame: false,
            last_bgra: Vec::new(),
            start_time: Instant::now(),
        })
    }

    pub fn even_dimensions(&self) -> (u32, u32) {
        ((self.width & !1).max(2), (self.height & !1).max(2))
    }

    fn rebuild(&mut self) -> Result<()> {
        if self.agent_mode {
            let _ = crate::service::desktop::sync_thread_to_input_desktop();
        }
        let (width, height, device, context, duplication, staging) =
            init_duplication(self.target.adapter_idx, self.target.output_idx)?;
        self.width = width;
        self.height = height;
        self.device = device;
        self.context = context;
        self.duplication = duplication;
        self.staging = staging;
        self.has_first_frame = false;
        Ok(())
    }

    /// Captures latest BGRA frame into `self.last_bgra`.
    fn acquire_bgra(&mut self, timeout_ms: u32) -> Result<AcquireState> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let acquire_res = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };

        if let Err(e) = acquire_res {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(AcquireState::Unchanged);
            }
            if e.code() == DXGI_ERROR_ACCESS_LOST
                || e.code() == DXGI_ERROR_INVALID_CALL
                || (self.agent_mode && e.code().0 as u32 == 0x8007_0005)
            {
                self.rebuild()?;
                return Ok(AcquireState::Reacquired);
            }
            return Err(e.into());
        }

        let _guard = ReleaseGuard(&self.duplication);

        if self.has_first_frame
            && frame_info.LastPresentTime == 0
            && frame_info.AccumulatedFrames == 0
        {
            return Ok(AcquireState::Unchanged);
        }

        let Some(res) = resource else {
            return Ok(AcquireState::Unchanged);
        };
        let tex: ID3D11Texture2D = res.cast()?;

        unsafe {
            self.context.CopyResource(&self.staging, &tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            let row_bytes = self.width as usize * 4;
            let total_bytes = row_bytes * self.height as usize;
            if self.last_bgra.len() != total_bytes {
                self.last_bgra.resize(total_bytes, 0);
            }
            let pitch = mapped.RowPitch as usize;
            let src_ptr = mapped.pData as *const u8;
            for y in 0..self.height as usize {
                let src_row = std::slice::from_raw_parts(src_ptr.add(y * pitch), row_bytes);
                let dst_row = &mut self.last_bgra[y * row_bytes..(y + 1) * row_bytes];
                dst_row.copy_from_slice(src_row);
            }
            self.context.Unmap(&self.staging, 0);
        }

        self.has_first_frame = true;
        Ok(AcquireState::Updated)
    }

    pub fn capture_next(&mut self, timeout_ms: u32, force_frame: bool) -> Result<CaptureOutcome> {
        let content_changed = match self.acquire_bgra(timeout_ms)? {
            AcquireState::Updated => true,
            AcquireState::Unchanged => {
                if !force_frame || self.last_bgra.is_empty() {
                    return Ok(CaptureOutcome::Unchanged);
                }
                false
            }
            AcquireState::Reacquired => return Ok(CaptureOutcome::Reacquired),
        };

        let (ew, eh) = self.even_dimensions();
        let nv12 = bgra_to_nv12(&self.last_bgra, self.width, self.height);
        Ok(CaptureOutcome::Frame(RawFrame {
            width: ew,
            height: eh,
            nv12,
            timestamp_us: self.start_time.elapsed().as_micros() as u64,
            content_changed,
        }))
    }
}

/// High-speed GDI `BitBlt` capturer used automatically when DXGI Desktop Duplication is rejected
/// (e.g. hybrid-GPU laptop display routing, HDR/10-bit surfaces, RDP/virtual displays, or slot limits).
pub struct GdiSingleCapturer {
    pub target: MonitorTarget,
    agent_mode: bool,
    width: u32,
    height: u32,
    mem_dc: HDC,
    dib: HBITMAP,
    old_obj: HGDIOBJ,
    dib_bits: *mut u8,
    has_first_frame: bool,
    last_bgra: Vec<u8>,
    start_time: Instant,
}

impl Drop for GdiSingleCapturer {
    fn drop(&mut self) {
        unsafe {
            if !self.mem_dc.0.is_null() {
                if !self.old_obj.0.is_null() {
                    let _ = SelectObject(self.mem_dc, self.old_obj);
                }
                let _ = DeleteDC(self.mem_dc);
            }
            if !self.dib.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.dib.0));
            }
        }
    }
}

impl GdiSingleCapturer {
    pub fn new(target: MonitorTarget, agent_mode: bool) -> Result<Self> {
        if agent_mode {
            let _ = crate::service::desktop::sync_thread_to_input_desktop();
        }
        let width = target.info.width.max(2);
        let height = target.info.height.max(2);

        let mem_dc = unsafe { CreateCompatibleDC(HDC::default()) };
        anyhow::ensure!(!mem_dc.0.is_null(), "CreateCompatibleDC failed");

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32), // top-down DIB matching screen scanline order
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };

        let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib_res = unsafe {
            CreateDIBSection(
                mem_dc,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                windows::Win32::Foundation::HANDLE::default(),
                0,
            )
        };
        let dib = match dib_res {
            Ok(h) if !h.0.is_null() && !bits_ptr.is_null() => h,
            Ok(h) => {
                unsafe {
                    if !h.0.is_null() {
                        let _ = DeleteObject(HGDIOBJ(h.0));
                    }
                    let _ = DeleteDC(mem_dc);
                }
                anyhow::bail!("CreateDIBSection returned null buffer pointer");
            }
            Err(e) => {
                unsafe {
                    let _ = DeleteDC(mem_dc);
                }
                return Err(e).context("CreateDIBSection failed");
            }
        };

        let old_obj = unsafe { SelectObject(mem_dc, HGDIOBJ(dib.0)) };

        Ok(Self {
            target,
            agent_mode,
            width,
            height,
            mem_dc,
            dib,
            old_obj,
            dib_bits: bits_ptr as *mut u8,
            has_first_frame: false,
            last_bgra: Vec::new(),
            start_time: Instant::now(),
        })
    }

    pub fn even_dimensions(&self) -> (u32, u32) {
        ((self.width & !1).max(2), (self.height & !1).max(2))
    }

    fn draw_cursor(&self) {
        unsafe {
            let mut ci = CURSORINFO {
                cbSize: std::mem::size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            if GetCursorInfo(&mut ci).is_err() {
                return;
            }
            if (ci.flags.0 & CURSOR_SHOWING.0) == 0 || ci.hCursor.0.is_null() {
                return;
            }
            let hicon = HICON(ci.hCursor.0);
            let mut icon_info = ICONINFO::default();
            let mut hot_x = 0i32;
            let mut hot_y = 0i32;
            if GetIconInfo(hicon, &mut icon_info).is_ok() {
                hot_x = icon_info.xHotspot as i32;
                hot_y = icon_info.yHotspot as i32;
                if !icon_info.hbmMask.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
                }
                if !icon_info.hbmColor.0.is_null() {
                    let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
                }
            }

            let draw_x = ci.ptScreenPos.x - self.target.info.left - hot_x;
            let draw_y = ci.ptScreenPos.y - self.target.info.top - hot_y;
            if draw_x > -128
                && draw_x < self.width as i32 + 128
                && draw_y > -128
                && draw_y < self.height as i32 + 128
            {
                let _ = DrawIconEx(
                    self.mem_dc,
                    draw_x,
                    draw_y,
                    hicon,
                    0,
                    0,
                    0,
                    HBRUSH::default(),
                    DI_NORMAL,
                );
            }
        }
    }

    fn acquire_bgra(&mut self) -> Result<AcquireState> {
        if self.agent_mode {
            let _ = crate::service::desktop::sync_thread_to_input_desktop();
        }
        let w = self.width as i32;
        let h = self.height as i32;

        unsafe {
            let screen_dc = GetDC(HWND::default());
            anyhow::ensure!(!screen_dc.0.is_null(), "GetDC(NULL) returned null");

            let blit_res = BitBlt(
                self.mem_dc,
                0,
                0,
                w,
                h,
                screen_dc,
                self.target.info.left,
                self.target.info.top,
                ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
            )
            .or_else(|_| {
                BitBlt(
                    self.mem_dc,
                    0,
                    0,
                    w,
                    h,
                    screen_dc,
                    self.target.info.left,
                    self.target.info.top,
                    SRCCOPY,
                )
            });

            let _ = ReleaseDC(HWND::default(), screen_dc);
            blit_res.context("GDI BitBlt failed")?;
        }

        self.draw_cursor();

        let total_bytes = self.width as usize * self.height as usize * 4;
        let dib_slice = unsafe { std::slice::from_raw_parts(self.dib_bits, total_bytes) };
        if self.has_first_frame && self.last_bgra.as_slice() == dib_slice {
            return Ok(AcquireState::Unchanged);
        }
        if self.last_bgra.len() != total_bytes {
            self.last_bgra.resize(total_bytes, 0);
        }
        self.last_bgra.copy_from_slice(dib_slice);
        self.has_first_frame = true;
        Ok(AcquireState::Updated)
    }

    pub fn capture_next(&mut self, force_frame: bool) -> Result<CaptureOutcome> {
        let content_changed = match self.acquire_bgra()? {
            AcquireState::Updated => true,
            AcquireState::Unchanged => {
                if !force_frame || self.last_bgra.is_empty() {
                    return Ok(CaptureOutcome::Unchanged);
                }
                false
            }
            AcquireState::Reacquired => return Ok(CaptureOutcome::Reacquired),
        };

        let (ew, eh) = self.even_dimensions();
        let nv12 = bgra_to_nv12(&self.last_bgra, self.width, self.height);
        Ok(CaptureOutcome::Frame(RawFrame {
            width: ew,
            height: eh,
            nv12,
            timestamp_us: self.start_time.elapsed().as_micros() as u64,
            content_changed,
        }))
    }
}

pub enum SingleCapturer {
    Dxgi(DxgiSingleCapturer),
    Gdi(GdiSingleCapturer),
}

impl SingleCapturer {
    pub fn new(target: MonitorTarget, agent_mode: bool) -> Result<Self> {
        match DxgiSingleCapturer::new(target.clone(), agent_mode) {
            Ok(dxgi) => Ok(Self::Dxgi(dxgi)),
            Err(dxgi_err) => {
                tracing::warn!(
                    "DXGI capture unavailable for {} ({dxgi_err:#}), falling back to GDI BitBlt",
                    target.info.name
                );
                let gdi = GdiSingleCapturer::new(target, agent_mode)
                    .with_context(|| format!("DXGI ({dxgi_err:#}) and GDI fallback both failed"))?;
                Ok(Self::Gdi(gdi))
            }
        }
    }

    fn target(&self) -> &MonitorTarget {
        match self {
            SingleCapturer::Dxgi(d) => &d.target,
            SingleCapturer::Gdi(g) => &g.target,
        }
    }

    fn raw_dimensions(&self) -> (u32, u32) {
        match self {
            SingleCapturer::Dxgi(d) => (d.width, d.height),
            SingleCapturer::Gdi(g) => (g.width, g.height),
        }
    }

    pub fn even_dimensions(&self) -> (u32, u32) {
        match self {
            SingleCapturer::Dxgi(d) => d.even_dimensions(),
            SingleCapturer::Gdi(g) => g.even_dimensions(),
        }
    }

    fn last_bgra(&self) -> &[u8] {
        match self {
            SingleCapturer::Dxgi(d) => &d.last_bgra,
            SingleCapturer::Gdi(g) => &g.last_bgra,
        }
    }

    fn is_gdi(&self) -> bool {
        matches!(self, SingleCapturer::Gdi(_))
    }

    fn acquire_bgra(&mut self, timeout_ms: u32) -> Result<AcquireState> {
        match self {
            SingleCapturer::Dxgi(d) => match d.acquire_bgra(timeout_ms) {
                Ok(state) => Ok(state),
                Err(e) => {
                    let target = d.target.clone();
                    let agent_mode = d.agent_mode;
                    tracing::warn!(
                        "DXGI runtime capture failed ({e:#}), switching to GDI BitBlt fallback"
                    );
                    let mut gdi = GdiSingleCapturer::new(target, agent_mode)?;
                    let st = gdi.acquire_bgra()?;
                    *self = SingleCapturer::Gdi(gdi);
                    Ok(st)
                }
            },
            SingleCapturer::Gdi(g) => g.acquire_bgra(),
        }
    }

    pub fn capture_next(&mut self, timeout_ms: u32, force_frame: bool) -> Result<CaptureOutcome> {
        match self {
            SingleCapturer::Dxgi(d) => match d.capture_next(timeout_ms, force_frame) {
                Ok(out) => Ok(out),
                Err(e) => {
                    let target = d.target.clone();
                    let agent_mode = d.agent_mode;
                    tracing::warn!(
                        "DXGI runtime capture failed ({e:#}), switching to GDI BitBlt fallback"
                    );
                    let mut gdi = GdiSingleCapturer::new(target, agent_mode)?;
                    let out = gdi.capture_next(force_frame)?;
                    *self = SingleCapturer::Gdi(gdi);
                    Ok(out)
                }
            },
            SingleCapturer::Gdi(g) => g.capture_next(force_frame),
        }
    }
}

enum AcquireState {
    Updated,
    Unchanged,
    Reacquired,
}

fn init_duplication(
    adapter_idx: u32,
    output_idx: u32,
) -> Result<(
    u32,
    u32,
    ID3D11Device,
    ID3D11DeviceContext,
    IDXGIOutputDuplication,
    ID3D11Texture2D,
)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let adapter = unsafe { factory.EnumAdapters1(adapter_idx)? };
    let output = unsafe { adapter.EnumOutputs(output_idx)? };
    let output1: IDXGIOutput1 = output.cast()?;

    let adapter_base: IDXGIAdapter = adapter.cast()?;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;

    unsafe {
        D3D11CreateDevice(
            &adapter_base,
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice failed on target DXGI adapter")?;
    }

    let device = device.context("D3D11CreateDevice returned null device")?;
    let context = context.context("D3D11CreateDevice returned null context")?;
    let duplication = unsafe {
        if let Ok(output5) = output.cast::<IDXGIOutput5>() {
            match output5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) {
                Ok(dupl) => dupl,
                Err(_) => output1
                    .DuplicateOutput(&device)
                    .context("IDXGIOutput1::DuplicateOutput failed")?,
            }
        } else {
            output1
                .DuplicateOutput(&device)
                .context("IDXGIOutput1::DuplicateOutput failed")?
        }
    };

    let desc = unsafe { duplication.GetDesc() };
    let width = desc.ModeDesc.Width;
    let height = desc.ModeDesc.Height;

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe {
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
    }
    let staging = staging.context("CreateTexture2D staging failed")?;

    Ok((width, height, device, context, duplication, staging))
}

pub struct CompositeCapturer {
    capturers: Vec<SingleCapturer>,
    virt_left: i32,
    virt_top: i32,
    canvas_w: u32,
    canvas_h: u32,
    scale: f32,
    canvas_bgra: Vec<u8>,
    start_time: Instant,
}

impl CompositeCapturer {
    pub fn new(targets: Vec<MonitorTarget>, agent_mode: bool) -> Result<Self> {
        anyhow::ensure!(!targets.is_empty(), "no monitors attached");
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        let mut capturers = Vec::with_capacity(targets.len());

        for t in targets {
            min_x = min_x.min(t.info.left);
            min_y = min_y.min(t.info.top);
            max_x = max_x.max(t.info.left + t.info.width as i32);
            max_y = max_y.max(t.info.top + t.info.height as i32);
            capturers.push(SingleCapturer::new(t, agent_mode)?);
        }

        let virt_w = (max_x - min_x).max(2) as f32;
        let virt_h = (max_y - min_y).max(2) as f32;
        let scale = (2560.0 / virt_w).min(1440.0 / virt_h).min(1.0);
        let canvas_w = (((virt_w * scale).round() as u32) & !1).max(2);
        let canvas_h = (((virt_h * scale).round() as u32) & !1).max(2);

        Ok(Self {
            capturers,
            virt_left: min_x,
            virt_top: min_y,
            canvas_w,
            canvas_h,
            scale,
            canvas_bgra: vec![0u8; canvas_w as usize * canvas_h as usize * 4],
            start_time: Instant::now(),
        })
    }

    pub fn even_dimensions(&self) -> (u32, u32) {
        (self.canvas_w, self.canvas_h)
    }

    pub fn capture_next(&mut self, timeout_ms: u32, force_frame: bool) -> Result<CaptureOutcome> {
        let mut any_updated = false;
        for (idx, cap) in self.capturers.iter_mut().enumerate() {
            let wait = if idx == 0 { timeout_ms } else { 0 };
            match cap.acquire_bgra(wait) {
                Ok(AcquireState::Updated) => {
                    any_updated = true;
                }
                Ok(AcquireState::Reacquired) => {
                    if !cap.last_bgra().is_empty() {
                        any_updated = true;
                    }
                }
                Ok(AcquireState::Unchanged) => {}
                Err(e) => {
                    tracing::warn!("composite capture error on {}: {e:#}", cap.target().info.name);
                }
            }
        }

        if !any_updated && !force_frame {
            return Ok(CaptureOutcome::Unchanged);
        }

        self.canvas_bgra.fill(0);
        let cw = self.canvas_w as usize;
        let ch = self.canvas_h as usize;

        for cap in &self.capturers {
            let bgra = cap.last_bgra();
            if bgra.is_empty() {
                continue;
            }
            let (raw_w, raw_h) = cap.raw_dimensions();
            let mw = raw_w as usize;
            let mh = raw_h as usize;
            let t = cap.target();
            let dst_x0 = ((t.info.left - self.virt_left) as f32 * self.scale)
                .round()
                .max(0.0) as usize;
            let dst_y0 = ((t.info.top - self.virt_top) as f32 * self.scale)
                .round()
                .max(0.0) as usize;
            let dst_w = ((mw as f32 * self.scale).round() as usize).max(1);
            let dst_h = ((mh as f32 * self.scale).round() as usize).max(1);

            for dy in 0..dst_h {
                let cy = dst_y0 + dy;
                if cy >= ch {
                    break;
                }
                let sy = (dy * mh) / dst_h;
                let src_row = &bgra[sy * mw * 4..(sy + 1) * mw * 4];
                let dst_row = &mut self.canvas_bgra[cy * cw * 4..(cy + 1) * cw * 4];
                for dx in 0..dst_w {
                    let cx = dst_x0 + dx;
                    if cx >= cw {
                        break;
                    }
                    let sx = (dx * mw) / dst_w;
                    dst_row[cx * 4..cx * 4 + 4].copy_from_slice(&src_row[sx * 4..sx * 4 + 4]);
                }
            }
        }

        let nv12 = bgra_to_nv12(&self.canvas_bgra, self.canvas_w, self.canvas_h);
        Ok(CaptureOutcome::Frame(RawFrame {
            width: self.canvas_w,
            height: self.canvas_h,
            nv12,
            timestamp_us: self.start_time.elapsed().as_micros() as u64,
            content_changed: any_updated,
        }))
    }
}

pub enum ScreenCapturer {
    Single(SingleCapturer),
    Composite(CompositeCapturer),
}

impl ScreenCapturer {
    pub fn open(monitor_index: u32, agent_mode: bool) -> Result<Self> {
        let targets = enumerate_monitors()?;
        anyhow::ensure!(!targets.is_empty(), "未检测到任何可用的活动显示器");

        if monitor_index == u32::MAX && targets.len() > 1 {
            return Ok(Self::Composite(CompositeCapturer::new(targets, agent_mode)?));
        }
        let first = targets[0].clone();
        let target = targets
            .into_iter()
            .find(|t| t.info.index == monitor_index)
            .unwrap_or(first);
        Ok(Self::Single(SingleCapturer::new(target, agent_mode)?))
    }

    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            ScreenCapturer::Single(s) => s.even_dimensions(),
            ScreenCapturer::Composite(c) => c.even_dimensions(),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match self {
            ScreenCapturer::Single(SingleCapturer::Dxgi(_)) => "DXGI",
            ScreenCapturer::Single(SingleCapturer::Gdi(_)) => "GDI",
            ScreenCapturer::Composite(c) => {
                let has_dxgi = c.capturers.iter().any(|x| !x.is_gdi());
                let has_gdi = c.capturers.iter().any(|x| x.is_gdi());
                match (has_dxgi, has_gdi) {
                    (true, true) => "DXGI+GDI(全景)",
                    (false, true) => "GDI(全景)",
                    _ => "DXGI(全景)",
                }
            }
        }
    }

    pub fn is_gdi_fallback(&self) -> bool {
        match self {
            ScreenCapturer::Single(s) => s.is_gdi(),
            ScreenCapturer::Composite(c) => c.capturers.iter().any(|x| x.is_gdi()),
        }
    }

    pub fn capture_next(&mut self, timeout_ms: u32, force_frame: bool) -> Result<CaptureOutcome> {
        match self {
            ScreenCapturer::Single(s) => s.capture_next(timeout_ms, force_frame),
            ScreenCapturer::Composite(c) => c.capture_next(timeout_ms, force_frame),
        }
    }
}
