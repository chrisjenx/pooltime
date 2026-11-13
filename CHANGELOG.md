# Changelog

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
