# 14 — Cloud backend (post-MVP)

**The MVP has no cloud backend and must not need one.** Stages 1 and 2
([12](12-identity-and-connectivity.md#the-three-stages)) work entirely on the user's own
infrastructure. This doc describes what gets built for stage 3, and — more importantly —
the one decision that must be made before any of it is written.

## What the backend is

| Service | Job |
|---|---|
| **Identity** | accounts, login, sessions. Passkeys or OAuth; do not build passwords. |
| **Device directory** | which daemons belong to an account, online status, last seen |
| **Pairing** | issue and redeem enrollment codes ([12](12-identity-and-connectivity.md#stage-3-pairing)) |
| **Relay** | carry daemon↔client traffic through NAT on both sides |
| **Push fan-out** | hold APNs/FCM credentials, deliver attention events |
| **Update metadata** | version manifests for the desktop app and daemon |

## What the backend is not

```text
✗ not a session store          — sessions live and die on the user's machine
✗ not a PTY owner              — it never spawns a process
✗ not required for local use   — loopback and tailnet paths never touch it
✗ not a place terminal history is retained
```

If a feature needs the backend to remember terminal content, it is the wrong feature.
The daemon is the source of truth; the backend is a directory and a pipe.

---

## The decision to make first: what can the relay read?

Everything else here is ordinary web-service work. This is not, and **retrofitting it is
expensive** — it changes pairing, key storage, the login flow, and the client SDK.

| | **A — Dumb relay, end-to-end encrypted** | **B — Terminating relay** |
|---|---|---|
| Backend sees terminal bytes | No | **Yes — all of them** |
| Crypto | device keypairs; client↔daemon handshake (Noise or TLS) tunnelled through the relay | TLS to the relay, TLS onward; plaintext in between |
| Pairing must exchange | account binding **and** device public key | account binding only |
| Complexity | Meaningfully higher | Low |
| Debuggability | Relay logs are opaque | Relay can be inspected |
| If the relay is breached | Attacker gets ciphertext | Attacker gets source code, command output, and every secret an agent printed |

**Recommendation: A.**

The reason is specific to this product, not general security preference. These terminals
run coding agents. Their output routinely contains source code, file paths, and API keys
that tools print by accident — the same reasoning that already governs
[terminal logs](06-security.md#terminal-logs-are-sensitive) locally. Option B means
operating infrastructure that holds all of it in plaintext for every user, which is a
liability, a compliance surface, and a hard thing to explain on a landing page.

It is also a **trust asymmetry**: the local product's whole story is "your machine, your
data." A relay that reads everything quietly reverses that.

Choose before building the relay. Choosing A later means redoing pairing and key
distribution for every already-enrolled device.

## Relay shape

```text
daemon                                  client (phone / web / desktop)
  │                                        │
  │ outbound WSS, held open                │ outbound WSS
  ▼                                        ▼
┌──────────────────────────────────────────────┐
│  relay: authenticate both ends,              │
│         match by device_id,                  │
│         copy bytes                           │
└──────────────────────────────────────────────┘
        (option A: bytes are already encrypted
         to the peer; the relay cannot read them)
```

Properties to design for:

- **Outbound-only from the daemon.** No inbound ports, no NAT traversal, no DNS. This is
  the entire reason the relay exists.
- **Many idle long-lived connections.** The scaling constraint is open sockets and
  memory, not CPU. Sizing follows connection count, not request rate.
- **Bytes are the cost driver.** Terminal output is usually KB/s, but an agent dumping
  a build log is bursty and large. Meter it; consider a per-device rate cap.
- **The relay must not become sticky state.** A relay node restart should cause a
  reconnect, not data loss — the offset protocol already makes that a non-event
  ([04-api-protocol.md](04-api-protocol.md#offsets-are-the-replay-index)). This is a
  direct payoff of the MVP design.
- **Prefer a direct path when one exists.** Same-LAN clients should not relay.

## Infrastructure options

| Option | Fit for relay | Notes |
|---|---|---|
| **Fly.io** | Strong | Anycast, WebSocket-friendly, cheap global regions, simple deploy. Good default. |
| **Cloudflare Durable Objects** | Strong | A DO per device is a natural match for connection pairing; edge-global. Ties you to CF's model. |
| Plain VMs (Hetzner / DigitalOcean) | Good | Cheapest per byte; you own load balancing, TLS, and rollout. |
| AWS / GCP | Workable | ALB WebSocket support is fine; more operational surface than this needs at the start. |

**Recommendation:** start with a single small deployment (Fly.io or one VM) in one
region. The relay is nearly stateless per connection; multi-region is a latency
optimization to make when there are users to measure, not before.

Identity, directory, and pairing are a conventional web app — any of the above hosts it.
Use managed Postgres. Do not reuse the daemon's SQLite design here; it solves a different
problem.

### The self-host escape hatch

Keep stage 2 (Tailscale Serve, Cloudflare Tunnel) fully supported forever. It means a
user can run the entire product without touching your infrastructure. That is worth real
money in trust for a tool that runs commands on developer machines, and it costs nothing
to preserve because it already works.

## Repo placement

```text
cloud/
├── api/          identity, device directory, pairing, push fan-out
├── relay/        the byte pipe
├── infra/        IaC — keep it declarative and in-repo
└── README.md     runbook: deploy, rotate, revoke
```

## Sequencing

Nothing here starts until the MVP ships and native clients are actually being built.
When it does, the order is:

```text
1. identity + device directory   (a login and a list; no relay yet)
2. pairing                        (bind a daemon to an account over the existing
                                   stage-2 transport — provable without the relay)
3. relay                          ← the trust decision above gates this
4. push fan-out
5. direct/LAN path optimization
```

Steps 1 and 2 are testable over Tailscale with no relay at all. That is deliberate: it
lets the account model be proven before committing to the hardest piece.
