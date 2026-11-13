# Install

pooltime is one binary for macOS (arm64, x86_64) and Linux (x86_64, arm64; static musl builds).
There is no Windows build.

> These docs describe pooltime **v1.0.0** or later. v0.1.0 has the proxy, accounts,
> profiles, import, service and dashboard, but not: the `expiring`/`erroring` states and
> `pooltime accounts relogin`, the status line (`pooltime statusline`, `connect --statusline`),
> desktop notifications, the request journal (`pooltime logs --request`, Requests/Events views),
> weekly burn, MCP (`connect --mcp`) and the full `pooltime doctor`. Install v1.0.0 or later for
> those.

## Homebrew (macOS, Linux)

```sh
brew install chrisjenx/tap/pooltime
```

Optional on macOS: `brew install terminal-notifier`, so clicking an alert opens the dashboard
(see [Re-login](relogin.md#notifications)).

## Install script

Downloads the release binary for your platform, checks its sha256 and puts it in `~/.local/bin`.
It never uses sudo.

```sh
curl -fsSL https://raw.githubusercontent.com/chrisjenx/pooltime/main/install.sh | sh
```

| Variable | Default | Meaning |
|---|---|---|
| `POOLTIME_INSTALL_DIR` | `~/.local/bin` | where to put the binary |
| `POOLTIME_VERSION` | latest release | pin a version, e.g. `1.0.0` |
| `POOLTIME_RELEASE_BASE` | GitHub releases | download base URL (mirrors) |

Prebuilt archives are also attached to each [GitHub release](https://github.com/chrisjenx/pooltime/releases).

## From source

Needs a stable Rust toolchain.

```sh
cargo install --locked --git https://github.com/chrisjenx/pooltime pooltime
```

## Check it

```sh run
pooltime --version
pooltime --help
```

## Next

Run the wizard:

```sh
pooltime setup
```

It is safe to re-run. In order it:

1. **Accounts**: does nothing if pooltime already has one; otherwise offers to import from bro
   (when `~/.claude-max-pool` or `~/.bro` exists), else signs in a first Claude account.
2. **Service**: offers to install the background service (launchd on macOS, systemd `--user` on
   Linux) and waits for it to answer.
3. **Profiles**: connects the Claude Code profiles you pick (see [Profiles](profiles.md)).
4. Runs `pooltime doctor` and prints the dashboard URL.

For a scripted, no-questions setup (flags for every prompt), see [`llms.txt`](../llms.txt). The
status line and MCP server are added with `pooltime connect --statusline --mcp` afterwards
(`setup` does not add them yet).

## Background service

```sh
pooltime service install     # install and start (re-running replaces and reloads it)
pooltime service status      # installed? running?
pooltime service restart     # new worker takes new requests; the old one finishes its streams
pooltime service uninstall   # disconnects every Claude Code profile first, then removes it
```

The service runs `pooltime serve --supervise`: a small supervisor that owns the port and runs
the daemon as a worker process. `pooltime service restart` starts a new worker from the installed
binary, which takes new connections within about a second, while the old worker finishes its
in-flight requests (streams included) and exits; the port never closes, so nothing is refused or
answered with 503. A worker that crashes is replaced at once (clients wait on the open port
meanwhile). `pooltime service status` shows the supervisor and the worker generations.

- **Upgrading from 1.0.x:** the service definition changed. Run `pooltime service install` once
  (or `pooltime service restart`, which notices the old definition and reinstalls it). That
  restart still waits for in-flight requests like before; `pooltime doctor` warns until it is
  done.
- The supervisor itself is only replaced by `pooltime service install` (a full restart that
  drains first). It rarely changes; `service restart` says when it is older than the worker.
- A changed `port` or `host` needs `pooltime service install`: the supervisor keeps the socket it
  bound.

- **macOS**: a LaunchAgent at `~/Library/LaunchAgents/dev.pooltime.daemon.plist` (label
  `dev.pooltime.daemon`), loaded into your GUI session. It restarts the daemon if it exits.
- **Linux**: a systemd user unit at `~/.config/systemd/user/pooltime.service` (honours
  `XDG_CONFIG_HOME`), `Restart=on-failure`, `TimeoutStopSec=600`, `KillMode=mixed` (a stop
  reaches the supervisor, which stops its workers), `ExecReload` = the restart signal. A user unit stops when you log
  out; to keep it running (servers, SSH-only machines):

  ```sh
  loginctl enable-linger "$USER"
  ```

Both run `pooltime serve --supervise` with `POOLTIME_HOME` set; when installed by Homebrew they point at the
stable `<prefix>/bin/pooltime` symlink, so `brew upgrade` needs only `pooltime service restart`.
The service does not see your shell's `POOLTIME_PORT`/`POOLTIME_HOST`: put a non-default port in
`config.toml` instead (see [Configuration](configuration.md)).

Without the service, run the daemon in a terminal:

```sh
pooltime serve
```

Stopping it (Ctrl-C / SIGTERM) drains in-flight requests for up to 10 minutes; a second signal
stops it at once.

## Data directory

Everything lives in `~/.pooltime` (override with `POOLTIME_HOME`), mode 0700:

| Path | What |
|---|---|
| `config.toml` | settings ([Configuration](configuration.md)), 0600 |
| `models.toml` | model table and family mappings (edited from the dashboard) |
| `accounts/<name>/` | `account.toml`, `credentials.json` (0600), a lock file |
| `pooltime.db` | SQLite: journal, account timeline, usage history, alert de-dup |
| `logs/` | `pooltime.log.<date>` JSON lines ([Retention](retention.md)) |
| `run/` | `mgmt.key`, `mcp.key`, `status-cache.json`, per-profile snapshots in `profiles/` |

`pooltime doctor` checks the permissions.

## Uninstall

```sh
pooltime service uninstall   # restores every connected profile, removes the service
pooltime disconnect          # (if you ran without the service) restore profiles
brew uninstall pooltime      # or delete the binary from ~/.local/bin
rm -rf ~/.pooltime           # removes accounts and history
```
