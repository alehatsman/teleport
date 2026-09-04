# 13 — Native mobile and desktop clients (post-MVP)

Native iOS/Android apps are **out of MVP scope**. This doc exists so the MVP does not
accidentally make them expensive.

## The protocol is already native-ready

This is not an accident and it should be preserved deliberately:

- **Raw VT bytes on the wire.** The daemon streams terminal output unmodified. Any
  client with a VT emulator can render it. Nothing is HTML, JSON-wrapped, or
  browser-shaped.
- **Byte offsets as the only cursor.** No session affinity, no server-side per-client
  state to restore, no sticky routing. A client reconnects with an integer.
- **One HTTP + WebSocket API for everyone.** Desktop, web, and phone use the same
  endpoints ([01-architecture.md](01-architecture.md#what-this-eliminates)).

The only thing that would break this is adding a browser assumption to the protocol.
Do not.

## Four obligations the MVP must honor

Each is small, and each is expensive to retrofit.

| # | Obligation | Where |
|---|---|---|
| 1 | `Origin` is required only of clients that send it; native clients authenticate instead | [12](12-identity-and-connectivity.md#client-classes-and-why-origin-is-not-universal) |
| 2 | `Authorization: Bearer` accepted on the WS upgrade, not only `?token=` | [06](06-security.md#authentication) |
| 3 | Bounded attach (`tail`) so a phone never replays a 500 MB log | [04](04-api-protocol.md#bounded-attach) |
| 4 | `/health` advertises `api_versions` + `capabilities` for version skew | [04](04-api-protocol.md#get-apiv1health) |

Obligation 4 matters more for native than anything else here: an app store build can be
months behind the daemon a user just updated, and you cannot force either side to match.
The client must be able to ask what it is talking to.

## Terminal rendering: use a WebView first

| Option | Effort | Fidelity | Verdict |
|---|---:|---|---|
| **WebView + xterm.js** | Low | Identical to web by construction | **Start here** |
| Native VT emulator (e.g. SwiftTerm on iOS; a Termux-derived emulator on Android) | High | Better scroll/keyboard feel, lower memory | Only if the WebView proves measurably bad |
| Server-side render to a screen grid | Very high | — | **Reject** — the daemon would need a full VT parser, which is terminal-state snapshots wearing a disguise |

The recommended shape is **native shell, WebView terminal**:

```text
native  ── navigation, session list, device list
        ── push notifications
        ── biometric unlock
        ── keyboard accessory bar (Esc Tab Ctrl ↑↓←→ Ctrl-C)
        ── background/foreground lifecycle
   │
   └── WebView ── xterm.js ── the same Terminal component the web app uses
```

One terminal implementation, three platforms. Two native VT emulators would mean two
new sources of rendering bugs that must stay bug-compatible with xterm.js — and terminal
emulation edge cases are exactly where "it looks slightly wrong" reports come from.

Evaluate the native-emulator option only against a measured complaint (scroll latency,
memory under long output). Do not pre-optimize it.

## Lifecycle

Mobile OSes suspend apps aggressively. The WebSocket is dead after backgrounding; assume
it, do not detect it.

```text
foreground
    ↓
reconnect immediately with ?tail=<bytes>       ← not after=0, not the old offset
    ↓                                            if the gap is large
render, resume live stream

background
    ↓
close the socket cleanly (do not fight the OS for it)
    ↓
rely on push for attention events
```

Reconnect policy on foreground:

- gap since last offset is **small** (< 1 MiB) → `after=<last_offset>`, seamless
- gap is **large** or unknown → `tail=<bytes>`, accept the VT-state caveat in
  [04-api-protocol.md](04-api-protocol.md#bounded-attach)
- session is `exited` or `lost` → do not stream at all; fetch the tail of `/log`

A cheap "what changed" poll matters more than it looks: an app resuming from three days
of suspension should hit `GET /api/v1/sessions` once and render, not open five sockets.

## Push notifications

This is the feature that justifies installing the app: *an agent is waiting for you.*

```text
daemon detects attention event
    ↓
POST to cloud backend (device credential)
    ↓
backend fans out to APNs / FCM
    ↓
phone shows "codex is waiting for input — my-project"
```

APNs and FCM both require a server to hold credentials — this is a
[cloud backend](14-cloud-backend.md) feature and cannot be done daemon-to-phone.

### Detection heuristics

The daemon owns a PTY, not a protocol, so "waiting for input" is inferred, never known.
Ranked by reliability:

| Signal | Reliability | Notes |
|---|---|---|
| Process exited | Exact | Always notify (with exit code) |
| **BEL (`\x07`) in the output stream** | Good | Many CLIs ring the bell on completion or when prompting. Cheap to detect in the reader loop. |
| Output idle > N seconds while the process is alive | Decent | Tunable per preset; noisy for long builds |
| Preset-specific output pattern | Best per-agent | A regex on the preset; opt-in, not a framework |

**MVP hook (small, worth taking now):** the reader loop already scans every byte. Emit a
`session_events` row on BEL and on output-idle-while-alive. That is a handful of lines
in `session.rs`, it costs nothing, and it means the notification feature later is
"deliver existing events" rather than "add detection to the hot path."

Do **not** build an agent-protocol integration for this. Presets are metadata
([01-architecture.md](01-architecture.md#sessions-and-agents-are-the-same-thing)).

## Multi-device UX

The endpoint the phone actually needs is a **device list**, and it lives in the cloud
backend, not the daemon — a daemon only knows itself.

```text
Phone
 ├── aleh-macbook      ● online   3 sessions
 │    ├── codex        running    my-project
 │    ├── claude       running    teleport
 │    └── zsh          exited
 ├── aleh-desktop      ● online   1 session
 └── build-box         ○ offline  last seen 2h ago
```

The daemon's contribution is `device_id` / `device_name` on `/health`
([12](12-identity-and-connectivity.md#device-identity)). Everything else is backend
aggregation.

## Practical notes

- **App Store review:** an app that runs commands on the user's own machine is an
  established category (SSH and remote-desktop clients). Be ready to explain that
  execution is on hardware the user owns and controls, gated by their account. Do not
  describe it as running arbitrary code on *other people's* machines.
- **Cold start into a specific session** via deep link (`teleport://device/<id>/session/<id>`)
  from a push notification. The `session_id` is a ULID and already globally unique.
- **Never cache terminal output** in app storage without encryption at rest — same
  reasoning as [06-security.md](06-security.md#terminal-logs-are-sensitive).
- **Local network shortcut:** if the phone and daemon are on the same LAN, connecting
  directly beats relaying. Worth doing eventually; it is a latency and cost win, not a
  correctness one.
