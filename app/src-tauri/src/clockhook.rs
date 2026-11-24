//! P2-b: taskbar-clock click interception (ported from clockcal/poc `hook`).
//!
//! Threads:
//! - hook thread: WH_MOUSE_LL + message loop. Exists only while interception is enabled.
//!   The callback only classifies the point against a cached fixed-size rect set
//!   (`try_read`, never blocks), runs the DOWN/UP pairing state machine and posts plain
//!   integers with `PostThreadMessageW` (no allocation, no logging, no UIA, no Tauri):
//!   swallowed DOWN -> dispatcher (toggle); un-swallowed DOWN near a clock -> dispatcher
//!   (near-miss log); pointer entering the near band -> UIA thread (immediate probe).
//! - UIA thread (MTA): full rect scan on start/enable, TaskbarCreated, DISPLAYCHANGE,
//!   DPICHANGED, SETTINGCHANGE (full reset) and every `REFRESH_MS` (timer, enabled only);
//!   plus a cheap re-probe of the cached clock elements every `PROBE_NEAR_MS` while the
//!   pointer is near a clock and every `PROBE_IDLE_MS` otherwise (the clock can shift by
//!   42 px for a few seconds; a 1.5 s scan alone leaves the new strip unswallowed).
//! - dispatcher thread: turns click posts into `toggle_popup` on the Tauri main thread,
//!   logs near-misses (rate limited), runs the hook watchdog and the `--hook-timeout`.
//!
//! Unhook paths: `disable` (tray toggle / timeout / exit), hook-thread exit, panic hook,
//! `shutdown` from `RunEvent::Exit`. Windows also removes the hook when the process dies.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, Ordering::*};
use std::sync::mpsc::sync_channel;
use std::sync::{Mutex, OnceLock, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tauri::AppHandle;
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::IUIAutomationElement;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::applog::now_ms;
use crate::clock_logic::{
    entering_band, extend_clock, ltrb_to_rect, pair_decide, Applied, ClockExt, HookRects,
    PairDecision, RateLimit, RectCache, ScanResult, Zone, BAND_X, MAX_RECTS,
};
use crate::log;
use crate::popup_logic::Rect;
use crate::uia::{taskbars, Btn, Scanner};

pub const DEFAULT_TIMEOUT_S: u64 = 60;
pub const MAX_TIMEOUT_S: u64 = 600;
pub const DEFAULT_MISS_EXPIRE: u32 = 20;
const REFRESH_MS: u32 = 1500;
/// Probe timer tick; also the probe period while the pointer is near a clock.
const PROBE_NEAR_MS: u32 = 100;
/// Probe period when the pointer is elsewhere.
const PROBE_IDLE_MS: u64 = 400;
/// "Near" stays active this long after the last move inside a band.
const NEAR_ACTIVE_MS: i64 = 1000;
/// Pointer must be out of the band this long before a re-entry posts a probe.
const NEAR_HOLD_MS: i64 = 300;
/// Coalesce probe requests closer than this.
const PROBE_MIN_GAP: Duration = Duration::from_millis(30);
/// A probe failure falls back to a full scan at most this often.
const PROBE_FAIL_SCAN_GAP: Duration = Duration::from_millis(400);
const MISS_RETRIES: u32 = 3;
const MISS_RETRY_GAP_MS: u64 = 120;
const WATCHDOG_STALE: Duration = Duration::from_millis(1500);
const WATCHDOG_MIN_GAP: Duration = Duration::from_secs(3);

const WM_APP_REINSTALL: u32 = WM_APP + 1;
const WM_APP_REFRESH: u32 = WM_APP + 3;
const WM_APP_CLICK: u32 = WM_APP + 4;
const WM_APP_NEARMISS: u32 = WM_APP + 5;
const WM_APP_PROBE: u32 = WM_APP + 6;

const TIMER_SCAN: usize = 1;
const TIMER_PROBE: usize = 2;

// ---------------------------------------------------------------- shared state

static RECTS: RwLock<HookRects> = RwLock::new(HookRects::EMPTY);
static HOOK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static DOWN_PENDING: AtomicBool = AtomicBool::new(false);
static ENABLED: AtomicBool = AtomicBool::new(false);

static QPC_FREQ: AtomicI64 = AtomicI64::new(1);
static NEAR_HOLD_TICKS: AtomicI64 = AtomicI64::new(i64::MAX);
static LAST_CB_QPC: AtomicI64 = AtomicI64::new(0);
static LAST_NEAR_QPC: AtomicI64 = AtomicI64::new(0);
/// `applog::now_ms` of the last RECTS publish (0 = never).
static LAST_REFRESH_MS: AtomicU64 = AtomicU64::new(0);
static CB_COUNT: AtomicU64 = AtomicU64::new(0);
static MAX_US: AtomicU64 = AtomicU64::new(0);
static SWALLOWED: AtomicU64 = AtomicU64::new(0);
static NEAR_MISSES: AtomicU64 = AtomicU64::new(0);
static BAND_ENTRIES: AtomicU64 = AtomicU64::new(0);
static POST_FAILED: AtomicU64 = AtomicU64::new(0);
static REINSTALLS: AtomicU32 = AtomicU32::new(0);
static HOOK_TID: AtomicU32 = AtomicU32::new(0);
static UIA_TID: AtomicU32 = AtomicU32::new(0);
static DISPATCH_TID: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static RETRY_RECOVERED: AtomicU64 = AtomicU64::new(0);
static EXPIRED: AtomicU64 = AtomicU64::new(0);
static PROBES: AtomicU64 = AtomicU64::new(0);
static PROBE_MAX_US: AtomicU64 = AtomicU64::new(0);
static PROBE_FAILS: AtomicU64 = AtomicU64::new(0);
static MOVES_PROBE: AtomicU64 = AtomicU64::new(0);
static MOVES_SCAN: AtomicU64 = AtomicU64::new(0);
static EXT_DUMPS: AtomicU64 = AtomicU64::new(0);
static EXT_DUMP_MAX_US: AtomicU64 = AtomicU64::new(0);

static APP: OnceLock<AppHandle> = OnceLock::new();

struct Ctrl {
    hook_join: Option<JoinHandle<()>>,
    deadline: Option<Instant>,
    timeout: Duration,
}

static CTRL: Mutex<Ctrl> = Mutex::new(Ctrl {
    hook_join: None,
    deadline: None,
    timeout: Duration::from_secs(DEFAULT_TIMEOUT_S),
});

fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

/// Pass to `init` for "no auto-disable" (settings-driven, always-on interception).
pub const UNLIMITED: u64 = u64::MAX;

/// Clamp a user-supplied `--hook-timeout` into 1..=600 s.
pub fn clamp_timeout(s: u64) -> u64 {
    s.clamp(1, MAX_TIMEOUT_S)
}

pub fn is_enabled() -> bool {
    ENABLED.load(SeqCst)
}

fn pack(a: i32, b: i32) -> u64 {
    ((a as u32 as u64) << 32) | (b as u32 as u64)
}

fn unpack(v: u64) -> (i32, i32) {
    ((v >> 32) as u32 as i32, v as u32 as i32)
}

fn post(tid: u32, msg: u32, wp: usize, lp: isize) -> bool {
    tid != 0 && unsafe { PostThreadMessageW(tid, msg, WPARAM(wp), LPARAM(lp)).is_ok() }
}

// ---------------------------------------------------------------- the callback

#[inline]
fn classify(x: i32, y: i32) -> Zone {
    // Never blocks: if the UIA thread holds the write lock, treat as "no rect" (pass).
    RECTS.try_read().ok().map_or(Zone::Outside, |g| g.classify(x, y))
}

/// Pointer is in a clock or its band: remember it, and on (re)entry ask the UIA
/// thread for an immediate probe. Atomics + one PostThreadMessageW only.
#[inline]
fn mark_near(t0: i64) {
    let prev = LAST_NEAR_QPC.swap(t0, Relaxed);
    if entering_band(t0, prev, NEAR_HOLD_TICKS.load(Relaxed)) {
        BAND_ENTRIES.fetch_add(1, Relaxed);
        post(UIA_TID.load(Relaxed), WM_APP_PROBE, 0, 0);
    }
}

unsafe extern "system" fn ll_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode != HC_ACTION as i32 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let t0 = qpc();
    LAST_CB_QPC.store(t0, Relaxed);
    CB_COUNT.fetch_add(1, Relaxed);
    let msg = wparam.0 as u32;
    if msg == WM_MOUSEMOVE {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if classify(info.pt.x, info.pt.y) != Zone::Outside {
            mark_near(t0);
        }
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    if msg != WM_LBUTTONDOWN && msg != WM_LBUTTONUP {
        // Right/middle/X buttons, wheel: always pass.
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    let zone = if msg == WM_LBUTTONDOWN { classify(info.pt.x, info.pt.y) } else { Zone::Outside };
    let hit_rect = match zone {
        Zone::Clock(r) => Some(r),
        _ => None,
    };
    let (d, pending) = pair_decide(msg, hit_rect.is_some(), DOWN_PENDING.load(Relaxed));
    DOWN_PENDING.store(pending, Relaxed);
    if d != PairDecision::Pass {
        SWALLOWED.fetch_add(1, Relaxed);
    }
    if let (PairDecision::SwallowDown, Some((l, t, r, b))) = (d, hit_rect) {
        let posted = post(
            DISPATCH_TID.load(Relaxed),
            WM_APP_CLICK,
            pack(l, t) as usize,
            pack(r, b) as isize,
        );
        if !posted {
            POST_FAILED.fetch_add(1, Relaxed);
        }
    }
    if let Zone::Band(i) = zone {
        // Passed through but near a clock: log off-thread, re-probe.
        NEAR_MISSES.fetch_add(1, Relaxed);
        LAST_NEAR_QPC.store(t0, Relaxed);
        post(DISPATCH_TID.load(Relaxed), WM_APP_NEARMISS, pack(info.pt.x, info.pt.y) as usize, i as isize);
    }
    let us = (qpc() - t0).max(0) as u64 * 1_000_000 / QPC_FREQ.load(Relaxed).max(1) as u64;
    MAX_US.fetch_max(us, Relaxed);
    if d == PairDecision::Pass {
        CallNextHookEx(None, ncode, wparam, lparam)
    } else {
        LRESULT(1)
    }
}

// ---------------------------------------------------------------- hook install / remove

/// Single owner of unhooking: whoever swaps the handle out unhooks it. Any thread.
fn unhook_now(who: &str) -> bool {
    let h = HOOK.swap(null_mut(), SeqCst);
    if h.is_null() {
        return false;
    }
    let ok = unsafe { UnhookWindowsHookEx(HHOOK(h)).is_ok() };
    log!("[hook] UnhookWindowsHookEx by {who}: ok={ok}");
    ok
}

fn install(hmod: HINSTANCE) -> windows::core::Result<()> {
    let h = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(ll_proc), hmod, 0)? };
    let old = HOOK.swap(h.0, SeqCst);
    if !old.is_null() {
        let ok = unsafe { UnhookWindowsHookEx(HHOOK(old)).is_ok() };
        log!("[hook] old handle {old:?} unhooked after reinstall: ok={ok} (false is expected if Windows already removed it)");
    }
    log!("[hook] installed WH_MOUSE_LL handle={:?} tid={}", h.0, unsafe { GetCurrentThreadId() });
    Ok(())
}

