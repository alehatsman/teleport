# 09 — Frontend

Svelte + TypeScript + Vite, with xterm.js as the terminal. Vite produces static assets
that `teleportd` serves from the same origin as the API — so there is no CORS story, no
second server, and no SSR.

**Do not write a terminal renderer.** xterm.js is the terminal layer.

## Structure

```text
web/src/
├── main.ts
├── App.svelte              # routing between list and session views
├── lib/
│   ├── api.ts              # typed HTTP client for /api/v1
│   ├── stream.ts           # WebSocket client: framing, offsets, reconnect
│   ├── types.ts            # shared types mirroring the API doc
│   ├── Sessions.svelte     # session list, new-session/preset launcher
│   ├── Session.svelte      # one session: header, status, control lease UI
│   └── Terminal.svelte     # xterm.js, isolated
└── vite.config.ts
```

`Terminal.svelte` is the **only** file that imports xterm.js. Everything else deals in
session IDs and connection state. If a second component reaches into the xterm API, the
boundary has leaked.

## `stream.ts` — the part that must be right

This client owns the offset contract from
[04-api-protocol.md](04-api-protocol.md#offsets-are-the-replay-index).

```ts
type StreamState = "connecting" | "replaying" | "live" | "reconnecting" | "closed";

class SessionStream {
  private nextOffset = 0;          // bytes consumed so far — the reconnect cursor
  private backoff = 250;           // ms, →8000 with jitter
  private wantControl = false;     // sticky user intent, not connection state

  connect() {
    // With a cursor, resume exactly. Without one, take a bounded tail —
    // never after=0, which asks for the entire log.
    const cursor = this.hasCursor ? `after=${this.nextOffset}` : `tail=${DEFAULT_TAIL}`;

    // mode=control asks to *resume* a lease; it never preempts. Safe on reconnect.
    const mode = this.wantControl ? "control" : "observe";

    const ws = new WebSocket(
      `${wsBase}/api/v1/sessions/${this.id}/stream` +
        `?${cursor}&mode=${mode}&client_id=${CLIENT_ID}` +
        `&client_name=${encodeURIComponent(CLIENT_NAME)}&token=${TOKEN}`
    );
    ws.binaryType = "arraybuffer";
    // ...
  }

  private onReady(msg: ReadyFrame) {
    this.nextOffset = msg.replay_from;   // trust the server's replay start
    this.hasCursor = true;
    this.hasControl = msg.control;       // may be false even when we asked
    this.setPtySize(msg.cols, msg.rows); // observers letterbox to this
    if (msg.truncated) this.onTruncated(); // → term.reset() before the first chunk
  }

  // Explicit user action only. This is the one call that preempts.
  takeControl() {
    this.wantControl = true;
    this.send({ type: "claim_control" });
  }

  private onBinary(buf: ArrayBuffer) {
    const view = new DataView(buf);
    const offset = view.getBigUint64(0, false);   // big-endian
    const payload = new Uint8Array(buf, 8);

    if (Number(offset) < this.nextOffset) return; // already seen; drop
    this.nextOffset = Number(offset) + payload.length;
    this.onOutput(payload);                       // → term.write()
  }
}
```

Rules:

- **Big-endian**, 8-byte prefix. `getBigUint64(0, false)`.
- Advance `nextOffset` by payload length on every frame; send it as `after` on every
  reconnect. Never restart at `0` except after an `offset_ahead` error.
- `binaryType = "arraybuffer"` — the default `"blob"` forces an async read and reorders
  output.
- Reconnect with jittered exponential backoff (250 ms → 8 s).
- **Never clear the terminal buffer on a cursor reconnect.** Replay continues exactly
  where the client left off, so clearing would erase correct scrollback.
- **First attach with no cursor** omits `after` and takes the default `tail`. Do not
  send `after=0` — that asks for the entire log
  ([04-api-protocol.md](04-api-protocol.md#bounded-attach)).
- **When `ready` reports `truncated: true`**, call `term.reset()` *before* writing the
  first chunk. Tailed replay starts mid-VT-stream, so terminal state (colors,
  alt-screen, scroll region) is unknown and the first bytes may be half an escape
  sequence. Reset fixes the state and costs at most one garbled line. Show a
  "scrollback truncated" marker with a link to the full `/log`.
- On `slow_consumer` (close 1013), reconnect normally — it is an expected event.
- **Reconnect never preempts.** `mode=control` on attach asks the server to give back a
  lease that is still ours during the grace window; if someone else took control while
  we were offline, `ready` comes back `control:false` and we render as an observer.
  Never auto-send `claim_control` on reconnect — that steals the terminal back from
  whoever is using it ([04-api-protocol.md](04-api-protocol.md#why-attach-must-not-preempt)).
- `wantControl` is **user intent** and survives reconnects; `hasControl` is what the
  server last told us. Never conflate them.

## Client identity and token

```ts
// generated once, persisted forever
const CLIENT_ID   = localStorage.getItem("client_id") ?? newClientId();

// `crypto` is exposed only in a secure context (HTTPS, or a localhost origin).
// Over plain http:// on a LAN IP — the --i-know-what-im-doing path — it is
// undefined and randomUUID() throws before the app renders. Degrade, don't die.
function newClientId(): string {
  return globalThis.crypto?.randomUUID?.()
      ?? `c-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
const CLIENT_NAME = localStorage.getItem("client_name") ?? defaultName();  // "Chrome on macOS"
```

`client_id` is what lets a dropped controller resume its own lease and what names the
controller in everyone else's UI. It is **not** a credential.

The token is. The daemon prints a `?token=…` URL at startup; on first load the SPA
stores the token and **strips it from the address bar** (`history.replaceState`) so it
does not sit in the URL or leak through `Referer`
([06-security.md](06-security.md#token-on-the-websocket-upgrade)).

## Geometry

There is exactly one PTY size per session and only the controller sets it. Observers
must render *that* size, not their own viewport:

| Role | Behavior |
|---|---|
| Controller | `fitAddon.fit()` to the viewport, then send `resize`, debounced 150 ms |
| Observer | **do not fit.** Set the terminal to `ready`'s `cols`/`rows` and scale/letterbox the container to fit |

`ready` carries the current `cols`/`rows`, and `resized` carries every change. An
observer that fits to its own viewport renders output that was wrapped for a different
width — a 160-column desktop watching a phone-sized PTY looks broken, and it is the
first thing anyone notices when two devices watch one session.

## `Terminal.svelte`

- One `Terminal` + `FitAddon` per session component instance.
- `term.write(payload)` accepts `Uint8Array` directly; do not decode to a string.
  Multi-byte UTF-8 sequences split across PTY chunks would corrupt otherwise.
- `term.onData(d => stream.sendInput(d))` — only when this client holds the lease.
- Resize: `fitAddon.fit()` on container resize, then send
  `{"type":"resize",...}` **only if this client is the controller**. Debounce 150 ms.
  An observer resizes its terminal to the PTY's `cols`/`rows` instead — see
  [Geometry](#geometry).
- Cap scrollback (`scrollback: 10000`). Deep history lives in `/log`, not in RAM.
- Dispose the terminal and close the WebSocket on component destroy.

## Control lease UI

There is exactly one controller ([03-pty-layer.md](03-pty-layer.md#resize)). Make it
visible, never ambiguous:

| State | UI |
|---|---|
| This client controls | normal cursor, input enabled, badge "Controlling" |
| Observing | input disabled, dimmed cursor, prominent **Take control** button |
| Control revoked | toast "Control taken by <client_name>", switch to observing, no data loss |
| Asked for control, didn't get it | attach succeeded as observer; no toast, just the Take control button |

Claims are preemptive — one tap, no negotiation, no confirmation dialog. That is the
point: grabbing a runaway agent from a phone must be instant.

## Mobile

The phone uses the **same SPA**. No separate mobile API, no native app.

- Responsive layout: session list collapses to a sheet; terminal is full-bleed.
- The terminal is small on a phone. Resize only when controlling; when the desktop
  deliberately takes control back it resizes to its own geometry and the phone
  letterboxes to it ([Geometry](#geometry)). What must *not* happen is the desktop
  reclaiming control merely by reconnecting.
- Provide a key bar for what a soft keyboard cannot send: `Esc`, `Tab`, `Ctrl`, arrows,
  `Ctrl-C`.
- Handle `visibilitychange`: on resume, the socket is likely dead — reconnect
  immediately with the tracked offset rather than waiting for a timeout.
- PWA manifest + installability. **No service-worker caching of API responses** — stale
  session state is worse than a spinner.
- **Secure context is a hard prerequisite for most of this.** Installability, service
  workers, notifications, the clipboard API and `crypto` all require HTTPS or a
  `localhost` origin. Served over `http://<lan-ip>:<port>` none of them exist. Reaching a
  phone properly means Tailscale Serve ([07](07-remote-access.md)), which terminates TLS
  — the raw-LAN escape hatch is for debugging, and the mobile feature set silently
  collapses on it.

## Connection status

Reconnection is normal, not an error. Show a subtle inline indicator (a colored dot plus
`live` / `reconnecting` / `lost`). Never a modal. Never a full-screen error that hides
the terminal contents the user is trying to read.

## Dev workflow

```bash
# terminal 1
cargo run -p teleportd -- --data-dir ./.dev-data

# terminal 2
cd web && npm run dev     # :5173, proxying /api → 127.0.0.1:7337
```

Vite's dev proxy must forward **both** `/api` HTTP and the WebSocket upgrade (`ws: true`).
The dev origin `http://localhost:5173` is allowlisted only in debug builds
([06-security.md](06-security.md#browser-origin-defense)).

## Explicitly not in the frontend

```text
no SSR / SvelteKit / Next
no state-management library
no component/UI framework beyond plain CSS
no service-worker API caching
no client-side terminal emulation beyond xterm.js
no session state that the daemon does not also have
```

The daemon is the source of truth. The UI holds `nextOffset` and ephemeral view state —
nothing else.
