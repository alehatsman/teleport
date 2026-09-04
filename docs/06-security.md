> **This HTTP service is a remote-code-execution control plane by design.** Its entire
> purpose is spawning arbitrary child processes on the user's workstation. Treat every
> design decision here accordingly.

# 06 — Security

## Minimum security model

```text
teleportd never runs as root / Administrator
             +
loopback-only listener
             +
authenticated remote transport
             +
strict browser Origin allowlist
             +
one control lease per terminal
             +
no shell-command string concatenation
             +
logs and config readable only by owning OS user
```

All seven. None is optional, and none substitutes for another.

## Listener

Default bind is `127.0.0.1:7337`. `--listen` may change the port but **must reject any
non-loopback address** unless `--i-know-what-im-doing` is also passed, which logs a
prominent warning at startup and on every request.

Remote reachability comes from Tailscale Serve or Cloudflare Tunnel
([07-remote-access.md](07-remote-access.md)), never from widening the bind. This also
matters for identity headers: forwarded-identity headers are only trustworthy when the
backend is loopback-bound, because otherwise a direct caller can forge them.

## Privilege

Refuse to start as `uid 0` / elevated Administrator unless `--allow-root` is passed.
Nothing in this product needs privilege; running elevated turns a session-hijack bug
into a full host compromise.

## Browser origin defense

Network-level auth (Tailscale, Cloudflare Access) establishes **who can reach the
service**. Origin/Host checks address a completely different class: a malicious or
careless web page in the user's browser reaching `127.0.0.1`.

WebSocket upgrades are **not** protected by CORS preflight. Any page on the internet can
open a WebSocket to `ws://127.0.0.1:7337/...`. The `Origin` check is the only thing
stopping it.

Enforce on **all mutating HTTP** (`POST`, `DELETE`) and **all WebSocket upgrades**:

1. **`Origin` allowlist — when `Origin` is present.** Reject unknown values. Default
   allowlist:
   - `http://127.0.0.1:7337`, `http://localhost:7337`
   - the Vite dev origin, only when built in debug (`http://localhost:5173`)
   - `tauri://localhost` and `https://tauri.localhost` for the packaged shell
   - any origin explicitly listed in `config.toml` under `allowed_origins`
2. **Credential — when `Origin` is absent.** A missing `Origin` means the caller is not
   a browser (native app, CLI, script). It is **not** rejected on that basis; it must
   instead present a credential, and loopback alone suffices only in stage 1.
3. **`Host` allowlist — always.** Rejects DNS rebinding, where an attacker-controlled
   name resolves to `127.0.0.1` and carries an allowed-looking `Origin`. Accept only
   `127.0.0.1:<port>`, `localhost:<port>`, and hostnames configured for the remote
   transport (the tailnet name, the Cloudflare hostname).

```text
Origin present  → must be in the allowlist   (a browser is talking; enforce it)
Origin absent   → must present a credential  (not a browser; loopback-only in stage 1)
always          → Host must be in the allowlist
```

