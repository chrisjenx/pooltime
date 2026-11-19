# Changelog

## Unreleased

### Added

- Runway forecast: will the pool's weekly (7-day window) quota last until capacity comes back?
  From the pool's burn rate over the last 24 hours and each usable account's 7-day headroom and
  own reset time (an account resetting soon adds capacity), pooltime projects when every usable
  account is spent and compares that with the next reset. `pooltime status` shows a `runway:`
  line, the dashboard's weekly burn panel shows it too, the MCP `pool_usage` tool and
  `GET /api/v1/analytics/burn` return it as `runway`, and while it is at risk the status line
  segment adds `⚠ 7d out ~14h early`. See [Runway forecast](docs/relogin.md#runway-forecast).
- Runway alert: a desktop notification when the pool is projected to run out at least
  `alerts.runway_warn_hours` (default 6; `0` turns it off) before the next reset, at most once
  per reset cycle: "At the current rate the pool runs out ~14h before the next reset (Thu 09:00)".
  Only a shortfall within the next 7 days warns (one further out is shown only), and nothing
  warns until there is a full day of usage history (the forecast is marked low confidence
  until then).
- `pooltime doctor` notes (`i voice`, never a failure) that Claude Code's `/voice` dictation is
  unavailable in pooled sessions (Claude Code turns it off while `ANTHROPIC_AUTH_TOKEN` is set,
  and voice audio goes to api.anthropic.com directly), and suggests the OS dictation instead.

### Fixed

- An empty pool's weekly burn read `-0.0 subs used`; it now reads `0.0`.
- A desktop notification could fail to start with "text file busy" when the notifier program had
  just been written (seen in tests); starting it is now retried briefly.

## 1.2.1 - 2026-11-15

### Fixed

- Log files now stay within `retention.logs_max_mb` while the pool runs. Old logs were only
  pruned when the daemon started, and since 1.1.0 the service's worker runs for days, so
  `~/.pooltime/logs` kept growing past the cap until the next restart. The running pool now also
  prunes them every hour (files older than `logs_days`, then the oldest until the log files are
  under the cap). The file being written (the newest date in its name) is never deleted; if it
  alone is over the cap, every other log file goes. `pooltime doctor`'s storage line shows the
  logs against the cap.
- `retention.logs_days = 0` and `retention.logs_max_mb = 0` now mean "no limit" (no age limit,
  no size cap), like `journal_max_requests = 0`. Before, 0 deleted every log file but the
  current one.
- A host that does not resolve (this machine's DNS is down, or a DNS filter blocks it) no longer
  counts against your accounts. Before, every request tried each account in turn, failed each
  with `transport:dns`, recorded it as that account's error, and DNS failures of token refreshes
  could mark accounts `erroring`. Now:
  - Nothing is recorded against the account: its status and health are unchanged, the journal
    shows `network:dns` (API host) or `refresh:network:dns` (token host), and `doctor` leaves
    these out of the accounts' error rates.
  - When a provider's API host does not resolve, its other accounts are skipped (they send to the
    same host), and so is the provider until its next check: one real lookup at most every 2
    seconds, and never more than 5 seconds of skipping after the last failed lookup, so a
    resolver that is down for a moment (wake from sleep, a Wi-Fi change) costs seconds. Any
    answer clears it at once. A mapped request still goes to the other provider. With nowhere
    left to go, the request gets a retryable 502 `api_error` "pooltime: network unavailable (DNS
    lookup failed)", which Claude Code backs off and retries on.
  - When the token host does not resolve, an account whose token is due for refresh but not yet
    expired keeps using it; one whose token has expired is skipped for this request, and another
    account with a fresh token serves it.
  - The log has one warning per host when its lookups start failing and one when it answers
    again, instead of one per attempt. A host failing for 10 minutes shows in `pooltime doctor`
    (`dns <host>`), the dashboard banner and the status API (`network.dns_down`).
  - Other transport failures (connection reset, refused, TLS, timeouts) still fail over as before.

## 1.2.0 - 2026-11-14

### Changed

