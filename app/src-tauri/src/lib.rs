//! clockcal P2: Tauri skeleton (popup window, tray, hotkey, single instance) +
//! P2-b opt-in taskbar-clock click interception (`--hook`, see clockhook.rs).
//! P3-a: Google OAuth (oauth.rs, token_store.rs), CLI `--auth-*` + auth_* commands.

pub mod applog;
pub mod autostart;
pub mod calendar;
pub mod clock_logic;
pub mod clockhook;
pub mod monitor;
pub mod notifier;
pub mod notify_runner;
pub mod oauth;
pub mod openurl;
pub mod popup_logic;
pub mod settings;
pub mod token_store;
pub mod uia;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use popup_logic::{decide_toggle, place_popup, Rect, ToggleAction, BLUR_DEBOUNCE_MS};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Manager, PhysicalPosition, PhysicalSize, RunEvent, WebviewWindow, WindowEvent, Wry,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

pub const POPUP_LABEL: &str = "popup";
/// Logical popup size (scaled by the target monitor's DPI).
pub const POPUP_LOGICAL: (f64, f64) = (360.0, 640.0);
/// Logical gap between popup and taskbar/clock.
pub const POPUP_MARGIN_LOGICAL: f64 = 12.0;

/// Open the native notification centre (Win+N) above our popup. Experimental, off by default (OS notification centre always stays above our popup); enable with `--notif`.
static NOTIF_CENTER: AtomicBool = AtomicBool::new(false);
/// Focus-loss hide is ignored until this time (ms, applog clock) while the notification centre opens.
static NOTIF_BLUR_UNTIL: AtomicU64 = AtomicU64::new(0);

/// Send Win+N (toggles the Windows 11 notification centre).
fn send_win_n() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP, VK_LWIN};
    unsafe {
        keybd_event(VK_LWIN as u8, 0, 0, 0);
        keybd_event(b'N', 0, 0, 0);
        keybd_event(b'N', 0, KEYEVENTF_KEYUP, 0);
        keybd_event(VK_LWIN as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}

#[derive(Default)]
struct PopupState {
    last_blur_hide_ms: Option<u64>,
}

/// Tray item that toggles click interception (only present with `--hook`).
#[derive(Default)]
struct HookMenu(Mutex<Option<MenuItem<Wry>>>);

const HOOK_MENU_ON: &str = "클릭 가로채기 끄기";
const HOOK_MENU_OFF: &str = "클릭 가로채기 켜기";

#[derive(Debug)]
struct CliArgs {
    anchor: Option<Rect>,
    exit_after: Option<u64>,
    /// P2-b: opt-in WH_MOUSE_LL clock interception.
    hook: bool,
    /// Seconds after each enable before the hook is removed (1..=600).
    hook_timeout: u64,
    miss_expire: u32,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            anchor: None,
            exit_after: None,
            hook: false,
            hook_timeout: clockhook::DEFAULT_TIMEOUT_S,
            miss_expire: clockhook::DEFAULT_MISS_EXPIRE,
        }
    }
}

fn parse_args() -> CliArgs {
    let mut out = CliArgs::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--anchor" => {
                let v = it.next().unwrap_or_default();
                out.anchor = Rect::parse(&v);
                if out.anchor.is_none() {
                    log!("args: invalid --anchor '{v}' (expected x,y,w,h physical px)");
                }
            }
            "--exit-after" => {
                let v = it.next().unwrap_or_default();
                out.exit_after = v.parse().ok();
                if out.exit_after.is_none() {
                    log!("args: invalid --exit-after '{v}'");
                }
            }
            "--hook" => out.hook = true,
            "--notif" => NOTIF_CENTER.store(true, Ordering::Relaxed),
            "--hook-timeout" => {
                let v = it.next().unwrap_or_default();
                match v.parse::<u64>() {
                    Ok(n) => out.hook_timeout = clockhook::clamp_timeout(n),
                    Err(_) => log!(
                        "args: invalid --hook-timeout '{v}', using {}s",
                        clockhook::DEFAULT_TIMEOUT_S
                    ),
                }
            }
            "--miss-expire" => {
                let v = it.next().unwrap_or_default();
                match v.parse::<u32>() {
                    Ok(n) if n > 0 => out.miss_expire = n,
                    _ => log!("args: invalid --miss-expire '{v}'"),
                }
            }
            _ => {}
        }
    }
    out
}

/// P4-b: true once the acrylic backdrop is on (the page then makes its backgrounds translucent).
static ACRYLIC_ON: AtomicBool = AtomicBool::new(false);

