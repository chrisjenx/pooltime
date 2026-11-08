# Debugging a failed request

Every request through the pool gets an id, returned in the `x-pooltime-request-id` response header,
and a row in the **request journal**: model, outcome, timing, tokens and every upstream attempt
(account, outcome, classified reason, upstream status and upstream `request-id`). The journal and
logs never hold prompts, responses, tokens or upstream error messages: only the error type and a
classified reason.

## 1. Start with doctor

```sh
pooltime doctor
```

One line per check, `✓` fine, `!` worth a look, `✗` broken (exit 1), `i` information; each problem
comes with the command that fixes it. It checks the service, what answers on the port, each
connected profile (still pointing here, pins current, status line, MCP entry), each account's login
state and expiry, the error rate per account over 24 h, upstream reachability, the latest release,
clock skew, the notifier, file permissions and storage. `--no-version-check` (or
`POOLTIME_NO_VERSION_CHECK=1`) skips the GitHub release lookup.

## 2. Find the request

- **Dashboard → Requests**: recent requests, newest first; tick "Errors only"; click a row for its
  attempt chain. The same view shows per-account error rate and p50/p95 latency over 24 h.
- **Claude Code** with the MCP server registered: ask "why did my last request fail?"
  (`recent_errors`, `explain_request`).
- **Logs**: every log line about a request carries its `request_id`:

  ```sh
  pooltime logs -n 50          # last 50 lines
  pooltime logs --follow       # keep printing
  pooltime logs --json         # raw JSON lines
  ```

## 3. Explain it

```sh
pooltime logs --request req_7kq2mxw4pt3ad5hnc6zb
```

With the daemon running this prints the journal's attempt chain:

```text
req_7kq2mxw4pt3ad5hnc6zb 2026-11-08T10:00:00Z claude-sonnet-5 completed status=200 provider=anthropic ttfb=120ms total=900ms tokens=3/4
  #1 team-1 anthropic failover reason=quota upstream=429 upstream_id=req_u1 40ms
  #2 personal anthropic completed upstream=200 80ms
```

Exit 1 for an unknown id. With `--follow`, or with no daemon, it filters the log files to lines
carrying that id instead.

**Request outcome**: `completed`, `client_cancelled` (you pressed Esc; never counted against an
account), `upstream_truncated`, `idle_timeout`, `reset` or `error`.

**Attempt outcome**: the same words for the attempt that served, or `failover` for one that handed
over to the next account. **Reasons** are classified: `quota`, `status:<code>`, `transport:dns`,
`transport:tls`, `transport:connect`, `transport:reset`, `transport:idle_timeout`, `no_account`,
`unknown_path`, `invalid_request`, and so on.

Response headers worth knowing:

| Header | Meaning |
|---|---|
| `x-pooltime-request-id` | the journal id (every response) |
| `x-pool-account` | the account that served |
| `x-pool-upstream-rejected: 1` | Anthropic refused the request itself (e.g. a header-less 429); passed through, no account benched, no failover |
| `x-pool-route` | the request was routed as Claude Code's auto-mode safety classifier |

## 4. Account history

**Dashboard → Events** lists every account state change with the reason (e.g. `ready → needs_login`,
refresh token rejected). `pooltime status` and `pooltime accounts list` show the current state.

## 5. Debug mode (headers)

When the journal is not enough, turn on debug mode for up to 60 minutes: inbound and upstream
request/response **headers** are logged with the request id. `authorization`, `proxy-authorization`, `x-api-key`, `cookie`,
`set-cookie`, `chatgpt-account-id` and `code=`/`state=`/`k=` query values are redacted. Bodies are never
logged. It lives in memory only, so a restart ends it.

There is no CLI command for it; use the management API with your key (replace `3460` with your
configured port if you changed it):

```sh
key=$(cat ~/.pooltime/run/mgmt.key)
curl -s -X POST -H "Authorization: Bearer $key" -H 'Content-Type: application/json' \
  -d '{"minutes": 15}' http://127.0.0.1:3460/api/v1/debug
# reproduce, then read the lines:
pooltime logs --request <id>
# turn it off early:
curl -s -X DELETE -H "Authorization: Bearer $key" http://127.0.0.1:3460/api/v1/debug
```

## Log verbosity

`POOLTIME_LOG` takes a `tracing` filter (default `info`), e.g. `POOLTIME_LOG=pooltime=debug pooltime serve`.
Logs are JSON lines in `~/.pooltime/logs/pooltime.log.<date>`, rotated daily; see
[Retention](retention.md).

## Without a daemon

```sh run
pooltime logs -n 20
```

Related: [Dashboard](dashboard.md), [Configuration](configuration.md).
