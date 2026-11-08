# OAuth spikes (S1, S2)

Recorded for plan Task 2. Spec reference: design spec §16 "Sequencing inside M1". No product code depends on a guess here:
later tasks should build from the values below, and where a value is marked **pending** the dependent task must make it
configurable and not hard-code an assumption.

Nothing in this document is a token, secret or credential. The OAuth `client_id` is a public client identifier, and the
S2 fingerprints are truncated hashes of refresh tokens (16 hex chars of SHA-256), not tokens.

---

## S1 — Claude Code's real authorize / token / profile flow

**Question.** Which authorize URL, query parameters, scopes, redirect, token-exchange and refresh bodies does Claude Code
use to sign in a claude.ai subscription, so pooltime can run the same PKCE flow itself? Does `login_hint` work when
claude.ai is already signed in as a different user?

### S1 static (done)

**Method.** Newest installed binary `~/.local/share/claude/versions/2.1.319` (Mach-O arm64, Bun-compiled; the JS ships
as readable source inside the binary, header `// Version: 2.1.319`, build `2026-11-05T18:14:18Z`). Read-only. Ran
`strings -a -n 8` into `$TMPDIR/s1/strings.txt`, then a small Python script (`python3 -I`) that prints a window of
characters around a pattern. Identifiers below (`sEo`, `Dfe`, ...) are minified names valid for 2.1.319 only. Cross-checked
against bro's `pool/src/config.ts` and `pool/src/upstream/oauth-token.ts` (read-only).

#### Authorize URL builder (`sEo`, contains `searchParams.append("code","true")`)

```js
function sEo({codeChallenge:e,state:n,port:r,isManual:s,loginWithClaudeAi:h,inferenceOnly:y,orgUUID:b,loginHint:w,loginMethod:R,oauthClient:O,alsoRequestScope:M}){
  let U=h?nn().CLAUDE_AI_AUTHORIZE_URL:nn().CONSOLE_AUTHORIZE_URL,H=new URL(U);
  H.searchParams.append("code","true"),
  H.searchParams.append("client_id",O?.clientId??nn().CLIENT_ID),
  H.searchParams.append("response_type","code"),
  H.searchParams.append("redirect_uri",s?nn().MANUAL_REDIRECT_URL:`http://localhost:${r}/callback`);
  let j=O?O.scopes:y?[IT]:M?[...sOo(),M]:sOo();
  if(H.searchParams.append("scope",j.join(" ")),
     H.searchParams.append("code_challenge",e),
     H.searchParams.append("code_challenge_method","S256"),
     H.searchParams.append("state",n),
     b)H.searchParams.append("orgUUID",b);
  if(w)H.searchParams.append("login_hint",w);
  if(R)H.searchParams.append("login_method",R);
  return H.toString()}
```

Production endpoint table (`nn()` resolves to this object when not overridden by `CLAUDE_CODE_CUSTOM_OAUTH_URL` /
`CLAUDE_CODE_OAUTH_CLIENT_ID`):

```js
{BASE_API_URL:"https://api.anthropic.com",
 CONSOLE_AUTHORIZE_URL:"https://platform.claude.com/oauth/authorize",
 CLAUDE_AI_AUTHORIZE_URL:"https://claude.com/cai/oauth/authorize",
 CLAUDE_AI_ORIGIN:"https://claude.ai",
 TOKEN_URL:"https://platform.claude.com/v1/oauth/token",
 API_KEY_URL:"https://api.anthropic.com/api/oauth/claude_cli/create_api_key",
 ROLES_URL:"https://api.anthropic.com/api/oauth/claude_cli/roles",
 CLAUDEAI_SUCCESS_URL:"https://platform.claude.com/oauth/code/success?app=claude-code",
 MANUAL_REDIRECT_URL:"https://platform.claude.com/oauth/code/callback",
 CLIENT_ID:"9d1c250a-e61b-44d9-88ed-5944d1962f5e", ...}
