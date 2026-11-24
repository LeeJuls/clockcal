//! UI Automation scan of the taskbar(s) for the clock button (ported from clockcal/poc).
//! Must run on a thread that called CoInitializeEx(COINIT_MULTITHREADED).
//! Matching is by ClassName / AutomationId only; the localized Name is never used.

use windows::core::{w, PCWSTR, VARIANT};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
    TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_ClassNamePropertyId,
    UIA_ControlTypePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW};

pub const CLOCK_CLASS: &str = "SystemTray.OmniButton";
/// Newer Win11 builds (seen on 26300) name the clock/date button this way; the bell stays `OmniButtonRight`.
pub const CLOCK_CLASS_LEFT: &str = "SystemTray.OmniButtonLeft";

pub fn is_clock_class(class: &str) -> bool {
    class == CLOCK_CLASS || class == CLOCK_CLASS_LEFT
}
pub const CLOCK_AID: &str = "SystemTrayIcon";

#[derive(Clone, Debug)]
pub struct Btn {
    pub automation_id: String,
    pub class_name: String,
    pub rect: RECT,
    pub offscreen: bool,
    pub monitor: String,
    /// Non-empty, on a monitor, fully inside it, not off-screen.
    pub valid: bool,
}

#[derive(Clone, Debug)]
pub struct TaskbarScan {
    pub buttons: Vec<Btn>,
    /// (index into buttons, rule that matched)
    pub clock: Option<(usize, &'static str)>,
    /// UIA element of the matched clock button (for cheap re-probing).
    pub clock_elem: Option<IUIAutomationElement>,
    pub error: Option<String>,
}

pub struct Scanner {
    uia: IUIAutomation,
    cond: IUIAutomationCondition,
    /// ControlType == Button (diagnostic tray dump only).
    button_cond: IUIAutomationCondition,
}

fn bstr(r: windows::core::Result<windows::core::BSTR>) -> String {
    r.map(|b| b.to_string()).unwrap_or_default()
}

fn monitor_of(r: &RECT) -> Option<(String, RECT)> {
    unsafe {
        let hm = MonitorFromRect(r, MONITOR_DEFAULTTONULL);
        if hm.is_invalid() {
            return None;
        }
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(hm, &mut mi as *mut _ as *mut MONITORINFO).as_bool() {
            return None;
        }
        let end = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
        Some((String::from_utf16_lossy(&mi.szDevice[..end]), mi.monitorInfo.rcMonitor))
    }
}

fn inside(a: &RECT, outer: &RECT) -> bool {
    a.left >= outer.left && a.top >= outer.top && a.right <= outer.right && a.bottom <= outer.bottom
}

/// Shell_TrayWnd first, then every Shell_SecondaryTrayWnd.
pub fn taskbars() -> Vec<(&'static str, HWND)> {
    let mut v = Vec::new();
    unsafe {
        if let Ok(h) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) {
            if !h.is_invalid() {
                v.push(("Shell_TrayWnd", h));
            }
        }
        let mut after = HWND::default();
        while let Ok(h) =
            FindWindowExW(HWND::default(), after, w!("Shell_SecondaryTrayWnd"), PCWSTR::null())
        {
            if h.is_invalid() {
                break;
            }
            v.push(("Shell_SecondaryTrayWnd", h));
            after = h;
        }
    }
    v
}

