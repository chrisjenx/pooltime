# Grok (xAI) subscription routing in the pool

**Date:** 2026-07-16
**Status:** Design (incorporates Fable spec review; pending final review)
**Scope:** `pool/` (Bun/TypeScript service). One follow-on entry in root `models.json` is optional and out of scope here.

## Summary

Add a third upstream provider, `xai`, to the pool so it can pool **SuperGrok / X Premium+ subscription accounts** and serve Grok models (starting with `grok-4.5`) the same way it already pools Claude Max (`anthropic`) and ChatGPT-subscription (`openai`/Codex) accounts. Motivation: Grok 4.5 is reportedly close to Opus quality but much faster, and the user has a Grok subscription rather than wanting a pay-per-token xAI API key.

The integration parallels the Codex work, but is simpler in one dimension and trickier in another:

- **Simpler protocol:** xAI exposes a native **Anthropic-Messages-compatible** endpoint at `https://api.x.ai/v1/messages`. Since callers already send Anthropic-shaped bodies, the upstream is a *clone of `anthropic.ts` with known divergences*, not a full protocol translator like `codex-translate.ts`.
- **Trickier safety:** the pool's provider split is binary and **anthropic is the fall-through everywhere**. A missed routing site does not error — it silently serves Grok traffic on a Claude account. The design therefore turns "add a third enum value" into a compile-checked change.

## Goals

- Pool one or more Grok subscription accounts, authenticated via xAI's **official OAuth device-code flow** (no API key, no reverse-engineering).
- Serve `grok-4.5` through the pool's Anthropic endpoint (`POST /v1/messages`) with account selection, token refresh, and failover.
- Do it without ever silently misrouting Grok traffic onto Claude/ChatGPT accounts (or vice-versa).

## Non-goals (this spec)

- Transparent family mapping (`opus → grok-4.5`). That is **Phase 2** and is a redesign of mapped dispatch, not a threading exercise (see §7).
- The pay-per-token xAI API-key path.
- Any reverse-engineered / fingerprint-spoofing access route (ToS-violating; explicitly excluded).
- Claude-style utilization/headroom windows for xai accounts — xAI does not send the headers (see §6).

## Background

### Pool provider model today

- `Provider = "anthropic" | "openai"` (`pool/src/accounts/types.ts:16`).
- An account's provider is inferred from a marker file: `openai-auth.json` present → `"openai"`, else `"anthropic"` (`manager.ts:497` `providerFor`).
- Inference dispatch is in `server.ts` `handleAnthropic`: `route.provider === "openai"` → `proxyCodexMessages`, **else** → `proxyAnthropicMessages` (`server.ts:354`).
- `anthropic.ts` is a near-passthrough reverse proxy (pick account → refresh OAuth token → forward body to `/v1/messages` → stream SSE back, tapping usage).

### xAI OAuth findings (researched 2026-07-16)

OIDC discovery at `https://auth.x.ai/.well-known/openid-configuration`:

- `authorization_endpoint`: `https://auth.x.ai/oauth2/authorize`
- `token_endpoint`: `https://auth.x.ai/oauth2/token`
- `device_authorization_endpoint`: `https://auth.x.ai/oauth2/device/code`
- `grant_types_supported`: `authorization_code`, `refresh_token`, `urn:ietf:params:oauth:grant-type:device_code`
- `scopes_supported` include: `openid profile email offline_access api:access grok-cli:access`
- `code_challenge_methods_supported`: `S256`; `token_endpoint_auth_methods_supported` includes `none` → **public PKCE client**.

Post-login, inference uses the same `https://api.x.ai/v1` surface (Anthropic-compatible `/v1/messages`) with `Authorization: Bearer <oauth_access_token>`. The official Grok CLI stores tokens at `~/.grok/auth.json`.

**Feasibility risk (external, unfixable in code):** the OAuth inference surface is reportedly gated to **SuperGrok Heavy**; standard SuperGrok subscribers have seen HTTP 403 at inference time. The Phase 0 spike is the go/no-go gate for this.

