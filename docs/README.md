# teleport — implementation docs

`teleport` is a **local, headless terminal/session daemon with disposable clients**.
The daemon (`teleportd`) owns every PTY and child process. Desktop UI, browser, and
phone are interchangeable clients that attach and detach freely.

> A browser closing, a laptop UI restarting, or a phone going to sleep must never
> determine whether an agent continues running.

## Reading order

| # | Doc | Read it when |
|---|---|---|
| 01 | [Architecture](01-architecture.md) | Always first. Ownership boundaries, component map, dependency direction. |
| 02 | [Stack decisions](02-stack-decisions.md) | You want to know *why* Rust/Axum/portable-pty/Svelte, and what was rejected. |
| 03 | [PTY layer](03-pty-layer.md) | You touch `pty.rs`, threads, resize, or termination. Contains the Windows/ConPTY rules. |
| 04 | [API & wire protocol](04-api-protocol.md) | You touch `api.rs`, HTTP routes, or the WebSocket framing. |
| 05 | [Persistence](05-persistence.md) | You touch `persistence.rs`, SQLite, output logs, or restart recovery. |
| 06 | [Security](06-security.md) | You touch bind addresses, headers, auth, env vars, or anything user-supplied. |
| 07 | [Remote access](07-remote-access.md) | You wire up Tailscale Serve or Cloudflare Tunnel. |
| 08 | [Packaging](08-packaging.md) | You touch the Tauri shell, install flow, or daemon lifecycle. |
| 09 | [Frontend](09-frontend.md) | You touch `web/`, Svelte components, or xterm.js. |
| 10 | [Testing](10-testing.md) | Before you claim any milestone is done. |
| 11 | [MVP plan](11-mvp-plan.md) | Sequencing, milestones, acceptance criteria, scope boundary. |
| 12 | [Identity & connectivity](12-identity-and-connectivity.md) | You touch `auth.rs`, bind addresses, or anything about *who* is calling. Defines the three stages and the `Principal` seam. |
| 13 | [Native clients](13-native-clients.md) | Post-MVP. Read before changing the protocol — it lists what the MVP must not break. |
| 14 | [Cloud backend](14-cloud-backend.md) | Post-MVP. Accounts, device directory, relay, push. Contains one decision to make early. |
| 15 | [Open questions](15-open-questions.md) | **Before starting M1.** What the docs assert but have not proven, and the decisions still open. |

Docs 01–14 describe the design. [15](15-open-questions.md) is the list of places where
that design is asserted rather than proven — read it before writing `pty.rs`.

## Naming

| Research term | This project |
|---|---|
| `agentdeck` | `teleport` (repo / product) |
| `agentd` | `teleportd` (daemon binary) |

## The five invariants

Any change that violates one of these is wrong, regardless of how convenient it is.

1. **The PTY reader never waits for a network client.** Output drains to disk at PTY
   speed. Fan-out to subscribers is non-blocking and lossy-by-disconnect.
2. **A session's lifetime is independent of any UI.** Zero attached clients is a
   normal, indefinite state.
3. **One PTY has exactly one size and exactly one input writer.** Enforced by the
   control lease.
4. **The append-only output file is the replay index.** Byte offsets are the only
   cursor; there is no per-chunk database row.
5. **The daemon never trusts the network it sits on.** Reachability is a transport
   concern (loopback → tailnet → relay); authorization is a `Principal` the daemon
   resolves for itself. Binding loopback is not authentication — every request carries a
   credential, including on `127.0.0.1`, and no handler may ever depend on *how* a
   connection arrived.

## Where this is going

The MVP is stage 1 of a three-stage product, and the docs are written so that later
stages are **additive**:

```text
stage 1  loopback          daemon + browser on one machine          MVP
stage 2  tailnet / tunnel  phone reaches your machine privately     MVP (M9)
stage 3  account + relay   download app, log in, see your machines  v2
```

Docs 12–14 describe stages 2–3 and, crucially, the small set of things the MVP must get
right so they stay additive. They add **no MVP scope**. See
[12-identity-and-connectivity.md](12-identity-and-connectivity.md).

## Scope boundary (MVP)

Sessions survive **client disconnects**. Sessions do **not** survive a `teleportd`
crash or a host reboot — metadata and logs survive, the live PTY does not. This is a
deliberate, documented limit. See [01-architecture.md](01-architecture.md#the-crash-boundary)
for the second-stage design that would change it, and why we are not paying for it yet.
