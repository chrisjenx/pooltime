# Re-login

Subscription sign-ins expire (Claude's refresh tokens last about four weeks) or get revoked. pooltime
tracks each account's recorded login expiry, warns ahead of it and lets you re-sign in from the CLI,
the dashboard or Claude Code (MCP). Re-login uses the same flow as the first sign-in and checks that
the same account signed in again (see [Accounts](accounts.md#identity-check)).

## One account

```sh
pooltime accounts login work
```

The sign-in page is pre-filled with the email on file (`--email` to override).

## Several at once

```sh
pooltime accounts relogin --dead          # every account in needs_login
pooltime accounts relogin --expiring      # logins expiring within alerts.warn_days[0] days (3)
pooltime accounts relogin --expiring 7    # ...within 7 days
pooltime accounts relogin --all           # every account
```

`relogin` walks the accounts one after another (each sign-in finishes before the next starts), so
their expiry clocks line up and you re-login everything in one sitting about once a month.
The dashboard's banner has the same **Re-login all** button; each account row has **Re-login**.

## Headless and SSH

When there is no local browser (`SSH_TTY` is set, or Linux without `DISPLAY`/`WAYLAND_DISPLAY`)
sign-in defaults to **paste** mode; force it anywhere with `--mode paste`.

**Claude**: pooltime prints a URL. Open it in a browser on any machine, sign in, and paste the
`code#state` line the page shows at the `Code:` prompt.

```sh
pooltime accounts login work --mode paste
```

**ChatGPT / Codex**: OpenAI has no "copy this code" page; the browser is always redirected to
`http://localhost:1455/auth/callback` (or `:1457`). Two ways to finish:

- Forward the ports from your laptop, then open the printed URL in the laptop's browser; the
  redirect reaches pooltime on the server and the sign-in finishes on its own:

  ```sh
  ssh -L 1455:127.0.0.1:1455 -L 1457:127.0.0.1:1457 you@server
  ```

- Or, without forwarding: after signing in, the browser lands on a page that fails to load. Copy
  the full address from the address bar and paste it at the `Code:` prompt.

On Linux, keep the service running after you log out with `loginctl enable-linger "$USER"` (see
[Install](install.md#background-service)).

## From Claude Code

With the MCP server registered (`pooltime connect --mcp`), ask Claude Code to re-login an account:
the `start_relogin` tool returns the sign-in link to open.

## Notifications

The daemon raises a desktop notification, once per event (remembered across restarts):

- a login expires within each `alerts.warn_days` threshold (default 3 days, then 1 day);
- an account enters `needs_login`, `billing` or `erroring`.

| Platform | Backend |
|---|---|
| macOS | `terminal-notifier` if installed (click opens the dashboard), else `osascript` |
| Linux | `notify-send` if installed |

Turn them off with `alerts.notify = false` in `config.toml` or `POOLTIME_ALERTS_NOTIFY=0`; the
events are still logged and shown on the dashboard. `pooltime doctor` reports which notifier would be
used. The [status line](profiles.md#status-line) shows the same counts:
`pool 6/8 · 7d 59% used · ⚠ 2 need login`.

## Expiry rules

- The recorded expiry is the earliest the provider has reported. When a provider reports none, the
  expiry is unknown, and an unknown expiry never makes an account `expiring` or `needs_login`.
- At or after the recorded expiry pooltime makes one probe refresh. Only a refusal
  (`invalid_grant`, refresh token expired or reused) moves the account to `needs_login`; a network
  or 5xx failure shows `erroring` and is retried.

## Troubleshooting

**"That looks like Claude's email verification code."** While signing in, claude.ai can email a
magic link. If the link opens in a different tab from the one you started in, Claude shows a short
numeric "verification code" and asks you to enter it where you first tried to sign in. That code
belongs in the Claude sign-in tab (the one where you entered your email), not in pooltime's paste
box or at the `Code:` prompt. Type it there; pooltime finishes on its own after you approve access.
The paste box only wants the `code#state` line from the manual redirect page, or a full redirect
URL. The same message appears in the dashboard, `pooltime accounts login --mode paste` and over the
API.