## Key decisions

1. **New provider id `xai`** (model ids stay `grok-*`).
2. **Device-code login**, not localhost-callback. The device flow needs no bound port, no port-collision fallback, and no browser-redirect race — deleting the machinery `openai-login.ts:26-75` exists solely to manage. It prints a URL + short code and polls the token endpoint.
3. **Clone `anthropic.ts` into `xai.ts` with ~6 known divergences** (§5). Approach C (generalize both into a shared passthrough core) is deferred — it would touch the pool's hot path for the whole service to add a second consumer, which is more regression risk than warranted now. Noted, unscheduled.
4. **Make provider routing compile-checked** so a missed site fails `tsc`, not production (§4).
5. **Phase the work** (§8) so the tier-gating risk is disproven before any real code lands.

## 4. Provider threading — make misses fail at compile time

Fable review: "mechanical" undersells this. The split is `if openai / else anthropic`, so every missed site *silently serves Grok on a Claude account*. Landmines:

- `server.ts:354` — `if (route.provider === "openai") return proxyCodexMessages(...)`, else `proxyAnthropicMessages`. A `grok-4.5` route falls through to Anthropic.
- `server.ts:273` `openAIEndpointModelError` — `if (route.provider !== "openai") return null`. On `/v1/chat/completions` a grok model is not rejected, drops into the CLI-subprocess path, and `resolveModel` defaults an unknown id to **"sonnet"** — Grok request answered by Claude Sonnet.
- `server.ts:296` `nonOauthOpenAIBackendError` — same `!== "openai"`; on `CLAUDE_POOL_BACKEND=cli` grok silently runs the Claude CLI.
- `models.ts:201` `isModelRoute` — `provider === "anthropic" || provider === "openai"`. A user's on-disk `{"provider":"xai"}` row is **silently filtered out** by `mergeModelTable`, then `resolveModel` (`models.ts:73`) re-defaults the id to `provider: "anthropic"`. This guard compounds every other miss.

**Required changes:**

- Add `"xai"` to `Provider` (`types.ts:16`) and to `isModelRoute`'s allow-list (`models.ts:201`).
- **Remove the defaulted `provider` parameters** so `tsc` enumerates the call sites:
  - `pick(sessionKey, tried, provider, modelFamily)` (`manager.ts:604`) — currently `provider: Provider = "anthropic"`.
  - `setAffinity(sessionKey, accountName, provider, now)` (`manager.ts:912`).
  - `routingSnapshot(provider, now)` (`manager.ts:852`).
  - Note `anthropic.ts:96` calls `mgr.setAffinity(sessionKey, account.name)` relying on the default — a clone that updates `pick` but not `setAffinity` would silently pin xai sessions under the `anthropic:` ledger key (`sessions.ts:36`) and lose stickiness with no type error.
- **Invert the three guards** from `!== "openai"` / `=== "openai"` to `!== "anthropic"` / add explicit `xai` dispatch, so any non-anthropic provider is handled deliberately.
- Dispatch: in `handleAnthropic`, add `if (route.provider === "xai") return proxyXaiMessages(body, headers, mgr, config, signal, route, hooks)`.

## 5. `xai.ts` — clone of `anthropic.ts` with known divergences

Not a verbatim passthrough. `proxyAnthropicMessages` takes **no `ModelRoute`** and never rewrites `body.model` (for the anthropic path caller-model == upstream-model). Divergences to implement:

1. **Auth source:** replace `accessTokenFor` (`oauth-token.ts`, hardwired to `claudeAiOauth` + `config.oauthTokenUrl`) with an xai token accessor + refresh (§6 auth).
2. **Base URL:** `https://api.x.ai/v1/messages`, from a new `config.xaiApiBaseUrl` (default `https://api.x.ai`), reusing the `messagesUrl()` join logic.
3. **Rate-limit headers:** `hasRateLimitHeaders`/`parseRateLimitSnapshot` (`anthropic.ts:432-481`) are Anthropic-`unified`-specific and dead for xai — omit. `resetAtFromHeaders` (`anthropic.ts:483`) uses Anthropic header names; for xai only `retry-after` applies.
4. **Route threading (for Phase 2):** thread `route` in so the proxy can rewrite `body.model = route.upstreamModel` and clamp `max_tokens` for mapped requests. Phase 1 (`id === upstreamModel`) is a no-op rewrite, but the seam must exist now so Phase 2 is not a structural change.
5. **`max_tokens` clamp:** a Claude-sized `max_tokens` (e.g. 64000) may 400 on Grok. `codex-translate` absorbs this today; a passthrough needs an explicit per-model clamp.
6. **Thinking-block stripping:** still call `stripCodexThinking` (`anthropic.ts:86`) — a session that ran mapped-to-Codex carries stash-signature thinking blocks xAI will reject. See §9 for the reverse direction.

Everything else (streaming prefix buffer + `StreamUsageTap`, 401→refresh→retry, `tried`-set failover loop, `recordSuccess`/`recordError`, hop-by-hop header scrubbing) is reused as-is. Sell this as "6 known divergences", not "near-passthrough".

## 6. Auth / token lifecycle, usage sweep, and routing without snapshots

### New files (mirror Codex counterparts)

- `upstream/xai-constants.ts` (← `codex-constants.ts`): endpoints above, public `client_id`, scopes (`openid profile email offline_access api:access grok-cli:access`). The `client_id` and the confirmation that the device grant is permitted for it are **captured during the spike** from the open-source reference impls (opencode-grok-auth / pi-xai-oauth / grok-cli), the same way `CODEX_CLIENT_ID` came from codex-rs.
- `accounts/xai-oauth.ts` (← `openai-oauth.ts`): `normalizeXaiAuthJson` + `refreshXaiToken`.
- `accounts/xai-login.ts` (← `openai-login.ts`, but **device-code**): request device code → print URL + user code → poll `token_endpoint` until authorized/expired → store creds.

### Credential shape

`XaiOauthCreds { accessToken; refreshToken; expiresAt }` in `xai-auth.json` (the provider marker). **`planType` is dropped** (YAGNI until the spike proves the id_token carries a usable plan claim). `providerFor` precedence: check `xai-auth.json` before `openai-auth.json`; document that both markers in one dir is unsupported.

### Refresh grant — spike this specifically

`openai-oauth.ts:31` refreshes with a **JSON body**, which OpenAI tolerates. RFC-6749 endpoints (which `auth.x.ai` implies) typically require `application/x-www-form-urlencoded` and reject JSON. Copying Codex's refresh verbatim is the **most likely silent Phase 1 failure** — it works until the first token expiry (~1h), then breaks. `refreshXaiToken` uses form-encoding; the spike must exercise the **refresh** grant, not only the initial code/device exchange. Reuse the `refreshLocks` dedup pattern from `ensureFreshToken` (`openai-codex.ts:236`).

### `getAccount` needs a real third branch

`manager.ts:517-563` is binary ternaries. For `provider === "xai"`, both `oauth` and `openai` resolve null → `authenticated` takes the anthropic else-branch → `false` → the account is permanently "not authenticated". Add an explicit xai branch computing `authenticated`/`tokenExpiresAt`/`subscriptionType` from `XaiOauthCreds`. Also: `readCreds` (`manager.ts:481`) does a macOS Keychain read as the anthropic fallback — without the branch, every `getAccount` on an xai account does a pointless `security` lookup (and `listAccounts` runs on every `pick`).

### Usage sweep — three-way dispatch

`usage.ts:226` dispatches `a.provider === "openai" ? maybeRefreshCodexUsage : maybeRefreshUsage`. An xai account falls into the **Anthropic** branch → `fetchUsageSnapshot` returns null → a bogus `"usage refresh failed…"` written to `usage.json` every `usageRefreshTtlMs` (120s) per xai account, plus a permanent false error on the dashboard. Make the dispatch three-way: **skip xai** (no known ground-truth usage endpoint) unless/until one is found. This must land together with the `getAccount` branch.

