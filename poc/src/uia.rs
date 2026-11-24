//! UI Automation scan of the taskbar(s) for the clock button.
//! Must run on a thread that called CoInitializeEx(COINIT_MULTITHREADED).
//! Matching is by ClassName / AutomationId only; Name is printed but never used.

use windows::core::{w, BSTR, PCWSTR, VARIANT};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement, TreeScope_Descendants,
    UIA_ClassNamePropertyId,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW};

pub const CLOCK_CLASS: &str = "SystemTray.OmniButton";
pub const CLOCK_AID: &str = "SystemTrayIcon";

#[derive(Clone, Debug)]
pub struct Btn {
    pub automation_id: String,
    pub class_name: String,
    pub name: String,
    pub rect: RECT,
    pub offscreen: bool,
    pub monitor: String,
    pub monitor_rect: RECT,
    /// Non-empty, on a monitor, fully inside it, not off-screen.
    pub valid: bool,
}

#[derive(Clone, Debug)]
pub struct TaskbarScan {
    pub class: &'static str,
    pub hwnd: HWND,
    pub buttons: Vec<Btn>,
    /// (index into buttons, rule that matched)
    pub clock: Option<(usize, &'static str)>,
    pub error: Option<String>,
}

pub struct Scanner {
    uia: IUIAutomation,
    cond: IUIAutomationCondition,
}

fn bstr(r: windows::core::Result<BSTR>) -> String {
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

pub fn fmt_rect(r: &RECT) -> String {
    format!(
        "left={} top={} w={} h={} (r={} b={})",
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
        r.right,
        r.bottom
    )
}

/// Shell_TrayWnd first, then every Shell_SecondaryTrayWnd.
pub fn taskbars() -> Vec<(&'static str, HWND)> {
    let mut v = Vec::new();
    unsafe {
        if let Ok(h) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null())
            && !h.is_invalid()
        {
            v.push(("Shell_TrayWnd", h));
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
            let cond = uia.CreatePropertyCondition(UIA_ClassNamePropertyId, &VARIANT::from(CLOCK_CLASS))?;
            Ok(Self { uia, cond })
        }
    }

    pub fn btn_of(e: &IUIAutomationElement) -> Btn {
        unsafe {
            let rect = e.CurrentBoundingRectangle().unwrap_or_default();
            let offscreen = e.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(false);
            let (monitor, monitor_rect) = monitor_of(&rect).unwrap_or_else(|| ("<none>".into(), RECT::default()));
            let nonempty = rect.right > rect.left && rect.bottom > rect.top;
            let valid = nonempty && monitor != "<none>" && inside(&rect, &monitor_rect) && !offscreen;
            Btn {
                automation_id: bstr(e.CurrentAutomationId()),
                class_name: bstr(e.CurrentClassName()),
                name: bstr(e.CurrentName()),
                rect,
                offscreen,
                monitor,
                monitor_rect,
                valid,
            }
        }
    }

    /// Scan a single taskbar window (used for quick retries after a transient miss).
    pub fn scan_one(&self, class: &'static str, hwnd: HWND) -> TaskbarScan {
        let mut out = TaskbarScan { class, hwnd, buttons: Vec::new(), clock: None, error: None };
        let res: windows::core::Result<()> = (|| unsafe {
            let root = self.uia.ElementFromHandle(hwnd)?;
            let arr = root.FindAll(TreeScope_Descendants, &self.cond)?;
            for i in 0..arr.Length()? {
                let e = arr.GetElement(i)?;
                let b = Self::btn_of(&e);
                // Re-check the class (the condition already filters; belt and braces).
                if b.class_name == CLOCK_CLASS {
                    out.buttons.push(b);
                }
            }
            Ok(())
        })();
        if let Err(e) = res {
            out.error = Some(format!("{e}"));
        }
        out.clock = pick_clock(&out.buttons);
        out
    }

    pub fn scan(&self) -> Vec<TaskbarScan> {
        taskbars().into_iter().map(|(c, h)| self.scan_one(c, h)).collect()
    }

    /// Every descendant whose class contains "OmniButton" (context for `list`).
    pub fn related(&self, hwnd: HWND) -> windows::core::Result<Vec<Btn>> {
        unsafe {
            let root = self.uia.ElementFromHandle(hwnd)?;
            let t = self.uia.CreateTrueCondition()?;
            let arr = root.FindAll(TreeScope_Descendants, &t)?;
            let mut v = Vec::new();
            for i in 0..arr.Length()? {
                let e = arr.GetElement(i)?;
                let cls = bstr(e.CurrentClassName());
                if cls.contains("OmniButton") && cls != CLOCK_CLASS {
                    v.push(Self::btn_of(&e));
                }
            }
            Ok(v)
        }
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
    if let Some(i) = rightmost(&|b| b.class_name == CLOCK_CLASS && b.automation_id == CLOCK_AID) {
        return Some((i, "rule1:class+automationid"));
    }
    rightmost(&|b| b.class_name == CLOCK_CLASS).map(|i| (i, "rule2:fallback-rightmost-omnibutton"))
}