/// (Re)apply the acrylic backdrop with a tint that matches the app theme. Falls back to opaque on failure.
fn apply_acrylic_effect(win: &WebviewWindow) {
    let dark = !matches!(win.theme(), Ok(tauri::Theme::Light));
    let tint = if dark { (32, 32, 32, 140) } else { (243, 243, 243, 140) };
    match window_vibrancy::apply_acrylic(win, Some(tint)) {
        Ok(()) => {
            ACRYLIC_ON.store(true, Ordering::Relaxed);
            log!("acrylic: applied dark={dark}");
        }
        Err(e) => {
            ACRYLIC_ON.store(false, Ordering::Relaxed);
            log!("acrylic: failed ({e}); staying opaque");
        }
    }
    let on = ACRYLIC_ON.load(Ordering::Relaxed);
    let _ = win.eval(format!("window.__clockcalAcrylic && window.__clockcalAcrylic({on})"));
}

/// P6: settings as stored, with `autostart` reflecting the real registry state.
#[tauri::command]
fn get_settings() -> settings::Settings {
    let mut s = settings::load();
    s.autostart = autostart::is_enabled();
    s.clock_hook = clockhook::is_enabled();
    s
}

/// P6: validate (clamp), apply autostart, persist. Returns what was actually stored.
#[tauri::command]
async fn set_settings(app: AppHandle, new: settings::Settings) -> Result<settings::Settings, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let s = new.sanitized();
        autostart::set_enabled(s.autostart).map_err(|e| format!("autostart: {e}"))?;
        settings::save(&s).map_err(|e| e.to_string())?;
        let hook_ok = if s.clock_hook {
            clockhook::enable("settings")
        } else {
            clockhook::disable("settings");
            true
        };
        let h = app.clone();
        let _ = app.run_on_main_thread(move || sync_hook_menu(&h));
        if !hook_ok {
            log!("settings: saved but click interception could not start");
            return Err("hook: could not start".to_string());
        }
        log!("settings: saved clock_hook={} notify={} lead={}m autostart={}", s.clock_hook, s.notify_enabled, s.notify_lead_min, s.autostart);
        Ok(s)
    })
    .await
    .map_err(|_| "settings task failed".to_string())?
}

/// First launch of an installed build (no settings.json yet): start with Windows by default.
/// Dev builds under `target\debug|release` never register themselves.
fn first_run_autostart() {
    let Some(p) = settings::path() else { return };
    let exe = std::env::current_exe().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    if autostart::is_dev_exe(&exe) {
        return;
    }
    if p.exists() {
        // Reinstall: uninstall removed the Run value but settings.json kept the user's choice.
        if settings::load().autostart && !autostart::is_enabled() {
            match autostart::set_enabled(true) {
                Ok(()) => log!("settings: autostart restored after reinstall"),
                Err(e) => log!("settings: autostart restore failed: {e}"),
            }
        }
        return;
    }
    let s = settings::Settings { autostart: true, ..settings::Settings::default() };
    match autostart::set_enabled(true).and_then(|_| settings::save(&s)) {
        Ok(()) => log!("settings: first run -> autostart enabled"),
        Err(e) => log!("settings: first run autostart failed: {e}"),
    }
}

#[tauri::command]
fn ui_effects() -> bool {
    ACRYLIC_ON.load(Ordering::Relaxed)
}

fn popup_window(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(POPUP_LABEL)
}

