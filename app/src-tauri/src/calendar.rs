//! P3-b: read-only Google Calendar data layer + month-unit local cache.
//!
//! - `calendarList.list` and `events.list(singleEvents=true)` over HTTPS with the
//!   access token from `oauth::access_token` (401 -> refresh once -> retry once;
//!   429/5xx -> one backoff retry; then a typed error, no retry storm).
//! - Cache: `%APPDATA%\clockcal\cache\<profile>\YYYY-MM.json` + `calendars.json`
//!   (event data only, never tokens), atomic write (temp + rename), TTL 5 min,
//!   stale fallback when the fetch fails.
//!
//! SECURITY: never log tokens, URLs with tokens, response bodies or event titles.
//! Logs may contain: profile, counts, HTTP status, timing, masked calendar ids.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Local, NaiveDate, SecondsFormat, TimeZone};
use serde::{Deserialize, Serialize};

use crate::log;
use crate::oauth::{self, pct_encode, unix_now, AuthError};
use crate::token_store;

pub const API_BASE: &str = "https://www.googleapis.com/calendar/v3";
/// A cached month younger than this is returned without any network call.
pub const CACHE_TTL_S: u64 = 300;
/// v2: events carry `response` (own RSVP); v1 month files are refetched.
const CACHE_VERSION: u32 = 4;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const BACKOFF: Duration = Duration::from_millis(1500);
const MAX_PAGES: usize = 20;
const NO_TITLE: &str = "(제목 없음)";
/// Shown like Google Calendar does for events whose title is hidden from us (private / free-busy only).
const BUSY_TITLE: &str = "바쁨";

// ---------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalError {
    /// No usable login (never logged in or revoked).
    NeedLogin,
    /// 401 persisted after one token refresh.
    Unauthorized,
    /// 403 that is not a rate limit (API disabled, no access to the calendar).
    Forbidden,
    /// 429 or 403 rateLimitExceeded / userRateLimitExceeded, after one backoff retry.
    RateLimited,
    /// 5xx after one backoff retry.
    Server(u16),
    /// Other non-2xx.
    Http(u16),
    /// Transport failure (kind only).
    Network(&'static str),
    /// 2xx with an unusable body.
    Parse(&'static str),
    Auth(String),
    BadArgument(&'static str),
    Io(&'static str),
    /// Write attempted without calendar.events (old token, or Google says scope is insufficient).
    NeedsReauth,
    /// The user is not an attendee of the event (no `self` attendee).
    NotAttendee,
    /// If-Match precondition (412) failed again after one re-read.
    Conflict,
}

impl Display for CalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalError::NeedLogin => write!(f, "need login"),
            CalError::Unauthorized => write!(f, "unauthorized (401) after token refresh"),
            CalError::Forbidden => write!(f, "forbidden (403)"),
            CalError::RateLimited => write!(f, "rate limited (429/403 rate limit)"),
            CalError::Server(s) => write!(f, "google server error (http {s})"),
            CalError::Http(s) => write!(f, "http {s}"),
            CalError::Network(k) => write!(f, "network error: {k}"),
            CalError::Parse(s) => write!(f, "parse error: {s}"),
            CalError::Auth(s) => write!(f, "auth error: {s}"),
            CalError::BadArgument(s) => write!(f, "bad argument: {s}"),
            CalError::Io(s) => write!(f, "io: {s}"),
            CalError::NeedsReauth => write!(f, "needs re-login (calendar.events scope)"),
            CalError::NotAttendee => write!(f, "not an attendee of this event"),
            CalError::Conflict => write!(f, "event changed concurrently (412)"),
        }
    }
}

impl CalError {
    /// Short stable code for the UI (never contains secrets or event data).
    pub fn code(&self) -> String {
        match self {
            CalError::NeedsReauth | CalError::Unauthorized => "needs_reauth".into(),
            CalError::NeedLogin => "need_login".into(),
            CalError::NotAttendee => "not_attendee".into(),
            CalError::Conflict => "conflict".into(),
            CalError::Forbidden => "forbidden".into(),
            CalError::RateLimited => "rate_limited".into(),
            CalError::Server(_) => "server_error".into(),
            CalError::Http(404) => "not_found".into(),
            CalError::Http(s) => format!("http_{s}"),
            CalError::Network(_) => "network".into(),
            CalError::Parse(_) => "parse".into(),
            CalError::Auth(_) => "auth_error".into(),
            CalError::BadArgument(_) => "bad_argument".into(),
            CalError::Io(_) => "io".into(),
        }
    }
}

impl std::error::Error for CalError {}

impl From<AuthError> for CalError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::NeedLogin => CalError::NeedLogin,
            AuthError::InvalidProfile => CalError::BadArgument("invalid profile name"),
            other => CalError::Auth(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------- data model

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarInfo {
    pub id: String,
    pub summary: String,
    pub background_color: Option<String>,
    pub foreground_color: Option<String>,
    pub primary: bool,
    pub selected: bool,
    pub access_role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub calendar_id: String,
    pub calendar_name: String,
    pub calendar_color: Option<String>,
    pub summary: String,
    /// RFC 3339 date-time, or `YYYY-MM-DD` when `all_day`.
    pub start: String,
    /// Same format as `start`. For all-day events this date is EXCLUSIVE.
    pub end: String,
    pub all_day: bool,
    pub location: Option<String>,
    /// hangoutLink, else the conferenceData video entry point.
    pub meet_url: Option<String>,
    pub html_link: Option<String>,
    pub status: String,
    /// The user's own RSVP: "accepted" | "needsAction" | "tentative" | "declined".
    /// "accepted" when there is no self attendee. Old cache files default to it.
    #[serde(default = "default_response")]
    pub response: String,
    /// True when the user is an invited attendee (self attendee exists and the user
    /// is not the organizer) -> the UI may offer RSVP. Old cache files default to false.
    #[serde(default)]
    pub self_attendee: bool,
}

#[derive(Deserialize)]
struct RawOrganizer {
    #[serde(default, rename = "self")]
    is_self: bool,
}

fn default_response() -> String {
    "accepted".to_string()
}

#[derive(Deserialize)]
struct RawAttendee {
    #[serde(default, rename = "self")]
    is_self: bool,
    #[serde(rename = "responseStatus")]
    response_status: Option<String>,
}

/// Own RSVP from `attendees[]`; anything missing or unknown maps to "accepted".
fn self_response(attendees: &[RawAttendee]) -> String {
    match attendees.iter().find(|a| a.is_self).and_then(|a| a.response_status.as_deref()) {
        Some(s @ ("needsAction" | "tentative" | "declined" | "accepted")) => s.to_string(),
        _ => default_response(),
    }
}

// ---------------------------------------------------------------- parsing

#[derive(Deserialize)]
struct RawCalendarList {
    #[serde(default)]
    items: Vec<RawCalendar>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct RawCalendar {
    id: Option<String>,
    summary: Option<String>,
    #[serde(rename = "summaryOverride")]
    summary_override: Option<String>,
    #[serde(rename = "backgroundColor")]
    background_color: Option<String>,
    #[serde(rename = "foregroundColor")]
    foreground_color: Option<String>,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    selected: bool,
    #[serde(rename = "accessRole")]
    access_role: Option<String>,
}

/// One page of calendarList. Returns (calendars, nextPageToken).
pub fn parse_calendar_list(body: &str) -> Result<(Vec<CalendarInfo>, Option<String>), CalError> {
    let raw: RawCalendarList =
        serde_json::from_str(body).map_err(|_| CalError::Parse("calendarList json"))?;
    let items = raw
        .items
        .into_iter()
        .filter_map(|c| {
            let id = c.id?;
            let summary = c.summary_override.or(c.summary).unwrap_or_else(|| id.clone());
            Some(CalendarInfo {
                id,
                summary,
                background_color: c.background_color,
                foreground_color: c.foreground_color,
                primary: c.primary,
                selected: c.selected,
                access_role: c.access_role,
            })
        })
        .collect();
    Ok((items, raw.next_page_token.filter(|t| !t.is_empty())))
}

#[derive(Deserialize)]
struct RawEvents {
    #[serde(default)]
    items: Vec<RawEvent>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct RawEvent {
    id: Option<String>,
    status: Option<String>,
    summary: Option<String>,
    visibility: Option<String>,
    location: Option<String>,
    #[serde(rename = "htmlLink")]
    html_link: Option<String>,
    #[serde(rename = "hangoutLink")]
    hangout_link: Option<String>,
    #[serde(rename = "conferenceData")]
    conference_data: Option<RawConference>,
    start: Option<RawTime>,
    end: Option<RawTime>,
    #[serde(default)]
    attendees: Vec<RawAttendee>,
    organizer: Option<RawOrganizer>,
}

#[derive(Deserialize)]
struct RawTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    date: Option<String>,
}

#[derive(Deserialize)]
struct RawConference {
    #[serde(default, rename = "entryPoints")]
    entry_points: Vec<RawEntryPoint>,
}

#[derive(Deserialize)]
struct RawEntryPoint {
    #[serde(rename = "entryPointType")]
    entry_point_type: Option<String>,
    uri: Option<String>,
}

/// (value, all_day)
fn time_value(t: &Option<RawTime>) -> Option<(String, bool)> {
    let t = t.as_ref()?;
    if let Some(dt) = &t.date_time {
        Some((dt.clone(), false))
    } else {
        t.date.as_ref().map(|d| (d.clone(), true))
    }
}

/// Cancelled entries and entries without usable start/id yield None.
fn event_from_raw(e: RawEvent, cal: &CalendarInfo) -> Option<Event> {
    let status = e.status.clone().unwrap_or_else(|| "confirmed".into());
    if status == "cancelled" {
        return None;
    }
    let (Some(id), Some((start, all_day))) = (e.id.clone(), time_value(&e.start)) else {
        return None;
    };
    let end = time_value(&e.end).map(|(v, _)| v).unwrap_or_else(|| start.clone());
    let meet_url = e.hangout_link.clone().filter(|s| !s.is_empty()).or_else(|| {
        e.conference_data.as_ref().and_then(|c| {
            c.entry_points
                .iter()
                .find(|p| p.entry_point_type.as_deref() == Some("video"))
                .and_then(|p| p.uri.clone())
        })
    });
    Some(Event {
        id,
        calendar_id: cal.id.clone(),
        calendar_name: cal.summary.clone(),
        calendar_color: cal.background_color.clone(),
        summary: e
            .summary
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                let hidden = matches!(e.visibility.as_deref(), Some("private") | Some("confidential"))
                    || cal.access_role.as_deref() == Some("freeBusyReader");
                (if hidden { BUSY_TITLE } else { NO_TITLE }).to_string()
            }),
        start,
        end,
        all_day,
        location: e.location.clone().filter(|s| !s.is_empty()),
        meet_url,
        html_link: e.html_link.clone(),
        status,
        response: self_response(&e.attendees),
        // `attendees[].self` means "this calendar", not "the signed-in user": only trust it on our own primary calendar.
        self_attendee: cal.primary
            && e.attendees.iter().any(|a| a.is_self)
            && !e.organizer.as_ref().is_some_and(|o| o.is_self),
    })
}

