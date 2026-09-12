#!/usr/bin/env bash
# Stops what up.sh started. Matches on --data-dir .dev-data specifically so
# this can never touch a provisioned/live teleportd instance running
# elsewhere (e.g. :7337 from ~/dotfiles/components/teleport).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
serve_port="${MOBILE_DEV_SERVE_PORT:-8443}"

if pkill -f "target/debug/teleportd .*--data-dir $repo_root/.dev-data"; then
  echo "==> stopped dev teleportd"
else
  echo "==> no dev teleportd running"
fi

if pkill -f "$repo_root/web/node_modules/.bin/vite"; then
  echo "==> stopped vite"
else
  echo "==> no vite running"
fi

if tailscale serve --https="$serve_port" off 2>/dev/null; then
  echo "==> removed Tailscale Serve mapping on :$serve_port"
else
  echo "==> no Tailscale Serve mapping on :$serve_port"
fi
