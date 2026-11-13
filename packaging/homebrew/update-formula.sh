#!/bin/sh
# Fill a formula with a release's URLs and sha256s. The release workflow runs it on the tap's
# Formula/pooltime.rb after each release; by hand, give it a checkout of chrisjenx/homebrew-tap:
#
#   packaging/homebrew/update-formula.sh <version> [formula]
#
# (the default formula, packaging/homebrew/pooltime.rb, is only the template).
#
# The checksums come from the release's SHA256SUMS asset, fetched from
# $POOLTIME_RELEASE_BASE/v<version>/SHA256SUMS (default base:
# https://github.com/chrisjenx/pooltime/releases/download), or read from the local file
# $POOLTIME_SHA256SUMS when that is set. Fails unless all four targets are listed.
set -eu

die() {
    printf 'update-formula: error: %s\n' "$*" >&2
    exit 1
}

[ $# -ge 1 ] && [ $# -le 2 ] || die "usage: $0 <version> [formula]"
version="${1#v}"
case "$version" in
    '' | *[!0-9A-Za-z.+-]*) die "invalid version: $1" ;;
esac
formula="${2:-$(dirname "$0")/pooltime.rb}"
[ -f "$formula" ] || die "formula not found: $formula"

base="${POOLTIME_RELEASE_BASE:-https://github.com/chrisjenx/pooltime/releases/download}"
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t pooltime-formula)
trap 'rm -rf "$tmp"' EXIT

if [ -n "${POOLTIME_SHA256SUMS:-}" ]; then
    cp "$POOLTIME_SHA256SUMS" "$tmp/SHA256SUMS"
else
    # Retry briefly (a 404 included): right after a release is published the asset may not be visible yet.
    curl -fsSL --retry 6 --retry-delay 5 --retry-all-errors -o "$tmp/SHA256SUMS" "${base}/v${version}/SHA256SUMS" ||
        die "could not download ${base}/v${version}/SHA256SUMS"
fi

for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    asset="pooltime-${version}-${target}.tar.gz"
    sum=$(awk -v a="$asset" '$2 == a || $2 == "*" a { print $1 }' "$tmp/SHA256SUMS")
    printf '%s' "$sum" | grep -Eq '^[0-9a-f]{64}$' || die "no sha256 for $asset in SHA256SUMS"
    printf '%s %s\n' "$target" "$sum" >>"$tmp/sums"
done

# Rewrite every release URL (the formula's version comes from them), then the sha256 that follows each URL
# (matched by the target in that URL).
awk -v version="$version" -v sums="$tmp/sums" '
    BEGIN {
        while ((getline line < sums) > 0) {
            split(line, f, " ")
            sha[f[1]] = f[2]
        }
    }
    /url "https:\/\/github.com\/chrisjenx\/pooltime\/releases\/download\// {
        target = ""
        for (t in sha) if (index($0, "-" t ".tar.gz\"")) target = t
        if (target == "") {
            print "update-formula: error: no known target in: " $0 > "/dev/stderr"
            bad = 1
            exit 1
        }
        sub(/download\/v[^\/]*\/pooltime-[^"]*\.tar\.gz/, \
            "download/v" version "/pooltime-" version "-" target ".tar.gz")
        pending = target
    }
    /sha256 "[0-9a-f]*"/ && pending != "" {
        sub(/sha256 "[0-9a-f]*"/, "sha256 \"" sha[pending] "\"")
        filled++
        pending = ""
    }
    { print }
    END {
        if (bad) exit 1
        if (filled != 4) {
            print "update-formula: error: filled " filled + 0 " sha256 lines, expected 4" > "/dev/stderr"
            exit 1
        }
    }
' "$formula" >"$tmp/pooltime.rb" || die "could not update $formula"

cp "$tmp/pooltime.rb" "$formula"
printf 'update-formula: %s now installs pooltime %s\n' "$formula" "$version"