- Profile sync no longer loads a connected profile's whole `.claude.json` into memory. It parses
  only the two keys pooltime owns there (`mcpServers` and `additionalModelOptionsCache`) and
  writes every other value back as its exact original bytes, so the idle memory of a pool with a
  connected profile no longer grows with the size of that file, and nothing pooltime does not own
  is ever reformatted. The settings `env` checks read only `env` from `settings.json`.

### Routing explained

- The dashboard's Routing view has a **Next request** panel: pick a model (it starts on your most
  requested Claude model this week) and it says which account the next request goes to and why,
  in one line ("Next request for claude-opus-4-5 goes to personal: highest score (3.75, ahead of
  team-1 at 2.50), 7-day 37% used, resets in 4d 21h."). Below it, one row per account: eligible or
  the reason it is out (signed out, rate-limited until, billing, reserve tier, over the 5-hour
  limit, at its in-flight cap, ...), 5-hour and 7-day used, 7-day reset, expiry share, taper,
  priority, weight and any timed override, pinned sessions, the score with a bar, and a Next
  marker. It refreshes with the page.
- New `GET /api/v1/routing/preview?model=<id>` (management key) behind it, and the MCP tool
  `explain_routing`. The preview runs the router's own code (the scorer now exposes its
  per-account breakdown as `routing::rank`, which `choose` decides on) and changes nothing: no
  tie-break turn is used, no session is pinned, nothing is journaled.
- The Routing view's help for **Minimum headroom** now says 5-hour headroom (it said 7-day; the
  setting has always applied to the 5-hour window).

### Fixed

- Linux: `pooltime service restart` and `systemctl --user reload pooltime` no longer need
  `/bin/kill` (missing on NixOS): the unit's `ExecReload` runs pooltime itself
  (hidden `<bin> service signal-reload $MAINPID`). A unit written by 1.1.x is updated in place on
  the next `service restart`, without restarting the service (a hand-edited, symlinked or
  read-only unit is left as it is, with a warning, and the reload still goes ahead). CI now runs
  install, restart under load and uninstall against a real systemd user manager.
- A seamless restart no longer waits for blocking work a request started (a profile sync from
  `connect`, a model refresh): the old generation waits only for its own background tasks' work.
- A seamless restart can no longer leave stale account state behind: if the old generation's
  release runs out of time while its database queue is still flushing, the account-state rows
  still queued are dropped instead of landing after the new generation has loaded its state.

## 1.1.1 - 2026-11-13

### Fixed

- Claude Code's connectivity probe (`HEAD /api/hello`) gets an empty 200 instead of a 404, so it
  no longer adds "unknown endpoint" lines to the log and error rows to the request journal.

## 1.1.0 - 2026-11-13

### Changed

- Usage is shown as **used**, not left, everywhere a person reads it, matching the usage page on
  claude.ai ("40% used"). Dashboard bars fill from 0 to 100% as usage grows (blue, amber from 60%,
  red from 85%); the Overview table reads "5-hour used" and "7-day used", the pool picture reads
  "1.79 of 7 subscriptions used this week", and the Sort menu offers "7-day used" and "5-hour used"
  (a sort saved as "7-day left" or "5-hour left" keeps its order). `pooltime status` and
  `accounts list` show `7D USED` and `pool 2/3 ready · 7d 77% used`; the Claude Code status line
  segment reads `pool 6/8 · 7d 59% used`. The JSON API only gains fields: `seven_day_used` per
  account and `pool.seven_day_used_total` (also in the MCP `pool_usage` answer); `seven_day_left`
  and `seven_day_left_total` are unchanged, and routing is untouched.

### Seamless restarts

- `pooltime service restart` no longer makes new requests wait while the old daemon finishes its
  streams (it used to answer them with 503 for as long as the longest reply took). The service
  now runs `pooltime serve --supervise`: a small supervisor that owns the port and runs the
  daemon as worker processes. A restart starts a new worker from the installed binary (so
  `brew upgrade pooltime && pooltime service restart` picks up the new version), lets it take new
  connections within about a second, and lets the old worker finish its in-flight requests and
  exit. A crashed worker is replaced at once while clients wait on the open port. `service
  status` and `doctor` show the supervisor and its worker generations. See
  [`docs/specs/2026-11-13-seamless-restart.md`](docs/specs/2026-11-13-seamless-restart.md).
