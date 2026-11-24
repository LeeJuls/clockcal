// Dev-only fake of the Tauri IPC bridge. Fixture shapes mirror src-tauri/src/calendar.rs (MonthResult / Event).
// Query: ?s=normal|stale|login|noclient|error|slow   &now=2026-11-24T15:42:07+09:00
(function () {
  const q = new URLSearchParams(location.search);
  const scenario = q.get("s") || "normal";

  // Frozen "now" (still ticks) so screenshots are reproducible.
  const fixed = new Date(q.get("now") || "2026-11-24T15:42:07+09:00").getTime();
  const real0 = Date.now();
  const RealDate = Date;
  class FakeDate extends RealDate {
    constructor(...a) { if (a.length === 0) super(fixed + (RealDate.now() - real0)); else super(...a); }
    static now() { return fixed + (RealDate.now() - real0); }
  }
  window.Date = FakeDate;

  const BLUE = "#4285f4", GREEN = "#0b8043", RED = "#d50000", PURPLE = "#8e24aa", ORANGE = "#f4511e";
  const cal = (id, name, c, primary) => ({ id, summary: name, background_color: c, foreground_color: "#ffffff", primary, selected: true, access_role: "owner" });
  const calendars = [cal("me@example.com", "내 캘린더", BLUE, true), cal("team@example.com", "팀 일정", GREEN, false),
    cal("hol@group.v.calendar.google.com", "대한민국 휴일", RED, false)];

  let n = 0;
  const ev = (o) => Object.assign({ id: "e" + (++n), calendar_id: "me@example.com", calendar_name: "내 캘린더", calendar_color: BLUE,
    summary: "", start: "", end: "", all_day: false, location: null, meet_url: null,
    html_link: "https://calendar.google.com/calendar/event?eid=" + n, status: "confirmed" }, o);
  const team = { calendar_id: "team@example.com", calendar_name: "팀 일정", calendar_color: GREEN };

  const events = [
    ev({ summary: "주간 스프린트 회의", start: "2026-11-24T10:00:00+09:00", end: "2026-11-24T11:00:00+09:00", meet_url: "https://meet.google.com/abc-defg-hij" }),
    // duplicate id across two calendars: UI must show it once, with the first calendar's color
    ev({ id: "dup1", summary: "디자인 리뷰", start: "2026-11-24T14:00:00+09:00", end: "2026-11-24T15:30:00+09:00", location: "본관 3층 회의실 B" }),
    ev({ id: "dup1", summary: "디자인 리뷰", start: "2026-11-24T14:00:00+09:00", end: "2026-11-24T15:30:00+09:00", location: "본관 3층 회의실 B", ...team }),
    ev({ summary: "프로젝트 마감", start: "2026-11-24", end: "2026-11-25", all_day: true, ...team }),
    ev({ summary: "1:1 면담 — 아주 아주 긴 제목이 들어와도 한 줄에서 말줄임표로 잘려야 하는 일정입니다", start: "2026-11-24T16:30:00+09:00", end: "2026-11-24T17:00:00+09:00", location: "https://zoom.example/j/1234567890 (온라인)", meet_url: "https://meet.google.com/zzz-yyyy-xxx" }),
    ev({ summary: "야간 배포", start: "2026-11-24T23:00:00+09:00", end: "2026-11-25T01:30:00+09:00", ...team }),
    // multi-day all-day: 11/26..11/28 (end exclusive)
    ev({ summary: "팀 워크숍 (제주)", start: "2026-11-26", end: "2026-11-29", all_day: true, calendar_color: PURPLE, calendar_name: "팀 일정", calendar_id: "team@example.com" }),
    ev({ summary: "스탠드업", start: "2026-11-26T09:30:00+09:00", end: "2026-11-26T09:45:00+09:00", meet_url: "https://meet.google.com/aaa-bbbb-ccc" }),
    ev({ summary: "출장 항공편", start: "2026-11-26T06:50:00+09:00", end: "2026-11-26T08:10:00+09:00", calendar_color: ORANGE, calendar_name: "개인", calendar_id: "p" }),
    ev({ summary: "점심 약속", start: "2026-11-20T12:00:00+09:00", end: "2026-11-20T13:00:00+09:00", location: "서울 강남구" }),
    ev({ summary: "월말 정산", start: "2026-11-30T15:00:00+09:00", end: "2026-11-30T16:00:00+09:00" }),
    ev({ summary: "12월 킥오프", start: "2026-12-01T10:00:00+09:00", end: "2026-12-01T11:30:00+09:00", ...team }),
    ev({ summary: "성탄절", start: "2026-12-25", end: "2026-12-26", all_day: true, calendar_color: RED, calendar_name: "대한민국 휴일", calendar_id: "hol@group.v.calendar.google.com" }),
    ev({ id: "gone", status: "cancelled", summary: "취소된 일정(표시되면 안 됨)", start: "2026-11-25T10:00:00+09:00", end: "2026-11-25T11:00:00+09:00" }),
    ev({ summary: "10월 말 일정", start: "2026-10-29T10:00:00+09:00", end: "2026-10-29T11:00:00+09:00" }),
  ];

  // ?s=evil adds hostile fixtures: CSS-injection color, non-allowlisted Meet/html links (must render inert).
  if (scenario === "evil") {
    events.push(
      ev({ summary: "악성 색상 일정", start: "2026-11-24T18:00:00+09:00", end: "2026-11-24T19:00:00+09:00", calendar_color: "url(https://evil.example/c.png)" }),
      ev({ summary: "비허용 Meet 링크", start: "2026-11-24T19:30:00+09:00", end: "2026-11-24T20:00:00+09:00", meet_url: "https://meet.google.com.evil.com/x", html_link: "https://drive.google.com/x" }),
    );
  }

  // ?s=rsvp adds one event per response state on 11/24 (plus a declined-only day 11/27 -> must show no dot).
  if (["rsvp", "reauth", "notattendee", "respfail"].includes(scenario)) { // reauth/notattendee/respfail reuse the rsvp fixtures
    const r = (summary, h, response, o) => ev(Object.assign({ summary, response, start: `2026-11-24T${h}:00:00+09:00`, end: `2026-11-24T${h}:50:00+09:00` }, o));
    events.push(
      r("[수락] 기획 리뷰", "08", "accepted"),
      r("[응답 대기] 신규 입사자 환영 미팅", "12", "needsAction", { calendar_color: GREEN, calendar_name: "팀 일정", location: "본관 2층" }),
      r("[미정] 파트너 사 미팅", "13", "tentative", { calendar_color: ORANGE }),
      r("[거절] 전체 회식", "19", "declined", { calendar_color: PURPLE }),
      ev({ summary: "[거절] 휴일 워크숍", response: "declined", start: "2026-11-27T10:00:00+09:00", end: "2026-11-27T11:00:00+09:00" }),
    );
  }

  function monthEvents(y, m) {
    const lo = new Date(y, m - 1, 1).getTime(), hi = new Date(y, m, 1).getTime();
    return events.filter((e) => {
      const s = e.all_day ? new Date(e.start + "T00:00:00").getTime() : new RealDate(e.start).getTime();
      const t = e.all_day ? new Date(e.end + "T00:00:00").getTime() : new RealDate(e.end).getTime();
      return s < hi && t > lo;
    });
  }

  const delay = (ms) => new Promise((r) => setTimeout(r, ms));
  window.__MOCK_LOG = [];
  const handlers = {
    async calendar_month(a) {
      window.__MOCK_LOG.push(["calendar_month", a.year, a.month, !!a.force]);
      await delay(scenario === "slow" ? 2500 : 60);
      if (scenario === "login") throw "need login";
      if (scenario === "noclient") throw "need login";
      if (scenario === "error") throw "network error: dns";
      const base = { profile: a.profile, year: a.year, month: a.month, fetched_at: Math.floor(RealDate.now() / 1000),
        calendars, events: monthEvents(a.year, a.month), skipped_calendars: [] };
      if (scenario === "stale") return { ...base, source: "stale", stale: true, error: "network error: connect" };
      return { ...base, source: a.force ? "fetched" : "cache", stale: false, error: null };
    },
    async auth_status() { return { profile: "personal", client_configured: scenario !== "noclient", logged_in: false, can_respond: scenario !== "reauth" }; },
    // ?s=reauth -> needs_reauth, ?s=notattendee -> not_attendee, ?s=respfail -> network error, else success after ~400ms.
    async calendar_respond(a) {
      window.__MOCK_LOG.push(["calendar_respond", a.calendarId, a.eventId, a.response]);
      await delay(Number(q.get("rd") || 400)); // ?rd=ms to hold the pending state for screenshots
      if (scenario === "reauth") throw "needs_reauth";
      if (scenario === "notattendee") throw "not_attendee";
      if (scenario === "respfail") throw "network error: dns";
      const e = events.find((x) => x.id === a.eventId && x.calendar_id === a.calendarId);
      if (!e) throw "not_attendee";
      e.response = a.response;
      return { ...e };
    },
    async auth_login() { await delay(800); return { expires_in_s: 3600 }; },
    async auth_logout() { return true; },
    async auth_import_client(a) { await delay(200); try { if (!JSON.parse(a.json).installed) throw 0; } catch (_) { throw "client config: not valid json"; } return null; },
    async "open_url"(a) { window.__MOCK_LOG.push(["open_url", a.url]); },
  };
  window.__TAURI_INTERNALS__ = {
    invoke(cmd, args) {
      const f = handlers[cmd];
      return f ? f(args || {}) : Promise.reject("unknown command " + cmd);
    },
  };
})();