```

**Recorded values for pooltime (subscription sign-in):**

| Item | Value |
|---|---|
| Authorize host (`loginWithClaudeAi` true, the default) | `https://claude.com/cai/oauth/authorize` |
| Authorize host (`loginWithClaudeAi` false, `--console`) | `https://platform.claude.com/oauth/authorize` (not used by pooltime) |
| How `loginWithClaudeAi` is chosen | `claude auth login`: `H = forcedMethod ? forcedMethod==="claudeai" : !consoleFlag`; default is claude.ai. CLI help: `--claudeai` "Use Claude subscription (default)", `--console` "Use Anthropic Console (API usage billing)", `--email <email>` "Pre-populate email address on the login page" (maps to `login_hint`), `--sso` "Force SSO login flow" (maps to `login_method=sso`) |
| `client_id` | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` (matches bro `oauthClientId`) |
| Token URL | `https://platform.claude.com/v1/oauth/token` (matches bro `oauthTokenUrl`) |
| Manual redirect URL | `https://platform.claude.com/oauth/code/callback` |
| Automatic redirect URL | `http://localhost:<port>/callback` (note `localhost`, not `127.0.0.1`; the listener binds `127.0.0.1` on an ephemeral port, `listen(0,"127.0.0.1")`, and only serves path `/callback`) |

**Authorize query parameters, in the order Claude Code appends them:**

1. `code=true`
2. `client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e`
3. `response_type=code`
4. `redirect_uri=<localhost callback or manual redirect, URL-encoded by URLSearchParams>`
5. `scope=<space-separated scopes>`
6. `code_challenge=<S256 of verifier, base64url>`
7. `code_challenge_method=S256`
8. `state=<random>`
9. `orgUUID=<uuid>` (only when set, e.g. `forceLoginOrgUUID` setting)
10. `login_hint=<email>` (only when set)
11. `login_method=<value>` (only when set; Claude Code only ever sets `"sso"`)

**Scope list for the claude.ai login.** Derived from the constants in the same chunk
(`IT="user:inference"`, `_4="user:profile"`, `DVn="user:sessions:claude_code"`, `c="org:create_api_key"`,
`Ajt="user:plugins"`, `PLUGINS_SCOPE_REGISTERED:!0` in prod, dedupe via `[...new Set(e)]`):

```js
var c="org:create_api_key", n=[c,_4], r=[_4,IT,DVn,"user:mcp_servers","user:file_upload"];
function Rjt(){let t=nn(),e=[...r]; if(t.PLUGINS_SCOPE_REGISTERED)e.push(Ajt); return e}
function sOo(){return L([...n,...Rjt()])}   // authorize scope
```

Authorize `scope` (default login), in order:

```
org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins
```

- `inferenceOnly` (the `setup-token` path) sends only `user:inference`.
- Refresh default scope (`Rjt()`, no `org:create_api_key`): `user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload user:plugins`.
- There are further optional scopes (`user:projects:read`, `user:projects:write`, `user:design:*`) used only by
  separate scope-expansion flows; pooltime does not need them.
- The scope set is a moving target between Claude Code versions (the `user:plugins` scope is gated by a registered flag, and
  scopes were added over time). Task 24 should keep the scope string in config with the above as the default, and treat an
  `invalid_scope` response on refresh as "retry once with the scopes stored on the credential" (what Claude Code does; see
  "Refresh error classes").

**PKCE / state.** `code_challenge_method=S256`; verifier is random, challenge = base64url(SHA-256(verifier)); `state` is a
random string. The callback listener rejects a mismatched `state` (`rejectOnStateMismatch`) and also reports an `error`
callback param (`error`, `error_description`, `error_uri`); an `error_description` of `account_on_hold` is a distinct case.

