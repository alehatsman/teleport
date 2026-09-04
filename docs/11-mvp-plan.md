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
Linux — **met, 10/10** (see "Delivered for Unix" above). The fixtures are themselves
Unix-shell fixtures (`/bin/sh` scripts, `libc::kill`/`killpg` process-tree checks) and
`daemon/tests/pty_primitive.rs` is `cfg(unix)`-gated accordingly; porting them to run on
Windows (a `cmd.exe`-based equivalent suite, not just lifting the `cfg` gate) is tracked
as [W2](15-open-questions.md#w2--windows-fixture-parity-not-yet-attempted), not silently
implied by this Gate. Two of the ten — `child exits normally`, `child exits nonzero` —
are additionally blocked on
[W1](15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows)
even once ported; the rest, including user-initiated `terminate` (hard-kills, unaffected
by W1) and the grandchild-tree-close case, are expected to pass on Windows once W2 lands.

---

## M2 — Session ownership and backpressure

> **Delivered 2026-09-04** — `daemon/src/session.rs` + `daemon/tests/session_backpressure.rs`
> (4 fixtures, one a `#[cfg(test)]` unit test co-located in `session.rs` for access to
> private `Fanout` state; 17/17 total daemon tests green on Linux, `cargo clippy
> --all-targets -- -D warnings` clean). Built on `pty.rs`'s `on_output` closure hook, as
> that module's M1 note anticipated. `next_offset` and the subscriber registry share one
> `Fanout`, guarded by one `Mutex` separate from `Session`'s own PTY handle -- creating/
> listing sessions never contends with another session's hot output path. The byte half
> of the 256-chunk/8-MiB bound is a `tokio::sync::Semaphore` per subscriber: `publish`
> acquires a chunk's length in permits *before* the chunk is visible via `try_send`, and
> `Subscription::recv` returns them once drained, so the count that gates admission and
> the count a subscriber can observe never diverge -- `tokio::sync::mpsc`'s own bound is
> chunk-count-only. Overflow and channel-closed both collapse to the same immediate
> `retain`-based removal (no separate "mark slow" state) since there's no WS close code to
> emit yet — that distinction is M4's. `Session` holds `PtySession` directly, no `Mutex`:
> `TerminalSession`'s methods take `&self` (docs/03-pty-layer.md#the-terminalsession-trait),
> so `write`/`resize`/`terminate` run independently rather than serialized behind one lock
> — a write stuck on a full channel no longer wedges `terminate`. `terminate()` removes
> the session from `SessionManager`'s directory (a `Weak` back-reference lets it do so
> without every caller remembering to), so a terminated session's pty.rs threads and
> channels can actually be dropped instead of leaking for the daemon's lifetime; a
> session whose child exits on its own, with nobody calling `terminate()`, is not yet
> reclaimed -- that needs `exit_rx` wired to session state, which is M4/M7 as noted below,
> not this pass. `exit_rx`/`eof_rx` from `pty::spawn` are otherwise left unconsumed; wiring
> session state to them is M4/M7, not backpressure. No persistence: `output.vt` and the
> log are M3.
>
> **Amended during M3.** The gate fixture
> (`slow_subscriber_is_disconnected_and_never_blocks_the_reader`) failed roughly three
> runs in five, which was misread at the time as a timing flake under CPU contention. It
> was not: its draining subscriber used `subscribe()`, so it never saw the tens of KiB
> the child had already produced before `create()` returned, and could not account for
> all 10 MiB no matter how fast it drained — a subscribe race showing up as a stall
> indistinguishable from the backpressure failure the test exists to catch. It now
> attaches at 0 and counts the replay, which is exactly the race
> [M3's `attach`](#m3--append-only-replay) was built to close. 8/8 clean runs after.

**Deliver:** `session.rs` — `SessionManager`, `Session`, subscriber registry.

- a session survives **zero** subscribers, indefinitely
- bounded per-subscriber queue (256 chunks / 8 MiB), `try_send` only
- overflow marks the subscriber slow and disconnects it; the reader never blocks
- one mutex guards `{next_offset, subscribers}`; the reader holds it briefly

**Gate:** with a subscriber that never reads, PTY drain rate is unchanged and daemon
memory stays flat. Verified under 1 MB/s sustained output.

**D1 resolved 2026-09-04, before M4.** As originally specified, a subscriber accumulated
live output for the whole duration of its replay and could overflow this same 8 MiB bound
before it ever went live — which made a busy session permanently unattachable. The attach
ordering was correct; the budget was not. The fix is the **moving boundary**
([04](04-api-protocol.md#catch-up--register-late-not-early)): `Session::attach` no longer
registers, it returns a `Replay` that serves history in 1 MiB rounds off the fan-out
mutex, and registration happens only once the remaining gap is ≤ 1 MiB — one eighth of
the queue bound, leaving seven eighths of headroom for live output. Still one buffer,
still 8 MiB. Delivered in `session.rs` + two gate fixtures in
`daemon/tests/session_catchup.rs`.

---

## M3 — Append-only replay

> **Delivered 2026-09-04** — `daemon/src/log.rs` + `daemon/tests/output_log.rs`
> (6 fixtures, platform-independent) + `daemon/tests/session_replay.rs` (4 fixtures,
> `cfg(unix)`, the gate itself) and a second `#[cfg(test)]` unit test in `session.rs`;
> 32/32 daemon tests green on Linux, `cargo clippy --all-targets -- -D warnings` clean,
> cross-compile-checked for `x86_64-pc-windows-gnu`. `next_offset` **moved out of
> `session.rs` into `OutputLog`**: the ordering rule above is only enforceable if
> "append to the file" and "advance the offset" are one call, so `Fanout` now holds an
> `OutputLog` behind the same mutex it already held the counter behind, and `publish()`
> is `log.append()` followed by the fan-out. The identity is asserted after *every*
> operation in `output_log.rs`, not just at the end. `Session::attach(from)` is the new
> atomic attach: it reads the replay boundary `N`, opens a read handle, and registers
> the subscriber under one lock acquisition, so every chunk that subscriber ever
> receives starts at exactly `N` — that, not a dedup pass, is what closes
> [the attach race](04-api-protocol.md#attach-race). `from > N` returns
> `AttachError::OffsetAhead` (M4 renders it as the `offset_ahead` frame).
>
> **Amended by [D1](#m2--session-ownership-and-backpressure) (2026-09-04, same day).**
> `attach` no longer registers. It returns a `Replay` — the `ready` fields plus a read
> handle — and `Replay::next_round` serves history in bounded rounds, registering only
> in the round that finds the remaining gap small enough to sit in a subscriber queue
> alongside live output. The boundary and the registration are still one lock
> acquisition, so the attach race stays closed exactly as described above; what moved is
> *which* round takes it.
>
> Three resolutions the docs left open, recorded rather than silently picked:
> **(1)** 05's "buffered append" is `write_all` straight to the file, not a userspace
> `BufWriter` — a buffered byte is not visible to the separate read handle replay uses,
> so buffering it would publish offsets whose bytes a reconnect cannot read. The page
> cache is the buffer, and no `fsync` ever runs on the reader thread or under the
> fan-out mutex — one `LogSyncer` thread for the whole daemon holds a second reference
> to each log's open file and flushes on the 2 s timer, so a slow disk stalls the syncer
> instead of the terminal. **(2)** A chunk straddling
> `log_max_bytes` is written up to the limit and truncated there, so `log_capped_at`
> is always exactly `log_max_bytes` for a log that got capped by growing. **(3)** After
> a failed write the log stops persisting but keeps advancing offsets (05's `io_error`
> rule); `readable_end()` is therefore the actual file length, which is what every read
> clamps to — equal to `min(next_offset, log_capped_at)` in the normal and capped
> cases, and honest in the degraded one. A failed append sets `log_capped_at` as well, so
> the hole is *reported* rather than served as a replay range that stops short of a live
> stream resuming past it. Reopening with a stored `output_bytes` ahead of the file and
> no recorded cap reports the cap at the file length for the same reason.
>
> Not wired: `StoredState` is the M7 seam and `create()` always passes `None`, so
> restart recovery is proven at the `log.rs` level, not yet end to end through SQLite.
> `LogEvent`s (`Warned`/`Capped`/`IoError`) are returned by `append` and traced — the
> `session_events` rows they become are M7. Bounded attach (`tail`, `max_replay_bytes`,
> `truncated`) is deliberately still M4: `attach()` serves the full range and M4 narrows
> `replay_from` before reading, which is what keeps a VT state snapshot substitutable
> for a byte range later ([04](04-api-protocol.md#the-vt-state-caveat--read-this-before-implementing)).

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

**Met on Linux, 4/4 + 6/6.** In order:
`disconnect_between_chunks_and_reconnect_has_no_gap_or_duplicate` (2 MiB, drops the
subscription between two `recv()`s, lets output accumulate with zero subscribers,
re-attaches at the held offset, and compares the accumulated stream against `output.vt`
byte for byte); `repeated_attach_against_a_concurrent_writer_never_gaps` (200 attach
rounds against a live writer, each asserting the replay range starts where the last one
stopped and the first live chunk starts at the boundary);
`replay_across_a_cap_stops_at_the_cap_and_live_output_continues`; and
`reopening_a_capped_log_does_not_rewind_next_offset` in `output_log.rs`. The restart leg
is exercised as a reopen with a `StoredState`, not a daemon process restart — there is
no SQLite to restart from until M7, and the `max()` rule the gate is really about lives
in `OutputLog::open`. `output_log.rs` is not `cfg(unix)`-gated, so the offset model runs
on the whole CI matrix; the four gate fixtures spawn `/bin/sh` and are Unix-only, same
boundary as [W2](15-open-questions.md#w2--windows-fixture-parity-not-yet-attempted).

---

## M4 — HTTP + WebSocket API

> **Delivered 2026-09-04** — `daemon/src/api.rs`, `ws.rs`, `auth.rs`, `config.rs`,
> `presets.rs`, plus the M4 slice of `session.rs` (metadata, the `running`/`closing`/
> `exited` state machine wired to `exit_rx`, and the control lease) and `device.rs`/
> `main.rs` moving into the library so `api.rs` can reach them. `daemon/tests/http_api.rs`
> (8 fixtures, in-process via `tower::oneshot`) and `daemon/tests/ws_protocol.rs` (14
> fixtures, a real socket via `tokio-tungstenite`) plus unit tests in `auth.rs`,
> `config.rs`, `presets.rs`, `session.rs` and `ws.rs`; 91/91 daemon tests green on Linux,
> `cargo clippy --all-targets` clean.
>
> **A conflict with M2, found and resolved, not silently picked:** `Session::terminate()`
> removed its session from `SessionManager`'s directory immediately, and a M2 test
> asserted exactly that. This doc's own `DELETE` contract requires the opposite — a
> terminated session stays listed as `exited` until an explicit `?purge=true`. Confirmed
> with the user before changing shared behavior a prior, already-gated milestone depended
> on: `terminate()` no longer self-removes (state still flips to `closing` immediately,
> `exited` once the exit-listener thread observes the child's actual exit);
> `SessionManager::purge` does the removal, called from `DELETE ?purge=true` after the
> directory is deleted. The M2 test was rewritten to assert the new contract instead of
> the old one.
>
> **Bounded attach lives in `ws.rs`, not `session.rs`** — `bound_attach` narrows the
> requested `after`/`tail` against `default_tail`/`max_replay_bytes` *before* calling
> `Session::attach`, exactly as M3 promised. `truncated` is true when that narrowing
> actually moved `after` forward, or when the catch-up loop itself gave up
> (`!attach.caught_up`); plain `tail`/default-tail replay is never flagged truncated —
> it never promised more.
>
> **Not wired:** the short-lived `POST /api/v1/ws-ticket` mitigation for tokens in the
> WS query string ([06](06-security.md#token-on-the-websocket-upgrade) lists it as
> "add before the daemon is routinely reachable off-host", not required for the MVP);
> a `?from=&to=`/`Range` implementation on `GET /log` exists but only the byte-range
> subset, no multipart ranges. [N2](15-open-questions.md#n2--websocket-compression)
> (WebSocket compression) is still open — flagged to investigate before this milestone
> closes, not done here; picking it up is a half-day task, not a blocker for M5.
> Windows: cross-compile-checked only, no fixtures run (same boundary as
> [W2](15-open-questions.md#w2--windows-fixture-parity-not-yet-attempted)).
>
> **Review pass (2026-09-04), before this milestone closed:** an independent code
> review of this diff found ten correctness bugs, all fixed here rather than carried
> into M5. Three shared one root cause — blocking work on the async runtime and an
> unenforced cap: `create_session` and `DELETE ?purge=true` ran
> `SessionManager::create`/`Session::terminate` (blocking `cwd`/`$PATH` checks, fork/exec,
> up to ~7s to die) inline on the Tokio worker instead of `spawn_blocking`; and
> `max_sessions` was checked, then the lock released, before the eventual `insert`, so
> concurrent creates could all pass the cap before any of them landed. Fixed together: a
> `reserve_slot`/`ReservationGuard` pair makes the check-and-reserve one atomic step, and
> both paths now do their blocking work through `spawn_blocking`. Two more were a
> control-lease race: `is_controller` then `write` were separate lock acquisitions (a
> concurrent `claim_control` could land in the gap), and the lease was keyed on
> `client_id` alone with no way to tell apart two simultaneous connections sharing one
> (a reloaded tab racing its own not-yet-closed socket). Fixed with a lease `epoch`
> bumped on every grant and checked alongside `client_id`, and `write_if_controller`,
> which holds the lease lock across the check and the write. The rest, each independent:
> `max_sessions` was counting `exited`-but-unpurged entries against the cap (now only
> `Running`/`Closing` do — routine create/DELETE traffic without `?purge=true` could
> otherwise wedge `create()` at `429` forever with nothing actually running);
> `resolve_executable` checked a relative command path against the daemon's own cwd
> instead of the session's requested `cwd` (what `pty::spawn` actually uses); a
> malformed `Range` header's `end + 1` could overflow `u64::MAX`; an explicit `args: []`
> was indistinguishable from an omitted field and silently got the preset's default args
> substituted back in; and `presets.toml` was checked for valid TOML but never for a
> duplicate id or an empty command. All ten have a regression test; 91/91 daemon tests
> green, `cargo clippy --all-targets` clean.

**Deliver:** `api.rs`, `ws.rs`, `auth.rs`. Full surface from
[04-api-protocol.md](04-api-protocol.md).

- `/health` with `api_versions` + `capabilities`; device fields behind the principal
- sessions CRUD, `/log`, `/stream`
- mixed framing: text = control JSON, binary = raw bytes with an 8-byte BE offset prefix
- attach sequence in the correct order ([04](04-api-protocol.md#attach-race)), driving
  `Replay::next_round` to the live boundary and writing each round to the socket before
  asking for the next ([D1](04-api-protocol.md#catch-up--register-late-not-early))
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

**Met on Linux.** `ws_protocol.rs` covers: `ready` always first, correct/contiguous
binary offsets, observer input/resize rejected, `claim_control` preempts and notifies
the loser, `mode=control` never preempts, disconnect-grace resume for the same
`client_id`, the ping-pong regression (a reconnect after someone else claims stays an
observer), grace expiry with no auto-grant, `resized` broadcast to observers, the `exit`
frame's `final_offset`, and bad/missing `Origin` handling. `http_api.rs` covers
authenticated vs. unauthenticated `/health`, every route but `/health` rejecting a
missing token, `422` (not `404`) with no row written for a bad executable, `429` at
`max_sessions + 1` with the daemon still healthy, the full create→list→terminate→purge
lifecycle, and `bad_origin` on a mutating request. `bound_attach`'s arithmetic (`tail`,
default-tail, the `max_replay_bytes` clamp and its `truncated` flag) is unit-tested
directly rather than through a socket. Not covered by an automated test: the 20 s/60 s
Ping/Pong keepalive timing itself (implemented, not worth a real 60-second test run).

---

## M5 — Browser terminal

> **Delivered 2026-09-04** — `web/src/lib/{types,identity,api,stream}.ts`,
> `Terminal.svelte`, `Sessions.svelte`, `Session.svelte`, `App.svelte`; `daemon/src/main.rs`
> serves `web/dist` with SPA fallback. Two bugs only a real browser caught, not
> typechecking: `sendInput` handed the WebSocket a JS string, which always goes out as a
> *text* frame under the mixed-framing protocol — the daemon correctly rejected every
> keystroke as `bad_request` and no input had ever reached a PTY through this client;
> fixed by encoding to UTF-8 bytes before send. And a reopened tab always came back an
> observer even for the client that held control seconds earlier, because `wantControl`
> lived only in the in-memory `SessionStream` a page load discards — persisted per
> session id in `identity.ts` instead, cleared the moment the server says otherwise. A
> follow-up review pass then caught `terminate`/`purge` in `Sessions.svelte` with no
> error handling (unlike `refresh`/`launch`), so a failed `DELETE` threw unhandled and
> reached the user as a silent no-op button — caught into the same `loadError` banner.
> `npm run check` and `npm run build` clean throughout.

**Deliver:** `web/` — session list, session view, xterm.js behind `Terminal.svelte`,
`stream.ts` implementing the offset contract.

- `binaryType = "arraybuffer"`; write `Uint8Array` to xterm directly (no string decode)
- jittered reconnect; **never** clear the buffer on reconnect
- daemon serves `web/dist` with SPA fallback

**Gate:** close the tab mid-agent-run, reopen, and the terminal shows a correct
continuous transcript.

**Met on Linux** — verified against a real daemon with a Playwright script driving two
browser contexts (docs/10-testing.md's manual-pass style, run mid-milestone rather than
only pre-release): create a session, take control, type a distinguishable command,
confirm the echo, close and reopen the tab (replay continuous, control auto-resumed),
second client attaches as observer and preempts, first client gets the "Control taken
by" toast and demotes. Not saved as a repo test file — cross-client control-lease
behavior is manual/scripted verification by design, not an automated e2e suite (see M6).

> **Not** "the same on a phone over the LAN" — the daemon binds loopback and refuses
> anything else without `--i-know-what-im-doing` ([06](06-security.md#listener)), and
> Tailscale does not arrive until M9. Test a second *local* browser here; the phone is
> the M9 gate. If you want the phone earlier, use the escape-hatch flag deliberately and
> never let it become the development default.

---

## M6 — Control lease

> **Delivered 2026-09-04** — no new code: this milestone's whole surface already
> existed. The epoch-based lease (`daemon/src/session.rs`: `attach_control`,
> `claim_control`, `release_control`, `begin_control_grace`, `write_if_controller`,
> `is_controller`) and its `ws.rs` wiring (`not_controller` on input/resize from a
> non-controller, `control_revoked` to the loser) landed as part of M4's review pass —
> a lease race there (`is_controller` then `write` as separate lock acquisitions, and no
> way to tell apart two connections sharing one `client_id`) needed the epoch mechanism
> to fix, so the M6 protocol surface shipped a milestone early, with regression coverage
> in `daemon/tests/ws_protocol.rs` (`claim_control_preempts_and_notifies_the_loser`,
> `resize_from_the_controller_reaches_observers_as_resized`, `not_controller` on both
> input and resize from an observer). The UI half (`Session.svelte`'s Controlling badge
> / Take-control button / revocation toast, `stream.ts` never auto-sending
> `claim_control` on reconnect) then shipped with M5 and was gate-tested there.
> Re-verified independently today rather than trusting that record at face value:
> two live browser contexts (desktop + phone viewport) against a real daemon and the
> built SPA — desktop claims control, phone correctly renders as observer (no badge, no
> resize authority); desktop resizes, phone letterboxes instead of fighting it; a rogue
> WebSocket opened directly (bypassing the UI) has both an input frame and a resize
> rejected with `not_controller`, confirmed by their absence from `/log` and unchanged
> `cols`/`rows` on the session — not just the error frame; phone claims control,
> preempts the desktop, desktop gets the toast and demotes with no data loss; a second
> rogue connection is rejected the same way post-preemption, confirming the daemon
> enforces this per-lease-holder, not by special-casing the first client. All ten checks
> passed. 99/99 daemon tests green.

**Deliver:** single-controller enforcement end to end.

- `mode=control` on attach, `claim_control` / `release_control`
- preemptive claims; `control_revoked` to the loser
- observers' input and resize rejected with `not_controller`, never buffered
- UI: Controlling / Observing / Take control

**Gate:** a phone and a desktop attached simultaneously never fight over PTY size, and
input never reaches the PTY from a non-controller.

**Met on Linux** — see the independent re-verification above.

---

## M7 — SQLite metadata and recovery

> **Delivered 2026-09-04** — `daemon/src/persistence.rs`: a single writer-actor thread
> owning one `rusqlite::Connection` (WAL, `synchronous=NORMAL`, `busy_timeout=5000`), fed
> by an `mpsc` command channel with `oneshot` replies; `_blocking` methods for
> `session.rs` (entirely synchronous — `blocking_send`/`blocking_recv`), plain `async fn`s
> for `api.rs`'s handlers, and fire-and-forget `note_*` methods (`try_send`, no reply) for
> the one path reachable from the PTY reader thread, which must never wait on SQLite.
> `PRAGMA user_version` migrations, one migration so far (the full schema from
> [05](05-persistence.md#schema)). 106 daemon tests green (7 new persistence unit tests
> plus one new black-box gate test), 0 clippy warnings.
>
> **Session/SessionManager wiring:** `SessionManager` takes an `Option<persistence::Db>`
> (`with_db`, `None` in every pre-M7 test fixture — no unrelated test now needs SQLite).
> `create()` inserts the row *after* `output.vt` opens, *before* `pty::spawn`
> (docs/01-architecture.md#session-creation-sequence: the row must exist before the child
> can produce a byte); `terminate()` marks `closing`; the exit-listener thread makes the
> one terminal write a live session ever makes (always `state='exited'` — see below);
> `resize()` and the reader-loop's publish closure fire-and-forget `cols`/`rows` and a
> throttled (≤1/s) `output_bytes`, both via lock-free `try_send`, so SQLite never sits on
> the PTY drain path. Every DB write from a live `Session` is best-effort (`warn!` on
> failure, never fails the caller) — a lost metadata write degrades restart recovery, not
> the live session.
>
> **A design decision the doc left implicit, resolved rather than silently picked:** a
> recovered/historical row (`lost`, or `exited`-but-unpurged from a previous process) is
> **not** reconstructed as a `crate::session::Session` — a `Session` always owns a live
> `PtySession`, and there is no such thing as one with no PTY
> (docs/01-architecture.md#the-crash-boundary: "SQLite can remember that a process used
> to exist; it cannot recreate an OS PTY handle"). Instead `api.rs`'s `list_sessions` /
> `get_session` / `get_log` / `delete_session` fall back to `persistence::Db` and
> `log::LogReader::open`'s standalone-file path when a session id isn't held live by
> `SessionManager` — `LogReader::read_range` already self-clamps to the real file length,
> so this can't over-read regardless of what the DB row's `output_bytes` says. `GET
> /api/v1/sessions` merges live + historical, live winning on a duplicate id (fresher —
> `controller`/`subscribers` a DB row can't have, and a live row's own `output_bytes`
> column can itself lag by up to a second).
>
> **Two scope cuts, recorded in `persistence.rs`'s module doc rather than made silently:**
> (1) `session_events` only ever gets `created`/`exited`/`lost` rows — `resized`,
> `control_granted`/`_revoked`, `subscriber_attached`/`_detached`, `slow_consumer`,
> `bell`, `idle` would each need new plumbing through `ws.rs`/the control lease/the reader
> loop, out of proportion to this milestone's gate; the schema already has the room. (2)
> `sessions.log_capped_at` is never written — stays `NULL` even once a log actually caps
> — purely informational (harmless: `read_range` self-clamps regardless), fixed by the
> same `note_*` pattern whenever someone next touches capping.
>
> GC (directory-first-row-second, `retain_days` default 14, new `config.toml` field) runs
> once at startup and every 6h via a detached `tokio::spawn` loop in `main.rs`.
>
> **Gate, verified two ways.** Unit-level (`persistence.rs`'s own tests): insert a
> `running` row, write real bytes past the stored `output_bytes` directly to
> `output.vt`, reopen the `Db` — asserts `state=lost`, `lost_reason=daemon_restart`,
> `output_bytes` reconciled to the file (file-wins case), and separately a capped-log
> case where the column stays ahead (column-wins case). Black-box
> (`daemon/tests/restart_recovery.rs`, new — same style as `skeleton.rs`): spawns the
> real `teleportd` binary, creates a session via a hand-rolled HTTP/1.1 client over a raw
> `TcpStream` (no HTTP client dependency for one test file), lets it produce real output,
> `SIGKILL`s the process, restarts it against the same data dir, and asserts over the real
> API that the session reads back `lost`/`daemon_restart`, `output_bytes` matches the
> file exactly, and `/log` serves the complete, untruncated log. Ran 5x with no flakes.

**Deliver:** `persistence.rs` — schema, migrations via `user_version`, single writer
actor, startup recovery.

- WAL, one writer thread, mpsc + oneshot
- **no terminal bytes in rows**; **no `env` column**
- startup: stale `running`/`closing` → `lost` / `daemon_restart`; reconcile
  `output_bytes` from actual file length
- GC: directory first, row second

**Gate:** `SIGKILL` the daemon mid-session; on restart the session reads `lost`, the log
is complete and readable, and `output_bytes` matches the file.

**Met on Linux** — see the black-box `restart_recovery.rs` test in the delivered
callout above.

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
