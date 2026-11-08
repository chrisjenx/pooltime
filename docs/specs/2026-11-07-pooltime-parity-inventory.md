# pooltime — parity inventory of bro-cli (appendix to the design)

Source: bro-cli @ `bd0019f` (`/Users/christopherjenkins/git/personal/bro-cli`). Compiled 2026-11-07 from a full read of
the root CLI (`src/*.js`), the Bun pool (`pool/src/**`), `pool/ARCHITECTURE.md`, and ~650 test titles
(root `node:test` 58, pool `bun:test` ~594; biggest: `manager.test.ts` 138, `codex-translate.test.ts` 58,
`openai-codex.test.ts` 43, `anthropic.test.ts` 36). File:line refs are to that commit.

Every item below must end up either **ported (with a Rust test)** or **dropped on purpose (with a reason)** in
pooltime's `docs/parity.md`. The bro test suite is a second source of truth: its test names read as behavior specs.

Size for reference: pool ≈11.5K LOC TS (accounts 2.7K, Anthropic upstream 1.8K, Codex 1.8K, server 1K, dashboard
2.5K, legacy 0.8K), root CLI 1.4K LOC JS.

---

## 1. Anthropic `/v1/messages` (direct OAuth reverse proxy)

**Ingress**
- Route → proxy auth → JSON parse (400 on bad JSON) → only then disable ingress idle timer for that request.
- Proxy auth: `Authorization: Bearer <key>` or `x-api-key: <key>`; failure → 401 `authentication_error`.

**Upstream request**
- Body re-serialized semantically verbatim, except `stripCodexThinking`: remove thinking blocks fabricated by the
  Codex translator (stash signatures / no signature) — Anthropic returns a retry-stable 400 otherwise. Drop emptied
  assistant turns, don't strand system messages, exempt effort-only empty-content system messages.
  (`anthropic.ts:91-92`, `codex-translate.ts:228-285`)
- Headers: copy incoming; delete hop-by-hop (connection, content-length, host, keep-alive, proxy-*, te, trailer,
  transfer-encoding, upgrade) and `x-api-key`; set `authorization: Bearer <account token>`. Never synthesize
  `anthropic-version`/`anthropic-beta`.
- URL join tolerates base already ending in `/v1`; never `new URL(path, base)` (drops gateway prefixes) (`shared.ts:227-237`).
- Timeouts: no library fetch timeout; custom idle watchdog over header wait and each demanded body read
  (`ANTHROPIC_IDLE_TIMEOUT_MS` 600s, floor 30s, capped at request timeout); per-attempt deadline
  `REQUEST_TIMEOUT_MS` 900s. Empty chunks don't re-arm the watchdog; backpressure with no pending read is not idle
  (`shared.ts:127-214`).

**Token refresh**
- Refresh when `expiresAt - now <= 5 min`. `POST platform.claude.com/v1/oauth/token` JSON
  `{grant_type: refresh_token, refresh_token, client_id}`; 20s timeout. One in-flight refresh per account (tokens
  rotate). Persist rotated creds; clears dead marker.
- `invalid_grant` (top-level string or nested `error.type`) → mark refresh token dead (fingerprint the *rejected*
  token, not a re-read — a new login may have landed) (`oauth-token.ts:76-91`, `manager.ts:1446-1458`).
- Upstream 401 → force refresh, retry same account once. 401 after that → terminal (asymmetry with Codex; decide).

**Response classification**
| Upstream | Behavior |
|---|---|
| 429 **without** quota signal (no `anthropic-ratelimit-unified-*`, no `retry-after`, no rate-limit wording) | per-request refusal (e.g. missing Claude Code system prompt): terminal passthrough + `x-pool-upstream-rejected: 1`; **no bench, no failover, no cross-provider hop** (`anthropic.ts:217-230, 597-624`) |
| 429 with quota signal | mark rate-limited (reset = explicit → blocking-window reset → cooldown 1h), evict session pins, fail over |
| 529/500/503 | same-account backoff up to 4 retries (full jitter, base 500ms doubling, 8s cap, `retry-after` wins capped). Exhausted → replay upstream body+headers (strip content-encoding/length), add `retry-after` if missing, record error once. No failover, no sideline (overload isn't the account's fault) |
| 403 | entitlement wording (organization/subscription/billing/credit balance — deliberately narrow) → billing block; else access-denied 1h cooldown. Evict pins, fail over |
| other non-OK | record error, terminal passthrough |
| transport throw | 502 `api_error` with transport flag → fail over |
| refresh failure | 401 `authentication_error` → fail over |

