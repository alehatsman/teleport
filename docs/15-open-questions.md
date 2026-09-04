# 15 — Open questions and the M1 spike

Docs 01–14 describe a design. This file lists the places where that design is
**asserted but not yet proven**, and the decisions still genuinely open.

A doc stating a mechanism confidently is not evidence the mechanism works. Everything
below is a known gap, with the milestone it blocks and the thing that closes it.

**This file should shrink.** When an item is resolved, fold the answer into the doc it
belongs to and delete the entry here. An open-questions file that only grows is a
backlog pretending to be a spec.

## Status

| # | Question | Blocks | Closed by |
|---|---|---|---|
| [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows) | ConPTY children that exit gracefully are never reaped on Windows | **M1 — new, hard blocker** | **confirmed, root cause still open** — general to ConPTY (not `cmd.exe`-specific), spike, Windows (2026-09-04) |
| [W2](#w2--windows-fixture-parity-not-yet-attempted) | `daemon/tests/pty_primitive.rs` is Unix-shell-only; no Windows fixture suite exists yet | M1 | **open** — write a `cmd.exe`-based equivalent suite |
| [S1](#s1--who-reaps-the-child) | Who reaps the child, and what proves it exited? | M1 | **partial** — Linux closed; Windows blocked by [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows) |
| [S2](#s2--eof-is-not-exit) | Does EOF on the master mean the child exited? | M1 | **closed** — spike, Linux (2026-09-04); Windows blocked by [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows) |
| [S3](#s3--a-blocking-write-wedges-terminate) | Can a blocking PTY write wedge `terminate`? | M1 | **closed** — spike, Linux + Windows (2026-09-04) |
| [S4](#s4--does-dropping-the-master-close-the-pseudoconsole) | Does dropping the master close the pseudoconsole on Windows? | M1 | **partial** — Unix closed; Windows result confounded by [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows), see below |
| [D2](#d2--session-list-freshness) | How does the session list stay fresh? | M5 | decision |
| [D3](#d3--attention-signals-in-the-mvp-ui) | Are `bell` / `idle` surfaced in the MVP? | M8 | decision |
| [N1](#n1--keystroke-latency) | Keystroke latency over a relayed tailnet | — | M9 measurement |
| [N2](#n2--websocket-compression) | WebSocket compression | M4 | half-day investigation |
| [N3](#n3--xtermjs-write-pacing-on-reattach) | xterm.js write pacing on reattach | M5 | decision |
| [N4](#n4--reconnect-storms-and-reader-thread-contention) | Many simultaneous catch-ups reacquiring the reader thread's mutex | M4 | measurement + decision |

W1 and S1–S4 are **blocking**. The rest are decisions that must be *made* before
their milestone, not necessarily *built*. (D1 — replay sharing the live subscriber
budget — is **closed**: the fix is the catch-up loop in
[04-api-protocol.md](04-api-protocol.md#catch-up--register-late-not-early), built and
gated 2026-09-04.) **W1 is the one that matters most right now**:
until it's understood, no Windows result from S1/S2/S4 can be trusted, because all three
depend on observing a ConPTY child exit on its own.

---

# The M1 spike

> **Do not write `pty.rs` until S1–S4 are answered by running code on Linux, macOS and
> Windows.**

`03-pty-layer.md` is correctly identified as the highest-risk file in the product. It is
also the file whose claims are least verifiable by reading. Budget one day. The output is
four answers and a decision on the per-session thread count — not production code.

The spike exists because [Thread model](03-pty-layer.md#thread-model) specifies **two**
threads per session while the design has **three** blocking concerns: `read()`, `write()`,
and waiting on the child. That arithmetic does not work, and every question below is a
consequence of it.

## W1 — ConPTY children are never observed as exited on Windows

**Not one of the original four questions.** Found while running S1/S2/S4 for real on
Windows (2026-09-04, Windows 11 build 26200, ≥24H2, `portable-pty` 0.9.0, run from a
real interactive PowerShell session — not through WSL interop, ruled out separately
below). It supersedes and explains every Windows "TIMEOUT" result recorded under
S1/S2/S4: they are not four separate mysteries, they are one.

**Finding:** a child spawned under a ConPTY that exits **on its own** — `cmd.exe /c
"exit 0"`, `cmd.exe /c "exit 7"`, `cmd.exe /c "echo hi"`, and a quick-exiting parent
in the grandchild case — is **never** observed as exited by `child.wait()` or
`child.try_wait()`. Every one of these hung past a 10-20s test timeout. A child that
is **externally killed** (`taskkill /F`, matching what our own `terminate()` would
do) *is* observed correctly and near-instantly, every time, through the exact same
`wait()`/`try_wait()` call. So this is not a broken reaping mechanism in general —
only the graceful, self-initiated exit path never signals.

**Ruled out:**

- **Not the WSL bridge.** First seen when driving Windows through WSL process
  interop (`cmd.exe`/`powershell.exe` launching the spike `.exe` from inside WSL),
  which was already suspected as an unreliable bridge (no real console/window-station
  context). Re-ran identically from a genuinely interactive Windows Terminal /
  PowerShell session on the user's own machine — same result. This is real Windows
  behavior, not a bridge artifact.
- **Not `portable-pty` wiring the wrong handle.** `WinChild::wait()` is a plain
  `WaitForSingleObject`/`GetExitCodeProcess` on `proc: Mutex<OwnedHandle>`
  ([`src/win/mod.rs`](https://github.com/wezterm/wezterm/blob/main/pty/src/win/mod.rs)),
  and that handle is wired directly from `pi.hProcess` in `CreateProcessW`'s own
  `PROCESS_INFORMATION` in [`src/win/psuedocon.rs`](https://github.com/wezterm/wezterm/blob/main/pty/src/win/psuedocon.rs)
  — no indirection, no wrong handle. And the *exact same* `wait()` call correctly
  observes an externally-killed child, proving `WaitForSingleObject` on this handle
  does signal when the process is genuinely gone.
- **Not `cmd.exe`/this machine failing to reap in general.** Control test:
  spawned the identical `cmd.exe /c "exit 0"` with **no ConPTY at all**
  (`spike/src/bin/s0_control.rs`, plain `std::process::Command`) — `wait()` returned
  in 15ms with `exit_code=Some(0)`. ConPTY is specifically the variable.
- **Not a `CommandBuilder` argv-quoting bug.** Hypothesis was that `portable-pty`'s
  own Win32 command-line quoting (`CommandBuilder::cmdline()` /
  `append_quoted()` in `src/cmdbuilder.rs`) might produce a different literal command
  line than `s0_control.rs`'s plain `std::process::Command`, and that the resulting
  string might hit one of `cmd.exe`'s well-known `/c`-quote-stripping quirks. Read
  both: `append_quoted()` is the standard `ArgvQuote` Win32 algorithm (same one
  Rust's own `std::process::Command` uses) — `"exit 0"` contains a space, so both
  paths quote it identically to `cmd.exe /c "exit 0"`. Same literal command line,
  different outcome. Ruled out.

**Not yet found:** the actual root cause on the Windows/ConPTY side. A quick search
turned up a family of known ConPTY process-lifecycle issues (e.g. microsoft/terminal
[#4564](https://github.com/microsoft/terminal/issues/4564), "ConPTY host lingers
when all connected clients have been terminated," marked fixed for 22H2 — our build
is newer than that fix, so if it's the same class of issue, either the fix doesn't
cover this exact case or something else is at play) but nothing that's a confirmed
match for this exact symptom. Also checked `WinChild::is_complete()`/`wait()`
directly (`src/win/mod.rs`): plain `GetExitCodeProcess`/`WaitForSingleObject` on
`pi.hProcess`, textbook-correct, nothing ConPTY-specific in `portable-pty`'s own code
that could explain this.

**Confirmed general to ConPTY, not `cmd.exe`-specific (2026-09-04, same machine).**
`s5_minimal exit0`/`exit7` — spawning `mini_exit.exe`, a trivial Rust binary with no
shell and no console API calls beyond std's implicit runtime init, under ConPTY
instead of `cmd.exe` — **hung exactly the same way**: `wait()` never returned within
the timeout. `s5_minimal sigkill` (external `taskkill`) reaped correctly in 75ms,
same as every other externally-killed case. This rules out `cmd.exe`'s own
console-detach handling as the cause: **any process attached to a ConPTY, that exits
by simply returning from `main`/calling `ExitProcess`, is not observed as exited.**
Only forced termination (`TerminateProcess`, which bypasses whatever cooperative
shutdown path a graceful exit goes through) is observed. This is a property of
ConPTY / the Windows console subsystem itself, not of any particular child program —
narrows the search, but also means "swap the binary" is exhausted as an avenue.

**Resolved: the process is genuinely still resident, not a lost wakeup
(2026-09-04).** While `s5_minimal.exe exit0` was hung, `Get-Process -Name
mini_exit` from a separate PowerShell session showed it alive and idle:

```
Handles  NPM(K)    PM(K)      WS(K)     CPU(s)     Id  SI ProcessName
-------  ------    -----      -----     ------     --  -- -----------
     24       6      452       2596       0.00  21040   1 mini_exit
```

`0.00` CPU seconds — not spinning, not doing work, just alive. So this is not a
stale-handle/lost-signal bug in `WaitForSingleObject`; the OS itself has not torn
the process down. Its own `ExitProcess` call (from `std::process::exit(0)`, nothing
fancier) is genuinely blocked somewhere before the process is allowed to fully die
— almost certainly on some cooperative console-detach handshake with ConPTY's
internal host that the reachable APIs (`portable-pty`, or anything built on
`GetExitCodeProcess`/`WaitForSingleObject`) can't see into or unblock.

This also **rules out CPU-idle as a heuristic**: a process legitimately idle at a
shell prompt and a process stuck mid-`ExitProcess` look identical from the outside
(0.00 CPU, "Running" status, no output). Silence-based detection can't tell them
apart, so that's not a viable fallback signal on its own.

**Reader EOF checked too, and it doesn't arrive either.** Re-ran with the EOF
logging added above: `s5_minimal exit0`/`exit7` produced **no** `[s5] reader EOF`
line in a ~12s window (8s wait-timeout + 4s grace). So the master-side pipe doesn't
close/EOF either — not just the process handle. Consistent with the S4 finding
above (no EOF within 10s of dropping the master). This rules out reader-EOF as a
fallback exit signal too: whatever is stuck is holding the whole ConPTY session
open, not narrowly the process handle. There is currently no independent signal
available, at the `portable-pty`/Win32-console-API level, that a gracefully-exiting
child under ConPTY has actually finished.

**Where this leaves root-cause digging:** further progress needs tooling this spike
doesn't have budget for — WinDbg/ETW tracing of what `mini_exit.exe`'s threads are
actually blocked on during the hang. Time-boxed here rather than pursued further;
this was a one-day spike and is now well past that. Two remaining leads, in
decreasing priority:

- **Job Objects + an I/O completion port**, instead of `wait()`/`try_wait()` on the
  process handle. `JOB_OBJECT_MSG_EXIT_PROCESS`/`_ACTIVE_PROCESS_ZERO` notifications
  come from the kernel's job-object accounting, a different code path than
  `GetExitCodeProcess`/`WaitForSingleObject` — worth trying since it's independent
  of whatever ConPTY-side handshake is stuck, though if the process truly hasn't
  finished dying at the kernel level, this may not fire either. This is how several
  other tools (containerd, Docker) track Windows process lifecycle robustly, so
  there's precedent — but it's a real implementation, not a quick spike script, and
  a scope decision, not just a technical one.
- check whether a newer `portable-pty` (0.9.0 is what's pinned in
  [02-stack-decisions.md](02-stack-decisions.md)) or upstream wezterm `main` carries
  a workaround for exactly this — lower priority now that the symptom is confirmed
  general to ConPTY rather than a `portable-pty` wiring bug, but cheap to check.

**Engineering implication if root cause is never found:** the existing termination
policy ([03-pty-layer.md](03-pty-layer.md#termination)) already hard-kills after a
bounded wait regardless of whether the graceful signal worked — so *user-initiated*
terminate stays correct on Windows even with W1 unresolved, it just always takes the
full timeout-then-kill path rather than reaping early. What W1 actually breaks is
the case nobody is terminating: an agent process that finishes **on its own**. That
session would sit as `running` indefinitely with no forcing function, and — per the
finding above — there is no cheap heuristic (CPU, silence) to distinguish "finished
but stuck exiting" from "legitimately idle and still running." Closing this gap for
real needs either the root cause, or the Job Object path above; there is no cheap
heuristic-based shortcut available given what's been ruled out so far.
determines whether this is "OS hasn't reaped it" (probe-based fallback is sound) or
something stranger.

**Why this blocks M1 harder than S1-S4 did on their own:** if this holds up, the
*common* case — an agent process finishing normally — would never move a session to
`exited` on Windows at all. That's a direct hit on session-list accuracy, the
product's stated front page, and it's worse than anything the original four
questions anticipated: S1 assumed reaping just needed the right thread/mechanism;
this says the mechanism doesn't see the event at all, on this build, for this
spawn path. **Do not write the Windows leg of `pty.rs`'s reap/exit-status path
against the current design until W1 is understood.** The Unix leg (S1-S4, fully
closed on Linux) is unaffected and can proceed.

## W2 — Windows fixture parity not yet attempted

`daemon/tests/pty_primitive.rs` (docs/10-testing.md#1-pty-integration-fixtures-daemontestspty_rs)
is written entirely around Unix shell mechanics -- `/bin/sh -c` scripts, `stty`,
`setsid`/`trap` for the grandchild case, and `libc::kill`/`killpg` liveness probes for
the process-tree-kill assertion. None of that exists on Windows, so the file is
`#![cfg(unix)]`-gated: on a Windows checkout `cargo test` currently runs **zero** of
these fixtures, not "8 pass, 2 blocked on [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows)."

**Closed by:** a `cmd.exe`- or PowerShell-based equivalent fixture file, written when
someone can actually run and verify it on Windows (same constraint that shaped the M1
spike itself -- docs/11-mvp-plan.md's "proceed for Unix now" decision). Until then, treat
the M1 Gate as Linux-verified only; do not read `cfg(unix)` as "everything else already
passes elsewhere."

## S1 — Who reaps the child?

**Claimed:** [10-testing.md](10-testing.md#failure-injection-checklist) requires exit
codes to be recorded (`kill -9` an agent → "exit code recorded").

**Gap:** nothing in the thread model calls `wait()`. `portable_pty::Child::wait()`
blocks; `try_wait()` needs an owner and a cadence. Neither is specified. Today the exit
code has no path into the session row.

**Experiment**

- children that exit `0`, exit `7`, and are `SIGKILL`ed externally
- a child killed while producing output at full rate
- Windows: confirm the exit code is observable at all through a ConPTY child

**Pass criteria:** exit code observed within 100 ms of actual exit on all three
platforms, with no thread parked in `wait()` while a `write` or `terminate` is pending.

**Candidate answers — pick from the spike, do not pre-commit:**

| Option | Cost |
|---|---|
| Third dedicated thread per session, blocking `wait()` | one more thread per session; simplest and most obvious |
| `try_wait()` polled from the control thread on a 50–100 ms tick | no extra thread, but requires the control thread to never block — see [S3](#s3--a-blocking-write-wedges-terminate) |
| `SIGCHLD` handler | Unix-only; needs a Windows path regardless. Rejected unless the spike surprises us. |

**Spike result (2026-09-04):** ran `exit0`/`exit7`/external-`SIGKILL` under both
mechanisms, plus `try_wait()` polling at 75 ms, on Linux x86_64
(`portable-pty` 0.9.0). Both mechanisms observe the correct exit code every time;
`poll` latency tracks the tick (~75 ms), `blocking wait()` on a dedicated thread is
~0 ms. `SIGKILL`ed children report `exit_code=1` — `portable-pty` normalizes a
signal death to a plain failure code on Unix, it does **not** surface the signal
number. Don't build any UI or log line that expects to distinguish "exited 1" from
"killed by signal" through this API; if that distinction ever matters, it has to
come from a Unix-specific path around `portable-pty`, not through it.

**Decision (mechanism): dedicated blocking `wait()` thread**, one per session. It is
the simplest option, meets the <100 ms pass criterion with room to spare on Linux,
and — combined with [S3](#s3)'s separate writer thread — never sits behind a pending
`write`/`terminate`. Reject polling: it adds a tick-latency tradeoff for no benefit
once a dedicated thread is already paid for by S3. This sets the per-session thread
count to **four**: reader, writer, control (`resize`/`terminate`), reaper (`wait()`).
See [03-pty-layer.md#thread-model](03-pty-layer.md#thread-model), now updated.

**Windows (11, build 26200 / ≥24H2, cross-compiled `x86_64-pc-windows-gnu`, run from
a real interactive session): partial, and the incomplete half matters more than the
working half.** The externally-killed case (`taskkill /F` an unresponsive child)
reproduces cleanly — exit code observed at ~0 ms via both `poll` and `blocking
wait()`, same as Linux, confirming the *mechanism* (dedicated `wait()` thread) works
correctly when the OS actually signals the process as gone. But a child that exits
**on its own** — `exit0`, `exit7`, and the quick-exiting parent in `grandchild` — was
**never observed as exited at all**, every case timing out. This is
[W1](#w1--conpty-children-are-never-observed-as-exited-on-windows), not a gap in
this decision: the dedicated-thread mechanism is still the right one, it just has
nothing to observe on Windows for the graceful-exit path until W1 is understood.
**The per-session thread count and mechanism decision above stands for both
platforms; S1's Windows *exit detection* does not close until W1 does.**

## S2 — EOF is not exit

**Claimed:** the [Reader loop](03-pty-layer.md#reader-loop) breaks on `read() == 0`
("EOF: child exited or master closed") and the [State machine](03-pty-layer.md#state-machine)
reaches `EXITED` when the reader thread exits on EOF.

**Gap:** on Unix, EOF on the master means *every slave fd is closed* — not that the child
exited. The two diverge in both directions:

- a child that spawns a grandchild inheriting the PTY, then exits: the grandchild holds
  the slave open, **no EOF**, and the session sits in `running` with a dead child
- a child that closes its own descriptors and keeps running

This is the difference between "the agent finished" and "the session list is lying to
you" — and the session list is the product's front page.

**Experiment**

- `sh -c 'sleep 60 & exit 0'` — does `read()` return 0? when is the exit code available?
- Windows equivalent: a `cmd /c` that backgrounds a process and exits
- a child exiting mid-burst: confirm the output tail is fully drained before EOF is acted on

**Pass criteria:** `exited` and the exit code derive from the child wait, **never** from
EOF. EOF only stops the reader. The two events are handled correctly in either order, and
the state machine tolerates each arriving without the other.

This has a knock-on for the checklist: `10-testing.md` line "Kill an agent process
externally (`kill -9`) → reader sees EOF" encodes the assumption. Fix it when S2 closes.

**Spike result (2026-09-04, Linux x86_64):** confirmed the basic case (`echo hi;
exit 0`) and a 20000-line burst both show `wait()` and EOF landing together
(gap 0 ms) — no surprise there, nothing holds the pty open after the shell exits.

The grandchild case took real effort to even reproduce, and the effort *is* the
finding. Naive detachment does **not** survive session teardown on Linux:

- `nohup sleep 5 &` — dies with the parent. (Root cause turned out to be
  unrelated to nohup's own reliability; see below.)
- `setsid sh -c 'sleep 5' &` — also dies, immediately, even though `setsid`
  genuinely puts it in a new session and process group.
- Both reproduced identically through `portable-pty`'s own spawn **and** through
  the standalone `script` utility, ruling out a `portable-pty`-specific bug — this
  is Linux tty/session teardown behavior, not our code.
- What actually survives: `trap '' HUP; setsid sh -c 'trap "" HUP; sleep 5' &`.
  SIGHUP must be ignored **before** the fork races against the parent's exit — a
  plain `setsid` or `nohup` after the fact loses that race essentially every time
  in this environment. Once it survives, the master genuinely does not see EOF
  until the grandchild exits (verified: `wait()` returns at 0 ms, EOF arrives at
  ~5000 ms, matching the grandchild's sleep).

**Implication:** the doc's framing ("a child that spawns a grandchild inheriting
the PTY, then exits: the grandchild holds the slave open, no EOF") is real, but
rarer in practice than it reads — a background job needs a correctly-ordered
`setsid` + SIGHUP-ignore to survive at all, and most naive daemonizing attempts
(`nohup cmd &`, `cmd & disown`) will simply die with the shell instead of leaking
into this state. Both outcomes still require the same handling on our side: exit
code and state must come from `wait()` alone (confirmed never wrong across every
scenario tested), and the reader thread must tolerate EOF arriving long after
`wait()`, or never distinctly before it.

**Open corollary this spike surfaced, not yet answered by the docs:** when
`wait()` returns before EOF (the surviving-grandchild case), does the session
move to `exited` immediately using the child's exit code, while the reader
thread keeps draining in the background until its own EOF (which may now be
unbounded — the grandchild can live indefinitely)? Or does `exited` wait for
both? The state machine in [03](03-pty-layer.md#state-machine) is written for the
*user-initiated* `CLOSING → EXITED` path; it does not say what happens when the
child exits on its own while a descendant still holds the slave. Recommend:
`exited` fires on `wait()` alone (that's the whole point of S2 — never block
session-list accuracy on EOF), and the reader thread keeps appending to
`output.vt` in the background, independent of session state, until it gets EOF
or the session is GC'd. Needs a decision recorded in
[03-pty-layer.md#state-machine](03-pty-layer.md#state-machine) before M2's
`SessionManager` is built around it.

**Windows:** not run — every S2 scenario needs the parent to exit gracefully
(`basic`, `grandchild`, `midburst` all end with the shell exiting on its own), which
is exactly what [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows)
says never gets observed. Re-run once W1 has an answer.

## S3 — A blocking write wedges `terminate`

**Claimed:** [Thread model](03-pty-layer.md#thread-model) puts `write`, `resize` and
`terminate` on one control thread, on the grounds that commands "are short and
non-blocking except terminate, which is bounded."

**Gap:** writing to a PTY master **blocks** when the child is not reading stdin. The
kernel buffer is roughly 4–8 KiB on Linux. Meanwhile
[the fixture list](10-testing.md#1-pty-integration-fixtures-daemontestspty_rs) requires a
1 MiB write. While that write is blocked, `resize` and `terminate` cannot be serviced —
so `DELETE /api/v1/sessions/{id}` hangs and the bounded 5 s + 2 s wait in
[Concrete policy](03-pty-layer.md#concrete-policy) never even starts.

A session that cannot be killed because someone pasted into it is a shipping bug, and the
current thread model permits it.

**Experiment**

- a child that opens the tty and never reads it
- write 1 MiB; while blocked, issue `resize` and `terminate`
- measure time to `exited`

**Pass criteria:** terminate completes inside the documented bound regardless of a
pending write. Input that cannot be delivered is dropped or errored — never allowed to
hold the session open.

**Candidate answers:**

| Option | Cost |
|---|---|
| Separate writer thread; `terminate` stays on the control thread | one more thread; terminate is structurally never behind a write |
| Non-blocking master + bounded input queue, drop on overflow | gives input the same backpressure story as output; more code, and Unix/Windows diverge |

Either answer puts the per-session thread count at **three or four**, not two. That is the
honest price of the invariant. Pay it and update `03-pty-layer.md#thread-model`; do not
economize on threads and reintroduce the wedge.

**Spike result (2026-09-04, Linux x86_64):** confirmed the wedge directly, and it
took a correction to reproduce honestly. First attempt wrote 1 MiB to a
never-reading child's master and it "wrote" fine in 170 ms — because the pty's
default termios is cooked+echo, and echo drains an unread write regardless of
whether the child ever reads stdin. That's not the scenario S3 is about. Set the
pty to raw mode (`cfmakeraw` + `tcsetattr`) first — what a real remote-shell
session runs in anyway — and the write genuinely never returns on its own.

With that corrected: **shared queue** (write and terminate on one worker
processing commands in order, matching the current two-thread model) — terminate
did not complete inside a 10 s bound; it is queued strictly behind the write and
the write never finishes on its own. **Separate writer thread** (terminate issued
directly, bypassing the writer's queue) — terminate completes in ~0 ms regardless.

**Decision: separate dedicated writer thread**, per the first candidate answer.
`resize` and `terminate` stay together on the control thread (both genuinely
short/bounded); only `write` gets its own thread. Combined with [S1](#s1-who-reaps-the-child)'s
reaper thread, that's four threads per session: reader, writer, control
(`resize`/`terminate`), reaper (`wait()`).
[03-pty-layer.md#thread-model](03-pty-layer.md#thread-model) is updated to match.
Input that can't be delivered (a full/blocked write) is simply left blocked on its
own thread — never allowed to hold up `resize` or `terminate`; whether to also add
a bounded input queue with drop-on-overflow (the second candidate) is a
can-defer, not required to close S3.

**Windows confirmation (2026-09-04, build 26200, real interactive session):** both
modes ran to completion — unlike S1/S2/S4, this test's `terminate` is an external
`taskkill`, not a graceful exit, so it isn't touched by
[W1](#w1--conpty-children-are-never-observed-as-exited-on-windows). `separate`:
terminate in 73 ms regardless of the pending write. `shared`: write blocked for
757 ms, then terminate followed at 826 ms total — notably *not* an indefinite hang
the way the Linux raw-mode repro was. ConPTY's input buffering apparently doesn't
block as persistently as a raw Unix pty with nothing reading it; the wedge is still
structurally present (terminate is still queued behind the write and pays its full
duration), it's just naturally bounded here to under a second in this one test. That
difference is exactly why the fix should not lean on "the write will unblock soon
enough" — separate writer thread is the decision on both platforms, confirmed
directly on both. **S3 fully closed, Linux and Windows.**

## S4 — Does dropping the master close the pseudoconsole?

**Claimed:** [Termination](03-pty-layer.md#termination) step 2 gives the Windows graceful
stop as "`ClosePseudoConsole` via dropping the `portable-pty` master handle."

**Gap:** the reader thread holds a `try_clone_reader()` handle and the writer holds
`take_writer()`. Whether dropping the `MasterPty` closes the HPCON while those live is an
implementation detail of `portable-pty`, not a documented guarantee. If it does not,
graceful stop is a no-op on Windows and **every** close falls through to the hard kill —
which the platform checklist says loses the output tail.

Also unverified: the claim that `ClosePseudoConsole` returns immediately from build 26100
and can block on older builds. That claim is the entire reason the bounded wait sits on a
dedicated thread, and it is load-bearing for the M1 gate.

**Experiment**

- Windows 11 ≥ 24H2 **and** a build < 24H2
- close under heavy output load; measure how long the drop takes
- the grandchild case: a shell that started a background process — is the tree actually gone?

**Pass criteria:** a documented drop order that reliably closes the pseudoconsole on both
builds, with a measured upper bound on how long it can block. If dropping is unreliable,
call `ClosePseudoConsole` explicitly rather than relying on `Drop` ordering.

**Spike result — Unix, closed (2026-09-04, Linux x86_64):** the question doesn't have
a Windows-only answer; the Unix equivalent ("drop the master, which raises SIGHUP" —
[Termination](03-pty-layer.md#concrete-policy) step 2's stated fallback) has the
identical shape and was tested directly. With a reader clone and writer held exactly
as the real thread model holds them (mirroring production, not a synthetic
single-handle test), dropping *only* the `MasterPty` — while the reader/writer clones
stay alive — closed nothing: no EOF, no SIGHUP, no effect, for at least 10s. This is
expected once you look at it (the clones are `dup()`s of the same underlying fd, and
the kernel reference-counts the open file description, not `portable-pty`'s Rust
handle) but it directly contradicts treating "drop master" as a termination
mechanism, since the real architecture *always* has the reader thread holding a
clone for the session's entire life. **Recommendation: delete the Unix fallback
language in [03-pty-layer.md#concrete-policy](03-pty-layer.md#concrete-policy) step 2.**
Rely solely on the primary path already documented there — `killpg(pgid, SIGHUP)`
then `SIGTERM` — and only drop the master/reader/writer handles as post-reap
cleanup, never as the mechanism that makes termination happen. Done below.

**Windows — result obtained, but confounded by [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows), not usable as-is.**
First attempt drove Windows through WSL process interop (`cmd.exe`/`powershell.exe`
invoking the cross-compiled `.exe` from inside WSL) and every non-externally-killed
test hung — at the time this looked like a WSL-bridge limitation (no real console/
window-station context) rather than a ConPTY finding. Re-ran identically from a
genuine interactive Windows Terminal / PowerShell session on the user's own machine
(build 26200, ≥24H2) — **same hang**. That ruled out the bridge as the explanation
and led directly to [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows):
ConPTY children that exit gracefully are never observed as exited on this build,
full stop, regardless of how they're launched.

Concretely: `drop(master)` itself is fast (~0 ms, both `plain` and `grandchild`
scenarios) and no EOF arrived within the 10 s window either time — consistent with
the Unix finding (dropping master alone doesn't close anything while reader/writer
clones live). But this test's `child.wait()` at the end also never returns, which
given W1 doesn't prove anything about whether the pseudoconsole itself closed —
it's the same underlying symptom masking the result. **S4's Windows half stays
open until W1 is understood**; re-run this specific test once W1 has an answer or a
workaround, since `drop(master)` returning fast is the only part of this result not
already explained by W1.

---

# Decisions to make

## D2 — Session-list freshness

**Blocks M5.** Unspecified anywhere: how the UI learns that a session exited, or that a
new one appeared, without opening its stream.

**Recommendation: poll `GET /api/v1/sessions` every 3 s while the list view is visible,
and stop on `visibilitychange`.** Boring, explicit, and correct. An events WebSocket is a
v2 addition and should not be built now.

The point of recording the decision is that it not be discovered halfway through M5 and
answered by whatever is quickest that afternoon.

## D3 — Attention signals in the MVP UI

**Blocks M8.** M2 records `bell` and `idle` rows in `session_events`
([05](05-persistence.md#schema)). **Nothing in the MVP ever reads them.**

Meanwhile [13-native-clients.md](13-native-clients.md#push-notifications) names *"an agent
is waiting for you"* as the feature that justifies the product existing, and
[08-packaging.md](08-packaging.md) lists OS notifications as in-scope for the Tauri shell
— which cannot be built, because attention state is not on the API.

**Cost to close:** two fields on the session-list payload (`last_bell_ms`,
`idle_since_ms`), a badge in the session list, a tray notification in M10. Roughly two
hours.

**Recommendation: take it in M8.** It is the highest ratio of user-visible payoff to
lines of code anywhere in the plan, and it converts the M10 notification bullet from
aspiration into something implementable.

**If rejected, delete the `bell` and `idle` event types from the schema.** Recording
events that nothing reads is dead weight that will rot into a lie about what the daemon
detects.

---

# Networking quality

None of these is a defect in what is written. They are the difference between "the
reconnect is correct" and "the reconnect is impressive", and the docs currently address
only the first.

## N1 — Keystroke latency

**Not blocking. Measure at M9.**

Every keystroke costs one full round trip before it appears: the daemon owns the PTY, so
a character is not on screen until the child has processed it and the bytes have come
back. This is inherent to the architecture and is the correct trade — but it sets a
ceiling on how the product *feels* that no amount of offset correctness can raise.

A phone on cellular reaching a workstation behind CGNAT frequently cannot establish a
direct WireGuard path and falls back to a Tailscale DERP relay. 80–200 ms is ordinary. At
that RTT a terminal feels broken.

Mosh exists largely to solve exactly this, via speculative local echo. That is a real v2
option, and it touches the protocol — the client must predict and then reconcile against
authoritative bytes. **Do not foreclose it.** Keeping replay a server-side decision (as
[04](04-api-protocol.md) already requires for snapshots) is what keeps it possible.

**MVP action: measure, do not build.** At the M9 gate record:

- RTT from the phone on cellular to the daemon
- whether `tailscale status` reports a direct connection or DERP for that peer
- subjective typing feel at that RTT

Write the numbers into this section. They decide whether predictive echo is a v2 item or
something the MVP cannot ship without.

## N2 — WebSocket compression

**Investigate before M4 closes. Half a day.**

Terminal output compresses roughly 10:1. `permessage-deflate` is negotiated at the
handshake, requires **no protocol change**, and would cut cellular bytes and shorten every
replay. If the pinned `axum` / `tokio-tungstenite` versions support it at acceptable cost,
it is the cheapest networking win available, and it shortens every catch-up round on an
attach ([04](04-api-protocol.md#catch-up--register-late-not-early)) into the bargain.

**Do not compress above the WebSocket layer.** Compressing the payload ourselves would put
a codec between an offset and the bytes it indexes, which breaks the one invariant the
whole design rests on. Either the transport compresses, or nothing does.

## N3 — xterm.js write pacing on reattach

**Decide in M5.**

`default_tail` is 1 MiB ([07](07-remote-access.md)). Writing 1 MiB into `term.write()` in
one call stalls the render thread for seconds — at exactly the moment the product is
supposed to feel instantaneous. Bounded attach solved the *network* problem and left the
*rendering* one.

Two parts, both client-side:

- **the client chooses `tail`.** A phone should ask for ~256 KiB rather than inheriting a
  desktop default. The protocol already supports this; the web app just has to use it.
- **pace the replay write.** xterm.js exposes a `write(data, callback)` flow-control
  pattern; use it for replay so the first screen paints before the rest is fed in.

Add to the M5 gate: reattaching to a session with a large log paints the first screen in
under a second.

## N4 — Reconnect storms and reader-thread contention

**Measure before M4 closes.**

Catch-up ([04](04-api-protocol.md#catch-up--register-late-not-early)) decides whether to
register once per round, not once per attach — each round re-acquires the same fan-out
mutex the PTY reader thread locks on every `publish()`
([03](03-pty-layer.md#reader-loop)). Per client, that is now bounded: a stalled client is
cut off after four consecutive non-shrinking rounds, and a client that always gains a
little ground but never quite converges is cut off after a fixed total-round ceiling
either way
([Convergence](04-api-protocol.md#catch-up--register-late-not-early)) — so one client can
only reacquire the lock so many times before the daemon stops trying and hands it the live
stream with a hole.

What that does **not** bound: many clients catching up *at once* — the shape of a
reconnect storm after a network blip drops every attached client on a multi-session host
in the same second. Each round's own hold is short (arithmetic and a length check, no
I/O), but the aggregate rate of acquisitions scales with concurrent catch-ups, and the
reader thread competes for the same lock on every chunk it appends. Unmeasured: whether
that shows up as observable jitter on the live path under realistic concurrency.

This is a traded-off cost, not an oversight — the alternative most implementations ship
(register once, up front) is exactly what D1 closed as the far worse failure: livelock,
not jitter (04-api-protocol.md#catch-up--register-late-not-early). The question is only
whether the trade needs a second mitigation.

**Before M4 closes:** load-test a synthetic reconnect storm (N simultaneous attaches to
one busy session) and measure reader-thread latency during it. If it stays acceptable,
record the number and close this. If not, the fix is bounding *concurrent* catch-ups per
session (an admission limit or a queue), not reworking the per-round design N4 measures.

---

# What is deliberately *not* here

These were reviewed and found sound; they are recorded so they are not re-litigated.

- **The offset model.** Byte offsets as the only cursor, persist-before-advance,
  advance-before-fan-out, one mutex over `{next_offset, subscribers}`. Correct and
  internally consistent across docs 03, 04 and 05.
- **The control lease.** Attach-never-preempts plus a grace window is the right shape;
  it is the fix for a problem most implementations ship with.
- **The crash boundary.** Documented honestly rather than papered over, with the
  second-stage design sketched and correctly deferred.
- **Milestone ordering.** PTY first, UI last, Tauri after the product works. Right, and
  the discipline to keep it is worth more than any item above.

One caveat with no action attached: **the MVP's networking wow-factor is capped by
Tailscale onboarding.** "Install Tailscale on two devices, create an account, run
`tailscale serve --bg`, copy a token URL" is a competent fifteen-minute setup, not a magic
moment. The staging in [12](12-identity-and-connectivity.md) is right and the relay is
correctly deferred — but the magic lives in stage 3, and the MVP should be judged on
correctness, not on delight it cannot yet deliver.
