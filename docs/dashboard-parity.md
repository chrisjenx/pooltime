# Dashboard parity with bro

What bro's dashboard (`pool/src/server/dashboard-*.ts`: overview table, account drawer) shows per
account and for the pool, against pooltime's **Overview** and **Accounts** views. "Before" is v1.0.0;
"after" is this change. New status fields are in `GET /api/v1/status` (`plan`, `tier`,
`last_used_at`, `usage_updated_at`, `unified_status`, `usage_check`, `counters`, and
`reset_estimated` per window). See [Dashboard](dashboard.md).

## Pool summary

| Item | bro | pooltime before | pooltime after |
|---|---|---|---|
| Accounts available | "available / total" metric | "N of M accounts ready" | same |
| Unavailable count | metric + attention box | banner + per-state pills | same (more detail than bro) |
| Active sessions, total | metric | missing | in the 7-day picture |
| In-flight requests, total | metric | missing | in the 7-day picture |
| Usage-check warnings | count in the attention box, "View usage warnings" filter | missing | count in the 7-day picture, "Usage check failing" filter, row chip, attention list |
| Pool 7-day headroom | — | "N of M subscriptions left", bar | same (bro has none) |

## Account list (bro's overview table)

| Item | bro | pooltime before | pooltime after |
|---|---|---|---|
| Provider · plan | "Anthropic · max" under the name | provider on cards only, no plan | "anthropic · max" on table rows; provider and plan badges on cards |
| Non-default priority / weight | `P10` / `×2` chips | Accounts card text only | chips on table rows (`P10`, `×2`, `×5 until 17:00`) |
| Status | Ready / Cooldown / Logged out / Sidelined / Billing | seven exact state words, since when | same, plus "for 2d" on every row |
| 5-hour used | meter | cards only | "5-hour left" meter with reset on the Overview table |
| 7-day used | meter | "7-day left" bar | same |
| 7-day reset | relative time column | absolute + relative | own column, absolute + relative |
| Next reset (soonest of 5h / 7d) | column | missing | both resets shown (5-hour cell and 7-day column), so a separate column adds nothing |
| Reset already passed | rolled forward, "Reset assumed — awaiting usage check" | "resets 3h ago" | rolled forward one window, marked "assumed"; made-up resets marked "estimated" |
| Sessions, in flight per account | columns | cards only | "Activity" column |
| Sort | every column, asc/desc; default name | fixed: most severe first | **Sort** menu: 7-day reset (default, soonest first), 7-day left, 5-hour left, state, name, activity; **Reverse**; remembered in localStorage. Unknown reset / needs login go last, then by name. Applies to Overview, Accounts and Routing |
| Search / filters | name search; provider; status incl. unavailable, usage warning; clear | missing | search (name or email), state (incl. unavailable, usage check failing), provider (when there are several), shown count, **Clear filters** |
| Attention list | "N unavailable" box, filter shortcut | "Accounts by severity" (all accounts) | **Needs attention**: only accounts that need a look, most severe first, so needs-login stays on top whatever the sort |

## Account detail (bro's drawer, pooltime's card)

| Item | bro | pooltime before | pooltime after |
|---|---|---|---|
| Unavailable reason | text | "Why" with classified reason | same |
| Every usage window incl. per-model | name, % used, reset | same | same, with assumed / estimated resets |
| Usage freshness | "Usage checked X ago · snapshot Y ago" | missing | "usage checked 5m ago · snapshot 2m ago" |
| Usage check error | text | missing | "usage check failed (timeout, 10m ago)" |
| Usage status (`unifiedStatus`) | value | missing | "status allowed" |
| Authentication | Authenticated / Login required | status word | same |
| Access-token expiry | "Expires in X" / "Expired · refresh on use" | missing | "Token: expires in 6h (refreshes by itself)"; hidden for a dead account (bro's §10.11 bug) |
| Login (refresh) expiry | — | shown, warn colours | same |
| Rate tier | value | missing | "Rate tier" |
| Last used | relative | missing | "Last used" (successes only) |
| Total requests | number | missing | "Requests: 42 since 02:00 (1 failed) · 5,210 all time" |
| Window requests / input / output tokens | numbers | missing | "Requests" and "Tokens" (in, out, cache read, cache write) for the current window |
| Recorded window cost | `$` estimate | missing | **not ported**: pooltime does not price requests (subscriptions are flat-rate); tokens are shown instead |
| Cooldown until | relative | "Until" | same |
| Last serving error | text | "Last error" | same |
| Recheck billing | button | button | same |
| Priority / weight edit | form in the drawer | Routing view | same (Routing view) |
| Login command | `bun run … accounts login` | Re-login button (key holders) | same, plus `pooltime accounts login <name>` on dead accounts in the read-only view |
| Routing factors (expiry share, 5h headroom, 5h factor, viability, score, route reason, next pick) | drawer + Routing view | missing | **deferred**: needs a routing-preview API over the scorer; it belongs to the Routing view, not the account summary |

## Dropped or deferred in the parity inventory

`docs/specs/2026-11-07-pooltime-parity-inventory.md` and `docs/parity.md` drop no dashboard item;
§10.11 (one status per account, no stale headroom for a dead account, `last_used` only on success)
is fixed and kept: needs-login accounts still show no windows, headroom or usage freshness.
