//! `hook` subcommand: WH_MOUSE_LL on a dedicated thread, UIA rect refresh thread,
//! logger + watchdog thread. The callback only does a point-in-rect test against a
//! fixed-size cached rect set; it never allocates, never calls UIA, never does I/O.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, Ordering::*};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_CLOSE_EVENT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::logln;
use crate::uia::{fmt_rect, taskbars, Scanner};

// ---------------------------------------------------------------- shared state

const MAX_RECTS: usize = 8;

#[derive(Clone, Copy)]
struct RectSet {
    n: usize,
    r: [(i32, i32, i32, i32); MAX_RECTS], // left, top, right, bottom (physical px)
}
impl RectSet {
    const EMPTY: RectSet = RectSet { n: 0, r: [(0, 0, 0, 0); MAX_RECTS] };
}

static RECTS: RwLock<RectSet> = RwLock::new(RectSet::EMPTY);
static HOOK: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static SWALLOW_MODE: AtomicBool = AtomicBool::new(false);
static IGNORE_INJECTED: AtomicBool = AtomicBool::new(false);
static DOWN_SWALLOWED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

static QPC_FREQ: AtomicI64 = AtomicI64::new(1);
static LAST_CB_QPC: AtomicI64 = AtomicI64::new(0);
static CB_COUNT: AtomicU64 = AtomicU64::new(0);
static MOVE_COUNT: AtomicU64 = AtomicU64::new(0);
static LAST_X: AtomicI32 = AtomicI32::new(0);
static LAST_Y: AtomicI32 = AtomicI32::new(0);
static MAX_US: AtomicU64 = AtomicU64::new(0);
static SUM_US: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static SWALLOWED: AtomicU64 = AtomicU64::new(0);
static REINSTALLS: AtomicU32 = AtomicU32::new(0);
static HOOK_TID: AtomicU32 = AtomicU32::new(0);
static UIA_TID: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
static RETRY_RECOVERED: AtomicU64 = AtomicU64::new(0);
static EXPIRED: AtomicU64 = AtomicU64::new(0);

static TX: OnceLock<SyncSender<Ev>> = OnceLock::new();

const WM_APP_REINSTALL: u32 = WM_APP + 1;
const WM_APP_SIMLOSS: u32 = WM_APP + 2;
const WM_APP_REFRESH: u32 = WM_APP + 3;

#[derive(Clone, Copy)]
struct Ev {
    msg: u32,
    x: i32,
    y: i32,
    inside: bool,
    swallowed: bool,
    injected: bool,
    ignored: bool,
    rects_n: u8,
    dur_us: u64,
}

fn qpc() -> i64 {
    let mut v = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v
}

// ---------------------------------------------------------------- the callback

/// Returns (inside, rects_n). Never blocks: if the writer holds the lock, report "no rect".
#[inline]
fn hit(x: i32, y: i32) -> (bool, u8) {
    match RECTS.try_read() {
        Ok(g) => {
            let s = *g;
            drop(g);
            for &(l, t, r, b) in &s.r[..s.n] {
                if x >= l && x < r && y >= t && y < b {
                    return (true, s.n as u8);
                }
            }
            (false, s.n as u8)
        }
        Err(_) => (false, 0),
    }
}

