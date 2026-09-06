#!/bin/sh
# Installs the latest (or a requested) teleportd release binary.
# docs/16-release-pipeline.md#scriptsinstallsh
#
#   curl -fsSL https://raw.githubusercontent.com/alehatsman/teleport/main/scripts/install.sh | sh
#
# Env overrides:
#   TELEPORT_VERSION      tag to install, e.g. "v0.1.0" (default: latest release)
#   TELEPORT_INSTALL_DIR  where to put the binary (default: $HOME/.local/bin)
#
# POSIX sh only -- no bashisms -- because `curl | sh` runs under whatever
# /bin/sh is, not necessarily bash.
set -eu

repo="alehatsman/teleport"
install_dir="${TELEPORT_INSTALL_DIR:-$HOME/.local/bin}"

log() { printf '%s\n' "$*" >&2; }
die() {
    log "error: $*"
    exit 1
}

# --- detect target triple -------------------------------------------------

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Linux) os_part="unknown-linux-gnu" ;;
    Darwin) os_part="apple-darwin" ;;
    *)
        die "unsupported OS '$os'. Windows users: grab the .zip from
  https://github.com/$repo/releases and unzip it yourself -- this script
  only handles Linux and macOS."
        ;;
esac

case "$arch" in
    x86_64 | amd64) arch_part="x86_64" ;;
    arm64 | aarch64)
        if [ "$os" = "Linux" ]; then
            die "no Linux arm64 release yet (docs/16-release-pipeline.md#targets).
  Build from source: https://github.com/$repo#readme"
        fi
        arch_part="aarch64"
        ;;
    *) die "unsupported architecture '$arch'" ;;
esac

target="${arch_part}-${os_part}"
archive="teleport-${target}.tar.gz"

# --- resolve version -------------------------------------------------------

if [ -n "${TELEPORT_VERSION:-}" ]; then
    version="$TELEPORT_VERSION"
else
    log "resolving latest release..."
    version="$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" \
        | grep '"tag_name":' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    [ -n "$version" ] || die "could not resolve the latest release version"
fi

base_url="https://github.com/$repo/releases/download/$version"

# --- download + verify ------------------------------------------------------

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

log "downloading $archive ($version)..."
curl -fsSL -o "$work_dir/$archive" "$base_url/$archive" \
    || die "failed to download $base_url/$archive (bad version, or no release for $target yet)"
curl -fsSL -o "$work_dir/checksums.txt" "$base_url/checksums.txt" \
    || die "failed to download checksums.txt for $version"

expected="$(grep "  $archive\$" "$work_dir/checksums.txt" | awk '{print $1}')"
[ -n "$expected" ] || die "$archive is not listed in checksums.txt"

actual="$(cd "$work_dir" && sha256sum "$archive" | awk '{print $1}')"
[ "$expected" = "$actual" ] || die "checksum mismatch for $archive (expected $expected, got $actual)"

# --- install ----------------------------------------------------------------

tar -xzf "$work_dir/$archive" -C "$work_dir"
mkdir -p "$install_dir"
cp "$work_dir/teleport-$target/teleportd" "$install_dir/teleportd"
chmod +x "$install_dir/teleportd"
log "installed teleportd $version to $install_dir/teleportd"

# `teleport` (the CLI client, docs/11-mvp-plan.md#m11--cli-client) rides in
# the same archive as of that milestone -- older releases don't have it, so
# its absence here is not an error.
if [ -f "$work_dir/teleport-$target/teleport" ]; then
    cp "$work_dir/teleport-$target/teleport" "$install_dir/teleport"
    chmod +x "$install_dir/teleport"
    log "installed teleport $version to $install_dir/teleport"
fi

case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) log "note: $install_dir is not on your PATH. Add it, e.g.:
  export PATH=\"$install_dir:\$PATH\"" ;;
esac

log "run it: teleportd   (then open http://127.0.0.1:7337)"