/// Position (physical px) and show the popup. `anchor` = clock rect in physical px.
fn show_popup(app: &AppHandle, anchor: Option<Rect>) {
    let Some(win) = popup_window(app) else {
        log!("show: popup window missing");
        return;
    };
    // Invalid anchor (e.g. auto-hide taskbar, zero size) -> treat as no anchor.
    let anchor = anchor.filter(|r| r.is_valid());
    let mon = match anchor {
        Some(a) => {
            let (cx, cy) = a.center();
            monitor::monitor_from_point(cx, cy)
        }
        None => monitor::primary_monitor(),
    };
    let Some(mon) = mon else {
        log!("show: monitor query failed; showing at current position");
        let _ = win.show();
        let _ = win.set_focus();
        return;
    };
    let scale = mon.scale();
    let size = (
        (POPUP_LOGICAL.0 * scale).round() as i32,
        (POPUP_LOGICAL.1 * scale).round() as i32,
    );
    let margin = (POPUP_MARGIN_LOGICAL * scale).round() as i32;
    let (x, y) = place_popup(anchor, &mon.monitor, &mon.work, size, margin);

    // Position first (may trigger WM_DPICHANGED resize when crossing monitors),
    // then force the exact physical size, then re-apply position.
    let _ = win.set_position(PhysicalPosition::new(x, y));
    let _ = win.set_size(PhysicalSize::new(size.0 as u32, size.1 as u32));
    let _ = win.set_position(PhysicalPosition::new(x, y));
    let _ = win.show();
    let _ = win.set_always_on_top(true);
    let _ = win.set_focus();
    // Tell the page it was (re)shown so it resets to today (visibilitychange/blur are unreliable in WebView2).
    let _ = win.eval("window.__clockcalShown && window.__clockcalShown()");
    if NOTIF_CENTER.load(Ordering::Relaxed) {
        // The notification centre takes focus when it opens; keep our popup on top of it afterwards.
        NOTIF_BLUR_UNTIL.store(applog::now_ms() + 1500, Ordering::Relaxed);
        let w = win.clone();
        std::thread::spawn(move || {
            send_win_n();
            std::thread::sleep(Duration::from_millis(450));
            let _ = w.set_always_on_top(true);
            let _ = w.set_focus();
            log!("notif: Win+N sent, popup re-raised");
        });
    }

    let target = Rect::new(x, y, size.0, size.1);
    log!(
        "show: anchor={:?} monitor={:?} work={:?} dpi={} scale={:.2} primary={} target={:?} inside_work={} above_anchor={:?}",
        anchor,
        mon.monitor,
        mon.work,
        mon.dpi,
        scale,
        mon.primary,
        target,
        mon.work.contains_rect(&target),
        anchor.map(|a| target.bottom() <= a.y)
    );
}

fn log_actual_rect(app: &AppHandle, tag: &str) {
    if let Some(win) = popup_window(app) {
        let pos = win.outer_position().ok();
        let size = win.outer_size().ok();
        let vis = win.is_visible().ok();
        let sf = win.scale_factor().ok();
        let ipos = win.inner_position().ok();
        let isize = win.inner_size().ok();
        match (pos, size) {
            (Some(p), Some(s)) => {
                let r = Rect::new(p.x, p.y, s.width as i32, s.height as i32);
                let inner = ipos
                    .zip(isize)
                    .map(|(p, s)| Rect::new(p.x, p.y, s.width as i32, s.height as i32));
                log!(
                    "{tag}: actual popup outer={r:?} inner={inner:?} visible={vis:?} window_scale={sf:?}"
                );
            }
            _ => log!("{tag}: actual rect unavailable"),
        }
    }
}

fn hide_popup(app: &AppHandle, reason: &str) {
    if let Some(win) = popup_window(app) {
        let _ = win.hide();
        log!("hide: reason={reason}");
    }
}

/// Toggle the popup. `anchor` = physical-px clock rect (hook path) or None (tray/hotkey).
/// Returns the decision (None = popup window missing).
pub fn toggle_popup(app: &AppHandle, anchor: Option<Rect>) -> Option<ToggleAction> {
    let win = popup_window(app)?;
    let visible = win.is_visible().unwrap_or(false);
    let last = app
        .state::<Mutex<PopupState>>()
        .lock()
        .map(|s| s.last_blur_hide_ms)
        .unwrap_or(None);
    let now = applog::now_ms();
    let action = decide_toggle(visible, last, now, BLUR_DEBOUNCE_MS);
    log!("toggle: visible={visible} last_blur_hide_ms={last:?} now_ms={now} -> {action:?}");
    match action {
        ToggleAction::Show => show_popup(app, anchor),
        ToggleAction::Hide => hide_popup(app, "toggle"),
        ToggleAction::Ignore => {}
    }
    Some(action)
}

/// Sync the tray item text with the hook state. Main thread.
pub fn sync_hook_menu(app: &AppHandle) {
    let on = clockhook::is_enabled();
    if let Ok(g) = app.state::<HookMenu>().0.lock() {
        if let Some(item) = g.as_ref() {
            let _ = item.set_text(if on { HOOK_MENU_ON } else { HOOK_MENU_OFF });
        }
    }
}

fn toggle_hook(app: &AppHandle) {
    if clockhook::is_enabled() {
        log!("tray: click interception off");
        clockhook::disable("tray");
    } else {
        log!("tray: click interception on");
        clockhook::enable("tray");
    }
    // Keep the settings sheet / next start in line with the tray choice.
    let mut s = settings::load();
    s.clock_hook = clockhook::is_enabled();
    if let Err(e) = settings::save(&s) {
        log!("tray: could not persist clock_hook: {e}");
    }
    sync_hook_menu(app);
}

// ---------------------------------------------------------------- P3-a auth commands

