#!/bin/sh
# pooltime installer: downloads a release binary, verifies its sha256 and installs it.
#
#   curl -fsSL https://raw.githubusercontent.com/chrisjenx/pooltime/main/install.sh | sh
#
# Environment:
#   POOLTIME_VERSION       version to install, e.g. 1.0.0 (default: the latest release)
#   POOLTIME_INSTALL_DIR   where to put the binary (default: $HOME/.local/bin)
#   POOLTIME_RELEASE_BASE  release download base URL
#                          (default: https://github.com/chrisjenx/pooltime/releases/download)
#
# Never uses sudo. Supports macOS and Linux on arm64/aarch64 and x86_64.
set -eu

REPO="chrisjenx/pooltime"
RELEASE_BASE="${POOLTIME_RELEASE_BASE:-https://github.com/${REPO}/releases/download}"
INSTALL_DIR="${POOLTIME_INSTALL_DIR:-${HOME}/.local/bin}"

say() {
    printf 'pooltime-install: %s\n' "$*"
}

die() {
    printf 'pooltime-install: error: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || die "'$1' is required but was not found"
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        arm64 | aarch64) arch=aarch64 ;;
        x86_64 | amd64) arch=x86_64 ;;
        *) die "unsupported architecture: $arch (supported: arm64/aarch64, x86_64)" ;;
    esac
    # A shell running under Rosetta reports x86_64 on Apple silicon; prefer the native build.
    if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] &&
        [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
        arch=aarch64
    fi
    case "$os" in
        Darwin) echo "${arch}-apple-darwin" ;;
        Linux) echo "${arch}-unknown-linux-musl" ;;
        *) die "unsupported OS: $os (supported: macOS, Linux)" ;;
    esac
}

download() {
    # $1 = URL, $2 = output file. https only, except for an explicit http:// URL (local dry runs).
    if command -v curl >/dev/null 2>&1; then
        case "$1" in
            http://*) proto='=https,http' ;;
            *) proto='=https' ;;
        esac
        curl -fsSL --proto "$proto" --proto-redir "$proto" -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        die "'curl' or 'wget' is required"
    fi
}

latest_version() {
    tag=
    if command -v curl >/dev/null 2>&1; then
        # /releases/latest redirects to /releases/tag/<tag>; this avoids the rate-limited API.
        final=$(curl -fsSLI --proto '=https' --proto-redir '=https' -o /dev/null -w '%{url_effective}' \
            "https://github.com/${REPO}/releases/latest" 2>/dev/null || true)
        case "${final##*/}" in
            v[0-9]* | [0-9]*) tag="${final##*/}" ;;
        esac
    fi
    if [ -z "$tag" ]; then
        api="https://api.github.com/repos/${REPO}/releases/latest"
        json="$tmp/latest.json"
        download "$api" "$json" || die "could not query the latest release ($api); set POOLTIME_VERSION"
        tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$json" | head -n 1)
    fi
    [ -n "$tag" ] || die "could not find the latest release tag; set POOLTIME_VERSION"
    echo "${tag#v}"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "'sha256sum' or 'shasum' is required to verify the download"
    fi
}

main() {
    need uname
    need tar
    need mktemp

    target=$(detect_target)

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t pooltime)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    if [ -n "${POOLTIME_VERSION:-}" ]; then
        version="${POOLTIME_VERSION#v}"
    else
        version=$(latest_version)
    fi

    asset="pooltime-${version}-${target}.tar.gz"
    url="${RELEASE_BASE}/v${version}/${asset}"

    say "downloading pooltime ${version} for ${target}"
    download "$url" "$tmp/$asset" || die "download failed: $url"
    download "${url}.sha256" "$tmp/${asset}.sha256" || die "download failed: ${url}.sha256"

    expected=$(awk 'NR == 1 {print $1}' "$tmp/${asset}.sha256")
    listed=$(awk 'NR == 1 {print $2}' "$tmp/${asset}.sha256")
    listed="${listed#\*}"
    printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' ||
        die "malformed checksum file for ${asset}: expected a 64-character lowercase hex sha256"
    [ "$listed" = "$asset" ] ||
        die "checksum file names '${listed:-nothing}', expected '${asset}'; not installing"
    actual=$(sha256_of "$tmp/$asset")
    if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
        die "checksum mismatch for ${asset} (expected ${expected:-nothing}, got ${actual}); not installing"
    fi
    say "checksum verified"

    mkdir "$tmp/unpack"
    tar -xzf "$tmp/$asset" -C "$tmp/unpack" pooltime 2>/dev/null || die "${asset} does not contain a pooltime binary"
    [ -f "$tmp/unpack/pooltime" ] && [ ! -L "$tmp/unpack/pooltime" ] || die "${asset} does not contain a pooltime binary"

    mkdir -p "$INSTALL_DIR"
    # Copy then rename so a running pooltime is never overwritten in place.
    cp "$tmp/unpack/pooltime" "$INSTALL_DIR/.pooltime.tmp.$$"
    chmod 755 "$INSTALL_DIR/.pooltime.tmp.$$"
    mv -f "$INSTALL_DIR/.pooltime.tmp.$$" "$INSTALL_DIR/pooltime"
    say "installed $("$INSTALL_DIR/pooltime" --version) to $INSTALL_DIR/pooltime"

    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            say "warning: $INSTALL_DIR is not on your PATH; add it, e.g."
            say "  export PATH=\"$INSTALL_DIR:\$PATH\""
            ;;
    esac

    say "next: run 'pooltime setup'"
}

main "$@"