fn hook_thread(ready: std::sync::mpsc::SyncSender<Result<u32, String>>) {
    unsafe {
        let mut msg = MSG::default();
        // Create the message queue before announcing the thread id.
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let hmod: HINSTANCE = match GetModuleHandleW(None) {
            Ok(m) => m.into(),
            Err(e) => {
                let _ = ready.send(Err(format!("GetModuleHandleW: {e}")));
                return;
            }
        };
        if let Err(e) = install(hmod) {
            let _ = ready.send(Err(format!("SetWindowsHookExW: {e}")));
            return;
        }
        let _ = ready.send(Ok(GetCurrentThreadId()));
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 || r.0 == -1 {
                break;
            }
            if msg.message == WM_APP_REINSTALL {
                REINSTALLS.fetch_add(1, SeqCst);
                if let Err(e) = install(hmod) {
                    log!("[watchdog] reinstall FAILED: {e}");
                }
            } else {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        unhook_now("hook-thread-exit");
    }
}

/// Turn interception on (no-op if already on). Arms the `--hook-timeout` deadline.
pub fn enable(why: &str) -> bool {
    let Ok(mut c) = CTRL.lock() else { return false };
    if HOOK_TID.load(SeqCst) != 0 {
        return true;
    }
    DOWN_PENDING.store(false, SeqCst);
    if why != "start" {
        // Fresh full scan; until it lands the rect set may be empty (= pass-through).
        post_refresh(5);
    }
    let (tx, rx) = sync_channel(1);
    let jh = thread::Builder::new()
        .name("clockcal-hook".into())
        .spawn(move || hook_thread(tx));
    let jh = match jh {
        Ok(j) => j,
        Err(e) => {
            log!("[hook] spawn failed: {e}");
            return false;
        }
    };
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(tid)) => {
            HOOK_TID.store(tid, SeqCst);
            c.hook_join = Some(jh);
            c.deadline = if c.timeout.is_zero() { None } else { Some(Instant::now() + c.timeout) };
            ENABLED.store(true, SeqCst);
            log!("[hook] enabled ({why}) timeout={}s", c.timeout.as_secs());
            true
        }
        Ok(Err(e)) => {
            log!("[hook] install failed ({why}): {e}");
            let _ = jh.join();
            false
        }
        Err(_) => {
            log!("[hook] hook thread not ready in 5s ({why}); forcing unhook");
            unhook_now("enable-timeout");
            false
        }
    }
}