impl Scanner {
    pub fn new() -> windows::core::Result<Self> {
        unsafe {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let c1 =
                uia.CreatePropertyCondition(UIA_ClassNamePropertyId, &VARIANT::from(CLOCK_CLASS))?;
            let c2 = uia
                .CreatePropertyCondition(UIA_ClassNamePropertyId, &VARIANT::from(CLOCK_CLASS_LEFT))?;
            let cond = uia.CreateOrCondition(&c1, &c2)?;
            let button_cond = uia.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_ButtonControlTypeId.0),
            )?;
            Ok(Self { uia, cond, button_cond })
        }
    }

    fn btn_of(e: &IUIAutomationElement) -> Btn {
        let rect = unsafe { e.CurrentBoundingRectangle() }.unwrap_or_default();
        Self::btn_with_rect(e, rect)
    }

    fn btn_with_rect(e: &IUIAutomationElement, rect: RECT) -> Btn {
        unsafe {
            let offscreen = e.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(false);
            let (monitor, monitor_rect) =
                monitor_of(&rect).unwrap_or_else(|| ("<none>".into(), RECT::default()));
            let nonempty = rect.right > rect.left && rect.bottom > rect.top;
            let valid =
                nonempty && monitor != "<none>" && inside(&rect, &monitor_rect) && !offscreen;
            Btn {
                automation_id: bstr(e.CurrentAutomationId()),
                class_name: bstr(e.CurrentClassName()),
                rect,
                offscreen,
                monitor,
                valid,
            }
        }
    }

    /// Scan a single taskbar window.
    pub fn scan_one(&self, hwnd: HWND) -> TaskbarScan {
        let mut out =
            TaskbarScan { buttons: Vec::new(), clock: None, clock_elem: None, error: None };
        let mut elems = Vec::new();
        let res: windows::core::Result<()> = (|| unsafe {
            let root = self.uia.ElementFromHandle(hwnd)?;
            let arr = root.FindAll(TreeScope_Descendants, &self.cond)?;
            for i in 0..arr.Length()? {
                let e = arr.GetElement(i)?;
                let b = Self::btn_of(&e);
                // The condition already filters by class; belt and braces.
                if is_clock_class(&b.class_name) {
                    out.buttons.push(b);
                    elems.push(e);
                }
            }
            Ok(())
        })();
        if let Err(e) = res {
            out.error = Some(format!("{e}"));
        }
        out.clock = pick_clock(&out.buttons);
        out.clock_elem = out.clock.and_then(|(i, _)| elems.get(i).cloned());
        out
    }

    /// Cheap re-read of a previously matched clock element (one cross-process
    /// property read + class/aid check). `Err` = element gone / not the clock any
    /// more -> caller must do a full `scan_one`.
    pub fn probe(&self, e: &IUIAutomationElement) -> Result<Btn, String> {
        let rect = unsafe { e.CurrentBoundingRectangle() }.map_err(|e| format!("{e}"))?;
        let class = bstr(unsafe { e.CurrentClassName() });
        if !is_clock_class(&class) {
            return Err(format!("class changed to '{class}'"));
        }
        let b = Self::btn_with_rect(e, rect);
        if b.automation_id != CLOCK_AID {
            return Err(format!("automation id changed to '{}'", b.automation_id));
        }
        Ok(b)
    }

    /// Diagnostic only (slow): every Button whose ClassName starts with
    /// `SystemTray.` on this taskbar, sorted left to right.
    pub fn dump_tray(&self, hwnd: HWND) -> Result<Vec<Btn>, String> {
        let mut v = Vec::new();
        let res: windows::core::Result<()> = (|| unsafe {
            let root = self.uia.ElementFromHandle(hwnd)?;
            let arr = root.FindAll(TreeScope_Descendants, &self.button_cond)?;
            for i in 0..arr.Length()? {
                let b = Self::btn_of(&arr.GetElement(i)?);
                if b.class_name.starts_with("SystemTray.") {
                    v.push(b);
                }
            }
            Ok(())
        })();
        res.map_err(|e| format!("{e}"))?;
        v.sort_by_key(|b| (b.rect.top, b.rect.left));
        Ok(v)
    }
}

/// Rule 1: ClassName == SystemTray.OmniButton AND AutomationId == SystemTrayIcon
///         (right-most if several).
/// Rule 2 (fallback): right-most SystemTray.OmniButton.
pub fn pick_clock(btns: &[Btn]) -> Option<(usize, &'static str)> {
    let rightmost = |pred: &dyn Fn(&Btn) -> bool| {
        btns.iter()
            .enumerate()
            .filter(|(_, b)| pred(b))
            .max_by_key(|(_, b)| b.rect.right)
            .map(|(i, _)| i)
    };
    if let Some(i) = rightmost(&|b| is_clock_class(&b.class_name) && b.automation_id == CLOCK_AID) {
        return Some((i, "rule1:class+automationid"));
    }
    rightmost(&|b| is_clock_class(&b.class_name)).map(|i| (i, "rule2:fallback-rightmost-omnibutton"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(aid: &str, right: i32) -> Btn {
        Btn {
            automation_id: aid.into(),
            class_name: CLOCK_CLASS.into(),
            rect: RECT { left: right - 50, top: 0, right, bottom: 40 },
            offscreen: false,
            monitor: "M".into(),
            valid: true,
        }
    }

    #[test]
    fn rule1_prefers_automation_id_over_rightmost() {
        let v = [b("SystemTrayIcon", 100), b("Other", 300)];
        assert_eq!(pick_clock(&v), Some((0, "rule1:class+automationid")));
    }

    #[test]
    fn clock_class_accepts_left_variant_only_for_clock() {
        assert!(is_clock_class("SystemTray.OmniButton"));
        assert!(is_clock_class("SystemTray.OmniButtonLeft"));
        assert!(!is_clock_class("SystemTray.OmniButtonRight"));
        assert!(!is_clock_class("SystemTray.OmniButtonCenter"));
        let mut v = b("SystemTrayIcon", 100);
        v.class_name = CLOCK_CLASS_LEFT.into();
        assert_eq!(pick_clock(&[v]), Some((0, "rule1:class+automationid")));
    }

    #[test]
    fn rule2_fallback_rightmost() {
        let v = [b("A", 100), b("B", 300), b("C", 200)];
        assert_eq!(pick_clock(&v), Some((1, "rule2:fallback-rightmost-omnibutton")));
        assert_eq!(pick_clock(&[]), None);
    }
}
