// Plain node tests: node dev/logic.test.cjs  (no dependencies). Run under several TZs: TZ=Asia/Seoul / America/New_York / UTC.
const assert = require("node:assert");
// results come from another vm realm: compare through JSON, keep equal() strict.
const deq = assert.deepStrictEqual;
assert.deepEqual = (a, b) => deq(JSON.parse(JSON.stringify(a)), b);
assert.equal = assert.strictEqual;
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");
// src/*.js are classic browser scripts (package.json is "type":"module"), so load them with a fake `module`.
function load(f) {
  const m = { exports: {} };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, "..", "src", f), "utf8"), { module: m, Date, URL, String, Math, Number, Set, Map });
  return m.exports;
}
const L = load("logic.js");
const S = load("strings.js");

let pass = 0;
const t = (name, fn) => { try { fn(); pass++; console.log("ok   " + name); } catch (e) { console.log("FAIL " + name + "\n     " + e.message); process.exitCode = 1; } };

t("weekday/Sunday start: 2026-11-01 is Sunday, first cell", () => {
  const m = L.monthMatrix(2026, 11);
  assert.equal(m.length, 42);
  assert.equal(m[0].key, "2026-11-01");
  assert.equal(L.weekday(m[0].key), 0);
  assert.ok(m.every((c, i) => L.weekday(c.key) === i % 7));
});
t("month starting Wednesday (2026-07) leads with Jun 28-30", () => {
  const m = L.monthMatrix(2026, 7);
  assert.deepEqual(m.slice(0, 4).map((c) => c.key), ["2026-06-28", "2026-06-29", "2026-06-30", "2026-07-01"]);
  assert.equal(m[0].inMonth, false); assert.equal(m[3].inMonth, true);
});
t("February leap year: 2028 has 29 days, 2026/2100 have 28/28", () => {
  assert.equal(L.daysInMonth(2028, 2), 29);
  assert.equal(L.daysInMonth(2026, 2), 28);
  assert.equal(L.daysInMonth(2100, 2), 28);
  const m = L.monthMatrix(2028, 2);
  assert.equal(m.filter((c) => c.inMonth).length, 29);
  assert.ok(m.some((c) => c.key === "2028-02-29" && c.inMonth));
  assert.equal(L.addDays("2028-02-28", 1), "2028-02-29");
  assert.equal(L.addDays("2026-02-28", 1), "2026-03-01");
});
t("month boundaries: year wrap via addMonths", () => {
  assert.deepEqual(L.addMonths(2026, 12, 1), { y: 2027, m: 1 });
  assert.deepEqual(L.addMonths(2026, 1, -1), { y: 2025, m: 12 });
  assert.deepEqual(L.addMonths(2026, 3, -15), { y: 2024, m: 12 });
  assert.equal(L.addDays("2026-12-31", 1), "2027-01-01");
});
t("grid covers whole month for every month of 2026-2029", () => {
  for (let y = 2026; y <= 2029; y++) for (let mo = 1; mo <= 12; mo++) {
    const m = L.monthMatrix(y, mo);
    assert.equal(m.filter((c) => c.inMonth).length, L.daysInMonth(y, mo), `${y}-${mo}`);
    for (let i = 1; i < 42; i++) assert.equal(m[i].key, L.addDays(m[i - 1].key, 1));
  }
});
t("DST-safe day math (US spring forward / fall back, KR none)", () => {
  assert.equal(L.addDays("2026-03-08", 1), "2026-03-09");
  assert.equal(L.addDays("2026-11-01", 1), "2026-11-02");
  assert.equal(L.addDays("2026-03-07", 2), "2026-03-09");
  let k = "2026-01-01"; for (let i = 0; i < 365; i++) k = L.addDays(k, 1);
  assert.equal(k, "2027-01-01");
});
t("all-day: end date is exclusive", () => {
  assert.deepEqual(L.eventDays({ all_day: true, start: "2026-11-24", end: "2026-11-25" }), ["2026-11-24"]);
  assert.deepEqual(L.eventDays({ all_day: true, start: "2026-11-26", end: "2026-11-29" }), ["2026-11-26", "2026-11-27", "2026-11-28"]);
  assert.deepEqual(L.eventDays({ all_day: true, start: "2026-12-30", end: "2027-01-02" }), ["2026-12-30", "2026-12-31", "2027-01-01"]);
  assert.deepEqual(L.eventDays({ all_day: true, start: "2028-02-28", end: "2028-03-01" }), ["2028-02-28", "2028-02-29"]);
});
t("timed events: local-day span, midnight end exclusive, crossing midnight", () => {
  const mk = (s, e) => ({ all_day: false, start: s, end: e });
  const d = (y, mo, da, h, mi) => new Date(y, mo - 1, da, h, mi).toISOString();
  assert.deepEqual(L.eventDays(mk(d(2026, 11, 24, 10, 0), d(2026, 11, 24, 11, 0))), ["2026-11-24"]);
  assert.deepEqual(L.eventDays(mk(d(2026, 11, 24, 23, 0), d(2026, 11, 25, 1, 30))), ["2026-11-24", "2026-11-25"]);
  assert.deepEqual(L.eventDays(mk(d(2026, 11, 24, 22, 0), d(2026, 11, 25, 0, 0))), ["2026-11-24"]);
  assert.deepEqual(L.eventDays(mk(d(2026, 11, 24, 10, 0), d(2026, 11, 24, 10, 0))), ["2026-11-24"]);
  assert.equal(L.eventDays(mk("garbage", "garbage")).length, 0);
});
t("dedupe: same id across calendars -> first wins; cancelled dropped", () => {
  const evs = [
    { id: "a", calendar_color: "#111", status: "confirmed" },
    { id: "a", calendar_color: "#222", status: "confirmed" },
    { id: "b", calendar_color: "#333", status: "cancelled" },
    { id: "c", calendar_color: null, status: "confirmed" },
  ];
  const out = L.dedupe(evs);
  assert.deepEqual(out.map((e) => e.id), ["a", "c"]);
  assert.equal(out[0].calendar_color, "#111");
});
t("indexByDay: multi-day shows on each day, all-day first, sorted by start", () => {
  const evs = [
    { id: "1", summary: "late", all_day: false, start: new Date(2026, 10, 26, 15).toISOString(), end: new Date(2026, 10, 26, 16).toISOString(), status: "confirmed" },
    { id: "2", summary: "early", all_day: false, start: new Date(2026, 10, 26, 9).toISOString(), end: new Date(2026, 10, 26, 10).toISOString(), status: "confirmed" },
    { id: "3", summary: "ws", all_day: true, start: "2026-11-26", end: "2026-11-29", status: "confirmed" },
  ];
  const idx = L.indexByDay(evs);
  assert.deepEqual(idx.get("2026-11-26").map((e) => e.summary), ["ws", "early", "late"]);
  assert.deepEqual(idx.get("2026-11-27").map((e) => e.summary), ["ws"]);
  assert.deepEqual(idx.get("2026-11-28").map((e) => e.summary), ["ws"]);
  assert.equal(idx.get("2026-11-29"), undefined);
});
t("dotColors: max 3, distinct, default color", () => {
  const mk = (c) => ({ calendar_color: c });
  assert.deepEqual(L.dotColors([mk("#111"), mk("#111"), mk("#222")]), ["#111", "#222"]);
  assert.equal(L.dotColors([mk("#111"), mk("#222"), mk("#333"), mk("#444")]).length, 3);
  assert.deepEqual(L.dotColors([mk(null)]), [L.DEFAULT_COLOR]);
  assert.deepEqual(L.dotColors([mk("url(https://evil.example/c.png)"), mk("red")]), [L.DEFAULT_COLOR]);
  assert.deepEqual(L.dotColors(undefined), []);
});
t("time formatting (Korean 12h, same-period collapse, all-day)", () => {
  const D = (h, mi, d = 24) => new Date(2026, 10, d, h, mi).toISOString();
  const r = (s, e, day = "2026-11-24") => L.formatRange({ all_day: false, start: s, end: e }, day, S);
  assert.equal(r(D(10, 0), D(11, 0)), "오전 10:00 – 11:00");
  assert.equal(r(D(11, 30), D(13, 0)), "오전 11:30 – 오후 1:00");
  assert.equal(r(D(14, 0), D(15, 30)), "오후 2:00 – 3:30");
  assert.equal(r(D(0, 0), D(0, 30)), "오전 12:00 – 12:30");
  assert.equal(r(D(12, 0), D(12, 45)), "오후 12:00 – 12:45");
  assert.equal(r(D(9, 0), D(9, 0)), "오전 9:00");
  assert.equal(L.formatRange({ all_day: true, start: "2026-11-24", end: "2026-11-25" }, "2026-11-24", S), "종일");
  assert.equal(r(D(23, 0), D(1, 30, 25), "2026-11-24"), "오후 11:00 →");
  assert.equal(r(D(23, 0), D(1, 30, 25), "2026-11-25"), "→ 오전 1:30");
  assert.equal(L.formatClock(new Date(2026, 10, 24, 15, 42, 7), S), "오후 3:42:07");
  assert.equal(L.formatClock(new Date(2026, 10, 24, 0, 5, 9), S), "오전 12:05:09");
});
t("error mapping never leaks raw text; login kind detected", () => {
  assert.equal(L.mapError("need login", S).kind, "login");
  assert.equal(L.mapError("auth error: invalid_grant {token:abc}", S).kind, "login");
  for (const raw of ["network error: dns", "forbidden (403)", "rate limited (429/403 rate limit)", "google server error (http 503)", "weird {\"secret\":\"x\"}", "http 418"]) {
    const m = L.mapError(raw, S);
    assert.ok(!m.text.includes("{") && !m.text.includes("http"), raw);
  }
  assert.equal(L.mapError("network error: dns", S).kind, "offline");
});
t("isSafeLink: only https google hosts", () => {
  assert.ok(L.isSafeLink("https://calendar.google.com/calendar/event?eid=x"));
  assert.ok(L.isSafeLink("https://meet.google.com/abc-defg-hij"));
  assert.ok(!L.isSafeLink("http://calendar.google.com/x"));
  assert.ok(!L.isSafeLink("https://evilgoogle.com/x"));
  assert.ok(!L.isSafeLink("https://google.com.evil.example/x"));
  assert.ok(!L.isSafeLink("javascript:alert(1)"));
  assert.ok(!L.isSafeLink(""));
});
t("isSafeLink mirrors Rust allowlist (openurl.rs)", () => {
  for (const u of [
    "https://calendar.google.com/calendar/event?eid=abc", "https://calendar.google.com/", "https://calendar.google.com",
    "https://meet.google.com/abc-defg-hij", "https://meet.google.com/abc-defg-hij?authuser=1",
    "https://www.google.com/calendar", "https://www.google.com/calendar/render", "https://www.google.com/calendar/event?eid=1",
  ]) assert.ok(L.isSafeLink(u), u);
  for (const u of [
    "https://drive.google.com/x", "https://g.co/x", "https://accounts.google.com/", "http://calendar.google.com/",
    "https://calendar.google.com.evil.com/", "https://calendar.google.com@evil.com/", "https://evil.com@calendar.google.com/",
    "https://user:pw@calendar.google.com/", "https://calendar.google.com:443/", "https://calendar.google.com:8443/",
    "https://CALENDAR.GOOGLE.COM/", "HTTPS://calendar.google.com/", "https://calendar.google.com\\@evil.com/",
    "https://calendar.google.com%2f@evil.com/", "https://calendar.google.com /", " https://calendar.google.com/",
    "https://calendar.google.com/\n", "javascript:alert(1)", "file:///C:/x", "data:text/html,hi", "//calendar.google.com/",
    "calendar.google.com", "https://www.google.com/", "https://www.google.com/search?q=x", "https://www.google.com/calendarx",
    "https://www.google.com/calendar/../search", "", undefined, null, 5,
  ]) assert.ok(!L.isSafeLink(u), String(u));
});
t("safeColor: only #rgb/#rrggbb", () => {
  assert.equal(L.safeColor("#fff"), "#fff");
  assert.equal(L.safeColor("#A1B2C3"), "#A1B2C3");
  for (const c of ["url(https://evil.example/c.png)", "red", "expression(alert(1))", "#12", "#12345g", "#1234", "#fff;x", " #fff", "", null, undefined, 5])
    assert.equal(L.safeColor(c), L.DEFAULT_COLOR, String(c));
});

