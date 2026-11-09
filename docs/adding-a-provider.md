# Adding a provider (contributors)

A provider is an upstream subscription service (today: `anthropic` for Claude, `openai` for
ChatGPT/Codex). The inbound side is always the Anthropic Messages API; a provider translates to and
from its own wire format. Routing, health, alerts, analytics, the journal, the dashboard and MCP
contain no provider-specific branches beyond data, and a new provider should keep it that way.

Read [Architecture](architecture.md) first, and use `src/providers/openai/` (a translating provider)
as the worked example.

## The trait

`src/providers/mod.rs`:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> ProviderId;
    /// A usable access token, refreshing when `force` or near expiry.
    async fn access_token(&self, acct: &AccountMeta, force: bool) -> Result<String, RefreshErrorKind>;
    /// One upstream attempt: Done (committed response), Terminal, or Retry (try the next account).
    async fn attempt(&self, ctx: AttemptCtx<'_>, req: &InboundRequest, state: &StateBook) -> AttemptOutcome;
    /// Rate-limit / usage snapshot (utilization as a 0-1 fraction).
    async fn poll_usage(&self, acct: &AccountMeta) -> Result<RateLimitSnapshot, UsageError>;
    /// The routes (models) this provider serves for an account.
    async fn catalog(&self, acct: &AccountMeta) -> anyhow::Result<Vec<RouteDef>>;
    /// Optional live `/v1/models` rows (default: none).
    async fn live_models(&self) -> anyhow::Result<Vec<serde_json::Value>>;
    async fn refresh_live_models(&self) -> anyhow::Result<Vec<serde_json::Value>>;
}
```

Sign-in is a separate trait, `signin::SignInFlow::start(account, hint, mode)`, which must support
both `SignInMode::Callback` (localhost redirect) and `SignInMode::PasteCode` (headless), and must
verify the signed-in identity against the account's baseline before saving.

## Files to add or touch

| File | What |
|---|---|
| `src/providers/mod.rs` | new `ProviderId` variant, its `as_str` / `FromStr` word (this is also the `--provider` value) |
| `src/providers/<name>/` | the `Provider` impl: proxy/attempt, request and stream translation, OAuth refresh with death detection, usage parsing, catalog |
| `src/signin/<name>.rs` | the `SignInFlow` impl |
| `src/config.rs` | a `[<name>]` section for URLs, client id, scopes, UA, with defaults and tests |
| `src/server/daemon.rs` | construct the provider and add it to the `providers` map |
| `src/server/mgmt.rs`, `src/cli/accounts.rs` | pick the sign-in flow for the new `ProviderId` (daemon and in-process paths) |
| `src/routing/reserve.rs`, `src/server/inference.rs`, `src/config.rs` `[routing]` | per-provider in-flight cap |
| `src/routing/sessions.rs` | add it to `PROVIDERS` (sticky sessions are per provider) |
| `src/models/` | default routes and any mapping rules |
| `src/server/fallback.rs` | cross-provider fallback is written for exactly two providers today (`other()`); generalise it |
| `tests/support/fake_<name>.rs`, `tests/it/proxy_<name>.rs` (+ `mod` in `tests/it/main.rs`) | an in-process fake upstream and integration tests |

The compiler finds the rest: every `match` on `ProviderId` is exhaustive.

## Rules a provider must follow

- **Normalize** every utilization to a 0–1 fraction.
- **Classify** every failure into `FailReason` / `RefreshErrorKind` / `TransportKind`; never store
  or log upstream error messages, bodies or tokens.
- **Refresh refusals** (`invalid_grant`, revoked/reused token) mean `needs_login`; network and 5xx
  failures never do. Never sideline an account on a clock alone.
- **Rotate safely**: one refresh in flight per account, under the account's file lock; re-read the
  stored credentials after a timeout before retrying; write credentials atomically.
- **Streams**: commit the response at a defined point (Anthropic: `message_start`; Codex: the first
  content frame, so a created-then-failed stream can still fail over) with pre-commit buffering
  bounded by 64 KiB and the idle watchdog; emit keep-alives from a timer that does not await
  upstream; treat a client disconnect as `client_cancelled` (no account error).
- Use `crate::upstream::Upstream` (idle watchdog, no decompression); no total request timeout.
- **Tests**: no network. Fake upstreams on `127.0.0.1:0` only.

Then run the gates in [`AGENTS.md`](../AGENTS.md).