**Manual (paste-code) mode.** When the localhost listener cannot start (or the user is on another host), the user opens the
manual URL (`redirect_uri=https://platform.claude.com/oauth/code/callback`) and pastes `"<code>#<state>"` (Claude Code
splits the pasted line on `#` into `authorizationCode` and `state`). Claude Code builds BOTH URLs (manual `isManual:true`
and automatic `isManual:false`) from the same verifier/state, shows/opens the automatic one, and chooses which
`redirect_uri` to send in the token exchange based on whether the automatic callback actually delivered the code
(`useManualRedirect: !p`, where `p` = a pending localhost response exists). **The token-exchange `redirect_uri` must equal the
one that was in the authorize URL the user actually used.**

#### Token exchange (`HPs`)

```js
let w={grant_type:"authorization_code",code:e,
       redirect_uri:h?nn().MANUAL_REDIRECT_URL:`http://localhost:${s}/callback`,
       client_id:b??nn().CLIENT_ID,code_verifier:r,state:n};
if(y!==void 0)w.expires_in=y;
await Wt.post(nn().TOKEN_URL,w,{headers:{"Content-Type":"application/json"},timeout:30000,...});
// non-200 => error; 401 => "Authentication failed: Invalid authorization code"
```

- Request: `POST https://platform.claude.com/v1/oauth/token`, `Content-Type: application/json`, JSON body fields
  `grant_type="authorization_code"`, `code`, `redirect_uri`, `client_id`, `code_verifier`, `state` (plus optional
  `expires_in`, used for 1-year `setup-token`: `31536000`). Timeout 30 s. No `User-Agent` is set explicitly on this call
  (axios default).
- Response (read by `formatTokens`): `access_token`, `refresh_token`, `expires_in` (seconds), `refresh_token_expires_in`
  (optional seconds), `scope` (space-separated), `account: {uuid, email_address}`, `organization: {uuid, name}`,
  optional `workspace: {id, name}`. So the **token response itself carries the account email and org uuid**, which is
  enough for pooltime's identity check without a profile call (but the profile call is cheap, see below).

#### Refresh (`Dfe`)

```js
M={grant_type:"refresh_token",refresh_token:e,client_id:s??nn().CLIENT_ID,scope:O.join(" ")};
if(r!==void 0)M.expires_in=r;
let U=await Wt.post(nn().TOKEN_URL,M,{headers:{"Content-Type":"application/json"},timeout:30000,signal:y,...});
if(U.status!==200)throw Error(`Token refresh failed: ${U.statusText}`);
let{access_token:j,refresh_token:z=e,expires_in:te}=U.data, Y=Date.now()+te*1000, Z=oEo(H.refresh_token_expires_in,!1), ne=yOr(H.scope);
```

- Request JSON: `{"grant_type":"refresh_token","refresh_token":"<rt>","client_id":"9d1c250a-...","scope":"<space-separated>"}`
  (+ optional `expires_in`). `Content-Type: application/json`. **Timeout 30 000 ms** (bro uses 20 s,
  `TOKEN_REFRESH_TIMEOUT_MS`, and omits `scope`; omitting `scope` works in production today, so `scope` is optional, but
  sending the same scope set Claude Code sends is the safest match).
- Response fields consumed: `access_token`, `refresh_token` (**defaults to the old token if absent**, see S2 static
  evidence), `expires_in`, `refresh_token_expires_in`, `scope`, and `account.{uuid,email_address}` / `organization.uuid`.
- After a successful refresh Claude Code also GETs `/api/oauth/profile` (unless it already has a fresh cached profile or
  `skipProfileFetch`).
- Revoke (used on logout / lost races): `POST {TOKEN_URL}/revoke` JSON `{token, token_type_hint, client_id}`, timeout
  5 000 ms. Pooltime should call this when an account is removed, if it wants to be a good citizen.

#### Refresh error classes

`Dfe` failure handling (`Ji` parses `{error, error_description}`; `error` may be a string or `{type: "..."}`):