**Streaming**
- Buffer prefix until committed or 64 KiB. `ping` doesn't commit. Initial `error` event with rate-limit → bench +
  failover. Initial `overloaded_error` → same-account backoff; never clears → surface as **529** (keeps it
  cross-provider retryable) (`anthropic.ts:530-552, 631-634`).
- After commit: forward bytes **unchanged**. Tap parses usage (`message_start`/`message_delta`, cache tokens).
- `message_stop` or `error` is terminal: close the response right after enqueuing terminal bytes; release the
  reservation even if upstream holds the socket (`anthropic.ts:440-446`).
- EOF before terminal event = truncation → stream errors, no success recorded (`anthropic.ts:473-514`).
- Post-commit error frame forwarded verbatim and recorded. Pre-commit transport fault → fail over, not 502.
- Anthropic path injects **no** keep-alive pings (upstream pings pass through).
- **Client disconnect: never error the response stream and never record an error after the client is gone** — in
  Bun this killed the daemon. Test with a real socket.

**Exhaustion**: last retry's status/type/message, else 503 `overloaded_error` ("No Claude accounts configured…" /
"All Claude accounts are currently unavailable…"). Transport exhaustion adds `X-Pool-Retryable-Transport: 1`.

**Response headers set by pool**: `X-Pool-Account`, `X-Pool-Route: classifier`, `x-pool-upstream-rejected`,
`X-Pool-Retryable-Transport`.

## 2. Account selection & routing

- **Usable** = authenticated ∧ not dead-refresh ∧ not access-denied cooldown ∧ not rate-limited-until ∧ no spent
  account-wide window ∧ provider matches ∧ under soft in-flight cap (unless ignoreCap) ∧ not tried this request ∧
  model-scoped window for this family not spent.
- **Tiers**: only the lowest priority number tier is used (priority int ≥0, default 100).
- **Sticky sessions**: key `metadata.user_id`; ledger `"<provider>:<sessionKey>"`; kept only if account usable and
  in active tier; persisted (`sessions.json`) because switching accounts re-pays the prompt cache; idle expiry 30m.
- **Weighted strategy** (port only this one): `score = weight × expiryShare × fiveHourFactor / (activeSessions+1)`.
  `expiryShare` by distinct 7-day reset rank 5,3,2,1 then halving; unknown ranks first (probe), spent last.
  `fiveHourFactor = min(1, headroom5h / headroomTaperStart) ^ fiveHourExp`. Viability gate 5h headroom ≥
  `minHeadroom` (0.1); if none viable, best-effort all. Ties → round-robin cursor. Tuning (`fiveHourExp` [0,5],
  `minHeadroom` [0,1], `headroomTaperStart` (0,1]) persisted only when explicitly set. Weight 0.1–10, default 1.
- **Soft concurrency**: select+reserve synchronously before token refresh; skip accounts at cap; if all busy wait
  for release up to `INFLIGHT_WAIT_MS` 90s shared across retries; then exceed cap rather than error (never bypass
  auth/cooldown/exclusion). Wake fan-out never races one slot into two requests. Reservation held through streaming,
  released on completion/failure/abort. Caps: Anthropic 0 (unlimited), Codex 4.
- Model-scoped windows (e.g. `7d-fable`) block only that family; never set account-wide cooldown.
- Explicit `allowed` status overrides 100% utilization (Codex unenforced limits).
- Provider pin goes to the most recently served provider; weighted provider comparison uses one global expiry scale
  and compares the account `pick()` would actually serve (`manager.ts:843-905`).

## 3. Usage tracking & rate-limit snapshots

- Headers `anthropic-ratelimit-unified-<key>-{status,utilization,reset}` parsed generically (model-scoped keys
  too); reset = unix seconds; merged per window key.
