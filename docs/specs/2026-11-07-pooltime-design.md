# pooltime — design

Date: 2026-11-07 · Owner: Chris Jenkins (`chrisjenx`) · Status: draft for approval

Companion: [`2026-11-07-pooltime-parity-inventory.md`](2026-11-07-pooltime-parity-inventory.md) — the full
behavior inventory of today's bro-cli pool. It is the "don't regress" checklist and is part of this spec.

**§16 (binding implementation rules from the Fable review) overrides any earlier section it conflicts with.**

---

## 1. What this is, and why

**pooltime** is a new, public, from-scratch Rust replacement for the `bro` CLI and its Bun pool server. It pools
several LLM subscription accounts (Claude Max today, ChatGPT/Codex too, others later) behind one local
Anthropic-compatible endpoint that Claude Code talks to. It lives in a new repo, `github.com/chrisjenx/pooltime`
(never PR'd against upstream `JustSuperHuman/bro-cli`).

Why now — what actually hurts today (all verified on the live machine on 2026-11-07):

- **Accounts die silently.** Two of eight accounts (`acct-a`, `acct-d`) had been dead for
  6 and 16 days; the pool log contains zero lines about it. The dashboard files them under a vague "Sidelined".
- **Re-login every few weeks.** Root cause found: Anthropic issues each Claude OAuth refresh token with a hard
  ~27–30-day lifetime from sign-in (`refreshTokenExpiresAt` in the credentials file; Claude Code itself treats it as
  absolute: on refresh it stores `min(old, new)`). Refreshing never extends it. Accounts logged in on different days
  die one at a time. This cannot be engineered away with normal logins; it can be made visible, predictable, and
  near-effortless.
- **Errors are swallowed.** Failover lines don't say why; ~700 connection errors in the log carry no time, account,
  or request; usage-check failures say "see logs" while logging nothing.
- **Account states unclear in the UI.**
- bro needs Node + Bun; the user wants something faster, lighter, and easier to install and live with.

What the user checks day to day: **how much 7-day allowance is left across subscriptions** — because pricing and
workload changes mean they might burn 10 subs' worth one week and 2 the next, and they add/cancel subscriptions
accordingly.

How the user works: they start the pool (`bro start` today) and then run `claude` agents separately, in multiple
Claude Code profiles. No launcher wanted.

### Success looks like

1. The user never discovers a dead account by accident: they're warned days ahead, and re-login is one click per
   account.
2. One glance answers "how many subs' worth am I burning per week, and do I need more or fewer?"
3. When a request fails, the user (or Claude via MCP) can see exactly what happened, per attempt.
4. A newcomer can install and set it up from the README — or by pasting one prompt into their coding agent.
5. Every bro behavior is either ported with a test or consciously dropped, proven by a parity checklist plus a
   side-by-side harness against the old pool; pooltime is measurably lighter (memory, startup, added latency).

### Goals
- Single static-ish Rust binary; macOS (arm64, x86_64) and Linux (x86_64, arm64).
- Full Anthropic + Codex parity with bro's direct OAuth path (see inventory §1–§6).
- Provider plug-in architecture so Grok etc. can be added as one module.
- Clear account health, expiry countdowns, proactive alerts, seamless re-login.
- Weekly "subs burned" analytics with history.
- First-class debugging: request journal, account timeline, structured logs, `doctor`.
- Management from web dashboard, MCP, and CLI through one shared management layer.
- Bounded disk use for everything it stores.
- Super-clear human docs and LLM-friendly setup docs.

### Non-goals
- Windows.
- A `claude` launcher (no `pooltime` wrapper around `claude`; no permission-mode handling).
- The legacy CLI-subprocess backend and the OpenAI-format `/v1/chat/completions` endpoint (it only worked through
  that legacy path). The architecture should allow an OpenAI-format *inbound* adapter later.
- Alternative routing strategies (`expiring`, `headroom`) — only the default weighted strategy is ported.
- Long-lived (`setup-token`-style, 1-year, inference-only) tokens — **parked**, see §13.
- Any telemetry leaving the machine.

### Anti-goals (would be failure even if it "works")
- An account dies and nothing tells the user.
- Prompts, responses, or tokens written to logs or the journal.
- Unbounded disk growth.
- Regressing a hard-won streaming/failover quirk (inventory) — e.g. benching an account on a header-less 429,
  erroring a response stream after client disconnect, pull-driven keep-alives, altering Anthropic SSE bytes.
- Claude Code left pointing at a dead endpoint.
- Using browser profiles per account (user found this too aggressive).

---

## 2. Decisions made with the user

| Topic | Decision |
|---|---|
| Name | **pooltime** — repo, binary, command. Repo `github.com/chrisjenx/pooltime`. |
| Language | Rust, rewrite from scratch (not a line-by-line port); improve as we go. |
| Audience | **Public**; others must be able to install it. |
| Platforms | macOS + Linux. Windows explicitly not wanted. |
| Providers | Carry over Codex/ChatGPT at full parity; design for future providers (Grok…). |
| Launch model | **No launcher.** Pool runs as a background service; user runs plain `claude`. |
| Startup | Start at login, as a user choice. |
| Claude Code profiles | Manage an explicit list. On this machine: `~/.claude-personal` and `~/.claude` (default) connected; `~/.claude-work` left off. |
| Alerts | **A** dashboard banner + clear states/countdowns, **B** Claude Code status line segment, **C** macOS notification. (Not terminal/MCP push, not webhook.) |
| Re-login | Pool runs Claude's OAuth sign-in itself with `login_hint=<account email>`; verifies the returned identity. **No per-account browser profiles.** |
| Long-lived tokens | Not now; needs proof of usage-endpoint access and ToS acceptability first. |
| Main dashboard view | 7-day usage left per subscription + pool total, leading to "subs burned per week". |
| MCP scope | Check usage; see which accounts need re-login (and start re-login); tweak priority/weight (e.g. burn an account sooner). |
| Debugging | Builder adds what's needed (proposal accepted: journal, timeline, structured logs, doctor). |
| Retention | Nothing grows forever; everything capped/pruned. |
| Validation | Parity checklist + side-by-side harness + live shadow run + perf numbers (proposal accepted). |
| Cutover milestone | **Milestone 1 = full working pooling proxy at parity with bro, including Claude Code settings management and the dashboard UI.** The user switches off bro after M1 passes validation (§11). The extra improvements come in follow-up milestones (§14). |
| Distribution | `curl \| sh` install script and/or Homebrew. **No signed/notarized macOS app**, no Apple publishing (user has a Developer ID but doesn't want to use it for this). |

---

## 3. User experience

### Install & setup
- Install: GitHub Release prebuilt binaries; Homebrew tap (`brew install chrisjenx/tap/pooltime`); `curl | sh`
  installer; `cargo install pooltime`.
- `pooltime setup` — interactive, idempotent wizard:
  1. Import from bro if `~/.claude-max-pool` / `~/.bro` exist (offer), else add first account (sign-in).
  2. Offer to install the background service (launchd LaunchAgent on macOS, systemd `--user` unit on Linux).
  3. Choose Claude Code profiles to connect (auto-detect `~/.claude*` dirs containing `settings.json`, plus
     `$CLAUDE_CONFIG_DIR`); show current state of each.
  4. Offer to add the status line segment and register the MCP server in the chosen profiles.
  5. Run `doctor` and print the dashboard URL.

### CLI surface (proposed; exact naming is the builder's within this spirit)
```
pooltime setup                         # wizard (above)
pooltime serve                         # run in foreground (what the service runs)
pooltime service install|uninstall|status|restart
pooltime status                        # one-screen summary: accounts, 7d left, alerts
pooltime accounts list
pooltime accounts add <name> [--provider anthropic|openai] [--email you@x]
pooltime accounts login <name>         # re-login (same flow), uses login_hint
pooltime accounts relogin --expiring [<days>] | --dead | --all   # walks them in sequence
pooltime accounts remove <name>
pooltime accounts set <name> --priority N --weight W [--until <time>]
pooltime connect [<profile>...]        # point Claude Code profiles at the pool
pooltime disconnect [<profile>...]     # restore each profile's prior settings
pooltime models list|refresh
pooltime statusline                    # prints the status line segment (for Claude Code statusLine)
pooltime doctor
pooltime import bro                    # migrate from bro-cli state
pooltime logs [--follow] [--request <id>]
```

### Daily life
- Service is running; user runs `claude` in any connected profile; nothing else to remember.
- Status line shows e.g. `pool 6/8 · 7d 41% left · ⚠ 2 need login`.
- Dashboard (`http://127.0.0.1:<port>/`) leads with the 7-day picture.

---

## 4. Account health, alerts, re-login

### States (exactly one per account; same words everywhere: dashboard, CLI, MCP, status line)
| State | Meaning |
|---|---|
| **Ready** | usable |
| **Expiring** | usable, but login expires within the warn window (default 3 days); shows countdown |
| **Needs login** | refresh token dead/expired, or never logged in |
| **Rate-limited** | quota window spent; shows reset time and which window |
| **Billing** | entitlement/billing problem reported by provider |
| **Erroring** | repeated non-quota failures (e.g. refresh 5xx/network, persistent 401, access denied) above a threshold; shows last error class and count |
| **Disabled** | user paused it (optional; builder may include) |

Each state shows **since when** and **why** (the classified reason). Model-scoped exhaustion (e.g. Fable 7d spent)
is shown as a badge, not an account state. Dead accounts never display stale usage as healthy headroom.

### Expiry awareness
- Read `refreshTokenExpiresAt` (Anthropic) at import/login, and track `refresh_token_expires_in` from token
  responses using Claude Code's rule (stored expiry = `min(old, new)`; default 30 days if absent).
- Mark an account **Needs login** at its expiry time proactively, not on the next failed refresh.
- Codex: use whatever expiry signal exists; otherwise detect death by `invalid_grant` / `refresh_token_reused` on
  refresh (bro lacks this).

### Alerts
- **Dashboard**: banner ("2 accounts need login · 1 expires today [Re-login all]"); broken/expiring accounts sort
  first; per-account Re-login button.
- **Status line**: `pooltime statusline` prints a compact segment; setup **composes** it with the user's existing
  status line command rather than replacing it (these profiles already have status lines).
- **macOS notification**: at warn thresholds (default 3 days and 1 day before expiry) and when an account enters
  Needs login / Billing / Erroring. Each event notifies **once** (de-duplicated and persisted across restarts).
  No signed app bundle (user decision), so: if `terminal-notifier` is installed, use it so clicking opens the
  re-login page (Homebrew formula lists it as an optional/recommended dependency); otherwise fall back to
  `osascript display notification` with text that says where to go ("open pooltime → Re-login"). *(My call.)*
  Linux: freedesktop notification if available (nice-to-have).

### Re-login flow
- pooltime runs the Claude.ai OAuth PKCE flow itself (no `claude` subprocess, no Keychain dance): authorize URL
  `https://claude.com/cai/oauth/authorize` with `code=true`, client id, `response_type=code`, redirect to a
  localhost callback, Claude Code's scope set, PKCE S256, state, and **`login_hint=<account email>`**; token exchange
  at `https://platform.claude.com/v1/oauth/token` (JSON). Exact parameter set/values to be lifted from current
  Claude Code (verified present in Claude Code 2.1.319's `sEo` authorize-URL builder: `orgUUID`, `login_hint`,
  `login_method` params exist).
- The account's email is captured at first login/import (via the OAuth profile endpoint or token response) and
  stored with the account.
- After exchange, pooltime **verifies the identity** (email/org) matches the account; on mismatch it refuses to save,
  explains ("you're signed in to claude.ai as X; sign out and sign in as Y"), and offers to retry.
- `relogin --expiring/--dead/--all` (and dashboard "Re-login all") walks accounts one after another so all clocks
  line up into one session per ~4 weeks.
- Re-login is reachable from CLI, dashboard button, notification click, and MCP (MCP returns the link to click).
- Codex re-login: existing browser PKCE flow (ports 1455→1457), same identity verification by account id.
- No local browser (headless Linux / SSH) *(my call — desktop-first, but public users will hit this)*: Claude
  sign-in offers the manual "paste the code" path (Claude's `code=true` manual redirect page); docs explain SSH
  port-forwarding for Codex's fixed callback ports 1455/1457; systemd unit docs mention `loginctl enable-linger`;
  `llms.txt` covers the non-browser path. The provider sign-in contract must support both callback and
  paste-code modes.
- Unknown to verify early (spike in the plan): whether claude.ai honors `login_hint` when already signed in as a
  different user. If not, the mismatch guidance above is the fallback.

### Credential hardening
- pooltime owns its credential store (its own dir, files mode 0600); no dependency on Claude Code config dirs or the
  macOS Keychain at runtime. (Import still reads bro's files/Keychain items.)
- Atomic writes (temp + fsync + rename) for every state file.
- Single-writer guarantee: one refresh in flight per account, and a cross-process lock so two pooltime processes
  can't refresh the same account.
- If a refresh times out, re-read stored credentials before retrying (the server may have rotated).
- Every refresh attempt logged with a classified outcome (`invalid_grant` vs network vs 5xx vs other).

---

## 5. Usage analytics ("subs burned per week")

- Persist time-series of each account's window utilization (from response headers and the usage poll) and
  request/token counters.
- Headline: **subscription-equivalents used per week** = sum over accounts of 7-day utilization consumed in that
  week (each account counts as 1 sub by default; if plan is known — e.g. Max 5x vs 20x — the builder may show plan
  alongside but keep "1 account = 1 sub" as the default unit unless it's clearly better).
- Dashboard: current 7-day left per account + pool total; resets; weekly burn chart (≥8 weeks); forecast for the
  current week ("on pace for ~6.2 of 8 subs"); idle accounts highlighted ("2 subs mostly idle this week").
- Same numbers via MCP and `pooltime status`.

---

## 6. Debugging & observability

- **Request ID** for every inbound request; returned as a response header (e.g. `x-pooltime-request-id`).
- **Request journal** (local DB): time, model, provider, each attempt (account, outcome, classified reason,
  upstream status, upstream `request-id`), failover/cross-provider hops, TTFB, total duration, tokens, stream end
  (completed / client cancelled / upstream truncated / idle timeout / reset). **No bodies, no prompts, no tokens.**
- **Account timeline**: every state transition with time and reason.
- **Structured logs** via `tracing`: timestamp, level, request id, account, provider; human-readable on a TTY,
  JSON lines in files; rotated by size and age. Optional time-boxed debug mode that captures request/response
  *headers* (secrets redacted) — never bodies.
- **Error classification everywhere**: DNS, TLS, connect, reset, idle timeout, deadline vs upstream 4xx/5xx
  classes. No swallowed errors; every catch either handles, classifies+records, or propagates. (Inventory §10 lists
  the specific bro gaps to close.)
- **Dashboard**: "Recent requests" (filter errors; click → attempt chain), events timeline, per-account error rate
  and latency percentiles, storage used.
- **MCP**: "why did my last request fail", "connection errors today".
- **`pooltime doctor`**: service running, port/health, each connected profile points at the pool and pins are
  current, each account's login state and expiry, upstream reachability, clock skew, disk usage, version.

### Retention (all configurable; defaults are the builder's within these bounds)
- Log files: rotate by size and age (e.g. 7 days, fixed MB cap), delete old.
- Request journal: 30 days, plus a size ceiling; auto-prune.
- Usage history: downsample with age — hourly for 30 days, daily for 1 year, weekly totals for 2 years — then drop.
- DB compacts after pruning. Disk usage visible in dashboard and `doctor`.

---

## 7. Management surfaces

One internal management layer; three front-ends that can never disagree.

- **Dashboard** (web, embedded in the binary; plain HTML/JS, no Node build): overview (7-day picture, alerts),
  accounts (states, countdowns, re-login, priority/weight), routing/mappings/tuning, requests & events, settings.
  Must make every account state unambiguous at a glance.
- **MCP** (served by the pool itself over HTTP; `setup` registers it in connected profiles). Tools:
  - usage summary (per account + pool, weekly burn, forecast)
  - account health (states, since, why, expiry countdowns)
  - start re-login for an account → returns the sign-in link
  - set priority/weight for an account, optionally with an expiry ("burn harder until Friday") that auto-reverts
  - recent errors / explain a request by id
  - *Not exposed*: removing accounts, reading tokens, changing Claude Code connection.
- **CLI**: §3.
- **Security**: bind 127.0.0.1 by default; management API + MCP require a local key (generated at setup, stored
  0600, used automatically by CLI/dashboard/registered MCP); inference endpoint keeps optional proxy key.
  Timed weighting overrides auto-revert and are visible.

---

## 8. Claude Code integration

- `connect`/`disconnect` per profile, managing the `env` block of `<profile>/settings.json` exactly as inventory §6
  describes (per-profile snapshot, backfill, never snapshot own pins, restore on disconnect, skip unchanged writes),
  but with **atomic writes**.
- Catalog-derived Sonnet/Haiku pins, Fable cache-row rewrite in `<profile>/.claude.json`, max-context key, legacy
  pin scrubbing — ported as-is (inventory §6), including "unreachable catalog → change nothing".
- **All connected profiles are kept in sync** whenever the catalog changes (bro only refreshed the profile it was
  run from; on this machine `~/.claude` was stale since Oct 14).
- Claude Code has no fallback for an unreachable base URL: profiles are only left connected while the service is
  installed; `service uninstall` disconnects all profiles; `doctor` and the status line flag "connected but pool
  down".
- Status line segment and MCP registration are added per connected profile, composing with existing config.
- Import hands over bro's existing override snapshots (`~/.bro/pool-settings*.json`) so `disconnect` still restores
  the user's original pre-bro values.

---

## 9. Architecture (constraints and intent; internals are the builder's)

- Rust, async (tokio), a mature HTTP server/client stack. Single binary embeds dashboard assets.
- **Provider trait** — each provider implements: sign-in (PKCE flow, identity), token refresh + death detection,
  request send (non-stream + stream), protocol translation to/from the Anthropic wire format (identity for
  Anthropic), rate-limit/usage signal parsing (headers + poll), error classification, model catalog. Routing,
  health, alerts, analytics, journal, dashboard, MCP must not contain provider-specific branches beyond data.
- **Inbound** is Anthropic `/v1/messages` + `/v1/models` (+ health). Inbound format as a seam so an OpenAI-format
  front door can be added later.
- Streaming invariants (inventory §1, §4): Anthropic SSE bytes forwarded unchanged post-commit; pre-commit buffer
  64 KiB; keep-alives (Codex) driven by a timer independent of upstream reads; terminal event closes immediately;
  client disconnect never surfaces as an error or crash; idle watchdogs instead of blunt fetch timeouts.
- State: one SQLite DB for journal, timeline, usage history, alert de-dup; small files (or DB) for account creds
  and config. All writes crash-safe.
- Config: one TOML config file with documented defaults; env overrides (`POOLTIME_*`). Default port: builder's call
  (avoid colliding with bro's 3456 so both can run during the shadow run).
- Graceful drain on SIGTERM (up to 10 min, second signal forces); `/health` proves identity before anyone trusts or
  kills a listener.

---

## 10. Migration from bro

`pooltime import bro` (also offered by `setup`):
- Accounts from `~/.claude-max-pool/accounts/*`: Anthropic creds from `.credentials.json` or the macOS Keychain item
  `Claude Code-credentials-<sha256(dir)[:8]>`; `refreshTokenExpiresAt`; Codex `openai-auth.json`; `routing.json`
  priority/weight.
- `usage.json` markers/snapshots (so dead accounts stay dead), `models.json` routes + mappings, `tuning.json`.
- `~/.bro/pool-settings*.json` override snapshots → pooltime's per-profile snapshots.
- Import is read-only on bro's files. Note the rotation hazard: once pooltime refreshes an imported account, bro's
  copy is stale — so import should make clear bro must be stopped (`bro pool down`) before pooltime serves those
  accounts, except during the shadow run (§11), where the two must not share accounts *or* must accept that only
  one refreshes (builder decides; simplest: shadow run uses re-logged or a subset of accounts).

---

## 11. Validation (proving no regression)

1. **Parity checklist** `docs/parity.md` in the pooltime repo: every item in the inventory and every bro test
   (~650) mapped to a pooltime test or marked dropped-with-reason.
2. **Differential harness** (bro hard-codes some Codex URLs in `pool/src/upstream/codex-constants.ts` /
   `codex-models.ts`; adding env overrides to the user's bro fork so it can point at the fake upstream is fine):
   a fake Anthropic + Codex upstream that replays scripted scenarios (quota 429, header-less
   429 refusal, overload with/without retry-after, 403 billing vs generic, invalid_grant, mid-stream cut, client
   disconnect, Codex reasoning round-trip, oversized `response.created`, silent upstream needing keep-alives,
   cross-provider fallback…) against both bro and pooltime, diffing what the client receives (byte-for-byte where
   passthrough is required) and the resulting account state.
3. **Live shadow run**: pooltime on a different port with real accounts; a few real Claude Code sessions pointed at
   it; compare journals/behavior before cutover.
4. **Performance**: memory at idle and under load, startup time, added latency (TTFB overhead) — bro vs pooltime,
   numbers recorded in the repo.

---

## 12. Documentation

- README: what/why in plain words, 60-second quickstart, dashboard screenshot, ToS note (pooling subscription
  accounts is the user's responsibility under each provider's terms), "paste this into your coding agent to set me
  up" prompt.
- `docs/`: short task-focused pages — install, add account, re-login, read the dashboard, connect Claude Code
  profiles, debug a failed request, configuration reference, retention, migrating from bro, adding a provider
  (contributors), architecture overview.
- LLM-friendly: `AGENTS.md` (build/test/architecture for coding agents working on the repo) and `llms.txt` with
  exact, copy-pasteable, non-interactive setup steps (flags for every wizard prompt).
- License: MIT (builder's default; user may change).
- CI: build + test on macOS and Linux for every commit; release workflow producing binaries + Homebrew formula.

---

## 13. Parked / later

- **Long-lived tokens** (Claude Code `setup-token` style: 1-year TTL `31536000`, scope `user:inference` only).
  Would reduce re-login to ~yearly. Blockers: probably can't read `/api/oauth/usage` (needs `user:profile`) → usage
  only from response headers; ToS acceptability unverified. Revisit after a test.
- OpenAI-format inbound endpoint.
- Grok and other providers.
- Webhook/phone push alerts.
- Linux desktop notifications if not done in v1.

---

## 14. Milestones

**M1 — parity & cutover (user switches off bro after this).** Full working pool at parity with bro:
- Anthropic + Codex proxying, routing, failover, streaming invariants, usage/rate-limit tracking, model table,
  mappings, classifier, catalog discovery (inventory §1–§5), on the provider-trait architecture (§9) from day one.
- Account management: add/login/import/remove/priority/weight. pooltime's own sign-in flows are needed here anyway
  (it no longer shells out to `claude`); `login_hint` + identity check are cheap to include.
- Claude Code settings management for multiple profiles (§8), including keeping all connected profiles in sync.
- Dashboard UI at least at bro's level (overview, routing, settings, account detail).
- Background service install, `doctor` basics, `import bro`.
- Correctness hardening that is part of "working" rather than an extra: atomic writes, no swallowed errors,
  structured logs with request IDs, bounded log rotation, Codex dead-login detection.
- Validation gate (§11): parity checklist complete, differential harness green, shadow run done, perf numbers.
- Minimal README + install script/Homebrew so it can be installed the way users will install it.

**Follow-ups (after cutover), roughly in order of the user's pain:**
1. Account health states, expiry countdowns, proactive Needs-login, dashboard banner, `relogin --expiring/--all`.
2. Alerts: status line segment, macOS notifications.
3. Request journal + account timeline + debugging views + `doctor` full + retention/downsampling.
4. Weekly "subs burned" analytics, history, forecast.
5. MCP server.
6. Full docs set, `AGENTS.md`, `llms.txt`, agent-setup prompt; release polish.
7. Parked items (§13) if pursued.

The M1 design must not paint these into a corner (e.g. M1's DB/state layout and event plumbing should anticipate
the journal, timeline, and history).

---

## 15. Left to the builder

Crate choices; internal module layout; DB schema; exact CLI flag names and dashboard layout within the intent
above; default port; exact retention numbers within the stated shape; how "Erroring" thresholds are computed;
whether "Disabled" state ships in v1; how the shadow run avoids double-refreshing shared accounts; Homebrew tap
mechanics; dashboard visual design (must keep states unambiguous and the 7-day picture first).

### Calls I made (not explicitly decided by the user — check these)
- MIT license; Homebrew tap + curl installer + cargo.
- Notifications via `terminal-notifier` when present (clickable), `osascript` fallback (not clickable).
- Headless/SSH sign-in fallback (paste-code for Claude; documented port-forward for Codex).
- What goes in M1 vs follow-ups beyond "proxy parity + settings + UI" (§14) — e.g. I put structured logging,
  atomic writes, and Codex dead-login detection in M1 as correctness; health states/alerts/analytics/MCP after.
- Drop `/v1/chat/completions`, legacy CLI backend, non-default routing strategies, the launcher.
- Timed (auto-reverting) weighting overrides via MCP/CLI.
- MCP can't remove accounts, read tokens, or change Claude Code connection.
- Management API/MCP require a local key; bind 127.0.0.1.
- Retention defaults (7d logs, 30d journal, hourly→daily→weekly, 2y weekly totals).
- Notifications fire once per event; thresholds 3 days and 1 day.
- "1 account = 1 sub" as the analytics unit.
- Profiles are kept connected only while the service is installed.
- SQLite for history/journal; TOML config; `POOLTIME_*` env vars.
- pooltime stores its own credentials rather than using the macOS Keychain.

---

## 16. Binding implementation rules (from the Fable review, 2026-11-07)

These override earlier sections where they conflict. User-decided scope (M1 = proxy parity + settings + dashboard;
everything else eventually built) is unchanged; the review's suggested scope cuts were **not** adopted except where
noted.

### Sequencing inside M1
- **Spikes first (before any proxy code depends on them):** (S1) capture Claude Code's real authorize URL/params and
  which authorize host (`claude.com/cai/oauth/authorize` vs `platform.claude.com/oauth/authorize`) is used for
  claude.ai subscriptions, plus the manual redirect (`platform.claude.com/oauth/code/callback`); test whether
  `login_hint` selects/pre-fills the account. (S2) check whether Anthropic rotates refresh tokens on refresh
  (compare stored refresh token before/after on one account). Record results in `docs/spikes.md`; adjust only the
  affected tasks.
- **M1a** Anthropic proxy + accounts + settings/profiles + import + dashboard (Claude-only cutover possible).
  **M1b** Codex translation/catalog/login. **M1c** differential harness + perf numbers + shadow run → cutover.
- The dashboard's **7-day-left per account + pool total** (current snapshots only) is in M1; history/forecast later.
- Parity doc maps **by inventory item**, citing the bro test names that cover it (not 650 separate rows).

### Profiles & Claude Code files
- A profile is `{settings_dir, claude_json_path}`: `claude_json_path = ~/.claude.json` when
  `settings_dir == ~/.claude`, else `<settings_dir>/.claude.json`. Profiles are stored in pooltime config (the
  service has no shell env; never rely on `$CLAUDE_CONFIG_DIR` at runtime).
- Writes to `settings.json` / `.claude.json` (both are rewritten by running Claude Code): only if the target bytes
  differ; parse → patch → serialize fully before opening the temp file; re-check mtime+inode immediately before
  rename and retry (bounded) if changed; temp+fsync+rename in the same dir. Preserve key order and unknown fields.
- `connect` requires a healthy pool whose `/health` passes the identity check (not an installed service).
  `service uninstall` disconnects all profiles. `doctor`/status line flag "connected, pool down". A port change
  re-syncs all connected profiles.
- Inference auth: generate a random proxy key at setup and write it as `ANTHROPIC_AUTH_TOKEN` into connected
  profiles (no shared placeholder).
- Status line composition: pooltime stores the profile's original `statusLine` in its per-profile snapshot and
  installs a wrapper command (`pooltime statusline --wrap <profile-id>`) that passes Claude Code's stdin JSON to the
  original command, appends pooltime's segment, completes within ~150 ms using cached state (never a network call
  to upstream), and on timeout/failure prints the original output alone. `disconnect` restores the original.
- MCP registration (follow-up) goes in the profile's `.claude.json` `mcpServers` (same write rules) with a
  narrowly-scoped MCP token (read + start-relogin + set weight), never the full management key.

### Accounts, OAuth, identity
- **Canonical utilization unit is a 0–1 fraction everywhere internally.** Every provider parser normalizes
  (Anthropic headers already 0–1; `/api/oauth/usage` body and Codex `used-percent` are 0–100 → ÷100).
- M1 account status enum: `ready | needs_login | rate_limited | billing | access_denied` (+ model-scoped badges).
  The §4 richer states (`expiring`, `erroring`, `disabled`) extend it later. Never a catch-all "sidelined".
- `Expiring` is clock-driven from `refreshTokenExpiresAt`. Transition to `needs_login` **only on confirmation**: at/
  after expiry run one probe refresh; `invalid_grant` / "refresh token expired" → `needs_login`; network/5xx →
  `erroring` (or keep current state in M1). Stored expiry rule = `min(old, new)`; if the server sends no
  `refresh_token_expires_in` and there is no old value, the expiry is **unknown** (a 30-day guess may be displayed
  as "≈", never used to sideline).
- Refresh failure classes include `invalid_grant`, `refresh_token_reused` (Codex), "account on hold"
  (`oauth_refresh_account_on_hold` → billing), network, 5xx, other. Refresh timeout: 30 s (Claude Code's value).
- Identity: at import/login, `GET {api_base}/api/oauth/profile` (Bearer, Claude Code UA, `Cache-Control: no-cache`)
  → store `account.uuid`, `account.email`, `organization.uuid`. Match on `account.uuid` (email for display and
  `login_hint`). Accounts with no baseline (e.g. dead at import): first login shows the email and asks to bind;
  verification enforced afterwards.
- **All sign-in flows run inside the daemon** via the management API (the daemon opens the browser with
  `open`/`xdg-open`); `pooltime accounts login` is a thin client; in-process only when no daemon is running.
  Callback listener: 127.0.0.1, validates `state`, one-shot, 5-min expiry. Headless (`SSH_TTY` set or no
  `DISPLAY` on Linux) → print URL + paste-code path.
- Cross-process lock: `flock` on a per-account lock file around refresh and credential writes.

### Proxy / streaming
- Forward the **original request bytes** when no Codex-thinking block needs stripping; re-serialize only when
  something was removed (serde_json with `preserve_order` + `arbitrary_precision`).
- Upstream HTTP client: no total request timeout (per-chunk idle timeout + header-wait timeout + per-attempt
  deadline instead); automatic decompression **off**; never forward upstream `content-length` /
  `content-encoding` / `transfer-encoding` on streamed responses.
- SSE parser handles `\n`, `\r\n`, `\r`, multi-line `data:`, comment lines, 8 MiB line cap.
- Pre-commit buffering is bounded by size (64 KiB) **and** by the idle watchdog. `message_start` commits; `ping`
  never does. Codex exception: the Codex stream commits on the first content frame (not `message_start`) so a
  created→failed upstream can fail over; see `docs/parity.md`.
- Response bodies are produced by a spawned task feeding a channel; keep-alive pings come from an interval in that
  task and never await upstream. Client disconnect = channel send fails → `client_cancelled`: release reservation
  (via `Drop` guard), journal it, **never** record an account error. No state mutation may live after an `.await`
  in a handler future that hyper can cancel — use guards or the spawned task.
- Unknown inbound paths → Anthropic-shaped 404 JSON, journaled/logged (so new Claude Code endpoints such as
  `count_tokens` are visible).

### Logs, journal, privacy
- Journal/logs store error `type` + classified reason + upstream `request-id`, **not** upstream error messages
  (they can echo prompt content). Raw messages only in time-boxed debug mode, truncated and redacted.
- Debug header capture redacts `authorization`, `x-api-key`, `cookie`, `set-cookie`, `chatgpt-account-id`, and any
  `code=`/`state=` query values.
- Store a hash of `metadata.user_id`, not the raw value.
- Permissions: data dir 0700, creds/DB/config 0600; `doctor` checks them.

### Dashboard & management API
- `pooltime open` / `setup` print `http://127.0.0.1:<port>/?k=<key>`; the server exchanges it for an HttpOnly,
  SameSite=Strict cookie. Reads may be unauthenticated; all mutations require cookie or key. Reject requests whose
  `Host`/`Origin` isn't `127.0.0.1:<port>` / `localhost:<port>` (DNS rebinding). No CORS.

### Import & shadow run
- Snapshot mapping: `~/.bro/pool-settings.json` ↔ `~/.claude`; `pool-settings.<stem>.<sha256(resolved dir)[:8]>.json`
  where stem = basename with leading dots stripped. Unmatched snapshots (e.g. `pool-settings.prof.5019f7c5.json`
  here) are reported as orphans, not applied. Restore semantics: `null` → delete key; drop empty `env`.
- `usage.json` `deadRefreshToken` is a fingerprint (sha256[:16]) of the rejected refresh token: seed
  `needs_login` only if it matches the current token.
- Sticky-session keys import as `anthropic:<full metadata.user_id string>` (then hashed per the privacy rule).
- Keychain read via `security find-generic-password -s "Claude Code-credentials-<sha256(dir)[:8]>" -w` (may prompt).
- Import never deletes bro's files or Keychain items; it prints a cleanup command.
- **Shadow run uses a disjoint account set** (e.g. two accounts re-logged into pooltime only). `pooltime serve`
  refuses to serve imported accounts while a bro pool answers `/health` on its port unless `--i-stopped-bro`.

### Differential harness
- Compare **semantic SSE event sequences** + status + selected headers + resulting account state, with an explicit
  allowlist of expected differences (`X-Pool-Account`, request ids, `date`, ping timing, and every intended fix in
  inventory §10). Byte-exactness of Anthropic passthrough is asserted by pooltime unit tests, not by the harness.
- bro already honors `ANTHROPIC_API_BASE_URL` and `CLAUDE_OAUTH_TOKEN_URL`; the bro-fork patch only needs env
  overrides for the Codex URLs (`codex-constants.ts`, `codex-models.ts`). It's a dedicated plan task.

### Service
- launchd plist: `RunAtLoad`, `KeepAlive`, `ThrottleInterval`, `ExitTimeOut` = 600, stdout/stderr to pooltime's log
  dir, binary path via the stable Homebrew symlink when installed by brew. systemd user unit:
  `TimeoutStopSec=600`, `Restart=on-failure`; docs mention `loginctl enable-linger`.

### Stack (builder may substitute equivalents)
tokio, hyper 1 / axum, reqwest (rustls, stream; no gzip/brotli), rusqlite (bundled, WAL, single writer task),
tempfile (atomic persist + parent fsync), fs2/rustix flock, tracing + tracing-appender, clap, serde/serde_json
(preserve_order, arbitrary_precision), toml, rmcp (MCP, follow-up). Linux release builds on musl.
