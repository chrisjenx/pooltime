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
process, which launchd kills (process group) and systemd kills (cgroup). Rejected. (An `execve`
*in place*, with no fork, does not have that problem; it is how the supervisor itself is
replaced, see "Replacing the supervisor" below.)

## Design

- **Linux reload.** The unit's `ExecReload` is `<bin> service signal-reload $MAINPID` (a hidden
  subcommand that sends `SIGHUP`), not `/bin/kill -HUP $MAINPID`: NixOS and other systems
  without a merged `/usr` have no `/bin/kill`, and a bare `kill` would need a shell on `PATH`;
  the pooltime binary is always at the path the unit already names. `service restart` still
  goes through `systemctl --user reload`, so systemd sees the reload, and plain `systemctl
  --user reload pooltime` works the same. A unit from 1.1.x gets the new line on its next
  `service restart` (rewritten in place plus `daemon-reload`; the service keeps running), but
  only if its one `ExecReload=` line is exactly the 1.1.x one and the unit is a regular file:
  a hand-edited or symlinked unit is left alone, and a failed rewrite (read-only unit dir) is
  a warning, never a failed reload.
- **Control channel.** Each worker gets a `socketpair` end (fd inherited, `FD_CLOEXEC` set again
  in the worker) and the listener fd (hidden `serve --listen-fd N --control-fd M`). Line protocol:
  worker → `ready <version>`, `released`; supervisor → `active`, `handoff`, `stop`, `force`.
  While the socket is open a worker ignores OS signals and acts on these orders only. EOF from
  the supervisor = it died: the worker hands off (closes its listener so a restarted supervisor
  can bind) and drains on its own; from then on its own second `SIGTERM` forces exit. No
  filesystem socket, so no other user can connect or inject a listener.
- **Single writer.** A worker serves at once but starts its background tasks (usage pollers,
  catalog, profile sync, health tick, alerts, status cache, sampler, session and retention prune)
  only on `active`. On `handoff` the old worker aborts them, waits for the blocking work they
  started (profile writes, status-cache write, sampling round, notifications), stops the status
  cache *without* deleting it, flushes its queued database writes and stops persisting
  `account_state` rows (its in-flight requests keep updating its memory only; rows still queued
  then are dropped, see known limits); then it answers `released`
  (all of this bounded by 5 s). Only then does the supervisor send `active` to the new one (after
  at most 10 s, even if the old one never answers); the new one first reloads account state from
  the database, so it sees the rate limits, dead-token markers and billing blocks the old one
  learned. If the generation waiting for `active` dies first, the pending release is kept and its
  replacement waits for it too. Journal rows, timeline rows and token refreshes stay
  multi-process safe as before (SQLite WAL + busy timeout, refresh `flock`).
- **Handoff drain.** The old worker stops accepting and closes its copy of the listener at once,
  keeps its open connections for a 2 s grace (a client that just connected still gets its
  request served), then does today's graceful drain without the 503 holder. Refresh drain and
  store flush run as before.
- **Stop.** `SIGTERM`/`SIGINT` to the supervisor becomes `stop` for every worker (a second one:
  `force`); workers then drain exactly as before, including the 503 holder. Because workers
  ignore OS signals while supervised, a `SIGTERM` that reaches the whole process group (`pkill`,
  launchd) is counted once. The systemd unit also uses `KillMode=mixed`.
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
  the old way); `doctor` warns until then. While the old daemon drains, the CLI says so, shows
  progress and keeps waiting through a first Ctrl-C; on a timeout or a second Ctrl-C it still
  tries to load the new definition and, if launchd refuses because the old job is still there,
  says to run `pooltime service install` again. systemd queues the restart itself, so
  interrupting `systemctl` is harmless there.
- **Foreground.** `pooltime serve` without `--supervise` is unchanged.

**The supervisor is not upgraded by `service restart`.** It is deliberately small (no database,
no providers, no HTTP) so it rarely needs to change. Picking up a new supervisor needs a full
restart: `pooltime service install`. The worker protocol must stay backward compatible.

