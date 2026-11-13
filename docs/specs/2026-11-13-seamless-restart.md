# Seamless restart (2026-11-13)

**Problem.** `pooltime service restart` was `launchctl kickstart -k` / `systemctl restart`: the
daemon drained (in-flight streams finish, up to 10 min) while its still-bound listener answered
every new request with 503, and only then did the service manager start the new binary. The pool
is never idle, so a restart held new requests for 25 s, and a long reply could hold them for
minutes. Goal: the new binary serves new requests within about a second while the old process
finishes its streams; no refused connections and no 503s.

**Constraints.** launchd and systemd run one main process per job, consider the job over when it
exits, and kill what is left of its process group / cgroup. A new process cannot be started
"next to" the old one within the same job, and a detached process would escape `service status`,
`KeepAlive` and `service uninstall`.

## Options

**(a) Supervisor + worker generations (chosen).** The job's main process is `pooltime serve
--supervise`, a small loop that binds the port once and runs `pooltime serve` workers that
inherit the listening socket (fd passing by `exec`, no filesystem path). `service restart` sends
the supervisor `SIGHUP`: it starts a new worker from the configured binary path (the Homebrew
`<prefix>/bin/pooltime` symlink, so an upgrade takes effect), waits for it to report ready, then
tells the old worker to hand off: stop accepting, stop its background tasks, finish in-flight
requests and exit. Both workers accept from the *same* kernel socket, so a connection is never
refused or reset: it waits in the shared backlog until one of them accepts it. Everything stays
inside one launchd job / systemd unit, so status, `KeepAlive`, stop and uninstall work as before.

**(b) `SO_REUSEPORT` overlap.** Measured on this Mac (macOS 26): with two listeners on one port
the newest bind receives *every* connection, and closing a listener resets the connections queued
on it; Linux instead hashes connections across the listeners and also resets the closed
listener's queue (unless `net.ipv4.tcp_migrate_req` is on). So the old process cannot stop
listening without dropping connections on Linux, or the new one without dropping them on macOS.
Any later same-user process that binds with `SO_REUSEPORT` silently takes the port's traffic
(prompts and proxy keys). And it does not solve the one-process-per-job problem. Rejected.

**(c) Others.** Socket activation (launchd `Sockets`, systemd `.socket`) keeps connections queued
during the gap but the manager still waits for the old process to exit before starting the new
one: no overlap. An nginx-style self re-exec needs the new process to outlive the job's main
process, which launchd kills (process group) and systemd kills (cgroup). Rejected.

## Design

- **Control channel.** Each worker gets a `socketpair` end (fd inherited, `FD_CLOEXEC` set again
  in the worker) and the listener fd (hidden `serve --listen-fd N --control-fd M`). Line protocol:
  worker → `ready <version>`, `released`; supervisor → `active`, `handoff`. EOF from the
  supervisor = it died: the worker hands off (closes its listener so a restarted supervisor can
  bind) and drains. No filesystem socket, so no other user can connect or inject a listener.
- **Single writer.** A worker serves at once but starts its background tasks (usage pollers,
  catalog, profile sync, health tick, alerts, status cache, sampler, session and retention prune)
  only on `active`. On `handoff` the old worker aborts them, stops the status cache *without*
  deleting it, answers `released`, and only then does the supervisor send `active` to the new
  one (after at most 10 s). Request-path writes (journal, state, token refreshes) were already
  safe across processes (SQLite WAL + busy timeout, refresh `flock`).
- **Handoff drain.** The old worker stops accepting and closes its copy of the listener at once,
  keeps its open connections for a 2 s grace (a client that just connected still gets its
  request served), then does today's graceful drain without the 503 holder. Refresh drain and
  store flush run as before.
- **Stop.** `SIGTERM`/`SIGINT` to the supervisor is forwarded to every worker as `SIGTERM`
  (a second one forces); workers then drain exactly as before, including the 503 holder.
  Supervised workers ignore `SIGINT`/`SIGHUP` (a terminal's Ctrl-C reaches the whole process
  group). The systemd unit uses `KillMode=mixed` so only the supervisor gets the first `SIGTERM`.
- **Crashes.** A worker that dies unexpectedly is respawned at once; the port stays bound so
  clients wait in the backlog instead of being refused. Repeated failures back off (1, 2, 4,
  8 s); after 5 in a row, or if the very first worker never gets ready, the supervisor exits
  non-zero and the service manager's own restart policy takes over. A handoff whose new worker
  fails (exits, or no `ready` within 60 s) leaves the old worker serving and reports the error.
- **Status.** The supervisor keeps `run/supervisor.json` (0600: its pid and version, the active
  generation, its pid and version, draining pids, and the last handoff's result).
  `service restart` waits on it and reports the outcome; `service status` and `doctor` show it.
- **Old service definitions.** A plist/unit written before this change runs plain `serve`.
  `service restart` notices, explains, and reinstalls the definition once (that restart drains
  the old way); `doctor` warns until then.
- **Foreground.** `pooltime serve` without `--supervise` is unchanged.

**The supervisor is not upgraded by `service restart`.** It is deliberately small (no database,
no providers, no HTTP) so it rarely needs to change. Picking up a new supervisor needs a full
restart: `pooltime service install`. The worker protocol must stay backward compatible.

**Residual risks.** An idle keep-alive connection the old worker closes after the grace can race
a request the client is sending on it at that instant (the SDK retries a connection error; no
503). A new version's database migration must stay compatible with the previous binary for the
length of its drain. If the supervisor itself dies, its workers see the control socket close and
drain, but launchd may kill its process group first (in-flight streams lost, as today); systemd
waits for them up to `TimeoutStopSec`. While the very first worker boots, or a crashed one is
being replaced, connections wait in the backlog rather than being refused, for up to the boot
time (under a second normally).
