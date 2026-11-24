//! Fitting the main window onto small screens.
//!
//! eframe only clamps the initial window to the *whole* monitor and centres it on the whole
//! monitor, ignoring the title bar and the taskbar. On 1366×768, or a 1080p laptop at 125–150 %
//! scaling, a 1060×740 window therefore hangs off the bottom of the screen: that part can never be
//! scrolled into view, and the fixed minimum size could even be larger than the screen. Once the
//! window exists we read the real work area of its monitor and shrink / move the window to fit.

use eframe::egui::{pos2, Pos2, Rect, Vec2};

/// Initial inner window size (points) on screens that have room for it.
pub const DEFAULT_INNER: Vec2 = Vec2::new(1060.0, 740.0);
/// Smallest inner size the layouts are designed for (reduced further on tinier screens).
pub const MIN_INNER: Vec2 = Vec2::new(640.0, 440.0);

/// Gap kept between the window and the edges of the work area.
const MARGIN: f32 = 8.0;
/// Never shrink below this, however small the screen claims to be.
const FLOOR: Vec2 = Vec2::new(360.0, 260.0);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowFit {
    /// Smaller inner size, when the window does not fit the work area.
    pub inner_size: Option<Vec2>,
    /// Minimum inner size, lowered when the screen cannot even hold the default minimum.
    pub min_inner_size: Vec2,
    /// New outer position, when part of the window lies outside the work area.
    pub outer_pos: Option<Pos2>,
}

/// Plans how to make a window with the given `outer` / `inner` rects fit into `work`
/// (all in points).
pub fn fit_window(outer: Rect, inner: Rect, work: Rect, min_inner: Vec2) -> WindowFit {
    // Title bar + borders (on Windows the outer rect also includes the invisible resize borders).
    let chrome = (outer.size() - inner.size()).max(Vec2::ZERO);
    let max_inner = (work.size() - chrome - Vec2::splat(2.0 * MARGIN)).max(FLOOR);
    let size = inner.size().min(max_inner);
    let outer_size = size + chrome;
    let inner_size = ((size - inner.size()).length() > 0.5).then_some(size);
    // Re-centre after shrinking (eframe centred the old size), or when part of the window is
    // outside the work area; never push the title bar above / left of it.
    let off_screen = !work.contains_rect(Rect::from_min_size(outer.min, outer_size));
    let outer_pos = (inner_size.is_some() || off_screen).then(|| {
        let p = work.center() - outer_size / 2.0;
        pos2(p.x.max(work.left()), p.y.max(work.top()))
    });
    WindowFit {
        inner_size,
        min_inner_size: min_inner.min(size),
        outer_pos,
    }
}

