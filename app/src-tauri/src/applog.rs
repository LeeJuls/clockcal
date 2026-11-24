//! Minimal file logger: logs/app-<YYYYMMDD-HHMMSS>.log. Never log secrets.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows_sys::Win32::System::SystemInformation::GetLocalTime;

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

fn stamp() -> String {
    let t = unsafe {
        let mut st = std::mem::zeroed();
        GetLocalTime(&mut st);
        st
    };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

fn log_dir() -> PathBuf {
    if cfg!(debug_assertions) {
        // Dev builds: <app>/logs (git-ignored).
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("logs")
    } else {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("clockcal")
            .join("logs")
    }
}

/// Monotonic ms since process start.
pub fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub fn init() -> Option<PathBuf> {
    START.get_or_init(Instant::now);
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("app-{}.log", stamp()));
    let f = OpenOptions::new().create(true).append(true).open(&path).ok();
    let ok = f.is_some();
    let _ = LOG.set(Mutex::new(f));
    ok.then_some(path)
}

pub fn write(msg: &str) {
    let line = format!("[{} +{}ms] {}\n", stamp(), now_ms(), msg);
    eprint!("{line}");
    if let Some(m) = LOG.get() {
        if let Ok(mut g) = m.lock() {
            if let Some(f) = g.as_mut() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::applog::write(&format!($($t)*)) };
}
