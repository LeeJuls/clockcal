//! Win32 monitor queries (physical px; process is Per-Monitor-V2 aware).

use crate::popup_logic::Rect;
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows_sys::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetDpiForMonitor, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};

#[derive(Debug, Clone, Copy)]
pub struct MonitorInfo {
    pub handle: isize,
    pub monitor: Rect,
    pub work: Rect,
    pub dpi: u32,
    pub primary: bool,
}

impl MonitorInfo {
    pub fn scale(&self) -> f64 {
        self.dpi as f64 / 96.0
    }
}

fn to_rect(r: &RECT) -> Rect {
    Rect::from_ltrb(r.left, r.top, r.right, r.bottom)
}

/// Best effort; tao also does this. Must run before any window is created.
pub fn set_per_monitor_v2() -> bool {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0 }
}

/// 2 = DPI_AWARENESS_PER_MONITOR_AWARE (V1 or V2 both report 2).
pub fn current_awareness() -> i32 {
    unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) }
}

fn query(h: HMONITOR) -> Option<MonitorInfo> {
    if h.is_null() {
        return None;
    }
    unsafe {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(h, &mut mi) == 0 {
            return None;
        }
        let (mut dx, mut dy) = (96u32, 96u32);
        if GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) != 0 {
            dx = 96;
        }
        Some(MonitorInfo {
            handle: h as isize,
            monitor: to_rect(&mi.rcMonitor),
            work: to_rect(&mi.rcWork),
            dpi: dx,
            primary: mi.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
        })
    }
}

/// Monitor containing (nearest to) the given physical point.
pub fn monitor_from_point(x: i32, y: i32) -> Option<MonitorInfo> {
    unsafe { query(MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST)) }
}

pub fn primary_monitor() -> Option<MonitorInfo> {
    unsafe { query(MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)) }
}
