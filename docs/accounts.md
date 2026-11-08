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
pooltime accounts list            # state, 7-day headroom, priority, weight, email
pooltime accounts list --json
pooltime status                   # one screen: accounts, 7-day headroom, alerts
```

Each account is in exactly one state, with the same words in the CLI, dashboard, status line and
MCP, most severe first:

| State | Meaning | Routed? |
|---|---|---|
| `needs_login` | refresh token refused (or never signed in) | no |
| `billing` | provider reports a billing / entitlement problem | no |
| `access_denied` | provider refused access; retried after a cooldown | no |
| `erroring` | 3+ non-quota failures (refresh network/5xx, persistent 401, access denied) in 15 min with no success | yes (display only) |
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

Next: [Re-login](relogin.md), [Profiles](profiles.md), [Dashboard](dashboard.md).