/// Turn interception off and remove the hook (no-op if already off).
pub fn disable(why: &str) {
    let Ok(mut c) = CTRL.lock() else {
        unhook_now("disable-poisoned");
        return;
    };
    let was = ENABLED.swap(false, SeqCst);
    c.deadline = None;
    let tid = HOOK_TID.swap(0, SeqCst);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    if let Some(j) = c.hook_join.take() {
        let _ = j.join();
    }
    unhook_now("disable-final");
    DOWN_PENDING.store(false, SeqCst);
    if let Ok(mut g) = RECTS.write() {
        *g = HookRects::EMPTY;
    }
    if was || tid != 0 {
        log!(
            "[hook] disabled ({why}); hook handle now={:?}",
            HOOK.load(SeqCst)
        );
    }
}

// ---------------------------------------------------------------- UIA refresh thread

fn reason_name(code: usize) -> &'static str {
    match code {
        0 => "start",
        1 => "WM_DISPLAYCHANGE",
        2 => "WM_DPICHANGED",
        3 => "WM_SETTINGCHANGE",
        4 => "TaskbarCreated",
        5 => "enable",
        _ => "?",
    }
}

fn post_refresh(code: usize) {
    post(UIA_TID.load(SeqCst), WM_APP_REFRESH, code, 0);
}

