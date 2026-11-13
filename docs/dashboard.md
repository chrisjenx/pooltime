# Dashboard

The dashboard is built into the daemon: plain HTML and JavaScript served from
`http://127.0.0.1:3460/`, nothing loaded from anywhere else.

```sh
pooltime open
```

`open` prints `http://127.0.0.1:<port>/?k=<key>` and opens it. The key (`~/.pooltime/run/mgmt.key`)
is traded for a browser-session cookie on first load and removed from the address bar. Opened
without the key, the page is **read-only**: it shows the status overview only, with a hint to run
`pooltime open`. Requests whose `Host`/`Origin` is not `127.0.0.1:<port>` or `localhost:<port>` are
refused.

![Overview](img/dashboard-overview.png)

## Views

| View | What it shows / does |
|---|---|
| **Overview** | Alert banner ("2 accounts need login · 1 login expires within 3 days", **Re-login all**); the 7-day picture (subscriptions' worth of usage this week, "1.79 of 7 subscriptions used", with a bar that fills as it grows; accounts ready, in-flight requests, active sessions, failing usage checks, accounts by state); **Needs attention** (every account that is not ready, has a login expiring soon or a failing usage check, most severe first, with why and since when); the accounts table (plan, non-default routing, 5-hour and 7-day usage, 7-day reset, activity), sortable and filterable; weekly burn (subscriptions used per week, this week's forecast) |
| **Accounts** | One card per account, same sort and filters as the Overview: state, since when and why, plan and rate tier, 7-day usage and reset, login and access-token expiry, routing, in-flight requests and sessions, last used, requests and tokens (current window and all time), last error, usage per window with when it was last checked; **Re-login** (and **Recheck billing** for `billing`) |
| **Routing** | Priority, weight and temporary weight overrides ("until"); routing tuning (`min_headroom`, `five_hour_exp`, `headroom_taper_start`) |
| **Mappings** | Map a Claude model family (e.g. `opus`) to a Codex route; on/off switch |
| **Requests** | Recent requests (filter to errors), click one for its attempt chain; per-account error rate and p50/p95 latency over 24 h; storage used |
| **Events** | Every account state change, newest first, with the reason |
| **Settings** | Connected Claude Code profiles (connect / disconnect); about: version, data dir, theme |

Requests, Events and the weekly burn need the key (they are not in the read-only view).

![Requests](img/dashboard-requests.png)

## Reading the overview

- **7-day used** is how much of the account's provider weekly window is spent, as a percentage
  ("40% used"), like the usage page on claude.ai. Bars fill from empty to full as usage grows:
  blue while there is room, amber from 60% and red from 85%. The pool total adds the accounts up
  as "N of M subscriptions used this week" (one account = one subscription; an account that is
  logged out or has no 7-day window yet has no headroom and counts as fully used). The JSON API
  keeps `seven_day_left` (the complement) next to `seven_day_used`.
- An account that is not usable (`needs_login`, `billing`, `access_denied`) shows a hatched bar and
  `—`, never stale usage.
- Accounts are listed soonest 7-day reset first (the subscription to use up before it resets).
  Accounts with no known 7-day reset, including every `needs_login` one, come after, by name. The
  **Sort** menu also orders by 7-day used, 5-hour used (most used first), state, name or activity
  (**Reverse** flips it); the choice is remembered in the browser (`pooltime-sort` in
  localStorage; a choice saved by an older version as "7-day left" or "5-hour left" keeps its order). Search and the
  state / provider menus filter the list.
- **Needs attention** always lists the accounts that need a look most severe first (needs login on
  top), with the classified reason (e.g. "refresh token rejected by the provider") and how long it
  has been in that state.
- A reset time that has already passed (no usage check since) is shown rolled forward one window
  and marked "assumed"; a reset pooltime made up for a spent window is marked "estimated".

State words are the same everywhere; see [Accounts](accounts.md#look-at-them).

## Screenshots without real accounts

For development, `POOLTIME_FIXTURE_STATE=<file.json>` makes `pooltime serve` answer the dashboard's
reads from a fixture document instead of live state. Never point it at real data. The screenshots
here come from `tests/support/fixtures/dashboard.json` served that way (with a throwaway
`POOLTIME_HOME` and port), captured with headless Chrome (`--headless --screenshot`, 1280 px wide).

Related: [Debugging](debugging.md) (the Requests view in depth), [Accounts](accounts.md), [parity with bro's dashboard](dashboard-parity.md).