#[allow(unsafe_op_in_unsafe_fn)] // whole body is FFI glue; kept minimal
unsafe extern "system" fn ll_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode != HC_ACTION as i32 {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }
    let t0 = qpc();
    LAST_CB_QPC.store(t0, Relaxed);
    CB_COUNT.fetch_add(1, Relaxed);
    let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
    let msg = wparam.0 as u32;
    let (x, y) = (info.pt.x, info.pt.y);

    if msg == WM_MOUSEMOVE {
        MOVE_COUNT.fetch_add(1, Relaxed);
        LAST_X.store(x, Relaxed);
        LAST_Y.store(y, Relaxed);
        let d = (qpc() - t0).max(0) as u64 * 1_000_000 / QPC_FREQ.load(Relaxed).max(1) as u64;
        MAX_US.fetch_max(d, Relaxed);
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    let injected = info.flags & LLMHF_INJECTED != 0;
    let ignored = injected && IGNORE_INJECTED.load(Relaxed);
    let (inside, rects_n) = if ignored { (false, 0) } else { hit(x, y) };
    let mut swallow = false;
    if !ignored && SWALLOW_MODE.load(Relaxed) {
        if msg == WM_LBUTTONDOWN && inside {
            DOWN_SWALLOWED.store(true, Relaxed);
            swallow = true;
        } else if msg == WM_LBUTTONUP && DOWN_SWALLOWED.swap(false, Relaxed) {
            // Swallow the UP that pairs a swallowed DOWN wherever it lands,
            // so Explorer never sees a half click.
            swallow = true;
        }
    }
    if swallow {
        SWALLOWED.fetch_add(1, Relaxed);
    }
    let dt = qpc() - t0;
    let dur_us = (dt.max(0) as u64) * 1_000_000 / QPC_FREQ.load(Relaxed).max(1) as u64;
    MAX_US.fetch_max(dur_us, Relaxed);
    SUM_US.fetch_add(dur_us, Relaxed);
    if let Some(tx) = TX.get() {
        let ev = Ev { msg, x, y, inside, swallowed: swallow, injected, ignored, rects_n, dur_us };
        if tx.try_send(ev).is_err() {
            DROPPED.fetch_add(1, Relaxed);
        }
    }
    if swallow {
        LRESULT(1)
    } else {
        CallNextHookEx(None, ncode, wparam, lparam)
    }
}

// ---------------------------------------------------------------- unhook paths

/// Single owner of unhooking: whoever swaps the handle out unhooks it. Callable from any thread.
pub fn unhook_now(who: &str) -> bool {
    let h = HOOK.swap(null_mut(), SeqCst);
    if h.is_null() {
        return false;
    }
    let ok = unsafe { UnhookWindowsHookEx(HHOOK(h)).is_ok() };
    logln!("[hook] UnhookWindowsHookEx by {who}: ok={ok}");
    ok
}

unsafe extern "system" fn ctrl_handler(ctrl: u32) -> BOOL {
    STOP.store(true, SeqCst);
    if ctrl == CTRL_CLOSE_EVENT {
        // Process will be killed right after this returns; unhook here.
        unhook_now("ctrl-close");
    }
    BOOL(1)
}

fn install(hmod: HINSTANCE) -> windows::core::Result<()> {
    let h = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(ll_proc), hmod, 0)? };
    let old = HOOK.swap(h.0, SeqCst);
    if !old.is_null() {
        let ok = unsafe { UnhookWindowsHookEx(HHOOK(old)).is_ok() };
        logln!("[hook] old handle {old:?} unhooked after reinstall: ok={ok} (false is expected if Windows already removed it)");
    }
    logln!("[hook] installed WH_MOUSE_LL handle={:?} tid={}", h.0, unsafe { GetCurrentThreadId() });
    Ok(())
}

// ---------------------------------------------------------------- hook thread

fn hook_thread(ready: SyncSender<Result<u32, String>>) {
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
            match msg.message {
                WM_APP_REINSTALL => {
                    REINSTALLS.fetch_add(1, SeqCst);
                    if let Err(e) = install(hmod) {
                        logln!("[watchdog] reinstall FAILED: {e}");
                    }
                }
                WM_APP_SIMLOSS => {
                    // Simulate Windows silently dropping the hook: unhook but keep the
                    // stale handle value so the watchdog path is the only recovery.
                    let h = HOOK.load(SeqCst);
                    let ok = UnhookWindowsHookEx(HHOOK(h)).is_ok();
                    logln!("[test] simulated hook loss: unhooked {h:?} ok={ok} (handle kept as stale)");
                }
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        unhook_now("hook-thread-exit");
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
        _ => "?",
    }
}