> **Do not reject a request merely for lacking `Origin`.** `Origin` is a header browsers
> attach and refuse to let script forge — it is a defense against a malicious *page*,
> and it is meaningless for a native client, which can send whatever it likes. Requiring
> it would block every native mobile app
> ([13-native-clients.md](13-native-clients.md#four-obligations-the-mvp-must-honor)).

**Never enable permissive CORS.** No `Access-Control-Allow-Origin: *`, ever. Keep the
allowlist explicit and small.

## Authentication

> Authorization has **one seam**: every request resolves to a `Principal` before any
> handler runs. Handlers never inspect headers, never check the bind address, never know
> how the connection arrived. This is what makes accounts additive later rather than a
> rewrite — see [12-identity-and-connectivity.md](12-identity-and-connectivity.md#the-principal).
>
> "Identity belongs to the transport" is true of **stages 1 and 2**, not forever. Write
> it as a stage, not a principle.

MVP: loopback binding + Origin/Host checks + an authenticated transport for remote
access. That is the documented baseline.

Optional and recommended once the daemon is reachable beyond loopback — a **bearer
token**:

- Generated on first run into `<data_dir>/token`, mode `0600`.
- Sent as `Authorization: Bearer <token>` on HTTP **and on the WebSocket upgrade**.
  Accept `?token=` on the upgrade as well, because the browser `WebSocket` API cannot
  set headers — but native clients can and should use the header. Support both.
- Compared in **constant time**. A naive `==` on a secret is a timing oracle.
- Enabled by `--auth-token` or `config.toml`; off by default for pure-loopback use.

A custom username/password system is explicitly out of scope for the MVP
([11-mvp-plan.md](11-mvp-plan.md#out-of-scope)). When accounts arrive they come from the
cloud backend — passkeys or OAuth, never hand-rolled passwords
([14-cloud-backend.md](14-cloud-backend.md#what-the-backend-is)).

## Process spawning

- **argv array only.** `command` + `args: []string` are passed to `CommandBuilder`
  directly. There is no string that gets split, joined, or handed to a shell.
- A shell may legitimately be the `command` when the user explicitly asks for shell
  parsing. That is a user choice expressed in `command`, not a daemon behavior.
- Validate `cwd` exists and is a directory before spawning. Do not create it.
- Validate the executable resolves. Report "not found" as a clean `404`, not a spawn
  panic.
- Reject `cols`/`rows` outside `1..=1000`.

There is no allowlist of runnable commands. The product's purpose is running arbitrary
commands; a bypassable allowlist would be security theater. The real boundary is *who
can reach the API*.

## Secrets and environment

**Do not persist the inherited environment.** Agent environments commonly contain API
keys and credentials, and reconnecting a terminal does not require storing them.

- SQLite stores `command`, `argv_json`, `cwd`, and redacted metadata. There is no `env`
  column ([05-persistence.md](05-persistence.md#schema)).
- The child inherits the daemon's environment plus explicit per-session overrides.
- Overrides are held in memory for the session's lifetime and **redacted from all API
  responses** — return key names with `"***"` values, or omit the field entirely.
- Never log environment values. Never include them in `session_events.data_json`.

## Terminal logs are sensitive

Agent output contains source code, file paths, command output, and tokens that commands
print by accident. Therefore:

- Authorization on `GET /log` is **identical** to authorization on live WebSocket
  attach. No "it's just a file" shortcut, no unauthenticated download link.
- `<data_dir>` is `0700`; `output.vt` is `0600` (Unix). On Windows, create the data
  directory with an ACL granting only the owning user.
- Never expose log paths in API responses.
- `?purge=true` on delete, and the GC policy, exist so users can actually get rid of
  this data.

## Threat table

| Threat | Mitigation |
|---|---|
| Malicious web page hits `127.0.0.1` | Origin allowlist on mutating HTTP + all WS upgrades (browser clients) |
| Non-browser client bypassing the Origin check | Credential required whenever `Origin` is absent |
| DNS rebinding | Host header allowlist |
| Internet-wide exposure | Loopback bind; Tailscale Serve (tailnet-only) as default remote path |
| Forged identity headers | Trust forwarded identity only because the listener is loopback-only |
| Token theft via timing | Constant-time comparison |
| Credential leak via metadata | No `env` column; overrides redacted; never logged |
| Log exfiltration | `/log` shares live-attach authorization; `0600` files |
| Command injection | argv array; no shell string construction anywhere |
| Privilege escalation | Refuse to run elevated |
| Disk exhaustion | Per-session log cap + GC ([05-persistence.md](05-persistence.md#size-cap)) |
| Slow-client DoS on the PTY | Bounded queues; disconnect slow consumers ([03-pty-layer.md](03-pty-layer.md#backpressure)) |
| One client fighting another's terminal size | Single control lease ([04-api-protocol.md](04-api-protocol.md#control-lease)) |

## What "reliable from phone" actually means

Remote networking cannot make a powered-off or fully sleeping workstation reachable. Be
honest in the UI:

```text
host awake + network available:
    automatic remote reachability

phone switches Wi-Fi/cellular:
    automatic reconnect

browser suspended:
    session continues

desktop UI exits:
    session continues

Tailscale Serve restarts:
    sharing resumes when --bg is configured

teleportd crashes:
    live session is lost in MVP, logs survive

host reboots:
    previous sessions do not resume;
    daemon starts fresh
```

The live-PTY limitation is an application architecture boundary. No networking product
solves it.
