# 03 — PTY layer

This is the highest-risk file in the product. Everything platform-specific lives here.

## The rule

> **The PTY reader must never wait for a WebSocket client.**

Microsoft's own ConPTY documentation warns that its synchronous input/output channels
can deadlock if handled incorrectly, and recommends servicing the communication
channels independently. A phone can suspend its browser, enter a tunnel, switch
networks, lose Wi-Fi, or simply consume too slowly. None of that may reduce the rate at
which `teleportd` drains the PTY.

**Never write this:**

```text
PTY read
   ↓
await websocket.send()     ← WRONG
   ↓
PTY read
```

## Thread model

```text
                     ┌── dedicated output reader thread
                     │       ├── append to output.vt
                     │       ├── advance next_offset
                     │       └── non-blocking fanout to bounded queues
                     │
                     ├── dedicated writer thread
Unix PTY / ConPTY ───┤       └── write()  (can block indefinitely; see S3)
                     │
                     ├── dedicated control thread
                     │       ├── resize()
                     │       └── terminate()
                     │
                     └── dedicated reaper thread
                             └── child.wait()  (blocks until exit; see S1)
```

Four dedicated `std::thread`s per session, not two. **Not** `tokio::task::spawn_blocking`.

The original two-thread split (reader; input+control combined) does not actually cover
the design's three independently-blocking concerns — `read()`, `write()`, and waiting on
the child — and the M1 spike ([15-open-questions.md#the-m1-spike](15-open-questions.md#the-m1-spike))
confirmed both failure modes it predicted: a write on an unread pty can block
indefinitely and, on the original model, wedges `terminate` behind it ([S1](15-open-questions.md#s1--who-reaps-the-child),
[S3](15-open-questions.md#s3--a-blocking-write-wedges-terminate)); nothing in the
two-thread model calls `wait()`, so the exit code has no path into the session row.
`write` and `wait()` each need their own thread so neither can be stuck behind, or
block, `resize`/`terminate`. `resize` and `terminate` stay together on the control
thread — both are short and bounded, and that's where `terminate`'s own bounded wait
lives (see Termination, below).

`spawn_blocking` is documented for bounded blocking work that eventually finishes. A
permanently-blocking PTY loop occupies a blocking-pool slot indefinitely; enough
sessions and the pool starves, taking unrelated blocking work down with it. Dedicated
threads are simpler and line up with Microsoft's advice to service ConPTY's synchronous
channels separately.

Threads talk to the async world through:

- **reader → subscribers**: `tokio::sync::mpsc::Sender::try_send` (never `blocking_send`)
- **async → writer thread**: a dedicated channel/queue for `write()`, separate from control
- **async → control thread**: `std::sync::mpsc` or a `Mutex<PtyControl>`; `resize` and
  `terminate` are short and non-blocking except `terminate`'s own bounded wait (see below)
- **async → reaper thread**: nothing sends it commands; it exists only to block on
  `child.wait()` and publish the exit code + status transition when it returns

## Spawn

```rust
use portable_pty::{native_pty_system, CommandBuilder, PtySize, PtySystem};

fn spawn_session(command: &str, args: &[String], cwd: &Path, cols: u16, rows: u16)
    -> anyhow::Result<()>
{
    let system = native_pty_system();

    let pair = system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(command);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.cwd(cwd);

    let _child = pair.slave.spawn_command(cmd)?;

    // Move these into dedicated long-lived I/O workers.
    let _reader = pair.master.try_clone_reader()?;
    let _writer = pair.master.take_writer()?;

    Ok(())
}
```

Rules:

- **argv array, never a concatenated shell string.** A shell may be the `command` when
  the user deliberately wants shell parsing — that is their explicit choice, not a
  default. See [06-security.md](06-security.md).
- Drop the `slave` handle in the parent after spawning on Unix, so the master sees EOF
  when the child exits.
- `cmd.cwd()` must receive a validated, existing directory.
- Do **not** copy the daemon's full environment into metadata. The child inherits the
  daemon environment plus explicit overrides; overrides are redacted in API responses.

## Reader loop

```rust
loop {
    let n = pty_reader.read(&mut buffer)?;
    if n == 0 {
        break; // EOF: child exited or master closed
    }

    // 1. Persist output first.
    output_log.write_all(&buffer[..n])?;

    // 2. Advance the authoritative output offset, atomically with fan-out.
    let start = next_offset;
    next_offset += n as u64;

    // 3. Optional short RAM replay cache — not in the MVP.
    //    Correctness never depends on it; the file is authoritative.
    //    See 05-persistence.md#in-memory-ring-optional.
    ring.push(start, &buffer[..n]);

    // 4. Fan out without ever waiting on a slow subscriber.
    for subscriber in subscribers.iter() {
        if subscriber.try_send(start, &buffer[..n]).is_err() {
            subscriber.mark_slow();
        }
    }
}
```

Ordering is not negotiable: **persist, then advance the offset, then fan out.** A
subscriber must never see an offset for bytes that are not yet on disk, or a reconnect
that seeks to that offset will read past the end of the file.

Steps 2–4 happen under one short mutex that also guards subscriber registration. That
mutex is what closes the attach race ([04-api-protocol.md](04-api-protocol.md#attach-race)).

`buffer` is 64 KiB. Do not `fsync` per chunk; fsync on session close and on a periodic
timer.

## Backpressure

Every subscriber has a **bounded** outbound queue (start at 256 chunks / 8 MiB,
whichever trips first). Overflow closes that connection; it never blocks the reader.

```text
PTY output
    ↓
log append
    ↓
subscriber queue full?
    ├─ no  → enqueue
    └─ yes → disconnect subscriber (WS close 1013 "try again later")
              ↓
             client reconnects
              ↓
             replay from its last known offset
```

This converts a slow consumer from a process-level failure mode into an ordinary
reconnection event. It is a feature, not a degradation — exercise it in tests.

## Resize

`ResizePseudoConsole` (and `TIOCSWINSZ` on Unix) changes the *pseudoconsole's*
dimensions so attached console applications observe the correct width and height.
There is exactly **one** effective PTY size.

Therefore "every browser sends its own dimensions" is wrong. A 390px phone must not
fight a 160-column desktop.

**One control lease per session:**

```text
desktop: control
phone:   observe

phone presses "Take control"

desktop: observe
phone:   control
```

Only the controller may send keystrokes and resize events. Observers receive output
only. Lease semantics, transfer, and messages: [04-api-protocol.md](04-api-protocol.md#control-lease).

Resize handling:

- Coalesce: apply at most one resize per 100 ms per session; keep the latest.
- Clamp to `1..=1000` for both dimensions; reject anything else as a protocol error.
- Persist the applied `cols`/`rows` to SQLite, throttled (once per second is plenty).

## Termination

`ClosePseudoConsole` sends `CTRL_CLOSE_EVENT` to connected clients, and applications
may still emit output during shutdown. Microsoft advises either closing the output pipe
first or continuing to drain output around closure. Ending the pseudoconsole terminates
attached character-mode clients — **including processes created beneath a shell**. So
"close session" is meaningfully different from "forget the child PID".

Since **Windows 11 24H2 (build 26100)** `ClosePseudoConsole` returns immediately to
avoid a class of deadlocks. Older builds may wait indefinitely for pseudoconsole exit.
This is why closure happens on the dedicated control thread with a bounded wait, never
on a Tokio worker.

### State machine

```text
RUNNING
   │
   │ user requests terminate
   ▼
CLOSING
   ├── reject new input
   ├── continue draining output          ← keep the reader thread alive
   ├── request graceful shutdown
   ├── close pseudoconsole / process
   └── wait with bounded policy
   ▼
EXITED
```

This is the *user-initiated* path. The child can also exit on its own, and the reaper
thread's `wait()` can return well before the reader thread sees EOF — a descendant
that inherited the pty (e.g. a backgrounded process the child left behind) can hold
the master open indefinitely after `wait()` already has the exit code
([15-open-questions.md#s2](15-open-questions.md#s2--eof-is-not-exit)). In that case:
`RUNNING → EXITED` fires directly off the reaper thread's `wait()` result — do not
wait for reader EOF to mark the session `exited`, that's the whole point of keeping
exit code and EOF as separate signals. The reader thread keeps appending to
`output.vt` in the background, independent of session state, until it gets its own
EOF or the session is garbage-collected.

### Concrete policy

1. Set state `closing`. Input writes now return an error to the controller.
2. **Graceful signal.**
   - Unix: `libc::killpg(pgid, SIGHUP)`, then `SIGTERM` to the child. **Do not** fall
     back to dropping the master as a termination mechanism: the reader thread holds
     its own cloned fd for the session's entire life (`try_clone_reader()`), and the
     writer holds another (`take_writer()`) — both are `dup()`s of the same open file
     description, so dropping only the `MasterPty` handle while those live closes
     nothing and raises no signal (verified empirically,
     [15-open-questions.md#s4](15-open-questions.md#s4--does-dropping-the-master-close-the-pseudoconsole)).
     `killpg`/`SIGTERM` is the only mechanism relied on; dropping every remaining
     handle happens only as post-reap cleanup, after `wait()` has returned.
   - Windows: `ClosePseudoConsole` via dropping the `portable-pty` master handle.
     Unverified on real Windows as of this writing — see the Windows note in
     [15-open-questions.md#s4](15-open-questions.md#s4--does-dropping-the-master-close-the-pseudoconsole).
3. Wait up to **5 s** for child exit, **while the reader thread keeps draining**.
4. If still alive: `child.kill()` (hard kill — `SIGKILL` on Unix).
5. Wait up to a further **2 s**, then give up and mark the session `exited` with
   `exit_code = null` and a `lost_reason` of `"kill_timeout"`.
6. Reader thread exits on EOF; flush and fsync `output.vt`; record `exited_at_ms` and
   `exit_code`; emit the `exit` control frame to all subscribers; close their sockets.

`portable-pty`'s `Child::kill()` is a hard kill on Unix. Do not mistake it for a
graceful stop — that is why step 2 exists.

## The `TerminalSession` trait

`portable-pty` already handles platform abstraction. This trait exists to isolate
**application lifecycle policy**, not to replace it.

```rust
trait TerminalSession {
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
    fn terminate(&mut self) -> Result<()>;
}
```

Do not scatter ConPTY special cases through HTTP handlers. If a handler needs to know
the platform, the abstraction has leaked and the fix belongs in `pty.rs`.

## Platform checklist

Treat Windows as a first-class test platform from day one, not a port at the end. Its
synchronous pipe behavior and shutdown semantics genuinely differ from Unix PTYs.

| Behavior | Unix | Windows |
|---|---|---|
| Create | `openpty` | `CreatePseudoConsole` (synchronous I/O handles required) |
| Resize | `TIOCSWINSZ` | `ResizePseudoConsole` |
| Graceful stop | `SIGHUP`/`SIGTERM` to process group | `ClosePseudoConsole` → `CTRL_CLOSE_EVENT` |
| Close blocks? | No | Yes on < Win11 24H2; returns immediately from build 26100 |
| Child tree on close | Depends on signal delivery | Attached character-mode clients terminate, including under a shell |
| Deadlock risk | Low | Real — service input and output channels independently |