#[allow(unsafe_op_in_unsafe_fn)]
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
        // Post rather than refresh inline: broadcasts may be sent, and UIA calls from
        // inside a sent message are best avoided.
        let _ = PostMessageW(hwnd, WM_APP_REFRESH, WPARAM(c), LPARAM(0));
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// Quick re-scans of one taskbar after a miss (on the UIA thread, never the hook thread).
const MISS_RETRIES: u32 = 3;
const MISS_RETRY_GAP_MS: u64 = 120;

/// Last-known-good clock rect for one taskbar window, keyed by (class, hwnd).
struct Cached {
    class: &'static str,
    hwnd: isize,
    monitor: String,
    rect: RECT,
    misses: u32,
}

/// Owned by the UIA thread only.
struct RectCache {
    entries: Vec<Cached>,
    miss_expire: u32,
    last: String,
}

fn drop_entry(cache: &mut RectCache, idx: usize, why: &str) {
    let c = cache.entries.remove(idx);
    EXPIRED.fetch_add(1, Relaxed);
    logln!(
        "[refresh] expire taskbar={} hwnd={:#x} reason={why} dropped={}",
        c.class,
        c.hwnd,
        fmt_rect(&c.rect)
    );
}

fn refresh(scanner: &Scanner, reason: &str, cache: &mut RectCache) {
    let t = Instant::now();
    let timer = reason == "timer";
    if !timer && !cache.entries.is_empty() {
        // TaskbarCreated / DISPLAYCHANGE / DPICHANGED / SETTINGCHANGE / start: full reset + rescan.
        logln!("[refresh] reason={reason} full reset: dropping {} cached rect(s)", cache.entries.len());
        cache.entries.clear();
    }
    let bars = taskbars();
    // Taskbar window gone (IsWindow false or no longer enumerated) -> drop its rect.
    let mut i = 0;
    while i < cache.entries.len() {
        let c = &cache.entries[i];
        let alive = unsafe { IsWindow(HWND(c.hwnd as *mut c_void)).as_bool() }
            && bars.iter().any(|(cl, h)| *cl == c.class && h.0 as isize == c.hwnd);
        if alive {
            i += 1;
        } else {
            drop_entry(cache, i, "taskbar-window-gone");
        }
    }

    let mut parts = Vec::new();
    for (class, hwnd) in bars {
        let key = hwnd.0 as isize;
        let mut s = scanner.scan_one(class, hwnd);
        if s.clock.is_none() {
            for attempt in 1..=MISS_RETRIES {
                thread::sleep(Duration::from_millis(MISS_RETRY_GAP_MS));
                s = scanner.scan_one(class, hwnd);
                if s.clock.is_some() {
                    RETRY_RECOVERED.fetch_add(1, Relaxed);
                    logln!("[refresh] retry ok taskbar={class} attempt={attempt}");
                    break;
                }
            }
        }
        let idx = cache.entries.iter().position(|c| c.class == class && c.hwnd == key);
        match s.clock {
            Some((bi, rule)) => {
                let b = &s.buttons[bi];
                if b.valid {
                    match idx {
                        Some(k) => {
                            let c = &mut cache.entries[k];
                            c.rect = b.rect;
                            c.monitor = b.monitor.clone();
                            c.misses = 0;
                        }
                        None => cache.entries.push(Cached {
                            class,
                            hwnd: key,
                            monitor: b.monitor.clone(),
                            rect: b.rect,
                            misses: 0,
                        }),
                    }
                    parts.push(format!("{} [{}] {} {rule}", class, b.monitor, fmt_rect(&b.rect)));
                } else {
                    // Explicit invalid rect (empty / off-screen, e.g. auto-hide) -> pass-through.
                    if let Some(k) = idx {
                        drop_entry(cache, k, "invalid-rect");
                    }
                    parts.push(format!(
                        "{} [{}] INVALID (offscreen={}) {} -> pass-through",
                        class,
                        b.monitor,
                        b.offscreen,
                        fmt_rect(&b.rect)
                    ));
                }
            }
            None => {
                let why = match &s.error {
                    Some(e) => format!("ERROR {e}"),
                    None => format!("no clock found ({} omnibuttons)", s.buttons.len()),
                };
                match idx {
                    Some(k) => {
                        MISSES.fetch_add(1, Relaxed);
                        cache.entries[k].misses += 1;
                        let n = cache.entries[k].misses;
                        if n >= cache.miss_expire {
                            drop_entry(cache, k, &format!("consecutive-misses={n}"));
                            parts.push(format!("{class}: {why}"));
                        } else {
                            let c = &cache.entries[k];
                            logln!(
                                "[refresh] miss taskbar={class} consecutive={n} kept={} ({why}, after {MISS_RETRIES} retries)",
                                fmt_rect(&c.rect)
                            );
                            parts.push(format!("{class} [{}] {} kept(miss)", c.monitor, fmt_rect(&c.rect)));
                        }
                    }
                    None => parts.push(format!("{class}: {why}")),
                }
            }
        }
    }

    let mut set = RectSet::EMPTY;
    for c in cache.entries.iter().take(MAX_RECTS) {
        set.r[set.n] = (c.rect.left, c.rect.top, c.rect.right, c.rect.bottom);
        set.n += 1;
    }
    if let Ok(mut g) = RECTS.write() {
        *g = set;
    }
    let summary = format!("rects={} | {}", set.n, parts.join(" | "));
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    if !timer || summary != cache.last {
        logln!("[refresh] reason={reason} took={ms:.1}ms {summary}");
    } else {
        logln!("[refresh] reason=timer took={ms:.1}ms unchanged rects={}", set.n);
    }
    cache.last = summary;
}

