#!/usr/bin/env bash
# Runs (almost) everything .github/workflows/ci.yml runs, locally in Docker,
# so pushing to a real CI run isn't the first time you find out something's
# broken. Mirrors each CI job as one docker run; cargo/npm state is cached in
# named volumes so repeat runs are fast, not fresh-container-every-time slow.
#
# What this can't do: macOS. Docker containers share the host Linux kernel --
# there is no macOS kernel to run in a container on non-Apple hardware, so
# nothing here can exercise real macOS pty/kernel behavior. That's still
# real-CI-only (see docs/15-open-questions.md's S5/N5 for why that gap
# specifically matters here). Everything else -- Linux native, Windows
# cross-compile, web, fmt, audit -- runs here.
#
# Usage: scripts/ci-local.sh [job...]
#   No args: run everything.
#   Args: any of fmt, test, clippy, audit, windows, web -- run just those.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

RUST_IMAGE="rust:1-bookworm"
NODE_IMAGE="node:22-bookworm"
CARGO_CACHE="teleport-cargo-cache"   # named volume: ~/.cargo/registry + git
TARGET_CACHE="teleport-target-cache" # named volume: daemon/target

daemon_run() {
    # --init: without it, PID 1 in the container never reaps a reparented
    # zombie (Docker's default), and daemon/tests/pty_primitive.rs's
    # terminate_kills_the_grandchild_process_tree fails spuriously --
    # kill(pid, 0) can't tell "still running" from "exited, awaiting reap".
    # Same root cause considered (and correctly ruled out for real CI, which
    # runs on VMs with a real init) while chasing macOS's eof_and_exit flake
    # -- see docs/15-open-questions.md#s5.
    docker run --rm --init \
        -v "$(pwd)/daemon:/work" -w /work \
        -v "$CARGO_CACHE:/usr/local/cargo/registry" \
        -v "$TARGET_CACHE:/work/target" \
        "$RUST_IMAGE" bash -euc "$1"
}

job_fmt() {
    echo "== daemon (fmt) =="
    daemon_run "rustup component add rustfmt && cargo fmt --check"
}

job_test() {
    echo "== daemon (ubuntu-equivalent): build + test =="
    daemon_run "cargo build --all-targets && cargo test --no-fail-fast"
}

job_clippy() {
    echo "== daemon (ubuntu-equivalent): clippy =="
    daemon_run "rustup component add clippy && cargo clippy --all-targets -- -D warnings"
}

job_audit() {
    echo "== daemon (audit) =="
    daemon_run "cargo install cargo-audit -q --locked && cargo audit"
}

job_windows() {
    echo "== daemon (windows cross-compile: build + clippy) =="
    # This is exactly what the M4 gate in docs/11-mvp-plan.md means by
    # "Windows: cross-compile-checked only" -- it proves the crate builds and
    # lints clean for the target, not that ConPTY/pty behavior is correct.
    daemon_run "
        rustup target add x86_64-pc-windows-gnu
        rustup component add clippy
        apt-get update -qq && apt-get install -qq -y gcc-mingw-w64-x86-64 >/dev/null
        cargo build --all-targets --target x86_64-pc-windows-gnu
        cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings
    "
}

job_web() {
    echo "== web =="
    docker run --rm \
        -v "$(pwd)/web:/work" -w /work \
        -v teleport-npm-cache:/root/.npm \
        "$NODE_IMAGE" bash -euc "npm ci && npm run check && npm run build"
}

ALL_JOBS=(fmt test clippy audit windows web)
requested=("${@:-${ALL_JOBS[@]}}")

echo "Running: ${requested[*]}"
echo "(macOS is not verifiable locally -- Docker can't run a macOS kernel on this hardware. Push to real CI for that, or see docs/15-open-questions.md S5/N5.)"
echo

failed=()
for job in "${requested[@]}"; do
    if ! "job_$job"; then
        failed+=("$job")
    fi
    echo
done

if [ "${#failed[@]}" -eq 0 ]; then
    echo "All requested jobs passed."
else
    echo "FAILED: ${failed[*]}"
    exit 1
fi