### Routing behaviour with no snapshots

Traced through `manager.ts`: with `rateLimitStatus: null`, `headroomOf`/`headroomFraction` → 1, `viable` → true, urgency ranks tie at 0. Intra-xai selection degenerates to `weight × loadFactor` (least-loaded with manual weights) — **this works**; "generic cooldown only" is fine for Phase 1.

Documented consequences:

- **429-reactive only:** an exhausted xai account rejoins at phantom full headroom after `rateLimitCooldownMs` (60 min default) and eats a live 429 each cycle. The recent un-sideline / ground-truth correction path (`manager.ts` `recordUsageSnapshot`) never runs for xai.
- **Dashboard string:** reason text will read "no live window data yet — probing to refresh headers" for xai, where "probing" never yields data. Cosmetic; adjust the copy for providers with no usage endpoint.
- **`RATE_LIMITED_LAST_ERROR = "rate limited by Anthropic"`** (`manager.ts:152`) will show for xai 429s. Cosmetic; generalize the string.
- **Phase 2 `pickProvider` bias (deferred but noted):** the phantom 1.0 `maxHeadroom` makes mapped traffic *systematically prefer Grok* over any Claude account with >0% utilization until Grok live-429s. Phase 2 must add a "no-data ≠ wins" rule before Grok joins a mapped candidate set.

## 7. Phase 2 (mapping) is a redesign, not threading — recorded for later

Not built in Phase 1, but scoped here so Phase 1 leaves the right seams:

- `models.ts:90` `mappingFor` guards `if (target.provider !== "openai") return null` and hardcodes `provider: "openai"` in the returned route. An `opus → grok-4.5` row is **silently inert** today, yet `validateMapping` (`server.ts:476`) accepts it (grok-4.5 is in the table) — the dashboard says "saved", nothing changes.
- `chooseMappedService` (`server.ts:311`) hardcodes a two-candidate list and the `"anthropic" | "openai"` literal; `serveWithCrossProviderFallback` (`server.ts:334`) computes `other = first === "anthropic" ? "openai" : "anthropic"` — "the other provider" is undefined with three.
- `ModelMapping.effort` / `CodexEffort` is Responses-API-specific; meaningless for an xai target, and a passthrough has no seam to apply it (does xAI honour Anthropic `thinking.budget_tokens`? — spike item).
- `/api/status` `targets` filter (`server.ts:97`) and `/v1/models` `owned_by` (`server.ts:140`) exclude/mislabel xai.

Phase 2 = "generalize mapped dispatch to an ordered N-provider candidate list", refactoring `server.ts:311-381` and `models.ts:83-92`.

## 8. Phasing

- **Phase 0 — Spike (go/no-go).** Build the minimal device-code login first (a token cannot be obtained otherwise; the throwaway script effectively *is* `xai-login.ts`). Then run the §10 checklist. A **403 at inference = stop** (tier gating). Alternatively harvest creds from `~/.grok/auth.json` if the official CLI is already logged in (scope `grok-cli:access`), mirroring `accounts import` (`cli.ts`).
- **Phase 1 — Walking skeleton.** `xai-constants.ts`, `xai-oauth.ts`, `xai-login.ts` (device code), provider threading (§4, compile-checked), `xai.ts` (§5), `getAccount` + usage-sweep branches (§6), `grok-4.5` route. **Direct addressing only** (`{"model":"grok-4.5"}`). Exit criteria: `curl` the pool for `grok-4.5` (stream + non-stream) succeeds, an xai account renders on the dashboard, and a token survives one refresh cycle.
- **Phase 2 — Mapping + polish.** Generalize mapped dispatch (§7), add the `pickProvider` no-data rule, dashboard labels, `stripXaiThinking` (§9), more model ids.
- **Later (unscheduled) — Approach C.** Generalize `anthropic.ts` + `xai.ts` into a shared passthrough core.

## 9. Cross-provider thinking signatures (gates Phase 2 fallback)