/// One page of events for `cal`. Cancelled entries and entries without usable
/// start/id are dropped. Returns (events, nextPageToken).
pub fn parse_events(body: &str, cal: &CalendarInfo) -> Result<(Vec<Event>, Option<String>), CalError> {
    let raw: RawEvents = serde_json::from_str(body).map_err(|_| CalError::Parse("events json"))?;
    let mut out = Vec::with_capacity(raw.items.len());
    for e in raw.items {
        if let Some(ev) = event_from_raw(e, cal) {
            out.push(ev);
        }
    }
    Ok((out, raw.next_page_token.filter(|t| !t.is_empty())))
}

/// Google error reason (e.g. `rateLimitExceeded`) from an error body, if any.
fn error_reason(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("error")?
        .get("errors")?
        .get(0)?
        .get("reason")?
        .as_str()
        .map(str::to_string)
}

/// Map a non-2xx response to a typed error.
pub fn classify_status(status: u16, body: &str) -> CalError {
    match status {
        401 => CalError::Unauthorized,
        429 => CalError::RateLimited,
        403 => match error_reason(body).as_deref() {
            Some("rateLimitExceeded") | Some("userRateLimitExceeded") | Some("quotaExceeded") => {
                CalError::RateLimited
            }
            Some("insufficientPermissions") => CalError::NeedsReauth,
            _ if body.contains("ACCESS_TOKEN_SCOPE_INSUFFICIENT") => CalError::NeedsReauth,
            _ => CalError::Forbidden,
        },
        500..=599 => CalError::Server(status),
        s => CalError::Http(s),
    }
}

// ---------------------------------------------------------------- month window

/// Local-time month boundaries as RFC 3339 `(timeMin, timeMax)`; timeMax is the
/// first instant of the next month (exclusive in the Calendar API).
pub fn month_window<Tz: TimeZone>(tz: &Tz, year: i32, month: u32) -> Option<(String, String)>
where
    Tz::Offset: Display,
{
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    let at = |d: NaiveDate| -> Option<String> {
        // Midnight can be skipped by a DST jump: fall forward to the first valid hour.
        for h in 0..4u32 {
            let naive = d.and_hms_opt(h, 0, 0)?;
            if let Some(dt) = tz.from_local_datetime(&naive).earliest() {
                return Some(dt.to_rfc3339_opts(SecondsFormat::Secs, false));
            }
        }
        None
    };
    Some((at(first)?, at(next)?))
}

pub fn validate_month(year: i32, month: u32) -> Result<(), CalError> {
    if (2000..=2100).contains(&year) && (1..=12).contains(&month) {
        Ok(())
    } else {
        Err(CalError::BadArgument("year/month out of range"))
    }
}

// ---------------------------------------------------------------- transport + auth

/// GET with bearer token, returns (status, body).
pub trait Http {
    fn get(&self, url: &str, bearer: &str) -> Result<(u16, String), CalError>;
    /// PATCH with JSON body and `If-Match`, returns (status, body).
    fn patch(&self, url: &str, bearer: &str, if_match: &str, body: &str) -> Result<(u16, String), CalError>;
}

/// (status, body)
type HttpReply = Result<(u16, String), CalError>;

fn net_err(e: reqwest::Error) -> CalError {
    CalError::Network(if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect failed"
    } else {
        "request failed"
    })
}

/// `force = true` must discard any cached access token and refresh.
pub trait TokenSource {
    fn token(&self, force: bool) -> Result<String, CalError>;
}

pub struct ReqwestHttp {
    client: reqwest::blocking::Client,
}

impl ReqwestHttp {
    pub fn new() -> Result<Self, CalError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .https_only(true)
            .build()
            .map_err(|_| CalError::Network("client build failed"))?;
        Ok(Self { client })
    }
}

impl Http for ReqwestHttp {
    fn get(&self, url: &str, bearer: &str) -> Result<(u16, String), CalError> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(bearer)
            .header("Accept", "application/json")
            .send()
            .map_err(|e| {
                CalError::Network(if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect failed"
                } else {
                    "request failed"
                })
            })?;
        let status = resp.status().as_u16();
        let body = resp.text().map_err(|_| CalError::Network("read body failed"))?;
        Ok((status, body))
    }

    fn patch(&self, url: &str, bearer: &str, if_match: &str, body: &str) -> Result<(u16, String), CalError> {
        let resp = self
            .client
            .patch(url)
            .bearer_auth(bearer)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("If-Match", if_match)
            .body(body.to_string())
            .send()
            .map_err(net_err)?;
        let status = resp.status().as_u16();
        let text = resp.text().map_err(|_| CalError::Network("read body failed"))?;
        Ok((status, text))
    }
}

/// Tokens from `oauth` (Credential Manager backed).
pub struct OAuthTokens {
    pub profile: String,
}

impl TokenSource for OAuthTokens {
    fn token(&self, force: bool) -> Result<String, CalError> {
        if force {
            // Drop the cached access token so oauth::access_token refreshes.
            if let Ok(Some(mut st)) = token_store::load(&self.profile) {
                st.access_token = None;
                st.expires_at = None;
                let _ = token_store::save(&self.profile, &st);
            }
        }
        oauth::access_token(&self.profile).map_err(CalError::from)
    }
}

/// Authenticated GET client implementing the retry policy.
pub struct Api<'a> {
    pub http: &'a dyn Http,
    pub tokens: &'a dyn TokenSource,
    pub backoff: Duration,
}

impl<'a> Api<'a> {
    pub fn new(http: &'a dyn Http, tokens: &'a dyn TokenSource) -> Self {
        Self { http, tokens, backoff: BACKOFF }
    }

    /// GET returning the body of a 2xx response.
    /// 401 -> refresh token once and retry once; 429/5xx -> one backoff retry.
    pub fn get_json(&self, url: &str) -> Result<String, CalError> {
        self.call(&|token| self.http.get(url, token))
    }

    /// PATCH returning the body of a 2xx response; same retry policy as GET.
    /// A 412 comes back as `CalError::Http(412)`.
    pub fn patch_json(&self, url: &str, if_match: &str, body: &str) -> Result<String, CalError> {
        self.call(&|token| self.http.patch(url, token, if_match, body))
    }

    fn call(&self, f: &dyn Fn(&str) -> HttpReply) -> Result<String, CalError> {
        let mut token = self.tokens.token(false)?;
        let mut refreshed = false;
        let mut backed_off = false;
        loop {
            let (status, body) = f(&token)?;
            if (200..300).contains(&status) {
                return Ok(body);
            }
            let err = classify_status(status, &body);
            match err {
                CalError::Unauthorized if !refreshed => {
                    refreshed = true;
                    token = self.tokens.token(true)?;
                }
                CalError::RateLimited | CalError::Server(_) if !backed_off => {
                    backed_off = true;
                    if !self.backoff.is_zero() {
                        std::thread::sleep(self.backoff);
                    }
                }
                e => return Err(e),
            }
        }
    }

    pub fn fetch_calendars(&self) -> Result<Vec<CalendarInfo>, CalError> {
        let mut all = Vec::new();
        let mut page: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = format!("{API_BASE}/users/me/calendarList?maxResults=250");
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", pct_encode(p)));
            }
            let (items, next) = parse_calendar_list(&self.get_json(&url)?)?;
            all.extend(items);
            match next {
                Some(n) => page = Some(n),
                None => return Ok(all),
            }
        }
        Err(CalError::Parse("too many calendarList pages"))
    }

    pub fn fetch_events(
        &self,
        cal: &CalendarInfo,
        time_min: &str,
        time_max: &str,
    ) -> Result<Vec<Event>, CalError> {
        let mut all = Vec::new();
        let mut page: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = format!(
                "{API_BASE}/calendars/{}/events?singleEvents=true&orderBy=startTime&maxResults=250&timeMin={}&timeMax={}",
                pct_encode(&cal.id),
                pct_encode(time_min),
                pct_encode(time_max)
            );
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", pct_encode(p)));
            }
            let (items, next) = parse_events(&self.get_json(&url)?, cal)?;
            all.extend(items);
            match next {
                Some(n) => page = Some(n),
                None => return Ok(all),
            }
        }
        Err(CalError::Parse("too many events pages"))
    }
}

