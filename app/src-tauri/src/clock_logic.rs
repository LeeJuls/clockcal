//! Pure logic for the taskbar-clock interception (no Win32 / Tauri), unit-tested.
//! - `RectSet`: fixed-size, allocation-free rect set the hook callback hit-tests.
//! - `pair_decide`: LBUTTONDOWN/UP swallow pairing state machine.
//! - `RectCache`: per-taskbar last-known-good clock rect with miss/expiry accounting.
//! - `HookRects` / `Zone`: clock rects + "near the clock" bands, one O(n<=8) classify.
//! - `entering_band`, `RateLimit`: near-band probe trigger and log rate limiting.
//!
//! All coordinates are physical pixels in virtual-screen space.

use crate::popup_logic::Rect;

pub const MAX_RECTS: usize = 8;

/// `(left, top, right, bottom)` in physical px.
pub type Ltrb = (i32, i32, i32, i32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RectSet {
    pub n: usize,
    pub r: [Ltrb; MAX_RECTS],
}

impl RectSet {
    pub const EMPTY: RectSet = RectSet { n: 0, r: [(0, 0, 0, 0); MAX_RECTS] };

    /// Valid (non-empty) rects only, at most `MAX_RECTS`.
    pub fn from_rects<I: IntoIterator<Item = Rect>>(it: I) -> Self {
        let mut s = Self::EMPTY;
        for r in it.into_iter().filter(|r| r.is_valid()) {
            if s.n == MAX_RECTS {
                break;
            }
            s.r[s.n] = (r.x, r.y, r.right(), r.bottom());
            s.n += 1;
        }
        s
    }

    /// O(n<=8), no allocation. Returns the rect that contains (x, y).
    /// Half-open: right/bottom edges are outside. Empty rects never hit.
    #[inline]
    pub fn hit(&self, x: i32, y: i32) -> Option<Ltrb> {
        let n = self.n.min(MAX_RECTS);
        self.r[..n]
            .iter()
            .copied()
            .find(|&(l, t, r, b)| r > l && b > t && x >= l && x < r && y >= t && y < b)
    }
}

/// Horizontal margin (physical px) of the band around each clock rect in which
/// mouse moves switch the UIA thread to fast probing and un-swallowed
/// LBUTTONDOWNs are logged as near-misses. Vertical extent = the clock rect's
/// (the clock spans the full taskbar height).
pub const BAND_X: i32 = 200;

/// Where a point lies relative to the cached clock rects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// Inside a clock rect (swallow).
    Clock(Ltrb),
    /// Not on a clock but within `BAND_X` of one (index into the set).
    Band(usize),
    Outside,
}

/// What the hook callback hit-tests: clock hit rects and their bands, same order.
/// `anchor[i]` is the real UIA clock rect for `clock.r[i]` (the hit rect may be
/// extended into the margin strip, see `extend_clock`); it is what a swallowed
/// click reports, so popup placement never sees the extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookRects {
    pub clock: RectSet,
    pub anchor: [Ltrb; MAX_RECTS],
    pub band: RectSet,
}

impl HookRects {
    pub const EMPTY: HookRects =
        HookRects { clock: RectSet::EMPTY, anchor: [(0, 0, 0, 0); MAX_RECTS], band: RectSet::EMPTY };

    /// Hit rect == anchor for every clock.
    pub fn new(clock: RectSet, band_x: i32) -> Self {
        let mut band = clock;
        let n = clock.n.min(MAX_RECTS);
        for i in 0..n {
            let (l, t, r, b) = clock.r[i];
            band.r[i] = (l.saturating_sub(band_x), t, r.saturating_add(band_x), b);
        }
        HookRects { clock, anchor: clock.r, band }
    }

    /// `(anchor, hit)` pairs; pairs whose anchor or hit is invalid are skipped, at
    /// most `MAX_RECTS`. Bands are built around the hit rects.
    pub fn with_hits<I: IntoIterator<Item = (Rect, Rect)>>(pairs: I, band_x: i32) -> Self {
        let mut clock = RectSet::EMPTY;
        let mut anchor = [(0, 0, 0, 0); MAX_RECTS];
        for (a, h) in pairs.into_iter().filter(|(a, h)| a.is_valid() && h.is_valid()) {
            if clock.n == MAX_RECTS {
                break;
            }
            clock.r[clock.n] = (h.x, h.y, h.right(), h.bottom());
            anchor[clock.n] = (a.x, a.y, a.right(), a.bottom());
            clock.n += 1;
        }
        HookRects { anchor, ..Self::new(clock, band_x) }
    }

