# Accounts

An account is one Claude (Max/Pro) or ChatGPT (Codex) subscription sign-in. pooltime keeps its own
credentials in `~/.pooltime/accounts/<name>/` (files mode 0600); it never reads Claude Code's config
dirs or the macOS Keychain at runtime. Account names may use letters, digits, `.`, `_` and `-`
(up to 64 characters).

Most commands talk to the running daemon (`pooltime serve` or the [service](install.md#background-service)).
Sign-in runs inside the daemon when one is running, else in the CLI process.

## Add a Claude account

```sh
pooltime accounts login personal
```

`login` creates the account (as Claude) if it does not exist, opens the browser and waits for the
redirect. Pass `--email you@example.com` to pre-fill the sign-in page; on a machine without a
browser, see [headless sign-in](relogin.md#headless-and-ssh).

Or create first and sign in later:

```sh
pooltime accounts add work --email you@example.com
pooltime accounts login work
```

`--email` on `add` also records the expected identity: the first sign-in must be that account.

## Add a ChatGPT / Codex account

```sh
pooltime accounts add codex-1 --provider openai --login
```

Codex sign-in uses a localhost redirect on port 1455 (or 1457 if 1455 is busy), like the Codex
CLI. To reuse the Codex CLI's existing sign-in instead:

```sh
pooltime accounts add codex-1 --provider openai --from-codex-auth
```

That reads `$CODEX_HOME/auth.json` (default `~/.codex/auth.json`; pass a path to use another).
pooltime then owns the sign-in (its refresh token rotates), so run `codex login` again if you also
want to keep using the Codex CLI.

## Identity check

After every sign-in pooltime asks the provider who signed in and compares it with the account on
file. Signing in as someone else saves nothing and tells you which account the browser is signed
in to. An account with no identity yet (new, or imported dead) shows the email and asks before
binding it; `--yes` binds without asking.

## Look at them

```sh
pooltime accounts list            # state, 7-day usage, priority, weight, email
pooltime accounts list --json
pooltime status                   # one screen: accounts, 7-day usage, alerts
```

Each account is in exactly one state, with the same words in the CLI, dashboard, status line and
MCP, most severe first:

| State | Meaning | Routed? |
|---|---|---|
| `needs_login` | refresh token refused (or never signed in) | no |
| `billing` | provider reports a billing / entitlement problem | no |
| `access_denied` | provider refused access; retried after a cooldown | no |
| `erroring` | 3+ non-quota failures (refresh network/5xx, persistent 401, access denied) in 15 min with no success; a host that does not resolve (DNS) never counts (a lasting one shows in `doctor` and the dashboard banner instead) | yes (display only) |
| `rate_limited` | a quota window is spent; shows which and when it resets | no, until reset |
| `expiring` | usable; recorded login expiry is within `alerts.warn_days[0]` days (3), or has passed but no refresh was refused yet (unconfirmed) | yes |
| `ready` | usable | yes |

An account is only moved to `needs_login` when the provider actually refuses its refresh token.
A passed or unknown expiry date alone never sidelines it.

## Routing knobs

```sh
pooltime accounts set work --priority 50          # lower number = preferred tier
pooltime accounts set work --weight 3             # share within the tier (0.1 to 10, default 1)
pooltime accounts set work --weight 5 --until 2d  # temporary; reverts by itself
pooltime accounts set work --clear-override       # cancel the temporary weight
```

The default priority is 100. Only the lowest-numbered tier with a usable account is used; within it,
accounts are picked by weight and remaining headroom, and a conversation stays on the account it
started on. The same controls are on the dashboard's Accounts view and in MCP (`set_weight`).

## Re-check or remove

```sh
pooltime accounts recheck work    # clear a billing block, refresh if needed, re-poll usage
pooltime accounts remove work     # asks first; --yes to skip
```

## Move the pool to a new machine

`accounts export` writes every account (its saved sign-in, identity, priority, weight and any
temporary weight) plus `config.toml` and `models.toml` to one passphrase-encrypted file;
`accounts import` adds them on the other machine, so you do not have to sign every account in
again. (Not to be confused with `pooltime import bro`, which migrates from bro-cli.)

> **The export file holds live refresh tokens.** Anyone with the file and the passphrase can use
> your subscriptions. Pick a strong passphrase, move the file over a channel you trust, and
> delete it (on both machines) once it is imported.

On the old machine:

```sh
pooltime service uninstall                 # stop the pool first (see below)
pooltime accounts export ~/pool.age        # asks for a passphrase twice (no echo)
```

On the new machine (with or without pooltime running):

```sh
pooltime accounts import ~/pool.age        # asks for the passphrase
rm ~/pool.age
pooltime connect --all                     # point this machine's Claude Code profiles at it
```

**Stop the old pool for good.** Refresh tokens rotate: when one machine refreshes an account's
sign-in, the copy on the other machine stops working (and the other way round), so two pools
sharing accounts log each other out. Stop the old pool before exporting (`pooltime service
uninstall`, or Ctrl-C a foreground `pooltime serve`) so the exported sign-ins are the newest,
and do not start it again; or remove the accounts there with `pooltime accounts remove <name>`
after the import. Export prints this reminder too.

What moves and what does not:

| Moved | Left behind |
|---|---|
| every account's `account.toml` and `credentials.json` | `config.toml`'s `[[profiles]]` (Claude Code profile paths on the old machine): run `pooltime connect` on the new one |
| routing priority, weight, temporary weights, expected identity | `run/` (pid, management key: generated anew) |
| `config.toml` (port, tuning, proxy key, timeouts, retention, alerts) | `logs/` and `pooltime.db` (journal, usage history, rate-limit and error state: rebuilt as the pool runs) |
| `models.toml` (model mappings) | |

Import:

- prints what the file holds (number of accounts per provider and their names), then a line per
  account;
- **skips** an account whose name already exists here, and keeps an existing `config.toml` /
  `models.toml`; `--replace` overwrites them (this machine's `[[profiles]]` are always kept);
- does not restore a sign-in that had already expired (no refresh token left, or its login
  expiry has passed): the account is added and listed with `pooltime accounts login <name>`;
- works whether or not the daemon runs. A running daemon picks the accounts up at once (they are
  created through its management API, like `accounts add`); restart it (`pooltime service
  restart`) if `config.toml` or `models.toml` were restored.

Passphrase: typed at the terminal by default (never echoed; twice for export). For scripts, use
`--passphrase-file PATH` (its first line) or the `POOLTIME_EXPORT_PASSPHRASE` environment
variable; it is never accepted as a command-line argument. An empty passphrase is refused.
`export` writes the file with mode 0600, atomically, and refuses to replace an existing file
without `--force`.

The file is a standard [age](https://age-encryption.org) file encrypted with a passphrase
(scrypt, then ChaCha20-Poly1305), so a wrong passphrase or any change to the file is detected and
nothing is written. `age -d` can decrypt it too if you ever need to look inside.

Next: [Re-login](relogin.md), [Profiles](profiles.md), [Dashboard](dashboard.md).
