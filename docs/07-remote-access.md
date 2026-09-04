# 07 — Remote access

`teleportd` listens on loopback. Reachability is a **transport** concern layered on top,
never an application listener change. Two supported deployment modes.

> **This is stage 2 of three.** Stage 1 is loopback-only; stage 3 is an account plus an
> outbound relay, which is what makes "download the app, log in" work without the user
> configuring anything ([12-identity-and-connectivity.md](12-identity-and-connectivity.md#the-three-stages)).
> Everything on this page stays supported forever — it is the self-host path, and it is
> the reason a user can run the whole product without touching our infrastructure.

```text
Private/personal                 Clientless/browser-only
================                 =======================
Phone                            Phone
  ↓ Tailscale                      ↓ Internet HTTPS
Serve                            Cloudflare Access
  ↓                                ↓
127.0.0.1:7337                   Cloudflare Tunnel
                                   ↓
                                 127.0.0.1:7337
```

## Options compared

| Option | Exposure | Phone requirement | Auth model | WS suitability | Complexity | Verdict |
|---|---|---|---|---|---:|---|
| **Tailscale Serve** | Tailnet only | Tailscale client / tailnet membership | Tailnet identity + ACLs; optional identity headers | Suitable reverse proxy | Low | **Default** |
| Tailscale Funnel | Public internet | Normal browser | App itself must be treated as internet-facing | HTTPS proxy | Medium | **Not by default** |
| **Cloudflare Tunnel + Access** | Public hostname gated by Access | Normal browser | Cloudflare Access login; origin can validate the Access JWT | WebSockets explicitly supported | Medium | **Clientless alternative** |
| SSH reverse tunnel | Depends on remote listener/proxy | Browser only after adding an HTTP/TLS proxy | SSH authenticates the tunnel; the browser app still needs its own auth | Technically possible | High | Expert fallback |

## Default: Tailscale Serve

Serve shares a local service securely **inside a tailnet**. It provisions and terminates
HTTPS automatically and proxies a loopback backend. With `--bg`, the configuration
resumes across Tailscale restarts and device reboots.

```bash
teleportd --listen 127.0.0.1:7337

tailscale serve --bg http://127.0.0.1:7337
```

That is the whole setup. The phone opens `https://<machine>.<tailnet>.ts.net/` and gets
the same SPA, same API, same WebSocket protocol as the desktop.

Notes:

- `http://127.0.0.1` is the documented HTTP proxy target — this is exactly why the
  daemon binds loopback.
- Serve can forward authenticated tailnet identity to the backend. Tailscale explicitly
  recommends binding such a backend to localhost only, because otherwise a direct caller
  could forge those headers. Our bind policy already satisfies this.
- Add the tailnet hostname to the `Host` allowlist and its `https://` origin to
  `allowed_origins` ([06-security.md](06-security.md#browser-origin-defense)).
- Restrict which tailnet devices/users may reach the machine via ACLs. Tailnet
  membership alone is a coarse boundary.

**Verify before shipping:** `tailscale serve status` after a full host reboot, to
confirm `--bg` persistence actually holds on each target OS.

## Why not Funnel

Funnel exposes a local service to the **public internet**. Its relay hides the host's
public IP and encrypts the path, but internet users can reach the endpoint.

A service whose purpose is spawning arbitrary child processes must not rely on an
obscure URL or on network exposure as authentication. If Funnel is ever used, the bearer
token in [06-security.md](06-security.md#authentication) becomes mandatory, not
optional. It is not a default.

## Clientless: Cloudflare Tunnel + Access

Use when installing Tailscale on every phone is undesirable — a borrowed device, a
locked-down work phone.

```bash
cloudflared tunnel create teleport
cloudflared tunnel route dns teleport teleport.example.com
cloudflared tunnel run --url http://127.0.0.1:7337 teleport
```

**The critical step people skip:** once a published route exists, *anyone on the
internet can reach it* unless an Access application is created for that hostname. Adding
the tunnel without Access is a direct RCE exposure.

1. Create the tunnel and route the hostname.
2. **Create a Cloudflare Access application covering that hostname**, with an explicit
   allow policy (a specific email, or an IdP group).
3. Validate the Access JWT at the origin. Access sets an authorization token on requests;
   the daemon should verify it against the team's public keys rather than assuming the
   tunnel is the only path in.
4. Add the public hostname to the `Host` allowlist and its origin to `allowed_origins`.

Cloudflare proxies WebSockets without protocol changes, but documents that network
software deployments can terminate long-lived connections — which is why the heartbeat
and offset-based reconnect in [04-api-protocol.md](04-api-protocol.md#keepalive-and-reconnection)
are mandatory, not nice-to-have.

## Expert fallback: SSH reverse tunnel

```bash
ssh -R 7337:127.0.0.1:7337 user@jump-host
```

`-R` allocates a listener on the remote host and forwards connections back over the SSH
channel. That solves **transport only**. You still need a reachable SSH server, tunnel
supervision (autossh or a systemd unit), HTTPS termination, and browser-user
authentication — all of which the other two options give you for free. Document it; do
not build tooling for it.

## Daemon configuration surface

```toml
# <data_dir>/config.toml
listen          = "127.0.0.1:7337"
allowed_origins = ["https://desktop.tail1234.ts.net"]
allowed_hosts   = ["desktop.tail1234.ts.net"]
auth_token      = false        # true enables bearer-token auth
retain_days     = 14
default_tail    = 1048576      # 1 MiB — replay when a client has no cursor
max_replay_bytes = 8388608     # 8 MiB — hard cap on any replay
log_warn_bytes  = 268435456    # 256 MiB
log_max_bytes   = 1073741824   # 1 GiB
```

CLI flags override the file. The file is the durable configuration the Tauri shell edits.

## Sequencing

Add Tailscale Serve **before** inventing application authentication
([11-mvp-plan.md](11-mvp-plan.md)). Bind loopback, expose privately with Serve, restrict
tailnet access, optionally validate Tailscale identity. Building a bespoke auth system
first would be effort spent on the problem the transport already solves.
