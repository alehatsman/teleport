# mobile-dev — observe teleport's web UI on a real phone, from an agent

Tools for the loop: edit `web/` → see it render on an actual iPhone → verify a
fix actually fixed something → repeat. Built for an agent with no GUI access
of its own. Everything here is dev-only scaffolding, not part of the product.

## Why this exists

`docs/09-frontend.md#mobile` says a lot of the mobile feature set only exists
in a secure context (HTTPS or localhost) and that "reaching a phone properly
means Tailscale Serve." This directory is the concrete, scriptable version of
that: an isolated dev daemon, a Vite dev server with HMR, a Tailscale Serve
mapping to reach it from a phone, and two observation tools (Mac Chrome via
real CDP, and a real iOS device via Apple's own `safaridriver`).

## Quick start

```bash
scripts/mobile-dev/up.sh
```

Prints two URLs: one for direct `curl`/API use, one to open on the phone.
Both carry the dev instance's own auth token in the query string (stripped
from the address bar on first load, per `docs/06-security.md#authentication`).

```bash
scripts/mobile-dev/down.sh   # tear it down when you're done
```

## Prerequisites

- `tailscale up` already done on this machine, tailnet reachable from the phone.
- Google Chrome installed at the standard macOS path (for `chrome.mjs`).
- For real-device testing: the iPhone paired over USB, **Settings → Safari →
  Advanced → Web Inspector** on, and `safaridriver --enable` run once
  interactively (it needs your macOS password — `up.sh`/`phone.mjs` can't do
  this step for you; run it yourself if `phone.mjs start` fails silently).

## The two observation channels

| | `chrome.mjs` | `phone.mjs` |
|---|---|---|
| What it drives | Headless Mac Chrome (Blink) | The real iPhone's real Safari (WebKit) |
| Fidelity | Approximate — misses Safari-only behavior (safe-area/notch, PWA install, keyboard avoidance) | Real |
| Setup cost | None beyond Chrome being installed | Cable, one-time Web Inspector toggle, `safaridriver --enable` |
| Use for | Fast day-to-day CSS/layout iteration | Anything Safari-specific, or a final check before calling a fix done |

Chrome-for-iOS is still WebKit under the hood (Apple mandates it for every
iOS browser), so `phone.mjs` covers it too regardless of which browser the
phone is actually running — Chrome-for-iOS just isn't independently
inspectable (see the dead end below).

## Hard-won gotchas — read before debugging something that looks broken

**1. `chrome --headless=new --screenshot=out.png <url>` lies.** This
project's own `web/CLAUDE.md` used to recommend it for visual checks. It
races the window resize against the first paint and can render a stale/wrong
box for one element while everything else looks correct. It produced a fully
reproducible false "header button overflows the viewport" bug report —
confirmed wrong by a properly-sequenced CDP session (set device metrics
*before* navigating, wait for `readyState === 'complete'` **and** a settle
delay, since Vite injects CSS via JS not a `<link>` tag, *then* capture).
`chrome.mjs` does it the right way. Don't reach for the bare CLI flag.

**2. The Host check strips the port; the Origin check doesn't.**
`daemon/src/auth.rs`: `allowed_hosts` entries must be the bare hostname
(`mac.tail37c478.ts.net`), `allowed_origins` entries must include the port
(`https://mac.tail37c478.ts.net:8443`). Get this backwards and you get a
confusing partially-broken state: plain `GET`s keep working (Host-only,
tolerant), but every mutating request — `ws-ticket`, session create/delete —
403s with `"Origin or Host rejected"`, which looks like the UI is stuck
"Connecting" forever with no other symptom. `up.sh` generates this file
correctly; if you hand-edit `.dev-data/config.toml`, get this right.

**3. A "lost" session is not a bug.** Restarting the dev daemon (a plain
`kill` + restart, not a graceful shutdown) orphans any session it had —
`docs/01-architecture.md#the-crash-boundary` is explicit that a live PTY does
not survive a daemon crash/restart, only the metadata and log do. You'll see
`ws-ticket` 404 with `"session not found"` even though `GET /sessions` still
lists the row — check `state`/`lost_reason` in that response before assuming
anything is broken; `"lost"` / `"daemon_restart"` means exactly what it says.
Create a fresh session after restarting `up.sh`.

**4. WebDriver's own `element/click` and key-sending are unreliable on a
real device, here.** Silent empty-body 400s on some buttons (worked once for
one button, failed repeatedly on others with no diagnostic), and synthetic
touches sometimes land as a long-press/text-selection gesture instead of a
tap. `phone.mjs`'s `click`/`type`/`key`/`fill`/`submit` commands all work
around this by dispatching real DOM events via `execute/sync` script
injection instead of WebDriver's native touch/key simulation — same code
paths a real tap or keystroke takes, just triggered more reliably. Don't
"fix" these back to the native WebDriver endpoints.

**5. A ghost autosuggestion is not real buffered input.** Claude Code's own
REPL sometimes shows a dimmed follow-up suggestion in the prompt box after a
task finishes (real example: `auto mode` proposed a follow-up task on its
own). Pressing Enter against it does nothing — because there's no real text
in the PTY's line buffer yet, it's just a rendered hint. If a submission
looks like it didn't work, check something real before concluding a key
didn't register: `curl .../api/v1/sessions | jq '.sessions[0].output_bytes'`
before and after. A screenshot comparison alone isn't enough evidence either
way — see gotcha 1.

**6. `phone.mjs session` fails with "already paired with another WebDriver
session"** if a previous session on that device was never closed. Fix:
`curl -X DELETE http://127.0.0.1:<port>/session/<old-id>` first (or just
restart `safaridriver`).

**7. `ios-webkit-debug-proxy` (and anything built on it — RemoteDebug's
adapter, `ios-safari-remote-debug-kit`) is a dead end for automation here.**
It serves Chrome's actual DevTools frontend bundle, and Chrome's extension
security model flatly forbids one extension (`claude-in-chrome`, or any
other) from scripting into another extension's page — not a bug, a hard
platform boundary. `safaridriver` (Apple's own, ships with macOS, real
WebDriver REST API, **no Xcode needed for a real device** — that's only
required for `safari:useSimulator: true`) is what actually works.

## Files

- `up.sh` / `down.sh` — bring the isolated dev daemon + Vite + Tailscale
  Serve mapping up/down. Never touches a provisioned/live teleportd — picks
  its own port (default 7339) and data dir (`.dev-data/`).
- `chrome.mjs` — `launch`, `shot <url> [w] [h] [out]`, `eval <url> <js> [w]`.
- `phone.mjs` — `devices`, `start`, `session <udid>`, `nav`, `shot`, `eval`,
  `click <selector>`, `fill <selector> <value>`, `submit <formSelector>`,
  `type <text>`, `key <name>`. State (safaridriver port, session id)
  persists in `/tmp/mobile-dev-phone.json` so you don't re-pass them.

Run either with no arguments for a usage line.