unsafe extern "system" fn hidden_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let tc = TASKBAR_CREATED.load(Relaxed);
    let code = match msg {
        WM_DISPLAYCHANGE => Some(1),
        WM_DPICHANGED => Some(2),
        WM_SETTINGCHANGE => Some(3),
        m if tc != 0 && m == tc => Some(4),
        _ => None,
    };
    if let Some(c) = code {
        // Post rather than refresh inline (broadcasts may be sent messages).
        let _ = PostMessageW(hwnd, WM_APP_REFRESH, WPARAM(c), LPARAM(0));
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn fmt(r: &Rect) -> String {
    format!("left={} top={} w={} h={}", r.x, r.y, r.w, r.h)
}

fn fmt_ltrb((l, t, r, b): (i32, i32, i32, i32)) -> String {
    format!("{l},{t}..{r},{b}")
}

fn btn_rect(b: &Btn) -> Rect {
    Rect::from_ltrb(b.rect.left, b.rect.top, b.rect.right, b.rect.bottom)
}

fn btn_result(b: &Btn) -> ScanResult {
    if b.valid {
        ScanResult::Found { rect: btn_rect(b), monitor: b.monitor.clone() }
    } else {
        ScanResult::Invalid
    }
}

/// Is the pointer near a clock right now (moved inside a band within NEAR_ACTIVE_MS)?
fn near_active() -> bool {
    let last = LAST_NEAR_QPC.load(Relaxed);
    let f = QPC_FREQ.load(Relaxed).max(1);
    last != 0 && qpc().saturating_sub(last) < f * NEAR_ACTIVE_MS / 1000
}

struct Uia {
    scanner: Scanner,
    cache: RectCache,
    last: String,
    /// (class, taskbar hwnd, clock element) from the last full scan (rule1 matches only).
    elems: Vec<(&'static str, isize, IUIAutomationElement)>,
    last_probe: Instant,
    last_full: Instant,
    dump_rl: RateLimit,
    probe_err_rl: RateLimit,
    /// Extended clock hit rects from the last full scan (see `extend_clock`).
    ext: Vec<ClockExt>,
}

/// Taskbar window rect (physical px; the process is Per-Monitor-V2 aware).
fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = windows::Win32::Foundation::RECT::default();
    unsafe { GetWindowRect(hwnd, &mut r) }.ok()?;
    let v = Rect::from_ltrb(r.left, r.top, r.right, r.bottom);
    v.is_valid().then_some(v)
}

impl Uia {
    /// Margin-strip extension for one taskbar's clock. Reads the taskbar window
    /// rect and every `SystemTray.*` button in this same pass; the dump must
    /// contain the clock at exactly `clock`, else no extension (layout changed
    /// between the two reads).
    fn compute_ext(&self, class: &'static str, hwnd: HWND, clock: Rect) -> (Option<ClockExt>, String) {
        let Some(tb) = window_rect(hwnd) else {
            return (None, "ext=none(no taskbar rect)".into());
        };
        let t = Instant::now();
        let btns = match self.scanner.dump_tray(hwnd) {
            Ok(v) => v,
            Err(e) => return (None, format!("ext=none(dump failed: {e})")),
        };
        EXT_DUMPS.fetch_add(1, Relaxed);
        EXT_DUMP_MAX_US.fetch_max(t.elapsed().as_micros() as u64, Relaxed);
        let sibs: Vec<Rect> = btns.iter().map(btn_rect).collect();
        if !btns.iter().any(|b| b.class_name == crate::uia::CLOCK_CLASS && btn_rect(b) == clock) {
            return (None, format!("ext=none(clock not in dump) tb=[{}]", fmt(&tb)));
        }
        let hit = extend_clock(clock, tb, &sibs);
        let desc = format!(
            "tb=[{}] gap(r/t/b)={}/{}/{} hit=[{}]",
            fmt(&tb),
            tb.right() - clock.right(),
            clock.y - tb.y,
            tb.bottom() - clock.bottom(),
            fmt(&hit)
        );
        let key = hwnd.0 as isize;
        (Some(ClockExt { class, hwnd: key, anchor: clock, hit }), desc)
    }

    fn publish(&self) {
        let h = self.cache.hook_rects(&self.ext, BAND_X);
        if let Ok(mut g) = RECTS.write() {
            *g = h;
        }
        LAST_REFRESH_MS.store(now_ms().max(1), Relaxed);
    }

    /// Log every SystemTray.* button of one taskbar (class, automation id, rect).
    fn dump(&self, why: &str, class: &str, hwnd: isize) {
        match self.scanner.dump_tray(HWND(hwnd as *mut c_void)) {
            Ok(v) => {
                let items: Vec<String> = v
                    .iter()
                    .map(|b| {
                        format!(
                            "{}|{}|{}..{} w={}",
                            b.class_name,
                            b.automation_id,
                            b.rect.left,
                            b.rect.right,
                            b.rect.right - b.rect.left
                        )
                    })
                    .collect();
                log!("[tray] {why} taskbar={class} n={} {}", v.len(), items.join(" ; "));
            }
            Err(e) => log!("[tray] {why} taskbar={class} dump failed: {e}"),
        }
    }

    fn moved(&mut self, class: &'static str, hwnd: isize, from: &Rect, to: &Rect, via: &str) {
        let n = if via == "probe" { &MOVES_PROBE } else { &MOVES_SCAN };
        n.fetch_add(1, Relaxed);
        log!(
            "[refresh] clock moved taskbar={class} from=[{}] to=[{}] dx={} dw={} via={via}",
            fmt(from),
            fmt(to),
            to.x - from.x,
            to.w - from.w
        );
        if let Some(dropped) = self.dump_rl.allow(now_ms()) {
            if dropped > 0 {
                log!("[tray] ({dropped} dumps suppressed)");
            }
            self.dump("after-move", class, hwnd);
        }
    }

    /// Cheap re-read of the cached clock elements. A failure falls back to a full scan.
    fn probe(&mut self, why: &str) {
        if self.elems.is_empty() || !ENABLED.load(Relaxed) {
            return;
        }
        let t = Instant::now();
        self.last_probe = t;
        let elems = self.elems.clone();
        let mut need_scan = false;
        let mut changed = false;
        let mut moves = Vec::new();
        for (class, hwnd, e) in &elems {
            match self.scanner.probe(e) {
                Ok(b) => match self.cache.apply(class, *hwnd, btn_result(&b)) {
                    Applied::Stored { moved_from: Some(from) } => {
                        changed = true;
                        moves.push((*class, *hwnd, from, btn_rect(&b)));
                    }
                    Applied::Invalid { dropped: Some(d) } => {
                        changed = true;
                        EXPIRED.fetch_add(1, Relaxed);
                        log!(
                            "[refresh] expire taskbar={class} reason=invalid-rect via=probe({why}) dropped={}",
                            fmt(&d.rect)
                        );
                    }
                    _ => {}
                },
                Err(err) => {
                    PROBE_FAILS.fetch_add(1, Relaxed);
                    if let Some(dropped) = self.probe_err_rl.allow(now_ms()) {
                        log!("[probe] taskbar={class} failed: {err} -> full scan (suppressed={dropped})");
                    }
                    need_scan = true;
                }
            }
        }
        if changed {
            self.publish();
        }
        let us = t.elapsed().as_micros() as u64;
        PROBES.fetch_add(1, Relaxed);
        PROBE_MAX_US.fetch_max(us, Relaxed);
        let moved = !moves.is_empty();
        for (class, hwnd, from, to) in moves {
            self.moved(class, hwnd, &from, &to, "probe");
        }
        if (need_scan || moved) && self.last_full.elapsed() >= PROBE_FAIL_SCAN_GAP {
            // A moved clock published its plain rect (the extension's siblings are
            // stale); a full scan recomputes the extension for the new layout.
            self.refresh(if need_scan { "probe-fail" } else { "probe-move" });
        }
    }

    fn refresh(&mut self, reason: &str) {
        let t = Instant::now();
        self.last_full = t;
        // Only these keep the cache; config/topology changes start from scratch.
        let incremental = matches!(reason, "timer" | "probe-fail" | "probe-move");
        if !incremental {
            let n = self.cache.reset();
            if n > 0 {
                log!("[refresh] reason={reason} full reset: dropping {n} cached rect(s)");
            }
        }
        let bars = taskbars();
        let gone = self.cache.retain_alive(|e| {
            let alive = unsafe { IsWindow(HWND(e.hwnd as *mut c_void)).as_bool() };
            alive && bars.iter().any(|(cl, h)| *cl == e.class && h.0 as isize == e.hwnd)
        });
        for e in gone {
            EXPIRED.fetch_add(1, Relaxed);
            log!(
                "[refresh] expire taskbar={} hwnd={:#x} reason=taskbar-window-gone dropped={}",
                e.class,
                e.hwnd,
                fmt(&e.rect)
            );
        }

        let mut elems = Vec::new();
        let mut moves = Vec::new();
        let mut parts = Vec::new();
        let mut exts = Vec::new();
        for (class, hwnd) in bars {
            let key = hwnd.0 as isize;
            let mut s = self.scanner.scan_one(hwnd);
            if s.clock.is_none() {
                for attempt in 1..=MISS_RETRIES {
                    thread::sleep(Duration::from_millis(MISS_RETRY_GAP_MS));
                    s = self.scanner.scan_one(hwnd);
                    if s.clock.is_some() {
                        RETRY_RECOVERED.fetch_add(1, Relaxed);
                        log!("[refresh] retry ok taskbar={class} attempt={attempt}");
                        break;
                    }
                }
            }
            if let (Some((_, rule)), Some(e)) = (s.clock, s.clock_elem.as_ref()) {
                if rule.starts_with("rule1") {
                    elems.push((class, key, e.clone()));
                }
            }
            let (res, desc) = match s.clock {
                Some((bi, rule)) => {
                    let b = &s.buttons[bi];
                    let r = btn_rect(b);
                    let desc = if b.valid {
                        format!("{class} [{}] {} {rule}", b.monitor, fmt(&r))
                    } else {
                        format!(
                            "{class} [{}] INVALID (offscreen={}) {} -> pass-through",
                            b.monitor,
                            b.offscreen,
                            fmt(&r)
                        )
                    };
                    (btn_result(b), desc)
                }
                None => {
                    let why = match &s.error {
                        Some(e) => format!("ERROR {e}"),
                        None => format!("no clock found ({} omnibuttons)", s.buttons.len()),
                    };
                    (ScanResult::Miss, format!("{class}: {why}"))
                }
            };
            let new_rect = match &res {
                ScanResult::Found { rect, .. } => Some(*rect),
                _ => None,
            };
            let desc = match new_rect {
                Some(r) => {
                    let (x, d) = self.compute_ext(class, hwnd, r);
                    exts.extend(x);
                    format!("{desc} {d}")
                }
                None => desc,
            };
            match self.cache.apply(class, key, res) {
                Applied::Stored { moved_from } => {
                    if let (Some(from), Some(to)) = (moved_from, new_rect) {
                        moves.push((class, key, from, to));
                    }
                    parts.push(desc)
                }
                Applied::MissNoEntry => parts.push(desc),
                Applied::Invalid { dropped } => {
                    if let Some(e) = dropped {
                        EXPIRED.fetch_add(1, Relaxed);
                        log!("[refresh] expire taskbar={class} hwnd={key:#x} reason=invalid-rect dropped={}", fmt(&e.rect));
                    }
                    parts.push(desc);
                }
                Applied::Kept { rect, monitor, misses } => {
                    MISSES.fetch_add(1, Relaxed);
                    log!(
                        "[refresh] miss taskbar={class} consecutive={misses} kept={} ({desc}, after {MISS_RETRIES} retries)",
                        fmt(&rect)
                    );
                    parts.push(format!("{class} [{monitor}] {} kept(miss)", fmt(&rect)));
                }
                Applied::Expired { dropped } => {
                    MISSES.fetch_add(1, Relaxed);
                    EXPIRED.fetch_add(1, Relaxed);
                    log!(
                        "[refresh] expire taskbar={class} hwnd={key:#x} reason=consecutive-misses={} dropped={}",
                        dropped.misses,
                        fmt(&dropped.rect)
                    );
                    parts.push(desc);
                }
            }
        }
        self.elems = elems;
        self.ext = exts;
        self.publish();
        let n = self.cache.entries.len();
        let summary = format!("rects={n} | {}", parts.join(" | "));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if !incremental || summary != self.last {
            log!("[refresh] reason={reason} took={ms:.1}ms {summary}");
        }
        self.last = summary;
        for (class, hwnd, from, to) in moves {
            self.moved(class, hwnd, &from, &to, "scan");
        }
    }
}

fn uia_thread(miss_expire: u32, ready: std::sync::mpsc::SyncSender<Result<u32, String>>) {
    unsafe {
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = ready.send(Err(format!("CoInitializeEx: {e}")));
            return;
        }
        let res = (|| -> Result<(), String> {
            let scanner = Scanner::new().map_err(|e| format!("UIA init: {e}"))?;
            TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), SeqCst);
            let hinst: HINSTANCE = GetModuleHandleW(None).map_err(|e| e.to_string())?.into();
            let wc = WNDCLASSW {
                lpfnWndProc: Some(hidden_wndproc),
                hInstance: hinst,
                lpszClassName: w!("ClockcalHookHidden"),
                ..Default::default()
            };
            RegisterClassW(&wc);
            // Top-level (not HWND_MESSAGE): message-only windows do not get broadcasts.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("ClockcalHookHidden"),
                w!("clockcal-hook"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                hinst,
                None,
            )
            .map_err(|e| format!("CreateWindowExW: {e}"))?;
            SetTimer(hwnd, TIMER_SCAN, REFRESH_MS, None);
            SetTimer(hwnd, TIMER_PROBE, PROBE_NEAR_MS, None);
            let now = Instant::now();
            let mut u = Uia {
                scanner,
                cache: RectCache::new(miss_expire),
                last: String::new(),
                elems: Vec::new(),
                last_probe: now,
                last_full: now,
                dump_rl: RateLimit::new(10_000, 4),
                probe_err_rl: RateLimit::new(10_000, 5),
                ext: Vec::new(),
            };
            u.refresh(reason_name(0));
            for (class, hwnd) in taskbars() {
                u.dump("start", class, hwnd.0 as isize);
            }
            let _ = ready.send(Ok(GetCurrentThreadId()));
            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0);
                if r.0 == 0 || r.0 == -1 {
                    break;
                }
                match msg.message {
                    WM_TIMER if msg.wParam.0 == TIMER_SCAN => {
                        if ENABLED.load(Relaxed) {
                            u.refresh("timer");
                        }
                    }
                    WM_TIMER if msg.wParam.0 == TIMER_PROBE => {
                        let due = near_active()
                            || u.last_probe.elapsed() >= Duration::from_millis(PROBE_IDLE_MS);
                        if due {
                            u.probe("timer");
                        }
                    }
                    WM_APP_PROBE => {
                        if u.last_probe.elapsed() >= PROBE_MIN_GAP {
                            u.probe("request");
                        }
                    }
                    WM_APP_REFRESH => u.refresh(reason_name(msg.wParam.0)),
                    _ => {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            let _ = KillTimer(hwnd, TIMER_SCAN);
            let _ = KillTimer(hwnd, TIMER_PROBE);
            let _ = DestroyWindow(hwnd);
            Ok(())
        })();
        if let Err(e) = res {
            log!("[uia] thread error: {e}");
            let _ = ready.send(Err(e));
        }
        CoUninitialize();
    }
}

