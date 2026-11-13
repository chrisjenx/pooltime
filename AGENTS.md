# AGENTS.md

Guidance for coding agents working **on** this repository. (To install and set up pooltime as a
user, read [`llms.txt`](llms.txt) instead.)

pooltime is a Rust daemon + CLI that pools Claude Max and ChatGPT/Codex subscription accounts behind
one local Anthropic-compatible endpoint for Claude Code. It replaces the pool in bro-cli.

## Build and test

```sh
cargo build                                   # debug binary at target/debug/pooltime
cargo test                                    # unit + integration tests (also the harness crate's unit tests)
cargo test --test it proxy_anthropic::        # one integration-test module (tests/it/proxy_anthropic.rs)
cargo build --profile debugging               # full debug info for a debugger (dev builds keep line tables only)
cargo run -q -p pooltime --bin pooltime -- --help
```

Rust stable, edition 2021. Workspace members: the `pooltime` crate (`src/`, lib + bin) and
`harness/`.

## Gates (all green before every commit)

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
scripts/check-docs.sh                         # runs ```sh run doc blocks, checks every documented subcommand exists
```

When you change proxy, streaming, routing or account-state behaviour, also run the differential
harness against bro (needs `bun` and a bro-cli checkout; not part of CI), see `harness/README.md`:

```sh
cargo run -p harness -- --bro <bro-cli checkout> --scenarios all
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, test and the docs check on macOS and Linux
(arm64 and x86_64).

## Architecture map

See [docs/architecture.md](docs/architecture.md) for the module map and request flow. In short:

- `src/cli/`: one file per command; thin clients of the management API (`cli/client.rs`).
- `src/server/`: axum daemon: `/v1/messages` (`inference`, `fallback`, `classifier`),
  `/v1/messages/count_tokens` (`count_tokens`), `/v1/models`,
  `/health`, `/api/v1/*` (`mgmt`, `journal_api`, `analytics_api`), dashboard, drain.
- `src/providers/`: the `Provider` trait; `anthropic/` passthrough, `openai/` Codex translation.
- `src/signin/`: OAuth PKCE flows (callback and paste-code).
- `src/accounts/`, `src/health/`, `src/routing/`, `src/models/`: account state, display states and
  alerts, account choice, model table.
- `src/claude_code/`: Claude Code profile connect/disconnect, pins, status line, MCP registration.
- `src/store/`: SQLite journal, timeline, usage history, retention.
- `src/supervisor/`: `serve --supervise`, worker generations and seamless restarts (the
  service's main process); `src/server/handoff.rs` is a worker's side.
- `src/mcp/`, `src/service/`, `src/import/`, `src/analytics/`.
- `src/config.rs`, `src/fsutil.rs`, `src/upstream.rs`, `src/sse.rs`, `src/logging.rs`, `src/errors.rs`.
- `web/` dashboard (vanilla JS, embedded); `tests/it/` integration tests, one module per file,
  linked into a single test binary (`tests/it/main.rs`), with fake upstreams in `tests/support/`;
  `harness/` differential harness.

Design docs: `docs/specs/2026-11-07-pooltime-design.md` (§16 overrides earlier sections) and
`docs/specs/2026-11-07-pooltime-parity-inventory.md`; parity map in `docs/parity.md`.

## Invariants (do not break)

- Platforms: macOS arm64/x86_64, Linux x86_64/arm64 (musl release builds). No Windows code paths.
- Binary/command `pooltime`. Data dir `~/.pooltime` (override `POOLTIME_HOME`), mode 0700;
  credentials, DB and config 0600.
- Env overrides use the `POOLTIME_` prefix only; generic `PORT`/`HOST` are ignored.
- Default bind `127.0.0.1:3460` (bro uses 3456; both must run side by side).
- Utilization is a **0–1 fraction** internally; every parser normalizes.
- Never log or persist prompt/response bodies, tokens, or upstream error messages: only error type,
  classified reason and upstream `request-id`. The exception is time-boxed debug mode, which logs
  redacted headers only, never bodies. No credential ever reaches a log, the journal, a
  notification or CLI output.
- Anthropic SSE bytes are forwarded **unchanged** after commit; request bytes are forwarded
  unchanged unless Codex-thinking stripping removed something.
- Pre-commit buffering is capped at 64 KiB and by the idle watchdog. `message_start` commits an
  Anthropic stream (`ping` never does); a Codex stream commits on its first content frame, so a
  created-then-failed upstream can still fail over.
- Upstream HTTP client: no total request timeout (header-wait + per-read idle + per-attempt
  deadline instead), automatic decompression off, and upstream `content-length` /
  `content-encoding` / `transfer-encoding` are never forwarded on streamed responses.
- Unknown inbound paths get an Anthropic-shaped 404 JSON and are journaled/logged.
- The session `metadata.user_id` is stored only as a hash, never raw.
- A client disconnect never records an account error and never panics or crashes the daemon.
  No state mutation may live after an `.await` in a cancellable handler future: use `Drop` guards
  or the spawned pump task.
- Every persisted file is written atomically (temp + fsync + rename, parent dir fsync).
- Claude Code profile file rule: `~/.claude` ↔ `~/.claude.json`; any other dir `D` ↔
  `D/.claude.json`. Profile writes: only when bytes differ, key order and unknown fields kept,
  re-check before rename and retry if Claude Code changed the file.
- One token refresh in flight per account, plus a cross-process `flock`; re-read credentials after
  a refresh timeout before retrying.
- Stored refresh-token expiry = `min(old, new)`; with no server value and no old one it is
  **unknown** (a guess may be displayed, never used to sideline).
- Never sideline an account on a clock alone: an unknown or passed expiry is not `needs_login`
  until a probe refresh is refused (`invalid_grant` and similar). Network/5xx never means
  `needs_login`.
- Sign-in flows run inside the daemon via the management API (the CLI is a thin client; it runs
  them in-process only when no daemon answers). Callback listener: 127.0.0.1, `state` checked,
  one-shot, 5-minute expiry.
- `connect` requires a healthy pool whose `/health` passes the identity check; `service uninstall`
  disconnects every profile first.
- Header-less 429 from Anthropic is passed through (`x-pool-upstream-rejected: 1`): no bench,
  no failover, no cross-provider hop.
- Management API and MCP are local-key protected; `Host`/`Origin` must be `127.0.0.1:<port>` or
  `localhost:<port>`; no CORS. The MCP token is never the management key. `/v1/*` refuses any
  `Origin` and (on a loopback bind) a non-loopback `Host`; the proxy key `connect` generates is
  enforced at once.
- Import is read-only on bro's files and Keychain items.
- Tests: `cargo test`; **no network** (fake upstreams on `127.0.0.1:0`, wiremock); tempdirs only,
  never the real `$HOME`, `~/.claude*`, `~/.bro` or `~/.claude-max-pool`.
- Integration tests share one process: add a file as a `mod` in `tests/it/main.rs`; never mutate
  the process environment (`std::env::set_var`), pass env to child `Command`s instead; spawn
  children and pick free ports only through `tests/support/procs.rs`.
- Never modify the developer's real Claude Code profiles (`~/.claude`, `~/.claude.json`,
  `~/.claude-*`) or bro's state while developing or testing; set `HOME`/`POOLTIME_HOME` to a temp
  dir for any manual run of `connect`, `setup` or `import`.
