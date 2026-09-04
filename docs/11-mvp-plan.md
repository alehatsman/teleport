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
- first-run generation of `device.json`, and `token` (256-bit, `0600`)
- bind with ephemeral-port fallback; write `<data_dir>/port` (`0600`), remove on clean
  shutdown ([08](08-packaging.md#port-discovery--do-not-hardcode-7337))
- print the `http://127.0.0.1:<port>/?token=…` URL at startup
- CI matrix: Linux / macOS / Windows, `cargo build` + `cargo test` + `cargo clippy -- -D warnings`

**Gate:** green CI on all three platforms. Nothing else.

---

## M1 — PTY primitive

> **Build the PTY daemon before the desktop app.**

> **Spike run 2026-09-04.** S1–S4 closed on Linux; S3 also closed on Windows. Decided
> the thread model below (4 threads, not 2–3 as originally estimated here — see
> [03-pty-layer.md#thread-model](03-pty-layer.md#thread-model)). Found a new, harder
> blocker along the way: **[W1](15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows)**
> — a ConPTY child that exits **on its own** (not killed) is never observed as exited
> by `wait()`/`try_wait()`, and the pty master never sees EOF either. Confirmed general
> to ConPTY (any process, not just a particular shell), and the process is genuinely
> still OS-resident while stuck — not a lost wakeup. Root cause needs WinDbg/ETW-level
> tracing this spike didn't have budget for; time-boxed rather than pursued further.
> **Decision: proceed with the deliverable below for Unix now. The Windows leg of exit
> detection for a self-exiting child is a known, open gap — see the amended Gate.**

> **Delivered for Unix, 2026-09-04** — `daemon/src/pty.rs` +
> `daemon/tests/pty_primitive.rs` (10/10 fixtures green on Linux, `cargo clippy`
> clean, cross-compile-checked for `x86_64-pc-windows-gnu`). Not run on real
> Windows or macOS yet. `pty.rs` does not own `output.vt`/offsets/subscriber
> fanout -- the reader thread calls a caller-supplied, must-not-block closure
> per chunk instead; that's where `session.rs` (M2) plugs in.

**Deliver:** `pty.rs` — spawn, read, write, resize, exit detection, termination, behind
the `TerminalSession` trait. Dedicated reader + control threads. No HTTP yet; drive it
from integration tests.

- `native_pty_system()` / `openpty` / `CommandBuilder` / `spawn_command`
- dedicated `std::thread` per direction — **not** `spawn_blocking`
  ([03-pty-layer.md](03-pty-layer.md#thread-model)). **Four** threads per session —
  `read`, `write`, `control` (`resize`/`terminate`), `reaper` (`child.wait()`) — each
  independently blocking and confirmed as such by the spike
  ([S1](15-open-questions.md#s1--who-reaps-the-child),
  [S3](15-open-questions.md#s3--a-blocking-write-wedges-terminate))
- full termination state machine: `RUNNING → CLOSING → EXITED`, graceful signal,
  bounded waits, hard kill fallback, keep draining output throughout
- resize with clamping. `pty.rs` clamps to `1..=1000` as a correctness backstop;
  the 100 ms coalescing this line originally asked for turned out to belong one
  layer up (`session.rs`, M2) -- coalescing is about merging *N observers'*
  competing resize requests into the one effective size the control lease
  implies, which is a session-level concept, not something the raw primitive
  should reach upward to decide

**Gate:** the PTY integration fixture list in
[10-testing.md](10-testing.md#1-pty-integration-fixtures-daemontestspty_rs) passes on
Linux, macOS, **and Windows** — including close-under-output-load and the grandchild
process-tree case — **except** the two fixtures that depend on observing a *graceful*
exit code on Windows (`child exits normally`, `child exits nonzero`), which are blocked
on [W1](15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows)
and tracked there, not silently skipped. Every other fixture, including
user-initiated `terminate` (which hard-kills and is unaffected by W1) and the
grandchild-tree-close case, is not deferred and must pass on Windows.

---

## M2 — Session ownership and backpressure

**Deliver:** `session.rs` — `SessionManager`, `Session`, subscriber registry.

- a session survives **zero** subscribers, indefinitely
- bounded per-subscriber queue (256 chunks / 8 MiB), `try_send` only
- overflow marks the subscriber slow and disconnects it; the reader never blocks
- one mutex guards `{next_offset, subscribers}`; the reader holds it briefly

**Gate:** with a subscriber that never reads, PTY drain rate is unchanged and daemon
memory stays flat. Verified under 1 MB/s sustained output.

**Before M4:** resolve [D1](15-open-questions.md#d1--replay-must-not-share-the-live-subscriber-budget).
As specified, a subscriber accumulates live output for the whole duration of its replay
and can overflow this same 8 MiB bound before it ever goes live — which makes a busy
session permanently unattachable. The attach ordering is correct; the budget is not.

---

## M3 — Append-only replay

**Deliver:** `log.rs` — append writer, range reader, offset accounting.

- persist **before** advancing the offset, advance **before** fan-out
- `file_length == min(next_offset, log_capped_at)` holds at all times — the capped case
  is the whole reason `log_capped_at` exists ([05](05-persistence.md#size-cap))
- range reads clamp to `min(next_offset, log_capped_at)`
- offsets **never rewind**, including across a restart of a capped session:
  `output_bytes = max(len(output.vt), stored output_bytes)`

**Gate:** disconnect exactly between output chunks, reconnect at the recorded offset,
and verify byte-for-byte that the union of replay + live output equals the log — **no
gaps, no duplicates**. Fuzz the attach point against a concurrent writer. Then cap a log
mid-stream, restart the daemon, and confirm `next_offset` did not move backwards.

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
- **token required by default**, including on loopback; constant-time compare
  ([06](06-security.md#loopback-is-not-a-user-boundary))
- `client_id` / `client_name` on attach; `ready` carries `cols`, `rows`,
  `log_capped_at`, `controller`
- **`mode=control` resumes, `claim_control` preempts**; `control_grace_ms` lease hold
  across a controller's disconnect ([04](04-api-protocol.md#why-attach-must-not-preempt))
- `max_sessions` enforced before spawn (`429`); `422` for an unresolvable executable
- server Ping every 20 s, close on 60 s without Pong

**Gate:** protocol tests pass. A scripted client can create a session, stream, drop,
reconnect at an offset, and terminate — with no gap or duplicate. A client sending **no
`Origin`** but a valid credential is accepted. Attaching with no cursor to a 500 MB log
transfers `default_tail`, not 500 MB. A second client reconnecting with `mode=control`
does **not** take the lease from a client that claimed it in the meantime.

---

## M5 — Browser terminal

**Deliver:** `web/` — session list, session view, xterm.js behind `Terminal.svelte`,
`stream.ts` implementing the offset contract.

- `binaryType = "arraybuffer"`; write `Uint8Array` to xterm directly (no string decode)
- jittered reconnect; **never** clear the buffer on reconnect
- daemon serves `web/dist` with SPA fallback

**Gate:** close the tab mid-agent-run, reopen, and the terminal shows a correct
continuous transcript.

> **Not** "the same on a phone over the LAN" — the daemon binds loopback and refuses
> anything else without `--i-know-what-im-doing` ([06](06-security.md#listener)), and
> Tailscale does not arrive until M9. Test a second *local* browser here; the phone is
> the M9 gate. If you want the phone earlier, use the escape-hatch flag deliberately and
> never let it become the development default.

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

**Also record** ([N1](15-open-questions.md#n1--keystroke-latency)): the RTT on that link,
and whether `tailscale status` reports a direct connection or a DERP relay for the phone.
A relayed path is common behind CGNAT and costs one full RTT per keystroke echo. That
number decides whether predictive local echo is a v2 item or something the product cannot
ship without — write it into 15.

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
| **Token on by default** + startup URL | M0/M4 | ~2 hours | A loopback socket is open to every OS user on the host. This is the one item that is a live security hole, not a seam. |
| `log_capped_at` column + clamped reads | M3 | ~2 hours | Without it the cap breaks `file_length == next_offset` and a restart rewinds offsets clients already hold. |
| `client_id` / `client_name` on attach | M4 | ~an hour | Required to name a controller and to let a dropped one resume. Unbuildable UI without it. |
| Attach-never-preempts + lease grace | M4 | ~2 hours | Otherwise a reconnecting desktop silently steals control from a phone, and ping-pongs on a flaky link. |
| `cols`/`rows` in `ready`, observers letterbox | M4/M5 | ~an hour | One PTY geometry; an observer fitting to its own viewport renders mis-wrapped output. |
| Port file + ephemeral fallback | M0 | ~an hour | 7337 is not a guarantee; without it a second OS user's shell probes the wrong daemon. |
| `max_sessions` cap | M4 | ~30 min | Nothing else stops a loop from creating ten thousand PTYs. |
| Updater refuses to restart under load | M10 | ~an hour | Shipping an update is a daemon restart — the crash boundary on a schedule we control. |
| `Principal` seam in `auth.rs` | M4 | ~an hour | Accounts become additive instead of an auth rewrite. |
| Origin-optional for credentialed clients | M4 | ~an hour | Requiring `Origin` blocks every native client, permanently. |
| `/health` `api_versions` + `capabilities` | M4 | ~an hour | App-store builds lag daemons; version skew is unavoidable. |
| `device.json` (`device_id`, `device_name`) | M0 | ~an hour | Multi-device clients otherwise reshape every payload. |
| `bell` / `idle` session events | M2 | ~an hour | Push notifications later become delivery, not hot-path surgery. |
| Recent-cwd list in the launcher | M8 | ~half a day | Phone launching is unusable without it. |

Everything else in docs 12–14 is **documentation only** and adds no MVP work.

Total: roughly **two days**, and the first row is not optional — it closes a hole that
exists the moment the daemon starts on any machine with more than one OS account.

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
| Offsets rewind after a capped log restarts | Medium | High | `log_capped_at`; `max()` on recovery; M3 gate |
| Local privilege escalation via the loopback port | High if unguarded | Critical | Token on by default, `0600`; M4 gate |
| Control ping-pong between reconnecting clients | High if unguarded | Medium | Attach never preempts; lease grace; M4 gate |
| `spawn_blocking` misuse starving Tokio | Medium | High | Dedicated threads only; grep the tree for `spawn_blocking` in review |
| Slow phone backpressuring the PTY | High if unguarded | High | Bounded queues + disconnect; M2 gate |
| Log disk exhaustion | Medium | Medium | Per-session cap + GC |
| macOS notarization/signing delays release | High | Medium | Set up at M10, not at release |
| Tailscale `--bg` not persisting on some OS | Low | Medium | Explicit reboot verification in the M9 gate |
| Scope creep into agent-specific integrations | High | Medium | Presets-only rule; agents are metadata, not a code path |
