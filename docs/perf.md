# Performance: pooltime vs bro

Measured with `harness bench` (Task 42) on 2026-11-08. Both daemons run on the same machine, one
at a time, in the differential harness's sandbox, against identical fake upstreams.

```sh
cargo build --release -p pooltime --bin pooltime
cargo run -p harness -- bench --bro <bro-cli checkout> --pooltime target/release/pooltime
```

Leave out `--pooltime` and the bench runs `cargo build --release` itself. Defaults: 60 s idle,
50 streams × 5 s, 15 timed boots of each kind, 300 TTFB requests per side at concurrency 4. A run
takes a few minutes. `--only bro|pooltime` measures one daemon.

## Results

Current numbers: two full runs (default parameters), back to back, after `run/mgmt.key` moved to
the relaxed write (see [first boot](#first-boot)):

| Metric | pooltime run 3 | bro run 3 | pooltime run 4 | bro run 4 |
|---|---|---|---|---|
| **Idle RSS** after 60 s | **17.7 MiB** | 51.9 MiB | **17.7 MiB** | 51.1 MiB |
| **RSS under 50 concurrent streams** (peak) | **25.2 MiB** | 129.8 MiB | **25.4 MiB** | 134.5 MiB |
| **Cold start, restart** (existing state dir; median of 15) | **17.3 ms** | 25.9 ms | **9.4 ms** | 25.6 ms |
| **Cold start, first boot** (empty state dir; median of 15) | **17.7 ms** | 26.7 ms | **17.7 ms** | 26.6 ms |
| Added TTFB p50 | 0.45 ms | 0.57 ms | 0.37 ms | 0.51 ms |
| Added TTFB p95 | 3.40 ms | 0.57 ms | 0.27 ms | 0.57 ms |
| TTFB direct to the fake, p50 / p95 | 2.83 / 3.56 ms | 2.69 / 3.53 ms | 2.81 / 3.30 ms | 2.69 / 3.41 ms |
| TTFB through the daemon, p50 / p95 | 3.28 / 6.96 ms | 3.27 / 4.10 ms | 3.18 / 3.57 ms | 3.20 / 3.97 ms |
| Streams completed (with `message_stop`) | 50/50 | 50/50 | 50/50 | 50/50 |
| Processes in the daemon's tree | 1 | 1 | 1 | 1 |

Pooled over runs 3 and 4 (30 timed boots each): first boot median **17.7 ms** for pooltime vs
**26.7 ms** for bro; restart **9.6 ms** vs **25.8 ms**.

In short:

- **Memory**: pooltime idles at about a third of bro's resident memory (17.7 vs 48–52 MiB) and
  stays about 5× smaller with 50 open streams (25–26 vs 127–135 MiB). Its RSS grows ~8 MiB under
  50 streams; bro's grows ~80 MiB.
- **Cold start**: pooltime reaches a healthy `/health` in about 10–18 ms whether or not its state
  exists. bro takes 26–27 ms. That is 1.5× faster on a first boot and 2–3× faster on a restart.
- **Latency**: both add well under 1 ms to time-to-first-byte at p50. p95 is noisy run to run: one
  pooltime run had a 3.4 ms p95 outlier, against 0.3–1.3 ms in the other three runs. Neither
  daemon is consistently ahead on latency.

### Earlier runs (before the relaxed `mgmt.key` write)

The first two runs, at `src/` `e954da7`, with `mgmt.key` written durably:

| Metric | pooltime run 1 | bro run 1 | pooltime run 2 | bro run 2 |
|---|---|---|---|---|
| Idle RSS after 60 s | 17.7 MiB | 47.7 MiB | 17.6 MiB | 51.9 MiB |
| RSS under 50 concurrent streams (peak) | 25.5 MiB | 126.5 MiB | 25.6 MiB | 130.8 MiB |
| Cold start, restart (median of 15) | 9.8 ms | 26.8 ms | 16.7 ms | 32.7 ms |
| Cold start, first boot (median of 15) | 26.2 ms | 33.1 ms | 32.3 ms | 31.8 ms |
| Added TTFB p50 / p95 | 0.46 / 0.42 ms | 0.60 / 0.69 ms | 0.76 / 1.25 ms | 0.74 / 1.59 ms |

Pooled: first boot **32.0 ms vs 32.1 ms, a tie**; restart 12.5 ms vs 29.5 ms.

### First boot

In runs 1–2, the first boot of pooltime wrote its management key (`run/mgmt.key`) with the
crash-safe atomic write that every persisted file gets: temp file, `fsync`, rename,
parent-directory `fsync` (plan Global Constraints). On macOS, Rust's `File::sync_all` is
`F_FULLFSYNC`, which also flushes the drive's cache. Booting with parts of the state pre-created
(Python timer, 0.5 ms `/health` polling, 15 boots each, release binary at `e954da7`, no accounts)
showed where the time went:

| State present at boot | Median | Min | Max |
|---|---|---|---|
| nothing (first boot) | 20.9 ms | 20.0 | 32.1 |
| `pooltime.db` only | 20.7 ms | 17.8 | 36.0 |
| `run/mgmt.key` only | 10.6 ms | 10.1 | 14.1 |
| `logs/` only | 22.1 ms | 19.5 | 37.4 |
| all of it (restart) | 9.7 ms | 8.8 | 28.9 |

So the ~10 ms between a first boot and a restart was the durable key write. Creating the SQLite
database costs nothing measurable.

`mgmt.key` can be regenerated: losing it to a power cut only changes the dashboard URL/cookie. So
it now uses `fsutil::atomic_write_relaxed`. That write is still a temp file plus an atomic rename,
still 0600, but it calls plain `fsync(2)` instead of `F_FULLFSYNC`. Every other persisted file
(credentials, database, config, snapshots, Claude Code files) keeps the full-durability
`atomic_write`. Runs 3–4 show the result: first boot 17.7 ms, down from about 32 ms.

## Environment

- Machine: Mac15,9 (MacBook Pro), Apple M3 Max, 16 cores, 128 GiB RAM, macOS 26.7.2 (25G332).
  Other user processes were running, including the user's live bro on port 3456, which the
  bench never touches.
- pooltime: runs 1–2 at `src/` `e954da7` (bench code from commit `5d5d422`, run from the working
  tree just before that commit); runs 3–4 with the relaxed `mgmt.key` write (commit
  `perf(server): relaxed fsync for regenerable mgmt.key`, measured from the working tree just
  before it). **release** profile
  (`lto = "thin"`, `codegen-units = 1`, `strip = true`), rustc 1.99.1 (Homebrew).
- bro: `b99b135` on `feat/codex-env-overrides` (the fork branch with the Codex URL overrides),
  bun 1.3.14, run as `bun run src/index.ts serve` in `pool/`.

## Method

- **Sandbox**: the harness's (`harness/src/daemons.rs`). Each child gets a cleared environment,
  temp `HOME`/`TMPDIR` (and bro's `CLAUDE_POOL_DIR`/`CLAUDE_CONFIG_DIR`, pooltime's
  `POOLTIME_HOME`), a fresh port that is never 3456 or 3460, a `PATH` that starts with a
  `security` shim that fails (bro cannot reach the Keychain; the run fails if it was called),
  every upstream URL on a 127.0.0.1 fake, and fake tokens. One Claude account each. Usage polling
  is off in both (`CLAUDE_USAGE_REFRESH=0`, `usage_refresh = false`), so neither daemon makes
  background upstream calls while idle.
- **Cold start**: time from just before the harness's start call (sandbox seeding of a few small
  files, then spawn) to the first `/health` 200; the harness polls every 5 ms. The two daemons
  boot alternately, run by run, so drift in machine load hits both alike. *First boot*: a new
  empty sandbox every time, one discarded warm-up round. *Restart*: one sandbox per daemon,
  reused; the untimed first round creates the state.
- **Idle RSS**: after boot, no traffic for 60 s, then the resident set of the daemon's whole
  process tree (`ps -A -o pid=,ppid=,rss=`, summed over the spawned process and all its
  descendants; for bro that is bun and anything it spawned — there were no children).
- **TTFB**: streaming `/v1/messages` requests; the fake answers with a complete short SSE turn at
  once. TTFB = request sent → first response body chunk. 4 workers each alternate one request
  straight to the fake and one through the daemon (alternating which goes first), 300 per side,
  after 10 warm-up requests per side. *Added TTFB* = daemon percentile − direct percentile (a
  difference of percentiles, not a percentile of per-request differences, so p95 can come out
  below p50).
- **RSS under load**: 50 concurrent streaming requests; the fake sends `message_start` at once,
  then 10 text deltas spread over 5 s. The process-tree RSS is sampled every 50 ms until every
  stream ends; the peak is reported. Every stream must end with `message_stop` or the bench
  fails.
- **Cleanup**: each daemon is killed with its whole process tree after each measurement; after
  the runs `pgrep` showed no pooltime and only the user's live bro.

## Caveats

- One machine, one OS. Linux (and musl builds) are not measured here.
- No real upstream: the fakes answer instantly, so the numbers isolate proxy overhead and say
  nothing about Anthropic/OpenAI latency. One account each, so routing work is minimal.
- Times are wall clock on a desktop with other work running; run-to-run spread is visible above
  (pooltime restart medians range from 9.4 to 17.3 ms). Both daemons were measured in the same runs.
- RSS counts resident pages only (shared libraries included). bun's JIT and GC make bro's RSS
  depend on history; bro was measured fresh, so a long-running bro is likely larger (the user's
  live bro showed ~245 MiB in `ps` during these runs, under real traffic).
- Cold start includes the harness's sandbox seeding (both daemons: a handful of small file
  writes) and up to 5 ms of polling granularity; the Python numbers in [first boot](#first-boot)
  use 0.5 ms polling and no seeding, hence lower.
- The bench is not part of `cargo test` (it needs bun, a bro checkout and minutes of wall clock).