// ---------------------------------------------------------------- dispatcher + watchdog

fn refresh_age_ms() -> String {
    match LAST_REFRESH_MS.load(Relaxed) {
        0 => "never".into(),
        t => now_ms().saturating_sub(t).to_string(),
    }
}

fn cached_desc() -> String {
    match RECTS.read() {
        Ok(g) => {
            let v: Vec<String> = (0..g.clock.n.min(MAX_RECTS))
                .map(|i| {
                    if g.clock.r[i] == g.anchor[i] {
                        fmt_ltrb(g.clock.r[i])
                    } else {
                        format!("{} hit={}", fmt_ltrb(g.anchor[i]), fmt_ltrb(g.clock.r[i]))
                    }
                })
                .collect();
            format!("[{}]", v.join(" ; "))
        }
        Err(_) => "[?]".into(),
    }
}

fn on_click(app: &AppHandle, wp: WPARAM, lp: LPARAM) {
    let (l, t) = unpack(wp.0 as u64);
    let (r, b) = unpack(lp.0 as u64);
    let anchor = ltrb_to_rect((l, t, r, b));
    log!(
        "[click] swallowed LBUTTONDOWN+UP on clock rect={} refresh_age_ms={} -> toggle",
        fmt(&anchor),
        refresh_age_ms()
    );
    post(UIA_TID.load(Relaxed), WM_APP_PROBE, 0, 0);
    let h = app.clone();
    let res = app.run_on_main_thread(move || {
        let a = crate::toggle_popup(&h, Some(anchor));
        log!("[click] toggle result={a:?}");
    });
    if let Err(e) = res {
        log!("[click] run_on_main_thread failed: {e}");
    }
}

