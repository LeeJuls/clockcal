//! P3-a: Google OAuth 2.0 for installed apps (loopback redirect + PKCE S256).
//!
//! - Client config: `%APPDATA%\clockcal\profiles\<profile>.json` (Google
//!   "installed" client JSON), read at runtime only, never logged.
//! - Tokens: Windows Credential Manager via `token_store` (`clockcal/<profile>`).
//! - Scopes: calendar.events (respond to invitations, C-13) + calendar.readonly
//!   (calendarList/colors). Tokens granted before C-13 have readonly only:
//!   `AuthStatus.can_respond` is false for them until the user logs in again.
//!
//! SECURITY: never log client_id/client_secret/codes/tokens/auth URL. Logs may
//! contain: profile name, yes/no, ports, expiry seconds, scopes, error codes.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::log;
use crate::token_store::{self, StoreError, StoredToken};

pub const SCOPE_EVENTS: &str = "https://www.googleapis.com/auth/calendar.events";
pub const SCOPE_READONLY: &str = "https://www.googleapis.com/auth/calendar.readonly";
/// Requested scopes, space separated (must equal EVENTS + " " + READONLY; asserted in tests).
pub const SCOPE: &str =
    "https://www.googleapis.com/auth/calendar.events https://www.googleapis.com/auth/calendar.readonly";

/// True when the space-separated granted `scope` string contains `wanted` exactly.
pub fn scope_has(scope: Option<&str>, wanted: &str) -> bool {
    scope.is_some_and(|s| s.split_whitespace().any(|x| x == wanted))
}

/// Whether the stored grant allows writing event responses. Old tokens
/// (readonly only, or no scope recorded) -> false.
pub fn can_respond(scope: Option<&str>) -> bool {
    scope_has(scope, SCOPE_EVENTS)
}
pub const REVOKE_URI: &str = "https://oauth2.googleapis.com/revoke";
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
/// Refresh when the access token expires within this many seconds.
pub const REFRESH_SKEW_S: u64 = 60;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// Serialises login/refresh/logout within the process.
static AUTH_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No usable refresh token (never logged in, or revoked: invalid_grant).
    NeedLogin,
    InvalidProfile,
    /// Client JSON missing/invalid. Payload is a fixed description, never file content.
    ClientConfig(&'static str),
    /// OAuth `error=` on the redirect (e.g. access_denied). Sanitised code only.
    Denied(String),
    StateMismatch,
    MissingCode,
    Timeout,
    /// Transport failure (kind only, no URL/body).
    Network(&'static str),
    /// Token endpoint returned non-2xx. Only the OAuth `error` code is kept, never the body.
    TokenEndpoint { status: u16, error: Option<String> },
    /// Token endpoint 2xx but unusable response.
    Protocol(&'static str),
    Store(StoreError),
    Browser(i64),
    Io(&'static str),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::NeedLogin => write!(f, "need login"),
            AuthError::InvalidProfile => write!(f, "invalid profile name"),
            AuthError::ClientConfig(s) => write!(f, "client config: {s}"),
            AuthError::Denied(c) => write!(f, "authorization denied: {c}"),
            AuthError::StateMismatch => write!(f, "state mismatch on callback"),
            AuthError::MissingCode => write!(f, "callback without code"),
            AuthError::Timeout => write!(f, "login timed out"),
            AuthError::Network(k) => write!(f, "network error: {k}"),
            AuthError::TokenEndpoint { status, error } => write!(
                f,
                "token endpoint http {status} error={}",
                error.as_deref().unwrap_or("-")
            ),
            AuthError::Protocol(s) => write!(f, "protocol: {s}"),
            AuthError::Store(e) => write!(f, "{e}"),
            AuthError::Browser(c) => write!(f, "could not open browser (code {c})"),
            AuthError::Io(s) => write!(f, "io: {s}"),
        }
    }
}

impl From<StoreError> for AuthError {
    fn from(e: StoreError) -> Self {
        AuthError::Store(e)
    }
}

/// Keep only `[A-Za-z0-9_.-]`, max 40 chars (for codes echoed from untrusted input).
fn sanitize_code(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .take(40)
        .collect()
}

// ---------------------------------------------------------------- profile / client

/// Profile names map to file names and credential targets: `[a-z0-9_-]{1,32}`.
pub fn validate_profile(p: &str) -> Result<(), AuthError> {
    let ok = !p.is_empty()
        && p.len() <= 32
        && p
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(AuthError::InvalidProfile)
    }
}

pub fn profiles_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("clockcal").join("profiles"))
}

pub fn client_path(profile: &str) -> Result<PathBuf, AuthError> {
    validate_profile(profile)?;
    profiles_dir()
        .map(|d| d.join(format!("{profile}.json")))
        .ok_or(AuthError::ClientConfig("APPDATA not set"))
}

#[derive(Clone)]
pub struct ClientConfig {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub auth_uri: String,
    pub token_uri: String,
}

impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("client_id", &"<redacted>")
            .field("client_secret", &self.client_secret.as_ref().map(|_| "<redacted>"))
            .field("auth_uri", &self.auth_uri)
            .field("token_uri", &self.token_uri)
            .finish()
    }
}

#[derive(Deserialize)]
struct ClientFile {
    installed: Option<ClientInner>,
}

#[derive(Deserialize)]
struct ClientInner {
    client_id: Option<String>,
    client_secret: Option<String>,
    auth_uri: Option<String>,
    token_uri: Option<String>,
}

/// Parse a Google "installed" client JSON. Errors never include file content.
pub fn parse_client_json(s: &str) -> Result<ClientConfig, AuthError> {
    let f: ClientFile =
        serde_json::from_str(s).map_err(|_| AuthError::ClientConfig("not valid json"))?;
    let i = f
        .installed
        .ok_or(AuthError::ClientConfig("no 'installed' section (desktop client required)"))?;
    let client_id = i
        .client_id
        .filter(|v| !v.trim().is_empty())
        .ok_or(AuthError::ClientConfig("client_id missing"))?;
    let auth_uri = i
        .auth_uri
        .unwrap_or_else(|| "https://accounts.google.com/o/oauth2/auth".into());
    let token_uri = i
        .token_uri
        .unwrap_or_else(|| "https://oauth2.googleapis.com/token".into());
    if !auth_uri.starts_with("https://") || !token_uri.starts_with("https://") {
        return Err(AuthError::ClientConfig("auth_uri/token_uri must be https"));
    }
    Ok(ClientConfig {
        client_id,
        client_secret: i.client_secret.filter(|v| !v.is_empty()),
        auth_uri,
        token_uri,
    })
}

