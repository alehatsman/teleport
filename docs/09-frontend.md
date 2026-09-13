# 09 — Frontend

Svelte + TypeScript + Vite, with xterm.js as the terminal. Vite produces static assets
that `teleportd` serves from the same origin as the API — so there is no CORS story, no
second server, and no SSR.

**Do not write a terminal renderer.** xterm.js is the terminal layer.

## Structure

Same four layers as the fleet's other web app (codefort), per
[ts-quality/docs/UI.md](https://github.com/alehatsman/ts-quality/blob/main/docs/UI.md)
rule 19: `api/` (data), `ui/` (domain-free primitives), `shell/` (app chrome),
`features/<x>/` (one domain each). Teleport has one domain — sessions — so there is one
feature directory holding both the list and the viewer. `shell/` is empty with a README
saying what lands there and when; `ui/` holds the two primitives promoted so far.

```text
web/
├── vite.config.ts
└── src/
    ├── main.ts
    ├── App.svelte              # hash routing between list and session views
    ├── app.css                 # tokens + shared blocks (web/CLAUDE.md)
    ├── api/
    │   ├── api.ts              # typed HTTP client for /api/v1 + describeError()
    │   ├── stream.ts           # WebSocket client: framing, offsets, reconnect
    │   ├── stream.test.ts
    │   ├── identity.ts         # client id / token / display name
    │   └── types.ts            # shared types mirroring the API doc
    ├── shell/                  # app chrome — empty until two features share some
    ├── ui/
    │   ├── StatusDot.svelte    # .dot + its text twin (SessionRow, Session)
    │   └── ErrorBanner.svelte  # .banner--error + role=alert (four call sites)
    └── features/
        └── sessions/
            ├── Sessions.svelte         # orchestrator: fetch/poll, filtering, launcher trigger
            ├── SessionFilters.svelte   # status toggle + search box (bindable, no logic)
            ├── SessionLauncher.svelte  # new-session form: presets, custom command, cwd, resume
            ├── DirectoryBrowser.svelte # inline cwd picker for the launcher (GET /api/v1/browse)
            ├── SessionList.svelte      # the list container; owns swipe-reveal exclusivity
            ├── SessionRow.svelte       # one row: display fields, swipe-to-reveal gesture
            ├── Session.svelte          # one session: header, status, control lease UI
            ├── KeyBar.svelte           # touch-only key row; emits bytes, Session decides
            └── Terminal.svelte         # xterm.js, isolated
```

Imports use the `@/` alias for `src/` (`tsconfig.app.json` paths + `vite.config.ts`
resolve.alias), matching codefort — e.g. `@/api/api`, `@/features/sessions/SessionRow.svelte`.
No relative `../` imports across layers; a component's own same-directory siblings may
use `./`.

`Terminal.svelte` is the **only** file that imports xterm.js. Everything else deals in
session IDs and connection state. If a second component reaches into the xterm API, the
boundary has leaked.

Each component owns its own markup and `<style>` block; a parent composes children via
props and callbacks, never by reaching into a child's internals. `Sessions.svelte` used
to be a single 1269-line file with the session list, the row markup, the swipe gesture,
the launcher form and the directory browser all in one `<script>`/`<style>` — it's now
five components composed together, each small enough to read in one sitting:

- **`SessionLauncher.svelte`** — mounted only while `showLauncher` is true. Props:
  `cwd`/`selectedPreset`/`customCommand` (`bind:`, two-way — see below), `presets:
  Preset[]`, `recentCwds: string[]`, `homeDir: string | null` (cwd placeholder + submit
  fallback), `resumeSession: Session | null` (set right before opening via "Resume",
  prefills and locks the preset/resume-id/cwd fields), `onLaunch: (req:
  CreateSessionRequest) => Promise<void>` (does the `createSession` call and closes the
  panel on success — stays in `Sessions.svelte`, which owns `sessions`; a thrown error
  surfaces as `launchError` here without closing), `onClose: () => void`. Owns:
  `resumeSessionId`/`launching`/`launchError`/browser-open state, the Escape-to-close
  handler, first-field autofocus.
  - `cwd`, `selectedPreset` and `customCommand` are **bindable, not local** — they must
    outlive this component's own mount/unmount cycle (it exists only while the panel is
    open) so a value already typed or chosen is never lost on a close+reopen. That
    persistence is deliberate in the original single-file version (`cwd`'s prefill logic
    is explicitly "never clobber a mid-typed value... across a reopen") — losing it would
    have been a silent behavior change from splitting the file, not a simplification.
- **`DirectoryBrowser.svelte`** — mounted only while the launcher's browser panel is
  open (a fresh instance each time; the original's `openBrowser()` always re-fetched on
  open anyway, so nothing relied on its browse state surviving a close). Props:
  `initialPath: string | null`, `onSelect: (path: string) => void`, `onClose: () =>
  void`. Owns: the `GET /api/v1/browse` call, its own loading/error state.
- **`SessionList.svelte`** — owns `openRowId`, the one-row-open-at-a-time swipe
  exclusivity (a list-level concern: opening one row must close whichever other row was
  open). Props: `sessions: Session[]`, `now: number`, `homeDir: string | null`, and the
  row action callbacks (below), passed straight through to each `SessionRow`.
- **`SessionRow.svelte`** — one list item: state dot, command/args/title/cwd/controller/
  outcome/age, the resume button, the swipe-to-reveal drag gesture (now purely local —
  "is *this* row dragging", no cross-row id comparison needed once each row is its own
  component instance), and the terminate/delete action button. Props: `session: Session`,
  `now: number`, `homeDir: string | null`, `isOpen: boolean` + `onOpenChange: (open:
  boolean) => void` (the swipe-reveal state, owned by `SessionList`), `onOpen: (id:
  string) => void`, `onResume: (session: Session) => void`, `onTerminate: (id: string) =>
  void`, `onPurge: (id: string) => void`.