fn on_near_miss(rl: &mut RateLimit, wp: WPARAM, lp: LPARAM) {
    post(UIA_TID.load(Relaxed), WM_APP_PROBE, 0, 0);
    let Some(dropped) = rl.allow(now_ms()) else { return };
    let (x, y) = unpack(wp.0 as u64);
    log!(
        "[near-miss] LBUTTONDOWN passed x={x} y={y} zone=band[{}] cached={} refresh_age_ms={}{}",
        lp.0,
        cached_desc(),
        refresh_age_ms(),
        if dropped > 0 { format!(" ({dropped} earlier near-misses not logged)") } else { String::new() }
    );
}

fn dispatcher_thread(app: AppHandle, ready: std::sync::mpsc::SyncSender<u32>) {
    unsafe {
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        // Thread timer (no window): WM_TIMER lands in this thread's queue.
        let timer = SetTimer(HWND::default(), 0, 200, None);
        let _ = ready.send(GetCurrentThreadId());
        let mut last_cursor = POINT::default();
        let _ = GetCursorPos(&mut last_cursor);
        let mut last_cb_seen = LAST_CB_QPC.load(Relaxed);
        let mut stale_since: Option<Instant> = None;
        let mut last_reinstall = Instant::now() - WATCHDOG_MIN_GAP;
        let mut near_rl = RateLimit::new(10_000, 20);
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 || r.0 == -1 {
                break;
            }
            match msg.message {
                WM_APP_CLICK => on_click(&app, msg.wParam, msg.lParam),
                WM_APP_NEARMISS => on_near_miss(&mut near_rl, msg.wParam, msg.lParam),
                WM_TIMER => {
                    // --hook-timeout deadline.
                    let expired = CTRL
                        .lock()
                        .ok()
                        .and_then(|c| c.deadline)
                        .is_some_and(|d| Instant::now() >= d);
                    if expired {
                        log!("[hook] --hook-timeout reached -> disabling interception (app keeps running)");
                        disable("hook-timeout");
                        let h = app.clone();
                        let _ = app.run_on_main_thread(move || crate::sync_hook_menu(&h));
                    }
                    // Watchdog: cursor moved but no hook callback for WATCHDOG_STALE -> reinstall.
                    let htid = HOOK_TID.load(SeqCst);
                    let mut p = POINT::default();
                    let moved = GetCursorPos(&mut p).is_ok()
                        && (p.x != last_cursor.x || p.y != last_cursor.y);
                    last_cursor = p;
                    let cb = LAST_CB_QPC.load(Relaxed);
                    if htid == 0 || cb != last_cb_seen {
                        last_cb_seen = cb;
                        stale_since = None;
                    } else if moved && stale_since.is_none() {
                        stale_since = Some(Instant::now());
                    }
                    if let Some(t) = stale_since {
                        if t.elapsed() >= WATCHDOG_STALE && last_reinstall.elapsed() >= WATCHDOG_MIN_GAP {
                            log!(
                                "[watchdog] cursor moved but no hook callback for {:?} -> requesting reinstall",
                                t.elapsed()
                            );
                            let _ = PostThreadMessageW(htid, WM_APP_REINSTALL, WPARAM(0), LPARAM(0));
                            last_reinstall = Instant::now();
                            stale_since = None;
                        }
                    }
                }
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        if timer != 0 {
            let _ = KillTimer(HWND::default(), timer);
        }
    }
}

// ---------------------------------------------------------------- lifecycle

/// Start the UIA + dispatcher threads and (if `start_enabled`) the hook. Call once.
pub fn init(app: &AppHandle, timeout_s: u64, miss_expire: u32, start_enabled: bool) {
    if APP.set(app.clone()).is_err() {
        return;
    }
    unsafe {
        let mut f = 0i64;
        let _ = QueryPerformanceFrequency(&mut f);
        let f = f.max(1);
        QPC_FREQ.store(f, SeqCst);
        NEAR_HOLD_TICKS.store(f.saturating_mul(NEAR_HOLD_MS) / 1000, SeqCst);
    }
    if let Ok(mut c) = CTRL.lock() {
        c.timeout = if timeout_s == UNLIMITED { Duration::ZERO } else { Duration::from_secs(clamp_timeout(timeout_s)) };
    }
    // Panic anywhere -> remove the hook first.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ENABLED.store(false, SeqCst);
        unhook_now("panic-hook");
        prev(info);
    }));

    let (dtx, drx) = sync_channel(1);
    let a = app.clone();
    let _ = thread::Builder::new()
        .name("clockcal-dispatch".into())
        .spawn(move || dispatcher_thread(a, dtx));
    match drx.recv_timeout(Duration::from_secs(5)) {
        Ok(tid) => DISPATCH_TID.store(tid, SeqCst),
        Err(_) => log!("[hook] dispatcher thread not ready (clicks cannot toggle)"),
    }

    let (utx, urx) = sync_channel(1);
    let _ = thread::Builder::new()
        .name("clockcal-uia".into())
        .spawn(move || uia_thread(miss_expire, utx));
    match urx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(tid)) => UIA_TID.store(tid, SeqCst),
        Ok(Err(e)) => log!("[hook] UIA thread failed: {e} (rects empty = pass-through)"),
        Err(_) => log!("[hook] UIA thread not ready in 10s (rects empty = pass-through)"),
    }
    if start_enabled && DISPATCH_TID.load(SeqCst) != 0 {
        enable("start");
    }
}