/// Reads the user's own client file `profiles\<profile>.json`.
pub fn load_client(profile: &str) -> Result<ClientConfig, AuthError> {
    let p = client_path(profile)?;
    match std::fs::read_to_string(&p) {
        Ok(s) => parse_client_json(&s),
        Err(_) => Err(AuthError::ClientConfig("file not readable")),
    }
}

const MAX_CLIENT_JSON_BYTES: usize = 32 * 1024;

/// Validate `json` as a Google desktop client file and store it as `<dir>\<profile>.json`
/// (temp file + rename). Content is never logged or echoed in errors.
pub fn import_client_to(dir: &std::path::Path, profile: &str, json: &str) -> Result<(), AuthError> {
    validate_profile(profile)?;
    if json.len() > MAX_CLIENT_JSON_BYTES {
        return Err(AuthError::ClientConfig("file too large"));
    }
    parse_client_json(json)?;
    std::fs::create_dir_all(dir).map_err(|_| AuthError::Io("create profiles dir failed"))?;
    let dest = dir.join(format!("{profile}.json"));
    let tmp = dir.join(format!("{profile}.json.tmp"));
    std::fs::write(&tmp, json).map_err(|_| AuthError::Io("write client file failed"))?;
    std::fs::rename(&tmp, &dest).map_err(|_| {
        let _ = std::fs::remove_file(&tmp);
        AuthError::Io("replace client file failed")
    })
}

/// Import a client JSON for `profile`. A token issued for the previous client is dropped,
/// so the next sign-in uses the new client.
pub fn import_client(profile: &str, json: &str) -> Result<(), AuthError> {
    validate_profile(profile)?;
    let _g = AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = profiles_dir().ok_or(AuthError::ClientConfig("APPDATA not set"))?;
    import_client_to(&dir, profile, json)?;
    let _ = token_store::delete_raw(&token_store::target_for(profile));
    log!("[oauth] import_client: profile={profile} ok");
    Ok(())
}

