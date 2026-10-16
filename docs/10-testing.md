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

### `target_os` gates are allowlisted

`cfg(unix)` and `cfg(windows)` are the portable idiom — "this fixture drives `/bin/sh`",
"this one drives ConPTY" — and are unrestricted. A **`target_os` gate is different**: it
takes one first-class platform out of a test's coverage, usually with a reason that reads
plausibly at the time.

Twice that reason has been wrong:

- Three fixtures were gated off macOS "pending measurement on real hardware"
  ([#25](https://github.com/alehatsman/teleport/issues/25)). The measurement found one
  wrong queue bound — wrong on Linux too, merely invisible there — fixed in
  [#32](https://github.com/alehatsman/teleport/pull/32), all three un-gated.
- `reconnect_storm.rs` then shipped Linux-only citing those three as precedent, four
  commits after that precedent was overturned. It passed 20/20 on macOS the first time
  anyone ran it ([#36](https://github.com/alehatsman/teleport/pull/36)).

Note what would not have caught either: a check demanding a rationale or an issue link
near the gate. Both had a rationale; both cited real documents. What was missing was a
deliberate decision at the moment of gating, visible to a reviewer.

So every `target_os` gate under `daemon/tests/` must be listed in
[`scripts/allowed-target-os-gates.tsv`](../scripts/allowed-target-os-gates.tsv), with the
issue that tracks removing it. `scripts/check-target-os-gates.sh` (CI job
`daemon (target_os gates)`) diffs the tree against that file **in both directions** —
adding a gate fails until it is listed, and un-gating fails until its row is deleted, so
an un-gating cannot quietly leave the old reason on the books.

Adding a row is not forbidden, just deliberate. If you have not run the test on the
platform you are gating it off, that is the cheaper next step: both entries above were
retired by someone finally running them, and one of them found a real product bug.

Product code under `daemon/src/` may branch on `target_os` freely — that is platform
logic, not absent coverage.

## Layers

### 1. PTY integration fixtures (`daemon/tests/pty_*.rs`)

Prove the primitive before building on it. Use a small deterministic child rather than
a real shell where possible — `cargo run --bin echo-fixture` style, or `printf`/`cmd /c`
equivalents chosen per platform.

- spawn a shell, write `echo hello\n`, read `hello` back
- write large input (1 MiB) without loss or reordering. **Windows has no equivalent**:
  ConPTY's input direction always passes through conhost's own VT parser, with no raw
  mode that bypasses it (unlike a Unix pty under `stty raw`), so an arbitrary byte-exact
  payload including control bytes isn't a meaningful test there — see
  [W2](15-open-questions.md#w2--windows-fixture-parity-not-yet-attempted). The Windows
  fixture file instead proves large *text* volume (`large_text_burst_read_drops_nothing`).
- read a large burst (100 MiB from `yes`) without dropping bytes or stalling — Windows:
  `for /L` stands in for `yes` (no infinite-repeat builtin reachable in one `/c` script);
  strip conhost's own VT init/teardown escape sequences before comparing, don't fight
  them with sentinel lines (see W2)
- resize and confirm the child observes it (`stty size` on Unix, `mode con` on Windows)
- child exits normally → exit code `0` recorded. The code comes from the child wait,
  **not** from EOF on the master — the two are separate signals and can arrive in
  either order ([S2](15-open-questions.md#s2--eof-is-not-exit)). *How far* apart they
  can land is OS-specific, and the fixture is split accordingly: Linux exempts ptys from
  the session-leader vhangup, so EOF can lag exit indefinitely, while macOS/XNU revokes
  the controlling tty outright and EOF coincides with exit
  ([S5](15-open-questions.md#s5--a-detached-grandchild-cannot-hold-a-pty-past-its-parent-on-macos)).
  Windows coverage:
  `pty_primitive_windows.rs`, proven against the real fix for
  [W1](15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows).
- child exits nonzero → exit code recorded accurately. Same Windows coverage as above
- terminate a running child → state machine reaches `exited` within the bounded policy.
  Windows: usually near-instant (`ClosePseudoConsole` returns immediately on this
  build), but caught once under concurrent load falling through to the full bound
  instead — assert the documented bound, not the usual fast path
- terminate a child that is producing output at full rate → no deadlock, no lost tail
- close a session whose shell has spawned grandchildren → the tree is gone (Windows:
  attached character-mode clients terminate with the pseudoconsole — confirmed via
  `start /B` launching an attached grandchild and a real `terminate()` call, see
  [S4](15-open-questions.md#s4--does-dropping-the-master-close-the-pseudoconsole))

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
- `POST /api/v1/shutdown` with no token/bad Origin → rejected, and never wakes
  `shutdown_signal()`; with a valid credential → `202` and does wake it
  (`daemon/tests/shutdown_endpoint.rs`). The same file also spawns the *real*
  `teleportd` binary and drives it over a raw `TcpStream` the way a curl-based
  Windows caller would, then asserts the process actually exits and its port file
  is removed — the Windows-daemon-stop gap (issue #12,
  [11-mvp-plan.md#m10](11-mvp-plan.md#m10-tauri-shell)) closed and run for real,
  not just wired up

### 4. Persistence / restart tests

- restart with a session marked `running` → becomes `lost` / `daemon_restart`
- `output_bytes` in the DB is stale on restart → **file length wins**
- log survives restart and `/log` still serves the full range
- GC deletes the directory before the row; a crash mid-GC leaves a row without a log,
  not a log without a row
- log cap reached → appending stops, `log_capped_at` recorded, live streaming continues

## Deadlines, preconditions and premises

Three rules, each of which cost a day to learn.

**A precondition waits on the condition, not on a clock.** A fixture that
needs 12 MiB of backlog before it can begin is not testing how fast this
machine produces 12 MiB. Spend patience on *stalling*: any progress resets
the budget, only an unbroken streak of no movement fails
(`support::await_offset`). The fixed-deadline version failed four fixtures at
once on a loaded box (#68, #102, #103, #104) while every byte was still
arriving, in order, undamaged.

**An assertion measures the thing it names.** `slow_subscriber_is_...` asserted
"10 MiB drained in under 10s" under a name about the reader never being
blocked. The per-chunk stall timeout already measures that; the total was
measuring the machine. Slow is not blocked, and slow is not wedged.

**Silence is evidence only if there was something to hear.** The corollary,
and the one that closed `reconnect_storm`. Its control subscriber is read
against a per-`recv()` stall bound — the right instrument for "the storm never
blocks the reader", except that the thing on the other end is a shell loop
(`printf; sleep 0.01`). Under 16 spinners that loop delivered **1–4 chunks per
2s window against a nominal 200**: gaps of seconds where idle gives 10ms. The
bound was failing the reader for the producer's descheduling. A quiet `recv()`
now reads `session.next_offset()` across the quiet period and fails only if
the session produced bytes the subscriber did not get — which is the sentence
the test's name makes. Before asserting on an absence, check that the thing
you are measuring was running.

**A timeout that duplicates a non-clock bound is pure flake.** `reconnect_storm`
joined its 40 storm clients under a 30s timeout, to catch a client that never
converges. `support::catch_up_allow_clamp` already fails that client at
`MAX_TEST_ROUNDS` — a count of rounds, not of seconds. The timeout added no
bound; it added a 30s budget on 480 MiB of concurrent replay. When a fixture
already has a bound in the right units, a wall clock beside it is not a
backstop, it is a second failure mode.

**Three kinds of fixed number, and only one of them expires.** Audit a new
one by asking which it is:

| Kind | Example | Expires? |
|---|---|---|
| Condition wait — poll with a bound, fail only if the event never happens | "the bell was never noticed", 15s | No. A slow box arrives later; it does not fail. |
| Policy-derived bound — asserts the daemon's own documented limit | `terminate()` under 9s against a 7s `GRACEFUL_WAIT + KILL_WAIT` | No. It measures a policy, not throughput. |
| Volume budget — covers a transfer whose duration scales with bytes | "12 MiB of backlog within 30s" | **Yes.** It is sized against today's hardware, and nothing writes the expiry date on the fixture. |

Every volume budget in this suite has been converted to the progress rule
above. If you add one, you are adding a fixture that will go red on someone's
laptop for no reason — `reconnect_storm` took **11.5s on a fully idle machine
against a 10s precondition budget**, which is how close one of these had
already drifted before anybody looked.

## A claim nothing can falsify is not evidence

Two things in this repo read as evidence while being unable to fail. Both bit
on 2026-10-16, within hours of each other, and they are the same defect wearing
different clothes.

**A test that cannot fail by not running.** The `ANCHORD_BIN`-gated suites
early-return when the variable is unset, and a test that returns is a test that
*passed*: `cargo test --test anchor_http` prints `ok. 17 passed` in 0.01s having
executed nothing — including the test whose name asserts that sessions survive a
restart. libtest swallows the `eprintln!("skipping")` on a pass, so not even the
word reaches the log. Counting executed tests cannot catch this, because the
count is never zero. Fixed by one *ungated* assertion per binary that goes red
when `CI` is set without a binary (#108, `5db5fbd`) — the fix had to be something
that could fail on its own.

**A comment that cannot fail by being out of date.** `docs/03-pty-layer.md`
carried "Unresolved blocker (2026-09-04)" for six weeks after W1 was resolved,
and `pty.rs`'s Windows terminate arm said "Unverified on real Windows" long after
S4 closed. Both were read as live risk by a second repository, which deferred a
platform port on the strength of them. Code that rots breaks a build; prose that
rots keeps persuading.

Three cheap habits, the third from the trap described at the top of
`15-open-questions.md`:

- **Date the status.** "Unverified (2026-09-04)" invites a check; "Unverified"
  invites belief. A status without a date has no expiry and so never gets one.
- **A citation becomes yours when you restate it as fact.** Both stale notes
  crossed into another repo and were built on. Verify at the point of repeating,
  not at the point of writing.
- **Prefer a reference that resolves to nothing over one that resolves to the
  wrong thing.** A dangling link makes a reader keep looking; a plausible wrong
  one makes them stop.

The general form, and the reason this section is in the testing doc rather than
a style guide: **assertions that can only succeed are not assertions.** When
reviewing a status claim — in a fixture, a comment, or a doc — ask what would
have to happen for it to go red. If the answer is "a person notices", it is
decoration. Intending to re-read is exactly what failed here; prefer a check that
can fail on its own.

**Publish the pair, not the green.** A single passing number hides whether a
fixture is at 60% of its budget or 115% of it, and those are the same colour.
When a timing matters, record what it took and what it was allowed:

| | loaded | quiet | factor |
|---|---|---|---|
| `session_catchup` | 80.9s | 16.7s | 4.8x |
| `reconnect_storm` | 44.3s | 11.5s | 3.9x |
| `session_backpressure` | 33.7s | 6.2s | 5.4x |
| `pty_primitive` | 29.9s | 5.7s | 5.2x |
| `anchor_http` | 16.3s | 3.1s | 5.2x |
| `http_api`, `ws_protocol`, `osc_*`, `output_log`, … | sub-second | sub-second | ~1x |

(Measured 2026-10-16. "Loaded" was 60 orphaned CPU spinners; "quiet" is the
same machine with them gone. A 5x spread between the two is the whole reason
a wall-clock budget is not an assertion.)

**The factor column is a tool, not a footnote.** Contention inflates exactly
the fixtures that move volume and essentially nothing else — the sub-second
suites barely move. So running a suite you do not know under deliberate load
and sorting by inflation factor finds its volume budgets faster than reading
it does. `anchor_http` is the proof: it was the one hidden volume budget in
this suite, found by reading, and it inflated 5.2x alongside the four that
were already failing. Use it as a second opinion on the taxonomy, never as a
replacement — inflation tells you a fixture *depends* on throughput, not
whether its budget is wrong. (Anchor's suggestion, and their suite's four
heaviest fixtures are these same four by name, independently arrived at: the
shape belongs to testing a PTY multiplexer, not to either implementation.)

If you do generate load, collect the PIDs explicitly and verify they died.
`jobs -p` in a non-interactive shell has no job table, so the obvious
`kill $HOGS` kills nothing and leaks every spinner — which is how the 60
above came to exist.

**Where the progress counter lives is forced, not chosen.** The predicate
above — any progress resets, only an unbroken stall fails — is portable. Its
implementation is not, and a rule that fixed the implementation would break
one of the two suites that learned it.

`Session::write` enqueues onto a *bounded* channel, so once the writer thread
is stuck in `write_all` the caller blocks instead of getting an error. A
wedged writer therefore cannot report anything about itself, and the streak
has to be sampled from outside — which is why
`terminate_is_not_wedged_by_a_stuck_write` also needs the writer to publish
"I returned": from outside, a writer that gave up and a writer that wedged
look identical, and without that flag the fixed test walks straight back to
vacuous. Anchor's equivalent returns an error on a full channel, so their
writer thread is the only thing that *can* observe its own streak and an
external watcher would be strictly worse — no flag needed, because "returned"
and "observed a wedge" are the same event there.

Same predicate, two correct implementations, decided by where the blocking
lives. Carry the predicate; derive the rest locally.

**A premise has to be reachable, and green is not evidence that it was.**
`terminate_is_not_wedged_by_a_stuck_write` passed from the day it was written
without ever wedging anything: a tty in canonical mode *discards* an
over-long line that never terminates rather than queueing it, so 4 KiB blobs
of `x` went nowhere and `terminate()` raced nothing. Complete lines wedge the
writer after ~9 KiB. Anchor found this class in their copy first (anchor#9);
teleport's copy had it too. When a fixture depends on a resource filling up,
make it print where it filled — a premise that quietly stops being reachable
looks exactly like a passing test.

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
| `terminate` while a write is blocked on an unreading child | terminate completes inside the bounded policy; input is dropped, never allowed to hold the session open ([S3](15-open-questions.md#s3--a-blocking-write-wedges-terminate)). The write must be **complete lines**, or the tty discards it and nothing blocks — see above |
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

## Web e2e (Playwright)

`web/e2e/` — real Svelte components, a real browser (Playwright), a real `teleportd`.
Never a mock WebSocket server: this repo already treats "prove it against the real
primitive" as the point of testing (this file's opening line), and the frontend has no
exception for that.

**Scope is deliberately narrow**: the real, user-facing critical paths and the one
invariant the product exists to protect — exactly one controller, truthfully shown
(docs/09-frontend.md#control-lease-ui) — not a browser-level re-run of this file's
failure-injection matrix above. That stays Rust's job; duplicating it through a browser
would be slower and no more convincing. Five spec files: session lifecycle (launch,
attach, typed input round-trips), control lease (claim preempts, the ousted client is
notified), the session list (search, closed-tab bulk delete), a reconnect/restart
regression test, and a mobile-viewport smoke pass — see each file's own header comment
for what it covers and why.

**The daemon fixture** (`web/e2e/fixtures/daemon.ts`) spawns a real `teleportd` against
an ephemeral port and a throwaway data dir, waits on the same signals a human would (the
`<data_dir>/port` file, then `/health` answering — never a fixed sleep), and reuses
`TELEPORTD_BIN` when set (CI: a pre-built binary) or falls back to `cargo run
--manifest-path daemon/Cargo.toml --` (local: `npm run test:e2e` alone just works,
`tasks/ui-e2e.yml` builds the binary first for speed). `--manifest-path` and not `-p
teleportd`, because there is no workspace root to select a member of. Auth stays **on**
— the real default ([06-security.md](06-security.md#authentication)) — a custom
`authedPage` fixture does
the one real `?token=…` navigation every test, the actual onboarding flow, rather than
routing around it with `auth_token = false`.

Most specs share one daemon instance (`global-setup.ts`/`global-teardown.ts`), safe
because every test creates whatever session(s) it needs and scopes its own assertions to
a unique marker — never the raw Active/Closed counts, which reflect every session on the
shared daemon and would be flaky under parallel test files.
`reconnect-and-lost.spec.ts` is the one exception: it owns a **dedicated** instance,
because it deliberately kills the daemon and restarts it against the same data dir and
port (to exercise the real "recovered as lost" path, [01](01-architecture.md#the-crash-boundary))
— that must never disturb whatever else is running in parallel against the shared one.

Run it: `cd web && npm run build && npm run test:e2e` (needs `npx playwright install
chromium webkit` once; the build is what the daemon serves as `--web-dist`, and the
fixture fails fast if it is missing), or `provision apply tasks/ui-e2e.yml` for the
whole thing in one command. CI runs it as its own `web-e2e` job ([.github/workflows/ci.yml](../.github/workflows/ci.yml)), separate
from the fast `web` job (lint/typecheck/build/vitest) so a typo doesn't wait on a Rust
build and two browser downloads.

The "mobile" Playwright project uses `devices['iPhone 14']`, which is **WebKit** — the
real engine an iPhone runs, not Chromium's mobile-viewport emulation
(`scripts/mobile-dev/README.md`'s own "Fidelity" table makes the same distinction for
manual testing). This app is mobile-first and two of this session's three real bugs were
WebKit-specific; emulated Chromium would not have caught either.

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
