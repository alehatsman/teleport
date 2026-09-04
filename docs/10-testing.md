# 10 — Testing

> **Test failure injection, not just happy paths.**

The happy path for this product is trivial and will work by accident. Every bug that
matters lives in disconnects, slow consumers, resize races, and shutdown ordering.

## Platform matrix

Windows is a **first-class test platform from day one**, not a port at the end. ConPTY's
synchronous pipe behavior and shutdown semantics genuinely differ from Unix PTYs, and
`ClosePseudoConsole` changed behavior in Windows 11 24H2 (build 26100).

| Platform | CI | Notes |
|---|---|---|
| Linux x86_64 | every commit | primary dev target |
| macOS (arm64) | every commit | |
| Windows 11 ≥ 24H2 | every commit | `ClosePseudoConsole` returns immediately |
| Windows 10 / 11 < 24H2 | nightly, or manual before release | `ClosePseudoConsole` may block — the harder case |

A green Linux build means nothing about ConPTY. Do not merge PTY-layer changes without
Windows results.

## Layers

### 1. PTY integration fixtures (`daemon/tests/pty_*.rs`)

Prove the primitive before building on it. Use a small deterministic child rather than
a real shell where possible — `cargo run --bin echo-fixture` style, or `printf`/`cmd /c`
equivalents chosen per platform.

