//! Pure popup logic (no Win32 / Tauri): toggle de-bounce and placement math.
//! All coordinates are physical pixels in virtual-screen space.

/// Rectangle in physical px. `x,y` = top-left; `w,h` = size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    pub const fn from_ltrb(l: i32, t: i32, r: i32, b: i32) -> Self {
        Self { x: l, y: t, w: r - l, h: b - t }
    }
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub const fn is_valid(&self) -> bool {
        self.w > 0 && self.h > 0
    }
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
    pub fn contains_rect(&self, o: &Rect) -> bool {
        o.x >= self.x && o.y >= self.y && o.right() <= self.right() && o.bottom() <= self.bottom()
    }
    /// Parse "x,y,w,h".
    pub fn parse(s: &str) -> Option<Rect> {
        let v: Vec<i32> = s.split(',').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
        match v.as_slice() {
            [x, y, w, h] if *w > 0 && *h > 0 => Some(Rect::new(*x, *y, *w, *h)),
            _ => None,
        }
    }
}

/// Default de-bounce window after a focus-loss hide.
pub const BLUR_DEBOUNCE_MS: u64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleAction {
    Show,
    Hide,
    /// Popup was just hidden by the same click that blurred it; do nothing.
    Ignore,
}

/// Decide what a toggle request does.
/// `last_blur_hide_ms` / `now_ms` are monotonic milliseconds.
pub fn decide_toggle(
    visible: bool,
    last_blur_hide_ms: Option<u64>,
    now_ms: u64,
    debounce_ms: u64,
) -> ToggleAction {
    if visible {
        return ToggleAction::Hide;
    }
    match last_blur_hide_ms {
        Some(t) if now_ms.saturating_sub(t) < debounce_ms => ToggleAction::Ignore,
        _ => ToggleAction::Show,
    }
}

/// Which monitor edge the taskbar occupies, inferred from monitor vs work area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarEdge {
    Bottom,
    Top,
    Left,
    Right,
    /// Work area == monitor (auto-hide or no taskbar on this monitor).
    None,
}

pub fn taskbar_edge(monitor: &Rect, work: &Rect) -> TaskbarEdge {
    if work.bottom() < monitor.bottom() {
        TaskbarEdge::Bottom
    } else if work.y > monitor.y {
        TaskbarEdge::Top
    } else if work.x > monitor.x {
        TaskbarEdge::Left
    } else if work.right() < monitor.right() {
        TaskbarEdge::Right
    } else {
        TaskbarEdge::None
    }
}

/// Gap between popup and taskbar/clock, physical px (caller scales by DPI).
fn clamp_axis(v: i32, size: i32, lo: i32, hi: i32) -> i32 {
    // If the popup is larger than the range, pin to `lo`.
    if size >= hi - lo {
        lo
    } else {
        v.clamp(lo, hi - size)
    }
}