    /// O(n<=8) twice, no allocation. Clock wins over band. `Zone::Clock` carries
    /// the anchor (real clock rect), not the possibly extended hit rect.
    #[inline]
    pub fn classify(&self, x: i32, y: i32) -> Zone {
        let n = self.clock.n.min(MAX_RECTS);
        for (i, &(l, t, r, b)) in self.clock.r[..n].iter().enumerate() {
            if r > l && b > t && x >= l && x < r && y >= t && y < b {
                return Zone::Clock(self.anchor[i]);
            }
        }
        let n = self.band.n.min(MAX_RECTS);
        for (i, &(l, t, r, b)) in self.band.r[..n].iter().enumerate() {
            if r > l && b > t && x >= l && x < r && y >= t && y < b {
                return Zone::Band(i);
            }
        }
        Zone::Outside
    }
}

/// Max growth (physical px) of the clock hit rect to the right of the UIA rect.
/// Measured strips: 6 px (primary, clock 3816 -> ShowDesktopButton 3822) and
/// 12 px (secondary, clock 7668 -> taskbar edge 7680).
pub const EXT_MAX_X: i32 = 32;
/// Max vertical growth (each side) towards the taskbar window's top/bottom.
pub const EXT_MAX_Y: i32 = 16;

fn intersects(a: &Rect, b: &Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

/// Clock hit rect: the UIA clock rect grown into the margin strip that Windows
/// still routes to the clock button (clicks there open the native flyout).
/// - Right: up to the nearest `SystemTray.*` sibling to the right (bell in the
///   swapped layout, ShowDesktopButton), else the taskbar window's right edge,
///   at most `EXT_MAX_X`.
/// - Vertical: up to the taskbar window's top/bottom, at most `EXT_MAX_Y`.
/// - Never left (the neighbour there is the bell / accent button).
///
/// `siblings` must be read in the same scan as `clock` (the clock rect itself may
/// be included; it is ignored). Returns `clock` unchanged when the inputs are
/// invalid, the clock is not inside the taskbar window, or the result would
/// touch any sibling.
pub fn extend_clock(clock: Rect, taskbar: Rect, siblings: &[Rect]) -> Rect {
    let inside = clock.x >= taskbar.x
        && clock.y >= taskbar.y
        && clock.right() <= taskbar.right()
        && clock.bottom() <= taskbar.bottom();
    if !clock.is_valid() || !taskbar.is_valid() || !inside {
        return clock;
    }
    let others = || siblings.iter().filter(|s| s.is_valid() && **s != clock);
    let mut right = taskbar.right().min(clock.right().saturating_add(EXT_MAX_X));
    for s in others() {
        let v_overlap = s.y < clock.bottom() && clock.y < s.bottom();
        if v_overlap && s.right() > clock.right() {
            right = right.min(s.x.max(clock.right()));
        }
    }
    let top = taskbar.y.max(clock.y.saturating_sub(EXT_MAX_Y));
    let bottom = taskbar.bottom().min(clock.bottom().saturating_add(EXT_MAX_Y));
    let hit = Rect::from_ltrb(clock.x, top, right, bottom);
    if others().any(|s| intersects(&hit, s)) {
        return clock;
    }
    hit
}

/// Hit rect computed for one taskbar's clock in a full scan. Only applied while
/// the cached clock rect still equals `anchor` (a moved clock = stale siblings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockExt {
    pub class: &'static str,
    pub hwnd: isize,
    pub anchor: Rect,
    pub hit: Rect,
}

/// Near-band edge trigger (QPC ticks or ms, any monotonic unit). `last == 0` means
/// "never". True when the pointer (re)enters the band after `hold` of absence.
#[inline]
pub fn entering_band(now: i64, last: i64, hold: i64) -> bool {
    last == 0 || now.saturating_sub(last) >= hold
}

/// Fixed-window rate limiter (off the hook thread; for log lines).
#[derive(Debug, Clone)]
pub struct RateLimit {
    pub window_ms: u64,
    pub max: u32,
    start: u64,
    count: u32,
    /// Events suppressed since the last allowed one.
    pub dropped: u32,
}

impl RateLimit {
    pub fn new(window_ms: u64, max: u32) -> Self {
        Self { window_ms: window_ms.max(1), max: max.max(1), start: 0, count: 0, dropped: 0 }
    }