| Class (Claude Code metric) | Condition |
|---|---|
| `oauth_refresh_account_on_hold` | HTTP 400/401/403 and the body matches an "account on hold" schema (marker string `account_on_hold`, the same value the authorize callback reports as `error_description`; carries an `error_uri`, default `https://claude.ai/restricted`). The exact body schema was not extracted; match on the `account_on_hold` marker |
| `oauth_refresh_invalid_grant` | HTTP 400/401 and `error == "invalid_grant"` (and not on-hold). Known `error_description` values: `Refresh token expired`, `Refresh token not found or invalid`, `No organization associated with this token`, `No account associated with this token`, `account_on_hold` |
| `invalid_scope` (retry class) | HTTP 400 and `error == "invalid_scope"`: Claude Code retries once with the scopes stored on the credential |
| terminal client errors | HTTP 400 and `error` in `{invalid_client, invalid_scope, unauthorized_client}` |
| `oauth_refresh_request_failed` | anything else (network, timeout, 5xx) |

Mapping for pooltime: `invalid_grant` → `needs_login` (terminal, stop retrying that token); `account_on_hold` →
`access_denied` with the restricted-page link; the rest are transient (`network`, backoff). bro's matcher also accepts the
nested `{"error":{"type":"invalid_grant"}}` envelope; keep both shapes.

#### Profile (`/api/oauth/profile`)

```js
let n=`${nn().BASE_API_URL}/api/oauth/profile`;   // https://api.anthropic.com/api/oauth/profile
await Wt.get(n,{headers:{"User-Agent":ui(),Authorization:`Bearer ${e}`,"Content-Type":"application/json","Cache-Control":"no-cache"},timeout:1e4,...})
```

- `GET https://api.anthropic.com/api/oauth/profile`; headers `User-Agent: claude-code/<VERSION>`,
  `Authorization: Bearer <access_token>`, `Content-Type: application/json`, `Cache-Control: no-cache`; timeout 10 s. (No
  `anthropic-beta` header here; the usage endpoint needs `anthropic-beta: oauth-2025-04-20`, const `Qc`.) Needs the
  `user:profile` scope.
- Response validated with a passthrough schema requiring only `account: {uuid, email}` and `organization: {uuid}`. Other
  fields read by name: `account.display_name`, `account.full_name`, `account.created_at`;
  `organization.organization_type` (maps `claude_max→max`, `claude_pro→pro`, `claude_enterprise→enterprise`,
  `claude_team→team`), `organization.rate_limit_tier`, `organization.seat_tier`, `organization.has_extra_usage_enabled`,
  `organization.billing_type`, `organization.cc_onboarding_flags`, `organization.claude_code_trial_ends_at`,
  `organization.claude_code_trial_duration_days`, `organization.subscription_created_at`, `organization.name`.
- Note the field is `account.email` in the profile response but `account.email_address` in the token response.
- The response `request-id` header is used as an "Anthropic-stamped" signal (a 2xx without it is not trusted).

#### User-Agent / version

`ui()` returns `` `claude-code/${VERSION}` `` with `VERSION:"2.1.319"` → **UA string `claude-code/2.1.319`**. This is the UA
Claude Code sends to the profile, roles, create_api_key and (per bro lessons) usage endpoints. bro's configured
`usageUserAgent` is stale (`claude-code/2.1.207`), so the UA default must be updated, and kept configurable.

### S1 live (Step 2): login_hint behavior — **pending, needs user**

- **Question:** does claude.ai honor `login_hint` (pre-select/pre-fill the account) while the browser is already signed in
  as a different user? If not, the spec's mismatch fallback (§"Re-login flow": refuse, explain, retry) is the primary path.
- **Method (later):** after Tasks 24 and 30 land, the user runs
  `pooltime accounts login --spike-login-hint <email>` while signed into claude.ai as a different account, and records
  (a) whether the email field is pre-filled / the right account chosen, (b) what happens when the signed-in user differs.
- **Status:** pending — needs user; run `pooltime accounts login --spike-login-hint <email>` after Tasks 24+30. Static
  evidence only confirms the parameter is emitted and that `claude auth login --email` documents it as "Pre-populate email
  address on the login page"; nothing static says what claude.ai does with a different signed-in session.

### S1: tasks this changes