const E = (o) => Object.assign({ id: "x", status: "confirmed", calendar_color: "#0b8043", all_day: false,
  start: "2026-11-24T10:00:00+09:00", end: "2026-11-24T11:00:00+09:00" }, o);
t("rsvpState/rsvpClass: missing/unknown -> accepted; classes per state", () => {
  for (const r of [undefined, null, "", "bogus", 5, "toString", "accepted"]) {
    assert.equal(L.rsvpState({ response: r }), "accepted", String(r));
    assert.equal(L.rsvpClass({ response: r }), "rsvp-accepted");
  }
  assert.equal(L.rsvpState(null), "accepted");
  assert.equal(L.rsvpClass({ response: "needsAction" }), "rsvp-pending");
  assert.equal(L.rsvpClass({ response: "tentative" }), "rsvp-pending rsvp-tentative");
  assert.equal(L.rsvpClass({ response: "declined" }), "rsvp-declined");
});
t("dotColors: declined events skipped; declined-only day has no dots", () => {
  assert.deepEqual(L.dotColors([E({ response: "declined", calendar_color: "#111111" }), E({ calendar_color: "#222222" })]), ["#222222"]);
  assert.deepEqual(L.dotColors([E({ response: "declined" })]), []);
  assert.deepEqual(L.dotColors([E({ response: "needsAction", calendar_color: "#333333" }), E({ calendar_color: "#444444" })]), ["#333333", "#444444"]);
});
t("dedupe: same id, different response -> first color, most pending response", () => {
  const a = E({ id: "d", response: "accepted", calendar_color: "#aa0000" });
  const b = E({ id: "d", response: "needsAction", calendar_color: "#00bb00" });
  const c = E({ id: "d", response: "tentative", calendar_color: "#0000cc" });
  const o = L.dedupe([a, b, c]);
  assert.equal(o.length, 1); assert.equal(o[0].calendar_color, "#aa0000"); assert.equal(o[0].response, "needsAction");
  assert.equal(a.response, "accepted"); // input not mutated
  assert.equal(L.dedupe([E({ id: "d", response: "declined" }), E({ id: "d", response: "tentative" })])[0].response, "tentative");
  assert.equal(L.dedupe([E({ id: "d", response: "tentative" }), E({ id: "d", response: "declined" })])[0].response, "tentative");
  assert.equal(L.rsvpState(L.dedupe([E({ id: "d", response: "declined" }), E({ id: "d" })])[0]), "accepted");
});