// ---------------------------------------------------------------- cache

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedMonth {
    pub version: u32,
    pub fetched_at: u64,
    pub year: i32,
    pub month: u32,
    pub events: Vec<Event>,
    /// Calendars skipped (403/404) while fetching, masked ids.
    #[serde(default)]
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCalendars {
    pub version: u32,
    pub fetched_at: u64,
    pub calendars: Vec<CalendarInfo>,
}

pub struct Cache {
    dir: PathBuf,
}

pub fn cache_root() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("clockcal").join("cache"))
}

pub fn is_fresh(fetched_at: u64, now: u64, ttl: u64) -> bool {
    // fetched_at in the future (clock moved back) is not trusted.
    fetched_at <= now && now - fetched_at < ttl
}

impl Cache {
    /// `root` = directory that contains one folder per profile.
    pub fn new(root: &Path, profile: &str) -> Self {
        Self { dir: root.join(profile) }
    }

    pub fn for_profile(profile: &str) -> Result<Self, CalError> {
        oauth::validate_profile(profile)?;
        let root = cache_root().ok_or(CalError::Io("APPDATA not set"))?;
        Ok(Self::new(&root, profile))
    }

    fn month_path(&self, year: i32, month: u32) -> PathBuf {
        self.dir.join(format!("{year:04}-{month:02}.json"))
    }

    fn calendars_path(&self) -> PathBuf {
        self.dir.join("calendars.json")
    }

    /// Temp file in the same directory, then rename over the target.
    fn write_atomic(&self, path: &Path, data: &[u8]) -> Result<(), CalError> {
        std::fs::create_dir_all(&self.dir).map_err(|_| CalError::Io("create cache dir"))?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, data).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err(CalError::Io("write cache temp"));
        }
        std::fs::rename(&tmp, path).map_err(|_| {
            let _ = std::fs::remove_file(&tmp);
            CalError::Io("rename cache file")
        })
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, path: &Path) -> Option<T> {
        let s = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&s).ok()
    }

    pub fn load_month(&self, year: i32, month: u32) -> Option<CachedMonth> {
        self.read_json::<CachedMonth>(&self.month_path(year, month))
            .filter(|c| c.version == CACHE_VERSION && c.year == year && c.month == month)
    }

    pub fn save_month(&self, m: &CachedMonth) -> Result<(), CalError> {
        let data = serde_json::to_vec(m).map_err(|_| CalError::Io("serialize cache"))?;
        self.write_atomic(&self.month_path(m.year, m.month), &data)
    }

    pub fn load_calendars(&self) -> Option<CachedCalendars> {
        self.read_json::<CachedCalendars>(&self.calendars_path())
            .filter(|c| c.version == CACHE_VERSION)
    }

    pub fn save_calendars(&self, c: &CachedCalendars) -> Result<(), CalError> {
        let data = serde_json::to_vec(c).map_err(|_| CalError::Io("serialize cache"))?;
        self.write_atomic(&self.calendars_path(), &data)
    }
}

// ---------------------------------------------------------------- high-level API

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Cache,
    Fetched,
    Stale,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonthResult {
    pub profile: String,
    pub year: i32,
    pub month: u32,
    pub fetched_at: u64,
    pub source: Source,
    pub stale: bool,
    /// Why the fetch failed (only when `stale`). Never contains secrets.
    pub error: Option<String>,
    /// Selected calendars (for colors / legend).
    pub calendars: Vec<CalendarInfo>,
    pub events: Vec<Event>,
    pub skipped_calendars: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalendarsResult {
    pub profile: String,
    pub fetched_at: u64,
    pub source: Source,
    pub stale: bool,
    pub error: Option<String>,
    pub calendars: Vec<CalendarInfo>,
}

pub fn mask_id(id: &str) -> String {
    let head: String = id.chars().take(6).collect();
    format!("{head}...")
}

fn selected_only(all: &[CalendarInfo], include_unselected: bool) -> Vec<CalendarInfo> {
    all.iter()
        .filter(|c| include_unselected || c.selected)
        .cloned()
        .collect()
}

/// Calendar list: fresh cache, else fetch, else stale cache.
pub fn get_calendars_with(
    api: &Api,
    cache: &Cache,
    profile: &str,
    force: bool,
    include_unselected: bool,
    now: u64,
) -> Result<CalendarsResult, CalError> {
    let cached = cache.load_calendars();
    if let (Some(c), false) = (&cached, force) {
        if is_fresh(c.fetched_at, now, CACHE_TTL_S) {
            return Ok(CalendarsResult {
                profile: profile.into(),
                fetched_at: c.fetched_at,
                source: Source::Cache,
                stale: false,
                error: None,
                calendars: selected_only(&c.calendars, include_unselected),
            });
        }
    }
    match api.fetch_calendars() {
        Ok(list) => {
            let c = CachedCalendars { version: CACHE_VERSION, fetched_at: now, calendars: list };
            if let Err(e) = cache.save_calendars(&c) {
                log!("[cal] calendars cache write failed: {e}");
            }
            Ok(CalendarsResult {
                profile: profile.into(),
                fetched_at: now,
                source: Source::Fetched,
                stale: false,
                error: None,
                calendars: selected_only(&c.calendars, include_unselected),
            })
        }
        Err(e) => match (cached, &e) {
            (_, CalError::NeedLogin) | (None, _) => Err(e),
            (Some(c), _) => Ok(CalendarsResult {
                profile: profile.into(),
                fetched_at: c.fetched_at,
                source: Source::Stale,
                stale: true,
                error: Some(e.to_string()),
                calendars: selected_only(&c.calendars, include_unselected),
            }),
        },
    }
}

/// Fetch all selected calendars' events for the window. Per-calendar 403/404 is
/// skipped (listed in `skipped`); every other error aborts.
fn fetch_month_events(
    api: &Api,
    calendars: &[CalendarInfo],
    time_min: &str,
    time_max: &str,
) -> Result<(Vec<Event>, Vec<String>), CalError> {
    let mut events = Vec::new();
    let mut skipped = Vec::new();
    for cal in calendars {
        match api.fetch_events(cal, time_min, time_max) {
            Ok(mut v) => events.append(&mut v),
            Err(CalError::Forbidden) | Err(CalError::Http(404)) => skipped.push(mask_id(&cal.id)),
            Err(e) => return Err(e),
        }
    }
    events.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.id.cmp(&b.id)));
    Ok((events, skipped))
}

/// Month data: fresh cache, else fetch, else stale cache (flagged).
#[allow(clippy::too_many_arguments)]
pub fn get_month_with<Tz: TimeZone>(
    api: &Api,
    cache: &Cache,
    tz: &Tz,
    profile: &str,
    year: i32,
    month: u32,
    force: bool,
    now: u64,
) -> Result<MonthResult, CalError>
where
    Tz::Offset: Display,
{
    validate_month(year, month)?;
    let cached = cache.load_month(year, month);
    let cals_cached = cache.load_calendars();
    let build = |m: &CachedMonth, source: Source, error: Option<String>, cals: Vec<CalendarInfo>| MonthResult {
        profile: profile.into(),
        year,
        month,
        fetched_at: m.fetched_at,
        source,
        stale: source == Source::Stale,
        error,
        calendars: cals,
        events: m.events.clone(),
        skipped_calendars: m.skipped.clone(),
    };
    if let (Some(m), false) = (&cached, force) {
        if is_fresh(m.fetched_at, now, CACHE_TTL_S) {
            let cals = cals_cached.map(|c| selected_only(&c.calendars, false)).unwrap_or_default();
            return Ok(build(m, Source::Cache, None, cals));
        }
    }

    let fetched = (|| -> Result<(CachedMonth, Vec<CalendarInfo>), CalError> {
        let (tmin, tmax) = month_window(tz, year, month).ok_or(CalError::BadArgument("month window"))?;
        let cl = get_calendars_with(api, cache, profile, force, false, now)?;
        let (events, skipped) = fetch_month_events(api, &cl.calendars, &tmin, &tmax)?;
        Ok((
            CachedMonth { version: CACHE_VERSION, fetched_at: now, year, month, events, skipped },
            cl.calendars,
        ))
    })();
    match fetched {
        Ok((m, cals)) => {
            if let Err(e) = cache.save_month(&m) {
                log!("[cal] month cache write failed: {e}");
            }
            Ok(build(&m, Source::Fetched, None, cals))
        }
        Err(e) => match (&cached, &e) {
            (_, CalError::NeedLogin) | (None, _) => Err(e),
            (Some(m), _) => {
                let cals = cals_cached.map(|c| selected_only(&c.calendars, false)).unwrap_or_default();
                Ok(build(m, Source::Stale, Some(e.to_string()), cals))
            }
        },
    }
}

/// Production entry: real HTTP, Credential Manager tokens, `%APPDATA%` cache, local time zone.
pub fn get_month(profile: &str, year: i32, month: u32, force: bool) -> Result<MonthResult, CalError> {
    validate_month(year, month)?;
    let cache = Cache::for_profile(profile)?;
    let http = ReqwestHttp::new()?;
    let tokens = OAuthTokens { profile: profile.to_string() };
    let api = Api::new(&http, &tokens);
    let t0 = std::time::Instant::now();
    let r = get_month_with(&api, &cache, &Local, profile, year, month, force, unix_now())?;
    // Cache hits are silent: the notifier polls every 20 s and would flood the (unrotated) log.
    if r.source != Source::Cache {
        log!(
            "[cal] get_month profile={profile} {year:04}-{month:02} source={:?} events={} elapsed={}ms",
            r.source,
            r.events.len(),
            t0.elapsed().as_millis()
        );
    }
    Ok(r)
}

