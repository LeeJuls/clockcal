//! P5: decide which events need a "starts in N min" toast. Pure logic here; the scheduler thread lives in lib.rs.
//!
//! Rule: timed, not cancelled, not declined; fire once per (event id, start) when
//! `start - lead <= now` and `now < start + GRACE_S` (a wake-up shortly after the start still notifies,
//! a long sleep does not replay old events).

use crate::calendar::Event;
use chrono::DateTime;
use std::collections::{BTreeMap, HashSet};

pub const GRACE_S: i64 = 60;
const PRUNE_AFTER_S: i64 = 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    pub key: String,
    pub title: String,
    pub body: String,
    pub start: i64,
}

pub fn fired_key(e: &Event, start: i64) -> String {
    format!("{}|{}", e.id, start)
}

fn start_secs(e: &Event) -> Option<i64> {
    if e.all_day {
        return None;
    }
    DateTime::parse_from_rfc3339(&e.start).ok().map(|d| d.timestamp())
}

/// `fmt_time` formats a unix timestamp as local clock text (injected so tests stay timezone-free).
pub fn due_events(
    events: &[Event],
    now: i64,
    lead_min: u32,
    fired: &BTreeMap<String, i64>,
    fmt_time: &dyn Fn(i64) -> String,
) -> Vec<Due> {
    let lead = i64::from(lead_min) * 60;
    let mut seen: HashSet<String> = HashSet::new();
    // Declined on any calendar copy of the same event id means declined (copies can disagree).
    let declined: HashSet<&str> = events.iter().filter(|e| e.response == "declined").map(|e| e.id.as_str()).collect();
    let mut out = Vec::new();
    for e in events {
        if e.status == "cancelled" || declined.contains(e.id.as_str()) {
            continue;
        }
        let Some(start) = start_secs(e) else { continue };
        if now < start - lead || now >= start + GRACE_S {
            continue;
        }
        let key = fired_key(e, start);
        if fired.contains_key(&key) || !seen.insert(key.clone()) {
            continue;
        }
        let mins_left = ((start - now) + 59).div_euclid(60).max(0);
        let when = if mins_left == 0 { "곧 시작".to_string() } else { format!("{mins_left}분 후") };
        let mut body = format!("{when} · {}", fmt_time(start));
        if let Some(loc) = e.location.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            body.push_str(" · ");
            body.push_str(&loc.chars().take(60).collect::<String>());
        }
        out.push(Due { key, title: e.summary.clone(), body, start });
    }
    out.sort_by_key(|d| d.start);
    out
}

/// Drop fired entries whose start is long past (keeps the file small).
pub fn prune(fired: &mut BTreeMap<String, i64>, now: i64) {
    fired.retain(|_, start| *start > now - PRUNE_AFTER_S);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, start: &str) -> Event {
        Event {
            id: id.into(),
            calendar_id: "c".into(),
            calendar_name: "c".into(),
            calendar_color: None,
            summary: format!("t-{id}"),
            start: start.into(),
            end: start.into(),
            all_day: false,
            location: None,
            meet_url: None,
            html_link: None,
            status: "confirmed".into(),
            response: "accepted".into(),
            self_attendee: false,
        }
    }
    const T: &str = "2026-11-24T10:00:00Z";
    fn t0() -> i64 {
        DateTime::parse_from_rfc3339(T).unwrap().timestamp()
    }
    fn run(events: &[Event], now: i64, fired: &BTreeMap<String, i64>) -> Vec<Due> {
        due_events(events, now, 10, fired, &|_| "10:00".into())
    }

    #[test]
    fn window_edges() {
        let e = [ev("a", T)];
        let none = BTreeMap::new();
        assert!(run(&e, t0() - 601, &none).is_empty());
        assert_eq!(run(&e, t0() - 600, &none).len(), 1);
        assert_eq!(run(&e, t0() + 59, &none).len(), 1);
        assert!(run(&e, t0() + 60, &none).is_empty());
    }

    #[test]
    fn fires_once() {
        let e = [ev("a", T)];
        let mut fired = BTreeMap::new();
        let d = run(&e, t0() - 300, &fired);
        assert_eq!(d.len(), 1);
        fired.insert(d[0].key.clone(), d[0].start);
        assert!(run(&e, t0() - 200, &fired).is_empty());
    }

    #[test]
    fn rescheduled_event_fires_again() {
        let mut fired = BTreeMap::new();
        fired.insert(fired_key(&ev("a", T), t0()), t0());
        let moved = [ev("a", "2026-11-24T10:05:00Z")];
        assert_eq!(run(&moved, t0() - 200, &fired).len(), 1);
    }

    #[test]
    fn skips_all_day_cancelled_declined() {
        let mut ad = ev("a", "2026-11-24");
        ad.all_day = true;
        let mut cx = ev("b", T);
        cx.status = "cancelled".into();
        let mut dc = ev("c", T);
        dc.response = "declined".into();
        assert!(run(&[ad, cx, dc], t0() - 300, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn same_event_on_two_calendars_once() {
        let mut b = ev("a", T);
        b.calendar_id = "other".into();
        assert_eq!(run(&[ev("a", T), b], t0() - 300, &BTreeMap::new()).len(), 1);
    }

    #[test]
    fn declined_copy_suppresses_other_copies() {
        let mut a = ev("a", T);
        a.response = "declined".into();
        let b = ev("a", T);
        assert!(run(&[b, a], t0() - 300, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn long_sleep_does_not_replay_old_events() {
        assert!(run(&[ev("a", T)], t0() + 3 * 3600, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn body_text_and_location() {
        let mut e = ev("a", T);
        e.location = Some("  3층 회의실  ".into());
        let d = run(&[e], t0() - 270, &BTreeMap::new());
        assert_eq!(d[0].body, "5분 후 · 10:00 · 3층 회의실");
        assert!(run(&[ev("b", T)], t0() - 10, &BTreeMap::new())[0].body.starts_with("1분 후"));
        assert!(run(&[ev("b", T)], t0() + 5, &BTreeMap::new())[0].body.starts_with("곧 시작"));
    }

    #[test]
    fn bad_start_is_ignored_and_prune_works() {
        assert!(run(&[ev("a", "garbage")], t0(), &BTreeMap::new()).is_empty());
        let mut f = BTreeMap::new();
        f.insert("old".into(), t0() - 2 * 24 * 3600);
        f.insert("new".into(), t0());
        prune(&mut f, t0());
        assert!(f.contains_key("new") && !f.contains_key("old"));
    }
}
