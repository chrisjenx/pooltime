# Differential harness (bro vs pooltime)

Plays scripted upstream scenarios against **bro-cli** (the TypeScript pool pooltime replaces) and
**pooltime**, side by side, and compares what a client sees and what each daemon did. Spec §16
"Differential harness"; results in `docs/harness-report.md`.

```sh
cargo run -p harness -- --bro <bro-cli checkout> --scenarios all
```

Exit status: `0` when every scenario ran and every difference is allowlisted, `1` when a scenario
has an unexplained difference or failed to run, `2` on a setup error.

Options: `--scenarios a,b` (subset), `--verbose` (print every field of both sides),
`--pooltime <bin>` (skip the cargo build of `target/debug/pooltime`), `--bun <path>`,
`--allowlist <toml>`, `--report <md>`.

The harness is **not** part of `cargo test` or CI: it needs bun and a bro checkout. Its unit
tests (SSE reduction, comparison, allowlist validation) do run under `cargo test`.

## Requirements

- `bun` (on `PATH`, else `/opt/homebrew/bin/bun`).
- A bro-cli checkout that honors the Codex URL overrides (`CODEX_API_BASE_URL`,
  `OPENAI_OAUTH_TOKEN_URL`; branch `feat/codex-env-overrides` of `chrisjenx/bro-cli`). bro already
  honors `ANTHROPIC_API_BASE_URL`, `CLAUDE_OAUTH_TOKEN_URL` and `CODEX_USAGE_URL`.
- `bun install` run once in `<bro>/pool` (the harness checks for `pool/node_modules`; nothing is
  installed into this repo).

## What a scenario run does

For each scenario (sequentially, fresh state every time):

1. Starts two pairs of fake upstreams on `127.0.0.1:0` (Anthropic + Codex, one pair per daemon),
   scripted identically. The fake server is `tests/support/fake_anthropic.rs`, included by path in
   `src/fake.rs` (no copy, no feature on the `pooltime` crate); the Codex event builders live in
   `src/fake.rs` because `tests/support/fake_openai.rs` depends on the tests' in-process fixture.
2. Seeds synthetic accounts with **fake** tokens (`fake-a-<name>`, `fake-o-<name>`,
   `fake-r-<name>`) - bro: `accounts/<name>/{.credentials.json,openai-auth.json,routing.json}`
   (+ `models.json` when the scenario enables mappings); pooltime: `accounts/<name>/{account.toml,
   credentials.json}` (+ `models.toml`) and a `config.toml` pointing every upstream URL at the fakes.
   Usage polling is off in both (`CLAUDE_USAGE_REFRESH=0`, `usage_refresh = false`); overload
   backoff is shortened identically in both (3 retries, 20 ms base, 80 ms cap).
3. Boots bro (`bun run src/index.ts serve` in `<bro>/pool`) and pooltime (`serve --i-stopped-bro`).
4. Sends the scenario's client steps to both concurrently, then reads account state
   (bro `GET /api/status`, pooltime `GET /api/v1/status` with the management key) and what each
   fake recorded, and compares.

### Safety

Both children run with a cleared environment rebuilt from an explicit list: `HOME`/`TMPDIR` (and
bro's `CLAUDE_POOL_DIR`/`CLAUDE_CONFIG_DIR`, pooltime's `POOLTIME_HOME`) inside a per-scenario temp
dir under `$TMPDIR`, `HOST=127.0.0.1`, a fresh port (never 3456 - the live bro - nor 3460), and
upstream URLs on 127.0.0.1 only. The harness asserts the sandbox home is not, and is not inside,
the real home's `.claude*`/`.bro`/`.claude-max-pool`/`.pooltime`. `PATH` starts with a shim
`security` that records the call and fails, so bro can never read the macOS Keychain; a scenario
fails if the shim was invoked. Tokens are never printed: observations carry account names only,
error *messages* are dropped (only error types are compared), and daemon log tails shown on a boot
failure mask anything token-shaped. The sandbox is deleted after each scenario.

## What is compared

Each daemon's run is flattened to `field -> value`:

| Field | Value |
|---|---|
| `step<i>.status` | HTTP status |
| `step<i>.header.<h>` | `content-type` (no parameters), `retry-after`, `x-pool-account`, `x-pool-route`, `x-pool-upstream-rejected`, `x-pool-retryable-transport`, `request-id` |
| `step<i>.sse` | semantic SSE sequence: `message_start:<model>:usage=<in>/<out>[,cache…]`, `block_start:<idx>:<type>[:name][:id]`, `delta:<idx>:<type>:<text>` (consecutive deltas of one block coalesced, so chunking does not matter), `block_stop:<idx>`, `message_delta:<stop_reason>:usage=…`, `error:<type>`; pings dropped |
| `step<i>.pings` | whether any `ping` arrived (`some`/`none`); count and timing are not compared |
| `step<i>.end` | how the stream ended: `clean`, `reset` (transport error), `client-hangup` |
| `step<i>.json` | projection of a JSON body: error type; message model, stop reason, usage and content (text, thinking, tool_use name/id/input); model list ids/owners/context windows |
| `account.<name>.status` | `ready \| needs_login \| rate_limited \| billing \| access_denied` (bro's account view mapped onto pooltime's labels) |
| `account.<name>.cooldown` | remaining cooldown in whole minutes (`-` when none) |
| `upstream.inference` | `messages@<account>` / `responses@<account>` per upstream inference request, in arrival order across both fakes |
| `upstream.refresh` | refresh grants per account |
| `upstream.detail<n>` | scenario-specific projection of upstream request bodies (e.g. Codex `input` item kinds) |
| `info.*` | step timings; printed with `--verbose`, never compared |

Scenarios with interchangeable accounts (`sticky_session`) rename accounts by first use (`A`, `B`).

## Allowlist and triage

Every difference is triaged as either

- **a pooltime bug** - fixed in pooltime with a regression test in the relevant `tests/*.rs`, and
  recorded in `harness/fixed.toml` (`[[fixed]]` with `scenario`, `summary`, `commit`, `test`; the
  report lists them), or
- **intended** - an entry in `harness/allowlist.toml`: anchored regexes for `scenario`, `field`,
  `bro` and `pooltime` values (`<absent>` = missing), a `reason`, and a `source` that must cite
  an INV item (e.g. §10.x), spec §16, a `docs/parity.md` decision or a ledger ruling (the harness refuses to
  load an entry without one). Allowlisting a pooltime bug to get green is not allowed.

The report also lists allowlist entries that matched nothing (stale, or scenario not selected).

## Adding a scenario

Add a `Scenario` in `src/scenarios/{anthropic,codex,routing}.rs`: the accounts to seed, a
`script` that routes the fakes (`by_account` / `by_refresh_account` give per-account hit
counters), and the client `Step`s. Run it with `--scenarios <name> --verbose`, then triage any
difference as above.
