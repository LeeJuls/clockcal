//! Console + file logger. Never called from the hook callback.

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows::Win32::System::SystemInformation::GetLocalTime;

static FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn now_hms() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

fn now_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Opens `<crate>/logs/<prefix>-<timestamp>.log`. Returns the path.
pub fn init(prefix: &str) -> std::io::Result<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("logs");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{prefix}-{}.log", now_stamp()));
    let f = File::create(&path)?;
    let _ = FILE.set(Mutex::new(f));
    Ok(path)
}

pub fn line(msg: &str) {
    let l = format!("{} {}", now_hms(), msg);
    println!("{l}");
    if let Some(m) = FILE.get()
        && let Ok(mut f) = m.lock()
    {
        let _ = writeln!(f, "{l}");
        let _ = f.flush();
    }
}

#[macro_export]
macro_rules! logln {
    ($($t:tt)*) => { $crate::logx::line(&format!($($t)*)) };
}