t("canRespond: invitations yes; own/accepted no unless answered here; cancelled/missing ids no", () => {
  const ans = new Set();
  assert.equal(L.canRespond(E({ id: "a", calendar_id: "c", response: "needsAction" }), ans), true);
  assert.equal(L.canRespond(E({ id: "a", calendar_id: "c", response: "tentative" }), ans), true);
  assert.equal(L.canRespond(E({ id: "a", calendar_id: "c", response: "declined" }), ans), true);
  assert.equal(L.canRespond(E({ id: "a", calendar_id: "c", response: "accepted" }), ans), false);
  assert.equal(L.canRespond(E({ id: "a", calendar_id: "c" }), ans), false);
  ans.add(L.evKey({ id: "a", calendar_id: "c" }));
  assert.equal(L.canRespond(E({ id: "a", calendar_id: "c", response: "accepted" }), ans), true);
  assert.equal(L.canRespond(E({ id: "a", calendar_id: "other", response: "accepted" }), ans), false);
  assert.equal(L.canRespond(E({ id: "a", calendar_id: "c", response: "needsAction", status: "cancelled" }), ans), false);
  assert.equal(L.canRespond(E({ id: "", calendar_id: "c", response: "needsAction" }), ans), false);
  assert.equal(L.canRespond(E({ id: "a", response: "needsAction" }), ans), false);
  assert.equal(L.canRespond(null, ans), false);
});
t("rsvpOptions: accepted/declined/tentative order, current checked, needsAction none", () => {
  assert.deepEqual(L.rsvpOptions({ response: "accepted" }), [{ value: "accepted", checked: true }, { value: "declined", checked: false }, { value: "tentative", checked: false }]);
  assert.deepEqual(L.rsvpOptions({ response: "tentative" }).map((o) => o.checked), [false, false, true]);
  assert.deepEqual(L.rsvpOptions({ response: "declined" }).map((o) => o.checked), [false, true, false]);
  assert.deepEqual(L.rsvpOptions({ response: "needsAction" }).map((o) => o.checked), [false, false, false]);
  assert.equal(S.rsvpYes + "|" + S.rsvpNo + "|" + S.rsvpMaybe, "예|아니요|미정");
});
t("isResponse: only accepted/declined/tentative", () => {
  for (const v of ["accepted", "declined", "tentative"]) assert.equal(L.isResponse(v), true);
  for (const v of ["needsAction", "", "x", undefined, null]) assert.equal(L.isResponse(v), false);
});
t("mergeEvent: id+calendar_id replaced whole; same id elsewhere gets response; others untouched; no mutation", () => {
  const a = E({ id: "m", calendar_id: "c1", response: "needsAction", summary: "old" });
  const b = E({ id: "m", calendar_id: "c2", response: "needsAction", summary: "copy" });
  const c = E({ id: "z", calendar_id: "c1", response: "needsAction" });
  const upd = E({ id: "m", calendar_id: "c1", response: "accepted", summary: "new" });
  const out = L.mergeEvent([a, b, c], upd);
  assert.equal(out.length, 3);
  assert.equal(out[0].summary, "new"); assert.equal(out[0].response, "accepted");
  assert.equal(out[1].summary, "copy"); assert.equal(out[1].response, "accepted");
  assert.equal(out[2].response, "needsAction");
  assert.equal(a.response, "needsAction"); assert.equal(b.response, "needsAction");
  assert.deepEqual(L.mergeEvent(null, upd), []);
});
t("after merge, dedupe/dotColors/rsvpClass reflect the new response", () => {
  const a = E({ id: "m", calendar_id: "c1", response: "needsAction", calendar_color: "#111111" });
  const b = E({ id: "m", calendar_id: "c2", response: "needsAction", calendar_color: "#222222" });
  const d = L.dedupe(L.mergeEvent([a, b], E({ id: "m", calendar_id: "c1", response: "declined", calendar_color: "#111111" })));
  assert.equal(d.length, 1); assert.equal(L.rsvpClass(d[0]), "rsvp-declined");
  assert.deepEqual(L.dotColors(d), []);
  const acc = L.dedupe(L.mergeEvent([a, b], E({ id: "m", calendar_id: "c1", response: "accepted", calendar_color: "#111111" })));
  assert.equal(L.rsvpClass(acc[0]), "rsvp-accepted");
});
t("mapRespondError: needs_reauth / not_attendee / network / other (raw never shown)", () => {
  assert.equal(L.mapRespondError("needs_reauth", S).kind, "reauth");
  assert.equal(L.mapRespondError("needs_reauth", S).text, S.reauthBanner);
  assert.equal(L.mapRespondError(new Error("needs_reauth"), S).kind, "reauth");
  assert.equal(L.mapRespondError("not_attendee", S).text, S.errNotAttendee);
  assert.equal(L.mapRespondError("network error: dns", S).text, S.errNetwork);
  const o = L.mapRespondError("boom: secret-token-123", S);
  assert.equal(o.kind, "other"); assert.equal(o.text, S.errRespond);
  assert.equal(L.mapRespondError(undefined, S).text, S.errRespond);
});