- **Task 3 (config / UA string):** default UA `claude-code/2.1.319`; make it configurable (`POOLTIME_...` / config) since it
  goes stale; also default OAuth `client_id`, token URL, authorize URL, manual redirect URL, profile URL are config values
  with the constants above.
- **Task 15 (OAuth refresh):** JSON body `grant_type`, `refresh_token`, `client_id` (+ `scope` recommended), JSON content
  type, 30 s timeout, parse `access_token`/`refresh_token`/`expires_in`/`refresh_token_expires_in`/`scope`, keep the old
  refresh token if the response omits one, error classification per the table above (400/401 `invalid_grant`, nested error
  envelope, `account_on_hold`).
- **Task 24 (Claude sign-in):** authorize host `claude.com/cai/oauth/authorize`, exact param order, scope string, S256 PKCE,
  `state` check, localhost `/callback` redirect (ephemeral port) vs manual redirect with `code#state` paste, token exchange
  JSON with `redirect_uri` matching the URL used; capture `account.email_address` + `organization.uuid` from the token
  response for the identity check; `login_hint` param (value pending live check, include it anyway).
- **Task 30 (`accounts login --spike-login-hint`):** carries the pending S1-live check.
- **Identity/profile tasks (email capture, plan type):** profile endpoint path/headers/fields above; fetch is optional because
  the token response already carries email and org uuid.

---

## S2 — Does Anthropic rotate the refresh token on refresh?

**Question.** When bro refreshes an account, does the stored refresh token change (rotation)? If yes, two processes that
refresh the same account (bro and pooltime during the shadow run) would invalidate each other, so the shadow run must keep
account sets disjoint (spec §16 already requires disjointness; this spike only confirms the reason).

### Method

- Read-only fingerprinting by the controller (not this task), no credential file was read by the implementer. For each
  `~/.claude-max-pool/accounts/*/.credentials.json`: record `sha256(refreshToken)` truncated to 16 hex chars, plus the file
  mtime. The script was `python3 -I <job tmp>/s2.py` (re-run it unchanged to re-check; it never prints a token).
- Re-check at or after **2026-11-07T23:15** (about 6 h later), or earlier if bro's next refresh has bumped an mtime.
- Interpretation rules:
  - mtime advanced and fingerprint changed → refresh token **rotates**.
  - mtime advanced and fingerprint identical → **no rotation** (bro rewrites the file with `refresh_token ?? current` on every
    refresh: `pool/src/upstream/oauth-token.ts`, `mgr.updateOAuthCreds`, so an advanced mtime proves a write happened).
  - mtime not advanced → no information for that account (compare only accounts whose mtime advanced). The accounts whose
    mtime is old (`haynet-chris` 2026-11-01, `haynet-christopherjenkins` 2026-10-25) look dormant or dead and are not
    usable as evidence.

### Baseline (captured 2026-11-07T17:15:14 local)

| account | sha256(refreshToken)[:16] | credentials mtime (local) |
|---|---|---|
| haynet-chris | `9335cde77db2558b` | 2026-11-01T05:40:27 |
| haynet-chrisjenkins | `3160d89a7053e60d` | 2026-11-07T12:31:22 |
| haynet-chrisjenx | `de34fccdda07f426` | 2026-11-07T12:32:23 |
| haynet-christopherjenkins | `8fcd58f8807b29e2` | 2026-10-25T16:01:37 |
| haynet-cjenkins | `9c8846b7c12026da` | 2026-11-07T12:36:23 |
| haynet-cjenks | `530cd163797226fe` | 2026-11-07T12:39:23 |
| personal-adbrchfam | `2b8807966162f032` | 2026-11-07T11:25:24 |
| personal-chrismarkjenkins | `cf878cf68b0d4c04` | 2026-11-07T12:27:23 |

Observation: six accounts were rewritten between 11:25 and 12:39 today (11:25, 12:27, 12:31, 12:32, 12:36, 12:39),
consistent with bro refreshing each account when its access token neared expiry; their next refresh should land roughly one
access-token lifetime later (plausibly after the recheck time; if no mtime has advanced by 23:15, wait longer rather than
refresh anything by hand).

