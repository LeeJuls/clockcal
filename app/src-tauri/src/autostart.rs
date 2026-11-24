//! Start with Windows: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value `clockcal` = quoted exe path.
//! Per-user key, no admin rights. The hook (`--hook`) is deliberately NOT part of the command line.

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "clockcal";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Run-key command for an exe path: quoted so spaces in the path are safe.
pub fn run_value(exe: &str) -> String {
    format!("\"{exe}\"")
}

/// Cargo output dirs (`...\target\debug|release`) are dev builds.
pub fn is_dev_exe(exe: &str) -> bool {
    exe.to_lowercase().contains(r"\target\")
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn open(access: u32) -> Result<Key, &'static str> {
    let mut h: HKEY = std::ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, access, &mut h) };
    if rc == ERROR_SUCCESS {
        Ok(Key(h))
    } else {
        Err("open Run key")
    }
}

/// Current registry value (None when absent).
pub fn current_value() -> Option<String> {
    let k = open(KEY_QUERY_VALUE).ok()?;
    let name = wide(VALUE_NAME);
    let mut len: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(k.0, name.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), &mut len)
    };
    if rc != ERROR_SUCCESS || len == 0 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    let rc = unsafe {
        RegQueryValueExW(k.0, name.as_ptr(), std::ptr::null(), std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len)
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(String::from_utf16_lossy(&buf))
}

pub fn is_enabled() -> bool {
    current_value().is_some()
}

pub fn set_enabled(on: bool) -> Result<(), &'static str> {
    let k = open(KEY_SET_VALUE)?;
    let name = wide(VALUE_NAME);
    if on {
        let exe = std::env::current_exe().map_err(|_| "current exe")?;
        // A dev build must not overwrite the installed exe's registration.
        if is_dev_exe(&exe.to_string_lossy()) {
            return Ok(());
        }
        let data = wide(&run_value(&exe.to_string_lossy()));
        let rc = unsafe { RegSetValueExW(k.0, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err("write Run value")
        }
    } else {
        let rc = unsafe { RegDeleteValueW(k.0, name.as_ptr()) };
        if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err("delete Run value")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_exe_detection() {
        assert!(is_dev_exe(r"D:\x\app\src-tauri\target\debug\clockcal.exe"));
        assert!(!is_dev_exe(r"C:\Users\u\AppData\Local\clockcal\clockcal.exe"));
    }

    #[test]
    fn run_value_is_quoted() {
        assert_eq!(run_value(r"C:\Program Files\clockcal\clockcal.exe"), "\"C:\\Program Files\\clockcal\\clockcal.exe\"");
    }
}
