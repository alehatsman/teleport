# 13 — Native mobile and desktop clients (post-MVP)

Native iOS/Android apps are **out of MVP scope**. This doc exists so the MVP does not
accidentally make them expensive.

## Two phases, two different blockers

"Native iOS app" reads like one project. It isn't — it splits into two pieces with
different dependencies, and conflating them is what makes the whole thing look stuck
behind the MVP when most of it is not.

| | **Phase 1 — Tailscale-only shell** | **Phase 2 — account/push** |
|---|---|---|
| Needs | daemon + API (M0–M9, delivered) | [cloud backend](14-cloud-backend.md) — not started |
| Auth | `Principal::DeviceToken`, existing bearer token ([12](12-identity-and-connectivity.md#the-principal)) | `Principal::Account`, pairing, device credential |
| Reachability | Tailscale Serve, same as today's browser client | outbound relay |
| Gives you | session list, attach, control, from your phone | push ("agent is waiting"), multi-device list, no manual URL/token entry |
| Blocked on M10 (Tauri)? | **no** — M10 is desktop packaging, orthogonal | no |
| Blocked on `cloud/`? | **no** | **yes** |

**Phase 1 is buildable now, in parallel with M10.** Every server-side obligation it
needs (bearer-on-WS, Origin-optional for credentialed clients, bounded `tail`, `/health`
capabilities) is already built and listed in the obligations table below. Nothing in
this doc's "post-MVP" framing is a technical blocker for Phase 1 — it's just the reason
nobody built it yet. Phase 2 is genuinely gated: push and pairing cannot exist before
`cloud/` does, per [14-cloud-backend.md](14-cloud-backend.md#sequencing).

The rest of this doc was written before that split existed and describes the end
state (both phases together). [Phase 1 implementation plan](#phase-1-implementation-plan)
at the bottom scopes down to what's actually buildable first.

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

**Done in M8** — both signals land on the session-list payload
([04-api-protocol.md](04-api-protocol.md#get-apiv1sessions):
`last_bell_ms`/`idle_since_ms`), and `Sessions.svelte` shows a badge for a running
session that needs attention. What's still open for M10: an actual OS tray
notification reading these fields, and the idle threshold is a single hardcoded
constant (`session::IDLE_THRESHOLD_MS`, 30s), not the per-preset knob this table
calls for.

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

## Phase 1 implementation plan

Scoped to what needs no cloud backend: manual daemon URL + token entry (paste or QR,
no pairing flow), Tailscale reachability, no push. Requires Xcode/macOS — work resumes
on a Mac, this plan is the handoff.

**Repo shape** (per [01-architecture.md](01-architecture.md#repository-layout)'s
`mobile/` placeholder):

```text
mobile/
└── ios/
    └── Teleport/
        ├── App.swift              — entry point, deep-link routing
        ├── ConnectView.swift      — daemon URL + token entry, stored in Keychain
        ├── SessionListView.swift  — GET /api/v1/sessions, 3s poll while visible (D2)
        ├── TerminalView.swift     — WKWebView hosting web/'s existing Terminal.svelte
        ├── KeyboardAccessory.swift — Esc Tab Ctrl ↑↓←→ Ctrl-C bar
        ├── BiometricGate.swift    — Face ID gate before showing terminal content
        └── Lifecycle.swift        — foreground reconnect (tail=), background close
```

**Step order (riskiest assumption first, same discipline as the M1 spike):**

1. **Connectivity spike, no UI.** A bare `URLSessionWebSocketTask` from a throwaway
   Swift command-line target, dialing a real daemon over Tailscale with
   `Authorization: Bearer <token>` on the upgrade. This exact path
   (native client, no `Origin`, credential on WS) is spec'd in
   [12](12-identity-and-connectivity.md#client-classes-and-why-origin-is-not-universal)
   and listed as an MVP obligation, but has never been driven by a real native
   client — every test of it so far is the browser or Rust-side tests. Confirm the
   attach handshake, offset framing, and bounded `tail` behave as documented before
   writing any Swift UI on top of an assumption.
2. **`TerminalView`**: WKWebView pointed at the daemon's own served SPA
   (`http://<tailnet-host>:<port>/?token=…`) — reuse the whole web app inside the
   WebView rather than reimplementing the terminal component natively. Confirms the
   "native shell, WebView terminal" shape end to end before building navigation
   around it.
3. **`ConnectView` + Keychain** — manual URL/token entry, since pairing doesn't exist
   yet. This is deliberately the crude Phase 1 UX; Phase 2 replaces it with QR pairing.
4. **`SessionListView`** — session list + polling (D2), badge on `last_bell_ms`/
   `idle_since_ms` (already on the M8 session payload) even with no push behind it yet.
5. **`Lifecycle.swift`** — foreground/background per the [Lifecycle](#lifecycle)
   section above; this is the part most likely to reveal iOS-specific surprises
   (background execution time limits, WKWebView suspension behavior) and should be
   tested against real backgrounding, not the simulator's lifecycle, before trusting it.
6. **`KeyboardAccessory` + `BiometricGate`** — polish, lowest risk, do last.

**Validation:** each step gets a real device + real Tailscale connection test before
moving on, not a simulator-only pass — steps 1 and 5 in particular are exactly the kind
of "looks right, isn't" gap this project's spike culture exists to catch
([15-open-questions.md](15-open-questions.md)).

**Explicitly deferred to Phase 2, do not build early:** push, pairing/QR enrollment,
device list, local-network direct-connect shortcut.
