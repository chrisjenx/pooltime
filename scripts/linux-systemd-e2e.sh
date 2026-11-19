#!/usr/bin/env bash
# Linux end-to-end check of the background service under a real systemd user manager:
# `service install`, a streaming request in flight, `service restart` (and a plain
# `systemctl --user reload`), the supervisor replacing itself in place (`SIGUSR2`, what
# `pooltime update` sends: same MainPID), no refused connection and no 503 meanwhile, the stream
# completes, then `service uninstall`.
#
#   scripts/linux-systemd-e2e.sh <pooltime binary>
#
# It never touches the invoking user's files or services: it creates a throwaway user whose home
# (the temp HOME) lives in a root-owned temp dir, enables lingering so that user gets its own
# systemd user manager, runs the scenario as that user against a loopback fake Anthropic upstream
# (no network), and deletes the user and the dir again. The inner phase refuses to run as anyone
# but that user in that home. Needs Linux with systemd, passwordless sudo, python3 and curl: run it in CI or a
# disposable VM. Exits non-zero on any failure, never skips.
set -euo pipefail

STREAM_PAUSE=${STREAM_PAUSE:-20} # seconds the slow stream holds between its first and last frames

die() {
	echo "linux-systemd-e2e: FAIL: $*" >&2
	exit 1
}
say() { echo "linux-systemd-e2e: $*"; }

# ---- outer: a throwaway user with its own user manager --------------------------------------

outer() {
	local bin=$1
	[ "$(uname -s)" = Linux ] || die "Linux only"
	[ -x "$bin" ] || die "no executable pooltime binary at $bin"
	command -v systemctl >/dev/null || die "systemctl not found"
	command -v python3 >/dev/null || die "python3 not found"
	command -v curl >/dev/null || die "curl not found"
	sudo -n true 2>/dev/null || die "needs passwordless sudo (run it in CI or a disposable VM)"

	# Globals: the EXIT trap runs after this function's locals are gone.
	E2E_USER=pooltime-e2e-$$
	E2E_UID=""
	E2E_BASE=""
	local user=$E2E_USER uid home
	cleanup_outer() {
		local rc=$?
		sudo loginctl disable-linger "$E2E_USER" 2>/dev/null || true
		if [ -n "$E2E_UID" ]; then
			sudo systemctl stop "user@$E2E_UID.service" 2>/dev/null || true
		fi
		sudo pkill -KILL -u "$E2E_USER" 2>/dev/null || true
		sudo userdel --remove "$E2E_USER" 2>/dev/null || true
		if [ -n "$E2E_BASE" ]; then sudo rm -rf "$E2E_BASE"; fi
		exit "$rc"
	}
	trap cleanup_outer EXIT
	# A root-owned temp dir holds the throwaway home and a marker naming its user: the inner
	# phase refuses to run anywhere else.
	E2E_BASE=$(sudo mktemp -d /tmp/pooltime-e2e.XXXXXX)
	sudo chmod 0755 "$E2E_BASE"
	home=$E2E_BASE/home
	echo "$user" | sudo tee "$E2E_BASE/marker" >/dev/null
	sudo useradd --create-home --home-dir "$home" --shell /bin/bash "$user"
	uid=$(id -u "$user")
	E2E_UID=$uid
	[ "$(getent passwd "$user" | cut -d: -f6)" = "$home" ] || die "unexpected home for $user"
	say "throwaway user $user (uid $uid, home $home)"

	sudo install -d -o "$user" -m 0700 "$home/.local/bin" "$home/e2e"
	sudo install -o "$user" -m 0755 "$bin" "$home/.local/bin/pooltime"
	sudo install -o "$user" -m 0755 "$0" "$home/e2e/run.sh"

	sudo loginctl enable-linger "$user"
	sudo systemctl start "user@$uid.service"
	local runtime=/run/user/$uid
	for _ in $(seq 100); do
		sudo test -S "$runtime/systemd/private" && break
		sleep 0.2
	done
	sudo test -S "$runtime/systemd/private" || die "no systemd user manager for $user"
	say "user manager: $(sudo -u "$user" env XDG_RUNTIME_DIR="$runtime" systemctl --user is-system-running || true)"

	# A clean environment: nothing of the invoking user's (XDG_CONFIG_HOME, ...) leaks in.
	sudo -H -u "$user" env -i \
		HOME="$home" USER="$user" LOGNAME="$user" LANG=C.UTF-8 \
		XDG_RUNTIME_DIR="$runtime" \
		DBUS_SESSION_BUS_ADDRESS="unix:path=$runtime/bus" \
		PATH="$home/.local/bin:/usr/local/bin:/usr/bin:/bin" \
		STREAM_PAUSE="$STREAM_PAUSE" \
		POOLTIME_E2E_INNER=1 \
		POOLTIME_E2E_BASE="$E2E_BASE" \
		bash "$home/e2e/run.sh" "$home/.local/bin/pooltime"
}

