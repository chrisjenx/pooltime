# Architecture

One Rust binary. `pooltime serve` is a tokio/axum daemon on `127.0.0.1:3460` that exposes:

| Path | What |
|---|---|
| `POST /v1/messages` | Anthropic Messages API (what Claude Code calls); proxy key required |
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
| `server/` | `daemon` (boot), `router` (routes + middleware), `auth`, `inference` (`/v1/messages` and the failover loop), `fallback` (cross-provider), `classifier`, `models_route`, `health`, `shutdown` (drain), `mgmt` (management API), `journal_api`, `analytics_api`, `dashboard` |
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
| `service/` | launchd and systemd unit rendering and control |
| `import/` | `bro.rs`: plan and apply a bro-cli import |
| `upstream.rs` | HTTP client with header-wait, per-read idle and per-attempt deadlines; no decompression |
| `sse.rs` | SSE parser (observer for Anthropic, parser for Codex translation) |
| `fsutil.rs` | atomic writes, guarded read-modify-write, flock |
| `logging.rs` | tracing setup, log pruning, request ids, header redaction, debug mode |
| `errors.rs` | classified failure kinds (never upstream text) |

Outside `src/`: `web/` (dashboard), `harness/` (differential harness against bro; see
[harness report](harness-report.md)), `tests/` (integration tests with in-process fake upstreams),
`packaging/homebrew/`, `install.sh`.

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
status-line cache, usage sampler, session pruning (every minute), retention (hourly).

## Data

`~/.pooltime/`: `config.toml`, `models.toml`, `accounts/<name>/{account.toml,credentials.json,lock}`,
`pooltime.db`, `logs/`, `run/{mgmt.key,mcp.key,status-cache.json,profiles/}`. See
[Install](install.md#data-directory).

Related: [Adding a provider](adding-a-provider.md), [`AGENTS.md`](../AGENTS.md), [parity](parity.md),
[performance](perf.md).
