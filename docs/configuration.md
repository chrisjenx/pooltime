# Configuration reference

pooltime reads `~/.pooltime/config.toml` (mode 0600). A missing file means all defaults; a file that
does not parse or has an out-of-range value is an error. Precedence:

**built-in defaults < `config.toml` < `POOLTIME_*` environment variables**

Durations are strings: `"15m"`, `"2500ms"`, `"1h"`. Restart the daemon after editing
(`pooltime service restart`; a changed `port` or `host` needs `pooltime service install`, since
the service's supervisor keeps the socket it bound). Some values are also written by pooltime itself: `[[profiles]]` by
`connect`, `proxy_key` on first connect, `[routing]` tuning from the dashboard or `import bro`.

## Environment variables

Only the `POOLTIME_` prefix is read; generic `PORT`/`HOST` are ignored.

| Variable | Overrides / does |
|---|---|
| `POOLTIME_HOME` | data directory (default `~/.pooltime`) |
| `POOLTIME_HOST` | `host` |
| `POOLTIME_PORT` | `port` |
| `POOLTIME_PROXY_KEY` | `proxy_key` |
| `POOLTIME_ALERTS_NOTIFY` | `alerts.notify` (`1`/`true`/`on`, `0`/`false`/`off`) |
| `POOLTIME_LOG` | log filter, `tracing` syntax (default `info`) |
| `POOLTIME_NO_BROWSER` | `1`: never launch a browser; print the URL |
| `POOLTIME_NO_VERSION_CHECK` | `1`: `doctor` skips the GitHub release lookup |
| `POOLTIME_RELEASES_API` | where `doctor` looks up the latest release (tests) |
| `POOLTIME_FIXTURE_STATE` | dev only: serve dashboard reads from a JSON fixture ([Dashboard](dashboard.md)) |

The background service sets only `POOLTIME_HOME`; put anything else it needs in `config.toml`.

## Top level

| Key | Default | Meaning |
|---|---|---|
| `host` | `"127.0.0.1"` | IP address to bind (a non-loopback address needs a `proxy_key`, else the daemon refuses to start) |
| `port` | `3460` | port (must not be 0; bro uses 3456, so both can run side by side) |
| `proxy_key` | generated on first `connect` | key Claude Code sends as `ANTHROPIC_AUTH_TOKEN`; the running daemon enforces a key `connect` generates at once, no restart |

`pooltime serve --host/--port` override these for one run.

## `[timeouts]`

| Key | Default | Meaning |
|---|---|---|
| `request_timeout` | `"15m"` | whole-request deadline |
| `anthropic_idle` | `"10m"` | max silence on an Anthropic stream (at least 30 s, at most `request_timeout`) |
| `codex_idle` | `"5m"` | max silence on a Codex stream (at least 30 s) |
| `token_refresh_skew` | `"5m"` | refresh an access token this long before it expires |
| `token_refresh_timeout` | `"30s"` | token refresh request timeout |
| `inflight_wait` | `"90s"` | how long a request waits on an in-flight refresh of the same account |
| `stream_keepalive` | `"1s"` | SSE ping cadence before the first upstream byte (at least 100 ms) |
| `usage_window` | `"5h"` | rolling window of the per-account request counters. |
| `rate_limit_cooldown` | `"1h"` | bench time after a quota 429 with no reset time |
| `access_denied_cooldown` | `"1h"` | how long an access-denied or billing account waits before it is tried again. |
| `overload_retry_max` | `4` | retries on upstream overload (529) |
| `overload_retry_base` | `"500ms"` | first overload backoff (must not exceed the cap) |
| `overload_retry_cap` | `"8s"` | longest overload backoff |
| `session_idle` | `"30m"` | how long a conversation stays pinned to its account (at least 60 s) |

## `[routing]`

| Key | Default | Meaning |
|---|---|---|
| `min_headroom` | `0.1` | headroom (0–1) an account needs to stay in rotation |
| `five_hour_exp` | `1.0` | exponent on the five-hour headroom factor, 0–5 |
| `headroom_taper_start` | `0.2` | five-hour headroom below which an account's share tapers, (0, 1] |
| `anthropic_max_inflight` | `0` | concurrent requests per Claude account (0 = unlimited) |
| `codex_max_inflight` | `4` | concurrent requests per Codex account (0 = unlimited) |
| `classifier_route` | `"anthropic"` | Claude Code's auto-mode safety classifier: `anthropic` keeps it on Claude accounts even when its family is mapped; `default` routes it like any request |

Per-account priority and weight live with the account (`pooltime accounts set`), not here.

## `[retention]`

| Key | Default | Meaning |
|---|---|---|
| `logs_days` | `7` | delete log files older than this |
| `logs_max_mb` | `50` | then delete oldest files until under this |
| `journal_days` | `30` | journal, account timeline and alert records older than this are pruned |
| `journal_max_mb` | `200` | prune oldest journal rows while the database holds more than this |
| `journal_max_requests` | `100000` | ...or the journal holds more requests than this (0 = no row cap) |
| `history_hourly_days` | `30` | usage history: hourly rows kept, then rolled up to daily |
| `history_daily_days` | `365` | daily rows kept, then rolled up to weekly |
| `history_weekly_days` | `730` | weekly rows kept, then deleted |

See [Retention](retention.md).

## `[alerts]`

| Key | Default | Meaning |
|---|---|---|
| `warn_days` | `[3, 1]` | notify this many days before a login expires; the first entry is also the `expiring` window |
| `notify` | `true` | show desktop notifications (events are recorded either way) |

## `[[profiles]]`

Written by `pooltime connect`; one entry per connected Claude Code profile. Do not edit by hand
while connected; use `connect`/`disconnect`.

```toml
[[profiles]]
id = "default"
settings_dir = "/Users/you/.claude"
```

## `[anthropic]` and `[openai]`

Upstream URLs, OAuth client ids, scopes and user agents. They are configurable only because they
drift across Claude Code and Codex CLI releases; leave them alone unless a release note says
otherwise.

| Key | Default |
|---|---|
| `anthropic.api_base` | `"https://api.anthropic.com"` |
| `anthropic.token_url` | `"https://platform.claude.com/v1/oauth/token"` |
| `anthropic.authorize_url` | `"https://claude.com/cai/oauth/authorize"` |
| `anthropic.manual_redirect_url` | `"https://platform.claude.com/oauth/code/callback"` |
| `anthropic.profile_url` | `"https://api.anthropic.com/api/oauth/profile"` |
| `anthropic.client_id` | Claude Code's client id |
| `anthropic.scope` | Claude Code's scope set |
| `anthropic.user_agent` | `"claude-code/2.1.319"` |
| `anthropic.usage_refresh` | `true`: poll usage (Claude and Codex) |
| `anthropic.usage_ttl` | `"2m"`: how long a usage poll stays fresh (Claude and Codex) |
| `anthropic.usage_fetch_timeout` | `"2500ms"`: usage poll timeout, at least 250 ms (Claude and Codex) |
| `openai.api_base` | `"https://chatgpt.com/backend-api/codex"` |
| `openai.usage_url` | `"https://chatgpt.com/backend-api/wham/usage"` |
| `openai.token_url` | `"https://auth.openai.com/oauth/token"` |
| `openai.authorize_url` | `"https://auth.openai.com/oauth/authorize"` |
| `openai.client_id` | the Codex CLI's client id |
| `openai.scope` | `"openid profile email"` (refresh grant) |
| `openai.login_scope` | the Codex CLI's sign-in scopes |
| `openai.originator` | `"codex_cli_rs"` |
| `openai.callback_ports` | `[1455, 1457]`: sign-in redirect ports; only these are registered for the client id |
| `openai.user_agent` | `"codex-cli"` |

## Other files

- `models.toml`: the model table and family mappings (Claude family → Codex route). Edit from the
  dashboard's **Mappings** view; `pooltime models list` shows the result.
- `accounts/<name>/account.toml`: per-account provider, email/identity, priority, weight.

## Example

```toml
port = 3470

[routing]
codex_max_inflight = 2

[retention]
journal_days = 14

[alerts]
warn_days = [5, 1]
```