t("canRespond: self_attendee flag wins over response heuristic", () => {
  const E = (o) => Object.assign({ id: "a", calendar_id: "c", status: "confirmed", response: "accepted" }, o);
  assert.equal(L.canRespond(E({ self_attendee: true }), new Set()), true);
  assert.equal(L.canRespond(E({ self_attendee: false, response: "needsAction" }), new Set()), false);
  assert.equal(L.canRespond(E({ self_attendee: true, status: "cancelled" }), new Set()), false);
});

t("dedupe: respond via the primary copy (self_attendee) even if a shared copy came first", () => {
  const base = { id: "x", status: "confirmed", all_day: false, start: "2026-11-25T10:00:00+09:00", end: "2026-11-25T11:00:00+09:00", summary: "m" };
  const shared = Object.assign({}, base, { calendar_id: "col@x", calendar_color: "#111111", self_attendee: false, response: "accepted" });
  const mine = Object.assign({}, base, { calendar_id: "me@x", calendar_color: "#222222", self_attendee: true, response: "needsAction" });
  const out = L.dedupe([shared, mine]);
  assert.equal(out.length, 1);
  assert.equal(out[0].calendar_id, "me@x"); assert.equal(out[0].self_attendee, true);
  assert.equal(out[0].response, "needsAction"); assert.equal(out[0].calendar_color, "#111111");
});

console.log(`\n${pass} passed (TZ=${process.env.TZ || "system"})`);
