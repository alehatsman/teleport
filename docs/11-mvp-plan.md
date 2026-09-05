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
                     ├─ M10 Tauri shell
                     └─ M11 CLI client
```

M10 and M11 are siblings, not a chain — each only needs M9's protocol/auth surface, and
neither blocks the other (same reasoning [13-native-clients.md](13-native-clients.md#two-phases-two-different-blockers)
already applies to a native mobile Phase 1: nothing here is a technical dependency,
just where the work happened to land first).

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

> **CI flake found and closed, 2026-09-04:** `eof_and_exit_are_independent_signals`'s
> detach recipe used `perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' -- sh -c '...'`
> (picked over the `setsid(1)` binary, which is util-linux and not on stock
> macOS). Confirmed failing for real on `macos-latest`: the grandchild produced
> *zero bytes* of output, meaning `perl` itself silently never started -- the
> exact risk the recipe's own comment named, just not guarded against. Root
> cause traced back to S2's own spike finding: what actually matters is SIGHUP
> ignored *before* the fork races the parent's `exit 0`; a plain `trap '' HUP`
> on the parent covers a backgrounded `sleep` (no exec of its own) just as
> well, since fork inherits signal disposition and an ignored signal survives
> `exec` too. Dropped `perl`/`setsid(1)` entirely -- verified locally that
> `trap '' HUP; sleep 2 & echo $!` alone reproduces the same timing (direct
> child exits ~1ms, EOF ~2000ms) -- one less external dependency for CI to be
> missing. Additionally hardened by having the grandchild `echo` its own pid
> (`$!`, same idiom as `terminate_kills_the_grandchild_process_tree`) and
> asserting it's alive via `kill(pid, 0)` right after the direct child exits --
> turns "it really started" into a fact instead of an inference from timing,
> so the EOF-latency floor is no longer gambling on that. Deliberately does
> *not* assert the grandchild is dead once EOF fires: past reparenting,
> whatever ancestor reaps it (usually init) may not do so immediately, so a
> live `kill(pid, 0)` doesn't mean still-running -- asserting otherwise would
> just be a different flake.

> **...and reopened, then actually closed, 2026-09-05.** The recipe above fixed the
> `perl`-never-started failure but the fixture still wasn't green on `macos-latest`, so it
> was `#[cfg(target_os = "linux")]`-gated pending real hardware
> ([S5](15-open-questions.md#s5--a-detached-grandchild-cannot-hold-a-pty-past-its-parent-on-macos)).
> Run on an actual Mac, the "flake" turns out to be perfectly deterministic: macOS/XNU
> `proc_exit` calls `VNOP_REVOKE(ttyvp, REVOKEALL)` on a session leader's controlling
> terminal, which invalidates every descriptor to that tty in every process no matter
> what signals anyone is ignoring, whereas Linux's `disassociate_ctty()` explicitly
> exempts ptys and only sends `SIGHUP`. Not a race — a kernel difference, proven with
> `spike/src/bin/s10_ctty_revoke.rs` (the grandchild is alive but its writes return
> `EIO`; a `noctty` control behaves exactly like Linux). The gate is gone, replaced by a
> macOS twin fixture that asserts the real behaviour *and* that the grandchild survives
> the revoke. No output is lost — XNU drains before it revokes, measured byte-exact.

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
> **CI gap found and closed, 2026-09-04:** `http_api.rs` was missing the
> `cfg(unix)` gate every other `/bin/sh`-driving test file has, so `cargo test`
> actually ran its fixtures on `windows-latest` -- 3 of them 422'd (no `/bin/sh`
> there) instead of the 201/CREATED they hardcode. Gated it like its siblings;
> Windows CI now matches the "cross-compile-checked only" boundary this doc
> already claimed.
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
> `control_granted`/`_revoked`, `subscriber_attached`/`_detached`, `slow_consumer` would
> each need new plumbing through `ws.rs`/the control lease, out of proportion to this
> milestone's gate; the schema already has the room. `bell`/`idle` were in this same cut
> at M7 but got wired for real in M8 below — see that callout. (2)
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

> **Delivered 2026-09-05.** Most of this milestone's surface, like M6's, had already
> shipped as part of M4/M5: `presets.rs`, `presets.toml`, `GET /api/v1/presets`, and a
> working preset `<select>` + custom-command fallback in `Sessions.svelte` all predate
> this callout. Two gaps remained and are closed here:
>
> **Recent working directories** (`web/src/lib/Sessions.svelte`): a `recentCwds` derived
> value dedupes `sessions` by `cwd`, keeps each one's most recent `created_at_ms`, sorts
> descending, caps at 8, and feeds a `<datalist>` on the `cwd` input — progressive
> enhancement over the existing free-text field, no new storage or endpoint (`sessions`
> was already being polled for the list view).
>
> **D3 — attention signals** (docs/15-open-questions.md, now resolved and removed from
> that file): D3's own text turned out to be stale — it claimed M2 recorded `bell`/`idle`
> rows that nothing read, but `persistence.rs`'s M7 module doc already said the opposite:
> those two were never inserted. Built for real instead of just wiring a read side:
> `session.rs`'s `on_output` closure scans every chunk for a BEL byte (`Session::last_bell_ms`,
> throttled `session_events` write); a new periodic sweep task in `main.rs` (same shape as
> `spawn_gc_task`) calls `Session::tick_idle` on every live session every 5s, detecting
> output gone quiet for 30s while the process stays running (`Session::idle_since_ms`,
> cleared automatically once output resumes). Both are lock-free `Arc<AtomicI64>` fields,
> never contending with the PTY hot path. Surfaced as `last_bell_ms`/`idle_since_ms` on
> `GET /api/v1/sessions` (docs/04-api-protocol.md#get-apiv1sessions), `null` for anything
> not `running`. `Sessions.svelte` shows a badge when a running session is idle or rang a
> bell in the last two minutes (bell recency is bounded client-side; the daemon field
> itself never clears). The 30s idle threshold and 5s sweep interval are new, undocumented-
> elsewhere defaults (`session::IDLE_THRESHOLD_MS`/`IDLE_SWEEP_INTERVAL_MS`) — called out
> as picked, not specified anywhere before now. No `persistence.rs` change was needed:
> `Command::NoteEvent`/`Db::note_event` were already fully generic.
>
> New `daemon/tests/attention_signals.rs` (3 tests): BEL detection against a real spawned
> shell, and `tick_idle`'s set/clear/no-op-after-exit behavior driven with synthetic
> clocks and a tiny threshold rather than a real 30-second sleep. 110/110 daemon tests green,
> 0 clippy warnings. Web: `svelte-check` and `vite build` both clean.

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

**Met on Linux** — the launch/disconnect/replay mechanics this gate is actually about
were browser-verified as part of M4/M5's own delivered callouts (the preset launcher
shipped then). Today's two additions (recent-cwd datalist, attention badge) are
UI-only and were verified via `svelte-check` + `vite build` plus the automated suite
above, not exercised in a live browser this session — noted rather than implied by
reusing M6's "independently re-verified" language for something narrower.