pub fn get_calendars(profile: &str, force: bool) -> Result<CalendarsResult, CalError> {
    let cache = Cache::for_profile(profile)?;
    let http = ReqwestHttp::new()?;
    let tokens = OAuthTokens { profile: profile.to_string() };
    let api = Api::new(&http, &tokens);
    get_calendars_with(&api, &cache, profile, force, false, unix_now())
}

// ---------------------------------------------------------------- respond to invitation (C-13)

/// Exactly "accepted" | "declined" | "tentative" (never "needsAction").
pub fn validate_response(r: &str) -> Result<&'static str, CalError> {
    match r {
        "accepted" => Ok("accepted"),
        "declined" => Ok("declined"),
        "tentative" => Ok("tentative"),
        _ => Err(CalError::BadArgument("response")),
    }
}

/// Non-empty, at most 1024 bytes, no control characters.
pub fn validate_id(s: &str, what: &'static str) -> Result<(), CalError> {
    if s.is_empty() || s == "." || s == ".." || s.len() > 1024 || s.chars().any(|c| c.is_control()) {
        Err(CalError::BadArgument(what))
    } else {
        Ok(())
    }
}

/// PATCH request derived from an `events.get` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespondPatch {
    /// For the If-Match header.
    pub etag: String,
    /// `{"attendees":[...]}`: the full array, only the self entry's responseStatus changed.
    pub body: String,
}

/// Pure: build the patch body from an `events.get` response. `NotAttendee` when no
/// attendee has `self == true`. Other fields are never sent, except
/// `attendeesOmitted: true` when Google marked the list as partial (so it is not
/// treated as the full guest list).
pub fn build_respond_patch(event_json: &str, response: &str) -> Result<RespondPatch, CalError> {
    let response = validate_response(response)?;
    let v: serde_json::Value =
        serde_json::from_str(event_json).map_err(|_| CalError::Parse("event json"))?;
    let etag = v
        .get("etag")
        .and_then(|e| e.as_str())
        .filter(|e| !e.is_empty())
        .ok_or(CalError::Parse("event without etag"))?
        .to_string();
    let mut attendees = match v.get("attendees") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ => return Err(CalError::NotAttendee),
    };
    let me = attendees
        .iter_mut()
        .find(|a| a.get("self").and_then(|s| s.as_bool()) == Some(true))
        .ok_or(CalError::NotAttendee)?;
    let obj = me.as_object_mut().ok_or(CalError::Parse("attendee json"))?;
    obj.insert("responseStatus".into(), serde_json::Value::String(response.into()));
    let mut body = serde_json::Map::new();
    body.insert("attendees".into(), serde_json::Value::Array(attendees));
    if v.get("attendeesOmitted").and_then(|o| o.as_bool()) == Some(true) {
        body.insert("attendeesOmitted".into(), serde_json::Value::Bool(true));
    }
    let body = serde_json::to_string(&serde_json::Value::Object(body))
        .map_err(|_| CalError::Parse("serialize patch"))?;
    Ok(RespondPatch { etag, body })
}

/// Pure: set `response` on every event matching calendar_id + id. Returns how many.
pub fn apply_response(events: &mut [Event], calendar_id: &str, event_id: &str, response: &str) -> usize {
    let mut n = 0;
    for e in events.iter_mut().filter(|e| e.calendar_id == calendar_id && e.id == event_id) {
        e.response = response.to_string();
        n += 1;
    }
    n
}

impl Cache {
    fn cached_months(&self) -> Vec<CachedMonth> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            let Some(stem) = name.strip_suffix(".json") else { continue };
            if let Some((y, m)) = parse_year_month(stem) {
                if let Some(c) = self.load_month(y, m) {
                    out.push(c);
                }
            }
        }
        out
    }

    /// First cached copy of this event (any month file).
    pub fn find_event(&self, calendar_id: &str, event_id: &str) -> Option<Event> {
        self.cached_months()
            .into_iter()
            .flat_map(|m| m.events)
            .find(|e| e.calendar_id == calendar_id && e.id == event_id)
    }

    /// Update `response` in every month file containing the event (an event can
    /// span months). `fetched_at` is kept. Returns the number of events changed.
    pub fn update_response(&self, calendar_id: &str, event_id: &str, response: &str) -> usize {
        let mut total = 0;
        for mut m in self.cached_months() {
            let n = apply_response(&mut m.events, calendar_id, event_id, response);
            if n > 0 {
                if let Err(e) = self.save_month(&m) {
                    log!("[cal] respond: cache write failed: {e}");
                } else {
                    total += n;
                }
            }
        }
        total
    }
}

/// Respond flow with injectable transport/cache. `can_respond` = granted scope
/// contains calendar.events; false short-circuits before any network call.
pub fn respond_with(
    api: &Api,
    cache: &Cache,
    calendar_id: &str,
    event_id: &str,
    response: &str,
    can_respond: bool,
) -> Result<Event, CalError> {
    let response = validate_response(response)?;
    validate_id(calendar_id, "calendar_id")?;
    validate_id(event_id, "event_id")?;
    if !can_respond {
        return Err(CalError::NeedsReauth);
    }
    let url = format!(
        "{API_BASE}/calendars/{}/events/{}",
        pct_encode(calendar_id),
        pct_encode(event_id)
    );
    let patch_url = format!("{url}?sendUpdates=all");
    let mut retried = false;
    let body = loop {
        let got = api.get_json(&url)?;
        let patch = build_respond_patch(&got, response)?;
        match api.patch_json(&patch_url, &patch.etag, &patch.body) {
            Ok(b) => break b,
            Err(CalError::Http(412)) if !retried => retried = true,
            Err(CalError::Http(412)) => return Err(CalError::Conflict),
            Err(e) => return Err(e),
        }
    };
    let raw: RawEvent = serde_json::from_str(&body).map_err(|_| CalError::Parse("patched event json"))?;
    // Calendar name/colour: from the cached copy of the event, else the calendar cache.
    let cached = cache.find_event(calendar_id, event_id);
    let info = match (&cached, cache.load_calendars()) {
        (Some(e), _) => CalendarInfo {
            id: calendar_id.to_string(),
            summary: e.calendar_name.clone(),
            background_color: e.calendar_color.clone(),
            foreground_color: None,
            primary: false,
            selected: true,
            access_role: None,
        },
        (None, Some(c)) => c
            .calendars
            .into_iter()
            .find(|c| c.id == calendar_id)
            .unwrap_or_else(|| fallback_calendar(calendar_id)),
        (None, None) => fallback_calendar(calendar_id),
    };
    let mut ev = event_from_raw(raw, &info).ok_or(CalError::Parse("patched event unusable"))?;
    // The cache must reflect what we just wrote even if Google echoes something else.
    ev.response = response.to_string();
    let n = cache.update_response(calendar_id, event_id, response);
    log!("[cal] respond: ok response={response} cache_entries_updated={n}");
    Ok(ev)
}

fn fallback_calendar(id: &str) -> CalendarInfo {
    CalendarInfo {
        id: id.to_string(),
        summary: id.to_string(),
        background_color: None,
        foreground_color: None,
        primary: false,
        selected: true,
        access_role: None,
    }
}

/// Production entry: real HTTP, Credential Manager tokens, `%APPDATA%` cache.
pub fn respond(profile: &str, calendar_id: &str, event_id: &str, response: &str) -> Result<Event, CalError> {
    validate_response(response)?;
    validate_id(calendar_id, "calendar_id")?;
    validate_id(event_id, "event_id")?;
    let cache = Cache::for_profile(profile)?;
    let scope = match token_store::load(profile) {
        Ok(Some(st)) => st.scope,
        Ok(None) => return Err(CalError::NeedLogin),
        Err(_) => return Err(CalError::Io("read token")),
    };
    let can = oauth::can_respond(scope.as_deref());
    let http = ReqwestHttp::new()?;
    let tokens = OAuthTokens { profile: profile.to_string() };
    let api = Api::new(&http, &tokens);
    let r = respond_with(&api, &cache, calendar_id, event_id, response, can);
    match &r {
        Ok(_) => log!("[cal] respond profile={profile} response={response} ok"),
        Err(e) => log!("[cal] respond profile={profile} response={response} failed code={}", e.code()),
    }
    r
}

// ---------------------------------------------------------------- CLI

fn parse_year_month(s: &str) -> Option<(i32, u32)> {
    let (y, m) = s.split_once('-')?;
    if y.len() != 4 || m.len() != 2 {
        return None;
    }
    Some((y.parse().ok()?, m.parse().ok()?))
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n).collect();
        t.push('…');
        t
    }
}