    /// `Some(dropped_before)` if this event may be logged, `None` if suppressed.
    pub fn allow(&mut self, now_ms: u64) -> Option<u32> {
        if self.count == 0 || now_ms.saturating_sub(self.start) >= self.window_ms {
            self.start = now_ms;
            self.count = 0;
        }
        if self.count < self.max {
            self.count += 1;
            Some(std::mem::take(&mut self.dropped))
        } else {
            self.dropped += 1;
            None
        }
    }
}

pub fn ltrb_to_rect(v: Ltrb) -> Rect {
    Rect::from_ltrb(v.0, v.1, v.2, v.3)
}

pub const MSG_LBUTTONDOWN: u32 = 0x0201;
pub const MSG_LBUTTONUP: u32 = 0x0202;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairDecision {
    Pass,
    /// Swallow and toggle the popup.
    SwallowDown,
    /// Swallow the UP paired with a swallowed DOWN (wherever it lands).
    SwallowUp,
}

/// Pairing state machine. `down_pending` = a DOWN was swallowed and its UP not seen yet.
/// Returns `(decision, new_down_pending)`. Only the left button is ever swallowed.
pub fn pair_decide(msg: u32, inside: bool, down_pending: bool) -> (PairDecision, bool) {
    match msg {
        MSG_LBUTTONDOWN if inside => (PairDecision::SwallowDown, true),
        MSG_LBUTTONUP if down_pending => (PairDecision::SwallowUp, false),
        _ => (PairDecision::Pass, down_pending),
    }
}

// ---------------------------------------------------------------- rect cache

/// Last-known-good clock rect for one taskbar window, keyed by (class, hwnd).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub class: &'static str,
    pub hwnd: isize,
    pub monitor: String,
    pub rect: Rect,
    pub misses: u32,
}

/// Result of scanning one taskbar (after retries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanResult {
    Found { rect: Rect, monitor: String },
    /// Clock found but rect empty / off-screen / off-monitor (e.g. auto-hide).
    Invalid,
    Miss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// `moved_from` = previous cached rect when it differed (None: new or unchanged).
    Stored { moved_from: Option<Rect> },
    /// Invalid rect: cached entry (if any) dropped -> pass-through.
    Invalid { dropped: Option<Entry> },
    /// Miss within budget: last-known-good kept.
    Kept { rect: Rect, monitor: String, misses: u32 },
    /// Miss budget exhausted: entry dropped.
    Expired { dropped: Entry },
    /// Miss with nothing cached.
    MissNoEntry,
}

#[derive(Debug, Clone)]
pub struct RectCache {
    pub entries: Vec<Entry>,
    pub miss_expire: u32,
}

impl RectCache {
    pub fn new(miss_expire: u32) -> Self {
        Self { entries: Vec::new(), miss_expire: miss_expire.max(1) }
    }

    /// Full reset (TaskbarCreated / DISPLAYCHANGE / DPICHANGED / SETTINGCHANGE / start / enable).
    pub fn reset(&mut self) -> usize {
        let n = self.entries.len();
        self.entries.clear();
        n
    }

    /// Drop entries whose taskbar window is gone. Returns the dropped entries.
    pub fn retain_alive(&mut self, alive: impl Fn(&Entry) -> bool) -> Vec<Entry> {
        let (keep, gone): (Vec<_>, Vec<_>) = self.entries.drain(..).partition(|e| alive(e));
        self.entries = keep;
        gone
    }

    fn idx(&self, class: &str, hwnd: isize) -> Option<usize> {
        self.entries.iter().position(|e| e.class == class && e.hwnd == hwnd)
    }

    pub fn apply(&mut self, class: &'static str, hwnd: isize, res: ScanResult) -> Applied {
        let idx = self.idx(class, hwnd);
        match res {
            ScanResult::Found { rect, monitor } if rect.is_valid() => {
                let moved_from = match idx {
                    Some(k) => {
                        let e = &mut self.entries[k];
                        let old = e.rect;
                        e.rect = rect;
                        e.monitor = monitor;
                        e.misses = 0;
                        (old != rect).then_some(old)
                    }
                    None => {
                        self.entries.push(Entry { class, hwnd, monitor, rect, misses: 0 });
                        None
                    }
                };
                Applied::Stored { moved_from }
            }
            ScanResult::Found { .. } | ScanResult::Invalid => {
                Applied::Invalid { dropped: idx.map(|k| self.entries.remove(k)) }
            }
            ScanResult::Miss => match idx {
                Some(k) => {
                    self.entries[k].misses += 1;
                    let n = self.entries[k].misses;
                    if n >= self.miss_expire {
                        Applied::Expired { dropped: self.entries.remove(k) }
                    } else {
                        let e = &self.entries[k];
                        Applied::Kept { rect: e.rect, monitor: e.monitor.clone(), misses: n }
                    }
                }
                None => Applied::MissNoEntry,
            },
        }
    }