/// Run blocking auth work off the main thread; errors become non-secret strings.
async fn auth_blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, oauth::AuthError> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|_| "auth task failed".to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn auth_status(profile: String) -> Result<oauth::AuthStatus, String> {
    auth_blocking(move || oauth::status(&profile)).await
}

#[tauri::command]
async fn auth_login(profile: String) -> Result<oauth::LoginInfo, String> {
    auth_blocking(move || oauth::login(&profile)).await
}

#[tauri::command]
async fn auth_import_client(profile: String, json: String) -> Result<(), String> {
    auth_blocking(move || oauth::import_client(&profile, &json)).await
}

#[tauri::command]
async fn auth_logout(profile: String) -> Result<bool, String> {
    auth_blocking(move || oauth::logout(&profile, true)).await
}

// ---------------------------------------------------------------- P3-b calendar commands

#[tauri::command]
async fn calendar_month(
    profile: String,
    year: i32,
    month: u32,
    force: Option<bool>,
) -> Result<calendar::MonthResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        calendar::get_month(&profile, year, month, force.unwrap_or(false))
    })
    .await
    .map_err(|_| "calendar task failed".to_string())?
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn calendar_list(profile: String) -> Result<calendar::CalendarsResult, String> {
    tauri::async_runtime::spawn_blocking(move || calendar::get_calendars(&profile, false))
        .await
        .map_err(|_| "calendar task failed".to_string())?
        .map_err(|e| e.to_string())
}

/// C-13: answer an invitation. Errors are short codes (needs_reauth, not_attendee,
/// bad_argument, conflict, not_found, forbidden, rate_limited, network, ...).
#[tauri::command]
async fn calendar_respond(
    profile: String,
    calendar_id: String,
    event_id: String,
    response: String,
) -> Result<calendar::Event, String> {
    tauri::async_runtime::spawn_blocking(move || {
        calendar::respond(&profile, &calendar_id, &event_id, &response)
    })
    .await
    .map_err(|_| "task_failed".to_string())?
    .map_err(|e| e.code())
}

/// The single Google account. The id names `profiles\personal.json` and the credential entry `clockcal/personal`.
pub const PROFILE: &str = "personal";
/// Accounts the notifier watches.
pub const PROFILES: &[&str] = &[PROFILE];

fn spawn_tray_auth(profile: &'static str, login: bool) {
    std::thread::spawn(move || {
        if login {
            match oauth::login(profile) {
                Ok(i) => log!("tray: login ok profile={profile} expires_in={}s", i.expires_in_s),
                Err(e) => log!("tray: login failed profile={profile}: {e}"),
            }
        } else {
            match oauth::logout(profile, true) {
                Ok(existed) => log!("tray: logout ok profile={profile} existed={existed}"),
                Err(e) => log!("tray: logout failed profile={profile}: {e}"),
            }
        }
    });
}

fn setup_tray(app: &AppHandle, with_hook_item: bool) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "열기", true, None::<&str>)?;
    let login = MenuItem::with_id(app, "login", "로그인", true, None::<&str>)?;
    let logout = MenuItem::with_id(app, "logout", "로그아웃", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
    let hook = if with_hook_item {
        let hook = MenuItem::with_id(app, "hook", HOOK_MENU_OFF, true, None::<&str>)?;
        if let Ok(mut g) = app.state::<HookMenu>().0.lock() {
            *g = Some(hook.clone());
        }
        Some(hook)
    } else {
        None
    };
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = vec![&open];
    if let Some(h) = hook.as_ref() {
        items.push(h);
    }
    items.push(&login);
    items.push(&logout);
    items.push(&quit);
    let menu = Menu::with_items(app, &items)?;
    let mut b = TrayIconBuilder::with_id("main")
        .tooltip("clockcal")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "open" => show_popup(app, None),
            "hook" => toggle_hook(app),
            "login" => spawn_tray_auth(PROFILE, true),
            "logout" => spawn_tray_auth(PROFILE, false),
            "quit" => {
                log!("tray: quit");
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                toggle_popup(tray.app_handle(), None);
            }
        });
    if let Some(icon) = app.default_window_icon() {
        b = b.icon(icon.clone());
    }
    b.build(app)?;
    Ok(())
}

fn setup_hotkey(app: &AppHandle) {
    let candidates = [
        ("Win+Alt+C", Shortcut::new(Some(Modifiers::SUPER | Modifiers::ALT), Code::KeyC)),
        ("Ctrl+Alt+C", Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyC)),
    ];
    for (name, sc) in candidates {
        match app.global_shortcut().register(sc) {
            Ok(()) => {
                log!("hotkey: registered {name}");
                return;
            }
            Err(e) => log!("hotkey: {name} registration failed: {e}"),
        }
    }
    log!("hotkey: no fallback hotkey registered (non-fatal)");
}