/// Compute popup top-left in physical px.
/// - `anchor` = clock rect (physical px) or None for tray/hotkey.
/// - `monitor`/`work` = rcMonitor/rcWork of the monitor that holds the anchor
///   (or the primary monitor when `anchor` is None).
/// - `size` = popup physical size, `margin` = gap in physical px.
///
/// Result is always clamped inside `work`.
pub fn place_popup(
    anchor: Option<Rect>,
    monitor: &Rect,
    work: &Rect,
    size: (i32, i32),
    margin: i32,
) -> (i32, i32) {
    let (w, h) = size;
    let edge = taskbar_edge(monitor, work);
    // Default: bottom-right corner of the work area.
    let mut x = work.right() - w - margin;
    let mut y = work.bottom() - h - margin;
    if let Some(a) = anchor {
        match edge {
            TaskbarEdge::Bottom | TaskbarEdge::None => {
                // Above the clock, right-aligned to the work area.
                y = a.y - h - margin;
            }
            TaskbarEdge::Top => {
                y = a.bottom() + margin;
            }
            TaskbarEdge::Left => {
                x = a.right() + margin;
                y = a.bottom() - h;
            }
            TaskbarEdge::Right => {
                x = a.x - w - margin;
                y = a.bottom() - h;
            }
        }
    } else if edge == TaskbarEdge::Top {
        y = work.y + margin;
    } else if edge == TaskbarEdge::Left {
        x = work.x + margin;
    }
    (
        clamp_axis(x, w, work.x, work.right()),
        clamp_axis(y, h, work.y, work.bottom()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SZ: (i32, i32) = (540, 960); // 360x640 @150%
    const M: i32 = 18;

    fn popup(p: (i32, i32)) -> Rect {
        Rect::new(p.0, p.1, SZ.0, SZ.1)
    }

    // ---- toggle ----
    #[test]
    fn toggle_shows_when_hidden() {
        assert_eq!(decide_toggle(false, None, 10_000, 300), ToggleAction::Show);
    }

    #[test]
    fn toggle_hides_when_shown() {
        assert_eq!(decide_toggle(true, None, 10_000, 300), ToggleAction::Hide);
        assert_eq!(decide_toggle(true, Some(9_990), 10_000, 300), ToggleAction::Hide);
    }

    #[test]
    fn toggle_debounced_after_blur_hide() {
        assert_eq!(decide_toggle(false, Some(10_000), 10_050, 300), ToggleAction::Ignore);
        assert_eq!(decide_toggle(false, Some(10_000), 10_299, 300), ToggleAction::Ignore);
        assert_eq!(decide_toggle(false, Some(10_000), 10_300, 300), ToggleAction::Show);
        assert_eq!(decide_toggle(false, Some(10_000), 15_000, 300), ToggleAction::Show);
    }

    #[test]
    fn toggle_clock_going_backwards_is_ignore_not_panic() {
        // saturating_sub: now < last -> 0 -> within window
        assert_eq!(decide_toggle(false, Some(10_000), 9_000, 300), ToggleAction::Ignore);
    }

    // ---- parse ----
    #[test]
    fn parse_rect() {
        assert_eq!(Rect::parse("7562,2088,106,72"), Some(Rect::new(7562, 2088, 106, 72)));
        assert_eq!(Rect::parse("-1920, 0, 10, 10"), Some(Rect::new(-1920, 0, 10, 10)));
        assert_eq!(Rect::parse("1,2,3"), None);
        assert_eq!(Rect::parse("1,2,0,4"), None);
        assert_eq!(Rect::parse("a,b,c,d"), None);
    }

    // ---- placement ----
    #[test]
    fn bottom_taskbar_above_clock_right_aligned() {
        let mon = Rect::new(0, 0, 3840, 2160);
        let work = Rect::from_ltrb(0, 0, 3840, 2088);
        let clock = Rect::new(3710, 2088, 106, 72);
        let p = popup(place_popup(Some(clock), &mon, &work, SZ, M));
        assert!(work.contains_rect(&p), "{p:?}");
        assert!(p.bottom() <= clock.y, "above clock");
        assert_eq!(p.right(), work.right() - M);
        assert_eq!(p.bottom(), clock.y - M);
    }

    #[test]
    fn top_taskbar_below_clock() {
        let mon = Rect::new(0, 0, 3840, 2160);
        let work = Rect::from_ltrb(0, 72, 3840, 2160);
        let clock = Rect::new(3710, 0, 106, 72);
        let p = popup(place_popup(Some(clock), &mon, &work, SZ, M));
        assert!(work.contains_rect(&p), "{p:?}");
        assert!(p.y >= clock.bottom());
    }

    #[test]
    fn left_taskbar_right_of_clock() {
        let mon = Rect::new(0, 0, 3840, 2160);
        let work = Rect::from_ltrb(100, 0, 3840, 2160);
        let clock = Rect::new(0, 2080, 100, 80);
        let p = popup(place_popup(Some(clock), &mon, &work, SZ, M));
        assert!(work.contains_rect(&p), "{p:?}");
        assert!(p.x >= clock.right());
    }

    #[test]
    fn right_taskbar_left_of_clock() {
        let mon = Rect::new(0, 0, 3840, 2160);
        let work = Rect::from_ltrb(0, 0, 3740, 2160);
        let clock = Rect::new(3740, 2080, 100, 80);
        let p = popup(place_popup(Some(clock), &mon, &work, SZ, M));
        assert!(work.contains_rect(&p), "{p:?}");
        assert!(p.right() <= clock.x);
    }

    #[test]
    fn secondary_monitor_large_coords() {
        // Secondary to the right of a 3840 primary.
        let mon = Rect::new(3840, 0, 3840, 2160);
        let work = Rect::from_ltrb(3840, 0, 7680, 2088);
        let clock = Rect::new(7562, 2088, 106, 72);
        let p = popup(place_popup(Some(clock), &mon, &work, SZ, M));
        assert!(work.contains_rect(&p), "{p:?}");
        assert!(p.bottom() <= clock.y);
        assert!(p.x >= 3840);
    }

    #[test]
    fn secondary_monitor_negative_coords() {
        // Secondary to the left/top of primary (negative origin), 100% scale.
        let mon = Rect::new(-1920, -300, 1920, 1080);
        let work = Rect::from_ltrb(-1920, -300, 0, 732);
        let clock = Rect::new(-130, 732, 100, 48);
        let sz = (360, 640);
        let p = Rect::new(0, 0, sz.0, sz.1);
        let (x, y) = place_popup(Some(clock), &mon, &work, sz, 12);
        let p = Rect { x, y, ..p };
        assert!(work.contains_rect(&p), "{p:?}");
        assert!(p.bottom() <= clock.y);
        assert!(p.right() <= 0);
    }

    #[test]
    fn anchor_far_left_is_clamped_into_work_area() {
        // Clock-ish anchor near the left edge: x comes from work.right anyway,
        // but an anchor near the top must clamp y to work.y.
        let mon = Rect::new(0, 0, 1920, 1080);
        let work = Rect::from_ltrb(0, 0, 1920, 1032);
        let weird = Rect::new(10, 100, 50, 40); // popup would go above y=0
        let sz = (360, 640);
        let (x, y) = place_popup(Some(weird), &mon, &work, sz, 12);
        let p = Rect::new(x, y, sz.0, sz.1);
        assert!(work.contains_rect(&p), "{p:?}");
        assert_eq!(p.y, 0);
    }

    #[test]
    fn tall_popup_in_small_work_area_clamps_to_top() {
        // 1366x768 @100%: work area 728 high; a 640 popup fits but the clamp must hold
        // when the anchor would push it above the top (e.g. 125% -> 800 high > 728).
        let mon = Rect::new(0, 0, 1366, 768);
        let work = Rect::from_ltrb(0, 0, 1366, 728);
        let clock = Rect::new(1260, 728, 106, 40);
        let (x, y) = place_popup(Some(clock), &mon, &work, (360, 640), 12);
        let p = Rect::new(x, y, 360, 640);
        assert!(work.contains_rect(&p), "{p:?}");
        assert_eq!(p.bottom(), clock.y - 12);
        let (x, y) = place_popup(Some(clock), &mon, &work, (450, 800), 15);
        assert_eq!((x, y), (1366 - 450 - 15, 0), "taller than work area pins to top");
    }

    #[test]
    fn popup_bigger_than_work_area_pins_to_origin() {
        let mon = Rect::new(0, 0, 800, 600);
        let work = Rect::from_ltrb(0, 0, 800, 560);
        let (x, y) = place_popup(None, &mon, &work, (900, 700), 10);
        assert_eq!((x, y), (0, 0));
    }

    #[test]
    fn no_anchor_bottom_right_of_primary() {
        let mon = Rect::new(0, 0, 3840, 2160);
        let work = Rect::from_ltrb(0, 0, 3840, 2088);
        let p = popup(place_popup(None, &mon, &work, SZ, M));
        assert!(work.contains_rect(&p));
        assert_eq!(p.right(), work.right() - M);
        assert_eq!(p.bottom(), work.bottom() - M);
    }

    #[test]
    fn edge_detection() {
        let mon = Rect::new(0, 0, 100, 100);
        assert_eq!(taskbar_edge(&mon, &Rect::from_ltrb(0, 0, 100, 90)), TaskbarEdge::Bottom);
        assert_eq!(taskbar_edge(&mon, &Rect::from_ltrb(0, 10, 100, 100)), TaskbarEdge::Top);
        assert_eq!(taskbar_edge(&mon, &Rect::from_ltrb(10, 0, 100, 100)), TaskbarEdge::Left);
        assert_eq!(taskbar_edge(&mon, &Rect::from_ltrb(0, 0, 90, 100)), TaskbarEdge::Right);
        assert_eq!(taskbar_edge(&mon, &mon), TaskbarEdge::None);
    }
}