    pub fn rect_set(&self) -> RectSet {
        RectSet::from_rects(self.entries.iter().map(|e| e.rect))
    }

    /// Hook rects: per entry, the extended hit rect from `ext` if it was computed
    /// for exactly this clock rect, otherwise the plain clock rect.
    pub fn hook_rects(&self, ext: &[ClockExt], band_x: i32) -> HookRects {
        HookRects::with_hits(
            self.entries.iter().map(|e| {
                let hit = ext
                    .iter()
                    .find(|x| x.class == e.class && x.hwnd == e.hwnd && x.anchor == e.rect)
                    .map_or(e.rect, |x| x.hit);
                (e.rect, hit)
            }),
            band_x,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::popup_logic::{decide_toggle, ToggleAction, BLUR_DEBOUNCE_MS};

    const PRIMARY: Rect = Rect::new(3710, 2088, 106, 72);
    const SECOND: Rect = Rect::new(7562, 2088, 106, 72);

    // ---- RectSet ----
    #[test]
    fn hit_inside_both_clocks() {
        let s = RectSet::from_rects([PRIMARY, SECOND]);
        assert_eq!(s.n, 2);
        assert_eq!(s.hit(3710, 2088), Some((3710, 2088, 3816, 2160)));
        assert_eq!(s.hit(3815, 2159), Some((3710, 2088, 3816, 2160)));
        assert_eq!(s.hit(7600, 2100), Some((7562, 2088, 7668, 2160)));
    }

    #[test]
    fn hit_edges_and_neighbours_pass() {
        let s = RectSet::from_rects([PRIMARY]);
        assert_eq!(s.hit(3816, 2100), None, "right edge exclusive");
        assert_eq!(s.hit(3700, 2100), None, "button left of the clock");
        assert_eq!(s.hit(3750, 2087), None, "above the taskbar");
        assert_eq!(s.hit(3750, 2160), None, "bottom edge exclusive");
    }

    #[test]
    fn empty_or_invalid_rects_never_hit() {
        assert_eq!(RectSet::EMPTY.hit(0, 0), None);
        let s = RectSet::from_rects([Rect::new(0, 0, 0, 10), Rect::new(5, 5, 10, -3)]);
        assert_eq!(s.n, 0);
        // Even a hand-built degenerate entry must not hit.
        let mut raw = RectSet::EMPTY;
        raw.r[0] = (10, 10, 10, 20);
        raw.n = 1;
        assert_eq!(raw.hit(10, 15), None);
    }

    #[test]
    fn rect_set_caps_at_max() {
        let many = (0..20).map(|i| Rect::new(i * 10, 0, 5, 5));
        let s = RectSet::from_rects(many);
        assert_eq!(s.n, MAX_RECTS);
        assert!(s.hit(72, 1).is_some(), "index 7 = x 70..75 tracked");
        assert!(s.hit(82, 1).is_none(), "index 8 = x 80..85 past the cap");
    }

    #[test]
    fn negative_coordinates() {
        let s = RectSet::from_rects([Rect::new(-130, 732, 100, 48)]);
        assert!(s.hit(-100, 750).is_some());
        assert!(s.hit(-131, 750).is_none());
    }

    // ---- pairing ----
    #[test]
    fn down_inside_then_up_anywhere_swallowed() {
        let (d, p) = pair_decide(MSG_LBUTTONDOWN, true, false);
        assert_eq!((d, p), (PairDecision::SwallowDown, true));
        // UP lands outside (user dragged off the clock): still swallowed.
        let (d, p) = pair_decide(MSG_LBUTTONUP, false, p);
        assert_eq!((d, p), (PairDecision::SwallowUp, false));
        // A following UP (no pending DOWN) passes.
        assert_eq!(pair_decide(MSG_LBUTTONUP, true, p), (PairDecision::Pass, false));
    }

    #[test]
    fn down_outside_passes_and_up_passes() {
        let (d, p) = pair_decide(MSG_LBUTTONDOWN, false, false);
        assert_eq!((d, p), (PairDecision::Pass, false));
        // UP inside the clock without a swallowed DOWN must pass (no half click).
        assert_eq!(pair_decide(MSG_LBUTTONUP, true, p), (PairDecision::Pass, false));
    }

    #[test]
    fn other_buttons_pass_and_keep_state() {
        for m in [0x0204u32, 0x0205, 0x0207, 0x0208, 0x020A, 0x020B, 0x020C, 0x0200] {
            assert_eq!(pair_decide(m, true, false), (PairDecision::Pass, false), "msg {m:#x}");
            assert_eq!(pair_decide(m, true, true), (PairDecision::Pass, true), "msg {m:#x} pending");
        }
    }

    #[test]
    fn right_click_on_clock_passes_between_swallowed_pair() {
        let (_, p) = pair_decide(MSG_LBUTTONDOWN, true, false);
        let (d, p) = pair_decide(0x0204, true, p); // RBUTTONDOWN
        assert_eq!(d, PairDecision::Pass);
        let (d, _) = pair_decide(MSG_LBUTTONUP, false, p);
        assert_eq!(d, PairDecision::SwallowUp);
    }

    // ---- cache ----
    fn found(r: Rect) -> ScanResult {
        ScanResult::Found { rect: r, monitor: "M".into() }
    }

    #[test]
    fn store_then_rect_set() {
        let mut c = RectCache::new(20);
        assert_eq!(c.apply("Shell_TrayWnd", 1, found(PRIMARY)), Applied::Stored { moved_from: None });
        assert_eq!(
            c.apply("Shell_SecondaryTrayWnd", 2, found(SECOND)),
            Applied::Stored { moved_from: None }
        );
        let s = c.rect_set();
        assert_eq!(s.n, 2);
        assert!(s.hit(3750, 2100).is_some() && s.hit(7600, 2100).is_some());
    }

    #[test]
    fn miss_keeps_last_known_good_until_expiry() {
        let mut c = RectCache::new(3);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        assert!(matches!(c.apply("Shell_TrayWnd", 1, ScanResult::Miss), Applied::Kept { misses: 1, rect, .. } if rect == PRIMARY));
        assert!(matches!(c.apply("Shell_TrayWnd", 1, ScanResult::Miss), Applied::Kept { misses: 2, .. }));
        assert_eq!(c.rect_set().n, 1, "still kept");
        assert!(matches!(c.apply("Shell_TrayWnd", 1, ScanResult::Miss), Applied::Expired { .. }));
        assert_eq!(c.rect_set().n, 0, "expired -> pass-through");
        assert_eq!(c.apply("Shell_TrayWnd", 1, ScanResult::Miss), Applied::MissNoEntry);
    }

    #[test]
    fn found_resets_miss_counter() {
        let mut c = RectCache::new(3);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        c.apply("Shell_TrayWnd", 1, ScanResult::Miss);
        c.apply("Shell_TrayWnd", 1, ScanResult::Miss);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        assert_eq!(c.entries[0].misses, 0);
        assert!(matches!(c.apply("Shell_TrayWnd", 1, ScanResult::Miss), Applied::Kept { misses: 1, .. }));
    }

    #[test]
    fn invalid_rect_drops_immediately() {
        let mut c = RectCache::new(20);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        assert!(matches!(c.apply("Shell_TrayWnd", 1, ScanResult::Invalid), Applied::Invalid { dropped: Some(_) }));
        assert_eq!(c.rect_set().n, 0);
        // A "found" with an empty rect counts as invalid too.
        assert_eq!(
            c.apply("Shell_TrayWnd", 1, found(Rect::new(0, 0, 0, 0))),
            Applied::Invalid { dropped: None }
        );
        assert_eq!(c.rect_set().n, 0);
    }

    #[test]
    fn reset_and_dead_window_drop() {
        let mut c = RectCache::new(20);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        c.apply("Shell_SecondaryTrayWnd", 2, found(SECOND));
        let gone = c.retain_alive(|e| e.hwnd != 2);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].hwnd, 2);
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.reset(), 1);
        assert_eq!(c.rect_set().n, 0);
    }

    #[test]
    fn same_class_different_hwnd_are_separate() {
        let mut c = RectCache::new(2);
        c.apply("Shell_SecondaryTrayWnd", 10, found(PRIMARY));
        c.apply("Shell_SecondaryTrayWnd", 11, found(SECOND));
        c.apply("Shell_SecondaryTrayWnd", 10, ScanResult::Miss);
        assert!(matches!(c.apply("Shell_SecondaryTrayWnd", 10, ScanResult::Miss), Applied::Expired { .. }));
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].hwnd, 11);
    }

    #[test]
    fn miss_expire_zero_is_clamped() {
        let mut c = RectCache::new(0);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        assert!(matches!(c.apply("Shell_TrayWnd", 1, ScanResult::Miss), Applied::Expired { .. }));
    }

    // ---- clock shift (measured 2026-11-17: 3710 <-> 3668, w=106, both taskbars) ----
    const SHIFTED: Rect = Rect::new(3668, 2088, 106, 72);
    /// Measured left neighbour in the unshifted layout: SystemTray.OmniButtonRight 3662..3698.
    const NEIGHBOUR_X: i32 = 3680;
    const Y: i32 = 2120;

    fn hook_rects(c: &RectCache) -> HookRects {
        HookRects::new(c.rect_set(), BAND_X)
    }

    #[test]
    fn shift_is_reported_as_moved() {
        let mut c = RectCache::new(20);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        assert_eq!(
            c.apply("Shell_TrayWnd", 1, found(SHIFTED)),
            Applied::Stored { moved_from: Some(PRIMARY) }
        );
        assert_eq!(c.apply("Shell_TrayWnd", 1, found(SHIFTED)), Applied::Stored { moved_from: None });
        assert_eq!(
            c.apply("Shell_TrayWnd", 1, found(PRIMARY)),
            Applied::Stored { moved_from: Some(SHIFTED) }
        );
    }

    #[test]
    fn shift_3710_3668_tracked_both_halves_swallowed() {
        let mut c = RectCache::new(20);
        // Unshifted: 3790 (clock right part) swallowed.
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        assert!(matches!(hook_rects(&c).classify(3790, Y), Zone::Clock(_)));
        // Shift picked up by a probe -> the new left part 3668..3710 is swallowed.
        c.apply("Shell_TrayWnd", 1, found(SHIFTED));
        let h = hook_rects(&c);
        assert!(matches!(h.classify(3690, Y), Zone::Clock(_)), "3690 on shifted clock");
        assert!(matches!(h.classify(3668, Y), Zone::Clock(_)), "new left edge");
        // The strip the clock left (3774..3816) is not swallowed any more (logged).
        assert_eq!(h.classify(3790, Y), Zone::Band(0));
        // Shift back.
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        let h = hook_rects(&c);
        assert!(matches!(h.classify(3790, Y), Zone::Clock(_)));
        assert_eq!(h.classify(3690, Y), Zone::Band(0));
    }

    #[test]
    fn stale_alternate_rect_is_not_a_hit_area_neighbour_protected() {
        // Deliberately NO union of recently seen rects: in the unshifted layout the
        // shifted rect's extra strip 3668..3710 covers 30 of the 36 px of the
        // SystemTray.OmniButtonRight neighbour (3662..3698).
        let mut c = RectCache::new(20);
        c.apply("Shell_TrayWnd", 1, found(SHIFTED));
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        let h = hook_rects(&c);
        assert_eq!(h.clock.n, 1, "only the current rect per taskbar");
        assert_eq!(h.classify(NEIGHBOUR_X, Y), Zone::Band(0), "neighbour click passes");
    }

    #[test]
    fn neighbour_outside_both_rects_passes() {
        for cur in [PRIMARY, SHIFTED] {
            let mut c = RectCache::new(20);
            c.apply("Shell_TrayWnd", 1, found(cur));
            let h = hook_rects(&c);
            // Tray icon 3566..3614 and show-desktop 3822..3840: outside both rects.
            assert_eq!(h.classify(3590, Y), Zone::Band(0), "tray icon, cur={cur:?}");
            assert_eq!(h.classify(3830, Y), Zone::Band(0), "show desktop, cur={cur:?}");
            assert_eq!(h.classify(3000, Y), Zone::Outside, "far left, cur={cur:?}");
            assert_eq!(h.classify(3750, 2000), Zone::Outside, "above taskbar, cur={cur:?}");
        }
    }

    #[test]
    fn bands_follow_clock_order_and_two_taskbars() {
        let h = HookRects::new(RectSet::from_rects([PRIMARY, SECOND]), BAND_X);
        assert_eq!(h.band.n, 2);
        assert_eq!(h.band.r[0], (3510, 2088, 4016, 2160));
        assert_eq!(h.classify(7400, Y), Zone::Band(1));
        assert!(matches!(h.classify(7600, Y), Zone::Clock((7562, ..))));
        assert_eq!(HookRects::EMPTY.classify(3750, Y), Zone::Outside, "empty = pass");
        // Band math never overflows.
        let e = HookRects::new(RectSet::from_rects([Rect::new(i32::MAX - 10, 0, 5, 5)]), BAND_X);
        assert_eq!(e.band.r[0].2, i32::MAX);
    }

    #[test]
    fn entering_band_edge_trigger() {
        assert!(entering_band(100, 0, 50), "never seen");
        assert!(!entering_band(100, 80, 50), "still near");
        assert!(entering_band(200, 100, 50), "re-entered after hold");
        assert!(!entering_band(50, 100, 50), "clock skew is not an entry");
    }

    #[test]
    fn rate_limit_window() {
        let mut r = RateLimit::new(10_000, 3);
        assert_eq!(r.allow(0), Some(0));
        assert_eq!(r.allow(1), Some(0));
        assert_eq!(r.allow(2), Some(0));
        assert_eq!(r.allow(3), None);
        assert_eq!(r.allow(4), None);
        assert_eq!(r.allow(10_000), Some(2), "new window reports drops");
        assert_eq!(r.allow(10_001), Some(0));
    }

    // ---- margin strip extension (measured 2026-11-17) ----
    // Taskbar windows assumed to span the clock's band (to be confirmed by the
    // `tb=[..]` log line); the right edges are the monitor edges 3840 / 7680.
    const TB1: Rect = Rect::from_ltrb(0, 2088, 3840, 2160);
    const TB2: Rect = Rect::from_ltrb(3840, 2088, 7680, 2160);
    const BELL_L: Rect = Rect::from_ltrb(3662, 2088, 3698, 2160);
    const SHOW_DESKTOP: Rect = Rect::from_ltrb(3822, 2088, 3840, 2160);
    /// Swapped layout: clock 3668..3774, bell 3780..3816 (secondary 7520..7626 / 7632..7668).
    const BELL_R: Rect = Rect::from_ltrb(3780, 2088, 3816, 2160);
    const BELL_R2: Rect = Rect::from_ltrb(7632, 2088, 7668, 2160);
    const SHIFTED2: Rect = Rect::from_ltrb(7520, 2088, 7626, 2160);

    fn ext(class: &'static str, hwnd: isize, anchor: Rect, hit: Rect) -> ClockExt {
        ClockExt { class, hwnd, anchor, hit }
    }

    #[test]
    fn extend_stops_before_show_desktop_on_primary() {
        let hit = extend_clock(PRIMARY, TB1, &[BELL_L, PRIMARY, SHOW_DESKTOP]);
        assert_eq!(hit, Rect::from_ltrb(3710, 2088, 3822, 2160));
    }

    #[test]
    fn extend_reaches_taskbar_edge_on_secondary() {
        let hit = extend_clock(SECOND, TB2, &[SECOND]);
        assert_eq!(hit, Rect::from_ltrb(7562, 2088, 7680, 2160));
    }

    #[test]
    fn extend_swapped_layout_stops_at_bell() {
        assert_eq!(
            extend_clock(SHIFTED, TB1, &[SHIFTED, BELL_R, SHOW_DESKTOP]),
            Rect::from_ltrb(3668, 2088, 3780, 2160)
        );
        assert_eq!(
            extend_clock(SHIFTED2, TB2, &[SHIFTED2, BELL_R2]),
            Rect::from_ltrb(7520, 2088, 7632, 2160)
        );
    }

    #[test]
    fn extend_vertical_to_taskbar_band_capped() {
        let tall = Rect::from_ltrb(0, 2080, 3840, 2160);
        assert_eq!(extend_clock(PRIMARY, tall, &[]).y, 2080, "8 px gap closed");
        let huge = Rect::from_ltrb(0, 1000, 3840, 2160);
        assert_eq!(extend_clock(PRIMARY, huge, &[]).y, 2088 - EXT_MAX_Y, "capped");
        let wide = Rect::from_ltrb(0, 2088, 9000, 2160);
        assert_eq!(extend_clock(PRIMARY, wide, &[]).right(), 3816 + EXT_MAX_X, "capped");
    }

    #[test]
    fn extend_refuses_bad_inputs() {
        // Clock not inside the taskbar window (auto-hide / stale window rect).
        assert_eq!(extend_clock(PRIMARY, TB2, &[]), PRIMARY);
        assert_eq!(extend_clock(PRIMARY, Rect::new(0, 0, 0, 0), &[]), PRIMARY);
        // A sibling overlapping the would-be hit rect vertically above -> no extension.
        let tall = Rect::from_ltrb(0, 2060, 3840, 2160);
        let above = Rect::from_ltrb(3700, 2070, 3760, 2086);
        assert_eq!(extend_clock(PRIMARY, tall, &[above]), PRIMARY);
        // Never grows left.
        assert_eq!(extend_clock(PRIMARY, TB1, &[]).x, 3710);
    }

    #[test]
    fn extended_hit_swallows_strip_anchor_stays_real() {
        let mut c = RectCache::new(20);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        c.apply("Shell_SecondaryTrayWnd", 2, found(SECOND));
        let exts = [
            ext("Shell_TrayWnd", 1, PRIMARY, extend_clock(PRIMARY, TB1, &[BELL_L, SHOW_DESKTOP])),
            ext("Shell_SecondaryTrayWnd", 2, SECOND, extend_clock(SECOND, TB2, &[])),
        ];
        let h = c.hook_rects(&exts, BAND_X);
        let a1 = (3710, 2088, 3816, 2160);
        let a2 = (7562, 2088, 7668, 2160);
        // Measured near-miss points from the user's real clicks.
        assert_eq!(h.classify(3819, 2116), Zone::Clock(a1));
        for (x, y) in [(7669, 2107), (7670, 2112), (7673, 2091), (7675, 2135), (7679, 2128)] {
            assert_eq!(h.classify(x, y), Zone::Clock(a2), "({x},{y})");
        }
        // Show desktop and the bell on the left still pass.
        assert_eq!(h.classify(3822, Y), Zone::Band(0));
        assert_eq!(h.classify(3830, Y), Zone::Band(0));
        assert_eq!(h.classify(NEIGHBOUR_X, Y), Zone::Band(0));
        // Beyond the taskbar edge / below the band passes.
        assert_eq!(h.classify(7680, Y), Zone::Band(1));
        assert_eq!(h.classify(7670, 2160), Zone::Outside);
    }

    #[test]
    fn stale_extension_is_not_applied_after_swap() {
        // Extension computed for the normal layout, then the clock swaps with the bell.
        let mut c = RectCache::new(20);
        c.apply("Shell_TrayWnd", 1, found(PRIMARY));
        let exts = [ext("Shell_TrayWnd", 1, PRIMARY, Rect::from_ltrb(3710, 2088, 3822, 2160))];
        c.apply("Shell_TrayWnd", 1, found(SHIFTED));
        let h = c.hook_rects(&exts, BAND_X);
        assert_eq!(h.clock.r[0], (3668, 2088, 3774, 2160), "plain rect until the next full scan");
        // Swapped bell 3780..3816 is NOT swallowed.
        assert_eq!(h.classify(3790, Y), Zone::Band(0));
        // Fresh extension for the swapped layout covers only the 6 px gap.
        let exts = [ext("Shell_TrayWnd", 1, SHIFTED, extend_clock(SHIFTED, TB1, &[BELL_R, SHOW_DESKTOP]))];
        let h = c.hook_rects(&exts, BAND_X);
        assert_eq!(h.classify(3777, Y), Zone::Clock((3668, 2088, 3774, 2160)));
        assert_eq!(h.classify(3780, Y), Zone::Band(0), "swapped bell passes");
    }

    #[test]
    fn with_hits_skips_invalid_and_matches_new() {
        let h = HookRects::with_hits([(PRIMARY, PRIMARY), (Rect::new(0, 0, 0, 0), PRIMARY)], BAND_X);
        assert_eq!(h, HookRects::new(RectSet::from_rects([PRIMARY]), BAND_X));
    }

    // ---- click path uses the same toggle debounce ----
    #[test]
    fn click_path_toggle_cycle() {
        // 1) hidden, clock click -> Show
        assert_eq!(decide_toggle(false, None, 1_000, BLUR_DEBOUNCE_MS), ToggleAction::Show);
        // 2) visible, clock click; DOWN swallowed so the popup keeps focus -> Hide
        assert_eq!(decide_toggle(true, None, 2_000, BLUR_DEBOUNCE_MS), ToggleAction::Hide);
        // 3) if the popup did blur-hide just before the click event arrived, the same
        //    click must not reopen it
        assert_eq!(decide_toggle(false, Some(3_000), 3_040, BLUR_DEBOUNCE_MS), ToggleAction::Ignore);
        // 4) a later deliberate click reopens
        assert_eq!(decide_toggle(false, Some(3_000), 3_400, BLUR_DEBOUNCE_MS), ToggleAction::Show);
    }
}