- Tests that capture tracing output with `with_default` must first call
  `crate::test_support::ensure_callsites_stay_enabled()`.
- MIT license. Conventional commits (`feat(scope): ...`, `fix(...)`, `docs: ...`), one commit per
  change; never commit tokens, credentials or real account emails.

## Releasing

Merge a PR that bumps `version` in the root `Cargo.toml` (and `Cargo.lock`) and adds the
`CHANGELOG.md` entry. Nothing else is manual: on the push to `main`, `.github/workflows/release.yml`
sees a version whose `v<version>` tag does not exist, builds the four targets, creates the GitHub
release (which creates the tag at that commit) and updates `Formula/pooltime.rb` in
`chrisjenx/homebrew-tap`. Pushes to `main` that leave the version alone stop after a one-job check.

- One-time setup: the repository secret `HOMEBREW_TAP_DEPLOY_KEY`, the private half of an SSH deploy
  key with write access to `chrisjenx/homebrew-tap`. Without it the tap job warns and skips; update
  the tap by hand with `packaging/homebrew/update-formula.sh <version> <tap checkout>/Formula/pooltime.rb`.
  The key is kept only on GitHub (the secret plus the matching deploy key on the tap); no
  machine holds a copy, so a new or lost machine changes nothing. To create or replace it (e.g. if the
  tap job fails with a permission error), run this from any machine with `gh` signed in, then delete
  the older of the two release-CI keys under the tap's Settings → Deploy keys:

  ```sh
  K=$(mktemp -d)/tapkey && ssh-keygen -q -t ed25519 -N "" -C "pooltime-release-ci" -f "$K" \
    && gh repo deploy-key add "$K.pub" --repo chrisjenx/homebrew-tap --allow-write --title "pooltime-release-ci" \
    && gh secret set HOMEBREW_TAP_DEPLOY_KEY --repo chrisjenx/pooltime < "$K"; rm -f "$K" "$K.pub"
  ```
- `packaging/homebrew/pooltime.rb` is only the template; the tap is the source of truth.
- A hand-pushed `vX.Y.Z` tag still works (it must match `Cargo.toml`). The "Release" workflow can
  also be run manually: without `publish` it only builds the four targets as a check.

## Docs

User docs live in `README.md`, `llms.txt` and `docs/*.md`. They must describe what the code does:
every command must exist in `pooltime <cmd> --help` (checked by `scripts/check-docs.sh`; flags
are checked by hand against `--help`), every config key must match `src/config.rs`.
Tag a ```sh block `run` only if it is offline-safe (no network, browser, service install or real
home); `scripts/check-docs.sh` executes those blocks against a temp home.