### Static evidence from the Claude Code 2.1.319 binary (inconclusive on its own, strongly suggestive)

1. Refresh persists whatever `refresh_token` the response carries and falls back to the old one only if absent:
   `let{access_token:j,refresh_token:z=e,expires_in:te}=U.data` (`Dfe`), then `ako` writes
   `refreshToken:r.refreshToken` into the credentials store.
2. The write is a **compare-and-swap on the old refresh token**: `ako` mutates only if
   `j.claudeAiOauth.refreshToken === postedRefreshToken` (or empty), retries 3 times, and otherwise "adopts the sibling's
   newer write" (`tengu_oauth_refresh_save_adopted_newer_write`). It also takes a cross-process lock
   (`.oauth_refresh.lock`, owner file `.oauth_refresh.lock.owner`, "another process is refreshing"). This machinery only makes
   sense if refresh tokens are single-use.
3. When a refresh result is raced out (CAS lost) Claude Code **revokes the newly minted refresh token**:
   `if(f.raced){await Cw(n.refreshToken,o.clientId); ...}` and
   `if(n.refreshToken&&n.refreshToken!==u)await Cw(n.refreshToken,o.clientId)` (`Cw` calls `/revoke`). Revoking a token
   that differs from the one sent implies the server hands out a **new** refresh token on refresh.
4. A "known dead refresh token" set is kept (`cs.has(se.refreshToken)` → `known_dead_refresh_token`), i.e. after an
   `invalid_grant` that token is never tried again.
5. bro's own header comment says "a rotating refresh_token is never spent twice in parallel" and its refresh falls back to
   `current.refreshToken` if the response omits one, the same defensive shape.

### Conclusion

**Pending re-check ≥ 2026-11-07T23:15.** Working assumption until then: refresh tokens **do rotate**, so treat every refresh
as consuming the old token. Design consequences that hold regardless of the final answer:

- Persist the new refresh token atomically (temp + fsync + rename) **before** using the new access token, and use
  compare-and-swap semantics against the old value (Claude Code does exactly this), serialising refreshes per account
  in-process (bro's per-account in-flight lock) and across processes when the credential file is shared.
- Never let bro and pooltime refresh the same account concurrently: shadow-run disjointness (spec §16) is **mandatory**
  (it already is); do not relax it even if S2 returns "no rotation", because Anthropic can change this server side.
- A pooltime-imported copy of a bro account must be treated as a fork of the refresh-token chain: after the first refresh
  by either side, the other side's copy may be dead (`invalid_grant`). Import must therefore either move the account
  (stop bro using it) or run a fresh pooltime login for it.

### S2: tasks this changes

- **Task 15 (OAuth refresh):** store returned `refresh_token`; CAS write; per-account refresh lock; fall back to the old token
  only if the response omits one; classify `invalid_grant` as terminal.
- **Import task(s) (bro import) and shadow-run tasks:** disjoint account sets, no shared refresh chain; document that
  imported accounts are moved, not mirrored.
- **Credential store task(s):** atomic write ordering as above.
- **Re-check (Task 2 follow-up, controller):** re-run the S2 script at or after 2026-11-07T23:15, fill in the "after" table,
  update the conclusion line above, and amend this doc.

---

## Cross-check with bro (read-only)

| Value | Claude Code 2.1.319 binary | bro `pool/src/config.ts` |
|---|---|---|
| client_id | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` (`oauthClientId`, line 171) |
| token URL | `https://platform.claude.com/v1/oauth/token` | same (`oauthTokenUrl`, line 170) |
| refresh timeout | 30 000 ms | 20 000 ms (`tokenRefreshTimeoutMs`, line 173) |
| refresh body | `grant_type, refresh_token, client_id, scope` | `grant_type, refresh_token, client_id` (no scope) |
| UA | `claude-code/2.1.319` | `claude-code/2.1.207` (stale, line 198) |
