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
- The supervisor itself is replaced by `pooltime update` without closing the port (see
  [Upgrading](#upgrading)); a supervisor from before 1.3 only by `pooltime service install` (a
  full restart that drains first). `pooltime doctor` and `service status` show the supervisor's
  and worker's versions.
- A changed `port` or `host` needs `pooltime service install`: the supervisor keeps the socket it
  bound.

- **macOS**: a LaunchAgent at `~/Library/LaunchAgents/dev.pooltime.daemon.plist` (label
  `dev.pooltime.daemon`), loaded into your GUI session. It restarts the daemon if it exits.
- **Linux**: a systemd user unit at `~/.config/systemd/user/pooltime.service` (honours
  `XDG_CONFIG_HOME`), `Restart=on-failure`, `TimeoutStopSec=600`, `KillMode=mixed` (a stop
  reaches the supervisor, which stops its workers), `ExecReload` = the restart signal (sent by `pooltime` itself, so no `/bin/kill` is needed). A user unit stops when you log
  out; to keep it running (servers, SSH-only machines):

  ```sh
  loginctl enable-linger "$USER"
  ```

Both run `pooltime serve --supervise` with `POOLTIME_HOME` set; when installed by Homebrew they point at the
stable `<prefix>/bin/pooltime` symlink, so an upgrade takes effect without reinstalling the service.
The service does not see your shell's `POOLTIME_PORT`/`POOLTIME_HOST`: put a non-default port in
`config.toml` instead (see [Configuration](configuration.md)).

Without the service, run the daemon in a terminal:

```sh
pooltime serve
```

Stopping it (Ctrl-C / SIGTERM) drains in-flight requests for up to 10 minutes; a second signal
stops it at once.

## Upgrading

```sh
pooltime update --check   # installed, latest release and service versions; changes nothing
pooltime update           # asks first; --yes (or -y) to skip the question, required without a terminal
```

`pooltime update` installs the latest [GitHub release](https://github.com/chrisjenx/pooltime/releases)
the way this binary was installed:

| Installed with | `pooltime update` runs |
|---|---|
| Homebrew (`<prefix>/Cellar/pooltime/...`) | `brew upgrade chrisjenx/tap/pooltime` |
| the install script (a `pooltime` binary in a directory you can write, e.g. `~/.local/bin`) | that release's `install.sh` for the same directory: download, sha256 check, atomic replace |
| `cargo install` (`~/.cargo/bin`) | nothing: it prints the `cargo install` command and exits 1 |
| a build under a cargo `target/` directory, or anything else | nothing: it says what to do and exits 1 |

Then, if the background service is running, it moves the service onto the new binary without
closing the port: the supervisor (the process launchd/systemd started) replaces itself in place
with the new binary (an `exec` in the same process, so the pid the service manager tracks stays
the same), keeps the listening socket and its worker, starts a worker from the new binary and
hands over to it as `service restart` does: in-flight requests and streams finish on the old
worker, new ones go to the new one, nothing is refused. It prints the versions before and after.

- Before replacing the supervisor, the new binary is asked whether it can take over (an older
  binary cannot), whether `config.toml` loads and whether the log directory can be written. If not, or if anything else goes wrong, the
  running service keeps serving as it was and `pooltime update` prints why and exits 1.
- It only signals the supervisor that runs right now: the state file is read again and its pid
  must still be a `pooltime serve --supervise` process. If the service runs a different binary
  than the one updated (installed from another place), it says so, names both paths and exits 1.
- Already on the latest release (say, after a manual `brew upgrade`)? `pooltime update` still
  moves a service that runs an older version onto the installed binary.
- A supervisor from before 1.3 cannot replace itself: `pooltime update` gives it new workers
  (`service restart`) and tells you to run `pooltime service install` once.
- No service running: only the binary is replaced.
- The release check uses GitHub's API (`POOLTIME_RELEASES_API` points it elsewhere, as for
  `doctor`); the install script honours `POOLTIME_RELEASE_BASE` (mirrors).

The same in-place replacement can be triggered by hand with `kill -USR2 <supervisor pid>` (the pid
is in `pooltime service status`); `SIGHUP` (`service restart`) keeps replacing only the workers.

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
