# 11 — MVP implementation plan

## Ordering principle

Build in **dependency order, riskiest-first**. The PTY layer is the technical risk;
everything else is well-understood work. Do not build UI to make the project feel like
it's moving — a beautiful terminal on top of a broken offset model is negative progress.

```text
M0 skeleton
 └─ M1 PTY primitive          ← the risk. Prove on 3 OSes before anything else.
     └─ M2 session ownership + backpressure
         └─ M3 append-only replay
             ├─ M4 HTTP/WS API
             │   └─ M5 browser terminal
             │       └─ M6 control lease
             ├─ M7 SQLite metadata + recovery
             └─ M8 presets
                 └─ M9 Tailscale Serve
                     └─ M10 Tauri shell
```

Each milestone has a **gate**. Do not start the next one until the gate passes on all
three platforms.

---

## M0 — Skeleton

**Deliver:** repo scaffolding that builds on Linux, macOS, Windows.

- `daemon/` cargo crate producing `teleportd`; deps pinned per
  [02-stack-decisions.md](02-stack-decisions.md#direct-dependencies)
- `web/` Vite + Svelte + TS project
- CLI: `--listen`, `--data-dir`, `--log-level`
- `tracing` initialized; data dir resolution + creation with owner-only permissions
- CI matrix: Linux / macOS / Windows, `cargo build` + `cargo test` + `cargo clippy -- -D warnings`

**Gate:** green CI on all three platforms. Nothing else.

---

## M1 — PTY primitive

> **Build the PTY daemon before the desktop app.**

**Deliver:** `pty.rs` — spawn, read, write, resize, exit detection, termination, behind
the `TerminalSession` trait. Dedicated reader + control threads. No HTTP yet; drive it
from integration tests.

- `native_pty_system()` / `openpty` / `CommandBuilder` / `spawn_command`
- dedicated `std::thread` per direction — **not** `spawn_blocking`
  ([03-pty-layer.md](03-pty-layer.md#thread-model))
- full termination state machine: `RUNNING → CLOSING → EXITED`, graceful signal,
  bounded waits, hard kill fallback, keep draining output throughout
- resize with clamping and 100 ms coalescing

**Gate:** the PTY integration fixture list in
[10-testing.md](10-testing.md#1-pty-integration-fixtures-daemontestspty_rs) passes on
Linux, macOS, **and Windows** — including close-under-output-load and the grandchild
process-tree case. Windows is not deferred.

---

## M2 — Session ownership and backpressure

**Deliver:** `session.rs` — `SessionManager`, `Session`, subscriber registry.

- a session survives **zero** subscribers, indefinitely
- bounded per-subscriber queue (256 chunks / 8 MiB), `try_send` only
- overflow marks the subscriber slow and disconnects it; the reader never blocks
- one mutex guards `{next_offset, subscribers}`; the reader holds it briefly

**Gate:** with a subscriber that never reads, PTY drain rate is unchanged and daemon
memory stays flat. Verified under 1 MB/s sustained output.

---

## M3 — Append-only replay

**Deliver:** `log.rs` — append writer, range reader, offset accounting.

- persist **before** advancing the offset, advance **before** fan-out
- `file_length == next_offset` holds at all times
- range reads clamp to `next_offset`
- size cap + `log_capped` handling

**Gate:** disconnect exactly between output chunks, reconnect at the recorded offset,
and verify byte-for-byte that the union of replay + live output equals the log — **no
gaps, no duplicates**. Fuzz the attach point against a concurrent writer.

---

## M4 — HTTP + WebSocket API

**Deliver:** `api.rs`, `ws.rs`, `auth.rs`. Full surface from
[04-api-protocol.md](04-api-protocol.md).

- `/health` with `api_versions` + `capabilities`; device fields behind the principal
- sessions CRUD, `/log`, `/stream`
- mixed framing: text = control JSON, binary = raw bytes with an 8-byte BE offset prefix
- attach sequence in the correct order ([04](04-api-protocol.md#attach-race))
- **bounded attach**: `tail` param, `default_tail`, `max_replay_bytes`, `truncated` in
  `ready` ([04](04-api-protocol.md#bounded-attach))
- `auth.rs` resolves a `Principal`; **handlers take the `Principal`, never headers**
  ([12](12-identity-and-connectivity.md#the-principal))
- Origin allowlist when `Origin` is present; credential when it is absent; Host
  allowlist always
- `Authorization: Bearer` accepted on the WS upgrade as well as `?token=`
- server Ping every 20 s, close on 60 s without Pong

**Gate:** protocol tests pass. A scripted client can create a session, stream, drop,
reconnect at an offset, and terminate — with no gap or duplicate. A client sending **no
`Origin`** but a valid credential is accepted. Attaching with no cursor to a 500 MB log
transfers `default_tail`, not 500 MB.

---

## M5 — Browser terminal

**Deliver:** `web/` — session list, session view, xterm.js behind `Terminal.svelte`,
`stream.ts` implementing the offset contract.

- `binaryType = "arraybuffer"`; write `Uint8Array` to xterm directly (no string decode)
- jittered reconnect; **never** clear the buffer on reconnect
- daemon serves `web/dist` with SPA fallback

**Gate:** close the tab mid-agent-run, reopen, and the terminal shows a correct
continuous transcript. Same behavior on a phone browser over the LAN.

---

## M6 — Control lease

**Deliver:** single-controller enforcement end to end.

- `mode=control` on attach, `claim_control` / `release_control`
- preemptive claims; `control_revoked` to the loser
- observers' input and resize rejected with `not_controller`, never buffered
- UI: Controlling / Observing / Take control

**Gate:** a phone and a desktop attached simultaneously never fight over PTY size, and
input never reaches the PTY from a non-controller.

---

## M7 — SQLite metadata and recovery

**Deliver:** `persistence.rs` — schema, migrations via `user_version`, single writer
actor, startup recovery.

- WAL, one writer thread, mpsc + oneshot
- **no terminal bytes in rows**; **no `env` column**
- startup: stale `running`/`closing` → `lost` / `daemon_restart`; reconcile
  `output_bytes` from actual file length
- GC: directory first, row second

**Gate:** `SIGKILL` the daemon mid-session; on restart the session reads `lost`, the log
is complete and readable, and `output_bytes` matches the file.

> Ordering note: M7 lands after M4–M6 deliberately. Sessions work in memory first;
> persistence is added once the in-memory model is proven correct. If it's easier to
> write the row at creation from M1 onward, that's fine — but don't let schema design
> block the PTY work.

---

## M8 — Agent presets

**Deliver:** `presets.rs` + `presets.toml` + `GET /api/v1/presets` + a launcher in the UI.

> **Treat agents as presets.** A preset supplies executable, argv defaults, and
> presentation metadata. No scheduler, agent protocol, MCP layer, or provider SDK.

- ship presets for `claude`, `codex`, and a login shell
- explicit request fields override preset defaults
- `kind = "agent"` is metadata; the execution path is identical to a shell session
- **recent working directories** in the launcher, derived from the existing `sessions`
  table — no new storage. Typing `/home/me/src/project` on a phone keyboard is the
  difference between "I can launch an agent from my phone" and "I can't."

**Gate:** launching Claude Code and Codex from the UI works, disconnect-survives, and
replays correctly. Zero agent-specific code below `session.rs`.

---

## M9 — Tailscale Serve

> **Add Tailscale Serve before inventing application authentication.**

**Deliver:** documented setup, config support, verification.

```bash
teleportd --listen 127.0.0.1:7337
tailscale serve --bg http://127.0.0.1:7337
```

- tailnet hostname in `allowed_hosts`, its origin in `allowed_origins`
- optional bearer token implemented and tested, default off
- verify `--bg` persistence survives a **full host reboot** on each OS

**Gate:** a phone off the local network, on cellular, reaches a running agent over the
tailnet, takes control, and types into it.

---

## M10 — Tauri shell

> **Add Tauri after the daemon/web product works.**

**Deliver:** `desktop/src-tauri/` — thin.

- health check → attach or start `teleportd` **detached**
- daemon bundled via `externalBin`; closing the window does **not** stop the daemon
- tray, notifications, autostart registration (systemd user unit / LaunchAgent / Task
  Scheduler logon trigger)
- signing + notarization pipeline set up **here**, not at release

**Gate:** quit the desktop app with agents running; they keep running; reopening
reattaches. Browser-only mode remains fully functional.

---

## Small additions this plan makes beyond the original research

Each is cheap, and each is expensive to retrofit. None changes a milestone gate except
where noted.

| Addition | Milestone | Cost | Why now |
|---|---|---|---|
| Bounded attach (`tail`, `max_replay_bytes`) | M4 | ~half a day | Fixes a real bug — `after=0` on a large log replays everything. Mandatory for mobile. |
| `Principal` seam in `auth.rs` | M4 | ~an hour | Accounts become additive instead of an auth rewrite. |
| Origin-optional for credentialed clients | M4 | ~an hour | Requiring `Origin` blocks every native client, permanently. |
| `/health` `api_versions` + `capabilities` | M4 | ~an hour | App-store builds lag daemons; version skew is unavoidable. |
| `device.json` (`device_id`, `device_name`) | M0 | ~an hour | Multi-device clients otherwise reshape every payload. |
| `bell` / `idle` session events | M2 | ~an hour | Push notifications later become delivery, not hot-path surgery. |
| Recent-cwd list in the launcher | M8 | ~half a day | Phone launching is unusable without it. |

Everything else in docs 12–14 is **documentation only** and adds no MVP work.

## Definition of done

```text
MVP
├── macOS / Windows / Linux
├── arbitrary terminal command
├── agent presets
├── multiple sessions
├── daemon-owned PTYs
├── desktop disconnect survival
├── phone disconnect survival
├── append-only replay
├── one controller / multiple observers
├── SQLite metadata
├── responsive xterm.js UI
├── Tailscale Serve
└── thin optional Tauri package
```

Plus: the full failure-injection checklist in
[10-testing.md](10-testing.md#failure-injection-checklist) passes on all three
platforms, and the UI tells the truth about the crash boundary.

## Out of scope

The first release stops here. Each of these is a real feature; none of them is
MVP.

```text
daemon-crash-surviving live PTYs
OS-reboot session resurrection
native iOS / Android clients
provider-specific agent APIs
MCP transport
gRPC / protobuf
distributed session servers
multi-host clustering
terminal-state snapshots
shared multi-writer terminals
custom VPN or TLS stack
custom username/password system
```

If one of these is proposed mid-build, it goes on the list for v2 — the session-broker
design in [01-architecture.md](01-architecture.md#if-daemon-crash-survival-ever-becomes-a-requirement)
is the sketch for the first of them.

**Most likely to be pulled in first: terminal-state snapshots.** Native mobile forces it,
because tailed replay drops a client mid-VT-stream with unknown terminal state
([04-api-protocol.md](04-api-protocol.md#the-vt-state-caveat--read-this-before-implementing)).
The MVP mitigation is a client-side `term.reset()`. Keep replay a server-side decision so
a snapshot can later replace a byte range without a protocol change.

## v2 sequencing

Nothing here starts until the MVP ships. Order matters — each step is provable before the
next one is built.

```text
1. terminal-state snapshots       makes tailed attach clean          → doc 04
2. native app shells              WebView terminal, reuse everything → doc 13
3. identity + device directory    login and a machine list, no relay → doc 14
4. pairing                        provable over Tailscale alone      → doc 12
5. relay                          ← gated on the trust decision      → doc 14
6. push fan-out                   consumes the bell/idle events      → doc 13
7. direct/LAN path                latency + cost optimization
```

Step 5 is gated on one decision that must be made **before** any relay code is written:
whether the relay can read terminal bytes. Retrofitting end-to-end encryption means
redoing pairing and key distribution for every enrolled device
([14-cloud-backend.md](14-cloud-backend.md#the-decision-to-make-first-what-can-the-relay-read)).

## Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| ConPTY shutdown deadlock on Win < 24H2 | Medium | High | Bounded waits on the control thread; nightly CI on an older Windows image; M1 gate covers close-under-load |
| Offset gap/duplicate on reconnect | Medium | High | Single mutex over `{next_offset, subscribers}`; M3 fuzz gate |
| `spawn_blocking` misuse starving Tokio | Medium | High | Dedicated threads only; grep the tree for `spawn_blocking` in review |
| Slow phone backpressuring the PTY | High if unguarded | High | Bounded queues + disconnect; M2 gate |
| Log disk exhaustion | Medium | Medium | Per-session cap + GC |
| macOS notarization/signing delays release | High | Medium | Set up at M10, not at release |
| Tailscale `--bg` not persisting on some OS | Low | Medium | Explicit reboot verification in the M9 gate |
| Scope creep into agent-specific integrations | High | Medium | Presets-only rule; agents are metadata, not a code path |
