# 12 — Identity and connectivity (staged)

The MVP ships loopback-only with no accounts. The product ends with "download the app,
log in, see your machines." Those are the same system at different stages — **not a
rewrite**, provided one seam exists from day one.

> **The seam:** every request resolves to a `Principal` before any handler runs.
> Handlers never inspect headers, never check the bind address, never know how the
> connection arrived.

Get that right in M4 and stages 2 and 3 are additive. Get it wrong and auth is a rewrite.

## The three stages

| | Stage 1 — Local | Stage 2 — Private network | Stage 3 — Account |
|---|---|---|---|
| **Ships in** | MVP (M0–M8) | MVP (M9) | v2 |
| **Reachability** | `127.0.0.1` only | Tailscale Serve / Cloudflare Tunnel | Outbound relay |
| **Who can connect** | any OS user on the host who holds the token | tailnet members / Access-approved | account members |
| **Credential** | bearer token, `0600` in the data dir | same token + transport identity | account session + device credential |
| **Principal** | `LocalUser` | `LocalUser` or `DeviceToken` | `Account { user, device }` |
| **Cloud needed** | no | no (user's own Tailscale/CF) | yes ([14-cloud-backend.md](14-cloud-backend.md)) |
| **Native apps work** | on-host only | yes, over the tailnet | yes, anywhere |

**All three stages remain supported forever.** A user who never creates an account and
runs loopback-only must keep working — that is a feature (privacy, air-gapped machines,
enterprise policy), not a legacy path.

## The Principal

```rust
enum Principal {
    /// Presented the local token from <data_dir>/token, which only the owning
    /// OS user can read. That file permission — not the loopback bind — is the
    /// user boundary. See 06-security.md#loopback-is-not-a-user-boundary.
    LocalUser,

    /// Presented a valid bearer token from <data_dir>/token.
    /// Used by native apps in stage 2, CLI tools, and scripts.
    DeviceToken { token_id: String },

    /// Stage 3. Established by the cloud backend and carried through the relay.
    Account { user_id: String, device_id: String },
}
```

`auth.rs` exposes one function:

```rust
fn resolve(req: &Request, cfg: &Config) -> Result<Principal, AuthError>;
```

Every handler and the WebSocket upgrade take a `Principal` as an argument. In MVP, all
three variants authorize identically — there is one user. The value is that the
*shape* is right: when stage 3 adds per-account authorization, it changes `resolve` and
one policy function, not forty handlers.

**Do not** write `if is_loopback(addr)` inside a handler. That is the coupling this
whole doc exists to prevent.

## Client classes, and why Origin is not universal

`Origin` is a header **browsers** attach and refuse to let script forge. It is a defense
against a malicious page in the user's browser reaching the daemon. It is meaningless
for a native client, which can send any header it likes.

| Client class | Sends `Origin` | Can set `Authorization` on WS | Rule |
|---|---|---|---|
| Browser (web, Tauri WebView, mobile WebView) | yes, unforgeable | **no** (browser `WebSocket` API forbids headers) | Origin + Host allowlist mandatory; token via `?token=` if enabled |
| Native app (iOS `URLSessionWebSocketTask`, Android OkHttp) | no | **yes** | credential mandatory; Origin absent is normal |
| CLI / script | no | yes | credential mandatory |

The correct rule, replacing "missing Origin is rejected":

```text
if Origin present  → must be in the allowlist (a browser is talking; enforce it)
if Origin absent   → must present a credential (not a browser)
always             → Host must be in the allowlist
```

> An earlier draft of this rule said loopback alone was sufficient in stage 1. It is
> not — a loopback socket is open to every OS user on the machine, so "arrived on
> loopback" proves nothing about *who* is calling
> ([06-security.md](06-security.md#loopback-is-not-a-user-boundary)). Stage 1 requires
> the token like every other stage; what changes across stages is only how a client
> obtains one.

A missing `Origin` is not suspicious. A missing `Origin` **and** a missing credential
is. See [06-security.md](06-security.md#browser-origin-defense).

## Device identity

The daemon has a stable identity from first run, even in stage 1 where nothing consumes
it yet:

```text
<data_dir>/device.json
{
  "device_id":   "01K4N4ZP6C5GJ17G6X47K0VJX3",   // ULID, generated once, never changes
  "device_name": "aleh-macbook",                  // hostname by default, user-editable
  "platform":    "macos-aarch64"
}
```

Surfaced on `/health` to an authenticated caller
([04-api-protocol.md](04-api-protocol.md#get-apiv1health)).

**Why this is in the MVP:** the phone eventually shows a list of *machines*, each with
its sessions. If `device_id` does not exist until v2, then every session payload, every
notification, and every client-side cache key changes shape at that point. Adding one
generated ULID now costs nothing and prevents that.

Sessions do **not** get a `device_id` column — a daemon's sessions are all its own. The
client joins them: it knows which daemon it asked.

## Stage 3: pairing

The daemon must be bound to an account without ever typing a password into a terminal.

```text
Desktop-app path (silent — the common case)
───────────────────────────────────────────
user logs into the desktop app
    ↓
app already holds an account session
    ↓
app calls POST /api/v1/pair with a short-lived enrollment token from the backend
    ↓
daemon exchanges it for a device credential, stores 0600
    ↓
daemon appears in the account's device list


Headless path (SSH'd into a box, no GUI)
────────────────────────────────────────
$ teleportd pair
    ↓
daemon prints an 8-character code + a URL, polls the backend
    ↓
user opens the URL on any logged-in device, enters the code
    ↓
backend binds device → account, releases the credential
    ↓
daemon stores it 0600 and reports success
```

The code is short-lived (10 min), single-use, and rate-limited on the backend. This is
the standard TV/console pairing flow; do not invent a new one.

Unpairing is initiated from either side and must revoke the credential server-side —
a stolen laptop's daemon must stop being reachable.

## Connectivity: inbound vs outbound

Stages 1 and 2 are **inbound**: something connects to the daemon's listener. Stage 3 is
**outbound**: the daemon dials the relay and holds a persistent connection, so it works
behind NAT with no port configuration, no firewall rule, and no DNS.

The daemon should not care. Axum serves over any `AsyncRead + AsyncWrite` stream, so the
relay transport is additive:

```text
        ┌─ loopback TcpListener        (stage 1, 2)
        │
Router ─┼─ tailnet / tunnel            (stage 2, still inbound)
        │
        └─ relay-multiplexed streams   (stage 3, outbound)
```

**Do not build this abstraction in the MVP.** Just do not write anything that makes it
impossible: no handler reads the peer address, no handler assumes a `TcpStream`, and the
listener setup lives in one function in `main.rs`. That is the whole obligation.

## Migration guarantees

When stage 3 lands, these must all still hold:

- a stage-1 user who never logs in sees no behavior change
- a stage-2 Tailscale user is not forced onto the relay
- a bearer token issued in stage 2 keeps working
- the same `/api/v1` contract serves all three; capability negotiation via `/health`
  handles the differences
