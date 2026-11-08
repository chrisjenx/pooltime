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
  const EXPIRY_WARN_MS = 3 * 24 * 3600 * 1000;

  // Exact status words (spec §4/§16): the same everywhere.
  const WORDS = {
    ready: "Ready",
    needs_login: "Needs login",
    rate_limited: "Rate-limited",
    billing: "Billing",
    access_denied: "Access denied",
  };
  const SEVERITY = { needs_login: 0, billing: 1, access_denied: 2, rate_limited: 3, ready: 4 };
  const REASONS = {
    never_logged_in: "never logged in",
    refresh_token_dead: "refresh token rejected by the provider",
    refresh_token_expired: "login expired",
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
  };

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
  function weekWindow(a) {
    return windowsOf(a).find((w) => !w.model && String(w.key).split(/[-_]/).includes("7d")) || null;
  }
  function sevenDayLeft(a) {
    return isDead(a) || typeof a.seven_day_left !== "number" ? null : a.seven_day_left;
  }
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
  function sorted(accounts) {
    return accounts.slice().sort((a, b) =>
      (SEVERITY[a.status] ?? 9) - (SEVERITY[b.status] ?? 9) || String(a.id).localeCompare(String(b.id)));
  }
  function loginExpiringSoon(a) {
    if (isDead(a) || !a.refresh_expires_at) return false;
    const left = Date.parse(a.refresh_expires_at) - now();
    return left > 0 && left < EXPIRY_WARN_MS;
  }
  function pill(status) {
    return h("span", { class: "pill " + status, text: WORDS[status] || status });
  }
  function bar(fraction, invert) {
    // fraction = share to fill; invert = true when the fill is "used" (high is bad)
    if (typeof fraction !== "number") return h("div", { class: "bar none", title: "unknown" });
    const f = Math.max(0, Math.min(1, fraction));
    const good = invert ? 1 - f : f;
    const cls = good < 0.15 ? "bar low" : good < 0.4 ? "bar mid" : "bar";
    const fill = h("span");
    fill.style.width = (f * 100).toFixed(1) + "%";
    return h("div", { class: cls }, fill);
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

  async function poll() {
    try {
      const data = await api("GET", "/api/v1/status");
      ui.data = data;
      ui.error = null;
      ui.lastOk = Date.now();
      if (data && data.now) ui.skew = Date.parse(data.now) - Date.now();
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
    const fn = VIEWS[ui.view] || VIEWS.overview;
    $("view-" + ui.view).replaceChildren(...[].concat(fn(ui.data)));
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

  function renderBanner() {
    const b = $("banner");
    const accts = ui.data.accounts || [];
    const dead = accts.filter(isDead);
    const expiring = accts.filter(loginExpiringSoon);
    const blocked = accts.filter((a) => a.status === "billing" || a.status === "access_denied");
    const parts = [];
    if (dead.length) parts.push(dead.length + (dead.length === 1 ? " account needs" : " accounts need") + " login");
    if (expiring.length) parts.push(expiring.length + (expiring.length === 1 ? " login expires" : " logins expire") + " within 3 days");
    if (blocked.length) parts.push(blocked.length + " blocked (billing / access)");
    if (!parts.length) { b.hidden = true; return; }
    b.hidden = false;
    b.className = dead.length || blocked.length ? "banner" : "banner warn";
    const queue = sorted(dead).concat(expiring).map((a) => a.id);
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
      const total = typeof pool.seven_day_left_total === "number" ? pool.seven_day_left_total : 0;
      const n = pool.accounts || accts.length;
      const counts = {};
      accts.forEach((a) => { counts[a.status] = (counts[a.status] || 0) + 1; });
      const hero = h("div", { class: "panel" },
        h("h2", { text: "7-day picture" }),
        h("div", { class: "hero" },
          h("div", null,
            h("div", { class: "big" }, Math.max(0, total).toFixed(2), " ", h("small", { text: "of " + n + " subscriptions left this week" })),
            h("div", { class: "sub", text: (pool.available ?? 0) + " of " + n + " accounts ready" })),
          bar(n ? total / n : null, false)),
        h("div", { class: "counts" },
          Object.keys(WORDS).filter((s) => counts[s]).map((s) =>
            h("span", { class: "pill " + s, text: counts[s] + " " + WORDS[s] }))));

      const rows = sorted(accts).map((a) => {
        const left = sevenDayLeft(a);
        const w = weekWindow(a);
        return h("tr", null,
          h("td", { class: "nowrap" }, h("strong", { text: a.id })),
          h("td", null, pill(a.status)),
          h("td", { class: "barcell" }, bar(left, false)),
          h("td", { class: "num", text: left === null ? "—" : pct(left) + " left" }),
          h("td", { class: "num dim", text: left === null || !w || !w.reset ? "—" : "resets " + clock(w.reset) + " (" + rel(w.reset) + ")" }));
      });
      const bars = h("div", { class: "panel" },
        h("h2", { text: "7-day headroom by account" }),
        rows.length
          ? h("table", null, h("thead", null, h("tr", null,
              ["Account", "State", "7-day left", "", "Reset"].map((t) => h("th", { text: t })))),
            h("tbody", null, rows))
          : h("p", { class: "empty", text: "No accounts yet. Add one with `pooltime accounts add`." }));

      const attention = sorted(accts).map((a) => h("tr", null,
        h("td", { class: "nowrap" }, h("strong", { text: a.id })),
        h("td", null, pill(a.status)),
        h("td", { class: "dim nowrap", text: a.status_since ? "for " + dur(now() - Date.parse(a.status_since)) : "—" }),
        h("td", { text: statusDetail(a) || "" }),
        h("td", { class: "nowrap" }, a.status === "needs_login" && manage
          ? h("button", { type: "button", class: "small", onclick: () => relogin.start([a.id]) }, "Re-login")
          : null)));
      const list = h("div", { class: "panel" },
        h("h2", { text: "Accounts by severity" }),
        attention.length ? h("table", null, h("tbody", null, attention)) : h("p", { class: "empty", text: "—" }));
      return [hero, bars, list];
    },

    accounts(d) {
      const accts = sorted(d.accounts || []);
      if (!accts.length) return h("div", { class: "panel empty", text: "No accounts yet. Add one with `pooltime accounts add`." });
      return h("div", { class: "cards" }, accts.map(accountCard));
    },

    routing(d) {
      const accts = sorted(d.accounts || []);
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
          field("min_headroom", "Minimum headroom", "Accounts below this 7-day headroom are skipped while others have room."),
          field("headroom_taper_start", "Headroom taper start", "Below this headroom an account's share tapers off.")),
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
    const r = reasonText(a.reason);
    if (r) bits.push(r);
    if (a.until && (a.status === "rate_limited" || a.status === "access_denied")) {
      bits.push("until " + clock(a.until) + " (" + rel(a.until) + ")");
    }
    if (loginExpiringSoon(a)) bits.push("login expires " + rel(a.refresh_expires_at));
    return bits.join(" · ");
  }

  function accountCard(a) {
    const dead = isDead(a);
    const kv = (k, v, cls) => [h("dt", { text: k }), h("dd", { class: cls || null }, v)];
    let expiry = "—";
    let expiryCls = null;
    if (a.refresh_expires_at) {
      const left = Date.parse(a.refresh_expires_at) - now();
      expiry = (left > 0 ? "expires " : "expired ") + rel(a.refresh_expires_at) + " (" + clock(a.refresh_expires_at) + ")";
      expiryCls = left <= 0 ? "bad-text" : left < EXPIRY_WARN_MS ? "warn-text" : null;
    }
    const ov = a.weight_override;
    const routingText = "priority " + a.priority + " · weight " + a.effective_weight +
      (ov ? " (override until " + clock(ov.until) + ", base " + a.weight + ")" : "");
    const wins = windowsOf(a);
    const windows = dead || !wins.length
      ? h("p", { class: "dim usage-none", text: dead ? "— (no usage shown while logged out)" : "—" })
      : h("div", { class: "windows" }, wins.map((w) => [
          h("span", { class: "label", text: windowLabel(w.key, w.model) }),
          bar(w.utilization, true),
          h("span", { class: "val", text: pct(w.utilization) + " used · " + (w.reset ? "resets " + rel(w.reset) : "—") }),
        ]));
    return h("div", { class: "card sev-" + a.status },
      h("div", { class: "card-head" },
        h("h3", { text: a.id }),
        h("span", { class: "pill badge", text: a.provider }),
        pill(a.status),
        loginExpiringSoon(a) ? h("span", { class: "pill badge warn", text: "expiring" }) : null),
      a.identity && a.identity.email ? h("div", { class: "email", text: a.identity.email }) : null,
      h("dl", { class: "kv" },
        kv("Since", a.status_since ? rel(a.status_since) + " (" + clock(a.status_since) + ")" : "—"),
        kv("Why", reasonText(a.reason) || (a.status === "ready" ? "usable" : "—")),
        a.until ? kv("Until", clock(a.until) + " (" + rel(a.until) + ")") : null,
        kv("Login", expiry, expiryCls),
        kv("Routing", routingText),
        kv("Activity", a.in_flight + " in flight · " + a.sessions + " sessions"),
        a.last_error ? kv("Last error", reasonText(a.last_error.reason) + " · " + rel(a.last_error.at)) : null,
        kv("7-day left", sevenDayLeft(a) === null ? "—" : pct(sevenDayLeft(a)))),
      h("div", { class: "usage" }, h("div", { class: "usage-title", text: "Usage" }), windows),
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