- spawn a shell, write `echo hello\n`, read `hello` back
- write large input (1 MiB) without loss or reordering
- read a large burst (100 MiB from `yes`) without dropping bytes or stalling
- resize and confirm the child observes it (`stty size` on Unix, `mode con` on Windows)
- child exits normally → exit code `0` recorded. The code comes from the child wait,
  **not** from EOF on the master — the two are separate signals and can arrive in
  either order ([S2](15-open-questions.md#s2--eof-is-not-exit)). **Windows: blocked
  on [W1](15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows)**
  — a gracefully-exiting ConPTY child is not currently observed as exited at all;
  this fixture is expected to fail on Windows until W1 resolves, not silently skipped
- child exits nonzero → exit code recorded accurately. Same Windows caveat as above
- terminate a running child → state machine reaches `exited` within the bounded policy
- terminate a child that is producing output at full rate → no deadlock, no lost tail
- close a session whose shell has spawned grandchildren → the tree is gone (Windows:
  attached character-mode clients terminate with the pseudoconsole)

### 2. Session/offset unit tests

- `len(output.vt) == min(next_offset, log_capped_at)` after every operation — the
  uncapped form `output_bytes == len(output.vt)` is only a special case
- replay of `[a, b)` returns exactly the bytes the live stream delivered for that range
- attach with `after=N` produces **no gap and no duplicate** — this is the one to
  fuzz hardest
- attach at a boundary *exactly between* two output chunks
- attach while the reader is mid-write (hammer with a concurrent writer)
- `after > next_offset` → `offset_ahead` error, no panic
- attach with `after` far behind, against a session sustaining 1 MB/s → attach completes,
  no `1013`, no gap, no duplicate — the [D1](04-api-protocol.md#catch-up--register-late-not-early)
  regression test; it fails on any build that registers the subscriber before writing the
  replay, and passes trivially against an idle session, so it must be run against a busy one
- the same with `tail` unset, exercising `default_tail`
- a client that cannot keep up gets a *reported* hole, not an unbounded reconnect loop:
  the gap stops shrinking, replay clamps to the last `live_gap_bytes`, and the offset
  discontinuity is visible to the client
- attach with no cursor on a 500 MB log replays `default_tail`, not 500 MB
- `after=0` on a huge log is clamped to `max_replay_bytes` and `ready` reports
  `truncated: true` with a correct `replay_from`
- `tail=N` starts at exactly `max(0, next_offset - N)`
- subscriber queue overflow → subscriber disconnected, **reader unaffected**, offsets
  still monotonic
- log hits `log_max_bytes` → `log_capped_at` set, file stops growing, `next_offset`
  keeps advancing, live streaming continues
- replay across a cap boundary stops at `log_capped_at`; `after >= log_capped_at`
  replays nothing and `ready` reports `replay_from == next_offset`
- **restart a capped session → `next_offset` does not move backwards** (this is the one
  that silently corrupts every attached client if `max()` is written as file-wins)

### 3. Protocol tests

- `ready` is always the first frame
- binary server frames carry a correct 8-byte big-endian offset
- input from an observer is rejected with `not_controller` and never reaches the PTY
- resize from an observer is rejected
- `claim_control` preempts; the old controller gets `control_revoked`
- **`mode=control` on attach does not preempt** — a second client attaching with
  `mode=control` while someone holds the lease gets `control:false`
- controller drops, reconnects within `control_grace_ms` with the same `client_id` →
  resumes the lease
- controller drops, another client claims, the original reconnects with `mode=control`
  → original stays an observer (the ping-pong regression test)
- grace window expires with no reconnect → lease free; no auto-grant to an observer
- `ready` carries the current `cols`/`rows`; a `resize` from the controller reaches
  observers as `resized`
- `exit` frame carries the final offset and matches `output_bytes`
- bad `Origin` → upgrade rejected
- bad `Host` → upgrade rejected
- **missing `Origin` + valid credential → accepted** (this is a native client, not an
  attack; asserting the opposite would block every future mobile app)
- missing `Origin` + no credential → rejected outside stage 1
- `Authorization: Bearer` accepted on the WS upgrade, not just `?token=`
- unauthenticated `/health` omits `device_id`/`device_name`; authenticated includes them
- `/health` advertises `api_versions` and `capabilities`
- **every `/api/v1` route except `/health` rejects a request with no token**, including
  one arriving on loopback
- token compare is constant-time (assert the implementation, not the timing)
- `POST /sessions` with a nonexistent executable → `422`, not `404`, and no row written
- `max_sessions + 1` concurrent spawns → `429`, daemon healthy

### 4. Persistence / restart tests

- restart with a session marked `running` → becomes `lost` / `daemon_restart`
- `output_bytes` in the DB is stale on restart → **file length wins**
- log survives restart and `/log` still serves the full range
- GC deletes the directory before the row; a crash mid-GC leaves a row without a log,
  not a log without a row
- log cap reached → appending stops, `log_capped_at` recorded, live streaming continues

## Failure-injection checklist

Every one of these must produce a defined, tested state — not an exception trace.

| Injection | Expected |
|---|---|
| Kill the browser tab | session continues; reattach replays cleanly |
| Disable networking mid-stream | client reconnects with backoff from its offset |
| Sleep/wake the phone | `visibilitychange` triggers immediate reconnect |
| Fill a subscriber queue (pause the client, blast output) | that client disconnects (1013); PTY drain rate unchanged |
| Rapid resize (100/s) | coalesced; PTY size ends correct; no deadlock |
| Two clients both claiming control repeatedly | lease always held by exactly one; no lost input to the wrong PTY |
| Kill an agent process externally (`kill -9`) | state `exited` and exit code recorded from the child wait; reader stops on EOF ([S2](15-open-questions.md#s2--eof-is-not-exit)) |
| Kill `teleportd` (SIGKILL) | on restart, sessions are `lost`, logs intact and readable |
| Close a ConPTY session under heavy output load | no deadlock on Win < 24H2 **or** ≥ 24H2; output tail drained |
| Restart the daemon while a client is attached | client gets a clean close; reconnect shows `lost` state |
| Fill the disk | append fails → session marked with `io_error`, daemon stays up |
| `cwd` deleted after spawn | child's problem, not a daemon crash |
| Child exits leaving a grandchild holding the PTY | session reaches `exited` anyway; the master staying open must not strand it in `running` ([S2](15-open-questions.md#s2--eof-is-not-exit)) |
| `terminate` while a 1 MiB write is blocked on an unreading child | terminate completes inside the bounded policy; input is dropped, never allowed to hold the session open ([S3](15-open-questions.md#s3--a-blocking-write-wedges-terminate)) |
| Spawn a nonexistent executable | `422` from pre-spawn validation — **no row, no directory** ([01](01-architecture.md#session-creation-sequence)) |
| Spawn fails *after* validation (permissions, fork failure) | `500`, row recorded `exited` / `spawn_failed`, directory exists and is collected on schedule |
| Second daemon started as another OS user | binds an ephemeral port; neither daemon can read the other's token |
| Port 7337 already taken | ephemeral fallback; `<data_dir>/port` names the real one |
| Update the daemon with sessions running | refused/deferred; sessions keep running ([08](08-packaging.md#updates-must-not-kill-sessions)) |
| Log hits the size cap mid-run | appending stops, live stream continues, offsets keep advancing |

## Load sanity

Not a benchmark suite — a guard against architectural regressions.

- 20 concurrent sessions each emitting 1 MB/s → no dropped bytes in any `output.vt`
- one attached client at 1 MB/s sustained → memory flat (no unbounded queue growth)
- one *paused* client while output flows → memory flat, then the client disconnects

If memory grows unboundedly anywhere, a queue is unbounded — find it.

## Manual pre-release pass

Run on all three OSes:

1. Launch a shell session, type, resize the window, exit cleanly.
2. Launch an agent preset, let it run, close the browser, reopen, verify replay.
3. Attach the phone over Tailscale Serve, take control, type, hand control back.
   Put the desktop to sleep while it holds control, take control on the phone, wake the
   desktop — the phone must keep control.
4. Reboot the host; confirm Tailscale `--bg` sharing resumes and old sessions show
   `lost` with readable logs.
5. Kill `teleportd`; restart; confirm the UI states the truth about what was lost.