**Known limits.** The release waits only for the blocking work its own background tasks
started: they run in a scope (`background::Work`) that their blocking jobs count against, so a
profile sync or model refresh started by a request is not waited for (it may still write its
profile files during the overlap, as any request could before). The whole release is bounded at
5 s. If that bound runs out while the old generation is still flushing its queue, the
`account_state` rows still queued are dropped: every row write takes SQLite's write lock, then
checks the book's `persisting` flag and holds that flag's lock through its commit, so once
`stop_persisting()` returns a row is either committed (before `released`, so before the new
generation's reload) or never will be. What the old generation learned in those rows is lost to
the new one, which learns it again from the next upstream answer. If the supervisor is
killed, an orphaned worker counts `SIGTERM`s from the moment it notices: the first changes
nothing (it already drains), the second forces exit. Service-manager commands (`launchctl`,
`systemctl`) run in their own process group, so a Ctrl-C during the one-time reinstall reaches
only the CLI, which counts it; a `launchctl print` that cannot answer is read as "still loaded".

**Residual risks.** An idle keep-alive connection the old worker closes after the grace can race
a request the client is sending on it at that instant (the SDK retries a connection error; no
503). A new version's database migration must stay compatible with the previous binary for the
length of its drain. If the supervisor itself dies, its workers see the control socket close and
drain, but launchd may kill its process group first (in-flight streams lost, as today); systemd
waits for them up to `TimeoutStopSec`. While the very first worker boots, or a crashed one is
being replaced, connections wait in the backlog rather than being refused, for up to the boot
time (under a second normally).

## Replacing the supervisor (`pooltime update`, 1.3)

A worker generation picks up a new binary, but the supervisor kept running the version it started
with until `service install` (a full stop and start). Options:

- **`service install`** stops the job: in-flight requests drain while new ones wait or fail.
- **A new supervisor next to the old one**, handed the listener over a socket: it would not be the
  job's main process, so launchd/systemd would kill it with the old one (constraint above).
- **`execve` in place (chosen).** On `SIGUSR2` the supervisor `exec`s the binary at its configured
  path *in its own process*: same pid, so the job's main process never exits and neither manager
  notices (systemd `MainPID` and launchd's tracked pid unchanged; the workers stay in the job's
  process group / cgroup and remain its children). The listener and each worker's control socket
  survive the `exec` (`FD_CLOEXEC` cleared just before; set again by the new process), and the
  state crosses as a hidden `--resume <json>` argument: fd numbers, worker pids, generations,
  phases (active / draining, with their age for the drain backstop), restart and upgrade
  counters. The new process adopts the workers (no `Child` handles across an `exec`: they are
  reaped by pid on `SIGCHLD` plus a 1 s poll), then starts a generation from the new binary and
  hands off to it exactly like `SIGHUP`.

Safety:

- **Probe first.** `<bin> serve --reexec-probe` must answer `pooltime-resume <formats>
  <version>` with the format this supervisor writes, and fails if `config.toml` does not load
  or the data / log directory cannot be prepared and written.
  An older binary does not know the flag and fails the probe: no `exec`. The resumed process
  still falls back to defaults if `config.toml` broke after the probe (the socket is bound
  already), rather than orphaning its workers.
- **Only when idle.** No generation booting or releasing (their `ready` / `released` lines would
  be lost across the `exec`) and none to respawn; the upgrade waits up to 2 minutes for that.
- **Signals.** In supervisor mode only the main thread (the supervisor's loop) takes
  `SIGHUP`/`SIGTERM`/`SIGINT`/`SIGUSR2`: the runtime's threads block them from their start, and
  the log writer thread is spawned while the main thread blocks them too (until the handlers are
  installed). Before the `exec` the main thread blocks them and the loop runs on for 150 ms, so a
  signal the runtime caught just before is still handled (a stop cancels the upgrade, a restart
  postpones it until it is done). After that a signal stays pending in the kernel, survives the
  `exec` (pending signals and the mask do; handlers do not) and is delivered once the new process
  has its handlers. Workers get an empty mask from `std`'s spawn and ignore `SIGUSR2`, so a
  `pkill -USR2` or process-group signal does not kill them.
- **No exit once re-executed.** The new process holds the workers' control sockets, so it does
  not exit on a data-directory, log or listener error: it logs to stderr and keeps the adopted
  workers serving (without the listener it cannot start new ones; the first one's failure is
  reported).
- **Failure.** A failed probe or `execve` leaves the old supervisor as it was (descriptors marked
  close-on-exec again, mask restored). A new first worker that fails leaves the adopted one
  serving. Every outcome is recorded in `run/supervisor.json` (`upgrades`, `last_upgrade`), which
  `pooltime update` waits for. A supervisor from before 1.3 has no `can_upgrade` in its state
  file; `pooltime update` falls back to `service restart` for it.
- **Compatibility.** The `--resume` format is versioned (`proto`); a binary lists every format it
  reads, so a newer binary keeps accepting the older format.
