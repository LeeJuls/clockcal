// clockcal popup UI (P4-a). Data comes only from Rust commands; no tokens or Google calls here.
(function () {
  "use strict";
  const S = window.CalStrings;
  const L = window.CalLogic;
  // Single Google account. The profile id names the client file and credential entry; it is not shown in the UI.
  const PROFILE = "personal";
  const RETRY_MIN_MS = 60 * 1000;

  // withGlobalTauri is false in tauri.conf.json -> use the core IPC bridge that every Tauri 2 webview has.
  function invoke(cmd, args) {
    const f = (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) ||
      (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke);
    if (!f) return Promise.reject(new Error("no tauri bridge"));
    return f(cmd, args);
  }

  // P4-b: Rust applies the acrylic backdrop; the page only switches to translucent backgrounds.
  window.__clockcalAcrylic = (on) => document.documentElement.classList.toggle("acrylic", !!on);
  invoke("ui_effects").then(window.__clockcalAcrylic).catch(() => {});

  const $ = (id) => document.getElementById(id);
  function h(tag, cls, text) {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  }
  const SVG_NS = "http://www.w3.org/2000/svg";
  function svg(path, size, extra) {
    const s = document.createElementNS(SVG_NS, "svg");
    s.setAttribute("viewBox", "0 0 16 16");
    s.setAttribute("width", size);
    s.setAttribute("height", size);
    s.setAttribute("aria-hidden", "true");
    const p = document.createElementNS(SVG_NS, "path");
    p.setAttribute("d", path);
    p.setAttribute("fill", extra && extra.fill ? "currentColor" : "none");
    if (!(extra && extra.fill)) {
      p.setAttribute("stroke", "currentColor");
      p.setAttribute("stroke-width", "1.3");
      p.setAttribute("stroke-linejoin", "round");
      p.setAttribute("stroke-linecap", "round");
    }
    s.appendChild(p);
    return s;
  }
  const ICON_VIDEO = "M1.5 4.5h8a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-8a1 1 0 0 1-1-1v-5a1 1 0 0 1 1-1zM10.5 7l4-2.2v6.4l-4-2.2";
  const ICON_PIN = "M8 14.5s4.5-4.2 4.5-7.6a4.5 4.5 0 0 0-9 0C3.5 10.300 8 14.500 8 14.500zM8 8.600a1.700 1.700 0 1 0 0-3.400 1.700 1.700 0 0 0 0 3.400z";

  // ------------------------------------------------------------ state
  const now0 = new Date();
  const state = {
    todayKey: L.localKey(now0),
    year: now0.getFullYear(),
    month: now0.getMonth() + 1,
    selected: L.localKey(now0),
    results: new Map(), // "YYYY-MM" -> MonthResult
    needLogin: false,
    clientMissing: false,
    loggingIn: false,
    loadedOnce: new Set(),
    errorText: "",
    refreshing: false,
    lastForceRetry: 0,
    seq: 0,
    index: new Map(),
    answered: new Set(), // evKey of events answered here (accepted ones stay changeable; see L.canRespond)
    pending: new Set(),  // evKey with a calendar_respond in flight
    needReauth: false,
    respondError: "",
  };
  const ICON_CHEVRON = "M3.5 6l4.5 4.5L12.5 6";
  let menu = null; // { el, btn }
  let respondErrTimer = 0;

  function rebuildIndex() {
    const all = [];
    for (const r of state.results.values()) all.push(...r.events);
    state.index = L.indexByDay(all);
  }

  // ------------------------------------------------------------ render
  function renderClock() {
    const d = new Date();
    $("clock").textContent = L.formatClock(d, S);
    $("date").textContent = S.fullDate(d.getFullYear(), d.getMonth() + 1, d.getDate(), S.weekdaysLong[d.getDay()]);
    const k = L.localKey(d);
    if (k !== state.todayKey) { state.todayKey = k; renderAll(); }
  }

  function renderDow() {
    const dow = $("dow");
    dow.textContent = "";
    S.weekdaysShort.forEach((w) => dow.appendChild(h("div", null, w)));
  }

  function renderGrid() {
    $("month-title").textContent = S.monthTitle(state.year, state.month);
    const grid = $("grid");
    grid.textContent = "";
    const cells = L.monthMatrix(state.year, state.month);
    // Drop a trailing row that is entirely next-month days (frees ~38px for the agenda; matters at 520px height).
    const rows = Math.ceil((cells.map((c) => c.inMonth).lastIndexOf(true) + 1) / 7);
    for (const c of cells.slice(0, rows * 7)) {
      const b = h("button", "cell");
      b.type = "button";
      b.dataset.key = c.key;
      if (!c.inMonth) b.classList.add("dim");
      if (L.weekday(c.key) === 0) b.classList.add("sun");
      if (c.key === state.todayKey) b.classList.add("today");
      if (c.key === state.selected) b.classList.add("selected");
      b.setAttribute("aria-label", `${c.y}년 ${c.m}월 ${c.d}일`);
      b.appendChild(h("span", "n", String(c.d)));
      const colors = L.dotColors(state.index.get(c.key));
      if (colors.length) {
        const dots = h("span", "dots");
        colors.forEach((col) => {
          const dot = h("span", "dot");
          dot.style.background = L.safeColor(col);
          dots.appendChild(dot);
        });
        b.appendChild(dots);
      }
      b.addEventListener("click", () => selectDay(c.key));
      grid.appendChild(b);
    }
  }

  function banner() {
    const el = $("banner");
    const r = state.results.get(L.monthKey(state.year, state.month));
    let text = "";
    let action = null;
    if (state.needLogin) text = "";
    else if (state.needReauth) {
      text = S.reauthBanner;
      action = { label: state.loggingIn ? S.loggingIn : S.reauthButton, disabled: state.loggingIn, run: doLogin };
    } else if (state.respondError) text = state.respondError;
    else if (r && r.stale) text = S.staleBanner;
    // Hard errors without any data are shown in the agenda area instead (renderAgenda).
    el.hidden = !text;
    el.textContent = "";
    if (!text) return;
    el.appendChild(h("span", "banner-text", text));
    if (action) {
      const b = h("button", "banner-btn", action.label);
      b.type = "button";
      b.disabled = action.disabled;
      b.addEventListener("click", action.run);
      el.appendChild(b);
    }
  }

  function openLink(url) {
    if (!url || !L.isSafeLink(url)) return;
    // Rust app command (openurl.rs): validates https + host allowlist, then ShellExecute.
    invoke("open_url", { url }).catch(() => toast(S.errOpen));
  }

  let toastTimer = 0;
  function toast(text) {
    const old = document.querySelector(".toast");
    if (old) old.remove();
    const t = h("div", "toast", text);
    $("panel").style.position = "relative";
    $("panel").appendChild(t);
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => t.remove(), 2500);
  }


  // ------------------------------------------------------------ RSVP (C-13)
  function rsvpText(r) {
    return r === "accepted" ? S.rsvpYes : r === "declined" ? S.rsvpNo : r === "tentative" ? S.rsvpMaybe : S.rsvpNeedsAction;
  }

  function closeMenu(refocus) {
    if (!menu) return;
    const m = menu;
    menu = null;
    m.el.remove();
    m.btn.setAttribute("aria-expanded", "false");
    if (refocus && m.btn.isConnected) m.btn.focus();
  }
  document.addEventListener("mousedown", (e) => { if (menu && !menu.el.contains(e.target) && !menu.btn.contains(e.target)) closeMenu(false); }, true);
  window.addEventListener("blur", () => closeMenu(false));
  document.addEventListener("scroll", () => closeMenu(false), true);

  function openMenu(ev, btn) {
    closeMenu(false);
    const el = h("div", "rsvp-menu");
    el.setAttribute("role", "menu");
    el.setAttribute("aria-label", S.rsvpLabel);
    const items = L.rsvpOptions(ev).map((o) => {
      const b = h("button", "rsvp-opt");
      b.type = "button";
      b.setAttribute("role", "menuitemradio");
      b.setAttribute("aria-checked", String(o.checked));
      b.tabIndex = -1;
      b.appendChild(h("span", "rsvp-check", o.checked ? "\u2713" : ""));
      b.appendChild(h("span", null, rsvpText(o.value)));
      b.addEventListener("click", (e) => { e.stopPropagation(); closeMenu(true); respond(ev, o.value); });
      el.appendChild(b);
      return b;
    });
    el.addEventListener("click", (e) => e.stopPropagation());
    el.addEventListener("keydown", (e) => {
      // Nothing inside the menu may reach the day-navigation handler or the Esc/hide logic.
      e.stopPropagation();
      const i = items.indexOf(document.activeElement);
      if (e.key === "Escape") { e.preventDefault(); closeMenu(true); }
      else if (e.key === "ArrowDown") { e.preventDefault(); items[(i + 1) % items.length].focus(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); items[(i - 1 + items.length) % items.length].focus(); }
      else if (e.key === "Home") { e.preventDefault(); items[0].focus(); }
      else if (e.key === "End") { e.preventDefault(); items[items.length - 1].focus(); }
      else if (e.key === "Tab") { e.preventDefault(); closeMenu(true); }
    });
    document.body.appendChild(el);
    // position: fixed overlay so the scrolling agenda can't clip it; flip upward near the bottom.
    const r = btn.getBoundingClientRect();
    const mh = el.offsetHeight, mw = el.offsetWidth;
    const below = window.innerHeight - r.bottom - 6;
    el.style.top = (below >= mh || r.top < mh + 6 ? r.bottom + 2 : r.top - mh - 2) + "px";
    el.style.left = Math.max(6, Math.min(r.left, window.innerWidth - mw - 6)) + "px";
    btn.setAttribute("aria-expanded", "true");
    menu = { el, btn };
    (items.find((b) => b.getAttribute("aria-checked") === "true") || items[0]).focus();
  }

  function renderRsvp(ev) {
    const busy = state.pending.has(L.evKey(ev));
    const cur = busy ? S.rsvpSending : rsvpText(L.rsvpState(ev));
    const row = h("div", "rsvp-row");
    row.appendChild(h("span", "rsvp-label", S.rsvpLabel));
    const b = h("button", "rsvp-btn rsvp-" + L.rsvpState(ev));
    b.type = "button";
    b.dataset.key = L.evKey(ev);
    b.disabled = busy;
    b.setAttribute("aria-haspopup", "menu");
    b.setAttribute("aria-expanded", "false");
    b.setAttribute("aria-label", `${S.rsvpLabel}: ${cur}`);
    b.appendChild(h("span", null, cur));
    b.appendChild(svg(ICON_CHEVRON, 10));
    // The card itself is "open in browser": nothing the control does may bubble into it.
    b.addEventListener("click", (e) => {
      e.stopPropagation();
      if (menu && menu.btn === b) closeMenu(false); else openMenu(ev, b);
    });
    b.addEventListener("keydown", (e) => {
      e.stopPropagation(); // keep Enter/Space from the card's open handler and arrows from day navigation
      if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); openMenu(ev, b); }
      else if (e.key === "Escape" && menu) { e.preventDefault(); closeMenu(true); }
    });
    row.appendChild(b);
    row.addEventListener("click", (e) => e.stopPropagation());
    return row;
  }

  async function respond(ev, response) {
    const key = L.evKey(ev);
    if (!L.isResponse(response) || state.pending.has(key) || L.rsvpState(ev) === response) return;
    state.pending.add(key);
    state.respondError = "";
    renderAgenda(); banner();
    try {
      const updated = await invoke("calendar_respond", { profile: PROFILE, calendarId: ev.calendar_id, eventId: ev.id, response });
      if (!updated || typeof updated !== "object" || updated.id !== ev.id) throw new Error("bad response");
      for (const r of state.results.values()) r.events = L.mergeEvent(r.events, updated);
      state.answered.add(key);
      state.needReauth = false;
    } catch (err) {
      const e = L.mapRespondError(err, S);
      if (e.kind === "reauth") state.needReauth = true;
      else {
        state.respondError = e.text;
        clearTimeout(respondErrTimer);
        respondErrTimer = setTimeout(() => { state.respondError = ""; banner(); }, 6000);
      }
    }
    state.pending.delete(key);
    const hadFocus = document.activeElement === document.body || !document.activeElement;
    rebuildIndex();
    renderAll();
    if (hadFocus) { // the re-render dropped focus; give it back to the same control (keyboard users)
      const nb = [...document.querySelectorAll(".rsvp-btn")].find((x) => x.dataset.key === key);
      if (nb) nb.focus();
    }
  }

  function renderItem(ev, dayKey) {
    const item = h("div", "item " + L.rsvpClass(ev));
    item.tabIndex = 0;
    item.setAttribute("role", "button");
    if (ev.calendar_color) item.style.setProperty("--c", L.safeColor(ev.calendar_color));
    const bar = h("div", "bar");
    const body = h("div", "item-body");
    const rs = L.rsvpState(ev);
    const badgeText = rs === "needsAction" ? S.rsvpNeedsAction : rs === "tentative" ? S.rsvpTentative : rs === "declined" ? S.rsvpDeclined : "";
    const head = h("div", "item-head");
    head.appendChild(h("div", "item-title", ev.summary || S.noTitle));
    const canRsvp = L.canRespond(ev, state.answered);
    if (badgeText && !canRsvp) head.appendChild(h("span", "badge", badgeText));
    body.appendChild(head);
    body.appendChild(h("div", "item-time", L.formatRange(ev, dayKey, S)));
    if (ev.location) {
      const loc = h("div", "item-loc");
      loc.appendChild(svg(ICON_PIN, 11));
      loc.appendChild(h("span", null, ev.location.split("\n")[0]));
      loc.title = ev.location;
      body.appendChild(loc);
    }
    if (canRsvp) body.appendChild(renderRsvp(ev));
    item.title = ev.calendar_name ? `${ev.summary || S.noTitle} — ${ev.calendar_name}` : (ev.summary || "");
    item.append(bar, body);
    if (ev.meet_url && L.isSafeLink(ev.meet_url)) {
      const m = h("button", "meet-btn");
      m.type = "button";
      m.title = S.meet;
      m.setAttribute("aria-label", S.meet);
      m.appendChild(svg(ICON_VIDEO, 18));
      m.addEventListener("click", (e) => { e.stopPropagation(); openLink(ev.meet_url); });
      item.appendChild(m);
    }
    const open = () => openLink(ev.html_link);
    item.addEventListener("click", open);
    item.addEventListener("keydown", (e) => {
      if (e.target === item && (e.key === "Enter" || e.key === " ")) { e.preventDefault(); open(); }
    });
    return item;
  }

  function renderLogin() {
    const box = h("div", "state");
    box.appendChild(h("div", "state-title", S.loginTitle));
    if (state.clientMissing) {
      box.classList.add("guide");
      box.appendChild(h("div", "hint", S.needClient));
      const ol = h("ol", "guide-steps");
      S.clientSteps.forEach((parts) => {
        const li = h("li");
        for (const part of parts) {
          if (typeof part === "string") { li.appendChild(document.createTextNode(part)); continue; }
          const a = h("a", "ext-link", part.t);
          a.href = part.u;
          a.addEventListener("click", (e) => { e.preventDefault(); openLink(part.u); });
          li.appendChild(a);
        }
        ol.appendChild(li);
      });
      box.appendChild(ol);
      const ib = h("div", "import-box");
      const fb = h("button", "primary-btn", S.importFile);
      fb.type = "button";
      fb.addEventListener("click", pickClientFile);
      ib.appendChild(fb);
      const ta = h("textarea");
      ta.placeholder = S.importPastePlaceholder;
      ta.spellcheck = false;
      ta.setAttribute("aria-label", S.importPaste);
      const ab = h("button", "mini-btn", S.importApply);
      ab.type = "button";
      ab.addEventListener("click", () => { if (ta.value.trim()) importClient(ta.value); });
      ib.appendChild(ta);
      ib.appendChild(ab);
      box.appendChild(ib);
    } else {
      box.appendChild(h("div", "hint", S.loginHint));
      const b = h("button", "primary-btn", state.loggingIn ? S.loggingIn : S.loginButton);
      b.type = "button";
      b.disabled = state.loggingIn;
      b.addEventListener("click", doLogin);
      box.appendChild(b);
    }
    return box;
  }

  function renderAgenda() {
    closeMenu(false);
    const p = L.parseKey(state.selected);
    $("agenda-title").textContent =
      S.agendaTitle(p.m, p.d, S.weekdaysLong[L.weekday(state.selected)], state.selected === state.todayKey);
    const box = $("agenda");
    box.textContent = "";
    if (state.needLogin) { box.appendChild(renderLogin()); return; }
    const list = state.index.get(state.selected) || [];
    if (list.length) {
      list.forEach((ev) => box.appendChild(renderItem(ev, state.selected)));
      return;
    }
    const loaded = state.results.has(L.monthKey(p.y, p.m));
    const msg = loaded ? S.noEvents : (state.errorText || S.loading);
    box.appendChild(h("div", "empty", msg));
  }

  function renderAll() {
    renderGrid();
    renderAgenda();
    banner();
    $("refresh").classList.toggle("spin", state.refreshing);
  }

  // ------------------------------------------------------------ data
  async function fetchMonth(y, m, force) {
    const mk = L.monthKey(y, m);
    const prof = PROFILE;
    try {
      const r = await invoke("calendar_month", { profile: prof, year: y, month: m, force: !!force });
      if (prof !== PROFILE) return null; // account switched while loading
      state.results.set(mk, r);
      state.needLogin = false;
      state.errorText = "";
      return r;
    } catch (err) {
      if (prof !== PROFILE) return null;
      const e = L.mapError(err && err.message ? err.message : err, S);
      if (e.kind === "login") {
        state.needLogin = true;
        checkClient();
      } else if (!state.results.has(mk)) {
        state.errorText = e.text;
      } else {
        state.results.get(mk).stale = true;
      }
      return null;
    }
  }

  function checkClient() {
    const prof = PROFILE;
    invoke("auth_status", { profile: prof })
      .then((s) => { if (prof !== PROFILE) return; state.clientMissing = !(s && s.client_configured); renderAgenda(); })
      .catch(() => {});
  }

  /** Cache-first load: current month, then neighbours (for dots on dimmed days). Never awaited by first paint. */
  async function loadView(force) {
    const seq = ++state.seq;
    const { year: y, month: m } = state;
    state.refreshing = !!force;
    const first = await fetchMonth(y, m, force);
    if (seq !== state.seq) return;
    rebuildIndex();
    renderAll();
    if (first && first.stale && !force && Date.now() - state.lastForceRetry > RETRY_MIN_MS) {
      state.lastForceRetry = Date.now();
      state.refreshing = true;
      renderAll();
      await fetchMonth(y, m, true);
      if (seq !== state.seq) return;
      state.refreshing = false;
      rebuildIndex();
      renderAll();
    }
    if (first && !state.needLogin) {
      for (const d of [-1, 1]) {
        const n = L.addMonths(y, m, d);
        await fetchMonth(n.y, n.m, false);
        if (seq !== state.seq) return;
      }
    }
    state.refreshing = false;
    rebuildIndex();
    renderAll();
  }

  function pickClientFile() {
    const f = $("client-file");
    f.value = "";
    f.click();
  }

  async function importClient(text) {
    try {
      await invoke("auth_import_client", { profile: PROFILE, json: text });
    } catch (e) {
      toast(/client config|invalid profile/.test(String(e)) ? S.errImportInvalid : S.errImportFailed);
      return;
    }
    state.clientMissing = false;
    state.needLogin = true;
    state.needReauth = false;
    state.results.clear();
    state.loadedOnce.clear();
    rebuildIndex();
    renderAll();
    banner();
    toast(S.importDone);
  }

  async function logout() {
    try { await invoke("auth_logout", { profile: PROFILE }); } catch (_) { toast(S.errOther); return; }
    state.needLogin = true;
    state.results.clear();
    state.loadedOnce.clear();
    rebuildIndex();
    renderAll();
    banner();
    $("settings-note").textContent = S.loggedOut;
  }

  async function doLogin() {
    if (state.loggingIn) return;
    state.loggingIn = true;
    renderAgenda(); banner();
    try {
      await invoke("auth_login", { profile: PROFILE });
      state.needLogin = false;
      state.needReauth = false;
      state.loggingIn = false;
      banner();
      loadView(true);
    } catch (_) {
      state.loggingIn = false;
      renderAgenda(); banner();
      toast(S.errLoginFailed);
    }
  }

  // ------------------------------------------------------------ navigation
  function setView(y, m) {
    if (y === state.year && m === state.month) return;
    state.year = y; state.month = m;
    renderAll();
    loadView(false);
  }
  function selectDay(k) {
    const p = L.parseKey(k);
    state.selected = k;
    if (p.y !== state.year || p.m !== state.month) { state.year = p.y; state.month = p.m; renderAll(); loadView(false); }
    else renderAll();
  }
  function shiftMonth(n) {
    const t = L.addMonths(state.year, state.month, n);
    const day = Math.min(L.parseKey(state.selected).d, L.daysInMonth(t.y, t.m));
    state.selected = L.key(t.y, t.m, day);
    state.year = t.y; state.month = t.m;
    renderAll();
    loadView(false);
  }
  function goToday() {
    const d = new Date();
    state.todayKey = L.localKey(d);
    state.year = d.getFullYear(); state.month = d.getMonth() + 1;
    state.selected = state.todayKey;
    renderAll();
    loadView(false);
  }

  document.addEventListener("keydown", (e) => {
    if (!$("settings").hidden) {
      if (e.key === "Escape") { e.preventDefault(); settingsUI.close(); }
      return; // calendar shortcuts are off while the sheet is open
    }
    if (e.ctrlKey || e.altKey || e.metaKey) return;
    let handled = true;
    // D-04: when the agenda overflows, leave ↑/↓/Home to native scrolling.
    const ag = $("agenda");
    if (["ArrowUp", "ArrowDown", "Home"].includes(e.key) && ag && ag.scrollHeight > ag.clientHeight + 1) return;
    switch (e.key) {
      case "ArrowLeft": selectDay(L.addDays(state.selected, -1)); break;
      case "ArrowRight": selectDay(L.addDays(state.selected, 1)); break;
      case "ArrowUp": selectDay(L.addDays(state.selected, -7)); break;
      case "ArrowDown": selectDay(L.addDays(state.selected, 7)); break;
      case "PageUp": shiftMonth(-1); break;
      case "PageDown": shiftMonth(1); break;
      case "Home": goToday(); break;
      default: handled = false; // other keys are left alone
    }
    if (handled) e.preventDefault();
  });

  // ------------------------------------------------------------ lifecycle
  let clockTimer = 0;
  function startClock() {
    clearInterval(clockTimer);
    renderClock();
    clockTimer = setInterval(renderClock, 1000);
  }
  let hiddenAt = 0, lastShow = 0;
  function onShow(resetToToday) {
    const t = Date.now();
    if (!resetToToday && t - lastShow < 300) return; // an explicit reset is never swallowed (D-05)
    lastShow = t;
    startClock();
    if (resetToToday) { settingsUI.close(); goToday(); } else loadView(false);
  }
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) { hiddenAt = Date.now(); clearInterval(clockTimer); }
    else onShow(true);
  });
  // WebView2 does not always fire visibilitychange on Tauri hide()/show(), so a
  // focus return after the window lost focus (= was hidden) also resets to today.
  window.__clockcalShown = () => onShow(true); // called from Rust on every show
  let blurAt = 0;
  window.addEventListener("blur", () => { blurAt = Date.now(); });
  window.addEventListener("focus", () => {
    if (document.hidden) return;
    const away = blurAt && Date.now() - blurAt > 1000;
    blurAt = 0;
    onShow(!!away);
  });

  // ------------------------------------------------------------ settings (P6)
  const settingsUI = {
    open() {
      $("settings").hidden = false;
      $("settings-note").textContent = "";
      invoke("get_settings").then((v) => {
        $("set-notify").checked = !!v.notify_enabled;
        $("set-lead").value = String(v.notify_lead_min);
        if ($("set-lead").value !== String(v.notify_lead_min)) {
          const o = document.createElement("option");
          o.value = String(v.notify_lead_min); o.textContent = String(v.notify_lead_min);
          $("set-lead").appendChild(o); $("set-lead").value = o.value;
        }
        $("set-autostart").checked = !!v.autostart;
        $("set-clockhook").checked = v.clock_hook !== false;
      }).catch(() => { $("settings-note").textContent = S.errSettings; });
      $("set-notify").focus();
    },
    close() { $("settings").hidden = true; },
    save() {
      const payload = { notify_enabled: $("set-notify").checked, notify_lead_min: Number($("set-lead").value) || 10, autostart: $("set-autostart").checked, clock_hook: $("set-clockhook").checked };
      invoke("set_settings", { new: payload })
        .then(() => { $("settings-note").textContent = S.settingsSaved; })
        .catch(() => { $("settings-note").textContent = S.errSettings; });
    },
  };

  function init() {
    $("settings-btn").setAttribute("aria-label", S.settings);
    $("settings-btn").title = S.settings;
    $("settings-title").textContent = S.settings;
    $("settings-back").setAttribute("aria-label", S.settingsBack);
    $("lbl-notify").textContent = S.setNotify;
    $("lbl-lead").textContent = S.setLead;
    $("lbl-autostart").textContent = S.setAutostart;
    $("lbl-clockhook").textContent = S.setClockHook;
    $("lbl-reimport").textContent = S.setReimport;
    $("set-reimport").textContent = S.importFile;
    $("lbl-logout").textContent = S.setLogout;
    $("set-logout").textContent = S.setLogout;
    $("set-reimport").addEventListener("click", pickClientFile);
    $("set-logout").addEventListener("click", logout);
    $("client-file").addEventListener("change", (e) => {
      const f = e.target.files && e.target.files[0];
      if (!f) return;
      if (f.size > 32 * 1024) { toast(S.errImportInvalid); return; }
      const r = new FileReader();
      r.onload = () => { settingsUI.close(); importClient(String(r.result || "")); };
      r.onerror = () => toast(S.errImportFailed);
      r.readAsText(f);
    });
    $("settings-btn").addEventListener("click", () => settingsUI.open());
    $("settings-back").addEventListener("click", () => settingsUI.close());
    for (const id of ["set-notify", "set-lead", "set-autostart", "set-clockhook"]) $(id).addEventListener("change", () => settingsUI.save());
    $("prev").setAttribute("aria-label", S.prevMonth);
    $("next").setAttribute("aria-label", S.nextMonth);
    $("refresh").setAttribute("aria-label", S.refresh);
    $("today-btn").textContent = S.today;
    $("prev").addEventListener("click", () => shiftMonth(-1));
    $("next").addEventListener("click", () => shiftMonth(1));
    $("today-btn").addEventListener("click", goToday);
    $("refresh").addEventListener("click", () => { if (!state.needLogin) loadView(true); });
    renderDow();
    startClock();
    renderAll();            // first paint: empty grid + clock, no waiting on IPC
    lastShow = Date.now();
    loadView(false);        // then cache/refresh in the background
  }
  init();
})();