// ---------------------------------------------------------------- encoding / PKCE

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding (RFC 4648 sec. 5).
pub fn b64url(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(B64URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub fn random_b64url(nbytes: usize) -> Result<String, AuthError> {
    let mut buf = vec![0u8; nbytes];
    getrandom::fill(&mut buf).map_err(|_| AuthError::Io("rng failed"))?;
    Ok(b64url(&buf))
}

/// RFC 7636 S256: BASE64URL(SHA256(ASCII(verifier))).
pub fn pkce_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// 32 random bytes -> 43-char verifier (RFC 7636 recommends this).
    pub fn generate() -> Result<Self, AuthError> {
        let verifier = random_b64url(32)?;
        let challenge = pkce_challenge(&verifier);
        Ok(Self { verifier, challenge })
    }
}

/// Percent-encode everything except RFC 3986 unreserved characters.
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decode `application/x-www-form-urlencoded` component ('+' = space). None on bad escape/UTF-8.
pub fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let h = b.get(i + 1..i + 3)?;
                let v = u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()?;
                out.push(v);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

pub fn form_body(form: &[(&str, &str)]) -> String {
    form.iter()
        .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn redirect_uri_for(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Authorization URL. Contains client_id: do not log it.
pub fn build_auth_url(c: &ClientConfig, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let q = form_body(&[
        ("client_id", &c.client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ]);
    let sep = if c.auth_uri.contains('?') { '&' } else { '?' };
    format!("{}{sep}{q}", c.auth_uri)
}

// ---------------------------------------------------------------- callback parsing

fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Request line `GET <target> HTTP/1.x` -> target.
pub fn parse_request_line(line: &str) -> Option<&str> {
    let mut it = line.split(' ');
    let method = it.next()?;
    let target = it.next()?;
    let ver = it.next()?;
    (method == "GET" && ver.starts_with("HTTP/1.")).then_some(target)
}

/// Request target `/?state=..&code=..` -> authorization code.
/// Order: state must match first (also for error responses), then `error`, then `code`.
pub fn parse_callback(target: &str, expected_state: &str) -> Result<String, AuthError> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut state = None;
    let mut code = None;
    let mut error = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let Some(v) = pct_decode(v) else { continue };
        let slot = match k {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        if slot.is_none() {
            *slot = Some(v);
        }
    }
    match state {
        Some(s) if ct_eq(&s, expected_state) => {}
        _ => return Err(AuthError::StateMismatch),
    }
    if let Some(e) = error {
        return Err(AuthError::Denied(sanitize_code(&e)));
    }
    code.filter(|c| !c.is_empty()).ok_or(AuthError::MissingCode)
}

// ---------------------------------------------------------------- token responses

#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in: u64,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("expires_in", &self.expires_in)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("scope", &self.scope)
            .finish()
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
}

/// Parse the token endpoint reply. Error bodies are reduced to the OAuth `error` code.
pub fn parse_token_response(status: u16, body: &str) -> Result<TokenResponse, AuthError> {
    if !(200..300).contains(&status) {
        let error = serde_json::from_str::<ErrorBody>(body)
            .ok()
            .and_then(|e| e.error)
            .map(|e| sanitize_code(&e));
        return Err(AuthError::TokenEndpoint { status, error });
    }
    let t: TokenResponse =
        serde_json::from_str(body).map_err(|_| AuthError::Protocol("token json invalid"))?;
    if t.access_token.is_empty() {
        return Err(AuthError::Protocol("empty access_token"));
    }
    Ok(t)
}

pub fn is_invalid_grant(e: &AuthError) -> bool {
    matches!(e, AuthError::TokenEndpoint { error: Some(c), .. } if c == "invalid_grant")
}

/// True when there is no access token expiry or it ends within `skew` seconds.
pub fn needs_refresh(expires_at: Option<u64>, now: u64, skew: u64) -> bool {
    match expires_at {
        Some(t) => t <= now.saturating_add(skew),
        None => true,
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- transport

/// POST `application/x-www-form-urlencoded`, returns (status, body).
pub trait Transport {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), AuthError>;
}

pub struct HttpTransport {
    client: reqwest::blocking::Client,
}

impl HttpTransport {
    pub fn new() -> Result<Self, AuthError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .https_only(true)
            .build()
            .map_err(|_| AuthError::Network("client build failed"))?;
        Ok(Self { client })
    }
}

impl Transport for HttpTransport {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<(u16, String), AuthError> {
        let resp = self
            .client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(form_body(form))
            .send()
            .map_err(|e| {
                AuthError::Network(if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect failed"
                } else {
                    "request failed"
                })
            })?;
        let status = resp.status().as_u16();
        let body = resp.text().map_err(|_| AuthError::Network("read body failed"))?;
        Ok((status, body))
    }
}

// ---------------------------------------------------------------- loopback server

pub struct Loopback {
    listener: TcpListener,
    port: u16,
}

const HTML_OK: &str = "<!doctype html><html lang=\"ko\"><head><meta charset=\"utf-8\"><title>clockcal</title></head>\
<body style=\"font-family:'Segoe UI',sans-serif;padding:40px\"><h2>clockcal 로그인 완료, 창을 닫아도 됩니다</h2></body></html>";

fn html_fail(reason: &str) -> String {
    format!(
        "<!doctype html><html lang=\"ko\"><head><meta charset=\"utf-8\"><title>clockcal</title></head>\
<body style=\"font-family:'Segoe UI',sans-serif;padding:40px\"><h2>clockcal 로그인 실패</h2><p>{}</p></body></html>",
        sanitize_code(reason)
    )
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Read up to the end of headers. None on timeout/garbage.
fn read_request_head(stream: &mut TcpStream, deadline: Instant) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let _ = stream.set_read_timeout(Some(left.min(Duration::from_secs(5)).max(Duration::from_millis(10))));
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > MAX_REQUEST_BYTES {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
}

impl Loopback {
    /// Bind 127.0.0.1 on an OS-chosen free port.
    pub fn bind() -> Result<Self, AuthError> {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| AuthError::Io("loopback bind failed"))?;
        let port = listener
            .local_addr()
            .map_err(|_| AuthError::Io("loopback addr failed"))?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|_| AuthError::Io("loopback nonblocking failed"))?;
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn local_ip_is_loopback(&self) -> bool {
        self.listener
            .local_addr()
            .map(|a| a.ip().is_loopback())
            .unwrap_or(false)
    }

    pub fn redirect_uri(&self) -> String {
        redirect_uri_for(self.port)
    }

    /// Wait for the single callback request on path `/`, answer it, and return.
    /// Requests for other paths (e.g. /favicon.ico) get 404 and do not count.
    /// Consumes self: the listener is closed on return.
    pub fn wait_for_code(self, expected_state: &str, timeout: Duration) -> Result<String, AuthError> {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                log!("[oauth] callback: timeout after {}s", timeout.as_secs());
                return Err(AuthError::Timeout);
            }
            let (mut stream, peer) = match self.listener.accept() {
                Ok(x) => x,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(_) => return Err(AuthError::Io("accept failed")),
            };
            if !peer.ip().is_loopback() {
                continue; // cannot happen with a 127.0.0.1 bind; defensive.
            }
            let _ = stream.set_nonblocking(false);
            let Some(head) = read_request_head(&mut stream, deadline) else {
                continue; // speculative/empty connection
            };
            let line = head.lines().next().unwrap_or("");
            let Some(target) = parse_request_line(line) else {
                respond(&mut stream, "400 Bad Request", &html_fail("bad_request"));
                continue;
            };
            let path = target.split('?').next().unwrap_or("");
            if path != "/" {
                respond(&mut stream, "404 Not Found", "");
                continue;
            }
            let r = parse_callback(target, expected_state);
            match &r {
                Ok(_) => respond(&mut stream, "200 OK", HTML_OK),
                Err(AuthError::Denied(c)) => respond(&mut stream, "200 OK", &html_fail(c)),
                Err(AuthError::StateMismatch) => {
                    respond(&mut stream, "400 Bad Request", &html_fail("state_mismatch"))
                }
                Err(_) => respond(&mut stream, "400 Bad Request", &html_fail("missing_code")),
            }
            log!(
                "[oauth] callback: handled ok={} err={}",
                r.is_ok(),
                r.as_ref().err().map(|e| e.to_string()).unwrap_or_default()
            );
            return r;
        }
    }
}

// ---------------------------------------------------------------- browser

/// Open `url` with the default browser via ShellExecuteW (no shell parsing, so
/// `&` in the URL is safe). Only https URLs are accepted.
pub fn open_browser(url: &str) -> Result<(), AuthError> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    if !url.starts_with("https://") || url.contains(['\r', '\n', '\0']) {
        return Err(AuthError::Browser(-1));
    }
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated wide strings valid for the call.
    let h = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = h as isize as i64;
    if code > 32 {
        Ok(())
    } else {
        Err(AuthError::Browser(code))
    }
}

// ---------------------------------------------------------------- flows

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LoginInfo {
    pub expires_in_s: u64,
    pub scope: Option<String>,
}

/// Exchange an authorization code. `redirect_uri` must equal the one in the auth URL.
pub fn exchange_code(
    c: &ClientConfig,
    t: &dyn Transport,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenResponse, AuthError> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", &c.client_id),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    if let Some(s) = &c.client_secret {
        form.push(("client_secret", s));
    }
    let (status, body) = t.post_form(&c.token_uri, &form)?;
    parse_token_response(status, &body)
}