- Ground truth poll `GET {origin}/api/oauth/usage` (not under `/v1`): headers anthropic-version 2023-06-01,
  anthropic-beta `oauth-2025-04-20`, `User-Agent: claude-code/<ver>` (wrong UA → throttled bucket). Timeout 2.5s,
  TTL 120s, per-account lock, 60s sweep + one at startup (so benched accounts don't freeze at 429-time snapshot).
  Body utilization is 0–100 (percent) whereas `anthropic-ratelimit-unified-*-utilization` headers are 0–1 fractions and Codex `used-percent` is 0–100; bro's `usage.json` stores **0–1 fractions** (`usage.ts:30-50` divides by 100). Result **replaces** duration-keyed windows (keeps e.g. `overage`). Spent window with no
  reset → synthesize now+duration. Backfill 5h/7d from top-level objects when `limits[]` partial. Spent binding
  window → extend rate-limited-until; nothing blocking → clear stale cooldown. Entitlement 403 → billing block. A
  probe that started before a billing block must not clear it.
- Proactive sidelining: spent account-wide window with future reset → unavailable.
- Counters: rolling 5h window + lifetime (requests, input/output tokens). Cost always 0 on OAuth paths.

## 4. Codex / OpenAI path

**Translation Anthropic → Codex Responses**
- `system` → `instructions` (array joined `\n`). Text → `input_text`/`output_text` (adjacent joined `\n`). User
  images → `input_image` (data URL or URL). `tool_use` → `function_call` (JSON-string args). `tool_result` →
  `function_call_output` (string-folded; images → follow-up user message). `thinking` with stash signature, followed
  by text/tool_use in same message → `reasoning {id, summary, encrypted_content}`; orphan reasoning dropped (Codex
  400s otherwise); foreign thinking dropped.
- `stream:true`, `store:false`, `include:["reasoning.encrypted_content"]`, tools `{type:function,name,description,
  strict:false,parameters}`, `parallel_tool_calls:false`. tool_choice any→required, tool→{function,name}, none,
  else auto. temperature/top_p pass through. **Never send `max_output_tokens`** (400).
- Effort: `output_config.effort` (unless "default") wins over `thinking.budget_tokens` buckets (<8192 low, <16384
  medium, <32768 high, else xhigh); mapping effort override wins; default → unset; clamp to route's
  supportedEfforts (`max`→`xhigh` if supported, else omit).

**Upstream**: `POST chatgpt.com/backend-api/codex/responses`; Bearer, `chatgpt-account-id`, `originator:
codex_cli_rs`, `accept: text/event-stream`; idle watchdog 300s. Refresh `POST auth.openai.com/oauth/token` JSON
refresh grant, client_id `app_EMoamEEZ73f0CkXaXp7hrann`, scope `openid profile email`. Constants verified against
codex-rs `bdaad682` (reset-at absolute; no OpenAI-Beta header on HTTP; connector scopes).
Login: browser PKCE, bind callback port 1455 then 1457 *before* building redirect_uri; code exchange form-encoded;
account id from id_token claim. Import from `~/.codex/auth.json`.

**Codex responses**
- Rate-limit headers `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-at}`; window key from
  **duration** (<24h → 5h, else 7d), not slot. Replace snapshot only when non-empty. Status `allowed` at 100%
  unless response was 429.
- 429 → rate-limited until soonest *future* reset of either window (else retry-after) → fail over.
- 401/403 → force refresh, retry once; still refused → access-denied (evict pins) → fail over.
- Other non-OK → terminal `api_error` with legible message (detail → error.message → message → raw, 300 chars,
  account-tagged). (Codex 5xx is terminal within Codex pool today — decide.)
- Usage poll `chatgpt.com/backend-api/wham/usage` (UA `codex-cli`), gated only on last-check time; reject a window
  only when account limited **and** that window full.

**Stream translation Codex → Anthropic SSE**
- `response.created`→`message_start`; `output_item.added`→`content_block_start` (text/tool_use/thinking); text
  deltas; `function_call_arguments` → `input_json_delta`; `reasoning_summary_text` → `thinking_delta`;
  `output_item.done` → `content_block_stop`; reasoning with `encrypted_content` → `signature_delta`
  `JSON{id,encrypted_content}`; `completed`/`incomplete` → `message_delta` (stop_reason tool_use / end_turn /
  max_tokens / refusal for content_filter) + `message_stop`; `failed`/`error` → `error` (terminal).
- Usage: `input_tokens = total - cached`, `cache_read_input_tokens = cached`. Account totals include cached.
- Malformed tool args → deferred error unless terminal was a token-cap incomplete. EOF without terminal or without
  `message_start` → error.
- Pre-commit buffer 64 KiB; at cap emit forced `message_start` (real `response.created` echoes full instructions and
  can exceed it). SSE line cap 8 MiB. Pre-commit upstream error never streamed as lone frame inside a 200. Dropped
  reasoning events → `ping`. **Timer-driven keep-alive** ping every 1s of silence — must not depend on the consumer
  pulling the upstream. Tear down on terminal event (waiting for EOF can hang).
- Context carry: LRU 1000 keyed (sessionKey, routeId) seeds `message_start.usage.input_tokens` with previous
  turn's input+cached+output; only successful terminal usage updates it.

**Catalog discovery**: `GET chatgpt.com/backend-api/codex/models?client_version=…` per account, 8s, retry once on
401/403 after refresh. Strict validation (bad entry rejects that account's catalog); `visibility:"list"` only; slug
`^[a-z0-9][a-z0-9._-]*$`i; context_window/max_context_window (max kept only if ≥ context);
`supported_reasoning_levels[].effort` filtered, `ultra` excluded. Additive only. All fail → error; some fail → warn.
(bro needs restart to load new routes — pooltime should hot-reload.)

## 5. Model routing table

- Defaults: Claude aliases `opus, sonnet, haiku, fable` + full ids (`claude-opus-5`, `claude-opus-4-8`,
  `claude-sonnet-5`, `claude-haiku-5-5`, `claude-haiku-4-5`, `claude-fable-5`, `claude-fable-5-1`); Codex
  frontier (272K/872K max, efforts low..max) `gpt-6-astra/sol/luna`, `gpt-5.6-sol/terra/luna`, `gpt-5.6`→sol;
  others (none..xhigh) `gpt-5.5` (272K), `gpt-5.4` (1M max), `gpt-5.4-mini`. (Refresh these at build time.)
- `resolveModel`: exact → strip trailing `[1m]` (ci) → else pass through to Anthropic verbatim.
- Openai route with maxContextWindow ≥ 872000 listed as `<id>[1m]`. Bundled full Claude ids hidden from
  `/v1/models` (one row per family alias).
- `/v1/models`: live Anthropic catalog (`GET /v1/models?limit=1000` with **any** account's token, not `pick()`),
  10 min cache, single-flight, stale preferred over failure; then table Claude rows not listed; then OpenAI rows;
  configured OpenAI ids win collisions; private `context_window`/`max_context_window` fields on OpenAI rows.
- User overrides file: file routes shadow defaults; same-target rows inherit bundled metadata; duplicate ids keep
  first.
- Family mapping (default **off**): fable→gpt-6-astra, opus→gpt-6-sol, sonnet→gpt-6-sol, haiku→gpt-6-luna;
  persisted rows authoritative, missing filled from defaults; anthropic target or `to===from` = opt-out; unknown
  non-Claude id → 400; per-row effort sanitized; POST overlays onto **live** set. Families by substring order
  `fable, mythos, opus, sonnet, haiku`.
- Cross-provider fallback: at most one hop on 429/500/503/529 or retryable-transport; never on upstream-rejected;
  both fail → return the **first** response (log the second!). Shared slot-wait budget across providers.
- Classifier (Claude Code auto-mode security monitor): non-stream, no tools, max_tokens ≤1024, a system block
  starting with the literal `You are a security monitor for autonomous AI coding agents` → start on Anthropic, keep mapped fallback, never touch
  affinity, `X-Pool-Route: classifier`. Marker-drift warning (5-min throttle). `CLASSIFIER_ROUTE=default` disables.

## 6. Claude Code integration (settings override)

- Target `<profile>/settings.json` `env` block only; all other settings preserved. Managed keys:
  `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>`, `ANTHROPIC_AUTH_TOKEN` (proxy key or placeholder),
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS` + marker key, pin keys `ANTHROPIC_DEFAULT_{SONNET,OPUS,FABLE,HAIKU}_MODEL{,_NAME,
  _DESCRIPTION}`, retired `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
- Snapshot prior values once per profile (per-profile file — a shared file cross-contaminated profiles); backfill
  newly managed keys; never snapshot pooltime-written pin values; leave an unreadable snapshot alone; skip write if
  unchanged (don't rewrite under running sessions); disconnect restores prior (null → delete), drops empty `env`.
- Catalog-derived pins from the pool's own `/v1/models`: Sonnet = newest `claude-sonnet-*` + `[1m]`, name
  `Sonnet`, description `"<Name> with 1M context · Efficient for routine tasks"` (collapses duplicate 200K/1M rows);
  Haiku = newest bare id, name `Haiku`, description `"<Name> · Fastest for quick answers"` (else gateway resolves
  `haiku` to 4.5). **No Fable pin** (adds a row); instead rewrite existing `claude-fable-*` rows in
  the profile's `.claude.json` `additionalModelOptionsCache` in place, keep `[1m]`, atomic write. **Path rule:**
  default profile (`~/.claude`) → `~/.claude.json` (home dir, *not* inside `~/.claude`); any other profile dir →
  `<dir>/.claude.json` (bro `settings.js` `claudeJsonPath()`). Max context = min
  Codex context_window. "Newest" = highest `created`, then numeric-aware id compare. Unreachable catalog on
  refresh → no change (never drop the Sonnet pin). No invented picker text; one row per model.
- Legacy pin scrubbing (recognize older bro-written values/shapes; Haiku only via sibling description).
- Claude Code has **no fallback** for an unreachable base URL → a failed/cancelled bring-up must release the
  override.
- settings.json written non-atomically in bro — pooltime: atomic.

## 7. Process / CLI quirks

- `/health` must prove it's the pool (`status=="ok"` + numeric `accounts`) before trusting or killing a listener
  (a dev server SPA returns 200). Kill only `LISTEN` sockets on the port (bare lsof matches client sessions).
- Graceful drain on SIGTERM/SIGINT up to 10 min; second signal forces. Health refuses during drain.
- Port env is `BRO_POOL_PORT`, not generic `PORT` (pooltime: `POOLTIME_PORT`).
- `healthy()` treating slow `/health` as "not running" caused double-spawn risk — fix.
- Account name rule `^[A-Za-z0-9._-]{1,64}$`. `pruneToLiveAccounts`: removed account's usage must not resurrect.
- Keychain specifics (macOS `Claude Code-credentials-<sha256(dir)[:8]>`, adopt-only-if-changed) exist only because
  bro logs in via `claude`; pooltime's own login removes the need, but **import** must read them.

## 8. On-disk state to import from bro

- `~/.claude-max-pool/accounts/<name>/`: `.credentials.json` (`claudeAiOauth{accessToken, refreshToken,
  expiresAt ms, refreshTokenExpiresAt, scopes, subscriptionType, rateLimitTier}`) or Keychain item;
  `openai-auth.json` (`accessToken, refreshToken, accountId, expiresAt, planType` — presence = Codex account);
  `routing.json` (`priority`, `weight`).
- `~/.claude-max-pool/usage.json` (snapshots, markers), `sessions.json`, `models.json` (routes + mappings),
  `tuning.json`.
- `~/.bro/config.json` (permission mode — launcher only, not needed), `~/.bro/pool-settings*.json` (override
  snapshots per profile — needed to hand over/restore Claude Code settings cleanly).

## 9. Not ported (proposed)

- Legacy CLI-subprocess backend (`pool/src/subprocess/*`, `adapters/*`, `failover.ts runWithFailover`): text-only,
  no Fable, no rate-limit headers, no Codex. bro never uses it.
- `/v1/chat/completions` (only works via the legacy path).
- `ROUTING_STRATEGY=expiring|headroom`.
- The `bro` launcher (spawning `claude` with permission flags, `--dry-run`, `--safe`, hold-or-continue, removed-flag
  table).
- Migration-only code for bro's own historical formats beyond what import needs.
- Windows support.

## 10. Known gaps in bro (fix in pooltime — from the observability audit)

1. Account state changes never logged (`markRefreshTokenDead/RateLimited/AccessDenied/BillingBlocked` only mutate
   usage.json). Two accounts died silently (6d / 16d) with zero log lines.
2. `fetchUsageResult` collapses every failure to "usage refresh failed (see logs)" with nothing logged.
3. `recordError` is a single 500-char `lastError` wiped by next success; non-invalid_grant refresh failures, 401
   after refresh, and all Codex refresh failures leave account "Ready".
4. Codex has no dead-refresh-token detection (`invalid_grant`, `refresh_token_reused`).
5. Failover log lines omit the reason; earlier attempts' reasons discarded; cross-provider secondary error discarded.
6. Mid-stream upstream failures dumped raw by runtime with no timestamp/account/request (144 ECONNRESET, 20 idle
   timeouts, 536 unattributed TimeoutError in the current log).
7. Client aborts invisible (not counted/timed); aborted turns never counted.
8. Fire-and-forget `.catch(()=>{})` on usage refresh; `/v1/models` token lookup hammers dead accounts' refresh.
9. Corrupt `usage.json` silently resets state (dead accounts back in rotation); corrupt creds → "Logged out" with
   error lost; corrupt settings.json → may be overwritten.
10. Log: plain stdout/stderr appended forever (3.4 MB), no timestamps/levels/IDs, contains prompt fragments.
11. Dashboard: "Sidelined" conflates needs-login / non-billing 403 / spent window; dead account shows "Token:
    Expired · refresh on use" and stale snapshots projected as 0% used; "Last used" bumped by errors; management
    API unauthenticated.
12. Credentials written non-atomically; refresh lock in-process only; refresh timeout after server-side rotation
    loses the new token.
13. `refreshTokenExpiresAt` present in every credentials file but never read.
