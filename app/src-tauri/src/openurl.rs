//! `open_url` command: open an external link in the default browser.
//!
//! The frontend whitelists links (logic.js `isSafeLink`), but the webview is not a trust
//! boundary, so the same decision is re-made here in Rust with a stricter allow-list.

/// Hosts allowed with any path.
const ANY_PATH_HOSTS: [&str; 2] = ["calendar.google.com", "meet.google.com"];
/// Host allowed only under `/calendar`.
const CALENDAR_PATH_HOST: &str = "www.google.com";

/// Returns the normalized URL if `raw` is an allowed https link, else an error message.
pub fn validate_url(raw: &str) -> Result<String, String> {
    const REJECT: &str = "url not allowed";
    // Raw-string checks first: the `url` crate lowercases, strips default ports and
    // resolves backslashes, which would hide the variants we want to reject.
    if raw.len() > 2048 {
        return Err(REJECT.into()); // D-03: length cap (Google event links are far shorter)
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return Err(REJECT.into());
    }
    let rest = raw.strip_prefix("https://").ok_or(REJECT)?; // case-sensitive scheme
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Exact host only: no userinfo ('@'), no port (':'), no uppercase.
    let host_ok = ANY_PATH_HOSTS.contains(&authority) || authority == CALENDAR_PATH_HOST;
    if !host_ok {
        return Err(REJECT.into());
    }
    let u = url::Url::parse(raw).map_err(|_| REJECT.to_string())?;
    if u.scheme() != "https"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.port().is_some()
        || u.host_str() != Some(authority)
    {
        return Err(REJECT.into());
    }
    if authority == CALENDAR_PATH_HOST {
        let p = u.path();
        if p != "/calendar" && !p.starts_with("/calendar/") {
            return Err(REJECT.into());
        }
    }
    Ok(u.into())
}

#[cfg(windows)]
fn shell_open(url: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let verb = wide("open");
    let file = wide(url);
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if r > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecute failed ({r})"))
    }
}

#[cfg(not(windows))]
fn shell_open(_url: &str) -> Result<(), String> {
    Err("unsupported platform".into())
}

#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    let safe = validate_url(&url)?;
    shell_open(&safe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_allowed() {
        for u in [
            "https://calendar.google.com/calendar/event?eid=abc",
            "https://calendar.google.com/",
            "https://calendar.google.com",
            "https://meet.google.com/abc-defg-hij",
            "https://meet.google.com/abc-defg-hij?authuser=1",
            "https://www.google.com/calendar",
            "https://www.google.com/calendar/event?eid=1",
        ] {
            assert!(validate_url(u).is_ok(), "{u}");
        }
    }

    #[test]
    fn rejects_bad() {
        for u in [
            "http://calendar.google.com/",
            "https://evil.com/",
            "https://calendar.google.com.evil.com/",
            "https://calendar.google.com@evil.com/",
            "https://evil.com@calendar.google.com/",
            "https://user:pw@calendar.google.com/",
            "https://calendar.google.com:443/",
            "https://calendar.google.com:8443/",
            "https://CALENDAR.GOOGLE.COM/",
            "HTTPS://calendar.google.com/",
            "https://calendar.google.com\\@evil.com/",
            "https://calendar.google.com%2f@evil.com/",
            "https://calendar.google.com /",
            " https://calendar.google.com/",
            "https://calendar.google.com/\n",
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "data:text/html,hi",
            "//calendar.google.com/",
            "calendar.google.com",
            "https://www.google.com/",
            "https://www.google.com/search?q=x",
            "https://www.google.com/calendarx",
            "https://www.google.com/calendar/../search",
            "https://drive.google.com/",
            "https://g.co/x",
            "https://accounts.google.com/",
            "",
        ] {
            assert!(validate_url(u).is_err(), "{u:?}");
        }
    }
}