/// Login flow with injectable transport/browser/store target (tests).
pub fn login_with(
    target: &str,
    c: &ClientConfig,
    t: &dyn Transport,
    open: &dyn Fn(&str) -> Result<(), AuthError>,
    timeout: Duration,
) -> Result<LoginInfo, AuthError> {
    let pkce = Pkce::generate()?;
    let state = random_b64url(24)?;
    let lb = Loopback::bind()?;
    let redirect = lb.redirect_uri();
    log!(
        "[oauth] login: loopback 127.0.0.1:{} loopback_ip={} timeout={}s",
        lb.port(),
        lb.local_ip_is_loopback(),
        timeout.as_secs()
    );
    let url = build_auth_url(c, &redirect, &pkce.challenge, &state);
    open(&url)?;
    log!("[oauth] login: browser opened, waiting for callback");
    let code = lb.wait_for_code(&state, timeout)?;
    let tok = exchange_code(c, t, &code, &pkce.verifier, &redirect)?;
    let refresh = tok
        .refresh_token
        .clone()
        .ok_or(AuthError::Protocol("no refresh_token in response"))?;
    if tok.scope.as_deref().is_some_and(|s| !(scope_has(Some(s), SCOPE_EVENTS) && scope_has(Some(s), SCOPE_READONLY))) {
        log!("[oauth] login: warning granted scope={:?}", tok.scope);
    }
    let stored = StoredToken {
        refresh_token: refresh,
        access_token: Some(tok.access_token.clone()),
        expires_at: Some(unix_now().saturating_add(tok.expires_in)),
        scope: tok.scope.clone(),
    };
    token_store::save_at(target, &stored)?;
    log!(
        "[oauth] login: stored, expires_in={}s scope={:?}",
        tok.expires_in,
        tok.scope
    );
    Ok(LoginInfo {
        expires_in_s: tok.expires_in,
        scope: tok.scope,
    })
}

pub fn login(profile: &str) -> Result<LoginInfo, AuthError> {
    validate_profile(profile)?;
    let _g = AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let c = load_client(profile)?;
    log!("[oauth] login: profile={profile} client loaded: yes");
    let t = HttpTransport::new()?;
    login_with(
        &token_store::target_for(profile),
        &c,
        &t,
        &open_browser,
        CALLBACK_TIMEOUT,
    )
}

/// Valid access token for `target`, refreshing if needed. `client` is loaded lazily
/// (only when a refresh is required).
pub fn access_token_with(
    target: &str,
    client: &dyn Fn() -> Result<ClientConfig, AuthError>,
    t: &dyn Transport,
    now: u64,
) -> Result<(String, u64), AuthError> {
    let mut st = token_store::load_at(target)?.ok_or(AuthError::NeedLogin)?;
    if let (Some(at), false) = (&st.access_token, needs_refresh(st.expires_at, now, REFRESH_SKEW_S)) {
        let left = st.expires_at.unwrap_or(now).saturating_sub(now);
        return Ok((at.clone(), left));
    }
    let c = client()?;
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", &st.refresh_token),
        ("client_id", &c.client_id),
    ];
    if let Some(s) = &c.client_secret {
        form.push(("client_secret", s));
    }
    let res = t
        .post_form(&c.token_uri, &form)
        .and_then(|(status, body)| parse_token_response(status, &body));
    let tok = match res {
        Ok(t) => t,
        Err(e) if is_invalid_grant(&e) => {
            log!("[oauth] refresh: invalid_grant -> deleting stored token, need login");
            let _ = token_store::delete_raw(target);
            return Err(AuthError::NeedLogin);
        }
        Err(e) => return Err(e),
    };
    if let Some(rt) = tok.refresh_token {
        st.refresh_token = rt; // rotation (Google normally keeps the old one)
    }
    st.access_token = Some(tok.access_token.clone());
    st.expires_at = Some(now.saturating_add(tok.expires_in));
    if tok.scope.is_some() {
        st.scope = tok.scope;
    }
    token_store::save_at(target, &st)?;
    log!("[oauth] refresh: ok expires_in={}s", tok.expires_in);
    Ok((tok.access_token, tok.expires_in))
}

/// Returns (access_token, seconds until expiry). Never log the token.
pub fn access_token_ex(profile: &str) -> Result<(String, u64), AuthError> {
    validate_profile(profile)?;
    let _g = AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let t = HttpTransport::new()?;
    access_token_with(
        &token_store::target_for(profile),
        &|| load_client(profile),
        &t,
        unix_now(),
    )
}

pub fn access_token(profile: &str) -> Result<String, AuthError> {
    access_token_ex(profile).map(|(t, _)| t)
}