- **Once after upgrading:** run `pooltime service install` (or just `pooltime service restart`,
  which notices the old service definition and reinstalls it for you). That one restart still
  waits for in-flight requests (it shows progress, and a Ctrl-C does not leave the service
  unloaded); every later one hands over without a pause. `doctor` warns until it is done.
- One stop signal stops the pool gracefully even when it reaches every pooltime process at once
  (`pkill pooltime`, a service manager signalling the process group): in-flight streams finish.
- Only one daemon process at a time runs the background work and writes account state; the
  new one picks up what the old one learned (rate limits, expired logins, billing blocks).
- `/health` also reports the answering process's `pid`.

### Fixed

- `POST /v1/messages/count_tokens` (Claude Code's token counting) no longer fails with
  "unknown endpoint". Claude models are counted by Anthropic through a pooled account, with the
  same credential handling and proxy-key enforcement as `/v1/messages`; it never pins a session,
  counts as a served request or touches usage and burn analytics. Models Codex serves get a local
  estimate (request bytes / 4, flagged `x-pool-estimate: 1`) because OpenAI has no such endpoint.
  The request shows in the journal without attempts.

## 1.0.1 - 2026-11-12

### Dashboard

- Accounts are listed soonest 7-day reset first by default, with a remembered sort menu
  (7-day reset, 7-day left, 5-hour left, state, name, activity), search and state/provider
  filters.
- The account summary now shows everything bro's did: 5-hour and 7-day resets as columns
  (passed resets rolled forward and marked estimated), plan and rate tier, last used, usage
  freshness and errors, access-token expiry, request and token counts, and sessions in flight.
  "Accounts by severity" became "Needs attention". See
  [`docs/dashboard-parity.md`](docs/dashboard-parity.md).
- A favicon.

### Sign-in

- Pasting Claude's 6-digit email verification code into pooltime now explains where it goes
  (the Claude sign-in tab) instead of reporting a malformed code.

### Build

- Smaller, faster dev and test builds: trimmed debug info and a single integration-test binary.

## 1.0.0 - 2026-11-08

First stable release. pooltime replaces the pool in bro-cli.

### Pool and proxy (M1)

- One local Anthropic-compatible endpoint (`127.0.0.1:3460`) in front of several Claude Max/Pro
  and ChatGPT (Codex) accounts. Anthropic SSE is forwarded byte for byte; Codex Responses streams
  are translated to Anthropic messages, including thinking blocks and tool calls.
- Routing by 5-hour and 7-day headroom with sticky sessions, priorities and weights, failover on
  quota 429s, overload retries with backoff, and Claude-family to Codex model mappings.
- Token refresh, usage polling, billing and access-denied detection, and cooldowns until reset.
- Browser and paste sign-in for Claude and ChatGPT accounts, or reuse of a Codex CLI sign-in.
- `pooltime connect` points Claude Code profiles at the pool and pins the newest models;
  `disconnect` restores them. Profiles stay in sync while the pool runs.
- `pooltime import bro`, `pooltime setup`, a launchd / systemd `--user` service, and a local web
  dashboard with a management API.
- Static musl builds for Linux, Homebrew tap and an install script.

### Follow-ups (F1-F8)

- Account health states: `expiring` and `erroring`, expiry countdowns, proactive needs-login
  checks, and batch re-login (`pooltime accounts relogin --expiring | --dead | --all`).
- A status line segment composed with the profile's own status line (`connect --statusline`).
- Desktop notifications for expiring and dead accounts (macOS, Linux `notify-send`).
- A request journal with attempt chains (`pooltime logs --request`), the Requests and Events
  views, time-boxed debug mode, and retention.
- Weekly "subscriptions burned" analytics with a forecast and idle-account list.
- An MCP server for Claude Code (`connect --mcp`): usage, health, re-login links, weights,
  recent errors and request explanations.
- A full `pooltime doctor`.
- Documentation for users and coding agents (`README.md`, `docs/`, `AGENTS.md`, `llms.txt`).

### Performance

On the same machine as bro, against fake upstreams: about 40% of bro's idle memory in a fresh
daemon (the gap narrows with a large connected `.claude.json`) and 1.5–2.7× faster cold starts. See [`docs/perf.md`](docs/perf.md).
