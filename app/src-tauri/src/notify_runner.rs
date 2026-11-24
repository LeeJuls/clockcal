//! P5 scheduler thread: every TICK, load events (cache-first via calendar::get_month), pick due ones with
//! notifier::due_events, show a toast, persist the fired keys so a restart never repeats a toast.

use crate::{calendar, log, notifier, settings};
use chrono::{Datelike, Local, TimeZone, Timelike, Utc};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

const TICK: Duration = Duration::from_secs(20);
/// After a failed load (offline, needs login) wait this long before asking again.
const ERROR_BACKOFF_S: i64 = 300;
/// Offline with a cache: events still come from the cache, only the refresh retry is throttled.
const STALE_BACKOFF_S: i64 = 40;

fn fired_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("clockcal").join("notified.json"))
}

fn load_fired() -> BTreeMap<String, i64> {
    fired_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_fired(f: &BTreeMap<String, i64>) {
    let Some(p) = fired_path() else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    if let Ok(data) = serde_json::to_vec(f) {
        if std::fs::write(&tmp, data).is_ok() && std::fs::rename(&tmp, &p).is_ok() {
            return;
        }
    }
    let _ = std::fs::remove_file(&tmp);
}

/// "오후 3:05" in local time.
pub fn fmt_local(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(d) => {
            let h = d.hour();
            let h12 = if h % 12 == 0 { 12 } else { h % 12 };
            format!("{} {}:{:02}", if h < 12 { "오전" } else { "오후" }, h12, d.minute())
        }
        None => String::new(),
    }
}

fn load_events(profile: &str, now: i64, lead_min: u32) -> Result<(Vec<calendar::Event>, bool), calendar::CalError> {
    let here = Local.timestamp_opt(now, 0).single().ok_or(calendar::CalError::BadArgument("clock"))?;
    let cur = calendar::get_month(profile, here.year(), here.month(), false)?;
    // Stale = refresh failed (offline); report as error so the caller backs off instead of retrying every tick.
    let stale = cur.source == calendar::Source::Stale;
    let mut events = cur.events;
    // Events right after midnight of the next month can be inside the lead window.
    let ahead = Local.timestamp_opt(now + i64::from(lead_min) * 60, 0).single();
    if let Some(a) = ahead.filter(|a| (a.year(), a.month()) != (here.year(), here.month())) {
        if let Ok(next) = calendar::get_month(profile, a.year(), a.month(), false) {
            events.extend(next.events);
        }
    }
    Ok((events, stale))
}

/// Every account is watched; one that is not signed in (or has no client file yet) is skipped quietly.
pub fn start(app: &AppHandle, profiles: &'static [&'static str]) {
    let app = app.clone();
    std::thread::Builder::new()
        .name("notifier".into())
        .spawn(move || {
            let mut fired = load_fired();
            let mut retry_at: BTreeMap<&str, i64> = BTreeMap::new();
            log!("[notify] scheduler started profiles={}", profiles.len());
            loop {
                std::thread::sleep(TICK);
                let now = Utc::now().timestamp();
                let cfg = settings::load();
                if !cfg.notify_enabled {
                    continue;
                }
                let mut events: Vec<calendar::Event> = Vec::new();
                for &profile in profiles {
                    if now < retry_at.get(profile).copied().unwrap_or(0) {
                        continue;
                    }
                    match load_events(profile, now, cfg.notify_lead_min) {
                        Ok((e, stale)) => {
                            if stale {
                                // Short backoff: must stay below the shortest notify window (lead 1 min + 60 s grace).
                                log!("[notify] {profile}: using stale cache (retry in {STALE_BACKOFF_S}s)");
                                retry_at.insert(profile, now + STALE_BACKOFF_S);
                            }
                            events.extend(e);
                        }
                        Err(calendar::CalError::NeedLogin) => {
                            // Not signed in: nothing to log every few minutes.
                            retry_at.insert(profile, now + ERROR_BACKOFF_S);
                        }
                        Err(e) => {
                            log!("[notify] {profile}: load failed: {e} (retry in {ERROR_BACKOFF_S}s)");
                            retry_at.insert(profile, now + ERROR_BACKOFF_S);
                        }
                    }
                }
                let due = notifier::due_events(&events, now, cfg.notify_lead_min, &fired, &fmt_local);
                let mut changed = false;
                for d in due {
                    let r = app.notification().builder().title(&d.title).body(&d.body).show();
                    match r {
                        Ok(()) => log!("[notify] toast shown start={} lead={}m", d.start, cfg.notify_lead_min),
                        Err(e) => log!("[notify] toast failed: {e}"),
                    }
                    // Mark as fired even on failure: a repeated failing toast every 20s is worse than a missed one.
                    fired.insert(d.key, d.start);
                    changed = true;
                }
                let before = fired.len();
                notifier::prune(&mut fired, now);
                if changed || fired.len() != before {
                    save_fired(&fired);
                }
            }
        })
        .ok();
}
