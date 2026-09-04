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

  connect(mode: "control" | "observe") {
    // With a cursor, resume exactly. Without one, take a bounded tail —
    // never after=0, which asks for the entire log.
    const cursor = this.hasCursor ? `after=${this.nextOffset}` : `tail=${DEFAULT_TAIL}`;

    const ws = new WebSocket(
      `${wsBase}/api/v1/sessions/${this.id}/stream?${cursor}&mode=${mode}`
    );
    ws.binaryType = "arraybuffer";
    // ...
  }

  private onReady(msg: ReadyFrame) {
    this.nextOffset = msg.replay_from;   // trust the server's replay start
    this.hasCursor = true;
    if (msg.truncated) this.onTruncated(); // → term.reset() before the first chunk
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

## `Terminal.svelte`

- One `Terminal` + `FitAddon` per session component instance.
- `term.write(payload)` accepts `Uint8Array` directly; do not decode to a string.
  Multi-byte UTF-8 sequences split across PTY chunks would corrupt otherwise.
- `term.onData(d => stream.sendInput(d))` — only when this client holds the lease.
- Resize: `fitAddon.fit()` on container resize, then send
  `{"type":"resize",...}` **only if this client is the controller**. Debounce 150 ms.
- Cap scrollback (`scrollback: 10000`). Deep history lives in `/log`, not in RAM.
- Dispose the terminal and close the WebSocket on component destroy.

## Control lease UI

There is exactly one controller ([03-pty-layer.md](03-pty-layer.md#resize)). Make it
visible, never ambiguous:

| State | UI |
|---|---|
| This client controls | normal cursor, input enabled, badge "Controlling" |
| Observing | input disabled, dimmed cursor, prominent **Take control** button |
| Control revoked | toast "Control taken by <client>", switch to observing, no data loss |

Claims are preemptive — one tap, no negotiation, no confirmation dialog. That is the
point: grabbing a runaway agent from a phone must be instant.

## Mobile

The phone uses the **same SPA**. No separate mobile API, no native app.

- Responsive layout: session list collapses to a sheet; terminal is full-bleed.
- The terminal is small on a phone. Resize only when controlling, and expect the
  desktop to reclaim control and resize back — that is correct behavior, not a bug.
- Provide a key bar for what a soft keyboard cannot send: `Esc`, `Tab`, `Ctrl`, arrows,
  `Ctrl-C`.
- Handle `visibilitychange`: on resume, the socket is likely dead — reconnect
  immediately with the tracked offset rather than waiting for a timeout.
- PWA manifest + installability. **No service-worker caching of API responses** — stale
  session state is worse than a spinner.

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
