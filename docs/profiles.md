# Claude Code profiles

A Claude Code *profile* is a settings directory: `~/.claude` (id `default`) or any `~/.claude-<name>`
(id `claude-<name>`) that contains a `settings.json`. Its state file is `~/.claude.json` for
`~/.claude`, and `<dir>/.claude.json` for every other directory.

Connecting a profile points Claude Code at the pool; disconnecting puts back exactly what was there.

## Connect

`connect` needs a running pool (it checks the pool's `/health` first).

```sh
pooltime connect                    # list the profiles found and their state; changes nothing
pooltime connect default            # ~/.claude
pooltime connect claude-personal    # ~/.claude-personal
pooltime connect --all              # every ~/.claude* profile found
pooltime connect ~/work/claude-dir  # any settings directory, by path (contains a /)
```

A bare word is always a profile id, never a path relative to the current directory. A profile's
id is its directory name without leading dots, so two directories with the same name (say
`/a/.claude-x` and `/b/.claude-x`) cannot both be connected: `connect` refuses the second (`disconnect` still releases either).

Options:

- `--statusline` also wraps the profile's status line with the pool segment (see below).
- `--mcp` also registers the pool's MCP server in the profile's `.claude.json` (see below).

What `connect` writes, in the `env` block of `<profile>/settings.json`:

| Key | Value |
|---|---|
| `ANTHROPIC_BASE_URL` | `http://127.0.0.1:<port>` |
| `ANTHROPIC_AUTH_TOKEN` | the pool's proxy key (random, generated once) |
| `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL` (+ `_NAME`, `_DESCRIPTION`) | the newest Sonnet (1M context) and Haiku in the live Claude catalog |
| `CLAUDE_CODE_MAX_CONTEXT_TOKENS` | the smallest Codex context window, when Codex routes are served |

Pins an earlier bro wrote (including Opus/Fable ones) are recognised and removed; pins you set yourself are left alone.

It also refreshes Claude Code's cached Fable model rows in `.claude.json`. The values it replaces are
saved first in a per-profile snapshot (`~/.pooltime/run/profiles/<id>.json`) and the profile is
recorded in `config.toml`.

Writes are safe against a running Claude Code: a file is only rewritten when its bytes would change,
key order and unknown fields are kept, the write is atomic, and if Claude Code changed the file in
between, pooltime re-reads and retries instead of overwriting it. A symlinked `settings.json` is
written through the link.

## Keeping profiles in sync

While the pool runs it re-derives the pins of every connected profile every minute, so a new Claude
model or a changed port reaches all of them without reconnecting. When the catalog cannot be read
(no usable Claude account, upstream down) the existing pins are kept; only the base URL and key
follow the pool. Force a catalog refresh with:

```sh
pooltime models refresh
pooltime models list        # what Claude Code sees
```

## Disconnect

```sh
pooltime disconnect                  # every connected profile
pooltime disconnect claude-personal  # just this one
```

`disconnect` restores each profile's own values (a key that did not exist is removed again) and
works with the pool down. `pooltime service uninstall` disconnects every profile first: Claude Code
has no fallback for an unreachable base URL, so a profile must never point at a pool that is gone.
`pooltime doctor` and the status line flag "connected, pool down".

## Status line

```sh
pooltime connect default --statusline
```

The profile's `statusLine` becomes `pooltime statusline --wrap default`, which runs your original
status line command (with Claude Code's stdin), then appends the pool segment:

```text
pool 6/8 · 7d 41% left · ⚠ 2 need login
```

It reads a cache file the daemon keeps current (`~/.pooltime/run/status-cache.json`; counts only)
and never makes a network call. A cache older than 150 s prints `pool down`. `disconnect` puts the
original `statusLine` back. To print the segment alone:

```sh run
pooltime statusline
```

## MCP server

```sh
pooltime connect default --mcp
```

Adds `mcpServers.pooltime` (`http://127.0.0.1:<port>/mcp`) to the profile's `.claude.json`, with its
own MCP token (`~/.pooltime/run/mcp.key`), never the management key. Tools: `pool_usage`,
`pool_health`, `start_relogin` (returns the sign-in link), `set_weight` (optionally until a time),
`recent_errors`, `explain_request`. There is no tool to remove accounts, read tokens or change
Claude Code connections. `disconnect` removes the entry.

Related: [Install](install.md), [Debugging](debugging.md).