fn uia_thread(refresh_ms: u32, miss_expire: u32, ready: SyncSender<Result<u32, String>>) {
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
                lpszClassName: w!("ClockcalPocHidden"),
                ..Default::default()
            };
            RegisterClassW(&wc);
            // Top-level (not HWND_MESSAGE): message-only windows do not get broadcasts.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("ClockcalPocHidden"),
                w!("clockcal-poc"),
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
            SetTimer(hwnd, 1, refresh_ms, None);
            let mut cache = RectCache { entries: Vec::new(), miss_expire, last: String::new() };
            refresh(&scanner, reason_name(0), &mut cache);
            let _ = ready.send(Ok(GetCurrentThreadId()));
            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0);
                if r.0 == 0 || r.0 == -1 {
                    break;
                }
                match msg.message {
                    WM_TIMER => refresh(&scanner, "timer", &mut cache),
                    WM_APP_REFRESH => refresh(&scanner, reason_name(msg.wParam.0), &mut cache),
                    _ => {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
            let _ = KillTimer(hwnd, 1);
            let _ = DestroyWindow(hwnd);
            Ok(())
        })();
        if let Err(e) = res {
            logln!("[uia] thread error: {e}");
            let _ = ready.send(Err(e));
        }
        CoUninitialize();
    }
}

// ---------------------------------------------------------------- logger + watchdog thread