---

## M9 — Tailscale Serve

> **Add Tailscale Serve before inventing application authentication.**

**Deliver:** documented setup, config support, verification.

```bash
teleportd --listen 127.0.0.1:7337
tailscale serve --bg http://127.0.0.1:7337
```

- tailnet hostname in `allowed_hosts`, its origin in `allowed_origins`
- optional bearer token implemented and tested, on by default
- verify `--bg` persistence survives a **full host reboot** on each OS

**Gate:** a phone off the local network, on cellular, reaches a running agent over the
tailnet, takes control, and types into it.

> **Verified 2026-09-05 — config/docs half only; the gate itself is not yet met.**
> `allowed_hosts`/`allowed_origins` (`daemon/src/config.rs`), the bearer-token
> credential (`daemon/src/auth.rs`), and the Tailscale Serve setup instructions
> ([07-remote-access.md](07-remote-access.md#default-tailscale-serve)) were already
> built and documented during M4's auth-seam work — nothing new was written for M9.
> Re-verified rather than re-implemented: full `cargo test` (111 tests, 0 failures),
> `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` all clean, plus a
> local smoke test standing up a real `teleportd` with `allowed_hosts =
> ["fake-machine.tail1234.ts.net"]` / matching `allowed_origins` and driving it with
> curl exactly as a Tailscale Serve reverse proxy would forward a request (tailnet
> `Host` + `Origin` + `Authorization: Bearer`): correct `Host`/`Origin`/token → `201`;
> missing token → `401`; wrong `Host` → `403`; wrong `Origin` → `403`; no `Origin`
> (native-client shape) with a valid token → `201`. One correction made while
> checking this: the bullet above said the bearer token defaults **off** — that
> contradicts [06-security.md](06-security.md#authentication) ("bearer token, on by
> default") and the shipped code (`Config::default().auth_token == true`), both of
> which are correct; the bullet was simply stale and is fixed here.
>
> **Gate met 2026-09-05, on Linux (`mainpc`, real hardware, real tailnet).** Real
> `teleportd` on `127.0.0.1:7337`, `allowed_hosts`/`allowed_origins` set to
> `mainpc.tail37c478.ts.net`, `tailscale serve --bg` proxying it over HTTPS. Phone
> (iOS) with Wi-Fi off, on cellular, opened `https://mainpc.tail37c478.ts.net/?token=…`,
> took control of a session, and typed into it. `tailscale status` showed the phone's
> endpoint move from a private LAN address (an earlier same-Wi-Fi pass, kept as a
> distinct data point below) to a public IP once actually on cellular, connected
> **direct** — no DERP relay. See [N1](15-open-questions.md#n1--keystroke-latency)
> for the RTT numbers and typing-feel report.
>
> **Not done — needs a real reboot, deferred:** `tailscale serve status` after a
> **full host reboot**, to confirm `--bg` persistence actually holds on this OS. Not
> run in this session (rebooting the live dev machine mid-session is disruptive and
> wasn't asked for) — do this whenever convenient and report back; nothing else is
> outstanding for M9.

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

> **Implementation spec drafted 2026-09-05.** Validated against current code
> (`daemon/src/main.rs`, `device.rs`, `api.rs::health`) — no conflicts:
> `/health`, `device.json`, `<data_dir>/token`, `<data_dir>/port` already exist exactly
> as [08-packaging.md](08-packaging.md) assumes, so no daemon change is needed for M10.
> Two open questions resolved with you first: certs aren't in hand, so signed artifacts
> are **not** a target of this milestone (pipeline structure only, gated behind secrets
> that don't exist yet); dev/iteration happens Linux-first, mac/Windows validated via a
> CI matrix rather than local hardware.
>
> **Scope.** In: Tauri **v2** shell, detached daemon spawn/attach, tray (quit-daemon
> with confirmation, open UI, show logs), OS notifications
> (`tauri-plugin-notification`, wired to the existing `bell`/`idle` session events),
> hand-written per-OS autostart (systemd user unit / LaunchAgent plist / Task Scheduler
> XML — not a generic autostart plugin, since those don't reliably hit the specific
> mechanisms 08 calls for), updater staging (`tauri-plugin-updater`, never restarts
> while a session is `running`), signing/notarization CI structure with placeholder
> secrets, Linux `.AppImage`/`.deb` built and run locally.
> Out (deferred, not dropped): real signing certs/notarization credentials (external,
> not a repo change), hosted update-feed delivery ([16](16-release-pipeline.md)'s
> concern once signing exists), any new config-editing UI beyond what 08 already
> allows.
>
> **Interfaces.**
> ```text
> desktop/src-tauri/
>   Cargo.toml         # tauri, tauri-plugin-notification, tauri-plugin-updater,
>                       # tauri-plugin-single-instance
>   tauri.conf.json     # bundle.externalBin -> teleportd-<triple>
>   capabilities/default.json
>   src/
>     main.rs            # health-check/spawn/attach flow, tray, window setup
>     daemon.rs          # spawn_detached(), read_port_file(), read_token(), health()
>     autostart/{linux,macos,windows}.rs
>     updater.rs         # stage-and-check-before-restart
> ```
> No custom `#[command]` touches session state (08's rule, unchanged) — the WebView
> speaks the same HTTP/WS API as the phone. The only commands are shell-scoped:
> `install_autostart`, `uninstall_autostart`, `quit_daemon`, `daemon_status`.
> `daemon.rs` spawns via plain `std::process::Command`, **not**
> `tauri-plugin-shell`'s sidecar helper — see edge cases. The window's URL is resolved
> at runtime from the port file (`WebviewWindowBuilder::url(...)` after the health
> check), not a build-time `frontendDist` — confirm that shape against Tauri v2 before
> writing `main.rs`, since the framework more commonly expects the reverse.
>
> **Edge cases.**
> - **Windows: sidecar dies with the GUI.** *Correction, made scaffolding this spec
>   (2026-09-05): checked against `tauri-plugin-shell`'s actual source rather than
>   trusted from memory — it does not use a Windows Job Object. It sets
>   `CREATE_NO_WINDOW` and pipes stdio through itself, both wrong for a process meant
>   to outlive this app, but not the Job Object claim this bullet originally made.*
>   The real, general Windows risk: if this app's own process is inside a job with
>   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (common from an IDE, CI runner, or some
>   installer contexts), children join that job by default since Windows 8 and die
>   with it. Spawn with plain `std::process::Command` and
>   `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB`.
>   **Spike this first, on a packaged Windows build** (not `cargo run`) —
>   it can look fine in dev and fail only once bundled.
> - Daemon already running (autostart got there first): health check succeeds before
>   any spawn fires — no dedup needed.
>   Two GUI instances launched: `tauri-plugin-single-instance` focuses the existing
>   window instead of racing the spawn-detached path.
> - `/health` answers but wrong shape (someone else's daemon on the port): do not
>   attach; surface it, per 08.
> - Token file unreadable: treat like "refused" — let the daemon start attempt fail
>   loudly rather than looping on a bad health check.
> - Update requested with sessions running: reuse the tray-quit confirmation verbatim.
> - Autostart install fails (no systemd user session, e.g. a minimal container): log
>   and surface in-app; autostart is a convenience, not a launch dependency.
>
> **Validation.**
> 1. Local Linux gate check: launch → daemon starts detached → create a session → quit
>    the app (not tray-quit) → `ps`/`curl /health` from outside show the daemon and
>    session both still alive → reopen → reattaches.
> 2. Tray "quit daemon" confirms the session count, then actually stops everything.
> 3. Extend `release.yml`'s target matrix (or a sibling `desktop-release.yml`) to run
>    `tauri build` on `macos-latest` (both arches) and `windows-latest`, unsigned,
>    artifact-uploaded — proves the build on those OSes; hand-verifying the gate there
>    waits for real hardware or a VM.
> 4. Windows detached-spawn spike re-verified against that packaged CI build.
> 5. `cargo test` unaffected — no daemon code changes in this milestone.
> 6. Browser-only regression: `teleportd` + a plain browser tab, no desktop app
>    installed, still does everything it did pre-M10.
>
> **Scaffolded 2026-09-05** (`desktop/src-tauri/`, branch `m10-tauri-shell`) --
> `cargo check`/`clippy -D warnings`/`fmt --check`/`cargo test` all clean, and a real
> `tauri build` produces a working `.AppImage` + `.deb`. **Item 1 above run live, not
> simulated:** launched the built binary against an isolated scratch data dir
> (`XDG_DATA_HOME` override, not the real `~/.local/share/teleport` -- a real `teleportd`
> from earlier the same day was already running there and was left untouched) --
> health-probed, spawned `teleportd` detached, created a real session
> (`sessions_running: 1`), then `kill -9`'d the GUI process outright (harder than a
> clean quit). Daemon and the session's own process both survived; relaunching
> re-probed and reattached (`"attaching to our daemon" sessions_running=1`) without a
> second daemon spawning. One real bug caught by actually running it rather than only
> compiling: the tray icon needs an explicit fixed-size 8-bit RGBA image
> (`tauri::include_image!("icons/32x32.png")`) -- `app.default_window_icon()` and a
> 16-bit-depth placeholder PNG both panicked at startup with a buffer-size mismatch.
> Also corrected in the edge-case bullet above: the Windows Job Object claim was
> checked against `tauri-plugin-shell`'s actual source and turned out to be wrong as
> originally stated; fixed in place rather than left standing.
>
> Not done here, unchanged from the validation list: items 2-6 (Windows/macOS are
> unverified on real hardware; no CI matrix yet; no signed artifacts, per the
> not-yet-available-certs answer above). Full gap list in `desktop/README.md`.
>
> **Windows daemon stop, closed 2026-09-05 (issue #12).** The gap this scaffold's
> `stop_daemon_flow` TODO named -- `GenerateConsoleCtrlEvent` needs a console shared
> with the target process, which a Task-Scheduler/autostart-launched `teleportd` never
> has, so Windows had no SIGTERM equivalent and the tray's "Stop daemon" action only
> logged an error -- is resolved with the first of the two options that TODO listed: a
> small authenticated `POST /api/v1/shutdown` (docs/04-api-protocol.md#post-apiv1shutdown)
> that triggers `main.rs`'s existing `shutdown_signal()` via a shared `tokio::sync::Notify`,
> so Windows gets the exact same persistence-and-close treatment Unix's `kill(SIGTERM)`
> path already gave. Wired in cross-platform (every OS gets the route, not just
> Windows) rather than making it a special case; Unix's `terminate_gracefully` is
> untouched. `desktop/src-tauri/src/daemon.rs::shutdown_gracefully` (`#[cfg(windows)]`)
> is the new client-side call, replacing the stub in `main.rs::stop_daemon_flow`.
> Verified for real, not just wired up: `daemon/tests/shutdown_endpoint.rs` spawns the
> actual `teleportd` binary, sends a plain hand-written HTTP/1.1 request over a raw
> `TcpStream` the way a curl-based Windows caller would, and asserts the *process*
> exits and its port file is removed -- run repeatedly on this machine (native Windows),
> not assumed from the Unix-passing case.
>
> **Validation item 4 (Windows detached-spawn spike), run 2026-09-05, native Windows
> hardware -- found a real bug, fixed it.** `spike/src/bin/s11_windows_job_breakaway.rs`:
> a harness process assigns itself to a `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` job
> (simulating "launched inside an IDE/CI runner/installer job," per this section's own
> edge-case bullet, without needing an actual one), spawns a long-lived child with
> `spawn_detached`'s exact creation flags, and exits normally -- which is exactly the
> event under test, since it drops the harness's only handle to that job. Four arms, all
> run for real:
>
> | Arm | Result |
> |---|---|
> | `no_breakaway` (control: detached, no breakaway flag) | child dies essentially instantly -- confirms the job's kill-on-close genuinely fires here |
> | `breakaway` (the flags `spawn_detached` shipped with) | **the spawn itself fails**, `ERROR_ACCESS_DENIED` -- not a silent no-op |
> | `breakaway_allowed` (same job, `JOB_OBJECT_LIMIT_BREAKAWAY_OK` also set) | spawn succeeds, child outlives the parent |
> | `no_job` (no containing job at all -- a plain double-clicked launch) | spawn succeeds, child outlives the parent |
>
> The finding: `CREATE_BREAKAWAY_FROM_JOB` only works if the *containing* job itself
> grants `JOB_OBJECT_LIMIT_BREAKAWAY_OK`/`_SILENT_BREAKAWAY_OK` -- many real restrictive
> jobs (some IDE debuggers, CI runners, some installer/sandboxing contexts) do not set
> it, and in that case `CreateProcess` fails outright rather than degrading gracefully.
> Shipped as-is, this would have meant the daemon never starts under precisely the
> launch contexts this flag was added to defend against -- worse than the
> kill-with-the-job risk it exists to prevent. Fixed the same day:
> `daemon.rs::spawn_windows_with_breakaway_retry` tries the breakaway spawn first and,
> only on `io::ErrorKind::PermissionDenied`, retries once without the flag (accepting the
> narrower kill-with-the-job risk only in the specific case where the OS has just said
> breakaway isn't available anyway). `cargo check`/`clippy -D warnings`/`fmt --check`
> clean on the desktop crate.
>
> Not literally a packaged `tauri build` artifact under a real IDE/CI job -- the spike
> exercises the identical Win32 call and identical flags standalone, which is the part
> that was actually in question.
>
> **Closed, 2026-09-06.** `daemon.rs`'s `job_breakaway_tests` calls the real
> `spawn_detached`/`spawn_windows_with_breakaway_retry` (not reimplemented flags) against
> a real `teleportd.exe`, from a test process self-assigned to a real restrictive Job
> Object -- both the restrictive-job and no-job cases pass, run for real on this machine.
> Turned up one more real thing along the way: `spawn_detached` accepted `data_dir` but
> never forwarded it to the child, relying on the daemon's own default resolution to
> independently land on the same path -- harmless in production, but made isolated
> testing impossible without running a real daemon against the developer's actual data
> directory. Fixed by passing `--data-dir` explicitly. Full detail and remaining scope
> (literally inside a built `.msi`/`.exe`, or a genuine IDE/CI job rather than a
> self-assigned equivalent) in `desktop/README.md`.

> **Validation item 1 (the gate itself) re-run on macOS, 2026-09-05 -- real hardware
> (Darwin 24.6.0, arm64, Apple silicon), not CI.** The same box S5 and N5 were
> root-caused on. Built the real bundle (`npx tauri build --bundles app` ->
> `Teleport.app`, unsigned) and ran the gate end-to-end against the real default data
> dir (`~/Library/Application Support/teleport`). No isolation override was needed the
> way the Linux run needed one -- that directory did not exist on this machine, so
> there was no live daemon to protect; it was removed again afterwards.
>
> | Step | Result |
> |---|---|
> | launch | `no daemon reachable; starting teleportd detached`; data dir created `0700`, `token`/`port` `0600` |
> | detached spawn | daemon's `pgid` equals its own pid and differs from the GUI's, no controlling tty -- `process_group(0)` read out of `ps`, not assumed |
> | create a session | `POST /api/v1/sessions` -> `running`, `/health` `sessions_running: 1` |
> | quit | `kill -9` on the GUI's **entire process group**, not just its pid -- strictly harder than the Linux run, and the exact signal path `process_group(0)` exists to survive |
> | survives | daemon alive and reparented to pid 1; the session's own `/bin/sh` alive under it; `/health` still `sessions_running: 1` |
> | reopen | `attaching to our daemon ... sessions_running=1`; exactly one `teleportd`, same pid as before -- no second daemon spawned |
>
> No new bugs. The tray-icon buffer-size panic the Linux run caught did not recur --
> startup completed with `build_tray` running -- and the embedded SPA served
> (`GET /` -> `200 text/html`, so the window had a real UI to load, not a blank page).
> `POST /api/v1/shutdown` (issue #12), previously exercised only on native Windows and
> in `shutdown_endpoint.rs`, was also driven live here for the teardown: `202`, then
> both the daemon and its session process gone.
>
> Still not done on macOS, and not claimed by this run: validation item 2 (the tray
> "Stop daemon" confirmation needs a human click), `autostart/macos.rs`'s launchd
> LaunchAgent, and M9's `--bg` reboot persistence. macOS moves from "CI build only" to
> gate-verified in the cross-OS ledger (#8).

> **Validation item 1 (the gate itself) re-run on Windows, 2026-09-06 -- real hardware
> (Windows 11 Pro, build 26200), not CI.** The same box W1-W3/N5/the job-breakaway item
> above were verified on. Two passes.
>
> First pass used `cargo build --release`'s own `teleport-desktop.exe` + sidecar
> directly (this machine had no node/npm yet, so neither `npx tauri build` nor the
> daemon's `embedded-web` feature were available) against the real default data dir
> (`%LOCALAPPDATA%\teleport`, already populated from earlier work on this machine --
> no isolation override exists in `main.rs`'s launch path, so this ran against real
> data; nothing destructive, one extra session row same as any normal run):
>
> | Step | Result |
> |---|---|
> | launch | `no daemon reachable; starting teleportd detached`; daemon fell back to `serving API only` (no `web/dist` yet) |
> | detached spawn | `ParentProcessId` still points at the GUI (Windows never reparents on exit, unlike Unix's pid-1 reparenting) -- not itself proof; the quit step below is |
> | create a session | `POST /api/v1/sessions` -> `running`, real `cmd.exe` child, `/health` `sessions_running: 1` |
> | quit | GUI's single pid `taskkill /F`'d, not `/T` -- Windows has no whole-process-group-kill primitive to mirror the macOS run's group kill, and "one pid, no tree" is what a real user's Task Manager "End Task" actually does; the harder containing-*job* case is exactly what the job-breakaway item above already covers separately |
> | survives | `teleportd.exe` and the session's `cmd.exe` both alive, same pids; `/health` unchanged |
> | reopen | `attaching to our daemon ... sessions_running=1`; exactly one `teleportd.exe`, same pid as before |
> | shutdown | `POST /api/v1/shutdown` -> `{"status":"shutting_down"}`; daemon logged `shutdown signal received`; both processes exited clean |
>
> Passed, no new bugs -- but not yet against a real installer, the one gap the macOS
> run didn't have (macOS used a real `tauri build --bundles app`).
>
> So node/npm were installed (`winget install OpenJS.NodeJS.LTS`) to close that gap
> for real rather than write it up as a permanent caveat. `npm run build` (web UI) and
> `cargo build --release --features embedded-web` (daemon) both then worked, and
> `npx tauri build --bundles nsis` produced a real, unsigned `Teleport_0.1.0_x64-setup.exe`
> (installed silently with `/S`). One snag getting there, not Windows-gate-related but
> worth recording: `npx tauri build` looked for the sidecar under a
> `teleportd-x86_64-pc-windows-msvc.exe` name even though this machine's active rustup
> toolchain is `stable-x86_64-pc-windows-gnu` (`rustup show`) -- tauri-cli's own
> target-triple detection doesn't follow the project's toolchain override. Worked
> around by also staging the sidecar under the msvc-suffixed name (same binary,
> `CreateProcessW` doesn't care what a GNU-toolchain-compiled exe is named); the
> underlying toolchain mismatch is a local-machine setup quirk, not a repo bug.
>
> The real installer immediately caught a real bug the raw-binary pass above could not
> have: the installed app failed to launch at all
> (exit `STATUS_DLL_NOT_FOUND` / `0xC0000135`). Root cause, confirmed by a controlled
> A/B (copying the missing file in, relaunching, removing it again): `WebView2Loader.dll`
> is a real runtime dependency of this project's GNU/MinGW-toolchain Windows build (it
> sits right next to `cargo build`'s own output) but was never in the NSIS file list, so
> a clean install had no copy of it anywhere. This is specific to the GNU toolchain
> path -- the far more common MSVC-toolchain Tauri build statically resolves this
> differently -- which is exactly why build-only CI (which never actually launches the
> installed artifact) never caught it. Fixed with a new
> `desktop/src-tauri/tauri.windows.conf.json` (Tauri auto-merges
> `tauri.<platform>.conf.json` per platform, so this is additive and Windows-only, zero
> risk to the Linux/macOS bundles) declaring `target/release/WebView2Loader.dll` as a
> bundle resource. Rebuilt, reinstalled, full gate table above re-run end to end against
> the fixed installer with no manual workaround: passed, and for the first time the
> real embedded web UI actually rendered (`GET /` served the built SPA, not the
> API-only 404 fallback) -- the first time this milestone's Windows testing has ever
> seen the real window content rather than a proxy for it.
>
> One more thing caught while installed, tracked rather than blind-fixed: the NSIS
> installer's default per-user install directory, `%LOCALAPPDATA%\Teleport`, and the
> daemon's own data directory, `%LOCALAPPDATA%\teleport`, are the same physical folder
> (NTFS is case-insensitive). Confirmed harmless today -- the generated uninstaller
> only `Delete`s three named files then a non-recursive `RMDir` (so it no-ops rather
> than touching `state.db`/`sessions/`/etc.), and its "delete app data" checkbox
> targets a different, unused `%LOCALAPPDATA%\<reverse-dns-id>` path -- but an upgrade
> install while a previous `teleportd.exe` is still running detached in that same
> folder was not exercised, and Windows generally refuses to overwrite a running
> executable. Left open in `desktop/README.md` rather than guessed at.
>
> Windows moves from "CI build only" to gate-verified against a real installer in the
> cross-OS ledger (#8) -- the only one of the three platforms tested against an actual
> installer artifact rather than a bundled `.app`/raw binary, since that's what
> surfaced the `WebView2Loader.dll` gap. Still not done, unchanged from before this
> run: validation item 2 (tray "Stop daemon" confirmation needs a human click),
> `autostart/windows.rs`'s Task Scheduler/registry autostart, and M9's `--bg` reboot
> persistence.

---

## M11 — CLI client

> Buildable in parallel with M10 — the protocol has needed nothing new since M9. This
> is the smallest possible native client per [13-native-clients.md](13-native-clients.md#the-protocol-is-already-native-ready):
> no WebView, no VT emulator to write or bundle. The user's own terminal *is* the VT
> emulator.

**Deliver:** `cli/` — a `teleport` binary, standalone crate (own `Cargo.toml`, own
target dir), the same pattern `desktop/src-tauri/` already established for a second
Rust component alongside `daemon/` — no new root Cargo workspace.

- `teleport sessions` / `teleport new [--preset|--cmd|--cwd]` / `teleport kill <id>` —
  thin wrappers over the existing HTTP surface (04)
- `teleport attach <id>` — the feature: puts the local terminal in raw mode and
  bridges it to the session's WebSocket 1:1, byte for byte
- offset-tracked reconnect, resize forwarding while controller, ssh-style `~`
  escape sequences (below) — no protocol change, no server-side work
- connection resolution: `--url`/`--token`/`TELEPORT_TOKEN`, else local-daemon
  auto-discovery via `<data_dir>/port` + `<data_dir>/token` (the same two files
  `desktop/src-tauri/src/daemon.rs` already reads for M10)
- added to the release pipeline (16): same target matrix as `teleportd`, `teleport`
  binary copied into the same per-OS archive

**Gate:** `teleport attach` to a session running `vim`/`tmux` over the same Tailscale
link M9 proved is, to the eye and to the keyboard, indistinguishable from `ssh`ing in
and running the program directly — full-screen redraw, resize, Ctrl-C, Ctrl-D, colors
all correct. `~.` detaches; a second `teleport attach` resumes with no gap. `SIGKILL`
on the CLI process mid-session leaves the remote session running, same invariant every
other client already has to honor.

> **Implementation spec drafted 2026-09-05.** Validated against current code and docs
> — no conflicts, and nothing to build server-side: the WS framing, control-lease,
> offset-replay, and origin/auth rules (04, 06) already assume a non-browser caller.
> [06-security.md#browser-origin-defense](06-security.md#browser-origin-defense)
> names the case explicitly — *"native app, CLI, script"* — a missing `Origin` header
> was always meant to cover this. `cli/` as a standalone crate, not a workspace
> member, matches the repository's existing shape rather than introducing one:
> `daemon/` and `desktop/src-tauri/` are already two independent Rust crates with
> their own `Cargo.toml` and CI job, never sharing a workspace or a `Cargo.lock`.
>
> **Scope.** In: `cli/` crate; `sessions`/`new`/`attach`/`kill` subcommands; raw-mode
> passthrough with offset-tracked reconnect; resize forwarding while controller;
> ssh-style escape sequences; local-daemon auto-discovery; `ci.yml`/`release.yml`
> additions mirroring `daemon`'s existing jobs.
> Out (deferred, not dropped): shell completions; a config file of session
> shortcuts/aliases; a non-interactive `teleport exec <id> <cmd>`-style scripting mode
> (genuinely useful, same shape as `ssh host cmd`, but its own gate — this milestone
> is interactive parity with the web client, not a new capability); `teleport logs
> <id>` (tail without attaching — trivial once `attach` exists, cut for the first
> pass).
>
> **Interfaces.**
> ```text
> cli/
>   Cargo.toml         # tokio, tokio-tungstenite (already a dev-dependency of
>                       # daemon/tests/ws_protocol.rs — same crate, now a runtime one
>                       # here), clap, serde, serde_json, crossterm, anyhow, thiserror,
>                       # tracing, directories, ulid. HTTP client left open below.
>   src/
>     main.rs            # clap subcommands, connection resolution, dispatch
>     connect.rs         # resolve base url + token: --url/--token/env, else
>                         # <data_dir>/port + <data_dir>/token
>     http.rs             # GET/POST/DELETE wrappers over the HTTP surface (04)
>     attach.rs           # the ssh-like loop: raw mode, WS read/write tasks, offset
>                          # tracking, resize forwarding, escape-sequence parsing,
>                          # reconnect/backoff
>     identity.rs          # persisted client_id (ULID) in <data_dir>/cli-identity,
>                           # mirrors web/src/lib/identity.ts's role
> ```
> `reqwest` vs. a hand-rolled client on top of `tokio-tungstenite`'s existing `http`/
> `hyper` transitive deps: decide at write time, and if `reqwest` is chosen, add it to
> [02-stack-decisions.md](02-stack-decisions.md#direct-dependencies)'s spirit — that
> doc's rule is "adding a crate is a decision, not a reflex," and that applies here
> too even though `cli/` has its own `Cargo.toml`.
> No new server-side code, no protocol version bump. `teleport` is a plain `/api/v1`
> client — nothing it does is unavailable to curl or the web UI.
>
> **Escape sequences**, typed at the start of a line (mirrors `ssh`'s `~` convention
> deliberately — anyone who knows `ssh` already knows this):
> - `~.` — detach. Restores the local terminal and exits the CLI; the remote session
>   is untouched, exactly like closing a browser tab.
> - `~!` — claim control (`claim_control`), for attaching mid-session as an observer
>   and wanting the lease.
> - `~?` — print the two lines above.
> - `~~` — a literal `~` at start of line, passed through.
>
> **Edge cases.**
> - Resize while observing (not controller): never send `resize` — only the
>   controller sets PTY geometry
>   ([04](04-api-protocol.md#control-messages)). Letterbox instead, the same call
>   the web client already made for its own observers (09).
> - WS drop mid-session: reconnect with `after=<last offset consumed>`, jittered
>   backoff, per [04](04-api-protocol.md#keepalive-and-reconnection). Never clear
>   local scrollback.
> - A server `exit` frame ends the CLI process with the remote process's exit code
>   (`std::process::exit(code)`) — the one place `teleport attach` behaves exactly
>   like `ssh host cmd` returning the remote's status. `~.` detach always exits `0`
>   regardless of remote state; detaching is not the remote work finishing.
> - Attaching to an unknown/purged session id: `404` — print the daemon's error, exit
>   non-zero, and do **not** enter raw mode at all, so there is nothing to restore.
> - Piped stdin/stdout (`teleport attach x | grep foo`): skip raw-mode setup when
>   stdin/stdout isn't a real tty (crossterm's `is_tty` check) and fall back to a
>   plain byte pipe — the same shape as `ssh host cmd | grep foo` already working.
> - Ctrl-C is never intercepted locally — raw mode passes it through as a byte to the
>   remote PTY, same as `ssh`. The only way to end the local CLI without ending the
>   remote work is `~.`; call this out in `--help` and the README, since it is the one
>   place this client's behavior isn't automatically obvious even to an `ssh` user.
> - `401`/`403` on connect: print the daemon's error body plus a one-line hint
>   ("check --token / TELEPORT_TOKEN"), exit non-zero. `teleport` never sends an
>   `Origin` header — the credential is the whole story for this client, per
>   [06](06-security.md#browser-origin-defense).
>
> **Validation.**
> 1. Loopback: `teleportd` + `teleport attach` on the same box, run `vim`, resize,
>    `:q`, confirm exit code and terminal restoration.
> 2. The same Tailscale setup M9 already proved: `teleport attach --url
>    https://<tailnet-host> --token …` from a second machine, drive a full-screen
>    program, detach with `~.`, reattach, confirm continuous replay.
> 3. Cross-client parity: open the same session in the web UI as observer while the
>    CLI holds control; both see identical bytes, and the web UI's controller label
>    shows the CLI's `client_name`.
> 4. `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`
>    clean for `cli/`; add `cli-fmt` / `cli` (3-OS matrix) / `cli-audit` jobs to
>    `ci.yml`, mirroring `daemon-fmt` / `daemon` / `daemon-audit` exactly.
> 5. `SIGKILL` the CLI process mid-session (not `~.`): remote session survives (the
>    same invariant M2/M9 already require of every client); a fresh `teleport attach`
>    resumes with no offset gap.
> 6. `release.yml`: `teleport` built and copied into the same per-target archive as
>    `teleportd`; `scripts/install.sh` still installs correctly with two binaries in
>    the archive instead of one.

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
├── thin optional Tauri package
└── CLI client (ssh-like attach)
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