pub fn run() {
    let dpi_set = monitor::set_per_monitor_v2();
    let log_path = applog::init();
    log!(
        "start: pid={} log={:?} set_pmv2={} awareness={}",
        std::process::id(),
        log_path,
        dpi_set,
        monitor::current_awareness()
    );
    // P3-a CLI auth commands: run and exit before any window/hook/single-instance.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = oauth::run_cli(&raw) {
        log!("exit: auth cli code={code}");
        std::process::exit(code);
    }
    if let Some(code) = calendar::run_cli(&raw) {
        log!("exit: calendar cli code={code}");
        std::process::exit(code);
    }
    let args = parse_args();
    log!("args: {args:?}");

    let app = tauri::Builder::default()
        // Must be the first plugin: a 2nd launch forwards here and exits.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            log!("single-instance: 2nd launch argv_len={} -> toggle", argv.len());
            toggle_popup(app, None);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, sc, ev| {
                    if ev.state() == ShortcutState::Pressed {
                        log!("hotkey: pressed {sc:?}");
                        toggle_popup(app, None);
                    }
                })
                .build(),
        )
        .manage(Mutex::new(PopupState::default()))
        .manage(HookMenu::default())
        .invoke_handler(tauri::generate_handler![
            auth_status,
            auth_login,
            auth_import_client,
            auth_logout,
            calendar_month,
            calendar_list,
            calendar_respond,
            openurl::open_url,
            ui_effects,
            get_settings,
            set_settings
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            first_run_autostart();
            let cfg = settings::load();
            let hook_on = args.hook || cfg.clock_hook;
            if let Err(e) = setup_tray(&handle, true) {
                log!("tray: setup failed: {e}");
            }
            setup_hotkey(&handle);
            notify_runner::start(&handle, PROFILES);
            if let Some(w) = popup_window(&handle) {
                apply_acrylic_effect(&w);
            }
            // `--hook` = dev opt-in with auto-timeout; otherwise the saved setting decides (always-on, no timeout).
            let timeout = if args.hook { args.hook_timeout } else { clockhook::UNLIMITED };
            log!("[hook] start: on={hook_on} explicit_flag={} miss_expire={}", args.hook, args.miss_expire);
            clockhook::init(&handle, timeout, args.miss_expire, hook_on);
            sync_hook_menu(&handle);
            if let Some(m) = monitor::primary_monitor() {
                log!("primary monitor: {:?} work={:?} dpi={}", m.monitor, m.work, m.dpi);
            }
            if let Some(a) = args.anchor {
                let h = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(400));
                    let h2 = h.clone();
                    let _ = h.run_on_main_thread(move || show_popup(&h2, Some(a)));
                    std::thread::sleep(Duration::from_millis(600));
                    log_actual_rect(&h, "anchor-check");
                });
            }
            if let Some(n) = args.exit_after {
                let h = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(n));
                    log!("exit-after: {n}s elapsed, exiting");
                    h.exit(0);
                });
            }
            Ok(())
        })
        .on_window_event(|win, ev| {
            if win.label() != POPUP_LABEL {
                return;
            }
            match ev {
                WindowEvent::Focused(false) => {
                    if applog::now_ms() < NOTIF_BLUR_UNTIL.load(Ordering::Relaxed) {
                        log!("blur ignored (notification centre opening)");
                    } else if win.is_visible().unwrap_or(false) {
                        let _ = win.hide();
                        let app = win.app_handle();
                        if let Ok(mut s) = app.state::<Mutex<PopupState>>().lock() {
                            s.last_blur_hide_ms = Some(applog::now_ms());
                        }
                        log!("hide: reason=focus-lost");
                    }
                }
                WindowEvent::ThemeChanged(_) => {
                    if let Some(w) = win.app_handle().get_webview_window(POPUP_LABEL) {
                        apply_acrylic_effect(&w);
                    }
                }
                WindowEvent::CloseRequested { api, .. } => {
                    // Popup is reused; never destroy it (Alt+F4 just hides).
                    api.prevent_close();
                    let _ = win.hide();
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri app");

    app.run(|_app, ev| {
        if let RunEvent::ExitRequested { code, .. } = ev {
            log!("exit: requested code={code:?}");
        } else if let RunEvent::Exit = ev {
            clockhook::shutdown("exit");
            log!("exit: done");
        }
    });
}