fn msg_name(m: u32) -> &'static str {
    match m {
        WM_LBUTTONDOWN => "LBUTTONDOWN",
        WM_LBUTTONUP => "LBUTTONUP",
        WM_RBUTTONDOWN => "RBUTTONDOWN",
        WM_RBUTTONUP => "RBUTTONUP",
        WM_MBUTTONDOWN => "MBUTTONDOWN",
        WM_MBUTTONUP => "MBUTTONUP",
        WM_MOUSEWHEEL => "MOUSEWHEEL",
        WM_MOUSEHWHEEL => "MOUSEHWHEEL",
        WM_XBUTTONDOWN => "XBUTTONDOWN",
        WM_XBUTTONUP => "XBUTTONUP",
        _ => "OTHER",
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn logger_thread(rx: Receiver<Ev>, heartbeat: Duration) {
    let mut last_moves = 0u64;
    let mut last_summary = Instant::now();
    let mut last_cb_seen = LAST_CB_QPC.load(Relaxed);
    let mut last_cursor = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut last_cursor);
    }
    let mut stale_since: Option<Instant> = None;
    let mut last_reinstall = Instant::now() - Duration::from_secs(60);
    let mut last_check = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(e) => logln!(
                "[event] {} x={} y={} inside_clock={} swallowed={} injected={}{} rects={} cb_us={} max_us={}",
                msg_name(e.msg),
                e.x,
                e.y,
                yn(e.inside),
                yn(e.swallowed),
                yn(e.injected),
                if e.ignored { " (ignored)" } else { "" },
                e.rects_n,
                e.dur_us,
                MAX_US.load(Relaxed)
            ),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if STOP.load(Relaxed) && HOOK_TID.load(Relaxed) == 0 {
            // drain whatever is left, then exit
            while let Ok(e) = rx.try_recv() {
                logln!("[event] {} x={} y={} (late)", msg_name(e.msg), e.x, e.y);
            }
            break;
        }
        if last_summary.elapsed() >= Duration::from_secs(1) {
            let m = MOVE_COUNT.load(Relaxed);
            if m != last_moves {
                logln!(
                    "[moves] +{} in last 1s, last x={} y={} (callbacks total={}, max_us={})",
                    m - last_moves,
                    LAST_X.load(Relaxed),
                    LAST_Y.load(Relaxed),
                    CB_COUNT.load(Relaxed),
                    MAX_US.load(Relaxed)
                );
                last_moves = m;
            }
            last_summary = Instant::now();
        }
        // Watchdog: cursor keeps moving but no hook callback for `heartbeat` -> reinstall.
        if last_check.elapsed() >= Duration::from_millis(200) {
            last_check = Instant::now();
            let mut p = POINT::default();
            let moved = unsafe { GetCursorPos(&mut p).is_ok() } && (p.x != last_cursor.x || p.y != last_cursor.y);
            last_cursor = p;
            let cb = LAST_CB_QPC.load(Relaxed);
            if cb != last_cb_seen {
                last_cb_seen = cb;
                stale_since = None;
            } else if moved && stale_since.is_none() {
                stale_since = Some(Instant::now());
            }
            if let Some(t) = stale_since
                && t.elapsed() >= heartbeat
                && last_reinstall.elapsed() >= Duration::from_secs(3)
            {
                logln!(
                    "[watchdog] cursor moved but no hook callback for {:?} -> requesting reinstall",
                    t.elapsed()
                );
                let tid = HOOK_TID.load(SeqCst);
                if tid != 0 {
                    unsafe {
                        let _ = PostThreadMessageW(tid, WM_APP_REINSTALL, WPARAM(0), LPARAM(0));
                    }
                }
                last_reinstall = Instant::now();
                stale_since = None;
            }
        }
    }
}

// ---------------------------------------------------------------- entry

pub struct Opts {
    pub swallow: bool,
    pub timeout_s: u64,
    pub refresh_ms: u32,
    pub ignore_injected: bool,
    pub simulate_loss_s: Option<u64>,
    /// Consecutive timer misses after which a kept (last-known-good) rect is dropped.
    pub miss_expire: u32,
}

