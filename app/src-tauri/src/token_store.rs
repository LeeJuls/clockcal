//! Token storage in Windows Credential Manager (generic credential, local-machine
//! persistence, never roams). Target name = `clockcal/<profile>`.
//!
//! SECURITY: values stored here are secrets. Never log/print them; `StoredToken`
//! has a redacting `Debug` impl.

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND, FILETIME};
use windows_sys::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
    CRED_TYPE_GENERIC,
};

/// Max generic credential blob (CRED_MAX_CREDENTIAL_BLOB_SIZE = 5 * 512).
pub const MAX_BLOB: usize = 5 * 512;

/// Serialises every Cred* call in this process. Measured 2026-11-17: concurrent
/// CredWrite/CredDelete on *different* targets from parallel threads intermittently
/// resurrected a just-deleted credential (5 leaks / 10 parallel test runs, 0 serial).
static CRED_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cred_guard() -> std::sync::MutexGuard<'static, ()> {
    CRED_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Production target prefix. Tests use `clockcal-test/...`.
pub const TARGET_PREFIX: &str = "clockcal/";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredToken {
    pub refresh_token: String,
    #[serde(default)]
    pub access_token: Option<String>,
    /// Unix seconds when `access_token` expires.
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl std::fmt::Debug for StoredToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredToken")
            .field("refresh_token", &"<redacted>")
            .field("access_token", &self.access_token.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// Win32 error code from CredWrite/CredRead/CredDelete.
    Win32(u32),
    TooLarge(usize),
    Corrupt,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Win32(c) => write!(f, "credential store win32 error {c}"),
            StoreError::TooLarge(n) => write!(f, "credential blob too large ({n} bytes)"),
            StoreError::Corrupt => write!(f, "stored credential is not valid token json"),
        }
    }
}

/// `clockcal/<profile>`.
pub fn target_for(profile: &str) -> String {
    format!("{TARGET_PREFIX}{profile}")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Write raw bytes under `target` (overwrites).
pub fn write_raw(target: &str, blob: &[u8]) -> Result<(), StoreError> {
    if blob.len() > MAX_BLOB {
        return Err(StoreError::TooLarge(blob.len()));
    }
    let _g = cred_guard();
    let mut t = wide(target);
    let mut user = wide("clockcal");
    let cred = CREDENTIALW {
        Flags: 0,
        Type: CRED_TYPE_GENERIC,
        TargetName: t.as_mut_ptr(),
        Comment: std::ptr::null_mut(),
        LastWritten: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        CredentialBlobSize: blob.len() as u32,
        // CredWriteW does not modify the blob; the API just isn't const-correct.
        CredentialBlob: blob.as_ptr() as *mut u8,
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: std::ptr::null_mut(),
        TargetAlias: std::ptr::null_mut(),
        UserName: user.as_mut_ptr(),
    };
    // SAFETY: all pointers are valid for the duration of the call.
    let ok = unsafe { CredWriteW(&cred, 0) };
    if ok == 0 {
        return Err(StoreError::Win32(unsafe { GetLastError() }));
    }
    Ok(())
}

/// Read raw bytes; Ok(None) when the credential does not exist.
pub fn read_raw(target: &str) -> Result<Option<Vec<u8>>, StoreError> {
    let _g = cred_guard();
    let t = wide(target);
    let mut p: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: t is NUL-terminated; p receives a CredFree-owned buffer.
    let ok = unsafe { CredReadW(t.as_ptr(), CRED_TYPE_GENERIC, 0, &mut p) };
    if ok == 0 {
        let e = unsafe { GetLastError() };
        return if e == ERROR_NOT_FOUND {
            Ok(None)
        } else {
            Err(StoreError::Win32(e))
        };
    }
    // SAFETY: p is valid until CredFree.
    let out = unsafe {
        let c = &*p;
        let v = if c.CredentialBlob.is_null() || c.CredentialBlobSize == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
        };
        CredFree(p as *const _);
        v
    };
    Ok(Some(out))
}

/// Delete; Ok(false) when it did not exist.
pub fn delete_raw(target: &str) -> Result<bool, StoreError> {
    let _g = cred_guard();
    let t = wide(target);
    // SAFETY: t is NUL-terminated.
    let ok = unsafe { CredDeleteW(t.as_ptr(), CRED_TYPE_GENERIC, 0) };
    if ok == 0 {
        let e = unsafe { GetLastError() };
        return if e == ERROR_NOT_FOUND {
            Ok(false)
        } else {
            Err(StoreError::Win32(e))
        };
    }
    Ok(true)
}

pub fn save_at(target: &str, tok: &StoredToken) -> Result<(), StoreError> {
    let json = serde_json::to_vec(tok).map_err(|_| StoreError::Corrupt)?;
    write_raw(target, &json)
}

pub fn load_at(target: &str) -> Result<Option<StoredToken>, StoreError> {
    match read_raw(target)? {
        None => Ok(None),
        Some(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|_| StoreError::Corrupt),
    }
}

pub fn save(profile: &str, tok: &StoredToken) -> Result<(), StoreError> {
    save_at(&target_for(profile), tok)
}

pub fn load(profile: &str) -> Result<Option<StoredToken>, StoreError> {
    load_at(&target_for(profile))
}

pub fn delete(profile: &str) -> Result<bool, StoreError> {
    delete_raw(&target_for(profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_target(tag: &str) -> String {
        format!("clockcal-test/{tag}-{}", std::process::id())
    }

    #[test]
    fn target_name() {
        assert_eq!(target_for("personal"), "clockcal/personal");
    }

    #[test]
    fn debug_redacts() {
        let t = StoredToken {
            refresh_token: "RT-DUMMY".into(),
            access_token: Some("AT-DUMMY".into()),
            expires_at: Some(5),
            scope: None,
        };
        let s = format!("{t:?}");
        assert!(!s.contains("RT-DUMMY") && !s.contains("AT-DUMMY"));
    }

    #[test]
    fn raw_roundtrip_and_cleanup() {
        let tg = test_target("raw");
        let _ = delete_raw(&tg);
        assert_eq!(read_raw(&tg).unwrap(), None);
        write_raw(&tg, b"dummy-value").unwrap();
        assert_eq!(read_raw(&tg).unwrap().as_deref(), Some(&b"dummy-value"[..]));
        write_raw(&tg, b"dummy-2").unwrap(); // overwrite
        assert_eq!(read_raw(&tg).unwrap().as_deref(), Some(&b"dummy-2"[..]));
        assert!(delete_raw(&tg).unwrap());
        assert!(!delete_raw(&tg).unwrap());
        assert_eq!(read_raw(&tg).unwrap(), None);
    }

    #[test]
    fn token_roundtrip_and_cleanup() {
        let tg = test_target("tok");
        let t = StoredToken {
            refresh_token: "dummy-refresh".into(),
            access_token: Some("dummy-access".into()),
            expires_at: Some(1_900_000_000),
            scope: Some("https://www.googleapis.com/auth/calendar.readonly".into()),
        };
        save_at(&tg, &t).unwrap();
        assert_eq!(load_at(&tg).unwrap(), Some(t));
        write_raw(&tg, b"not json").unwrap();
        assert_eq!(load_at(&tg), Err(StoreError::Corrupt));
        assert!(delete_raw(&tg).unwrap());
        assert_eq!(load_at(&tg).unwrap(), None);
    }

    #[test]
    fn too_large_rejected() {
        let big = vec![b'x'; MAX_BLOB + 1];
        assert_eq!(
            write_raw("clockcal-test/never-written", &big),
            Err(StoreError::TooLarge(MAX_BLOB + 1))
        );
    }
}
