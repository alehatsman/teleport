#!/usr/bin/env bash
# Copies the daemon binary into desktop/src-tauri/binaries/, renamed with the
# target triple Tauri's `externalBin` convention expects
# (docs/08-packaging.md#build-pipeline, docs/11-mvp-plan.md#m10).
#
# Usage: scripts/copy-sidecar.sh [target-triple]
# Defaults to the host triple (`rustc --print host-tuple`) for local dev;
# CI passes the cross-build target explicitly.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target="${1:-$(rustc --print host-tuple)}"

exe_name="teleportd"
[[ "$target" == *windows* ]] && exe_name="teleportd.exe"

src="$repo_root/daemon/target/$target/release/$exe_name"
if [[ ! -f "$src" ]]; then
  # Also try the untargeted release dir (host builds via `cargo build
  # --release` without --target land here, not under target/<triple>/).
  src="$repo_root/daemon/target/release/$exe_name"
fi
if [[ ! -f "$src" ]]; then
  echo "error: daemon binary not found for target $target (looked in daemon/target/$target/release and daemon/target/release)" >&2
  echo "build it first: (cd daemon && cargo build --release --features embedded-web)" >&2
  exit 1
fi

dest_dir="$repo_root/desktop/src-tauri/binaries"
mkdir -p "$dest_dir"
dest="$dest_dir/teleportd-$target"
[[ "$exe_name" == *.exe ]] && dest="$dest.exe"

cp "$src" "$dest"
echo "copied $src -> $dest"