pub fn run(o: Opts) -> i32 {
    unsafe {
        let mut f = 0i64;
        let _ = QueryPerformanceFrequency(&mut f);
        QPC_FREQ.store(f.max(1), SeqCst);
    }
    SWALLOW_MODE.store(o.swallow, SeqCst);
    IGNORE_INJECTED.store(o.ignore_injected, SeqCst);

    // Panic anywhere -> unhook first, then exit.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        unhook_now("panic-hook");
        default_hook(info);
        std::process::exit(101);
    }));
    unsafe {
        if let Err(e) = SetConsoleCtrlHandler(Some(ctrl_handler), true) {
            logln!("[main] SetConsoleCtrlHandler failed: {e}");
        }
    }

    logln!(
        "[main] mode={} timeout={}s refresh={}ms ignore_injected={} miss_expire={} pid={}",
        if o.swallow { "SWALLOW" } else { "LOG-ONLY" },
        o.timeout_s,
        o.refresh_ms,
        o.ignore_injected,
        o.miss_expire,
        std::process::id()
    );

    // Hard backstop: whatever happens, the process is gone 5 s after the timeout.
    let backstop = o.timeout_s + 5;
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(backstop));
        unhook_now("backstop");
        logln!("[main] BACKSTOP exit after {backstop}s");
        std::process::exit(3);
    });

    let (tx, rx) = sync_channel::<Ev>(1024);
    let _ = TX.set(tx);
    let logger = thread::spawn(move || logger_thread(rx, Duration::from_millis(1500)));

    // UIA first so the rect cache is filled before the hook goes live.
    let (utx, urx) = sync_channel(1);
    let refresh_ms = o.refresh_ms;
    let miss_expire = o.miss_expire;
    let uia = thread::spawn(move || uia_thread(refresh_ms, miss_expire, utx));
    match urx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(tid)) => UIA_TID.store(tid, SeqCst),
        Ok(Err(e)) => logln!("[main] UIA thread failed: {e} (hook runs with empty rects = pass-through)"),
        Err(_) => logln!("[main] UIA thread not ready in 10s (rects empty = pass-through)"),
    }

    let (htx, hrx) = sync_channel(1);
    let hook = thread::spawn(move || hook_thread(htx));
    let hook_ok = match hrx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(tid)) => {
            HOOK_TID.store(tid, SeqCst);
            true
        }
        Ok(Err(e)) => {
            logln!("[main] hook install failed: {e}");
            false
        }
        Err(_) => {
            logln!("[main] hook thread not ready");
            false
        }
    };

    let start = Instant::now();
    let deadline = Duration::from_secs(o.timeout_s);
    let mut sim_done = false;
    while hook_ok && !STOP.load(SeqCst) && start.elapsed() < deadline {
        if let Some(s) = o.simulate_loss_s
            && !sim_done
            && start.elapsed() >= Duration::from_secs(s)
        {
            sim_done = true;
            unsafe {
                let _ = PostThreadMessageW(HOOK_TID.load(SeqCst), WM_APP_SIMLOSS, WPARAM(0), LPARAM(0));
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    let why = if STOP.load(SeqCst) {
        "ctrl-c"
    } else if hook_ok {
        "timeout"
    } else {
        "hook-failed"
    };
    logln!("[main] stopping ({why}) after {:.1}s", start.elapsed().as_secs_f64());
    STOP.store(true, SeqCst);

    let htid = HOOK_TID.load(SeqCst);
    if htid != 0 {
        unsafe {
            let _ = PostThreadMessageW(htid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    let _ = hook.join();
    // In case the hook thread could not unhook for some reason.
    unhook_now("main-final");
    HOOK_TID.store(0, SeqCst);

    let utid = UIA_TID.load(SeqCst);
    if utid != 0 {
        unsafe {
            let _ = PostThreadMessageW(utid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
    let _ = uia.join();
    let _ = logger.join();

    let n_btn = CB_COUNT.load(SeqCst) - MOVE_COUNT.load(SeqCst);
    let avg = if n_btn > 0 { SUM_US.load(SeqCst) as f64 / n_btn as f64 } else { 0.0 };
    logln!(
        "[summary] callbacks={} moves={} non-move={} swallowed={} dropped_log_events={} reinstalls={} max_cb_us={} avg_non_move_cb_us={:.1} hook_handle_now={:?}",
        CB_COUNT.load(SeqCst),
        MOVE_COUNT.load(SeqCst),
        n_btn,
        SWALLOWED.load(SeqCst),
        DROPPED.load(SeqCst),
        REINSTALLS.load(SeqCst),
        MAX_US.load(SeqCst),
        avg,
        HOOK.load(SeqCst)
    );
    logln!(
        "[summary] rect_misses_kept_or_expired={} retry_recovered={} rects_expired={}",
        MISSES.load(SeqCst),
        RETRY_RECOVERED.load(SeqCst),
        EXPIRED.load(SeqCst)
    );
    if hook_ok {
        logln!("[main] clean exit, hook removed");
        0
    } else {
        2
    }
}
