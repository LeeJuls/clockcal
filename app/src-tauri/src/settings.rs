//! User settings (`%APPDATA%\clockcal\settings.json`). Missing/corrupt file -> defaults; unknown fields ignored.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const MIN_LEAD: u32 = 1;
pub const MAX_LEAD: u32 = 120;
pub const DEFAULT_LEAD: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub notify_enabled: bool,
    /// Minutes before the start time (clamped to MIN_LEAD..=MAX_LEAD on load).
    pub notify_lead_min: u32,
    /// Start with Windows (applied by `autostart.rs`).
    pub autostart: bool,
    /// Intercept clicks on the taskbar clock (WH_MOUSE_LL hook, no auto-timeout). Applied at start and on save.
    pub clock_hook: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { notify_enabled: true, notify_lead_min: DEFAULT_LEAD, autostart: false, clock_hook: true }
    }
}

impl Settings {
    pub fn sanitized(mut self) -> Self {
        self.notify_lead_min = self.notify_lead_min.clamp(MIN_LEAD, MAX_LEAD);
        self
    }
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("clockcal").join("settings.json"))
}

pub fn parse(s: &str) -> Settings {
    serde_json::from_str::<Settings>(s).unwrap_or_default().sanitized()
}

pub fn load() -> Settings {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map(|s| parse(&s)).unwrap_or_default()
}

/// Serialises writers (settings sheet, tray toggle) so they never share the temp file.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn save(s: &Settings) -> Result<(), &'static str> {
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = path().ok_or("APPDATA not set")?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|_| "create settings dir")?;
    }
    let data = serde_json::to_vec_pretty(&s.clone().sanitized()).map_err(|_| "serialize settings")?;
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, data).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err("write settings temp");
    }
    std::fs::rename(&tmp, &p).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        "rename settings"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_on_garbage_or_empty() {
        assert_eq!(parse(""), Settings::default());
        assert_eq!(parse("not json"), Settings::default());
        assert_eq!(parse("{}"), Settings::default());
    }

    #[test]
    fn partial_and_unknown_fields() {
        let s = parse(r#"{"notify_lead_min": 5, "future_field": 1}"#);
        assert_eq!(s.notify_lead_min, 5);
        assert!(s.notify_enabled);
    }

    #[test]
    fn lead_is_clamped() {
        assert_eq!(parse(r#"{"notify_lead_min": 0}"#).notify_lead_min, MIN_LEAD);
        assert_eq!(parse(r#"{"notify_lead_min": 99999}"#).notify_lead_min, MAX_LEAD);
    }

    #[test]
    fn wrong_type_falls_back_to_defaults() {
        assert_eq!(parse(r#"{"notify_lead_min": "ten"}"#), Settings::default());
    }
}
