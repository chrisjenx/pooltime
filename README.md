# pooltime

pooltime pools several Claude Max and ChatGPT/Codex subscription accounts behind one local
Anthropic-compatible endpoint (`http://127.0.0.1:3460` by default) for Claude Code. It picks a healthy
account for each request, keeps sessions on the same account, refreshes sign-ins, tracks each account's
usage limits, and edits your Claude Code profiles' settings to point at the pool (and puts them back when
you disconnect). It runs on macOS and Linux and replaces the pool in bro-cli.

> **Terms of service.** Pooling consumer subscription accounts may violate the providers' terms of
> service. Whether and how you use pooltime with your accounts is your responsibility.

## Install

Homebrew (macOS, Linux):

```sh
brew install chrisjenx/tap/pooltime
```

Or the install script, which downloads the latest release binary, checks its sha256 and puts it in
`~/.local/bin` (no sudo):

```sh
curl -fsSL https://raw.githubusercontent.com/chrisjenx/pooltime/main/install.sh | sh
```

Set `POOLTIME_INSTALL_DIR` to install elsewhere, or `POOLTIME_VERSION` (e.g. `0.1.0`) to pin a version.
Prebuilt binaries for macOS (arm64, x86_64) and Linux (x86_64, arm64; static musl) are also attached to each
[GitHub Release](https://github.com/chrisjenx/pooltime/releases).

## Set up

```sh
pooltime setup
```

The wizard is safe to re-run. It imports from bro-cli if it finds it (or signs in your first account),
offers to install the background service (launchd on macOS, systemd `--user` on Linux), connects the Claude
Code profiles you choose, then runs `pooltime doctor` and prints the dashboard URL. `pooltime setup --help`
lists flags for running it non-interactively.

## Migrating from bro

```sh
pooltime import bro --dry-run   # show what would be imported
pooltime import bro             # import accounts, usage markers, sessions, models, tuning, profile snapshots
```

bro's files and Keychain items are never changed. Stop bro (`bro pool down`) before pooltime serves the
imported accounts: both would refresh the same tokens and one copy goes stale. While a bro pool answers on
its port, `pooltime serve` refuses imported accounts unless started with `--i-stopped-bro`.

## Everyday commands

```sh
pooltime status                  # accounts, 7-day headroom, alerts
pooltime accounts list           # accounts and their status
pooltime accounts login <name>   # sign in or re-login an account
pooltime accounts recheck <name> # re-verify an account now (token refresh + usage poll)
pooltime connect --all           # point every Claude Code profile under ~/.claude* at the pool
pooltime connect default         # ...or just the named profiles
pooltime disconnect              # restore the connected profiles' own settings
pooltime doctor                  # check the installation; exits 1 on any failure
pooltime open                    # open the dashboard
pooltime logs                    # daemon logs
pooltime service install         # run the daemon in the background (also: uninstall, status, restart)
```

Data lives in `~/.pooltime` (override with `POOLTIME_HOME`).

## License

[MIT](LICENSE)
