#!/bin/sh
# Verify the documentation against the real binary:
#
# 1. Every fenced ```sh block whose info string includes `run` (```sh run) in README.md,
#    AGENTS.md, llms.txt and docs/*.md is executed with `sh -eu`, each in a fresh temp
#    POOLTIME_HOME and HOME, with the freshly built `pooltime` first on PATH, browsers and the
#    release check disabled, and a port nothing listens on. Any non-zero exit fails.
#    Only offline-safe blocks may be tagged `run` (no network, browser, service or real home).
# 2. Every `pooltime <subcommand>` named in a code span or a sh/bash/plain code block is a real subcommand
#    (from `pooltime --help`), and for the command groups also a real second-level subcommand.
#
# Usage: scripts/check-docs.sh        (builds target/debug/pooltime first)
#        POOLTIME_BIN=/path/to/pooltime scripts/check-docs.sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

if [ -n "${POOLTIME_BIN:-}" ]; then
    case $POOLTIME_BIN in
    /*) bin=$POOLTIME_BIN ;;
    *) bin=$(pwd)/$POOLTIME_BIN ;;
    esac
    cd "$root"
else
    cd "$root"
    cargo build -q -p pooltime --bin pooltime
    bin=$root/target/debug/pooltime
fi
[ -x "$bin" ] || {
    echo "check-docs: no pooltime binary at $bin" >&2
    exit 2
}

work=$(mktemp -d "${TMPDIR:-/tmp}/pooltime-docs.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM
mkdir -p "$work/bin" "$work/blocks"
ln -s "$bin" "$work/bin/pooltime"

files="README.md AGENTS.md llms.txt"
for f in docs/*.md; do
    files="$files $f"
done

failed=0

# ---- 1. run the `sh run` blocks -------------------------------------------------------------
# Each block becomes $work/blocks/<file#>-<block#>.sh; its first line names file:line.
i=0
for f in $files; do
    i=$((i + 1))
    awk -v dir="$work/blocks" -v src="$f" -v fi="$i" '
        !inb && /^```sh[ \t]/ && substr($0, 6) ~ /(^|[ \t])run([ \t]|$)/ {
            inb = 1
            n++
            out = sprintf("%s/%04d-%04d.sh", dir, fi, n)
            printf "# %s:%d\n", src, NR > out
            next
        }
        inb && /^```[ \t]*$/ { inb = 0; close(out); next }
        inb { print > out }
    ' "$f"
done

ran=0
for b in "$work"/blocks/*.sh; do
    [ -e "$b" ] || continue
    where=$(head -n 1 "$b" | sed 's/^# //')
    t="$work/run.$ran"
    mkdir -p "$t/home" "$t/pooltime" "$t/tmp"
    if (cd "$t" && env -i \
        HOME="$t/home" \
        POOLTIME_HOME="$t/pooltime" \
        POOLTIME_PORT=1 \
        POOLTIME_NO_BROWSER=1 \
        POOLTIME_NO_VERSION_CHECK=1 \
        TMPDIR="$t/tmp" \
        PATH="$work/bin:/usr/bin:/bin" \
        sh -eu "$b" >"$t/out" 2>&1); then
        :
    else
        echo "check-docs: FAILED block at $where:" >&2
        sed 's/^/    /' "$b" >&2
        echo "  output:" >&2
        sed 's/^/    /' "$t/out" >&2
        failed=1
    fi
    ran=$((ran + 1))
done
echo "check-docs: ran $ran \`sh run\` blocks"

# ---- 2. every named subcommand exists -------------------------------------------------------
subcommands() {
    # The names listed under "Commands:" in `pooltime [group] --help`.
    # shellcheck disable=SC2086 # $1 is a single optional word
    "$bin" $1 --help | awk '/^Commands:/ { f = 1; next } f && /^[^ ]/ { f = 0 } f && NF { print $1 }'
}
top=$(subcommands "")
groups=""
for g in $top; do
    [ "$g" = help ] && continue
    if "$bin" "$g" --help | grep '^Commands:' >/dev/null; then
        groups="$groups $g"
        subcommands "$g" >"$work/sub.$g"
    fi
done

# `pooltime word [word]` inside code blocks, and in inline code spans elsewhere.
mentions() {
    awk '
        function scan(s, re,   m) {
            while (match(s, re)) {
                m = substr(s, RSTART, RLENGTH)
                sub(/^[^p]*pooltime /, "", m)
                print FILENAME ":" FNR ": " m
                s = substr(s, RSTART + RLENGTH)
            }
        }
        /^```/ {
            if (fence) { fence = 0; shell = 0 } else { fence = 1; shell = (substr($0, 4) ~ /^(sh|bash|shell)?([ \t]|$)/) }
            next
        }
        fence && shell { scan($0, "(^|[^A-Za-z0-9_./-])pooltime [a-z][a-z-]*( [a-z][a-z-]*)?"); next }
        fence { next }
        { scan($0, "`pooltime [a-z][a-z-]*( [a-z][a-z-]*)?") }
    ' "$@"
}

# shellcheck disable=SC2086 # $files is a list of paths without spaces
mentions $files >"$work/mentions"
checked=0
while IFS= read -r line; do
    where=${line%%: *}
    words=${line#*: }
    first=${words%% *}
    second=""
    [ "$first" != "$words" ] && second=${words#* }
    checked=$((checked + 1))
    if ! printf '%s\n' "$top" | grep -qx "$first"; then
        echo "check-docs: $where: \`pooltime $first\` is not a pooltime subcommand" >&2
        failed=1
        continue
    fi
    case " $groups " in
    *" $first "*)
        if [ -n "$second" ] && ! grep -qx "$second" "$work/sub.$first"; then
            echo "check-docs: $where: \`pooltime $first $second\` is not a pooltime subcommand" >&2
            failed=1
        fi
        ;;
    esac
done <"$work/mentions"
echo "check-docs: checked $checked \`pooltime <subcommand>\` mentions"

if [ "$failed" -ne 0 ]; then
    echo "check-docs: FAILED" >&2
    exit 1
fi
echo "check-docs: ok"
