// Pure calendar logic (no DOM, no Tauri). Runs in the WebView and in node (dev/logic.test.js).
// Dates are handled as local "YYYY-MM-DD" keys; day arithmetic uses UTC math, so DST never shifts a day.
(function (root) {
  const DEFAULT_COLOR = "#4285f4";
  const MAX_DOTS = 3;

  const pad = (n, w = 2) => String(n).padStart(w, "0");
  const key = (y, m, d) => `${pad(y, 4)}-${pad(m)}-${pad(d)}`;
  const parseKey = (k) => { const [y, m, d] = k.split("-").map(Number); return { y, m, d }; };
  const monthKey = (y, m) => `${pad(y, 4)}-${pad(m)}`;
  const daysInMonth = (y, m) => new Date(Date.UTC(y, m, 0)).getUTCDate(); // m is 1-12
  const weekday = (k) => { const { y, m, d } = parseKey(k); return new Date(Date.UTC(y, m - 1, d)).getUTCDay(); };

  function addDays(k, n) {
    const { y, m, d } = parseKey(k);
    const t = new Date(Date.UTC(y, m - 1, d + n));
    return key(t.getUTCFullYear(), t.getUTCMonth() + 1, t.getUTCDate());
  }
  function addMonths(y, m, n) {
    const i = y * 12 + (m - 1) + n;
    return { y: Math.floor(i / 12), m: (i % 12) + 1 };
  }
  const localKey = (date) => key(date.getFullYear(), date.getMonth() + 1, date.getDate());

  /** 6x7 grid, week starts Sunday. Cells: {key,y,m,d,inMonth}. */
  function monthMatrix(y, m) {
    const first = key(y, m, 1);
    const start = addDays(first, -weekday(first));
    const cells = [];
    for (let i = 0; i < 42; i++) {
      const k = addDays(start, i);
      const p = parseKey(k);
      cells.push({ key: k, y: p.y, m: p.m, d: p.d, inMonth: p.y === y && p.m === m });
    }
    return cells;
  }

  const RSVP_RANK = { needsAction: 3, tentative: 2, accepted: 1, declined: 0 }; // higher = more pending
  /** Normalized RSVP state; missing/unknown -> "accepted". */
  function rsvpState(ev) {
    const r = ev && ev.response;
    return typeof r === "string" && Object.prototype.hasOwnProperty.call(RSVP_RANK, r) ? r : "accepted";
  }
  /** CSS class(es) for the agenda card. */
  function rsvpClass(ev) {
    const r = rsvpState(ev);
    return r === "accepted" ? "rsvp-accepted" : r === "declined" ? "rsvp-declined" : "rsvp-pending" + (r === "tentative" ? " rsvp-tentative" : "");
  }


  // ---- RSVP (C-13). Rust sets Event.self_attendee (invited, not organizer, own primary calendar).
  // Old caches lack it: fall back to "non-accepted, or answered in this session" (`answered` Set of evKey).
  const evKey = (e) => `${e.calendar_id}
${e.id}`;
  const RESPONSES = ["accepted", "declined", "tentative"]; // Google order: 예 / 아니요 / 미정
  const isResponse = (v) => RESPONSES.includes(v);
  function canRespond(ev, answered) {
    if (!ev || ev.status === "cancelled" || typeof ev.id !== "string" || !ev.id || typeof ev.calendar_id !== "string" || !ev.calendar_id) return false;
    // Rust sets self_attendee only for invited (non-organizer) events. Old caches lack it
    // -> fall back to the "answered here / non-accepted" heuristic.
    if (typeof ev.self_attendee === "boolean") return ev.self_attendee;
    return rsvpState(ev) !== "accepted" || !!(answered && answered.has(evKey(ev)));
  }
  /** Menu options in display order; `checked` marks the current response (none for needsAction). */
  function rsvpOptions(ev) {
    const cur = rsvpState(ev);
    return RESPONSES.map((value) => ({ value, checked: value === cur }));
  }
  /** Replace `updated` in a flat event list: same id+calendar_id -> whole object; other copies of the same id -> response only. */
  function mergeEvent(events, updated) {
    return (events || []).map((e) => {
      if (e.id !== updated.id) return e;
      if (e.calendar_id === updated.calendar_id) return updated;
      return Object.assign({}, e, { response: updated.response });
    });
  }
  /** Map a calendar_respond rejection to {kind, text}. */
  function mapRespondError(raw, S) {
    const r = String(raw && raw.message ? raw.message : raw || "").toLowerCase();
    if (r.includes("needs_reauth")) return { kind: "reauth", text: S.reauthBanner };
    if (r.includes("not_attendee")) return { kind: "other", text: S.errNotAttendee };
    if (r.includes("network")) return { kind: "other", text: S.errNetwork };
    return { kind: "other", text: S.errRespond };
  }

  /** Same id across several calendars -> one event (first one wins, incl. its color; response = most pending). Drops cancelled. */
  function dedupe(events) {
    const at = new Map();
    const out = [];
    for (const e of events || []) {
      if (!e || e.status === "cancelled") continue;
      if (at.has(e.id)) {
        const i = at.get(e.id);
        if (RSVP_RANK[rsvpState(e)] > RSVP_RANK[rsvpState(out[i])]) out[i] = Object.assign({}, out[i], { response: rsvpState(e) });
        // Respond through the copy on my own primary calendar (the only one flagged self_attendee).
        if (e.self_attendee === true && out[i].self_attendee !== true)
          out[i] = Object.assign({}, out[i], { self_attendee: true, calendar_id: e.calendar_id, response: rsvpState(e) });
        continue;
      }
      at.set(e.id, out.length);
      out.push(e);
    }
    return out;
  }

  /** Inclusive list of local day keys the event covers (all-day end is exclusive; timed end at 00:00 exclusive). */
  function eventDays(ev) {
    let first, last;
    if (ev.all_day) {
      first = ev.start.slice(0, 10);
      last = addDays(ev.end.slice(0, 10), -1);
    } else {
      const s = new Date(ev.start), e = new Date(ev.end);
      if (isNaN(s) || isNaN(e)) return [];
      first = localKey(s);
      last = e > s ? localKey(new Date(e.getTime() - 1)) : first;
    }
    if (last < first) last = first;
    const days = [];
    for (let k = first, n = 0; k <= last && n < 400; k = addDays(k, 1), n++) days.push(k);
    return days;
  }

  const startMs = (e) => (e.all_day ? 0 : new Date(e.start).getTime());

  /** Map dayKey -> events sorted (all-day first, then start time, then title). */
  function indexByDay(events) {
    const idx = new Map();
    for (const ev of dedupe(events)) {
      for (const k of eventDays(ev)) {
        if (!idx.has(k)) idx.set(k, []);
        idx.get(k).push(ev);
      }
    }
    for (const list of idx.values()) {
      list.sort((a, b) =>
        (b.all_day - a.all_day) || (startMs(a) - startMs(b)) || String(a.summary).localeCompare(String(b.summary)));
    }
    return idx;
  }

  /** Up to MAX_DOTS distinct calendar colors for a day's events. */
  function dotColors(list) {
    const out = [];
    for (const e of list || []) {
      if (rsvpState(e) === "declined") continue; // declined is not busy
      const c = safeColor(e.calendar_color);
      if (!out.includes(c)) out.push(c);
      if (out.length >= MAX_DOTS) break;
    }
    return out;
  }

  function hm(date, S, withPeriod) {
    const h = date.getHours(), mi = date.getMinutes();
    const h12 = h % 12 === 0 ? 12 : h % 12;
    const t = `${h12}:${pad(mi)}`;
    return withPeriod ? `${h < 12 ? S.am : S.pm} ${t}` : t;
  }

  /** Time label for an event as shown under day `dayKey`. */
  function formatRange(ev, dayKey, S) {
    if (ev.all_day) return S.allDay;
    const s = new Date(ev.start), e = new Date(ev.end);
    if (isNaN(s) || isNaN(e)) return "";
    const sKey = localKey(s);
    const eKey = e > s ? localKey(new Date(e.getTime() - 1)) : sKey;
    if (sKey !== eKey) {
      // Crosses midnight: show the part that belongs to this day.
      if (dayKey === sKey) return `${hm(s, S, true)} ${S.multiDayArrow}`;
      if (dayKey === eKey) return `${S.multiDayArrow} ${hm(e, S, true)}`;
      return S.allDay;
    }
    if (e.getTime() === s.getTime()) return hm(s, S, true);
    const samePeriod = (s.getHours() < 12) === (e.getHours() < 12);
    return `${hm(s, S, true)} – ${hm(e, S, !samePeriod)}`;
  }

  function formatClock(date, S) {
    return `${hm(date, S, true)}:${pad(date.getSeconds())}`;
  }

  /** Map a backend error string (never shown raw) to a kind + short Korean message. */
  function mapError(raw, S) {
    const r = String(raw || "").toLowerCase();
    if (r.includes("need login") || r.includes("unauthorized") || r.includes("invalid_grant") || r.includes("auth error"))
      return { kind: "login", text: S.loginTitle };
    if (r.includes("network")) return { kind: "offline", text: S.errNetwork };
    if (r.includes("forbidden")) return { kind: "other", text: S.errForbidden };
    if (r.includes("rate limit")) return { kind: "other", text: S.errRate };
    if (r.includes("server error")) return { kind: "other", text: S.errServer };
    return { kind: "other", text: S.errOther };
  }

  /** Mirrors src-tauri openurl.rs validate_url exactly: https + exact lowercase host, no port/userinfo. */
  function isSafeLink(url) {
    if (typeof url !== "string" || url.length > 2048) return false; // mirrors Rust (D-03)
    // Raw-string checks first (URL parsing would normalize these away).
    if (/[\u0000-\u001f\u007f-\u009f\s\\]/u.test(url)) return false;
    if (!url.startsWith("https://")) return false; // case-sensitive scheme
    const authority = url.slice(8).split(/[/?#]/)[0];
    const anyPath = authority === "calendar.google.com" || authority === "meet.google.com";
    if (!anyPath && authority !== "www.google.com") return false;
    try {
      const u = new URL(url);
      if (u.protocol !== "https:" || u.username !== "" || u.password !== "" || u.port !== "" || u.hostname !== authority) return false;
      if (authority === "www.google.com" && u.pathname !== "/calendar" && !u.pathname.startsWith("/calendar/")) return false;
      return true;
    } catch { return false; }
  }

  /** Only #rgb / #rrggbb survive; anything else (url(), names, expressions) becomes the default color. */
  function safeColor(c) {
    return typeof c === "string" && /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/.test(c) ? c : DEFAULT_COLOR;
  }

  const api = { DEFAULT_COLOR, safeColor, MAX_DOTS, key, parseKey, monthKey, daysInMonth, weekday, addDays, addMonths, localKey,
    monthMatrix, dedupe, rsvpState, rsvpClass, eventDays, indexByDay, dotColors, formatRange, formatClock, mapError, isSafeLink,
    evKey, RESPONSES, isResponse, canRespond, rsvpOptions, mergeEvent, mapRespondError };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.CalLogic = api;
})(typeof window !== "undefined" ? window : globalThis);