/// Handles `--cal-list <profile>` and `--cal-events <profile> <YYYY-MM> [--force]`.
/// Returns Some(exit code) when a calendar command was given (caller must exit).
pub fn run_cli(args: &[String]) -> Option<i32> {
    let i = args
        .iter()
        .position(|a| matches!(a.as_str(), "--cal-list" | "--cal-events"))?;
    let cmd = args[i].as_str();
    let profile = args.get(i + 1).map(String::as_str).unwrap_or("");
    let force = args.iter().any(|a| a == "--force");
    log!("[cal] cli: {cmd} profile={profile} force={force}");
    let t0 = std::time::Instant::now();
    let res: Result<(), CalError> = (|| match cmd {
        "--cal-list" => {
            let r = get_calendars(profile, force)?;
            println!(
                "source={:?} stale={} calendars={} elapsed={}ms",
                r.source,
                r.stale,
                r.calendars.len(),
                t0.elapsed().as_millis()
            );
            if let Some(e) = &r.error {
                println!("note: fetch failed, showing stale cache ({e})");
            }
            for c in &r.calendars {
                println!(
                    "{:<9} {} color={} primary={} role={}",
                    mask_id(&c.id),
                    truncate_chars(&c.summary, 30),
                    c.background_color.as_deref().unwrap_or("-"),
                    c.primary,
                    c.access_role.as_deref().unwrap_or("-")
                );
            }
            Ok(())
        }
        _ => {
            let ym = args.get(i + 2).map(String::as_str).unwrap_or("");
            let (y, m) = parse_year_month(ym).ok_or(CalError::BadArgument("expected <profile> <YYYY-MM>"))?;
            let r = get_month(profile, y, m, force)?;
            println!(
                "source={:?} stale={} events={} calendars={} skipped={} elapsed={}ms",
                r.source,
                r.stale,
                r.events.len(),
                r.calendars.len(),
                r.skipped_calendars.len(),
                t0.elapsed().as_millis()
            );
            if let Some(e) = &r.error {
                println!("note: fetch failed, showing stale cache ({e})");
            }
            for e in r.events.iter().take(10) {
                let date: String = e.start.chars().take(10).collect();
                let tag = if e.all_day { "all-day" } else { "timed" };
                println!(
                    "{date} {tag:<7} {}{}",
                    truncate_chars(&e.summary, 24),
                    if e.meet_url.is_some() { " [meet]" } else { "" }
                );
            }
            Ok(())
        }
    })();
    match res {
        Ok(()) => Some(0),
        Err(e) => {
            println!("error: {e}");
            log!("[cal] cli: {cmd} -> error: {e}");
            Some(if e == CalError::NeedLogin { 3 } else { 2 })
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use std::cell::{Cell, RefCell};

    const CAL_LIST: &str = r##"{"kind":"calendar#calendarList","items":[
      {"id":"me@example.com","summary":"me@example.com","primary":true,"selected":true,
       "backgroundColor":"#9fe1e7","foregroundColor":"#000000","accessRole":"owner"},
      {"id":"team#group@group.calendar.google.com","summary":"Team","summaryOverride":"팀 일정",
       "selected":true,"backgroundColor":"#f83a22","foregroundColor":"#ffffff","accessRole":"reader"},
      {"id":"holiday@group.v.calendar.google.com","summary":"Holidays","accessRole":"reader"},
      {"summary":"no id"}
    ]}"##;

    fn cal(id: &str) -> CalendarInfo {
        CalendarInfo {
            id: id.into(),
            summary: "Cal".into(),
            background_color: Some("#112233".into()),
            foreground_color: None,
            primary: false,
            selected: true,
            access_role: None,
        }
    }

    const EVENTS_PAGE1: &str = r#"{"items":[
      {"id":"e1","status":"confirmed","summary":"Standup","location":"Room 3",
       "htmlLink":"https://www.google.com/calendar/event?eid=x",
       "start":{"dateTime":"2026-11-02T10:00:00+09:00"},"end":{"dateTime":"2026-11-02T10:30:00+09:00"},
       "hangoutLink":"https://meet.google.com/abc-defg-hij"},
      {"id":"e2","summary":"Holiday","start":{"date":"2026-11-05"},"end":{"date":"2026-11-06"}},
      {"id":"e3","status":"cancelled","start":{"dateTime":"2026-11-03T09:00:00+09:00"},"end":{"dateTime":"2026-11-03T10:00:00+09:00"}}
    ],"nextPageToken":"PAGE2/+="}"#;

    const EVENTS_PAGE2: &str = r#"{"items":[
      {"id":"e4_20261110T010000Z","summary":"  ","start":{"dateTime":"2026-11-10T10:00:00+09:00"},
       "end":{"dateTime":"2026-11-10T11:00:00+09:00"},
       "conferenceData":{"entryPoints":[{"entryPointType":"phone","uri":"tel:+1"},
         {"entryPointType":"video","uri":"https://zoom.example/j/1"}]}}
    ]}"#;

    #[test]
    fn parse_calendar_list_fields() {
        let (v, next) = parse_calendar_list(CAL_LIST).unwrap();
        assert!(next.is_none());
        assert_eq!(v.len(), 3); // entry without id dropped
        assert!(v[0].primary && v[0].selected);
        assert_eq!(v[0].background_color.as_deref(), Some("#9fe1e7"));
        assert_eq!(v[0].access_role.as_deref(), Some("owner"));
        assert_eq!(v[1].summary, "팀 일정"); // summaryOverride wins
        assert!(!v[2].selected);
        let sel = selected_only(&v, false);
        assert_eq!(sel.len(), 2);
        assert_eq!(selected_only(&v, true).len(), 3);
    }

    #[test]
    fn parse_events_timed_allday_cancelled_meet() {
        let c = cal("c1");
        let (v, next) = parse_events(EVENTS_PAGE1, &c).unwrap();
        assert_eq!(next.as_deref(), Some("PAGE2/+="));
        assert_eq!(v.len(), 2, "cancelled dropped");
        assert_eq!(v[0].summary, "Standup");
        assert!(!v[0].all_day);
        assert_eq!(v[0].meet_url.as_deref(), Some("https://meet.google.com/abc-defg-hij"));
        assert_eq!(v[0].location.as_deref(), Some("Room 3"));
        assert_eq!(v[0].calendar_color.as_deref(), Some("#112233"));
        assert!(v[1].all_day);
        assert_eq!((v[1].start.as_str(), v[1].end.as_str()), ("2026-11-05", "2026-11-06"));
        assert_eq!(v[1].status, "confirmed");
        assert!(v[1].meet_url.is_none());

        let (v2, next2) = parse_events(EVENTS_PAGE2, &c).unwrap();
        assert!(next2.is_none());
        assert_eq!(v2[0].summary, NO_TITLE);
        assert_eq!(v2[0].meet_url.as_deref(), Some("https://zoom.example/j/1"));
        assert!(v2[0].id.starts_with("e4_")); // recurring instance id
    }

    fn resp_of(item_extra: &str) -> String {
        let body = format!(
            r#"{{"items":[{{"id":"x","start":{{"dateTime":"2026-11-05T10:00:00+09:00"}},"end":{{"dateTime":"2026-11-05T11:00:00+09:00"}}{item_extra}}}]}}"#
        );
        let (v, _) = parse_events(&body, &cal("c")).unwrap();
        assert_eq!(v.len(), 1);
        v[0].response.clone()
    }

    #[test]
    fn response_from_self_attendee() {
        let att = |s: &str| {
            format!(
                r#","attendees":[{{"email":"a@x","responseStatus":"declined"}},{{"email":"me@x","self":true,"responseStatus":"{s}"}}]"#
            )
        };
        assert_eq!(resp_of(&att("needsAction")), "needsAction");
        assert_eq!(resp_of(&att("tentative")), "tentative");
        assert_eq!(resp_of(&att("declined")), "declined");
        assert_eq!(resp_of(&att("accepted")), "accepted");
        assert_eq!(resp_of(&att("weird")), "accepted", "unknown maps to accepted");
        assert_eq!(
            resp_of(r#","attendees":[{"email":"me@x","self":true}]"#),
            "accepted",
            "missing responseStatus"
        );
    }

    #[test]
    fn response_defaults_without_self() {
        assert_eq!(
            resp_of(r#","attendees":[{"email":"a@x","responseStatus":"needsAction"}]"#),
            "accepted"
        );
        assert_eq!(resp_of(""), "accepted");
        assert_eq!(resp_of(r#","attendees":[]"#), "accepted");
    }

    #[test]
    fn response_organizer_self_event() {
        let extra = r#","organizer":{"self":true,"email":"me@x"},"creator":{"self":true},
            "attendees":[{"email":"b@x","responseStatus":"needsAction"},
                         {"email":"me@x","self":true,"organizer":true,"responseStatus":"accepted"}]"#;
        assert_eq!(resp_of(extra), "accepted");
        assert_eq!(resp_of(r#","organizer":{"self":true}"#), "accepted");
    }

    #[test]
    fn old_cached_event_without_response_loads() {
        let j = r##"{"id":"e","calendar_id":"c","calendar_name":"n","calendar_color":null,
            "summary":"s","start":"2026-11-05","end":"2026-11-06","all_day":true,
            "location":null,"meet_url":null,"html_link":null,"status":"confirmed"}"##;
        let e: Event = serde_json::from_str(j).unwrap();
        assert_eq!(e.response, "accepted");
        let back = serde_json::to_string(&e).unwrap();
        assert!(back.contains(r#""response":"accepted""#));
    }

    #[test]
    fn parse_garbage_is_typed_error() {
        assert_eq!(parse_events("<html>", &cal("c")).unwrap_err(), CalError::Parse("events json"));
        assert!(parse_calendar_list("{").is_err());
    }

    #[test]
    fn classify() {
        assert_eq!(classify_status(401, ""), CalError::Unauthorized);
        assert_eq!(classify_status(429, ""), CalError::RateLimited);
        assert_eq!(classify_status(503, ""), CalError::Server(503));
        assert_eq!(classify_status(404, ""), CalError::Http(404));
        assert_eq!(classify_status(403, r#"{"error":{"errors":[{"reason":"forbidden"}]}}"#), CalError::Forbidden);
        assert_eq!(
            classify_status(403, r#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#),
            CalError::RateLimited
        );
    }

    #[test]
    fn month_window_basic_and_year_rollover() {
        let kst = FixedOffset::east_opt(9 * 3600).unwrap();
        assert_eq!(
            month_window(&kst, 2026, 11).unwrap(),
            ("2026-11-01T00:00:00+09:00".into(), "2026-12-01T00:00:00+09:00".into())
        );
        assert_eq!(
            month_window(&kst, 2026, 12).unwrap(),
            ("2026-12-01T00:00:00+09:00".into(), "2027-01-01T00:00:00+09:00".into())
        );
        let west = FixedOffset::west_opt(8 * 3600).unwrap();
        assert_eq!(
            month_window(&west, 2027, 1).unwrap(),
            ("2027-01-01T00:00:00-08:00".into(), "2027-02-01T00:00:00-08:00".into())
        );
        assert!(month_window(&kst, 2026, 13).is_none());
        assert!(validate_month(2026, 0).is_err());
        assert!(validate_month(1999, 5).is_err());
        assert!(validate_month(2026, 11).is_ok());
    }

    #[test]
    fn ttl_rules() {
        assert!(is_fresh(1000, 1000, 300));
        assert!(is_fresh(1000, 1299, 300));
        assert!(!is_fresh(1000, 1300, 300));
        assert!(!is_fresh(2000, 1000, 300), "future timestamp not trusted");
    }

    #[test]
    fn parse_year_month_cli() {
        assert_eq!(parse_year_month("2026-11"), Some((2026, 11)));
        assert_eq!(parse_year_month("2026-1"), None);
        assert_eq!(parse_year_month("x"), None);
    }

    // ---- fakes

    struct FakeHttp {
        /// (url substring, status, body) consumed in order of matching calls.
        replies: RefCell<Vec<(u16, String)>>,
        urls: RefCell<Vec<String>>,
        bearers: RefCell<Vec<String>>,
        /// (url, if_match, body) of every PATCH.
        patches: RefCell<Vec<(String, String, String)>>,
    }
    impl FakeHttp {
        fn new(replies: Vec<(u16, &str)>) -> Self {
            Self {
                replies: RefCell::new(replies.into_iter().rev().map(|(s, b)| (s, b.to_string())).collect()),
                urls: RefCell::new(vec![]),
                bearers: RefCell::new(vec![]),
                patches: RefCell::new(vec![]),
            }
        }
        fn calls(&self) -> usize {
            self.urls.borrow().len()
        }
    }
    impl Http for FakeHttp {
        fn get(&self, url: &str, bearer: &str) -> Result<(u16, String), CalError> {
            self.urls.borrow_mut().push(url.to_string());
            self.bearers.borrow_mut().push(bearer.to_string());
            self.replies.borrow_mut().pop().ok_or(CalError::Network("offline (no fake reply)"))
        }
        fn patch(&self, url: &str, bearer: &str, if_match: &str, body: &str) -> Result<(u16, String), CalError> {
            self.urls.borrow_mut().push(format!("PATCH {url}"));
            self.bearers.borrow_mut().push(bearer.to_string());
            self.patches.borrow_mut().push((url.into(), if_match.into(), body.into()));
            self.replies.borrow_mut().pop().ok_or(CalError::Network("offline (no fake reply)"))
        }
    }

    struct FakeTokens {
        normal: Cell<u32>,
        forced: Cell<u32>,
    }
    impl FakeTokens {
        fn new() -> Self {
            Self { normal: Cell::new(0), forced: Cell::new(0) }
        }
    }
    impl TokenSource for FakeTokens {
        fn token(&self, force: bool) -> Result<String, CalError> {
            if force {
                self.forced.set(self.forced.get() + 1);
                Ok("fresh-token".into())
            } else {
                self.normal.set(self.normal.get() + 1);
                Ok("old-token".into())
            }
        }
    }

    fn api<'a>(h: &'a FakeHttp, t: &'a FakeTokens) -> Api<'a> {
        Api { http: h, tokens: t, backoff: Duration::ZERO }
    }

    fn test_cache(tag: &str) -> (Cache, PathBuf) {
        let root = std::env::temp_dir().join(format!("clockcal-test-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (Cache::new(&root, "personal"), root)
    }

    #[test]
    fn retry_401_refreshes_once_then_succeeds() {
        let h = FakeHttp::new(vec![(401, "{}"), (200, "ok-body")]);
        let t = FakeTokens::new();
        let body = api(&h, &t).get_json("https://x/y").unwrap();
        assert_eq!(body, "ok-body");
        assert_eq!(h.calls(), 2);
        assert_eq!(*h.bearers.borrow(), vec!["old-token", "fresh-token"]);
        assert_eq!((t.normal.get(), t.forced.get()), (1, 1));
    }

    #[test]
    fn retry_401_twice_is_unauthorized_no_storm() {
        let h = FakeHttp::new(vec![(401, ""), (401, ""), (200, "never")]);
        let t = FakeTokens::new();
        assert_eq!(api(&h, &t).get_json("u").unwrap_err(), CalError::Unauthorized);
        assert_eq!(h.calls(), 2);
        assert_eq!(t.forced.get(), 1);
    }

    #[test]
    fn retry_429_and_5xx_once_only() {
        let t = FakeTokens::new();
        let h = FakeHttp::new(vec![(429, ""), (200, "ok")]);
        assert_eq!(api(&h, &t).get_json("u").unwrap(), "ok");
        assert_eq!(h.calls(), 2);

        let h = FakeHttp::new(vec![(503, ""), (502, ""), (200, "never")]);
        assert_eq!(api(&h, &t).get_json("u").unwrap_err(), CalError::Server(502));
        assert_eq!(h.calls(), 2);

        let h = FakeHttp::new(vec![(429, ""), (429, ""), (200, "never")]);
        assert_eq!(api(&h, &t).get_json("u").unwrap_err(), CalError::RateLimited);
        assert_eq!(h.calls(), 2);
    }

    #[test]
    fn forbidden_and_404_are_not_retried() {
        let t = FakeTokens::new();
        let h = FakeHttp::new(vec![(403, "{}"), (200, "never")]);
        assert_eq!(api(&h, &t).get_json("u").unwrap_err(), CalError::Forbidden);
        assert_eq!(h.calls(), 1);
        let h = FakeHttp::new(vec![(404, ""), (200, "never")]);
        assert_eq!(api(&h, &t).get_json("u").unwrap_err(), CalError::Http(404));
        assert_eq!(h.calls(), 1);
    }

    #[test]
    fn events_pagination_and_url_shape() {
        let h = FakeHttp::new(vec![(200, EVENTS_PAGE1), (200, EVENTS_PAGE2)]);
        let t = FakeTokens::new();
        let c = cal("team#grp@group.calendar.google.com");
        let v = api(&h, &t)
            .fetch_events(&c, "2026-11-01T00:00:00+09:00", "2026-12-01T00:00:00+09:00")
            .unwrap();
        assert_eq!(v.len(), 3);
        let urls = h.urls.borrow();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("/calendars/team%23grp%40group.calendar.google.com/events?"));
        assert!(urls[0].contains("singleEvents=true"));
        assert!(urls[0].contains("orderBy=startTime"));
        assert!(urls[0].contains("maxResults=250"));
        assert!(urls[0].contains("timeMin=2026-11-01T00%3A00%3A00%2B09%3A00"));
        assert!(!urls[0].contains("pageToken"));
        assert!(urls[1].contains("pageToken=PAGE2%2F%2B%3D"));
    }

    #[test]
    fn calendar_list_pagination() {
        let p1 = r#"{"items":[{"id":"a","summary":"A","selected":true}],"nextPageToken":"n1"}"#;
        let p2 = r#"{"items":[{"id":"b","summary":"B","selected":true}]}"#;
        let h = FakeHttp::new(vec![(200, p1), (200, p2)]);
        let t = FakeTokens::new();
        let v = api(&h, &t).fetch_calendars().unwrap();
        assert_eq!(v.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(h.urls.borrow()[1].contains("pageToken=n1"));
    }

    #[test]
    fn cache_roundtrip_atomic_no_temp_left() {
        let (cache, root) = test_cache("rt");
        let m = CachedMonth {
            version: CACHE_VERSION,
            fetched_at: 123,
            year: 2026,
            month: 11,
            events: vec![],
            skipped: vec!["abc...".into()],
        };
        cache.save_month(&m).unwrap();
        cache.save_month(&CachedMonth { fetched_at: 456, ..m.clone() }).unwrap(); // overwrite
        let back = cache.load_month(2026, 11).unwrap();
        assert_eq!(back.fetched_at, 456);
        assert!(cache.load_month(2026, 12).is_none());
        let names: Vec<String> = std::fs::read_dir(root.join("personal"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["2026-11.json"]);
        // Corrupt file -> treated as a miss, not a panic.
        std::fs::write(root.join("personal").join("2026-10.json"), "{garbage").unwrap();
        assert!(cache.load_month(2026, 10).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn kst() -> FixedOffset {
        FixedOffset::east_opt(9 * 3600).unwrap()
    }

    /// Fake replies for one full month fetch: calendarList then events per selected calendar (2).
    fn month_replies() -> Vec<(u16, &'static str)> {
        let ev = r#"{"items":[{"id":"x","summary":"T","start":{"dateTime":"2026-11-04T10:00:00+09:00"},"end":{"dateTime":"2026-11-04T11:00:00+09:00"}}]}"#;
        vec![(200, CAL_LIST), (200, ev), (200, ev)]
    }

    #[test]
    fn get_month_fetch_then_cache_hit_then_stale_on_failure() {
        let (cache, root) = test_cache("flow");
        let t = FakeTokens::new();
        let h = FakeHttp::new(month_replies());
        let r = get_month_with(&api(&h, &t), &cache, &kst(), "personal", 2026, 11, false, 1000).unwrap();
        assert_eq!(r.source, Source::Fetched);
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.calendars.len(), 2);
        assert_eq!(h.calls(), 3);

        // Within TTL: no network at all.
        let h2 = FakeHttp::new(vec![]);
        let r = get_month_with(&api(&h2, &t), &cache, &kst(), "personal", 2026, 11, false, 1000 + 299).unwrap();
        assert_eq!(r.source, Source::Cache);
        assert!(!r.stale);
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.calendars.len(), 2);
        assert_eq!(h2.calls(), 0);

        // Expired + offline -> stale cache, flagged, error text without secrets.
        let r = get_month_with(&api(&h2, &t), &cache, &kst(), "personal", 2026, 11, false, 1000 + 301).unwrap();
        assert_eq!(r.source, Source::Stale);
        assert!(r.stale);
        assert!(r.error.is_some());
        assert_eq!(r.fetched_at, 1000);
        assert_eq!(r.events.len(), 2);

        // force while offline also falls back to stale.
        let r = get_month_with(&api(&h2, &t), &cache, &kst(), "personal", 2026, 11, true, 1010).unwrap();
        assert_eq!(r.source, Source::Stale);

        // Never-cached month + offline -> error.
        let e = get_month_with(&api(&h2, &t), &cache, &kst(), "personal", 2026, 12, false, 1010).unwrap_err();
        assert_eq!(e, CalError::Network("offline (no fake reply)"));

        // NeedLogin is never masked by stale data.
        struct NoLogin;
        impl TokenSource for NoLogin {
            fn token(&self, _f: bool) -> Result<String, CalError> {
                Err(CalError::NeedLogin)
            }
        }
        let nl = NoLogin;
        let a = Api { http: &h2, tokens: &nl, backoff: Duration::ZERO };
        let e = get_month_with(&a, &cache, &kst(), "personal", 2026, 11, true, 5000).unwrap_err();
        assert_eq!(e, CalError::NeedLogin);

        // Cache file holds events only (no token-ish fields).
        let raw = std::fs::read_to_string(root.join("personal").join("2026-11.json")).unwrap();
        assert!(!raw.contains("token"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn get_month_skips_forbidden_calendar_and_force_refetches() {
        let (cache, root) = test_cache("skip");
        let t = FakeTokens::new();
        let ev = r#"{"items":[{"id":"x","summary":"T","start":{"date":"2026-11-04"},"end":{"date":"2026-11-05"}}]}"#;
        let h = FakeHttp::new(vec![(200, CAL_LIST), (403, "{}"), (200, ev)]);
        let r = get_month_with(&api(&h, &t), &cache, &kst(), "personal", 2026, 11, false, 10).unwrap();
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.skipped_calendars.len(), 1);
        assert!(r.events[0].all_day);

        // force=true bypasses a fresh cache.
        let h = FakeHttp::new(month_replies());
        let r = get_month_with(&api(&h, &t), &cache, &kst(), "personal", 2026, 11, true, 11).unwrap();
        assert_eq!(r.source, Source::Fetched);
        assert_eq!(h.calls(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mask_and_truncate() {
        assert_eq!(mask_id("abcdefghij"), "abcdef...");
        assert_eq!(mask_id("ab"), "ab...");
        assert_eq!(truncate_chars("가나다라마", 3), "가나다…");
        assert_eq!(truncate_chars("ab", 3), "ab");
    }

    // ---- respond to invitation (C-13)

    const GET_EVENT: &str = r#"{"kind":"calendar#event","etag":"\"3400\"","id":"ev1","status":"confirmed",
      "summary":"Planning","start":{"dateTime":"2026-11-25T10:00:00+09:00"},"end":{"dateTime":"2026-11-25T11:00:00+09:00"},
      "attendees":[
        {"email":"org@x","organizer":true,"responseStatus":"accepted","displayName":"Org"},
        {"email":"me@x","self":true,"responseStatus":"needsAction","comment":"hi"},
        {"email":"z@x","responseStatus":"declined","optional":true}
      ]}"#;

    const PATCHED_EVENT: &str = r#"{"etag":"\"3401\"","id":"ev1","status":"confirmed","summary":"Planning",
      "start":{"dateTime":"2026-11-25T10:00:00+09:00"},"end":{"dateTime":"2026-11-25T11:00:00+09:00"},
      "attendees":[{"email":"org@x","responseStatus":"accepted"},
        {"email":"me@x","self":true,"responseStatus":"declined"},{"email":"z@x","responseStatus":"declined"}]}"#;

    #[test]
    fn respond_validation() {
        assert_eq!(validate_response("accepted"), Ok("accepted"));
        assert_eq!(validate_response("declined"), Ok("declined"));
        assert_eq!(validate_response("tentative"), Ok("tentative"));
        for bad in ["", "needsAction", "Accepted", "accept", " accepted", "accepted ", "yes"] {
            assert_eq!(validate_response(bad), Err(CalError::BadArgument("response")), "{bad:?}");
        }
        assert!(validate_id("abc_123", "x").is_ok());
        assert!(validate_id("", "x").is_err());
        assert!(validate_id(&"a".repeat(1024), "x").is_ok());
        assert!(validate_id(&"a".repeat(1025), "x").is_err());
        assert!(validate_id("a\nb", "x").is_err());
        assert!(validate_id("a\0b", "x").is_err());
        assert!(validate_id("a\u{7f}b", "x").is_err());
    }

    #[test]
    fn patch_body_changes_only_self_keeps_order() {
        let p = build_respond_patch(GET_EVENT, "declined").unwrap();
        assert_eq!(p.etag, "\"3400\"");
        let body: serde_json::Value = serde_json::from_str(&p.body).unwrap();
        assert_eq!(body.as_object().unwrap().len(), 1, "only attendees is sent: {}", p.body);
        let got = body["attendees"].as_array().unwrap();
        let orig: serde_json::Value = serde_json::from_str(GET_EVENT).unwrap();
        let orig = orig["attendees"].as_array().unwrap();
        assert_eq!(got.len(), orig.len());
        for (i, (g, o)) in got.iter().zip(orig).enumerate() {
            let mut expect = o.clone();
            if i == 1 {
                expect["responseStatus"] = "declined".into();
            }
            assert_eq!(g, &expect, "attendee {i}");
        }
        assert_eq!(got[0]["email"], "org@x");
        assert_eq!(got[1]["self"], true);
        assert_eq!(got[1]["comment"], "hi");
        assert_eq!(got[2]["email"], "z@x");
    }

    #[test]
    fn patch_body_all_three_responses_and_omitted_flag() {
        for r in ["accepted", "declined", "tentative"] {
            let p = build_respond_patch(GET_EVENT, r).unwrap();
            let v: serde_json::Value = serde_json::from_str(&p.body).unwrap();
            assert_eq!(v["attendees"][1]["responseStatus"], r);
            assert_eq!(v["attendees"][0]["responseStatus"], "accepted");
        }
        let omitted = r#"{"etag":"e","attendeesOmitted":true,"attendees":[{"email":"me@x","self":true,"responseStatus":"needsAction"}]}"#;
        let v: serde_json::Value = serde_json::from_str(&build_respond_patch(omitted, "tentative").unwrap().body).unwrap();
        assert_eq!(v["attendeesOmitted"], true);
        assert_eq!(v.as_object().unwrap().len(), 2);
    }

    #[test]
    fn patch_body_errors() {
        let no_self = r#"{"etag":"e","attendees":[{"email":"a@x","responseStatus":"accepted"}]}"#;
        assert_eq!(build_respond_patch(no_self, "accepted"), Err(CalError::NotAttendee));
        let no_att = r#"{"etag":"e","summary":"solo"}"#;
        assert_eq!(build_respond_patch(no_att, "accepted"), Err(CalError::NotAttendee));
        let no_etag = r#"{"attendees":[{"self":true}]}"#;
        assert_eq!(build_respond_patch(no_etag, "accepted"), Err(CalError::Parse("event without etag")));
        assert_eq!(build_respond_patch("{", "accepted"), Err(CalError::Parse("event json")));
        assert_eq!(build_respond_patch(GET_EVENT, "maybe"), Err(CalError::BadArgument("response")));
        assert_eq!(CalError::NotAttendee.code(), "not_attendee");
    }

    fn ev(cal: &str, id: &str, resp: &str) -> Event {
        Event {
            id: id.into(),
            calendar_id: cal.into(),
            calendar_name: "Cal".into(),
            calendar_color: Some("#112233".into()),
            summary: "S".into(),
            start: "2026-11-25T10:00:00+09:00".into(),
            end: "2026-11-25T11:00:00+09:00".into(),
            all_day: false,
            location: None,
            meet_url: None,
            html_link: None,
            status: "confirmed".into(),
            response: resp.into(),
            self_attendee: true,
        }
    }

    #[test]
    fn self_attendee_invited_vs_organizer_vs_none() {
        let cal = CalendarInfo {
            id: "c".into(),
            summary: "n".into(),
            background_color: None,
            foreground_color: None,
            primary: true,
            access_role: Some("owner".into()),
            selected: true,
        };
        let parse = |extra: &str| {
            let body = format!(
                r#"{{"items":[{{"id":"e","start":{{"dateTime":"2026-11-25T10:00:00+09:00"}},"end":{{"dateTime":"2026-11-25T11:00:00+09:00"}}{extra}}}]}}"#
            );
            parse_events(&body, &cal).unwrap().0.remove(0)
        };
        let invited = parse(r#","organizer":{"email":"o@x"},"attendees":[{"email":"me@x","self":true,"responseStatus":"needsAction"}]"#);
        assert!(invited.self_attendee);
        let organizer = parse(r#","organizer":{"email":"me@x","self":true},"attendees":[{"email":"me@x","self":true,"responseStatus":"accepted"}]"#);
        assert!(!organizer.self_attendee);
        assert!(!parse("").self_attendee);
        // A subscribed colleague's calendar: `self` refers to that calendar, so never offer RSVP.
        let shared = CalendarInfo { id: "col@x".into(), primary: false, ..cal.clone() };
        let body = r#"{"items":[{"id":"e","start":{"dateTime":"2026-11-25T10:00:00+09:00"},"end":{"dateTime":"2026-11-25T11:00:00+09:00"},"organizer":{"email":"o@x"},"attendees":[{"email":"col@x","self":true,"responseStatus":"needsAction"}]}]}"#;
        assert!(!parse_events(body, &shared).unwrap().0.remove(0).self_attendee);
    }

    #[test]
    fn hidden_title_shows_busy() {
        let mk = |role: &str| CalendarInfo {
            id: "c".into(), summary: "n".into(), background_color: None, foreground_color: None,
            primary: false, access_role: Some(role.into()), selected: true,
        };
        let body = |extra: &str| format!(
            r#"{{"items":[{{"id":"e","start":{{"dateTime":"2026-11-25T10:00:00+09:00"}},"end":{{"dateTime":"2026-11-25T11:00:00+09:00"}}{extra}}}]}}"#);
        let t = |role: &str, extra: &str| parse_events(&body(extra), &mk(role)).unwrap().0.remove(0).summary;
        assert_eq!(t("reader", r#","visibility":"private""#), BUSY_TITLE);
        assert_eq!(t("freeBusyReader", ""), BUSY_TITLE);
        assert_eq!(t("reader", ""), NO_TITLE);
        assert_eq!(t("reader", r#","visibility":"private","summary":"비밀""#), "비밀");
    }

    #[test]
    fn validate_id_rejects_dot_segments() {
        assert!(validate_id(".", "x").is_err());
        assert!(validate_id("..", "x").is_err());
        assert!(validate_id("a.b", "x").is_ok());
    }

    #[test]
    fn apply_response_matches_calendar_and_id() {
        let mut v = vec![ev("c1", "a", "needsAction"), ev("c2", "a", "needsAction"), ev("c1", "b", "needsAction")];
        assert_eq!(apply_response(&mut v, "c1", "a", "declined"), 1);
        assert_eq!(v[0].response, "declined");
        assert_eq!(v[1].response, "needsAction", "same id, other calendar untouched");
        assert_eq!(v[2].response, "needsAction");
        assert_eq!(apply_response(&mut v, "c9", "a", "accepted"), 0);
    }

    #[test]
    fn cache_update_response_across_month_files() {
        let (cache, root) = test_cache("respond-upd");
        let mk = |month: u32, events: Vec<Event>| CachedMonth {
            version: CACHE_VERSION,
            fetched_at: 777,
            year: 2026,
            month,
            events,
            skipped: vec![],
        };
        cache.save_month(&mk(11, vec![ev("c1", "a", "needsAction"), ev("c1", "b", "accepted")])).unwrap();
        cache.save_month(&mk(12, vec![ev("c1", "a", "needsAction")])).unwrap(); // spans months
        cache.save_month(&mk(10, vec![ev("c1", "x", "needsAction")])).unwrap();
        assert_eq!(cache.update_response("c1", "a", "tentative"), 2);
        let nov = cache.load_month(2026, 11).unwrap();
        assert_eq!(nov.events[0].response, "tentative");
        assert_eq!(nov.events[1].response, "accepted");
        assert_eq!(nov.fetched_at, 777, "fetched_at kept");
        assert_eq!(cache.load_month(2026, 12).unwrap().events[0].response, "tentative");
        assert_eq!(cache.load_month(2026, 10).unwrap().events[0].response, "needsAction");
        assert_eq!(cache.update_response("c1", "nope", "accepted"), 0);
        assert_eq!(cache.find_event("c1", "b").unwrap().response, "accepted");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn respond_flow_get_then_patch_and_cache() {
        let (cache, root) = test_cache("respond-flow");
        cache
            .save_month(&CachedMonth {
                version: CACHE_VERSION,
                fetched_at: 1,
                year: 2026,
                month: 11,
                events: vec![ev("me@example.com", "ev1", "needsAction")],
                skipped: vec![],
            })
            .unwrap();
        let h = FakeHttp::new(vec![(200, GET_EVENT), (200, PATCHED_EVENT)]);
        let t = FakeTokens::new();
        let out = respond_with(&api(&h, &t), &cache, "me@example.com", "ev1", "declined", true).unwrap();
        assert_eq!(out.response, "declined");
        assert_eq!(out.calendar_name, "Cal", "calendar info taken from cached event");
        assert_eq!(out.summary, "Planning");
        let urls = h.urls.borrow();
        assert_eq!(urls[0], format!("{API_BASE}/calendars/me%40example.com/events/ev1"));
        let (purl, if_match, body) = h.patches.borrow()[0].clone();
        assert_eq!(purl, format!("{API_BASE}/calendars/me%40example.com/events/ev1?sendUpdates=all"));
        assert_eq!(if_match, "\"3400\"");
        assert!(body.starts_with(r#"{"attendees":["#) && body.contains(r#""responseStatus":"declined""#));
        assert_eq!(cache.load_month(2026, 11).unwrap().events[0].response, "declined");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn respond_retries_once_on_412_then_conflict() {
        let (cache, root) = test_cache("respond-412");
        let t = FakeTokens::new();
        // 412 once, then success.
        let h = FakeHttp::new(vec![(200, GET_EVENT), (412, "{}"), (200, GET_EVENT), (200, PATCHED_EVENT)]);
        assert!(respond_with(&api(&h, &t), &cache, "c", "ev1", "declined", true).is_ok());
        assert_eq!(h.patches.borrow().len(), 2);
        // 412 twice -> conflict, no third attempt.
        let h = FakeHttp::new(vec![(200, GET_EVENT), (412, "{}"), (200, GET_EVENT), (412, "{}")]);
        assert_eq!(respond_with(&api(&h, &t), &cache, "c", "ev1", "declined", true), Err(CalError::Conflict));
        assert_eq!(h.patches.borrow().len(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn respond_error_codes_and_no_network_when_unauthorised() {
        let (cache, root) = test_cache("respond-err");
        let t = FakeTokens::new();
        // can_respond=false: needs_reauth, zero calls.
        let h = FakeHttp::new(vec![]);
        let e = respond_with(&api(&h, &t), &cache, "c", "ev1", "accepted", false).unwrap_err();
        assert_eq!(e.code(), "needs_reauth");
        assert_eq!(h.calls(), 0);
        // Bad arguments rejected before network.
        assert_eq!(respond_with(&api(&h, &t), &cache, "c", "ev1", "needsAction", true).unwrap_err().code(), "bad_argument");
        assert_eq!(respond_with(&api(&h, &t), &cache, "", "ev1", "accepted", true).unwrap_err().code(), "bad_argument");
        assert_eq!(h.calls(), 0);
        // Not an attendee: get only, no patch.
        let h = FakeHttp::new(vec![(200, r#"{"etag":"e","attendees":[{"email":"a@x"}]}"#)]);
        assert_eq!(respond_with(&api(&h, &t), &cache, "c", "ev1", "accepted", true).unwrap_err().code(), "not_attendee");
        assert!(h.patches.borrow().is_empty());
        // Google says scope insufficient (403) on the patch.
        let insufficient = r#"{"error":{"code":403,"errors":[{"reason":"insufficientPermissions"}],"status":"PERMISSION_DENIED"}}"#;
        let h = FakeHttp::new(vec![(200, GET_EVENT), (403, insufficient)]);
        assert_eq!(respond_with(&api(&h, &t), &cache, "c", "ev1", "accepted", true).unwrap_err().code(), "needs_reauth");
        // Same on the get.
        let h = FakeHttp::new(vec![(403, insufficient)]);
        assert_eq!(respond_with(&api(&h, &t), &cache, "c", "ev1", "accepted", true).unwrap_err().code(), "needs_reauth");
        // 401 persisting after a refresh.
        let h = FakeHttp::new(vec![(401, "{}"), (401, "{}")]);
        assert_eq!(respond_with(&api(&h, &t), &cache, "c", "ev1", "accepted", true).unwrap_err().code(), "needs_reauth");
        // Other codes.
        assert_eq!(classify_status(403, "{}").code(), "forbidden");
        assert_eq!(classify_status(404, "{}").code(), "not_found");
        assert_eq!(classify_status(400, "{}").code(), "http_400");
        assert_eq!(classify_status(429, "{}").code(), "rate_limited");
        assert_eq!(classify_status(503, "{}").code(), "server_error");
        let _ = std::fs::remove_dir_all(root);
    }
}