`describeError()` (interpreting an `ApiError` into an actionable message) moved from a
local function into `api/api.ts` and is exported — `Sessions.svelte` (list/refresh
errors) and `SessionLauncher.svelte` (launch errors) both need it now, so it's promoted
per UI.md rule 11 rather than duplicated.

`SessionLauncher`'s `.launcher__actions` (Cancel/Launch button row) and
`DirectoryBrowser`'s equivalent `.browser__actions` (Cancel/Use-this-folder) are the same
three CSS properties — duplicated rather than promoted to `app.css`. Svelte scopes styles
per component, so reusing one class across the two would need the shared block
promotion anyway; three properties used by exactly two sibling components in one feature
isn't worth that indirection.

The list-row swipe-to-delete gesture handlers (`onTouchStart/Move/End`, the drag offset)
live in `SessionRow.svelte` — they're tightly coupled to the row DOM, not reusable, so
nothing promotes them per UI.md rule 11 ("promote on a second consumer"). Only the
one-row-open-at-a-time rule sits one level up, in `SessionList.svelte`.

## `stream.ts` — the part that must be right

This client owns the offset contract from
[04-api-protocol.md](04-api-protocol.md#offsets-are-the-replay-index).

```ts
type StreamState = "connecting" | "replaying" | "live" | "reconnecting" | "closed";

class SessionStream {
  private nextOffset = 0;          // bytes consumed so far — the reconnect cursor
  private backoff = 250;           // ms, →8000 with jitter
  private wantControl = false;     // sticky user intent, not connection state

  async connect() {
    // POST /api/v1/ws-ticket first (docs/06-security.md#token-on-the-websocket-upgrade,
    // mitigation 2) -- the long-lived TOKEN authenticates *that* call; the socket
    // itself only ever sees a 30s, session-scoped ticket. A failed fetch reconnects
    // through the normal backoff path below, never falls back to sending TOKEN.
    const { ticket } = await createWsTicket(this.id);

    // With a cursor, resume exactly. Without one, take a bounded tail —
    // never after=0, which asks for the entire log.
    const cursor = this.hasCursor ? `after=${this.nextOffset}` : `tail=${DEFAULT_TAIL}`;

    // mode=control asks to *resume* a lease; it never preempts. Safe on reconnect.
    const mode = this.wantControl ? "control" : "observe";

    const ws = new WebSocket(
      `${wsBase}/api/v1/sessions/${this.id}/stream` +
        `?${cursor}&mode=${mode}&client_id=${CLIENT_ID}` +
        `&client_name=${encodeURIComponent(CLIENT_NAME)}&ticket=${ticket}`
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
- **Every frame is untrusted input, not just wire content.** A binary frame shorter
  than 8 bytes, or a text frame that isn't valid JSON, closes the socket as a
  deliberate protocol violation (close 1002) rather than throwing inside the
  `onmessage` callback. The daemon should never send either; this is defense against
  the parsing code itself, not the daemon.
- **Reconnect never preempts.** `mode=control` on attach asks the server to give back a
  lease that is still ours during the grace window; if someone else took control while
  we were offline, `ready` comes back `control:false` and we render as an observer.
  Never auto-send `claim_control` on reconnect — that steals the terminal back from
  whoever is using it ([04-api-protocol.md](04-api-protocol.md#why-attach-must-not-preempt)).
- `wantControl` is **user intent** and survives reconnects; `hasControl` is what the
  server last told us. Never conflate them.

## Client identity and token

```ts
// generated once per tab (sessionStorage): survives a reload, not a closed tab.
// localStorage would make every tab of one browser the same client, and two
// tabs on one session would both hold "control".
const CLIENT_ID   = sessionStorage.getItem("client_id") ?? newClientId();

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

It's used as the `Authorization` header on every plain HTTP call (`api.ts`). `stream.ts`
does **not** put it on the WebSocket URL: it calls `POST /api/v1/ws-ticket` first and
uses the short-lived, session-scoped ticket that comes back instead
([06-security.md](06-security.md#token-on-the-websocket-upgrade), mitigation 2) — the
long-lived token itself never appears in a `ws://…` URL.

## Geometry

There is exactly one PTY size per session and only the controller sets it. Observers
must render *that* size, not their own viewport:

| Role | Behavior |
|---|---|
| Controller | `fitAddon.fit()` to the viewport, then send `resize`, debounced 150 ms |
| Observer | **do not fit.** Set the terminal to `ready`'s `cols`/`rows` and scale/letterbox the container to fit |
| Anyone, session `exited`/`lost` | `fitAddon.fit()` to the viewport, send nothing. No PTY is left to disagree with, and a letterboxed 120-column replay on a phone is unreadable; xterm reflows the old output |

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

**To see it render on an actual phone** (not just resize your desktop
browser), use [scripts/mobile-dev](../scripts/mobile-dev/README.md) — an
isolated dev daemon, Tailscale Serve wiring, and two observation tools (Mac
Chrome via CDP, and a real iOS device via `safaridriver`). Its README also
covers several non-obvious gotchas (a naive headless-Chrome screenshot flag
that produces false bug reports, an Origin-vs-Host port-matching rule that
looks like a stuck "Connecting" state, and why real-device automation has to
go through Apple's own WebDriver rather than the more obvious-looking
`ios-webkit-debug-proxy` route).

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