/// Delete the stored token. With `revoke`, best-effort revoke at Google first.
/// Returns whether a token existed.
pub fn logout(profile: &str, revoke: bool) -> Result<bool, AuthError> {
    validate_profile(profile)?;
    let _g = AUTH_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let target = token_store::target_for(profile);
    if revoke {
        if let Ok(Some(st)) = token_store::load_at(&target) {
            match HttpTransport::new()
                .and_then(|t| t.post_form(REVOKE_URI, &[("token", &st.refresh_token)]))
            {
                Ok((status, _)) => log!("[oauth] logout: revoke http {status}"),
                Err(e) => log!("[oauth] logout: revoke failed ({e})"),
            }
        }
    }
    let existed = token_store::delete_raw(&target)?;
    log!("[oauth] logout: profile={profile} existed={existed}");
    Ok(existed)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AuthStatus {
    pub profile: String,
    pub client_configured: bool,
    pub logged_in: bool,
    /// Unix seconds of access token expiry (None if not logged in / unknown).
    pub access_expires_at: Option<u64>,
    /// Seconds left (0 if already expired).
    pub access_expires_in_s: Option<u64>,
    pub scope: Option<String>,
    /// True only if the granted scope contains calendar.events (C-13).
    pub can_respond: bool,
}

pub fn status_at(profile: &str, target: &str, client_configured: bool, now: u64) -> Result<AuthStatus, AuthError> {
    let st = token_store::load_at(target)?;
    Ok(AuthStatus {
        profile: profile.to_string(),
        client_configured,
        logged_in: st.is_some(),
        access_expires_at: st.as_ref().and_then(|s| s.expires_at),
        access_expires_in_s: st
            .as_ref()
            .and_then(|s| s.expires_at)
            .map(|t| t.saturating_sub(now)),
        can_respond: can_respond(st.as_ref().and_then(|s| s.scope.as_deref())),
        scope: st.and_then(|s| s.scope),
    })
}

pub fn status(profile: &str) -> Result<AuthStatus, AuthError> {
    validate_profile(profile)?;
    // Parse (not just exists) so a malformed file shows up; content never leaves here.
    let configured = match load_client(profile) {
        Ok(_) => true,
        Err(e) => {
            log!("[oauth] status: profile={profile} client loaded: no ({e})");
            false
        }
    };
    status_at(profile, &token_store::target_for(profile), configured, unix_now())
}

// ---------------------------------------------------------------- CLI

/// Handles `--auth-status|--auth-login|--auth-token-check|--auth-logout <profile>`.
/// Returns Some(exit code) when an auth command was given (caller must exit).
pub fn run_cli(args: &[String]) -> Option<i32> {
    let i = args.iter().position(|a| {
        matches!(
            a.as_str(),
            "--auth-status" | "--auth-login" | "--auth-token-check" | "--auth-logout"
        )
    })?;
    let cmd = args[i].as_str();
    let profile = args.get(i + 1).map(String::as_str).unwrap_or("");
    log!("[oauth] cli: {cmd} profile={profile}");
    let res: Result<String, AuthError> = match cmd {
        "--auth-status" => status(profile).map(|s| {
            format!(
                "profile={} client_configured={} logged_in={} expires_in_s={} can_respond={} scope={}",
                s.profile,
                s.client_configured,
                s.logged_in,
                s.access_expires_in_s.map(|v| v.to_string()).unwrap_or("-".into()),
                s.can_respond,
                s.scope.unwrap_or("-".into())
            )
        }),
        "--auth-login" => {
            println!("브라우저에서 구글 로그인을 완료하세요 (제한 {}초)...", CALLBACK_TIMEOUT.as_secs());
            login(profile).map(|i| {
                format!(
                    "login ok, expires in {} s, scope={}",
                    i.expires_in_s,
                    i.scope.unwrap_or("-".into())
                )
            })
        }
        "--auth-token-check" => access_token_ex(profile).map(|(_, left)| format!("ok, expires in {left} s")),
        "--auth-logout" => logout(profile, true).map(|e| format!("logout ok (token existed={e})")),
        _ => unreachable!(),
    };
    match res {
        Ok(msg) => {
            println!("{msg}");
            log!("[oauth] cli: {cmd} -> {msg}");
            Some(0)
        }
        Err(e) => {
            let code = if e == AuthError::NeedLogin { 3 } else { 2 };
            println!("error: {e}");
            log!("[oauth] cli: {cmd} -> error: {e}");
            Some(code)
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const FIXTURE: &str = r#"{"installed":{"client_id":"fixture-id.apps.example","project_id":"p",
        "auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token",
        "client_secret":"fixture-secret","redirect_uris":["http://localhost"]}}"#;

    fn client() -> ClientConfig {
        parse_client_json(FIXTURE).unwrap()
    }

    fn test_target(tag: &str) -> String {
        format!("clockcal-test/oauth-{tag}-{}", std::process::id())
    }

    struct Mock {
        replies: RefCell<Vec<(u16, String)>>,
        calls: RefCell<Vec<Vec<(String, String)>>>,
    }
    impl Mock {
        fn new(replies: Vec<(u16, &str)>) -> Self {
            Self {
                replies: RefCell::new(replies.into_iter().rev().map(|(s, b)| (s, b.to_string())).collect()),
                calls: RefCell::new(vec![]),
            }
        }
        fn field(&self, call: usize, k: &str) -> Option<String> {
            self.calls.borrow()[call].iter().find(|(a, _)| a == k).map(|(_, v)| v.clone())
        }
    }
    impl Transport for Mock {
        fn post_form(&self, _url: &str, form: &[(&str, &str)]) -> Result<(u16, String), AuthError> {
            self.calls
                .borrow_mut()
                .push(form.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
            self.replies.borrow_mut().pop().ok_or(AuthError::Network("no mock reply"))
        }
    }

    #[test]
    fn pkce_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // Appendix B verifier bytes -> verifier string.
        let bytes: [u8; 32] = [
            116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212, 37, 77,
            105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        ];
        assert_eq!(b64url(&bytes), "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    }

    #[test]
    fn b64url_padding_cases() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn pkce_generate_shape() {
        let p = Pkce::generate().unwrap();
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge, pkce_challenge(&p.verifier));
        assert_ne!(Pkce::generate().unwrap().verifier, p.verifier);
    }

    #[test]
    fn profile_validation() {
        assert!(validate_profile("personal").is_ok());
        assert!(validate_profile("personal_2").is_ok());
        for bad in ["", "..\\x", "a/b", "Work", "a b", &"x".repeat(33)] {
            assert_eq!(validate_profile(bad), Err(AuthError::InvalidProfile), "{bad}");
        }
        assert!(client_path("personal").unwrap().ends_with("clockcal\\profiles\\personal.json"));
    }

    #[test]
    fn client_json_parse() {
        let c = client();
        assert_eq!(c.client_id, "fixture-id.apps.example");
        assert_eq!(c.client_secret.as_deref(), Some("fixture-secret"));
        assert_eq!(c.token_uri, "https://oauth2.googleapis.com/token");
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("fixture-id") && !dbg.contains("fixture-secret"));
        assert!(matches!(parse_client_json("{"), Err(AuthError::ClientConfig(_))));
        assert!(matches!(
            parse_client_json(r#"{"web":{"client_id":"x"}}"#),
            Err(AuthError::ClientConfig(_))
        ));
        assert!(matches!(
            parse_client_json(r#"{"installed":{"client_secret":"x"}}"#),
            Err(AuthError::ClientConfig(_))
        ));
        assert!(matches!(
            parse_client_json(r#"{"installed":{"client_id":"x","token_uri":"http://evil"}}"#),
            Err(AuthError::ClientConfig(_))
        ));
        // Defaults when uris omitted; no secret is fine (PKCE).
        let c2 = parse_client_json(r#"{"installed":{"client_id":"x"}}"#).unwrap();
        assert!(c2.client_secret.is_none());
        assert!(c2.auth_uri.starts_with("https://accounts.google.com/"));
    }

    #[test]
    fn auth_url_contents() {
        let u = build_auth_url(&client(), "http://127.0.0.1:5555", "CHAL", "ST-1");
        assert!(u.starts_with("https://accounts.google.com/o/oauth2/auth?"));
        let q = u.split_once('?').unwrap().1;
        let get = |k: &str| {
            q.split('&')
                .find_map(|p| p.strip_prefix(&format!("{k}=")))
                .map(|v| pct_decode(v).unwrap())
        };
        assert_eq!(get("client_id").as_deref(), Some("fixture-id.apps.example"));
        assert_eq!(get("redirect_uri").as_deref(), Some("http://127.0.0.1:5555"));
        assert_eq!(get("response_type").as_deref(), Some("code"));
        assert_eq!(get("scope").as_deref(), Some(SCOPE));
        let scopes: Vec<_> = get("scope").unwrap().split(' ').map(String::from).collect();
        assert_eq!(scopes, vec![SCOPE_EVENTS.to_string(), SCOPE_READONLY.to_string()]);
        assert_eq!(get("code_challenge").as_deref(), Some("CHAL"));
        assert_eq!(get("code_challenge_method").as_deref(), Some("S256"));
        assert_eq!(get("state").as_deref(), Some("ST-1"));
        assert_eq!(get("access_type").as_deref(), Some("offline"));
        assert!(!u.contains("fixture-secret"), "secret must never be in the auth URL");
        assert!(q.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5555"));
    }

    #[test]
    fn scope_constants_consistent() {
        assert_eq!(SCOPE, format!("{SCOPE_EVENTS} {SCOPE_READONLY}"));
    }

    #[test]
    fn can_respond_old_vs_new_tokens() {
        // Old token: readonly only, or no scope recorded at all.
        assert!(!can_respond(None));
        assert!(!can_respond(Some("")));
        assert!(!can_respond(Some(SCOPE_READONLY)));
        // Near-miss strings must not count (exact token match only).
        assert!(!can_respond(Some("https://www.googleapis.com/auth/calendar.events.readonly")));
        assert!(!can_respond(Some("https://www.googleapis.com/auth/calendar.events.owned")));
        // New token.
        assert!(can_respond(Some(SCOPE)));
        assert!(can_respond(Some(&format!("openid {SCOPE_READONLY} {SCOPE_EVENTS}"))));
        assert!(can_respond(Some(SCOPE_EVENTS)));
        assert!(scope_has(Some(SCOPE), SCOPE_READONLY));
    }

    #[test]
    fn status_can_respond_old_and_new() {
        let target = test_target("canresp");
        let _ = token_store::delete_raw(&target);
        let mk = |scope: Option<&str>| StoredToken {
            refresh_token: "RT".into(),
            access_token: None,
            expires_at: None,
            scope: scope.map(String::from),
        };
        token_store::save_at(&target, &mk(Some(SCOPE_READONLY))).unwrap();
        assert!(!status_at("personal", &target, true, 1).unwrap().can_respond);
        token_store::save_at(&target, &mk(None)).unwrap();
        assert!(!status_at("personal", &target, true, 1).unwrap().can_respond);
        token_store::save_at(&target, &mk(Some(SCOPE))).unwrap();
        assert!(status_at("personal", &target, true, 1).unwrap().can_respond);
        assert!(token_store::delete_raw(&target).unwrap());
        // Not logged in.
        assert!(!status_at("personal", &target, true, 1).unwrap().can_respond);
    }

    #[test]
    fn import_client_validates_and_writes() {
        let dir = std::env::temp_dir().join(format!("clockcal-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Rejected inputs write nothing.
        for bad in ["", "{", r#"{"web":{"client_id":"x"}}"#, r#"{"installed":{"client_secret":"x"}}"#] {
            assert!(matches!(import_client_to(&dir, "personal", bad), Err(AuthError::ClientConfig(_))), "{bad}");
        }
        assert!(!dir.join("personal.json").exists());
        assert_eq!(import_client_to(&dir, "../x", FIXTURE), Err(AuthError::InvalidProfile));
        let big = format!("{{\"installed\":{{\"client_id\":\"{}\"}}}}", "a".repeat(MAX_CLIENT_JSON_BYTES));
        assert!(matches!(import_client_to(&dir, "personal", &big), Err(AuthError::ClientConfig(_))));
        // Valid file is stored, and a second import replaces it.
        import_client_to(&dir, "personal", FIXTURE).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("personal.json")).unwrap(), FIXTURE);
        let other = r#"{"installed":{"client_id":"second"}}"#;
        import_client_to(&dir, "personal", other).unwrap();
        assert_eq!(parse_client_json(&std::fs::read_to_string(dir.join("personal.json")).unwrap()).unwrap().client_id, "second");
        assert!(!dir.join("personal.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pct_roundtrip() {
        let s = "4/0Aa b&c=d+é";
        assert_eq!(pct_decode(&pct_encode(s)).as_deref(), Some(s));
        assert_eq!(pct_decode("a+b%2Fc").as_deref(), Some("a b/c"));
        assert_eq!(pct_decode("%zz"), None);
        assert_eq!(pct_decode("%4"), None);
    }

    #[test]
    fn callback_parse_cases() {
        assert_eq!(
            parse_callback("/?state=S1&code=4%2F0AbC&scope=x", "S1").as_deref(),
            Ok("4/0AbC")
        );
        assert_eq!(parse_callback("/?state=S2&code=c", "S1"), Err(AuthError::StateMismatch));
        assert_eq!(parse_callback("/?code=c", "S1"), Err(AuthError::StateMismatch));
        assert_eq!(parse_callback("/", "S1"), Err(AuthError::StateMismatch));
        assert_eq!(
            parse_callback("/?error=access_denied&state=S1", "S1"),
            Err(AuthError::Denied("access_denied".into()))
        );
        // Error with wrong state -> state mismatch wins.
        assert_eq!(
            parse_callback("/?error=access_denied&state=X", "S1"),
            Err(AuthError::StateMismatch)
        );
        assert_eq!(parse_callback("/?state=S1", "S1"), Err(AuthError::MissingCode));
        assert_eq!(parse_callback("/?state=S1&code=", "S1"), Err(AuthError::MissingCode));
        assert_eq!(
            parse_callback("/?error=%3Cscript%3E&state=S1", "S1"),
            Err(AuthError::Denied("script".into()))
        );
        // Duplicate state: first wins.
        assert_eq!(
            parse_callback("/?state=S1&state=S2&code=c", "S1").as_deref(),
            Ok("c")
        );
    }

    #[test]
    fn request_line_parse() {
        assert_eq!(parse_request_line("GET /?a=b HTTP/1.1"), Some("/?a=b"));
        assert_eq!(parse_request_line("POST / HTTP/1.1"), None);
        assert_eq!(parse_request_line("GET /"), None);
        assert_eq!(parse_request_line(""), None);
    }

    #[test]
    fn token_response_parse() {
        let t = parse_token_response(
            200,
            r#"{"access_token":"AT","expires_in":3599,"refresh_token":"RT","scope":"s","token_type":"Bearer"}"#,
        )
        .unwrap();
        assert_eq!((t.access_token.as_str(), t.expires_in), ("AT", 3599));
        assert_eq!(t.refresh_token.as_deref(), Some("RT"));
        assert!(!format!("{t:?}").contains("AT\"") && !format!("{t:?}").contains("RT"));
        let e = parse_token_response(
            400,
            r#"{"error":"invalid_grant","error_description":"SECRET-ISH-DETAIL"}"#,
        )
        .unwrap_err();
        assert!(is_invalid_grant(&e));
        assert!(!e.to_string().contains("SECRET-ISH-DETAIL"));
        assert_eq!(
            parse_token_response(500, "<html>oops</html>").unwrap_err(),
            AuthError::TokenEndpoint { status: 500, error: None }
        );
        assert!(matches!(parse_token_response(200, "{}"), Err(AuthError::Protocol(_))));
        assert!(matches!(
            parse_token_response(200, r#"{"access_token":"","expires_in":1}"#),
            Err(AuthError::Protocol(_))
        ));
    }

    #[test]
    fn expiry_logic() {
        assert!(needs_refresh(None, 1000, 60));
        assert!(needs_refresh(Some(1000), 1000, 60));
        assert!(needs_refresh(Some(1060), 1000, 60));
        assert!(!needs_refresh(Some(1061), 1000, 60));
        assert!(needs_refresh(Some(900), 1000, 60));
        assert!(!needs_refresh(Some(u64::MAX), u64::MAX - 1, 0));
    }

    /// Synthetic GET to the real loopback server.
    fn get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    #[test]
    fn loopback_binds_127_random_port() {
        let a = Loopback::bind().unwrap();
        let b = Loopback::bind().unwrap();
        assert!(a.local_ip_is_loopback());
        assert_ne!(a.port(), 0);
        assert_ne!(a.port(), b.port());
        assert_eq!(a.redirect_uri(), format!("http://127.0.0.1:{}", a.port()));
    }

    #[test]
    fn loopback_wrong_state_rejected() {
        let lb = Loopback::bind().unwrap();
        let port = lb.port();
        let h = std::thread::spawn(move || lb.wait_for_code("GOOD", Duration::from_secs(10)));
        let resp = get(port, "/?state=BAD&code=c");
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        assert_eq!(h.join().unwrap(), Err(AuthError::StateMismatch));
        // Server is gone after one request.
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    #[test]
    fn loopback_ok_after_favicon_and_closes() {
        let lb = Loopback::bind().unwrap();
        let port = lb.port();
        let h = std::thread::spawn(move || lb.wait_for_code("GOOD", Duration::from_secs(10)));
        let fav = get(port, "/favicon.ico");
        assert!(fav.starts_with("HTTP/1.1 404"));
        let resp = get(port, "/?state=GOOD&code=abc");
        assert!(resp.starts_with("HTTP/1.1 200"));
        assert!(resp.contains("clockcal 로그인 완료, 창을 닫아도 됩니다"));
        assert_eq!(h.join().unwrap().as_deref(), Ok("abc"));
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    }

    #[test]
    fn loopback_access_denied() {
        let lb = Loopback::bind().unwrap();
        let port = lb.port();
        let h = std::thread::spawn(move || lb.wait_for_code("GOOD", Duration::from_secs(10)));
        let resp = get(port, "/?error=access_denied&state=GOOD");
        assert!(resp.contains("로그인 실패"));
        assert_eq!(h.join().unwrap(), Err(AuthError::Denied("access_denied".into())));
    }

    #[test]
    fn loopback_timeout() {
        let lb = Loopback::bind().unwrap();
        let t0 = Instant::now();
        assert_eq!(lb.wait_for_code("S", Duration::from_millis(300)), Err(AuthError::Timeout));
        assert!(t0.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn login_flow_end_to_end_mocked() {
        let target = test_target("login");
        let _ = token_store::delete_raw(&target);
        let mock = Mock::new(vec![(
            200,
            r#"{"access_token":"AT-1","expires_in":3599,"refresh_token":"RT-1","scope":"https://www.googleapis.com/auth/calendar.events https://www.googleapis.com/auth/calendar.readonly"}"#,
        )]);
        // Fake "browser": parse the auth URL and hit the loopback with the right state.
        let browser = |url: &str| -> Result<(), AuthError> {
            let q = url.split_once('?').unwrap().1.to_string();
            let get_q = move |k: &str| {
                q.split('&')
                    .find_map(|p| p.strip_prefix(&format!("{k}=")))
                    .map(|v| pct_decode(v).unwrap())
                    .unwrap()
            };
            let redirect = get_q("redirect_uri");
            let state = get_q("state");
            let port: u16 = redirect.rsplit(':').next().unwrap().parse().unwrap();
            std::thread::spawn(move || get(port, &format!("/?state={state}&code=CODE%2F1")));
            Ok(())
        };
        let info = login_with(&target, &client(), &mock, &browser, Duration::from_secs(10)).unwrap();
        assert_eq!(info.expires_in_s, 3599);
        assert_eq!(mock.field(0, "code").as_deref(), Some("CODE/1"));
        assert_eq!(mock.field(0, "grant_type").as_deref(), Some("authorization_code"));
        assert_eq!(mock.field(0, "code_verifier").map(|v| v.len()), Some(43));
        assert!(mock.field(0, "redirect_uri").unwrap().starts_with("http://127.0.0.1:"));
        let st = token_store::load_at(&target).unwrap().unwrap();
        assert_eq!(st.refresh_token, "RT-1");
        assert_eq!(st.access_token.as_deref(), Some("AT-1"));
        assert!(token_store::delete_raw(&target).unwrap());
    }

    #[test]
    fn login_flow_missing_refresh_token_fails_and_stores_nothing() {
        let target = test_target("norefresh");
        let _ = token_store::delete_raw(&target);
        let mock = Mock::new(vec![(200, r#"{"access_token":"AT","expires_in":10}"#)]);
        let browser = |url: &str| -> Result<(), AuthError> {
            let q = url.split_once('?').unwrap().1.to_string();
            let f = |k: &str| {
                q.split('&')
                    .find_map(|p| p.strip_prefix(&format!("{k}=")))
                    .map(|v| pct_decode(v).unwrap())
                    .unwrap()
            };
            let port: u16 = f("redirect_uri").rsplit(':').next().unwrap().parse().unwrap();
            let state = f("state");
            std::thread::spawn(move || get(port, &format!("/?state={state}&code=c")));
            Ok(())
        };
        let r = login_with(&target, &client(), &mock, &browser, Duration::from_secs(10));
        assert!(matches!(r, Err(AuthError::Protocol(_))));
        assert_eq!(token_store::load_at(&target).unwrap(), None);
    }

    #[test]
    fn access_token_cached_refresh_and_invalid_grant() {
        let target = test_target("access");
        let _ = token_store::delete_raw(&target);
        let c = client();
        let cfg = || Ok(c.clone());

        // Not logged in.
        let mock = Mock::new(vec![]);
        assert_eq!(access_token_with(&target, &cfg, &mock, 1000), Err(AuthError::NeedLogin));

        // Valid cached token -> no network.
        token_store::save_at(
            &target,
            &StoredToken {
                refresh_token: "RT".into(),
                access_token: Some("AT-old".into()),
                expires_at: Some(2000),
                scope: None,
            },
        )
        .unwrap();
        let no_client = || -> Result<ClientConfig, AuthError> { panic!("client must not load") };
        assert_eq!(
            access_token_with(&target, &no_client, &mock, 1000),
            Ok(("AT-old".into(), 1000))
        );
        assert!(mock.calls.borrow().is_empty());

        // Within skew -> refresh.
        let mock = Mock::new(vec![(200, r#"{"access_token":"AT-new","expires_in":3599}"#)]);
        assert_eq!(
            access_token_with(&target, &cfg, &mock, 1950),
            Ok(("AT-new".into(), 3599))
        );
        assert_eq!(mock.field(0, "grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(mock.field(0, "refresh_token").as_deref(), Some("RT"));
        let st = token_store::load_at(&target).unwrap().unwrap();
        assert_eq!(st.refresh_token, "RT");
        assert_eq!(st.expires_at, Some(1950 + 3599));

        // Server error other than invalid_grant -> token kept.
        let mock = Mock::new(vec![(503, "busy")]);
        assert!(matches!(
            access_token_with(&target, &cfg, &mock, 99_999),
            Err(AuthError::TokenEndpoint { status: 503, .. })
        ));
        assert!(token_store::load_at(&target).unwrap().is_some());

        // invalid_grant -> deleted, NeedLogin.
        let mock = Mock::new(vec![(400, r#"{"error":"invalid_grant"}"#)]);
        assert_eq!(access_token_with(&target, &cfg, &mock, 99_999), Err(AuthError::NeedLogin));
        assert_eq!(token_store::load_at(&target).unwrap(), None);
    }

    #[test]
    fn status_reports_no_secrets() {
        let target = test_target("status");
        let _ = token_store::delete_raw(&target);
        let s = status_at("personal", &target, true, 100).unwrap();
        assert!(!s.logged_in && s.access_expires_at.is_none());
        token_store::save_at(
            &target,
            &StoredToken {
                refresh_token: "RT-S".into(),
                access_token: Some("AT-S".into()),
                expires_at: Some(400),
                scope: Some(SCOPE.into()),
            },
        )
        .unwrap();
        let s = status_at("personal", &target, true, 100).unwrap();
        assert!(s.logged_in);
        assert_eq!(s.access_expires_in_s, Some(300));
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("RT-S") && !json.contains("AT-S"));
        assert!(token_store::delete_raw(&target).unwrap());
    }

    /// Optional negative test against the real Google token endpoint with a
    /// fake client and fake code. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn real_endpoint_fake_code_is_handled() {
        let c = parse_client_json(r#"{"installed":{"client_id":"clockcal-fake-client"}}"#).unwrap();
        let t = HttpTransport::new().unwrap();
        let e = exchange_code(&c, &t, "fake-code", "x".repeat(43).as_str(), "http://127.0.0.1:1")
            .unwrap_err();
        eprintln!("real endpoint error (sanitised): {e}");
        assert!(matches!(e, AuthError::TokenEndpoint { status: 400..=499, .. }));
    }
}