/// Work area (monitor minus taskbar) of the monitor containing `at`, in points.
pub fn work_area_points(at: Pos2, pixels_per_point: f32) -> Option<Rect> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let ppp = pixels_per_point.max(0.25);
    // winit has made the process per-monitor DPI aware, so these are physical pixels.
    let pt = POINT {
        x: (at.x * ppp).round() as i32,
        y: (at.y * ppp).round() as i32,
    };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: plain Win32 queries; `info` is a properly sized, initialised MONITORINFO.
    unsafe {
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() || !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
    }
    let r = info.rcWork;
    let work = Rect::from_min_max(
        pos2(r.left as f32 / ppp, r.top as f32 / ppp),
        pos2(r.right as f32 / ppp, r.bottom as f32 / ppp),
    );
    (work.width() >= 1.0 && work.height() >= 1.0).then_some(work)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::vec2;

    /// Windows 10/11 chrome at 100 %: 31 pt title bar, 7 pt invisible borders left/right/bottom.
    const CHROME: Vec2 = Vec2::new(14.0, 38.0);

    fn window(outer_pos: Pos2, inner: Vec2) -> (Rect, Rect) {
        let outer = Rect::from_min_size(outer_pos, inner + CHROME);
        let inner = Rect::from_min_size(outer_pos + vec2(7.0, 31.0), inner);
        (outer, inner)
    }

    fn assert_fits(fit: &WindowFit, outer: Rect, inner: Rect, work: Rect) {
        let size = fit.inner_size.unwrap_or(inner.size());
        let pos = fit.outer_pos.unwrap_or(outer.min);
        let placed = Rect::from_min_size(pos, size + CHROME);
        assert!(work.contains_rect(placed), "{placed:?} not inside {work:?}");
        assert!(fit.min_inner_size.x <= size.x && fit.min_inner_size.y <= size.y);
    }

    #[test]
    fn roomy_screen_is_left_alone() {
        let (outer, inner) = window(pos2(743.0, 311.0), DEFAULT_INNER);
        let work = Rect::from_min_size(Pos2::ZERO, vec2(2560.0, 1392.0));
        let fit = fit_window(outer, inner, work, MIN_INNER);
        assert_eq!(
            fit,
            WindowFit {
                inner_size: None,
                min_inner_size: MIN_INNER,
                outer_pos: None
            }
        );
    }

    #[test]
    fn laptop_1366x768_keeps_width_and_shrinks_height() {
        // eframe centres 1060x740 on the whole 768 px monitor: the bottom sinks under the taskbar.
        let (outer, inner) = window(pos2(153.0, 14.0), DEFAULT_INNER);
        let work = Rect::from_min_size(Pos2::ZERO, vec2(1366.0, 728.0));
        let fit = fit_window(outer, inner, work, MIN_INNER);
        let size = fit.inner_size.expect("window must shrink");
        assert_eq!(size.x, DEFAULT_INNER.x);
        assert!((size.y - (728.0 - CHROME.y - 2.0 * MARGIN)).abs() < 0.01);
        assert!(fit.outer_pos.is_some());
        assert_eq!(fit.min_inner_size, MIN_INNER);
        assert_fits(&fit, outer, inner, work);
    }

    #[test]
    fn full_hd_at_150_percent() {
        // 1920x1080 at 150 % = 1280x720 pt, 48 px taskbar = 32 pt. eframe already clamped the
        // inner height to the 720 pt monitor and put the window at the very top.
        let (outer, inner) = window(pos2(110.0, -19.0), vec2(1060.0, 720.0));
        let work = Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 688.0));
        let fit = fit_window(outer, inner, work, MIN_INNER);
        assert!(fit.inner_size.is_some());
        assert_fits(&fit, outer, inner, work);
    }

    #[test]
    fn tiny_screen_lowers_the_minimum_size() {
        // 1366x768 at 150 %: about 911x480 pt of work area, less than the default minimum height.
        let (outer, inner) = window(pos2(0.0, 0.0), vec2(911.0, 512.0));
        let work = Rect::from_min_size(Pos2::ZERO, vec2(911.0, 480.0));
        let fit = fit_window(outer, inner, work, MIN_INNER);
        assert!(fit.min_inner_size.y < MIN_INNER.y);
        assert_eq!(fit.min_inner_size.x, MIN_INNER.x);
        assert_fits(&fit, outer, inner, work);
    }

    #[test]
    fn taskbar_at_the_top_is_respected() {
        let (outer, inner) = window(pos2(153.0, 14.0), DEFAULT_INNER);
        let work = Rect::from_min_max(pos2(0.0, 48.0), pos2(1366.0, 768.0));
        let fit = fit_window(outer, inner, work, MIN_INNER);
        let pos = fit.outer_pos.expect("window must move below the taskbar");
        assert!(pos.y >= 48.0);
        assert_fits(&fit, outer, inner, work);
    }

    #[test]
    fn window_partly_off_a_big_screen_is_only_moved() {
        let (outer, inner) = window(pos2(1800.0, 900.0), DEFAULT_INNER);
        let work = Rect::from_min_size(Pos2::ZERO, vec2(2560.0, 1392.0));
        let fit = fit_window(outer, inner, work, MIN_INNER);
        assert_eq!(fit.inner_size, None);
        assert!(fit.outer_pos.is_some());
        assert_fits(&fit, outer, inner, work);
    }
}