The real cross-provider bomb (`pickProvider` docstring at `manager.ts:658` already warns thinking blocks don't round-trip):

- **Anthropic-signed thinking in history → xAI:** if xAI validates signatures, every mapped mid-session request with thinking history 400s deterministically across *all* xai accounts (terminal; failover cannot help).
- **Grok-produced thinking → later served by real Anthropic:** `stripCodexThinking` (`codex-translate.ts:225`) only strips blocks that are ours (stash-JSON or absent signature); an opaque xai signature passes `isOurs === false`, is forwarded to Anthropic, and Anthropic rejects the foreign signature.

Before any cross-provider fallback involving xai (Phase 2), add a `stripXaiThinking` analogue or a signature-tagging scheme. Phase 1 direct addressing does not cross providers, so it is unaffected — but the spike must still observe whether Grok emits thinking blocks at all.

## 10. Phase 0 spike checklist (must-checks, beyond "200 + usage present")

Replay a **real captured Claude Code request** (not a hello-world curl): thinking history, `tool_use`/`tool_result` turns, a `system` array, `cache_control` blocks, `metadata.user_id`, and the `anthropic-version` + `anthropic-beta` headers — `upstreamHeaders` (`anthropic.ts:505`) forwards all of these to xAI verbatim. Capture:

1. **Inference 200 vs 403** (tier-gating go/no-go) on `/v1/messages` with an OAuth bearer.
2. **Token *refresh* grant** succeeds with `application/x-www-form-urlencoded` (§6). This is the highest-value check.
3. **SSE event inventory**: does xAI emit `message_start` / `message_delta` / `ping` so `StreamUsageTap` (`anthropic.ts:361`) parses usage? (Missing `message_delta.usage` merely zeroes counters — tolerable.)
4. **Thinking-signature behaviour** both directions (§9): does xAI accept Anthropic-signed thinking history; does xAI emit its own thinking blocks?
5. **429 body wording** vs the `isRateLimit` phrase-sniffer (`shared.ts:134`) — if xAI's text doesn't match, a streamed 429 is treated as terminal instead of failing over.
6. **`retry-after` presence** on 429; cache-token usage fields; whether `max_tokens: 64000` 400s (informs the §5 clamp).
7. The working **public `client_id`**, redirect/device parameters, and scopes.

## 11. Testing (all `bun:test`)

- `xai-oauth.test.ts`: `refreshXaiToken` — success, no-refresh-token, timeout, non-ok, **form-encoding asserted** (← `openai-oauth.test.ts`).
- `manager` / `types`: `providerFor` + `getAccount` for the `xai` marker (authenticated/expiry derived); `providerFor` precedence when both markers exist.
- `models`: table resolves `grok-4.5 → xai`; `isModelRoute` accepts an on-disk `xai` row.
- `xai.ts` upstream against a mock `fetch`: non-stream usage recording; streaming prefix + tap; 401→refresh→retry; 429→cooldown+failover; 403→terminal with a clear tier message; `max_tokens` clamp applied; `body.model` rewrite when `route.upstreamModel !== route.id`.
- `xai-login`: device-code request + poll loop + token-exchange body shape with a mock (the interactive dance is not fully e2e-testable, same as the openai flow).
- Manual: `curl` the pool for `grok-4.5` (stream + non-stream); dashboard shows the xai account; token survives a forced refresh.

## 12. Risks / assumptions / open questions

1. **OAuth token accepted on `/v1/messages`** — spike §10.1 confirms; fallback is Approach B (chat-completions translation) for request/response mapping only, keeping everything else.
2. **Refresh grant encoding** — §10.2; assumed form-encoded.
3. **Tier gating (Heavy)** — external; §10.1 is the go/no-go.
4. **Public `client_id` + device grant allowed for it** — captured/verified in the spike from reference impls.
5. **Grok thinking blocks / signature validation** — §10.4 decides whether Phase 2 needs `stripXaiThinking`.
6. **No ground-truth usage endpoint for xai** — accepted; routing is 429-reactive for xai (§6).
