# Changelog

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

On the same machine as bro, against fake upstreams: about a third of bro's idle memory and faster
cold starts. See [`docs/perf.md`](docs/perf.md).
