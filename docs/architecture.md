# Architecture

One Rust binary. `pooltime serve` is a tokio/axum daemon on `127.0.0.1:3460` that exposes:

| Path | What |
|---|---|
| `POST /v1/messages` | Anthropic Messages API (what Claude Code calls); the proxy key is optional in config, but `connect` generates one and the running daemon enforces it at once |
| `POST /v1/messages/count_tokens` | token counting: forwarded to Anthropic with a pooled account for Claude models, a local estimate for Codex-served models (see below); same Host/Origin guard and proxy key as `/v1/messages` |
| `GET /v1/models` | the model list Claude Code sees |
| `GET /health` | identity check (so nobody trusts or kills a foreign listener) |
| `/api/v1/*` | management API: the one layer the dashboard, CLI and MCP all call |
| `GET /` | embedded dashboard (`web/`, compiled in) |
| `POST /mcp` | MCP server (Streamable HTTP, its own token) |

Every other CLI command is a thin client of the management API, except a few that work on files
with no daemon (`logs`, `disconnect`, `statusline`, `import bro`, `doctor`, sign-in when no daemon
runs).

## Module map (`src/`)

| Module | Responsibility |
|---|---|
| `main.rs`, `cli/` | clap entry point; one file per command (`setup`, `doctor`, `accounts`, ...); `cli/client.rs` finds the daemon and talks to it |
| `config.rs` | `config.toml`, `POOLTIME_*` overlay, defaults, data-dir `Paths` |
| `server/` | `daemon` (boot), `router` (routes + middleware), `auth`, `inference` (`/v1/messages` and the failover loop), `count_tokens`, `fallback` (cross-provider), `classifier`, `models_route`, `health`, `shutdown` (drain, handoff), `handoff` (a supervised worker's side), `mgmt` (management API), `journal_api`, `analytics_api`, `dashboard` |
| `providers/` | the `Provider` trait; `anthropic/` (passthrough proxy, OAuth refresh, rate-limit headers, usage poll, catalog) and `openai/` (Codex: request/stream translation, OAuth, usage, catalog); `stream_guard` (shared stream pump) |
| `signin/` | OAuth PKCE sign-in flows (`claude`, `openai`), the localhost `callback` listener, paste-code mode |
| `accounts/` | account ids, `account.toml` registry, credentials (0600), identity verification, the runtime `StateBook` (availability per account) |
| `health/` | display states on top of availability (`expiring`, `erroring`), alert events, desktop notifiers |
| `routing/` | weighted account choice, in-flight reservations, sticky sessions, tuning |
| `models/` | model table (`models.toml`), id resolution, family mappings to Codex routes |
| `claude_code/` | Claude Code profiles: discover, connect/disconnect, guarded JSON writes, pins, snapshots, status line wrapper, MCP registration |
| `store/` | SQLite (one writer thread): migrations, request journal, account timeline, usage history, retention |
| `analytics/` | usage sampler and weekly burn report |
| `mcp/` | MCP JSON-RPC transport and the six tools |
| `service/` | launchd and systemd unit rendering and control; `service restart` |
| `supervisor/` | `serve --supervise`: owns the port, runs worker generations, hands over on `SIGHUP`, respawns crashed workers; `proto` (control messages, `run/supervisor.json`) |
| `import/` | `bro.rs`: plan and apply a bro-cli import |
| `upstream.rs` | HTTP client with header-wait, per-read idle and per-attempt deadlines; no decompression |
| `sse.rs` | SSE parser (observer for Anthropic, parser for Codex translation) |
| `fsutil.rs` | atomic writes, guarded read-modify-write, flock |
| `logging.rs` | tracing setup, log pruning, request ids, header redaction, debug mode |
| `errors.rs` | classified failure kinds (never upstream text) |

Outside `src/`: `web/` (dashboard), `harness/` (differential harness against bro; see
[harness report](harness-report.md)), `tests/it/` (integration tests in one binary, in-process
fake upstreams in `tests/support/`), `packaging/homebrew/`, `install.sh`.

## Request flow (`POST /v1/messages`)

1. **Router middleware** assigns a request id (`x-pooltime-request-id`), opens the journal row,
   refuses browser traffic (any `Origin` header, or a non-loopback `Host` on a loopback bind:
   403), and checks the proxy key.
2. **Resolve the route** from the model id (`models/`): an OpenAI route goes to Codex only; an
   unmapped Claude model to Anthropic only; a family mapped to Codex picks a provider by weighted
   ranking with cross-provider fallback (`server/fallback.rs`). Claude Code's safety classifier
   starts on Claude accounts (`routing.classifier_route`).
3. **Per provider, loop** (`server/inference.rs`): use the conversation's sticky account if it is
   usable and in the active priority tier, else `routing::choose` (lowest priority tier, then
   weight × headroom); reserve an in-flight slot; get an access token (single refresh per account,
   flock across processes); call `Provider::attempt`.
4. **Attempt outcome**: `Done` (a committed response is streamed back), `Terminal` (send this
   error to the client), or `Retry` (record the classified reason, bench the account if it was a
   quota/access problem, try the next account). When a provider runs out, a mapped request may hop
   to the other provider.
5. **Streaming**: Anthropic SSE bytes are forwarded unchanged once committed (`message_start`,
   or 64 KiB buffered); Codex streams are translated to Anthropic events with 1 s keep-alive pings
   from a timer that never waits on upstream. A spawned task pumps the body; if the client
   disconnects, the reservation is released by `Drop`, the journal records `client_cancelled`, and
   nothing is held against the account.
6. **Journal**: when the last handle drops, the row (attempts, outcome, timing, tokens) is queued to
   the SQLite writer; never on the request path, never with bodies.

## Background tasks in the daemon

Usage polling and rate-limit tracking, profile sync (every minute), health/alerts evaluation,
status-line cache, usage sampler, session pruning (every minute), retention (hourly). Exactly one
process runs them: under the supervisor, only the active worker generation.

## Processes and restarts

`pooltime serve` alone is one process that binds the port and serves until `SIGTERM`/`SIGINT`,
then drains (in-flight requests finish, up to 10 minutes, while new ones get 503).

The background service runs `pooltime serve --supervise` instead
([design](specs/2026-11-13-seamless-restart.md)). The supervisor binds the port once and runs
`pooltime serve --listen-fd N --control-fd M` workers that inherit the listening socket and an
anonymous `socketpair` (no filesystem path, nothing another user can connect to). Protocol, one
line each: worker `ready <version>` / `released`, supervisor `active` / `handoff`.

1. `service restart` sends the supervisor `SIGHUP` (`launchctl kill SIGHUP`, `systemctl reload`).
2. It starts a new worker from the configured binary path (the Homebrew symlink, so upgrades take
   effect). The worker skips the "already running" preflight and serves at once.
3. On `ready`, the old worker gets `handoff`: it stops accepting, closes its copy of the socket
   (queued connections stay in the shared backlog), stops its background tasks without deleting
   the status cache, answers `released`, serves its open connections for a 2 s grace and drains.
4. On `released` (or after 10 s) the new worker gets `active` and starts the background tasks.

A worker that dies is respawned at once (1, 2, 4, 8 s backoff after repeated failures; after 5,
or if the first worker never starts, the supervisor exits non-zero for launchd/systemd to
restart). A new worker that fails during a restart leaves the old one serving. `SIGTERM` to the
supervisor is forwarded once to every worker; workers ignore `SIGINT`/`SIGHUP`. State for the CLI:
`run/supervisor.json` (0600). The supervisor itself changes only with `service install`.

## Data

`~/.pooltime/`: `config.toml`, `models.toml`, `accounts/<name>/{account.toml,credentials.json,lock}`,
`pooltime.db`, `logs/`, `run/{mgmt.key,mcp.key,status-cache.json,supervisor.json,profiles/}`. See
[Install](install.md#data-directory).

Related: [Adding a provider](adding-a-provider.md), [`AGENTS.md`](../AGENTS.md), [parity](parity.md),
[performance](perf.md).

## `POST /v1/messages/count_tokens`

Claude Code asks this for context accounting. It reuses the `/v1/messages` plumbing but is not an
inference:

- **Claude models** are forwarded to Anthropic's `/v1/messages/count_tokens` through the same
  account loop (`server/inference.rs` `serve_provider`, `Op::CountTokens`) and the same upstream
  request shaping (client `authorization`/`x-api-key` stripped, the account's bearer set,
  `anthropic-version`/`anthropic-beta` passed through as sent). A 401 refreshes and retries once;
  a quota 429 benches the account and fails over; other refusals are replayed.
- **Codex-served models** (an OpenAI model id, or a Claude id mapped to Codex when no Claude
  account can answer) get a local estimate, `{"input_tokens": ceil(request bytes / 4)}` with
  `x-pool-estimate: 1`. OpenAI has no counting endpoint and another tokenizer would not match the
  Claude count anyway; a clean 200 keeps Claude Code from logging a failure every turn.
- **No inference side effects**: no sticky session is read or written, the account records no
  success, token usage or request (so `last used`, counters and the utilization-based burn
  analytics are unaffected), and a plain upstream 4xx is not held against the account.
- **Journal**: the request gets a row (`path`, model, status, outcome, `provider`) so failures are
  visible, but no attempt rows, so per-account attempt counts and error rates stay about real
  inference.