# ---- inner: the scenario, as the throwaway user ---------------------------------------------

FAKE_PY='
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PAUSE = float(sys.argv[2])

def ev(name, data):
    return ("event: %s\ndata: %s\n\n" % (name, json.dumps(data))).encode()

class Fake(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"  # body until close

    def log_message(self, *args):
        pass

    def not_found(self):
        body = json.dumps({"type": "error", "error": {"type": "not_found_error", "message": "fake"}}).encode()
        self.send_response(404)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.not_found()

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length") or 0))
        if self.path.split("?")[0] != "/v1/messages":
            return self.not_found()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("request-id", "req_e2e")
        self.end_headers()
        w = self.wfile
        w.write(ev("message_start", {"type": "message_start", "message": {
            "id": "msg_e2e", "type": "message", "role": "assistant", "content": [],
            "usage": {"input_tokens": 3, "output_tokens": 1}}}))
        w.flush()
        if b"slow-stream" in body:
            time.sleep(PAUSE)
        w.write(ev("content_block_delta", {"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "e2e-done"}}))
        w.write(ev("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 2}}))
        w.write(ev("message_stop", {"type": "message_stop"}))
        w.flush()

srv = ThreadingHTTPServer(("127.0.0.1", 0), Fake)
srv.daemon_threads = True
with open(sys.argv[1], "w") as f:
    f.write(str(srv.server_address[1]))
srv.serve_forever()
'

inner() {
	local pt=$1
	# This phase writes ~/.pooltime and installs a service: only ever as the throwaway user the
	# outer phase created, in the home it created under its root-owned temp dir.
	local me base
	me=$(id -un)
	base=${POOLTIME_E2E_BASE:-}
	case "$me" in
	pooltime-e2e-*) ;;
	*) die "the inner phase runs only as a throwaway pooltime-e2e-* user (not $me)" ;;
	esac
	case "$base" in
	/tmp/pooltime-e2e.*) ;;
	*) die "the inner phase needs the outer phase's temp dir" ;;
	esac
	if [ "$(stat -c %U "$base")" != root ] || [ "$(cat "$base/marker" 2>/dev/null)" != "$me" ]; then
		die "$base was not created for $me by the outer phase"
	fi
	if [ "${HOME:-}" != "$base/home" ] || [ "$(getent passwd "$me" | cut -d: -f6)" != "$base/home" ]; then
		die "HOME must be the throwaway home $base/home"
	fi
	local work=$HOME/e2e
	# Globals: the EXIT trap runs after this function's locals are gone.
	E2E_PT=$pt
	fake_pid=""
	hammer_pid=""
	cleanup_inner() {
		local rc=$?
		if [ "$rc" != 0 ]; then
			say "--- diagnostics ---"
			systemctl --user status pooltime.service --no-pager 2>&1 || true
			journalctl --user -u pooltime.service --no-pager -n 200 2>&1 || true
			cat "$HOME/.pooltime/run/supervisor.json" 2>/dev/null || true
			tail -n 200 "$HOME"/.pooltime/logs/* 2>/dev/null || true
		fi
		if [ -n "$hammer_pid" ]; then kill "$hammer_pid" 2>/dev/null || true; fi
		"$E2E_PT" service uninstall >/dev/null 2>&1 || true
		if [ -n "$fake_pid" ]; then kill "$fake_pid" 2>/dev/null || true; fi
		exit "$rc"
	}
	trap cleanup_inner EXIT

	# Fake Anthropic upstream on a loopback port.
	python3 -c "$FAKE_PY" "$work/fake.port" "$STREAM_PAUSE" &
	fake_pid=$!
	for _ in $(seq 100); do [ -s "$work/fake.port" ] && break; sleep 0.1; done
	[ -s "$work/fake.port" ] || die "fake upstream did not start"
	local upstream
	upstream="http://127.0.0.1:$(cat "$work/fake.port")"

	# A free port for the pool (never 3460/3456).
	local port
	port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
	local base="http://127.0.0.1:$port"

	# A pool with one signed-in (fake) account.
	local data=$HOME/.pooltime
	install -d -m 0700 "$data" "$data/accounts" "$data/accounts/work"
	cat >"$data/config.toml" <<-EOF
		port = $port

		[anthropic]
		api_base = "$upstream"
		token_url = "$upstream/v1/oauth/token"
		usage_refresh = false

		[alerts]
		notify = false
	EOF
	chmod 0600 "$data/config.toml"
	printf 'id = "work"\nprovider = "anthropic"\n' >"$data/accounts/work/account.toml"
	printf '{"provider":"anthropic","access_token":"e2e-fake-access","refresh_token":"e2e-fake-refresh","expires_at":"%s","scopes":["user:inference"],"plan":"max"}\n' \
		"$(date -u -d '+2 hours' +%Y-%m-%dT%H:%M:%SZ)" >"$data/accounts/work/credentials.json"
	chmod 0600 "$data/accounts/work/account.toml" "$data/accounts/work/credentials.json"

	say "service install (port $port, upstream $upstream)"
	"$pt" service install
	local unit=$HOME/.config/systemd/user/pooltime.service
	[ -f "$unit" ] || die "no unit at $unit"
	grep -q "^ExecReload=$pt service signal-reload \$MAINPID\$" "$unit" ||
		die "unexpected ExecReload: $(grep ExecReload "$unit")"
	for _ in $(seq 150); do
		curl -sf -o /dev/null "$base/health" && break
		sleep 0.2
	done
	curl -sf -o /dev/null "$base/health" || die "the service never answered /health"
	"$pt" service status
	local gen0
	gen0=$(state_field generation)

	# A streaming request that spans both restarts.
	local hdr=(-H 'content-type: application/json' -H 'anthropic-version: 2023-06-01')
	curl -sS -N --max-time $((STREAM_PAUSE + 60)) "${hdr[@]}" \
		--data '{"model":"claude-sonnet-5","max_tokens":8,"stream":true,"messages":[{"role":"user","content":"slow-stream"}]}' \
		-o "$work/stream.out" -w '%{http_code}' "$base/v1/messages" >"$work/stream.code" 2>"$work/stream.err" &
	local stream_pid=$!
	for _ in $(seq 100); do grep -q message_start "$work/stream.out" 2>/dev/null && break; sleep 0.1; done
	grep -q message_start "$work/stream.out" || die "the slow stream did not start"

	# Fresh connections all the time: /health, and every tenth a short stream.
	hammer "$base" "$work/hammer.log" &
	hammer_pid=$!

	sleep 1
	say "service restart"
	"$pt" service restart | tee "$work/restart.out"
	grep -q "restarted without closing the port" "$work/restart.out" || die "no seamless handover"
	local gen1
	gen1=$(state_field generation)
	[ "$gen1" -gt "$gen0" ] || die "generation did not advance ($gen0 -> $gen1)"

	say "systemctl --user reload (ExecReload through pooltime)"
	local restarts
	restarts=$(state_field restarts)
	systemctl --user reload pooltime.service
	for _ in $(seq 150); do
		[ "$(state_field restarts)" -gt "$restarts" ] && break
		sleep 0.2
	done
	[ "$(state_field restarts)" -gt "$restarts" ] || die "systemctl reload did not restart the workers"
	[ "$(state_field last_restart.ok)" = True ] || die "the reload's handover failed"

	say "supervisor upgrade in place (SIGUSR2, as pooltime update sends it)"
	local main upgrades
	main=$(systemctl --user show -p MainPID --value pooltime.service)
	[ "$main" -gt 1 ] || die "no MainPID ($main)"
	[ "$(state_field can_upgrade)" = True ] || die "the supervisor does not offer in-place upgrades"
	upgrades=$(state_field upgrades)
	kill -USR2 "$main"
	for _ in $(seq 150); do
		[ "$(state_field upgrades)" -gt "$upgrades" ] && break
		sleep 0.2
	done
	[ "$(state_field upgrades)" -gt "$upgrades" ] || die "the supervisor did not report the upgrade"
	[ "$(state_field last_upgrade.ok)" = True ] || die "the upgrade failed: $(state_field last_upgrade.error)"
	[ "$(systemctl --user show -p MainPID --value pooltime.service)" = "$main" ] ||
		die "MainPID changed: systemd saw the supervisor exit"
	[ "$(state_field pid)" = "$main" ] || die "the state file names another supervisor pid"
	systemctl --user is-active --quiet pooltime.service || die "the unit is no longer active"
	grep -q message_stop "$work/stream.out" &&
		die "the slow stream ended before the restarts and the upgrade were done: raise STREAM_PAUSE"

	say "waiting for the in-flight stream"
	wait "$stream_pid" || die "the in-flight stream failed: $(cat "$work/stream.err")"
	[ "$(cat "$work/stream.code")" = 200 ] || die "stream status $(cat "$work/stream.code")"
	if ! grep -q e2e-done "$work/stream.out" || ! grep -q message_stop "$work/stream.out"; then
		die "the in-flight stream did not complete: $(cat "$work/stream.out")"
	fi

	kill "$hammer_pid"
	wait "$hammer_pid" 2>/dev/null || true
	hammer_pid=""
	local total bad
	total=$(($(wc -l <"$work/hammer.log")))
	bad=$(grep -cv '^ok ' "$work/hammer.log" || true)
	say "hammer: $total requests, $bad failed"
	[ "$total" -gt 50 ] || die "too few requests to mean anything ($total)"
	[ "$bad" = 0 ] || die "failures during the restarts: $(grep -v '^ok ' "$work/hammer.log" | sort | uniq -c)"

	say "service uninstall"
	"$pt" service uninstall
	[ ! -f "$unit" ] || die "the unit is still there"
	systemctl --user is-active --quiet pooltime.service && die "the unit is still active"
	for _ in $(seq 50); do
		pgrep -u "$(id -u)" -x pooltime >/dev/null || break
		sleep 0.2
	done
	pgrep -u "$(id -u)" -x pooltime >/dev/null && die "pooltime processes left: $(pgrep -a -u "$(id -u)" -x pooltime)"
	curl -s -o /dev/null "$base/health" && die "the port still answers"
	say "PASS"
}

# One field of run/supervisor.json (dotted path), printed by python.
state_field() {
	python3 - "$HOME/.pooltime/run/supervisor.json" "$1" <<-'EOF'
		import json, sys
		v = json.load(open(sys.argv[1]))
		for k in sys.argv[2].split("."):
		    v = v.get(k) if isinstance(v, dict) else None
		print(0 if v is None else v)
	EOF
}

# Until killed: one line per request, `ok <what>` or `fail <what> <status> <curl exit>`.
hammer() {
	local base=$1 log=$2 n=0 code rc out
	out=$(mktemp)
	trap 'rm -f "$out"; exit 0' TERM
	while :; do
		n=$((n + 1))
		if [ $((n % 10)) = 0 ]; then
			rc=0
			code=$(curl -sS --max-time 20 -H 'content-type: application/json' -H 'anthropic-version: 2023-06-01' \
				--data '{"model":"claude-sonnet-5","max_tokens":8,"stream":true,"messages":[{"role":"user","content":"hi"}]}' \
				-o "$out" -w '%{http_code}' "$base/v1/messages" 2>/dev/null) || rc=$?
			if [ "$rc" = 0 ] && [ "$code" = 200 ] && grep -q message_stop "$out"; then
				echo "ok stream" >>"$log"
			else
				echo "fail stream $code $rc" >>"$log"
			fi
		else
			rc=0
			code=$(curl -sS --max-time 20 -o /dev/null -w '%{http_code}' "$base/health" 2>/dev/null) || rc=$?
			if [ "$rc" = 0 ] && [ "$code" = 200 ]; then
				echo "ok health" >>"$log"
			else
				echo "fail health $code $rc" >>"$log"
			fi
		fi
	done
}

[ $# = 1 ] || die "usage: $0 <pooltime binary>"
if [ "${POOLTIME_E2E_INNER:-}" = 1 ]; then
	inner "$1"
else
	outer "$(readlink -f "$1")"
fi
