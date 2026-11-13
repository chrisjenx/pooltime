// pooltime dashboard. Vanilla JS, no build step, no inline code (CSP: default-src 'self').
// Reads GET /api/v1/status every 4 s; mutations go to the /api/v1/* management endpoints with
// the HttpOnly session cookie (same-origin fetch; the server checks Host/Origin). Never renders
// credentials: the status document carries none.
"use strict";

(function () {
  // ---- theme (applied before first paint: this script is loaded in <head>) ----------------
  const THEME_KEY = "pooltime-theme";
  function storedTheme() {
    try { return localStorage.getItem(THEME_KEY); } catch (_) { return null; }
  }
  function systemTheme() {
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  function applyTheme(t) { document.documentElement.dataset.theme = t; }
  applyTheme(storedTheme() || systemTheme());

  const POLL_MS = 4000;
  const TIMEOUT_MS = 10000;
  const SIGNIN_POLL_MS = 1500;
  // Login-expiry warn window: the server's (`expiry_warn_secs`, from alerts.warn_days[0]).
  function expiryWarnMs() {
    const s = ui.data && ui.data.expiry_warn_secs;
    return (typeof s === "number" ? s : 3 * 24 * 3600) * 1000;
  }

  // Exact status words (spec §4/§16): the same everywhere, most severe first.
  const WORDS = {
    needs_login: "Needs login",
    billing: "Billing",
    access_denied: "Access denied",
    erroring: "Erroring",
    rate_limited: "Rate-limited",
    expiring: "Expiring",
    ready: "Ready",
  };
  const SEVERITY = { needs_login: 0, billing: 1, access_denied: 2, erroring: 3, rate_limited: 4, expiring: 5, ready: 6 };
  const REASONS = {
    never_logged_in: "never logged in",
    refresh_token_dead: "refresh token rejected by the provider",
    refresh_token_expired: "login expired",
    refresh_token_expiring: "login expiring",
    state_unreadable: "saved state unreadable; recheck it",
    credentials_unreadable: "credentials file unreadable",
    credentials_unavailable: "credentials unavailable",
    billing: "provider reports a billing / entitlement problem",
    access_denied: "provider denied access",
    quota: "quota spent",
  };

  const ui = {
    data: null,
    skew: 0, // server clock minus local clock, so countdowns follow the server's "now"
    lastOk: 0,
    error: null,
    drafts: {}, // unsaved form values, keyed per field, survive re-renders
    msgs: {}, // per-form result messages
    view: "overview",
    // Debugging views (Requests / Events): fetched only while one of them is shown, key holders only.
    journal: { requests: null, stats: null, events: null, errorsOnly: false, open: {}, details: {}, error: null },
    // Weekly burn (Overview, key holders only): refetched at most every BURN_MS.
    burn: null, burnAt: 0, burnError: null,
    // Account order (Overview, Accounts, Routing), remembered in localStorage; filters are not.
    sort: { key: "reset7", reversed: false },
    filter: { q: "", state: "all", provider: "all" },
  };
  const SORT_STORE = "pooltime-sort";
  try {
    const saved = JSON.parse(localStorage.getItem(SORT_STORE) || "null");
    if (saved && typeof saved.key === "string") ui.sort = migrateSort(saved);
  } catch (_) { /* private mode or junk: keep the default */ }
  // The sort menu used to order by headroom left ("left7", "left5": most left first). Those saved
  // values keep their order under the used keys: least used first is "used" ascending, which is the
  // reverse of the menu's natural direction.
  const OLD_SORT_KEYS = { left7: "used7", left5: "used5" };
  function migrateSort(saved) {
    const key = OLD_SORT_KEYS[saved.key];
    return key ? { key, reversed: !saved.reversed } : { key: saved.key, reversed: !!saved.reversed };
  }
  function setSort(next) {
    ui.sort = next;
    try { localStorage.setItem(SORT_STORE, JSON.stringify(next)); } catch (_) { /* private mode */ }
  }

  let manage = false;

  // ---- tiny DOM helper (textContent only; never innerHTML) ----------------------------------
  function h(tag, attrs) {
    const el = document.createElement(tag);
    if (attrs) {
      for (const k of Object.keys(attrs)) {
        const v = attrs[k];
        if (v === null || v === undefined || v === false) continue;
        if (k === "class") el.className = v;
        else if (k === "text") el.textContent = v;
        else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
        else if (v === true) el.setAttribute(k, "");
        else el.setAttribute(k, String(v));
      }
    }
    for (let i = 2; i < arguments.length; i++) append(el, arguments[i]);
    return el;
  }
  function append(el, c) {
    if (c === null || c === undefined || c === false) return;
    if (Array.isArray(c)) { c.forEach((x) => append(el, x)); return; }
    el.appendChild(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  function $(id) { return document.getElementById(id); }

  // ---- time & number formatting -------------------------------------------------------------
  function now() { return Date.now() + ui.skew; }
  function num(n) { return typeof n === "number" ? n.toLocaleString() : "—"; }
  function compact(n) {
    if (typeof n !== "number") return "—";
    if (n < 1000) return String(n);
    if (n < 1e6) return (n / 1000).toFixed(n < 10000 ? 1 : 0) + "k";
    return (n / 1e6).toFixed(n < 1e7 ? 1 : 0) + "M";
  }
  function dur(ms) {
    let s = Math.round(Math.abs(ms) / 1000);
    const d = Math.floor(s / 86400); s -= d * 86400;
    const hr = Math.floor(s / 3600); s -= hr * 3600;
    const m = Math.floor(s / 60);
    if (d > 0) return hr ? d + "d " + hr + "h" : d + "d";
    if (hr > 0) return m ? hr + "h " + m + "m" : hr + "h";
    if (m > 0) return m + "m";
    return "<1m";
  }
  function rel(ts) {
    if (!ts) return "—";
    const diff = Date.parse(ts) - now();
    if (Math.abs(diff) < 60000) return "just now";
    return diff > 0 ? "in " + dur(diff) : dur(diff) + " ago";
  }
  function clock(ts) {
    if (!ts) return "—";
    const d = new Date(ts);
    const sameDay = Math.abs(d.getTime() - now()) < 20 * 3600 * 1000;
    const opts = { hour: "2-digit", minute: "2-digit", hourCycle: "h23" };
    if (!sameDay) { opts.month = "short"; opts.day = "numeric"; }
    return d.toLocaleString(undefined, opts);
  }
  function pct(x) { return Math.round(x * 100) + "%"; }
  function bytes(n) {
    if (typeof n !== "number") return "—";
    if (n < 1024) return n + " B";
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KiB";
    if (n < 1024 * 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + " MiB";
    return (n / 1024 / 1024 / 1024).toFixed(2) + " GiB";
  }

  // ---- account helpers ----------------------------------------------------------------------
  function isDead(a) { return a.status === "needs_login"; }
  // A dead account never shows leftover usage as headroom (spec §4): its windows are ignored
  // even if a stale snapshot is present.
  function windowsOf(a) { return isDead(a) ? [] : (a.windows || []); }
  // ---- usage as "used" (pure): shares are 0-1, null = unknown / not shown ----------------------
  function clamp01(x) { return Math.max(0, Math.min(1, x)); }
  // Share used of the account-wide 7-day window. The API's `seven_day_left` is its complement and
  // stays the fallback for a daemon that predates `seven_day_used`.
  function sevenDayUsed(a) {
    if (isDead(a)) return null;
    if (typeof a.seven_day_used === "number") return clamp01(a.seven_day_used);
    return typeof a.seven_day_left === "number" ? clamp01(1 - a.seven_day_left) : null;
  }
  // Pool total in subscriptions (one account = 1.0): the sum of the accounts' 7-day usage. An
  // account with no known usage (logged out, no window yet) has no headroom, so it counts as fully
  // used, the mirror of the old "left" total (and in step with the weekly burn).
  function poolUsed(accts) {
    return accts.reduce((t, a) => { const u = sevenDayUsed(a); return t + (u === null ? 1 : u); }, 0);
  }
  // Bar tone for a used share: accent while there is room, warning from 60%, red from 85%.
  function usedTone(used) { return used > 0.85 ? "bad" : used > 0.6 ? "warn" : ""; }
  function reasonText(r) {
    if (!r) return null;
    if (r.startsWith("quota:")) return windowLabel(r.slice(6)) + " quota spent";
    return REASONS[r] || r.replace(/_/g, " ");
  }
  function windowLabel(key, model) {
    const m = /^(\d+)([hdm])/.exec(String(key));
    let base = key;
    if (m) base = m[1] + (m[2] === "h" ? "-hour" : m[2] === "d" ? "-day" : "-min");
    return model ? base + " · " + model : base;
  }
  // Most severe first (the attention list and the Re-login queue).
  function sorted(accounts) {
    return accounts.slice().sort((a, b) =>
      (SEVERITY[a.status] ?? 9) - (SEVERITY[b.status] ?? 9) || String(a.id).localeCompare(String(b.id)));
  }

  // ---- account order (pure: no DOM, no clock; `t` is "now" in ms) ---------------------------
  // key -> [label, natural direction: 1 = ascending]. The first is the default.
  const SORTS = {
    reset7: ["7-day reset (soonest)", 1],
    used7: ["7-day used (most)", -1],
    used5: ["5-hour used (most)", -1],
    severity: ["State (most severe)", 1],
    name: ["Name", 1],
    activity: ["Activity (busiest)", -1],
  };
  function windowMs(key) {
    const m = /^(\d+)([hdm])/.exec(String(key));
    if (!m) return null;
    return Number(m[1]) * (m[2] === "d" ? 86400000 : m[2] === "h" ? 3600000 : 60000);
  }
  function accountWindow(a, token) {
    return windowsOf(a).find((w) => !w.model && String(w.key).split(/[-_]/).includes(token)) || null;
  }
  // The window's next reset in ms. A reported reset already passed rolls forward by the window
  // length (`assumed`: the next usage check will confirm it); unknown -> null.
  function nextReset(w, t) {
    if (!w || !w.reset) return null;
    const r = Date.parse(w.reset);
    if (isNaN(r)) return null;
    if (r > t) return { at: r, assumed: false };
    const d = windowMs(w.key);
    return d ? { at: r + (Math.floor((t - r) / d) + 1) * d, assumed: true } : null;
  }
  function fiveHourUsed(a) {
    const w = isDead(a) ? null : accountWindow(a, "5h");
    return w && typeof w.utilization === "number" ? clamp01(w.utilization) : null;
  }
  function sortValue(a, key, t) {
    switch (key) {
      case "reset7": { const r = nextReset(accountWindow(a, "7d"), t); return r ? r.at : null; }
      case "used7": return sevenDayUsed(a);
      case "used5": return fiveHourUsed(a);
      case "severity": return SEVERITY[a.status] ?? 9;
      case "name": return String(a.id);
      case "activity": return (a.in_flight || 0) + (a.sessions || 0);
      default: return null;
    }
  }
  // Comparator for `sort` = { key, reversed }. Accounts without a value (no known 7-day reset,
  // needs login, ...) always go after those with one, then by name; ties break on the soonest
  // 7-day reset, then the name.
  function compareAccounts(sort, t) {
    const key = SORTS[sort.key] ? sort.key : "reset7";
    const dir = SORTS[key][1] * (sort.reversed ? -1 : 1);
    const byName = (a, b) => String(a.id).localeCompare(String(b.id));
    const nullsLast = (va, vb) => (va == null ? (vb == null ? 0 : 1) : -1);
    return (a, b) => {
      const va = sortValue(a, key, t), vb = sortValue(b, key, t);
      if (va == null || vb == null) return nullsLast(va, vb) || byName(a, b);
      const d = (typeof va === "string" ? va.localeCompare(vb) : va - vb) * dir;
      if (d || key === "name") return d || byName(a, b);
      if (key !== "reset7") {
        const ra = sortValue(a, "reset7", t), rb = sortValue(b, "reset7", t);
        if (ra == null || rb == null) { if (ra !== rb) return nullsLast(ra, rb); } else if (ra !== rb) return ra - rb;
      }
      return byName(a, b);
    };
  }
  function sortAccounts(accounts, sort, t) { return accounts.slice().sort(compareAccounts(sort, t)); }
  // ---- end account order ----

  // Not usable for new requests right now (bro's "unavailable").
  function isUnavailable(a) { return ["needs_login", "billing", "access_denied", "rate_limited"].includes(a.status); }
  // The last ground-truth usage poll failed (a dead account's failure is its state already).
  function usageWarning(a) { return !isDead(a) && !!a.usage_check && a.usage_check.ok === false; }
  function matchesFilter(a, f) {
    const q = f.q.trim().toLowerCase();
    if (q && !String(a.id).toLowerCase().includes(q) &&
        !String((a.identity && a.identity.email) || "").toLowerCase().includes(q)) return false;
    if (f.provider !== "all" && a.provider !== f.provider) return false;
    if (f.state === "all") return true;
    if (f.state === "unavailable") return isUnavailable(a);
    if (f.state === "usage_warning") return usageWarning(a);
    return a.status === f.state;
  }
  // The server decides `expiring` (warn window from config); an account showing a more severe
  // state (e.g. rate-limited) whose login also expires soon still counts for re-login.
  function loginExpiringSoon(a) {
    if (a.status === "expiring") return true;
    if (isDead(a) || !a.refresh_expires_at) return false;
    return Date.parse(a.refresh_expires_at) - now() <= expiryWarnMs();
  }
  // "expires in 2d 4h" / "expired 3h ago, still working" (only a refused refresh ends a login).
  function expiryCountdown(ts) {
    const left = Date.parse(ts) - now();
    return left > 0 ? "login expires " + rel(ts) : "login expired " + rel(ts) + ", still working";
  }
  function pill(status) {
    return h("span", { class: "pill " + status, text: WORDS[status] || status });
  }

  // ---- journal helpers ----------------------------------------------------------------------
  const OUTCOME_CLASS = { completed: "ready", client_cancelled: "badge", failover: "rate_limited" };
  function outcomePill(o) {
    return h("span", { class: "pill " + (OUTCOME_CLASS[o] || "erroring"), text: String(o || "—").replace(/_/g, " ") });
  }
  function ms(n) {
    if (typeof n !== "number") return "—";
    return n < 1000 ? n + " ms" : (n / 1000).toFixed(n < 10000 ? 2 : 1) + " s";
  }
  function lockedPanel(title) {
    return h("div", { class: "panel" },
      h("h2", { text: title }),
      h("p", { class: "empty", text: "Only shown with the management key: run `pooltime open`, or unlock above." }));
  }
  // The attempt chain of one request (cross-provider hops show as a provider change).
  function chainView(d) {
    if (!d) return h("p", { class: "empty", text: "Loading…" });
    if (d.error) return h("p", { class: "msg err", text: d.error });
    const r = d.request || {};
    const attempts = d.attempts || [];
    const tokens = typeof r.input_tokens === "number"
      ? r.input_tokens + " in / " + r.output_tokens + " out" + (r.cache_read ? " · " + r.cache_read + " cache read" : "")
      : "—";
    return h("div", { class: "chain" },
      h("p", { class: "dim" },
        h("span", { class: "mono", text: r.id }), " · ", r.provider || "—", " · tokens " + tokens,
        r.session_hash ? h("span", null, " · session ", h("span", { class: "mono", text: r.session_hash })) : null),
      attempts.length
        ? h("ol", null, attempts.map((a, i) => h("li", null,
            i > 0 && a.provider !== attempts[i - 1].provider ? h("span", { class: "pill badge warn", text: "cross-provider" }) : null,
            " ", h("strong", { text: a.account }), " ", h("span", { class: "dim", text: a.provider }), " ",
            outcomePill(a.outcome),
            a.reason ? " " + a.reason : "",
            a.upstream_status != null ? " · upstream " + a.upstream_status : "",
            a.upstream_request_id ? h("span", null, " · ", h("span", { class: "mono", text: a.upstream_request_id })) : null,
            " · " + ms(a.ms))))
        : h("p", { class: "empty", text: "No upstream attempt." }));
  }
  // A usage bar: filled from 0 to 100% as the used share grows (like Anthropic's usage page).
  function bar(used) {
    if (typeof used !== "number") return h("div", { class: "bar none", title: "unknown" });
    const f = clamp01(used);
    const tone = usedTone(f);
    const cls = tone ? "bar " + tone : "bar";
    const fill = h("span");
    fill.style.width = (f * 100).toFixed(1) + "%";
    return h("div", { class: cls }, fill);
  }

  // ---- weekly burn chart (plain SVG via createElementNS; no inline style, CSP-safe) ----------
  const SVG_NS = "http://www.w3.org/2000/svg";
  function s(tag, attrs) {
    const el = document.createElementNS(SVG_NS, tag);
    if (attrs) {
      for (const k of Object.keys(attrs)) {
        const v = attrs[k];
        if (v === null || v === undefined || v === false) continue;
        if (k === "text") el.textContent = v;
        else el.setAttribute(k, String(v));
      }
    }
    for (let i = 2; i < arguments.length; i++) {
      const c = arguments[i];
      if (c) [].concat(c).forEach((x) => x && el.appendChild(x));
    }
    return el;
  }
  function subs(x) { return (typeof x === "number" ? x : 0).toFixed(1); }

  // One bar per ISO week (subs used); the current week adds a dashed "projected" outline; a
  // dashed rule marks the pool size (N subs). Native <title> tooltips per bar.
  function burnChart(b) {
    const weeks = (b.weeks || []).slice(-12);
    const f = b.forecast || {};
    const n = f.accounts || 0;
    const W = 640, H = 200, padL = 30, padR = 8, padT = 14, padB = 22;
    const top = Math.max(1, n, f.projected || 0, ...weeks.map((w) => w.subs_used || 0));
    const max = Math.ceil(top);
    const y = (v) => padT + (H - padT - padB) * (1 - v / max);
    const slot = (W - padL - padR) / Math.max(1, weeks.length);
    const bw = Math.max(6, Math.min(36, slot * 0.6));
    const ticks = [];
    const step = max <= 4 ? 1 : Math.ceil(max / 4);
    for (let v = 0; v <= max; v += step) {
      ticks.push(s("line", { class: "grid", x1: padL, x2: W - padR, y1: y(v), y2: y(v) }));
      ticks.push(s("text", { class: "axis", x: padL - 6, y: y(v) + 4, "text-anchor": "end", text: String(v) }));
    }
    const marks = weeks.map((w, i) => {
      const x = padL + slot * i + (slot - bw) / 2;
      const cur = i === weeks.length - 1 && w.week === f.week;
      const v = w.subs_used || 0;
      const parts = (w.per_account || []).map((a) => a.account + " " + subs(a.consumed)).join(", ");
      const g = s("g", null, s("title", { text: w.week + " (from " + w.start + "): " + subs(v) + " subs" + (parts ? " · " + parts : "") }));
      if (cur && typeof f.projected === "number" && f.projected > v) {
        g.appendChild(s("rect", { class: "proj", x, y: y(f.projected), width: bw, height: Math.max(0, y(v) - y(f.projected)), rx: 3 }));
      }
      g.appendChild(s("rect", { class: cur ? "used cur" : "used", x, y: y(v), width: bw, height: Math.max(0, y(0) - y(v)), rx: 3 }));
      // Hit target the full slot height, so thin bars are easy to hover.
      g.appendChild(s("rect", { class: "hit", x: padL + slot * i, y: padT, width: slot, height: H - padT - padB }));
      g.appendChild(s("text", { class: "axis", x: x + bw / 2, y: H - 6, "text-anchor": "middle", text: (w.week || "").slice(5) }));
      if (cur || i % 4 === 0) g.appendChild(s("text", { class: "val", x: x + bw / 2, y: y(v) - 4, "text-anchor": "middle", text: subs(v) }));
      return g;
    });
    const cap = n ? [
      s("line", { class: "cap", x1: padL, x2: W - padR, y1: y(n), y2: y(n) }),
      s("text", { class: "axis", x: W - padR, y: y(n) - 4, "text-anchor": "end", text: n + " subs" }),
    ] : null;
    return s("svg", { class: "burn", viewBox: "0 0 " + W + " " + H, role: "img", "aria-label": "Subscriptions used per week" },
      ticks, cap, marks);
  }

  function burnPanel() {
    const b = ui.burn;
    const title = h("h2", { text: "Weekly burn" });
    if (!b) return h("div", { class: "panel" }, title, h("p", { class: "empty", text: ui.burnError ? "Cannot load the weekly burn: " + ui.burnError : "Loading…" }));
    const f = b.forecast || {};
    const idle = b.idle_accounts || [];
    const pace = typeof f.projected === "number"
      ? "On pace for ~" + subs(f.projected) + " of " + f.accounts + " subs this week"
      : "This week just started: no forecast yet";
    return h("div", { class: "panel" }, title,
      ui.burnError ? h("p", { class: "msg err", text: "Stale: the last refresh failed (" + ui.burnError + ")" }) : null,
      h("p", { class: "sub" }, h("strong", { text: pace }), " · " + subs(f.subs_used_so_far) + " used so far (" + pct(f.elapsed_fraction || 0) + " of the week)"),
      burnChart(b),
      idle.length
        ? h("p", { class: "dim" }, idle.length + (idle.length === 1 ? " sub" : " subs") + " mostly idle this week: ",
            idle.map((id, i) => h("span", null, i ? ", " : "", h("strong", { text: id }))))
        : h("p", { class: "dim", text: "No idle subs this week." }));
  }

  // ---- API ----------------------------------------------------------------------------------
  async function api(method, path, body) {
    const ctl = new AbortController();
    const timer = setTimeout(() => ctl.abort(), TIMEOUT_MS);
    try {
      const resp = await fetch(path, {
        method,
        credentials: "same-origin",
        cache: "no-store",
        headers: body === undefined ? {} : { "content-type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
        signal: ctl.signal,
      });
      let json = null;
      try { json = await resp.json(); } catch (_) { /* empty body */ }
      if (!resp.ok) {
        const msg = (json && json.error && json.error.message) || "HTTP " + resp.status;
        if (resp.status === 401 && method !== "GET") lostSession();
        const err = new Error(msg);
        err.status = resp.status;
        throw err;
      }
      return json;
    } catch (e) {
      if (e.name === "AbortError") throw new Error("timed out");
      throw e;
    } finally {
      clearTimeout(timer);
    }
  }

  function lostSession() {
    manage = false;
    document.body.dataset.mode = "read-only";
    render();
  }

  // Requests / Events data (management key required; the read-only view shows a hint instead).
  async function loadJournal() {
    if (!manage || (ui.view !== "requests" && ui.view !== "events")) return;
    const j = ui.journal;
    try {
      if (ui.view === "requests") {
        const [reqs, stats] = await Promise.all([
          api("GET", "/api/v1/requests?limit=100" + (j.errorsOnly ? "&errors_only=true" : "")),
          api("GET", "/api/v1/stats"),
        ]);
        j.requests = reqs.requests || [];
        j.stats = stats;
      } else {
        const ev = await api("GET", "/api/v1/timeline?limit=200");
        j.events = ev.events || [];
      }
      j.error = null;
    } catch (e) {
      j.error = e.message;
    }
  }

  const BURN_MS = 60000;
  async function loadBurn() {
    if (!manage || ui.view !== "overview" || Date.now() - ui.burnAt < BURN_MS) return;
    try {
      ui.burn = await api("GET", "/api/v1/analytics/burn?weeks=12");
      ui.burnError = null;
    } catch (e) {
      ui.burnError = e.message;
    }
    ui.burnAt = Date.now();
  }

  async function toggleRequest(id) {
    const j = ui.journal;
    if (j.open[id]) { delete j.open[id]; render(); return; }
    j.open[id] = true;
    render();
    try {
      j.details[id] = await api("GET", "/api/v1/requests/" + encodeURIComponent(id));
    } catch (e) {
      j.details[id] = { error: e.message };
    }
    render();
  }

  async function setErrorsOnly(on) {
    ui.journal.errorsOnly = on;
    await loadJournal();
    render();
  }

  async function poll() {
    try {
      const data = await api("GET", "/api/v1/status");
      ui.data = data;
      ui.error = null;
      ui.lastOk = Date.now();
      if (data && data.now) ui.skew = Date.parse(data.now) - Date.now();
      await loadJournal();
      await loadBurn();
      render();
    } catch (e) {
      ui.error = e.message;
      renderConn();
    } finally {
      setTimeout(poll, POLL_MS);
    }
  }

  // ---- rendering ----------------------------------------------------------------------------
  function editing() {
    const el = document.activeElement;
    return !!el && $("main").contains(el) && /^(INPUT|SELECT|TEXTAREA)$/.test(el.tagName);
  }

  function render() {
    renderConn();
    renderNav();
    if (!ui.data) {
      const v = $("view-" + ui.view);
      v.replaceChildren(h("div", { class: "panel empty", text: ui.error ? "Cannot reach pooltime: " + ui.error : "Loading…" }));
      return;
    }
    renderBanner();
    // Leave the view alone while the user is typing in it; drafts survive the next render.
    if (editing()) return;
    paintView();
  }

  function paintView() {
    const fn = VIEWS[ui.view] || VIEWS.overview;
    $("view-" + ui.view).replaceChildren(...[].concat(fn(ui.data)));
  }

  // Sort and filter bar over an account list (Overview table, Accounts cards). Changes repaint
  // at once; the search box keeps focus and caret across the repaint.
  function accountToolbar(all, shown) {
    const v = ui.view;
    const f = ui.filter;
    const pick = (fn) => (ev) => { fn(ev.target.value); ev.target.blur(); paintView(); };
    const providers = [...new Set(all.map((a) => a.provider))].sort();
    const present = Object.keys(WORDS).filter((s) => all.some((a) => a.status === s));
    const warned = all.some(usageWarning);
    const searchId = "acct-search-" + v;
    return h("div", { class: "toolbar" },
      h("label", { class: "inline" }, h("span", { class: "dim", text: "Sort" }),
        h("select", { "aria-label": "Sort accounts", onchange: pick((k) => setSort({ key: k, reversed: false })) },
          Object.keys(SORTS).map((k) => h("option", { value: k, selected: ui.sort.key === k, text: SORTS[k][0] })))),
      h("button", { type: "button", class: "small ghost", title: "Reverse the order", "aria-pressed": ui.sort.reversed ? "true" : "false",
        onclick: () => { setSort({ key: ui.sort.key, reversed: !ui.sort.reversed }); paintView(); } },
        ui.sort.reversed ? "Reversed" : "Reverse"),
      h("input", { id: searchId, type: "search", placeholder: "Search name or email", value: f.q, "aria-label": "Search accounts",
        oninput: (ev) => {
          const pos = ev.target.selectionStart;
          f.q = ev.target.value;
          paintView();
          const el = $(searchId);
          if (el) { el.focus(); try { el.setSelectionRange(pos, pos); } catch (_) { /* type=search */ } }
        } }),
      h("select", { "aria-label": "Account state", onchange: pick((x) => { f.state = x; }) },
        h("option", { value: "all", text: "All states" }),
        h("option", { value: "unavailable", selected: f.state === "unavailable", text: "Unavailable" }),
        warned || f.state === "usage_warning" ? h("option", { value: "usage_warning", selected: f.state === "usage_warning", text: "Usage check failing" }) : null,
        present.concat(present.includes(f.state) || !WORDS[f.state] ? [] : [f.state]).map((s) =>
          h("option", { value: s, selected: f.state === s, text: WORDS[s] }))),
      providers.length > 1 || f.provider !== "all"
        ? h("select", { "aria-label": "Provider", onchange: pick((x) => { f.provider = x; }) },
            h("option", { value: "all", text: "All providers" }),
            providers.map((p) => h("option", { value: p, selected: f.provider === p, text: p })))
        : null,
      h("span", { class: "dim grow-right", text: shown === all.length ? all.length + (all.length === 1 ? " account" : " accounts") : shown + " of " + all.length + " accounts" }),
      shown !== all.length
        ? h("button", { type: "button", class: "small ghost", onclick: () => { ui.filter = { q: "", state: "all", provider: "all" }; paintView(); } }, "Clear filters")
        : null);
  }

  // The accounts to list, filtered and in the chosen order.
  function listed(all) {
    return sortAccounts(all.filter((a) => matchesFilter(a, ui.filter)), ui.sort, now());
  }

  function renderConn() {
    const c = $("conn");
    if (ui.error) {
      c.className = "conn down";
      c.textContent = "offline: " + ui.error;
    } else if (ui.data) {
      c.className = "conn";
      c.textContent = (manage ? "managing" : "read-only") + " · v" + (ui.data.version || "?");
    }
  }

  function renderNav() {
    document.querySelectorAll(".tabs a").forEach((a) => a.classList.toggle("active", a.dataset.view === ui.view));
    document.querySelectorAll(".view").forEach((s) => s.classList.toggle("active", s.dataset.view === ui.view));
  }

  // "N accounts need login · M expire today [Re-login all]" from the status `counts`.
  function renderBanner() {
    const b = $("banner");
    const accts = ui.data.accounts || [];
    const dead = accts.filter(isDead);
    const expiring = accts.filter((a) => !isDead(a) && loginExpiringSoon(a));
    const blocked = accts.filter((a) => a.status === "billing" || a.status === "access_denied");
    const erroring = accts.filter((a) => a.status === "erroring");
    const c = ui.data.counts || { needs_login: dead.length, expiring_today: 0, expiring: expiring.length };
    const later = c.expiring - c.expiring_today;
    const parts = [];
    if (c.needs_login) parts.push(c.needs_login + (c.needs_login === 1 ? " account needs" : " accounts need") + " login");
    if (c.expiring_today) parts.push(c.expiring_today + (c.expiring_today === 1 ? " expires" : " expire") + " today");
    if (later > 0) parts.push(later + (later === 1 ? " login expires" : " logins expire") + " soon");
    if (blocked.length) parts.push(blocked.length + " blocked (billing / access)");
    if (erroring.length) parts.push(erroring.length + " erroring");
    if (!parts.length) { b.hidden = true; return; }
    b.hidden = false;
    b.className = dead.length || blocked.length || erroring.length ? "banner" : "banner warn";
    // Re-login all: needs-login first, then the soonest expiry; one sign-in at a time (the
    // dialog starts the next only after the current one has settled).
    const soonest = (a, z) => Date.parse(a.refresh_expires_at) - Date.parse(z.refresh_expires_at);
    const queue = sorted(dead).concat(expiring.slice().sort(soonest)).map((a) => a.id);
    b.replaceChildren(
      h("span", { class: "grow", text: parts.join(" · ") }),
      ...(queue.length && manage
        ? [h("button", { type: "button", class: "small", onclick: () => relogin.start(queue) }, "Re-login all")]
        : []),
      h("a", { href: "#accounts" }, "View accounts"),
    );
  }

  // ---- views --------------------------------------------------------------------------------
  const VIEWS = {
    overview(d) {
      const accts = d.accounts || [];
      const pool = d.pool || {};
      const n = pool.accounts || accts.length;
      const counts = {};
      accts.forEach((a) => { counts[a.status] = (counts[a.status] || 0) + 1; });
      const total = poolUsed(accts);
      const hero = h("div", { class: "panel" },
        h("h2", { text: "7-day picture" }),
        h("div", { class: "hero" },
          h("div", null,
            h("div", { class: "big" }, Math.max(0, total).toFixed(2), " ", h("small", { text: "of " + n + " subscriptions used this week" })),
            h("div", { class: "sub", text: heroSub(accts, pool, n) })),
          bar(n ? total / n : null)),
        h("div", { class: "counts" },
          Object.keys(WORDS).filter((s) => counts[s]).map((s) =>
            h("span", { class: "pill " + s, text: counts[s] + " " + WORDS[s] }))));

      const rows = listed(accts).map(accountRow);
      const table = h("div", { class: "panel" },
        h("h2", { text: "Accounts" }),
        accts.length ? accountToolbar(accts, rows.length) : null,
        rows.length
          ? h("div", { class: "table-wrap" }, h("table", { class: "accounts" }, h("thead", null, h("tr", null,
              ["Account", "State", "5-hour used", "7-day used", "7-day reset", "Activity"].map((t) => h("th", { text: t })))),
            h("tbody", null, rows)))
          : h("p", { class: "empty", text: accts.length ? "No accounts match the filters." : "No accounts yet. Add one with `pooltime accounts add`." }));

      // Anything that needs a look, most severe first (needs login on top), whatever the sort.
      const attention = sorted(accts.filter((a) => a.status !== "ready" || loginExpiringSoon(a) || usageWarning(a))).map((a) => h("tr", null,
        h("td", { class: "nowrap" }, h("strong", { text: a.id })),
        h("td", null, pill(a.status)),
        h("td", { class: "dim nowrap", text: a.status_since ? "for " + dur(now() - Date.parse(a.status_since)) : "—" }),
        h("td", { text: [statusDetail(a), usageWarning(a) ? usageCheckText(a) : ""].filter(Boolean).join(" · ") }),
        h("td", { class: "nowrap" }, (isDead(a) || loginExpiringSoon(a)) && manage
          ? h("button", { type: "button", class: "small", onclick: () => relogin.start([a.id]) }, "Re-login")
          : null)));
      const list = h("div", { class: "panel" },
        h("h2", { text: "Needs attention" }),
        attention.length
          ? h("table", null, h("tbody", null, attention))
          : h("p", { class: "empty", text: accts.length ? "Nothing: every account is ready." : "—" }));
      return [hero, list, table, manage ? burnPanel() : lockedPanel("Weekly burn")];
    },

    accounts(d) {
      const all = d.accounts || [];
      if (!all.length) return h("div", { class: "panel empty", text: "No accounts yet. Add one with `pooltime accounts add`." });
      const accts = listed(all);
      return [
        h("div", { class: "panel toolbar-panel" }, accountToolbar(all, accts.length)),
        accts.length ? h("div", { class: "cards" }, accts.map(accountCard)) : h("div", { class: "panel empty", text: "No accounts match the filters." }),
      ];
    },

    routing(d) {
      const accts = sortAccounts(d.accounts || [], ui.sort, now());
      const ro = !manage;
      const rows = accts.map((a) => {
        const key = "routing:" + a.id;
        const dr = ui.drafts[key] || {};
        const set = (field) => (ev) => { (ui.drafts[key] = ui.drafts[key] || {})[field] = ev.target.value; };
        const ov = a.weight_override;
        return h("tr", null,
          h("td", { class: "nowrap" }, h("strong", { text: a.id }), " ", pill(a.status)),
          h("td", null, h("input", { type: "number", min: 0, step: 1, value: dr.priority ?? a.priority, disabled: ro, "aria-label": "priority", oninput: set("priority") })),
          h("td", null, h("input", { type: "number", min: 0, step: 0.1, value: dr.weight ?? a.weight, disabled: ro, "aria-label": "weight", oninput: set("weight") })),
          h("td", null, h("input", { type: "datetime-local", class: "until-input", value: dr.until ?? "", disabled: ro, "aria-label": "until", oninput: set("until") })),
          h("td", { class: "nowrap" }, ov
            ? h("span", null, "weight " + ov.weight + " until " + clock(ov.until) + " ",
              manage ? h("button", { type: "button", class: "small ghost", onclick: () => saveRouting(a, { until: null }) }, "Cancel") : null)
            : h("span", { class: "dim", text: "—" })),
          h("td", { class: "nowrap" },
            manage ? h("button", { type: "button", class: "small", onclick: () => saveRouting(a) }, "Save") : null,
            " ", msgEl(key)));
      });
      const table = h("div", { class: "panel" },
        h("h2", { text: "Priority & weight" }),
        h("p", { class: "section-note", text: "Lower priority number is preferred. With “until”, the weight is a temporary override that reverts by itself at that time." }),
        h("table", null,
          h("thead", null, h("tr", null, ["Account", "Priority", "Weight", "Until (optional)", "Active override", ""].map((t) => h("th", { text: t })))),
          h("tbody", null, rows)));

      const t = d.tuning || {};
      const tk = "tuning";
      const td = ui.drafts[tk] || {};
      const field = (name, label, help) => [
        h("label", { for: "tune-" + name, text: label }),
        h("input", { id: "tune-" + name, type: "number", step: 0.05, min: 0, value: td[name] ?? t[name], disabled: ro,
          oninput: (ev) => { (ui.drafts[tk] = ui.drafts[tk] || {})[name] = ev.target.value; } }),
        h("span", { class: "help", text: help }),
      ];
      const tuning = h("div", { class: "panel" },
        h("h2", { text: "Tuning" }),
        h("div", { class: "form-grid" },
          field("five_hour_exp", "5-hour exponent", "How strongly 5-hour usage lowers an account's score."),
          field("min_headroom", "Minimum headroom", "Accounts with less 7-day headroom than this are skipped while others have room (0.1 = skipped above 90% used)."),
          field("headroom_taper_start", "Headroom taper start", "Below this headroom an account's share tapers off (0.2 = from 80% 5-hour used).")),
        h("div", { class: "inline card-actions" },
          manage ? h("button", { type: "button", onclick: saveTuning }, "Save tuning") : null, msgEl(tk)));
      return [table, tuning];
    },

    mappings(d) {
      const m = d.mapping || { enabled: false, mappings: [] };
      const ro = !manage;
      const rows = (m.mappings || []).map((row) => {
        const key = "map:" + row.from;
        const effort = row.effort ? Object.keys(row.effort).sort().map((k) => k + " → " + row.effort[k]).join(", ") : "";
        return h("tr", null,
          h("td", { class: "mono", text: row.from }),
          h("td", null, h("input", { value: ui.drafts[key] ?? row.to, disabled: ro, "aria-label": "target for " + row.from,
            oninput: (ev) => { ui.drafts[key] = ev.target.value; } })),
          h("td", { class: "dim", text: effort || "—" }),
          h("td", { class: "nowrap" },
            manage ? h("button", { type: "button", class: "small", onclick: () => saveMapping(row, ui.drafts[key] ?? row.to, key) }, "Save") : null,
            " ", msgEl(key)));
      });
      const addKey = "map:new";
      const add = ui.drafts[addKey] || {};
      const addRow = manage ? h("tr", null,
        h("td", null, h("input", { placeholder: "family, e.g. opus", value: add.from ?? "", "aria-label": "new mapping from",
          oninput: (ev) => { (ui.drafts[addKey] = ui.drafts[addKey] || {}).from = ev.target.value; } })),
        h("td", null, h("input", { placeholder: "route id", value: add.to ?? "", "aria-label": "new mapping to",
          oninput: (ev) => { (ui.drafts[addKey] = ui.drafts[addKey] || {}).to = ev.target.value; } })),
        h("td", null),
        h("td", { class: "nowrap" },
          h("button", { type: "button", class: "small", onclick: () => saveMapping({ from: (ui.drafts[addKey] || {}).from || "" }, (ui.drafts[addKey] || {}).to || "", addKey) }, "Add"),
          " ", msgEl(addKey))) : null;
      return h("div", { class: "panel" },
        h("h2", { text: "Model mappings" }),
        h("div", { class: "inline" },
          h("label", { class: "inline" },
            h("input", { type: "checkbox", checked: !!m.enabled, disabled: ro, onchange: (ev) => setMappingEnabled(ev.target.checked) }),
            "Route mapped Claude families to their targets"),
          h("span", { class: "pill " + (m.enabled ? "ready" : "badge"), text: m.enabled ? "On" : "Off" }),
          msgEl("map:enabled")),
        h("p", { class: "section-note", text: "Requests for a mapped family go to the target route instead." }),
        h("table", null,
          h("thead", null, h("tr", null, ["From", "To", "Effort", ""].map((t) => h("th", { text: t })))),
          h("tbody", null, rows, addRow)));
    },

    requests() {
      if (!manage) return lockedPanel("Recent requests");
      const j = ui.journal;
      const st = j.stats || {};
      const statRows = (st.accounts || []).map((a) => h("tr", null,
        h("td", { class: "nowrap" }, h("strong", { text: a.account })),
        h("td", { class: "dim", text: a.provider }),
        h("td", { class: "num", text: String(a.attempts) }),
        h("td", { class: "num" + (a.errors ? " bad" : ""), text: a.errors + " (" + pct(a.error_rate || 0) + ")" }),
        h("td", { class: "num", text: ms(a.p50_ms) }),
        h("td", { class: "num", text: ms(a.p95_ms) })));
      const stats = h("div", { class: "panel" },
        h("h2", { text: "Accounts, last 24 hours" }),
        h("p", { class: "section-note", text: "Attempts per account; latency is attempt start to first byte (or failure)." }),
        statRows.length
          ? h("table", null,
            h("thead", null, h("tr", null, ["Account", "Provider", "Attempts", "Errors", "p50", "p95"].map((t) => h("th", { text: t })))),
            h("tbody", null, statRows))
          : h("p", { class: "empty", text: j.stats ? "No requests in the last 24 hours." : "Loading…" }));

      const rows = [];
      (j.requests || []).forEach((r) => {
        const open = !!j.open[r.id];
        rows.push(h("tr", { class: "clickable" + (open ? " open" : ""), tabindex: 0, title: "Show the attempt chain",
          onclick: () => toggleRequest(r.id),
          onkeydown: (ev) => { if (ev.key === "Enter" || ev.key === " ") { ev.preventDefault(); toggleRequest(r.id); } } },
          h("td", { class: "nowrap dim", text: clock(r.time) }),
          h("td", { class: "mono", text: r.model || r.path || "—" }),
          h("td", null, outcomePill(r.outcome)),
          h("td", { class: "num", text: r.status == null ? "—" : String(r.status) }),
          h("td", { class: "nowrap", text: r.account || "—" }),
          h("td", { class: "num", text: String(r.attempts || 0) }),
          h("td", { class: "num", text: ms(r.ttfb_ms) }),
          h("td", { class: "num", text: ms(r.total_ms) }),
          h("td", { class: "dim", text: r.reason || "" })));
        if (open) rows.push(h("tr", { class: "chain-row" }, h("td", { colspan: 9 }, chainView(j.details[r.id]))));
      });
      const list = h("div", { class: "panel" },
        h("h2", { text: "Recent requests" }),
        h("div", { class: "inline" },
          h("label", { class: "inline" },
            h("input", { type: "checkbox", checked: j.errorsOnly, onchange: (ev) => setErrorsOnly(ev.target.checked) }),
            "Errors only"),
          j.error ? h("span", { class: "msg err", text: j.error }) : null),
        rows.length
          ? h("table", null,
            h("thead", null, h("tr", null, ["Time", "Model", "Outcome", "Status", "Account", "Attempts", "TTFB", "Total", "Reason"].map((t) => h("th", { text: t })))),
            h("tbody", null, rows))
          : h("p", { class: "empty", text: j.requests ? "No requests recorded." : "Loading…" }));

      const s = st.storage || {};
      const kv = (k, v) => [h("dt", { text: k }), h("dd", null, v)];
      const storage = h("div", { class: "panel" },
        h("h2", { text: "Storage used" }),
        h("dl", { class: "kv" },
          kv("Database", bytes(s.db_bytes)),
          kv("Logs", bytes(s.logs_bytes)),
          kv("Journal", typeof st.journal_requests === "number" ? st.journal_requests + " requests" : "—")));
      return [list, stats, storage];
    },

    events() {
      if (!manage) return lockedPanel("Events");
      const j = ui.journal;
      const rows = (j.events || []).map((e) => h("tr", null,
        h("td", { class: "nowrap dim", text: clock(e.time) + " (" + rel(e.time) + ")" }),
        h("td", { class: "nowrap" }, h("strong", { text: e.account })),
        h("td", { class: "nowrap" }, e.from_state ? pill(e.from_state) : h("span", { class: "dim", text: "—" }), " → ", e.to_state ? pill(e.to_state) : "—"),
        h("td", { text: reasonText(e.reason) || "" })));
      return h("div", { class: "panel" },
        h("h2", { text: "Events" }),
        h("p", { class: "section-note", text: "Every account state change, newest first." }),
        j.error ? h("p", { class: "msg err", text: j.error }) : null,
        rows.length
          ? h("table", null,
            h("thead", null, h("tr", null, ["When", "Account", "Change", "Reason"].map((t) => h("th", { text: t })))),
            h("tbody", null, rows))
          : h("p", { class: "empty", text: j.events ? "No events recorded." : "Loading…" }));
    },

    settings(d) {
      // Absent without a session (local paths are for the key holder); null = config unreadable.
      const profiles = d.profiles || [];
      const st = d.storage || {};
      const pk = "profiles";
      const prof = h("div", { class: "panel" },
        h("h2", { text: "Connected Claude Code profiles" }),
        profiles.length
          ? h("table", null, h("tbody", null, profiles.map((p) => h("tr", null,
              h("td", null, h("strong", { text: p.id })),
              h("td", { class: "mono dim", text: p.settings_dir }),
              h("td", { class: "nowrap" }, manage
                ? h("button", { type: "button", class: "small ghost", onclick: () => profileAction("disconnect", [p.id]) }, "Disconnect")
                : null)))))
          : h("p", { class: "empty", text: d.profiles === undefined ? "Not shown in the read-only view."
              : d.profiles === null ? "Profiles unavailable (config.toml could not be read)." : "No profiles connected." }),
        manage ? h("div", { class: "inline card-actions" },
          h("input", { placeholder: "profile id or settings dir", value: ui.drafts[pk] ?? "", "aria-label": "profile to connect",
            oninput: (ev) => { ui.drafts[pk] = ev.target.value; } }),
          h("button", { type: "button", class: "small", onclick: () => profileAction("connect", [ui.drafts[pk] || ""]) }, "Connect"),
          h("button", { type: "button", class: "small ghost", onclick: () => profileAction("connect", null) }, "Connect all found"),
          msgEl(pk)) : null);
      const kv = (k, v) => [h("dt", { text: k }), h("dd", null, v)];
      const about = h("div", { class: "panel" },
        h("h2", { text: "About" }),
        h("dl", { class: "kv" },
          kv("Version", d.version || "—"),
          kv("Session", manage ? "managing (session cookie)" : "read-only"),
          kv("Data dir", h("span", { class: "mono", text: st.home || "—" })),
          kv("Database", bytes(st.db_bytes)),
          kv("Logs", bytes(st.logs_bytes)),
          kv("Theme", h("span", null, document.documentElement.dataset.theme, " (", h("code", { text: THEME_KEY }), " in localStorage)"))));
      return [prof, about];
    },
  };

  function statusDetail(a) {
    const bits = [];
    const r = a.status === "expiring" ? null : reasonText(a.reason);
    if (r) bits.push(r);
    if (a.until && (a.status === "rate_limited" || a.status === "access_denied")) {
      bits.push("until " + clock(a.until) + " (" + rel(a.until) + ")");
    }
    if (a.status === "erroring" && a.count) bits.push(a.count + (a.count === 1 ? " failure" : " failures") + " in 15 min");
    if (loginExpiringSoon(a) && a.refresh_expires_at) bits.push(expiryCountdown(a.refresh_expires_at));
    return bits.join(" · ");
  }

  // "2 of 7 accounts ready · 3 in flight · 4 active sessions · 1 usage check failing"
  function heroSub(accts, pool, n) {
    const sum = (k) => accts.reduce((t, a) => t + (a[k] || 0), 0);
    const warned = accts.filter(usageWarning).length;
    return [(pool.available ?? 0) + " of " + n + " accounts ready",
      sum("in_flight") + " in flight",
      sum("sessions") + (sum("sessions") === 1 ? " active session" : " active sessions"),
      warned ? warned + (warned === 1 ? " usage check failing" : " usage checks failing") : null,
    ].filter(Boolean).join(" · ");
  }
  function usageCheckText(a) {
    const c = a.usage_check;
    if (!c) return "usage not checked yet";
    return c.ok ? "usage checked " + rel(c.at) : "usage check failed (" + (c.error || "error") + ", " + rel(c.at) + ")";
  }
  // "anthropic · max" (plan when known).
  function planText(a) {
    return [a.provider, a.plan].filter(Boolean).join(" · ");
  }
  // Routing knobs off their defaults, as chips: "P10", "×5 until 00:00".
  function routingChips(a) {
    const chips = [];
    if (typeof a.priority === "number" && a.priority !== 100) chips.push(h("span", { class: "chip", title: "priority (lower is preferred)", text: "P" + a.priority }));
    if (a.weight_override) chips.push(h("span", { class: "chip", title: "temporary weight, base " + a.weight, text: "×" + a.weight_override.weight + " until " + clock(a.weight_override.until) }));
    else if (typeof a.weight === "number" && a.weight !== 1) chips.push(h("span", { class: "chip", title: "routing weight", text: "×" + a.weight }));
    return chips;
  }
  // Reset text for a window: "resets 09:00 (in 4d 21h)", flagged when estimated or rolled forward.
  function resetText(w, short) {
    const r = nextReset(w, now());
    if (!r) return w && w.reset ? "reset passed, next not reported" : "reset not reported";
    const when = short ? rel(new Date(r.at).toISOString()) : clock(r.at) + " (" + rel(new Date(r.at).toISOString()) + ")";
    return "resets " + when + (r.assumed ? ", assumed (awaiting usage check)" : w.reset_estimated ? ", estimated" : "");
  }

  // One row of the Overview accounts table.
  function accountRow(a) {
    const used5 = fiveHourUsed(a), used7 = sevenDayUsed(a);
    const w5 = accountWindow(a, "5h"), w7 = accountWindow(a, "7d");
    const r7 = nextReset(w7, now());
    const meter = (used, w) => h("div", { class: "meter" }, bar(used),
      h("span", { class: "dim", text: used === null ? "—" : pct(used) + " used" + (w ? " · " + resetText(w, true) : "") }));
    const busy = (a.in_flight || 0) + (a.sessions || 0);
    return h("tr", { class: "sev-" + a.status },
      h("td", null,
        h("div", { class: "nowrap" }, h("strong", { text: a.id })),
        h("div", { class: "meta" }, h("span", { class: "dim", text: planText(a) }), routingChips(a),
          usageWarning(a) ? h("span", { class: "chip warn", title: usageCheckText(a), text: "usage check failed" }) : null)),
      h("td", null, pill(a.status),
        loginExpiringSoon(a) && a.status !== "expiring" ? [" ", h("span", { class: "pill badge warn", text: "expiring" })] : null,
        h("div", { class: "dim small nowrap", text: a.status_since ? "for " + dur(now() - Date.parse(a.status_since)) : "" })),
      h("td", { class: "metercell" }, isDead(a) ? h("span", { class: "dim", text: "—" }) : meter(used5, w5)),
      h("td", { class: "metercell" }, isDead(a) ? h("span", { class: "dim", text: "—" }) : meter(used7, null)),
      h("td", { class: "num" }, used7 === null || !r7
        ? h("span", { class: "dim", text: "—" })
        : [h("div", { text: clock(r7.at) }),
           h("div", { class: "dim small", text: rel(new Date(r7.at).toISOString()) + (r7.assumed ? " (assumed)" : w7.reset_estimated ? " (estimated)" : "") })]),
      h("td", { class: "num" + (busy ? "" : " dim"), text: (a.in_flight || 0) + " in flight · " + (a.sessions || 0) + (a.sessions === 1 ? " session" : " sessions") }));
  }

  function accountCard(a) {
    const dead = isDead(a);
    const kv = (k, v, cls) => [h("dt", { text: k }), h("dd", { class: cls || null }, v)];
    let expiry = "—";
    let expiryCls = null;
    if (a.refresh_expires_at) {
      const left = Date.parse(a.refresh_expires_at) - now();
      expiry = (left > 0 ? "expires " : "expired ") + rel(a.refresh_expires_at) + " (" + clock(a.refresh_expires_at) + ")";
      expiryCls = left <= 0 ? "bad-text" : left < expiryWarnMs() ? "warn-text" : null;
    }
    // The access token refreshes by itself; shown for completeness (never for a dead account).
    const token = dead || !a.expires_at ? null
      : Date.parse(a.expires_at) > now() ? "expires " + rel(a.expires_at) + " (refreshes by itself)" : "expired, refreshes on next use";
    const ov = a.weight_override;
    const routingText = "priority " + a.priority + " · weight " + a.effective_weight +
      (ov ? " (override until " + clock(ov.until) + ", base " + a.weight + ")" : "");
    const c = a.counters || {};
    const win = c.window || {};
    const life = c.lifetime || {};
    const since = c.window_started_at ? " since " + clock(c.window_started_at) : "";
    const requests = c.window
      ? num(win.requests) + since + (win.errors ? " (" + num(win.errors) + " failed)" : "") + " · " + num(life.requests) + " all time"
      : null;
    const tokens = c.window && (win.input_tokens || win.output_tokens || win.cache_read || win.cache_write)
      ? compact(win.input_tokens) + " in · " + compact(win.output_tokens) + " out · " + compact(win.cache_read) + " cache read · " + compact(win.cache_write) + " cache write" + since
      : null;
    const used7 = sevenDayUsed(a);
    const w7 = accountWindow(a, "7d");
    const week = used7 === null ? "—" : pct(used7) + " used" + (w7 ? " · " + resetText(w7) : "");
    const wins = windowsOf(a);
    const windows = dead || !wins.length
      ? h("p", { class: "dim usage-none", text: dead ? "— (no usage shown while logged out)" : "—" })
      : h("div", { class: "windows" }, wins.map((w) => [
          h("span", { class: "label", text: windowLabel(w.key, w.model) }),
          bar(w.utilization),
          h("span", { class: "val", text: pct(w.utilization) + " used · " + resetText(w, true) }),
        ]));
    const fresh = [
      a.usage_updated_at ? "snapshot " + rel(a.usage_updated_at) : null,
      a.unified_status ? "status " + a.unified_status : null,
    ].filter(Boolean).join(" · ");
    const check = a.usage_check;
    return h("div", { class: "card sev-" + a.status },
      h("div", { class: "card-head" },
        h("h3", { text: a.id }),
        h("span", { class: "pill badge", text: a.provider }),
        a.plan ? h("span", { class: "pill badge", title: "subscription plan", text: a.plan }) : null,
        pill(a.status),
        loginExpiringSoon(a) && a.status !== "expiring" ? h("span", { class: "pill badge warn", text: "expiring" }) : null),
      a.identity && a.identity.email ? h("div", { class: "email", text: a.identity.email }) : null,
      h("dl", { class: "kv" },
        kv("Since", a.status_since ? rel(a.status_since) + " (" + clock(a.status_since) + ")" : "—"),
        kv("Why", a.status === "expiring" || a.status === "erroring"
          ? statusDetail(a)
          : reasonText(a.reason) || (a.status === "ready" ? "usable" : "—")),
        a.until && a.status !== "expiring" ? kv("Until", clock(a.until) + " (" + rel(a.until) + ")") : null,
        dead && !manage ? kv("Fix", h("code", { text: "pooltime accounts login " + a.id })) : null,
        kv("7-day", week),
        kv("Login", expiry, expiryCls),
        token ? kv("Token", token) : null,
        a.tier ? kv("Rate tier", h("span", { class: "mono", text: a.tier })) : null,
        kv("Routing", routingText),
        kv("Activity", a.in_flight + " in flight · " + a.sessions + (a.sessions === 1 ? " session" : " sessions")),
        kv("Last used", a.last_used_at ? rel(a.last_used_at) + " (" + clock(a.last_used_at) + ")" : "—"),
        requests ? kv("Requests", requests) : null,
        tokens ? kv("Tokens", tokens) : null,
        a.last_error ? kv("Last error", reasonText(a.last_error.reason) + " · " + rel(a.last_error.at)) : null),
      h("div", { class: "usage" },
        h("div", { class: "usage-title" }, "Usage",
          check || fresh ? h("span", { class: "usage-fresh" + (check && !check.ok ? " warn-text" : ""),
            text: [check ? usageCheckText(a) : null, fresh || null].filter(Boolean).join(" · ") }) : null),
        windows),
      manage ? h("div", { class: "card-actions" },
        h("button", { type: "button", class: dead ? "small" : "small ghost", onclick: () => relogin.start([a.id]) }, "Re-login"),
        a.status === "billing" ? h("button", { type: "button", class: "small ghost", onclick: () => recheck(a.id) }, "Recheck billing") : null,
        msgEl("acct:" + a.id)) : null);
  }

  // ---- per-form messages ----
  function msgEl(key) {
    const m = ui.msgs[key];
    return h("span", { class: "msg row-msg" + (m ? (m.ok ? " ok" : " err") : ""), text: m ? m.text : "" });
  }
  function setMsg(key, ok, text) {
    ui.msgs[key] = { ok, text };
    if (ok) setTimeout(() => { if (ui.msgs[key] && ui.msgs[key].text === text) { delete ui.msgs[key]; render(); } }, 4000);
  }
  async function mutate(key, method, path, body) {
    try {
      const out = await api(method, path, body);
      setMsg(key, true, "Saved");
      await refresh();
      return out;
    } catch (e) {
      setMsg(key, false, e.message);
      render();
      return null;
    }
  }
  async function refresh() {
    try {
      const data = await api("GET", "/api/v1/status");
      ui.data = data;
      if (data && data.now) ui.skew = Date.parse(data.now) - Date.now();
    } catch (_) { /* the poll loop reports it */ }
    if (document.activeElement && document.activeElement.blur) document.activeElement.blur();
    render();
  }

  // ---- mutations ----
  async function saveRouting(a, explicit) {
    const key = "routing:" + a.id;
    let body = explicit;
    if (!body) {
      const dr = ui.drafts[key] || {};
      body = {};
      if (dr.priority !== undefined && dr.priority !== "") body.priority = Number(dr.priority);
      if (dr.weight !== undefined && dr.weight !== "") body.weight = Number(dr.weight);
      if (dr.until) {
        const t = new Date(dr.until);
        if (isNaN(t.getTime())) { setMsg(key, false, "until is not a valid time"); render(); return; }
        body.until = t.toISOString();
        if (body.weight === undefined) body.weight = Number(a.weight);
      }
      if (!Object.keys(body).length) { setMsg(key, false, "nothing changed"); render(); return; }
    }
    const out = await mutate(key, "POST", "/api/v1/accounts/" + encodeURIComponent(a.id) + "/routing", body);
    if (out) delete ui.drafts[key];
    render();
  }
  async function saveTuning() {
    const dr = ui.drafts.tuning || {};
    const body = {};
    for (const k of Object.keys(dr)) if (dr[k] !== "") body[k] = Number(dr[k]);
    if (!Object.keys(body).length) { setMsg("tuning", false, "nothing changed"); render(); return; }
    if (await mutate("tuning", "POST", "/api/v1/tuning", body)) delete ui.drafts.tuning;
    render();
  }
  async function saveMapping(row, to, key) {
    const m = { from: String(row.from || "").trim(), to: String(to || "").trim() };
    if (row.effort) m.effort = row.effort;
    if (!m.from || !m.to) { setMsg(key, false, "from and to are required"); render(); return; }
    if (await mutate(key, "POST", "/api/v1/mappings", { mappings: [m] })) delete ui.drafts[key];
    render();
  }
  function setMappingEnabled(on) { mutate("map:enabled", "POST", "/api/v1/mappings", { enabled: on }); }
  async function profileAction(kind, names) {
    const body = names === null ? { all: true } : { profiles: names.filter((n) => n.trim()) };
    if (body.profiles && !body.profiles.length) { setMsg("profiles", false, "enter a profile id or directory"); render(); return; }
    if (await mutate("profiles", "POST", "/api/v1/profiles/" + kind, body)) delete ui.drafts.profiles;
    render();
  }
  function recheck(id) { mutate("acct:" + id, "POST", "/api/v1/accounts/" + encodeURIComponent(id) + "/recheck", {}); }

  // ---- re-login flow ------------------------------------------------------------------------
  // POST /accounts/{id}/login -> open authorize_url in a new tab -> poll GET /signin/{id} until
  // it settles: saved | mismatch (retry) | needs_binding (confirm -> POST /bind) | failed.
  // The code#state paste box (POST /signin/{id}/code) covers a browser on another machine.
  const relogin = {
    queue: [],
    index: 0,
    signin: null,
    timer: null,

    start(ids) {
      this.queue = ids.slice();
      this.index = 0;
      const d = $("relogin");
      if (!d.open) d.showModal();
      this.begin();
    },

    current() { return this.queue[this.index]; },

    hasMore() { return this.index + 1 < this.queue.length; },

    show(opts) {
      $("relogin-title").textContent = "Re-login: " + this.current();
      $("relogin-step").textContent = this.queue.length > 1 ? "Account " + (this.index + 1) + " of " + this.queue.length : "";
      $("relogin-status").textContent = opts.status || "";
      $("relogin-status").className = "status-line" + (opts.err ? " bad-text" : "");
      $("relogin-link-row").hidden = !opts.link;
      if (opts.link) $("relogin-link").href = opts.link;
      $("relogin-code").hidden = !opts.code;
      $("relogin-bind").hidden = !opts.bind;
      $("relogin-retry").hidden = !opts.retry;
      // Every settled-but-unsaved state (retry offered) can also skip ahead while accounts remain.
      $("relogin-next").hidden = !((opts.next || opts.retry) && this.hasMore());
    },

    async begin() {
      clearTimeout(this.timer);
      this.signin = null;
      const id = this.current();
      // Open the tab now, inside the click, so popup blockers allow it; navigate it once the
      // URL is known. The opener link is cut so the sign-in page cannot script this tab.
      let tab = null;
      try { tab = window.open("about:blank", "_blank"); if (tab) tab.opener = null; } catch (_) { tab = null; }
      this.show({ status: "Starting sign-in…" });
      try {
        const out = await api("POST", "/api/v1/accounts/" + encodeURIComponent(id) + "/login", { mode: "callback" });
        this.signin = out.signin_id;
        if (tab) tab.location.href = out.authorize_url;
        this.show({ status: "Waiting for you to sign in in the new tab…", link: out.authorize_url, code: true });
        this.timer = setTimeout(() => this.check(), SIGNIN_POLL_MS);
      } catch (e) {
        if (tab) tab.close();
        this.show({ status: "Could not start sign-in: " + e.message, err: true, retry: true });
      }
    },

    async check() {
      const sid = this.signin;
      if (!sid || !$("relogin").open) return;
      let v;
      try {
        v = await api("GET", "/api/v1/signin/" + encodeURIComponent(sid));
      } catch (e) {
        if (e.status === 404) { this.show({ status: "The sign-in expired.", err: true, retry: true }); return; }
        this.timer = setTimeout(() => this.check(), SIGNIN_POLL_MS);
        return;
      }
      if (sid !== this.signin) return;
      const who = (i) => (i && i.email) || "an account without an email on file";
      switch (v.status) {
        case "pending":
          this.timer = setTimeout(() => this.check(), SIGNIN_POLL_MS);
          return;
        case "saved":
          this.done("Signed in as " + who(v.identity) + ". Saved.");
          return;
        case "needs_binding":
          this.show({ status: "Signed in as " + who(v.got) + ". This account has no identity on file yet: bind it to " + who(v.got) + "?", bind: true, retry: true });
          return;
        case "mismatch":
          this.show({ status: v.message || ("Signed in as " + who(v.got) + ", expected " + who(v.expected) + ". Nothing was saved."), err: true, retry: true });
          return;
        default:
          this.show({ status: "Sign-in failed: " + (v.message || v.status), err: true, retry: true });
      }
    },

    done(text) {
      this.show({ status: text, next: true });
      refresh();
    },

    async bind() {
      try {
        const out = await api("POST", "/api/v1/signin/" + encodeURIComponent(this.signin) + "/bind", {});
        this.done("Bound to " + ((out.identity && out.identity.email) || "this sign-in") + ". Saved.");
      } catch (e) {
        this.show({ status: "Could not bind: " + e.message, err: true, retry: true });
      }
    },

    async submitCode(code) {
      if (!this.signin || !code.trim()) return;
      try {
        await api("POST", "/api/v1/signin/" + encodeURIComponent(this.signin) + "/code", { code: code.trim() });
        $("relogin-code-input").value = "";
        this.show({ status: "Code accepted, finishing…" });
        clearTimeout(this.timer);
        this.timer = setTimeout(() => this.check(), 300);
      } catch (e) {
        $("relogin-status").textContent = "Code rejected: " + e.message;
        $("relogin-status").className = "status-line bad-text";
      }
    },

    close() {
      clearTimeout(this.timer);
      this.signin = null;
      const d = $("relogin");
      if (d.open) d.close();
    },
  };

  // ---- wiring -------------------------------------------------------------------------------
  function route() {
    const v = (location.hash || "").replace(/^#/, "");
    ui.view = VIEWS[v] ? v : "overview";
    render();
    loadJournal().then(render);
  }

  function init() {
    manage = document.body.dataset.mode === "manage";
    // A key that was not accepted (or any leftover query) never stays in the address bar.
    if (location.search) history.replaceState(null, "", location.pathname + location.hash);

    $("theme").addEventListener("click", () => {
      const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
      applyTheme(next);
      try { localStorage.setItem(THEME_KEY, next); } catch (_) { /* private mode */ }
      render();
    });
    $("key-form").addEventListener("submit", async (ev) => {
      ev.preventDefault();
      const k = $("key-input").value.trim();
      if (!k) return;
      try {
        await api("POST", "/api/v1/session", { k });
        location.reload();
      } catch (e) {
        $("key-msg").textContent = e.message;
        $("key-msg").className = "msg err";
      } finally {
        $("key-input").value = "";
      }
    });
    $("relogin-close").addEventListener("click", () => relogin.close());
    $("relogin").addEventListener("close", () => relogin.close());
    $("relogin-retry").addEventListener("click", () => relogin.begin());
    $("relogin-bind").addEventListener("click", () => relogin.bind());
    $("relogin-next").addEventListener("click", () => { relogin.index++; relogin.begin(); });
    $("relogin-code").addEventListener("submit", (ev) => { ev.preventDefault(); relogin.submitCode($("relogin-code-input").value); });
    window.addEventListener("hashchange", route);
    route();
    poll();
  }

  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", init);
  else init();
})();
