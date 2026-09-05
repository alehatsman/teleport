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
| [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows) | ConPTY children that exit gracefully are never reaped on Windows | M1 | **resolved, same day (2026-09-05)** — root cause: conhost's ConPTY startup handshake sends a DSR/cursor-position query (`ESC[6n`) and blocks its `ConsoleIoThread` on the reply, which nothing ever sent; confirmed by spike (`s9_dsr_reply.rs`, wait() returns in 6-8ms once answered) and fixed in production (`daemon/src/pty.rs`'s `ConptyDsrProbe`), proven via `daemon/tests/pty_primitive_windows.rs` against the real `pty::spawn` path (63 passed, 0 failed); tracked in [#9](https://github.com/alehatsman/teleport/issues/9) |
| [W2](#w2--windows-fixture-parity-not-yet-attempted) | `daemon/tests/pty_primitive.rs` is Unix-shell-only; no Windows fixture suite exists yet | M1 | **closed, 2026-09-05** — `pty_primitive_windows.rs` now covers echo/write, resize, both exit-code fixtures, and all three terminate fixtures (bounded policy, under load, grandchild-tree-kill), written and run for real on this machine; one Unix fixture (byte-exact raw-mode write) has no Windows equivalent at all and is documented as such, not skipped silently; tracked in [#10](https://github.com/alehatsman/teleport/issues/10) |
| [W3](#w3--pty_primitive_windowsrss-own-tests-were-oversubscribing-the-ci-runner) | `pty_primitive_windows.rs`'s own 8 tests ran concurrently, and one precondition's timeout didn't match this runner's real ConPTY throughput | M1 | **closed, 2026-09-05** — two-round fix: serializing the file's tests (contention, real but partial: 161432→193109/262144 bytes) plus giving one precondition the same class of budget its own sibling test already uses for a comparable workload (10s→30s); tracked in [#27](https://github.com/alehatsman/teleport/issues/27) |
| [S1](#s1--who-reaps-the-child) | Who reaps the child, and what proves it exited? | M1 | **closed** — Linux closed 2026-09-04; Windows unblocked by [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows)'s fix, confirmed via `pty_primitive_windows.rs` |
| [S2](#s2--eof-is-not-exit) | Does EOF on the master mean the child exited? | M1 | **closed** — spike, Linux (2026-09-04); Windows unblocked by [W1](#w1--conpty-children-are-never-observed-as-exited-on-windows)'s fix |
| [S3](#s3--a-blocking-write-wedges-terminate) | Can a blocking PTY write wedge `terminate`? | M1 | **closed** — spike, Linux + Windows (2026-09-04) |
| [S4](#s4--does-dropping-the-master-close-the-pseudoconsole) | Does dropping the master close the pseudoconsole on Windows? | M1 | **closed, 2026-09-05** — Unix closed 2026-09-04; Windows re-run against the real fix via W2's `terminate_kills_the_grandchild_process_tree` (`pty_primitive_windows.rs`): dropping the master takes an attached grandchild down too, in ~0.5s, through the real `terminate()` path |
| [S5](#s5--a-detached-grandchild-cannot-hold-a-pty-past-its-parent-on-macos) | A detached grandchild's survival on macOS | M1 | **closed** — root-caused on real hardware 2026-09-05: XNU revokes the ctty on session-leader exit; not a flake, a kernel difference. Fixture un-gated with a macOS twin; [#11](https://github.com/alehatsman/teleport/issues/11) |
| [D2](#d2--session-list-freshness) | How does the session list stay fresh? | M5 | decision |
| [N1](#n1--keystroke-latency) | Keystroke latency over a relayed tailnet | — | **partial** — direct-path case measured 2026-09-05 (~36ms, instant feel); relayed/DERP case still needs a sample |
| [N2](#n2--websocket-compression) | WebSocket compression | M4 | **closed, 2026-09-05** — investigated by reading the pinned `axum`/`tungstenite` source directly: neither implements permessage-deflate or any extension negotiation at all (a literal `// TODO` in tungstenite's own handshake code), so this was never a config flag away; not implemented, recorded as infeasible-with-this-stack rather than done |
| [N3](#n3--xtermjs-write-pacing-on-reattach) | xterm.js write pacing on reattach | M5 | decision |
| [N4](#n4--reconnect-storms-and-reader-thread-contention) | Many simultaneous catch-ups reacquiring the reader thread's mutex | M4 | **closed, 2026-09-05** — measured on real CI: a 40-client concurrent reconnect storm cost a live subscriber a 0.995 throughput ratio (no measurable degradation); the per-round design needs no second mitigation |
| [N5](#n5--macos-pty-reads-average-14-bytes-starving-the-queue-bounds-count-half) | The `min(256 chunks, 8 MiB)` queue bound was ~2300x tighter on macOS than on Linux, because macOS pty reads are tiny | M4 | **resolved 2026-09-05** ([#25](https://github.com/alehatsman/teleport/issues/25)): one byte budget that charges per-chunk overhead; all three fixtures un-gated |
| [P1](#p1--the-native-bearer-ws-path-has-never-been-driven-by-a-real-native-client) | Has a real native (non-browser) client ever driven the bearer-on-WS auth path? | iOS Phase 1 spike (docs/13) | **open** — planned, needs a Mac |

No S/W question is blocking any more: S5 — the last one — is **closed** (2026-09-05,
root-caused on real macOS hardware rather than guessed at from CI), joining W1, W2 and
S4, all of which resolved the same day they were found blocking. The rest are decisions that must be *made* before their milestone, not
necessarily *built*. (D1 — replay sharing the live subscriber budget — is **closed**: the
fix is the catch-up loop in [04-api-protocol.md](04-api-protocol.md#catch-up--register-late-not-early),
built and gated 2026-09-04.) **W1 was the one that mattered most, and is now resolved**
(2026-09-05, same day it was found): root cause identified, fixed in production, proven
via a real integration test against `pty::spawn`, not just a spike. S4's Windows leg is
now re-run and confirmed against the real fix (2026-09-05, via W2's
`terminate_kills_the_grandchild_process_tree`); W2's own fixture-parity gap is closed the
same day, same machine.

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

**Tracked in [alehatsman/teleport#9](https://github.com/alehatsman/teleport/issues/9).**

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

**Where this leaves root-cause digging (as of the original spike):** further progress
needs tooling this spike didn't have budget for — WinDbg/ETW tracing of what
`mini_exit.exe`'s threads are actually blocked on during the hang. Time-boxed at the
time rather than pursued further. Two leads were left, in decreasing priority — both
now checked, see below.

**Re-verified independently, 2026-09-05, on different hardware (`DESKTOP-R8O9R54`,
same build 26200), with a fully native toolchain — no cross-compile from WSL anywhere
in the chain this time** (`rustup`'s `stable-x86_64-pc-windows-gnu` host toolchain
installed and run directly on this Windows machine, `cargo build`/`cargo run` from a
native PowerShell process, not `x86_64-pc-windows-gnu` cross-compiled from Linux). The
full spike suite reproduced W1 exactly: S1/S2/S4/S5's graceful-exit cases all hung to
their timeouts identically, S1/S3/S5's externally-killed cases all still reaped
correctly and fast (0–75ms). This matters because the original run, despite being
re-tried from "a genuine interactive Windows Terminal / PowerShell session," was still
built from a cross-compiled binary — a fully independent native build removes one more
variable. **W1 is not a cross-compilation or WSL-adjacent artifact of any kind.**

**Lead 1 (Job Objects + IOCP) — tried, ruled out (2026-09-05).** New spike,
`spike/src/bin/s6_job_object.rs`: assigns the ConPTY child to a Job Object wired to an
I/O completion port (`JobObjectAssociateCompletionPortInformation`), racing
`JOB_OBJECT_MSG_EXIT_PROCESS` against the same `wait()` and reader-EOF signals as S5.
**Result: no better than `wait()`.** `exit0`/`exit7` — no `EXIT_PROCESS` (or any other)
notification arrived within a 13s window; the job object never hears anything past the
`NEW_PROCESS` message it gets at spawn time. `sigkill` — works correctly and fast,
`EXIT_PROCESS` observed at 265ms, `wait()` at 75ms on the same run: the kill case was
never the question. This is a clean, symmetric negative: the externally-killed case
succeeds identically on *both* signals, the graceful-exit case fails identically on
*both* signals, which is strong evidence the two failures share the same root cause —
whatever is stuck blocks *all* kernel-level exit accounting for the process, not just
the specific `GetExitCodeProcess`/`WaitForSingleObject` path. Job Objects were the
"independent code path" hope; they turned out not to be independent enough. **Do not
pursue Job Objects/IOCP as a W1 workaround** — the code is left in the spike crate as a
documented negative result, not a starting point.

**Lead 2 (a newer `portable-pty`/wezterm fix) — checked, nothing found (2026-09-05).**
Reviewed wezterm's `pty/src/win/mod.rs` commit history on `main` and open/closed issues
for anything matching this symptom. The one recent (2026-06-07) Windows-PTY-related
change, [wezterm#7709](https://github.com/wezterm/wezterm/pull/7709), fixes `kill()`
misreading `TerminateProcess`'s nonzero-on-success return value — unrelated to exit
*detection*, and to a code path (`kill()`) this project doesn't even call the same way
(see [S3](#s3--a-blocking-write-wedges-terminate)'s external-`taskkill` approach). No
issue or commit found describing this exact symptom (a gracefully-exiting ConPTY child
never observed via `wait()`, `try_wait()`, EOF, or now Job Objects). Nothing to upgrade
to.

**Where this leaves root-cause digging, updated:** both cheap leads are exhausted.
What's left is genuinely WinDbg/ETW-level tracing of `mini_exit.exe`'s own thread state
during the hang — no shortcut avoids it now.

**WinDbg trace, 2026-09-05 — a real stack, and it overturns the working hypothesis.**
Installed the classic Debugging Tools for Windows (`cdb.exe`, not the Store WinDbg
Preview — needed console scriptability). Reproduced the `s5_minimal exit0` hang,
located the stuck `mini_exit.exe` and the `conhost.exe` spawned for its ConPTY, and
attached to both **non-invasively** (`cdb -pv`, which examines without taking over as
debugger — the target keeps running once detached) roughly 2s into the hang, dumping
every thread's stack (`~*kP`).

Every prior note in this section reasoned about the hang from the *outside* — timing,
CPU, EOF, exit codes — and landed on "stuck somewhere in `ExitProcess`" as the working
assumption, because a graceful exit was the thing under test and nothing else fit.
**That assumption is wrong, or at least incomplete.** `mini_exit.exe`'s one thread was
not anywhere near `ExitProcess`. Its entire stack was still in process *startup*:

```
ntdll!NtCreateFile
KERNELBASE!ConsoleCreateConnectionObject+0x1c8
KERNELBASE!ConsoleInitialize+0x19f
KERNELBASE!_KernelBaseBaseDllInitialize+0x526
KERNELBASE!KernelBaseDllInitialize+0xd
ntdll!LdrpCallInitRoutineInternal ... ntdll!LdrInitializeThunk
```

This is the CRT/loader's own DLL-init path, opening the process's initial connection
to the console subsystem — code that runs before `main()`, not after `std::process::exit()`.
`mini_exit.exe` prints its line and calls `exit()` from `main()`; if its only thread is
still inside `KernelBaseDllInitialize`, either it never reached `main()` at all on this
run, or (more likely, given every prior finding that output *does* eventually appear on
the pty) this specific stack was caught during a **second**, later console-connection
attempt this binary's runtime doesn't obviously make — genuinely unclear which, and worth
resolving before trusting this as the *complete* picture. What is clear: `ExitProcess`
was not where the previous ten spike runs' worth of indirect evidence pointed people to
guess, and it should stop being described as the leading theory.

**The more useful half of this trace is `conhost.exe`'s side.** Of its six threads,
five are unremarkably idle (threadpool workers parked in `NtWaitForWorkViaWorkerFactory`,
a render thread in `WaitForSingleObjectEx`, a Win32 message pump in `GetMessageW` — all
textbook idle-and-waiting, nothing to see). The sixth — `conhost`'s `ConsoleIoThread`,
the thread that services new connection requests — was caught **mid-request**, inside a
call chain that should not block indefinitely:

```
conhost!ConsoleIoThread
 → conhost!IoDispatchers::ConsoleHandleConnectionRequest
  → conhost!ConsoleAllocateConsole
   → conhost!Microsoft::Console::VirtualTerminal::VtIo::StartIfNeeded
    → conhost!Microsoft::Console::VtInputThread::DoReadInput
     → KERNELBASE!ReadFile → ntdll!NtReadFile   (blocked)
```

Read plainly: while handling `mini_exit.exe`'s own connection request, `conhost` calls
into `VtIo::StartIfNeeded`, which — synchronously, on the connection-handling thread
itself, not a background reader — issues a blocking `ReadFile` for VT input and never
gets it back. If `ConsoleIoThread` is the only thread that services connection requests
(consistent with everything else in this trace), then **while it is parked here, no new
console connection on this conhost can complete** — which lines up exactly with
`mini_exit.exe`'s own thread stuck at `NtCreateFile` in `ConsoleCreateConnectionObject`,
waiting for a response that the one thread able to send it will never get around to.
This is a specific, named, searchable candidate for the actual mechanism — a strong
step up from "some ConPTY-side handshake," though not yet confirmed as *the* cause
rather than *a* symptom caught at the same moment.

**Follow-up, same day, after a clean reboot — the single-snapshot concerns above are now
resolved, and one of them exposed a real mistake in this section's own reasoning.**

Rebooted to clear the zombie-process pile-up (confirmed clean: 0 leftover `conhost`/
`mini_exit` after reboot), then ran a **multi-snapshot** trace: attach-and-dump
`conhost`'s `ConsoleIoThread` roughly every 1.5s across a single hang, six snapshots
from 8ms to 11177ms. **`ConsoleIoThread` sits at the exact same `VtIo::StartIfNeeded →
VtInputThread::DoReadInput → ReadFile` call chain at every snapshot** — a stable,
whole-window block, not a mid-transition artifact. The single-snapshot concern is
closed: this is where it stays.

The other concern — "the process exited ~1s after the non-invasive dump, did attaching
cause that?" — led somewhere more important. The same multi-snapshot run's control pass
(no debugger, `mini_exit` polled from a separate process) showed it disappearing
undisturbed at ~12.3s, which **this section originally recorded as the child resolving
naturally.** That was wrong, and the mistake is worth stating plainly rather than
quietly fixing: `s5_minimal` (every prior test) gives up waiting after 8s, sleeps 4
more seconds "in case EOF shows up late," and *then calls `std::process::exit(1)`* —
8 + 4 = 12s, matching the observed disappearance almost exactly. **Process exit closes
every handle the OS holds for that process, including the ConPTY master** — which tears
down the whole console/child tree as a side effect. The ~12s "resolution" was never the
child completing; it was the *test harness's own scripted exit* cleaning up after
itself. Once you know to look for it, this is obvious — 8+4=12 is not a coincidence —
but nothing before the multi-snapshot control run isolated the harness's own exit from
the child's, because every prior test's observer process died at a fixed, predictable
offset from its own timeout.

**Built two more spikes to settle it properly.** `s7_long_wait.rs`: same as `s5_minimal`
but an 30s timeout instead of 8s, no grace-sleep — still confounded (it calls
`std::process::exit(1)` the instant its own timeout fires), and sure enough, polling a
live run showed `mini_exit` vanishing at the same moment `s7`'s own last log line
appeared, not before. `s8_never_timeout.rs` removes the confound entirely: `child.wait()`
with **no timeout at all**, and after `wait()` (which the code never expects to return),
the process parks in an infinite sleep loop rather than exiting — so nothing it does can
ever tear the tree down as a side effect. Run for **265+ seconds (4.4+ minutes)**,
polled independently the whole time: `mini_exit.exe` stayed alive and unreaped,
`wait()` never returned, and `s8` itself never exited. Manually killed at that point,
not because it resolved.

**Corrected conclusion: the graceful-exit hang has no observed natural resolution.**
It is not a slow reap that finishes around 12s if you wait long enough — every apparent
resolution seen anywhere in this investigation was the *observing* process's own exit
tearing the tree down, not the child completing on its own. Left genuinely alone, the
child and the `conhost.exe` connection-handling thread blocking it both stay wedged for
at least 4+ minutes, and there's no evidence either would ever resolve unassisted.

**A loosely similar historical bug exists**: [microsoft/terminal#1810](https://github.com/microsoft/terminal/issues/1810),
"ClosePseudoConsole API hanging," closed 2023/Terminal v1.17, whose report also mentions
"many leftover conhosts in the task manager." Not confirmed as the same underlying bug —
this build (26200) postdates its fix, and the issue's own thread doesn't document the
mechanism — recorded as a loose lead, not a match.

**Next step, not yet done:** the mechanism candidate (`conhost`'s `ConsoleIoThread`
synchronously blocked in `ReadFile` inside `VtIo::StartIfNeeded`) is still a strong
correlate, not a proven cause. Confirming it needs either ETW tracing of what that
`ReadFile` is actually waiting on, or reading `VtIo::StartIfNeeded`'s source (not
available locally — `conhost.exe`'s VT-I/O implementation isn't the open-source
`OpenConsole.exe`/Windows Terminal codebase; the OS-shipped `conhost.exe` this spawns
under is a separate, closed-source binary, so the wezterm/Windows-Terminal GitHub repo
searched earlier for leads 1-2 doesn't contain the code actually running here).

**RESOLVED, same day — root cause identified and fixed.** `s8_never_timeout`'s own log
had the answer sitting in it the whole time and this section didn't call it out until
now: the very first bytes the pty master ever produces, at 0-6ms, before anything else,
are `\x1b[6n` — a VT100 **Device Status Report / cursor-position query (DSR/CPR)**. That
lines up exactly with the WinDbg stack: `ConsoleIoThread` is blocked in
`VtInputThread::DoReadInput → ReadFile`, i.e. reading the **input** side of the very
same pty, waiting for the reply (`ESC[row;colR`) a real terminal would send back. Every
spike up to this point only ever read the master, never wrote to it — so conhost's own
startup handshake was left hanging, forever, on a reply nobody was ever going to send.

This is also a documented ConPTY behavior, not something specific to this build: the
`microsoft/terminal` wiki describes conhost's VT-I/O startup sending exactly this kind
of capability/cursor-position query and blocking a dedicated input thread on the reply
(background reading via [DeepWiki's ConPTY-and-VT-I/O
page](https://deepwiki.com/microsoft/terminal/2.4-conpty-and-vt-io); treat the specific
timeout figure it cites with caution — it describes a **3-second, DA1-specific**
`WaitUntilDA1(3000)` that falls through to a `StartupFailed` state, which does not match
what was actually observed here: a **DSR/CPR** query, unanswered for **265+ seconds**
with conhost still very much alive, not failed-and-torn-down. Likely a different query
on the same handshake path, possibly OS-build-dependent — flagged as a discrepancy
rather than silently reconciled). A handful of related community reports turned up too
([microsoft/terminal#18117](https://github.com/microsoft/terminal/issues/18117),
[#19922](https://github.com/microsoft/terminal/issues/19922),
[discussion #17716](https://github.com/microsoft/terminal/discussions/17716)) — none
confirmed as the identical bug, listed as pointers only.

**Confirmed by direct test**, not just by reading about it: `spike/src/bin/s9_dsr_reply.rs`
is `s8_never_timeout` with exactly one addition — a reader thread that watches for
`ESC[6n` and writes back `ESC[1;1R` the moment it sees it. Result: `wait()` returned in
**6-8ms** (both `exit0` → code 0 and `exit7` → code 7), instead of hanging indefinitely.
Ran multiple times, both scenarios, fully reproducible.

**Fix applied in production code**, not left as a spike-only finding: `daemon/src/pty.rs`
now wraps the pty reader in `ConptyDsrProbe` (Windows-only, `cfg(windows)`) — it scans
only the first 4KB ever read from the master for `ESC[6n`, answers it exactly once via
the existing write channel, and then gets out of the way completely for the rest of the
session (see its doc comment for why the budget and one-shot behavior both matter: a
program can legitimately send its own `ESC[6n` later expecting a real reply from
whatever terminal ends up attached, and this must never intercept that). Proven through
the real production code path, not the spike: `daemon/tests/pty_primitive_windows.rs`
runs the two exit-code fixtures Unix's `pty_primitive.rs` already had (`clean_exit_zero_
is_recorded_via_wait_not_eof`, `nonzero_exit_is_recorded_accurately`), against real
`cmd.exe` children spawned via `teleportd::pty::spawn`. Both pass, both resolve in
**0.02s total** for the pair — full `cargo test` for the `daemon` crate on this machine:
**63 passed, 0 failed**.

**What remains open:** the *mechanism* is now understood and neutralized well enough to
unblock the daemon, but it is still not a from-source-verified explanation — `conhost`'s
VT-I/O implementation is closed-source, so "conhost needs a DA1/DSR reply during startup
or its console-allocation path hangs indefinitely" is an empirically confirmed, externally
corroborated *behavior*, not something read directly out of Microsoft's source. The reply
value (`ESC[1;1R`, claiming cursor row/col 1;1) is also unverified as *the* value conhost
expects — it's a well-formed reply that happens to work, not a traced-and-confirmed one.
Full W2 Windows fixture parity (the rest of `pty_primitive.rs` — resize, terminate/
grandchild semantics; raw-mode has no Windows equivalent at all, see W2) followed this
same fix the same day — see [W2](#w2--windows-fixture-parity-not-yet-attempted).

**Engineering implication, now that the root cause is fixed:** the case this section
worried most about — an agent process that finishes **on its own**, with nobody calling
`terminate()` — is exactly what the `ConptyDsrProbe` fix restores exit detection for.
The existing termination policy ([03-pty-layer.md](03-pty-layer.md#termination)) was
already correct independent of this; what changes is that the Windows leg of `pty.rs`
no longer needs a heuristic or fallback signal at all, because the actual OS-level exit
notification now arrives, the same way it always did on Unix.

**Why this no longer blocks M1:** the *common* case — an agent process finishing
normally — now moves a session to `exited` on Windows correctly, confirmed via the real
`pty::spawn` code path, not just a spike. The Windows leg of `pty.rs`'s reap/exit-status
path can proceed on the existing design. [W2](#w2--windows-fixture-parity-not-yet-attempted)
(full Windows fixture parity) followed the same day and is also closed — see there.

## W2 — Windows fixture parity not yet attempted

**Tracked in [alehatsman/teleport#10](https://github.com/alehatsman/teleport/issues/10).**

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

**CLOSED, 2026-09-05, same machine as W1's fix.** `daemon/tests/pty_primitive_windows.rs`
now carries a full Windows-appropriate rewrite of `pty_primitive.rs`'s suite, written and
run for real (not cross-compiled, not guessed at from the Unix file):

- `echo_roundtrip` -- interactive `cmd.exe`, same shape as the Unix fixture
- `large_text_burst_read_drops_nothing` -- a `for /L` loop stands in for `yes`
  (cmd.exe has no infinite-repeat builtin reachable in one `/c` script); 200,000 lines
  verified undropped, undamaged, in order, once conhost's own VT init/teardown escape
  sequences are stripped (see below)
- `resize_is_observed_by_the_child` -- `mode con` is the Windows equivalent
  docs/10-testing.md already named for `stty size`
- `clean_exit_zero_is_recorded_via_wait_not_eof` / `nonzero_exit_is_recorded_accurately`
  -- unchanged, these already existed to prove W1's fix
- `terminate_reaches_exited_within_the_bounded_policy`, `terminate_under_output_load_does_not_deadlock`,
  `terminate_kills_the_grandchild_process_tree` -- the Windows termination mechanism
  (`ClosePseudoConsole` via dropping the master, killing every attached client) proven
  through the real `terminate()` path; the last of these also closes
  [S4](#s4--does-dropping-the-master-close-the-pseudoconsole)'s Windows leg, see there

**One Unix fixture has no Windows equivalent at all, by construction, not by omission:**
`large_write_arrives_intact_and_in_order`'s intent -- an arbitrary byte-exact payload
(all 256 byte values, including control bytes) survives a raw-mode round trip -- doesn't
translate. Unix raw mode (`stty raw -echo`) turns the pty into a plain byte pipe with
*no* interpretation; ConPTY has no equivalent for its *input* direction -- every byte
written to the master always passes through conhost's own VT input parser before the
child ever sees it, regardless of what console mode the child sets on its own stdin.
A payload built as `(0..N).map(|i| i % 256)` contains `ESC` (0x1B) roughly every 256
bytes, which conhost's parser reads as the start of a control sequence -- not a corner
case, guaranteed by the payload's own construction. There is no cmd.exe/PowerShell knob
and no child-side `SetConsoleMode` that bypasses this. Recorded as a genuine platform
gap, not a skipped test.

Two things surfaced along the way, folded into other sections rather than left here:
conhost writes its own VT session-init sequences (`ESC[?9001h`, `ESC[?1004h`, an SGR
reset, an OSC window-title write, ...) as the first bytes of *every* ConPTY session, and
separately, whichever `WriteConsole` call is literally last before a process exits races
conhost's own process-exit teardown sequence (observed directly: a final "y\r\n" arrived
as "y\r" + an escape byte + a late "\n"). Neither is specific to this fixture's content --
`large_text_burst_read_drops_nothing`'s fix (strip all VT escape sequences before
comparing, rather than fight the platform with sentinel lines) is the general answer, and
is now `strip_vt_sequences` in the test file itself, not documented as a design decision
elsewhere since it's test-only machinery. The terminate-latency flake this same suite
surfaced under concurrent load is recorded under [S4](#s4--does-dropping-the-master-close-the-pseudoconsole)
instead, since it's about `terminate()`'s timing, not fixture parity.

---

## W3 — `pty_primitive_windows.rs`'s own tests were oversubscribing the CI runner

**Tracked in [alehatsman/teleport#27](https://github.com/alehatsman/teleport/issues/27). Found and
closed same day, 2026-09-05, same suite as W2.**

Three CI runs in a row failed on `windows-latest` only, always the same test, always the
same line: `terminate_under_output_load_does_not_deadlock` (`pty_primitive_windows.rs:397`)
panicking at its own precondition (`pty_primitive_windows.rs:106`, `recv_until`'s timeout
branch) -- *before* reaching the `terminate()` call the test actually exists to check.
100% green every time on a dev machine (32 logical CPUs), 3/3 red on the hosted runner.

**Wrong first instinct: just raise the byte target or the timeout.** That's tuning a
number against a runner this repo cannot reproduce locally, exactly the mistake
[N5](#n5--macos-pty-reads-average-14-bytes-starving-the-queue-bounds-count-half) already warns against --
"not one to guess... with no way to reproduce this runner's speed locally." Before
touching the budget, the CI log's own panic payload was pulled (raw job log, not the
rendered one -- GitHub's own log renderer silently drops an oversized line rather than
erroring, which is why `gh run view --log` looked truncated) to get the actual number:
**161432 of the 262144 bytes needed, within the 10s budget** -- about 62% of target, a
real but *moderate* shortfall, not the 20-for-20-identical wall N5 hit. That distinction
matters: N5's retries failed identically because retrying doesn't change a relative-speed
race; this number pointed somewhere fixable instead.

**Root cause:** all 8 tests in this file spawn a real ConPTY child each and `cargo test`'s
default `--test-threads` (= logical CPUs) runs every `#[test]` in a binary concurrently.
Invisible on a 32-core dev box; on a hosted `windows-latest` runner (2 vCPUs, shared,
plausibly slowed further by realtime AV scanning on process/pipe I/O) that's 8-way
contention for real conhost/ConPTY work on 2 real cores -- explains a ~40% throughput
deficit far better than "the runner is generically slow," and unlike N5's case, is a cause
this repo could actually remove rather than work around.

**First fix, round one:** a single `std::sync::Mutex<()>` (`SERIAL` in the test file
itself, no new dependency -- `std::sync::Mutex::new` is `const`), acquired as the first
line of all 8 tests, forcing them to run one at a time regardless of `--test-threads`.
Poisoning is absorbed (`.unwrap_or_else(PoisonError::into_inner)`) on purpose: one test
panicking while holding the lock must not take the other seven down with it and turn one
real failure into eight confusing ones. Pushed and re-run against real CI (the only way to
check anything about this runner's speed) -- **still red, same test, same line**, but
moved: 193109 of the 262144 bytes now, up from 161432 (62% -> 74%). Contention was real
and the fix measurably helped; it just wasn't the whole story.

**Round two, same CI run's own log:** `large_text_burst_read_drops_nothing`, serialized
and running alone (no other test contending), took **30 seconds** for its own for-loop-
through-ConPTY workload -- near-instant locally. That test already carries a 60s budget
for that exact reason (`spawned.exit_rx.recv_timeout(Duration::from_secs(60))`, written
before this precondition was). `terminate_under_output_load_does_not_deadlock`'s
precondition, by contrast, was reusing `DEFAULT_TIMEOUT` (10s) -- a budget sized for
"does the child respond at all" checks (echo roundtrip, exit code), never re-examined
against sustained ConPTY throughput. This runner is genuinely slower at raw ConPTY I/O, not
only oversubscribed; serializing was necessary but not sufficient.

**Fix, complete:** keep the `SERIAL` mutex (real, measured improvement, addresses genuine
contention), and give this one precondition its own `BURST_PRECONDITION_TIMEOUT` (30s) --
matching, not guessing past, the budget its own sibling test already established for a
bigger comparable workload (200,000 lines / ~1.4 MB vs. this precondition's 256 KiB) in the
same file. The actual invariant under test -- `terminate()` must return in under 8s,
`exit_rx`/`eof_rx` bounded at 1s/5s -- is untouched; only the "make sure a real burst is
already in flight before racing it" precondition got more realistic room. Verified locally
(8/8 pass, `cargo clippy --all-targets -- -D warnings` clean, `cargo fmt --check` clean);
real confirmation is this fix's own next `windows-latest` CI run, same as every other
W-series entry in this file -- this runner's speed still can't be reproduced locally, only
measured one real run at a time.

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

**The open corollary above, answered for real (2026-09-05).** A CI run on
`ubuntu-latest` (not this spike's own machine — that never reproduced it, see
below) caught the case the corollary asked about directly: a WS test's
`binary_frames_carry_correct_contiguous_offsets` got the `exit` frame with
`final_offset: 0` instead of ever seeing `"hello"`. Root cause: `exited`
firing off `wait()` alone (correctly, per this section's own recommendation)
does not mean the reader thread has actually drained the last chunk yet — the
reaper thread's `wait()` can return before the reader's next `read()` does,
and nothing synchronized the two. `output.vt` was never wrong (S2's design
holds there — the reader keeps appending regardless of session state), but
`ws.rs`'s live `exit` frame captured `next_offset()` and closed the socket
the instant `exited` fired, so a fast process's trailing bytes could miss the
live connection entirely, recoverable only by reconnecting to read the log.
Fixed by finally consuming `eof_rx` (session.rs's module doc, "`eof_rx` is
consumed too"): `ws.rs` now finalizes the `exit` frame immediately when EOF
was already observed (the overwhelmingly common case — zero added latency),
and otherwise waits up to a 200ms bounded grace for it, still delivering any
output that arrives meanwhile, before finalizing regardless
(`EXIT_DRAIN_GRACE` in `ws.rs`). Bounded, not unbounded, for exactly the
reason this section gives: a live grandchild can hold EOF off indefinitely.
Regression test: `ws_protocol.rs`'s
`concurrent_fast_exits_never_lose_output_before_the_exit_frame`, run under
real thread contention (many sessions concurrently) since a single fast
`printf` did not reproduce this reliably even under deliberate CPU stress on
a 32-core dev machine (`taskset`-pinned to 2 cores, competing `yes` loops) —
the failure needed whatever scheduling latency `ubuntu-latest` had under load
that day, not something this repo could force to order on a well-provisioned
box. Recorded here rather than left as a false "verified" claim: the fix is
correct by construction (immediate finalize when already safe, a bounded
wait otherwise, a final non-blocking drain closing the remaining TOCTOU
window), not proven against the exact original interleaving.

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

**Re-run against the real fix, 2026-09-05 — closed.** W1's fix makes this directly
testable through the real production path for the first time:
`daemon/tests/pty_primitive_windows.rs`'s `terminate_kills_the_grandchild_process_tree`
(written for [W2](#w2--windows-fixture-parity-not-yet-attempted)) spawns an interactive
`cmd.exe` under `pty::spawn`, has it `start /B` a `powershell.exe`/`Start-Sleep` grandchild
(attached to the same console, not a new window — the Windows analogue of a Unix
grandchild that inherits the pty by not calling `setsid`), confirms the grandchild is
alive via an out-of-band `Get-CimInstance Win32_Process` query, then calls the real
`session.terminate()`. Result: the grandchild is gone within ~0.5s, every run. This
confirms both halves docs/10-testing.md's platform matrix already claimed --
`ClosePseudoConsole` (via `drop(master)`, `pty.rs`'s control thread) does take down
*every* attached character-mode client, not just the direct child -- and does so
through the actual termination policy, not a synthetic single-purpose spike. Combined
with `terminate_reaches_exited_within_the_bounded_policy` (same file) confirming the
direct child's own exit is observed via the fixed `ConptyDsrProbe` path, S4's Windows
half is no longer confounded by anything -- **closed**.

One related timing note surfaced by the same suite, not by this fixture specifically:
run concurrently with the rest of `pty_primitive_windows.rs` (several ConPTY sessions
tearing down at once), `terminate_reaches_exited_within_the_bounded_policy` was caught
once taking the full `GRACEFUL_WAIT` (5.0131867s) rather than the ~10ms it takes solo --
i.e. under load, `ClosePseudoConsole`'s "returns immediately" property (documented for
this build, ≥24H2) did not hold for that one run, and termination fell through to the
hard-kill step instead. Still within the documented bounded policy (that's what the step
is *for*), just not the fast path this build otherwise shows reliably. Not chased
further -- reproduced once, under real concurrency, same shape as
[S2](#s2--eof-is-not-exit)'s ws.rs regression (also only reproducible under real
scheduling contention, not forceable on demand). Recorded here rather than smoothed
over: the fixture's own assertion was loosened to match the actual documented contract
(the full bound) rather than the usual-but-unproven-reliable fast path.

---

## S5 — A detached grandchild cannot hold a pty past its parent on macOS

**Tracked in [alehatsman/teleport#11](https://github.com/alehatsman/teleport/issues/11).**

**Found in CI 2026-09-05; root-caused and closed the same day on real macOS hardware
(Darwin 24.6.0, arm64, Apple silicon).**

### What it looked like

S2 verified on Linux that a detached grandchild ignoring `SIGHUP` keeps a pty's master
open past the direct child's exit, and `daemon/tests/pty_primitive.rs`'s
`eof_and_exit_are_independent_signals` encoded exactly that. It was never reliably green
on `macos-latest`: one detach recipe (`perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV'`)
produced zero bytes, another (`trap '' HUP; sleep 2 &`) passed once and then failed with
"EOF arrived suspiciously early: 20ms". Two independently-reasoned recipes, neither
reliable, was read as a scheduling race.

**It is not a race, and it was never flaky.** On real hardware the "flaky" behaviour is
perfectly deterministic — 5/5 identical runs of the `grandchild` scenario, EOF and
`wait()` landing in the same millisecond. What varied in CI was which side of a
0-vs-2000ms assertion a *deterministic* 0ms landed on once the surrounding fixture
noise moved; the timing assertion made a hard, reproducible kernel behaviour look
stochastic.

### Root cause

Both kernels tear down a session leader's controlling terminal when it exits. Only Linux
exempts ptys.

**Linux** — `drivers/tty/tty_jobctrl.c`, `disassociate_ctty()`:

```c
if (on_exit && tty->driver->type != TTY_DRIVER_TYPE_PTY) {
        tty_vhangup_session(tty);
} else {
        struct pid *tty_pgrp = tty_get_pgrp(tty);
        if (tty_pgrp) {
                kill_pgrp(tty_pgrp, SIGHUP, on_exit);
```

A pty takes the `else` branch, so the teardown is *only* a `SIGHUP` to the foreground
process group. A grandchild that ignores `SIGHUP` shrugs it off and keeps its inherited
slave fd, the master stays open, and EOF lags exit for as long as that grandchild lives.

**macOS/XNU** — `bsd/kern/kern_exit.c`, `proc_exit()`, under the comment *"Controlling
process. Signal foreground pgrp, drain controlling terminal and revoke access to
controlling terminal."*:

```c
pgsignal(tpgrp, SIGHUP, 1);
...
VNOP_REVOKE(ttyvp, REVOKEALL, &context);
```

No pty exemption, and `REVOKEALL` is not a signal — it invalidates *every descriptor
pointing at that tty, in every process*, regardless of anyone's signal disposition. The
master hits EOF the instant the session leader exits. The `revoke(2)` hypothesis recorded
here previously was right; it is now confirmed rather than assumed.

### How it was verified

`spike/src/bin/s10_ctty_revoke.rs` — a hand-built pty (`posix_openpt`/`grantpt`/
`unlockpt`/`ptsname`, no `portable-pty` in the way, so the slave's *path* and the ctty
setup are both visible) plus a grandchild that `setsid()`s, ignores `SIGHUP`, and reports
to a log file instead of to the pty it is being measured on. It discriminates the two
hypotheses S5 could not separate from CI:

| Observation | `ctty` (real config) | `noctty` control |
| --- | --- | --- |
| Direct child reaped | 9ms | 9ms |
| Master EOF | **9ms** | **4181ms** (grandchild's own exit) |
| Grandchild alive after EOF | **yes**, for seconds; runs to completion | n/a — it outlives the master |
| Grandchild `write(inherited pty fd)` | **`EIO`, from the very first attempt** | succeeds, every byte reaches the master |
| Grandchild `open("/dev/tty")` | `ENXIO` | `ENXIO` (it `setsid`'d) |
| Fresh `open("/dev/ttysNNN")` | **succeeds**, `write` returns 22 | succeeds |

Three things fall out of that table:

- **It is a revoke, not a kill.** The grandchild is demonstrably alive (`kill(pid, 0)`
  succeeds for seconds afterwards, and it logs its own normal exit) while every write to
  its inherited pty fd fails with `EIO`. No amount of `trap ''`/`nohup`/`setsid` changes
  this, because none of it is about signals.
- **It is specifically the controlling terminal.** The `noctty` control — identical
  setup minus `setsid` + `TIOCSCTTY` — behaves exactly like Linux. So the mechanism is
  ctty teardown, not fd accounting and not `SIGHUP`.
- **There is no userspace workaround.** A fresh `open()` of the same `/dev/ttysNNN`
  succeeds and accepts writes, so the *device* survives — but the master is already
  permanently at EOF by then (a post-EOF `read(master)` returns 0 again, even after the
  reopened slave has been written to), so nothing sent through it can ever be read. The
  only way to avoid the revoke is to not have a controlling terminal at all, which is
  not on the table for a terminal product: no ctty means no job control, no `Ctrl-C`, no
  `SIGWINCH`.

### Does this cost the product anything?

**No.** The obvious worry is truncation — if the kernel yanks the tty at exit, does the
command's last output survive? XNU drains before it revokes (that is what "drain
controlling terminal *and* revoke" in the comment means), and it holds up under
measurement: a 20 000-line burst arrives as 208894/208894 bytes, deterministically across
runs, both with a prompt reader and with one deliberately stalled a full second before it
started reading. Nothing is lost.

What actually changes is a scenario that was never a product promise: on macOS a
backgrounded process cannot keep a teleport session's stream alive after the shell that
started it exits. Anyone wanting that wants `tmux`/`screen` — which work fine, because
they hold their *own* pty and are not descendants of this one.

### Resolution

`eof_and_exit_are_independent_signals` is no longer `#[cfg(target_os = "linux")]`-gated
with a hypothesis attached. It is now a documented two-OS split — the same question with
two different *correct* answers:

- Linux: `eof_and_exit_are_independent_signals` — EOF lags exit by the grandchild's
  lifetime.
- macOS: `eof_follows_the_session_leaders_exit_even_with_a_live_grandchild` — EOF
  coincides with exit, **and the grandchild is asserted to still be alive**. That
  liveness assertion is the load-bearing one: it is what distinguishes a revoked
  descriptor from a dead process, and it is what fails the day either kernel changes its
  mind.

Green 30/30 on the targeted fixture and 15/15 on the full `pty_primitive` suite under
12-way CPU saturation — the stress the original "scheduling race" theory predicted would
break it.

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

---

# Networking quality

None of these is a defect in what is written. They are the difference between "the
reconnect is correct" and "the reconnect is impressive", and the docs currently address
only the first.

## N1 — Keystroke latency

**Not blocking. Measure at M9.**

**Measured 2026-09-05, real hardware (`mainpc`, Linux, iPhone on cellular).** Numbers
below are real. **Caveat: this measures the direct-path case, not the relayed one this
section is actually worried about** — see after the numbers.

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

**Result (2026-09-05, iPhone on cellular, off the test machine's LAN, `tailscale
ping`):** 32, 34, 36, 43, 57 ms — median ~36 ms. `tailscale status` showed the peer's
endpoint as a public IP with `direct`, no `relay` in the status line: this connection
never touched DERP. Subjective typing feel through the actual UI at this RTT:
**instant, no noticeable lag**.

**What this does and doesn't answer.** It answers the *good* case cleanly: when a
direct WireGuard path exists, keystroke latency is a non-issue and predictive echo buys
nothing worth its complexity. It does **not** answer this section's actual worst case —
a phone behind CGNAT falling back to a DERP relay, the 80–200 ms scenario the whole
section is about. This specific phone/carrier/network combination happened to punch a
direct path; that is itself useful data (direct is achievable, not purely theoretical)
but it is not the number that decides whether predictive echo is required. **Still
open:** get a DERP-relayed sample — e.g. `tailscale ping` a peer while `tailscale
status` shows `relay` for it (some carrier/NAT combinations force this; a corporate or
CGNAT-heavy network is more likely to) — before treating N1 as closed.

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

**Investigated, 2026-09-05 — closed as infeasible with the pinned stack, not implemented.**
Read the actual source of both crates in the pinned tree rather than trusting the crate
docs' feature lists (`~/.cargo/registry/src/.../axum-0.8.9`, `tungstenite-0.29.0`):

- `axum::extract::ws` (the only WS implementation this daemon uses — `ws.rs` never touches
  `tokio-tungstenite` directly, `daemon`'s own dependency on it is dev-only, for tests
  driving a real socket) is built directly on `tokio_tungstenite::tungstenite::protocol`.
  Its `WebSocketConfig` — the one knob `axum::extract::ws::WebSocketUpgrade` exposes —
  has exactly five fields: `read_buffer_size`, `write_buffer_size`, `max_write_buffer_size`,
  `max_message_size`, `max_frame_size`, plus `accept_unmasked_frames`. No compression field,
  no extension list, nothing to turn on.
- `tungstenite` 0.29 implements **no** permessage-deflate codec at all, and the string
  `"permessage-deflate"` appears exactly once outside a comment in the whole crate — inside
  `handshake/headers.rs`'s own unit test, parsing header *text*, not negotiating anything.
- The client handshake's own RFC 6455 step 5 (validate/react to a server's
  `Sec-WebSocket-Extensions` response) is a literal `// TODO` in
  `handshake/client.rs` — extension negotiation was never wired up, client or server side.

So this was never a config flag away, on either the client or server side of this stack.
Closing it for real means one of: (a) swap `axum::extract::ws` for a different server-side
WS implementation with RFC 7692 support (a maintained one wasn't found during this pass —
most of the Rust WS ecosystem shares tungstenite's gap), or (b) hand-roll permessage-deflate
on top of the raw frames `axum`'s `Message::Binary`/`Text` already expose, terminating it
below the offset-prefix layer rather than above it (the constraint this section's own "do
not compress above the WebSocket layer" rule already anticipated). Both are materially more
than the half-day this item was scoped at, and neither is free — recorded as **investigated
and rejected for the pinned stack**, not as done, so a future pass doesn't waste time
looking for a config knob that isn't there. If cellular bandwidth becomes a measured
problem post-MVP, (b) is the shape to pursue; N1's numbers (direct-path RTT is already
"instant," not bandwidth-bound) mean nothing is currently pushing for it.

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

**Measured, 2026-09-05, real `ubuntu-latest` CI — closed, acceptable as designed.**
`daemon/tests/reconnect_storm.rs`: a long-lived control subscriber, already attached and
live, is drained continuously while 40 fresh clients simultaneously `attach(0)` against
the same session (a static 12 MiB backlog, so each storm client does several real
1 MiB catch-up rounds — real disk reads, real fan-out-mutex contention, on a genuine
multi-threaded Tokio runtime rather than cooperative single-thread interleaving).
Result: **baseline 94000 B/s, during the storm 93500 B/s — a 0.995 ratio, no measurable
degradation** on the control subscriber's own throughput, and it was never disconnected,
never stalled past a bounded timeout, and its replay-plus-live bytes stayed a byte-for-byte
prefix of the on-disk log throughout. The trade this section worried about (many
concurrent catch-ups competing for the same short-held mutex the reader thread also
locks) does not show up as observable jitter on the live path at this concurrency level,
confirming the per-round design over the rejected up-front-registration alternative was
the right call without needing a second mitigation (an admission limit or a queue).
Getting a clean number took three real bugs found and fixed against actual CI first, none
of them in the product — recorded in the fixture's own doc comments rather than repeated
here: an unbounded (then a paced) producer overflowing the control subscriber's own queue
before the storm even started (an ordering bug — attach before backlog was static, not a
rate problem), and a double-counted byte range in this file's own final correctness check.
`target_os = "linux"`-gated, same reasoning as `session_catchup.rs` and the N5 fixtures —
not re-tuned for `macos-latest`'s different timing.

---

## N5 — macOS pty reads average 14 bytes, starving the queue bound's count half

**Found in CI 2026-09-05; root-caused on real macOS hardware 2026-09-05
([#25](https://github.com/alehatsman/teleport/issues/25)). Diagnosis was
revised twice -- both earlier theories were wrong, and both are kept below to
save the next person the same detour. What remains open is a design decision,
not a diagnosis.**

`daemon/tests/session_backpressure.rs`'s `slow_subscriber_is_disconnected_and_never_blocks_the_reader`
drives its "fast" subscriber through `attach(0)`, then drains it continuously
against a session running `yes | head -c 10MiB` -- about as aggressive a
producer as a shell can make. Reliable on Linux CI; on `macos-latest` the fast
subscriber itself gets disconnected.

**First theory (wrong):** that `Replay::next_round`'s last round
([session.rs](../daemon/src/session.rs)) registers the live `Fanout`
subscriber *before* reading the final replay chunk off the lock leaves a
narrow window -- between registration and the caller's first `recv()` --
where nothing drains the fresh subscriber while the producer fills its 8 MiB
budget. Mitigated by retrying the whole attach on an early disconnect,
reasoning that each retry narrows the race (more of the producer's output
already on disk, less still in flight).

**That mitigation failed outright in CI: 20 out of 20 retries disconnected
early, not intermittently.** A race that narrows with each attempt does not
fail *every* attempt regardless of how many you allow. The retry's own
premise -- "once the producer exits there is no more live output left to
race against" -- only holds once the producer has actually finished; while
it is still running, a **fresh** `attach(0)` restarts the catch-up from
offset 0 every time, facing the *same* total gap on every attempt. If this
runner's catch-up throughput (bounded by real per-round disk reads,
deliberately kept off the fan-out lock so the PTY reader thread is never
blocked behind them -- see [N4](#n4--reconnect-storms-and-reader-thread-contention))
is simply slower than `yes`'s production rate for the whole test's duration,
every attempt hits `should_register`'s stalled-client path
([session.rs](../daemon/src/session.rs): `gap <= LIVE_GAP_BYTES ||
stalled_rounds >= MAX_STALLED_ROUNDS || total_rounds >= MAX_CATCHUP_ROUNDS`)
identically, truncating to the freshest `LIVE_GAP_BYTES` and reporting a
hole each time -- unwinnable by retrying, since retrying doesn't change the
relative throughput. This reads as a genuine, sustained mismatch on this
runner, not a narrow one-off timing coincidence.

**Not a one-line fix**, and not one to guess a third time against real CI
with no way to reproduce this runner's speed locally. Reading before
registering, or registering under a lock held through the read (closing the
*first* theory's window) would both reopen bugs this design already fixed on
purpose: the attach-race
([04](04-api-protocol.md#catch-up--register-late-not-early)) and the PTY
reader thread blocking on I/O under the shared lock. Neither would address
the actual, now-confirmed cause anyway. The fixture is `target_os`-gated off
macOS (2026-09-05) rather than shipped false-green or silently weakened.

**Second theory (also wrong):** that this was the runner -- a slow, shared or
throttled `macos-latest` box, plausibly not representative of any real
client. That predicted a fast local Mac would not reproduce it.

**Root cause (measured, 2026-09-05, M-series Mac).** It reproduces even on
fast hardware -- 3 failures in 20 unloaded runs -- so runner speed is an
*aggravator, not the cause*, and it is not about `should_register`'s
round-trip either. The queue
bound in [`fanout.rs`](../daemon/src/session/fanout.rs) is *whichever trips
first* of `MAX_QUEUE_CHUNKS = 256` and `MAX_QUEUE_BYTES = 8 MiB`. Those two
halves are calibrated against wildly different realities:

| | macOS (measured) | Linux |
| --- | --- | --- |
| mean pty read | **14 bytes** (min 2, p90 18, max 1024 -- the tty output buffer) | coalesces far larger |
| chunks per 10 MiB | **737,705** | orders of magnitude fewer |
| what 256 chunks actually holds | **~3.5 KiB** | up to the 8 MiB the design intends |
| which half of the bound governs | count | bytes |

So on macOS a subscriber is disconnected after buffering ~3.5 KiB, against a
design that promises 8 MiB of headroom -- roughly 2300x tighter. Any client
that stalls even briefly is dropped. `pty.rs`'s `READ_BUFFER_SIZE` of 64 KiB
is effectively dead there; no read ever exceeds 1024 bytes.

Confirmed by discrimination, not inference: raising `MAX_QUEUE_CHUNKS` alone
(65536 slots, byte budget untouched) takes the fixture from 3/20 failures to
**20/20 passing**.

**Resolved (2026-09-05): the bound is now a single byte budget that charges
per-chunk overhead.** Option 2 below, mechanised so it needs no second number.
`publish` acquires `payload.len() + CHUNK_OVERHEAD` permits (64 B: a channel
slot plus that queue's share of the shared `Arc<[u8]>` header) and
`Subscription::recv` returns exactly that, so the semaphore bounds *memory*
rather than payload bytes. The channel's capacity is derived from the same
budget (`MAX_QUEUE_BYTES / CHUNK_OVERHEAD`) and so cannot trip first -- it
exists because `mpsc::channel` needs a number, not as a second bound.

| chunk size | headroom before | after |
| --- | --- | --- |
| 14 B (macOS) | 3.5 KiB | ~1.5 MiB of payload, 8 MiB accounted |
| 64 KiB (Linux) | 8 MiB | 8 MiB -- overhead is 0.1%, unchanged |

**What decided it between the two candidates** was not the measurement, which
fits either. It was that `replay.rs` already derives the entire catch-up
convergence policy from the byte half:
`LIVE_GAP_BYTES = MAX_QUEUE_BYTES / 8`, with `should_register` handing a
subscriber live at `gap <= 1 MiB` on the stated premise that seven eighths of
its queue stay free. On macOS that subscriber's queue held 3.5 KiB. The count
bound was not merely a second bound disagreeing with the first -- it silently
invalidated a documented invariant belonging to another module. Bytes is the
unit this design reasons in; `256` was an artifact of `mpsc::channel`
requiring a capacity argument. Coalescing reads would have *hidden* that
rather than made `LIVE_GAP_BYTES` true.

Regression coverage is `fanout.rs`'s
`tiny_chunks_get_the_budget_not_a_slot_count`, which is deterministic and
needs no pty: a never-drained subscriber must accept exactly
`MAX_QUEUE_BYTES / queue_cost(14)` chunks and then be dropped -- both that it
is far past 256, and that it is still bounded.

**The remaining candidate, now its own question.** *Coalesce reads in
`pty.rs`* before publishing: 737k chunks per 10 MiB means 737k log appends,
mutex acquisitions, `Arc` allocations and per-subscriber sends, genuine
overhead paid only on macOS. That is a *throughput* question, not a bound
question, and it is cheaper than this section first assumed -- `libc` is
already a `cfg(unix)` dependency, so a zero-timeout `poll()` could merge only
*already-available* bytes and cost the interactive echo path nothing, which is
the objection that made coalescing look unaffordable. Unverified blocker:
`try_clone_reader()` hands back a bare `Box<dyn Read + Send>` with no fd, so
whether this is reachable through portable_pty 0.9 is unknown. Filed
separately.

**The two fixtures gated the same day: measured, and the picture is not what
either the original theory or the first correction said.**

* `daemon/tests/session_replay.rs`'s
  `disconnect_between_chunks_and_reconnect_has_no_gap_or_duplicate` (the M3
  gate) **is** N5, and the original gating was right. It passes **20/20 on an
  M-series Mac** -- which is why it was briefly un-gated -- and then failed
  first try on `macos-latest`. ~3.5 KiB of headroom is enough for a subscriber
  that never stalls; a slow runner stalls it. **Fast local hardware cannot
  observe this bug at all**, so a green local run is not evidence about it.
  Stays gated until the bound is fixed.
* `daemon/tests/session_catchup.rs`'s
  `attaching_far_behind_a_producing_session_reaches_live` (the D1 gate) is a
  *different* problem: it fails on its own guard, meaning it has stopped
  reproducing D1 rather than found a bug. The cause is the trickle's rate.
  `sleep 0.01` nominally ticks 100x/s but forks `sleep` every iteration, and
  macOS fork+exec holds it to ~31 iterations/s -- **51 chunks/s**, needing
  5.0 s to overflow 256 chunks against a catch-up window of ~4.8 s, exactly on
  the boundary (10/20 locally). Batching writes per fork removes fork cost
  from the rate and fixes it locally (112 chunks/s, 0/20) but **not** on
  `macos-latest`, which is slower still. Stays gated.

**The methodological lesson, which cost two wrong turns.** "Three fixtures
failed the same day, therefore one root cause" was the first error -- there
were two causes, not one. "It passes 20/20 on real hardware, therefore the
gate was wrong" was the second, and it is the more dangerous of the two: a
fast M-series Mac is not a slow CI runner, and for a bug whose whole mechanism
is *a subscriber stalling*, the fast machine is precisely the one that cannot
reproduce it. Measuring on real hardware settled the *mechanism* (14-byte
reads, count bound) but could not settle *exposure*. Both need checking, and
only CI can check the second.

**All three fixtures are un-gated as of the fix**, and the D1 one was
redesigned in the same change rather than after it, because the coupling ran
deeper than first recorded: `session_catchup.rs` relied on the *count* half to
kill its control subscriber, and its trickle is `tick\n` -- 5 bytes -- so it
was tripping 256 chunks at ~5 KiB **on Linux too**, not only on macOS. Making
the bound a byte budget would have needed ~890 s of trickle against a ~4.8 s
window there. It now drives the kill with bytes instead of a rate: the child
blocks on `read` until the test writes to it, then emits a burst half again
the size of the whole bound, with the control subscriber registered across all
of it. Nothing in that fixture is calibrated against how fast anything runs
any more.

That redesign front-loads the burst -- it lands before the catch-up walk
rather than during it, because letting it land during the walk reintroduces a
rate dependency in the other direction (a producer out-running the client for
four consecutive rounds trips `MAX_STALLED_ROUNDS`, and the walk clamps with a
reported hole). The control's claim is that the register-first ordering is
fatal; *when* it dies was never the claim.

**The bar this section set for itself, met:** green on `macos-latest`, not
just locally. 20/20 local iterations of all three fixtures is recorded here
only because it is necessary, not because it was ever sufficient -- that is
the mistake above, and it is exactly the evidence that misled once already.

---

# Native client readiness

## P1 — the native bearer-WS path has never been driven by a real native client

**Not blocking any MVP milestone. Blocks the iOS Phase 1 spike ([13-native-clients.md](13-native-clients.md#phase-1-implementation-plan)).**

[12-identity-and-connectivity.md](12-identity-and-connectivity.md#client-classes-and-why-origin-is-not-universal)
specifies the native-client path precisely: no `Origin` header, `Authorization: Bearer`
presented on the WS upgrade itself (not `?token=`), credential mandatory. Every test that
exercises this today is either the Rust-side test suite calling the handler directly, or
the browser client (which takes the *other* path — `Origin` present, no header on WS,
since `WebSocket` in a browser cannot set one). Nobody has yet pointed a real
`URLSessionWebSocketTask` (or any non-browser HTTP client) at a running daemon and
confirmed the handshake, offset framing, and bounded `tail` behave as documented from
that side.

This is exactly the shape of gap this file exists to hold — a mechanism confidently
specified and plausibly correct by code review, not yet proven by running the actual
client class against it. Low risk (the server-side code path is generic, not
browser/native-specific internally), but "low risk" was also W1's starting assumption.

**Closed by:** step 1 of the iOS Phase 1 plan — a bare connectivity spike, no UI, before
any Swift UI is built on top of the assumption. Needs a Mac; not runnable from this
environment.

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