/// Exit path: remove the hook, stop helper threads, log the summary.
pub fn shutdown(why: &str) {
    if APP.get().is_none() {
        return;
    }
    disable(why);
    for tid in [UIA_TID.swap(0, SeqCst), DISPATCH_TID.swap(0, SeqCst)] {
        post(tid, WM_QUIT, 0, 0);
    }
    log!(
        "[summary] callbacks={} swallowed={} near_misses={} band_entries={} max_cb_us={} post_failed={} reinstalls={} misses={} retry_recovered={} rects_expired={} probes={} probe_max_us={} probe_fails={} clock_moves(probe/scan)={}/{} ext_dumps={} ext_dump_max_us={} hook_handle_now={:?}",
        CB_COUNT.load(SeqCst),
        SWALLOWED.load(SeqCst),
        NEAR_MISSES.load(SeqCst),
        BAND_ENTRIES.load(SeqCst),
        MAX_US.load(SeqCst),
        POST_FAILED.load(SeqCst),
        REINSTALLS.load(SeqCst),
        MISSES.load(SeqCst),
        RETRY_RECOVERED.load(SeqCst),
        EXPIRED.load(SeqCst),
        PROBES.load(SeqCst),
        PROBE_MAX_US.load(SeqCst),
        PROBE_FAILS.load(SeqCst),
        MOVES_PROBE.load(SeqCst),
        MOVES_SCAN.load(SeqCst),
        EXT_DUMPS.load(SeqCst),
        EXT_DUMP_MAX_US.load(SeqCst),
        HOOK.load(SeqCst)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip_negative_and_large() {
        for (a, b) in [(0, 0), (-1920, -300), (7562, 2160), (i32::MIN, i32::MAX)] {
            assert_eq!(unpack(pack(a, b)), (a, b));
        }
    }

    #[test]
    fn timeout_clamped() {
        assert_eq!(clamp_timeout(0), 1);
        assert_eq!(clamp_timeout(60), 60);
        assert_eq!(clamp_timeout(601), 600);
        assert_eq!(clamp_timeout(u64::MAX), 600);
    }
}
