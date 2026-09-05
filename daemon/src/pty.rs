//! The PTY primitive: spawn, read, write, resize, exit detection, termination.
//!
//! See docs/03-pty-layer.md for the design this implements and
//! docs/15-open-questions.md for the spike (S1-S4, W1) that shaped it. Four
//! dedicated `std::thread`s per session -- reader, writer, control
//! (`resize`/`terminate`), reaper (`child.wait()`) -- never
//! `tokio::task::spawn_blocking` (docs/03-pty-layer.md#thread-model).
//!
//! **M1 scope boundary:** this file owns the raw primitive only. It does not
//! know about `output.vt`, byte offsets, or a subscriber registry -- that's
//! `session.rs` (M2, docs/11-mvp-plan.md#m2--session-ownership-and-backpressure),
//! built on top of what this file exposes. The reader thread instead calls a
//! caller-supplied `on_output` closure synchronously, once per chunk read --
//! that closure is where session.rs's persist-then-advance-offset-then-fanout
//! logic (docs/03-pty-layer.md#reader-loop) will eventually live. **The
//! closure must never block**, for exactly the reason the reader loop itself
//! must never block (docs/03-pty-layer.md#the-rule) -- pty.rs cannot enforce
//! that, it is a contract on the caller.
//!
//! **Windows: the ConPTY startup handshake.** conhost.exe's `VtIo::StartIfNeeded`
//! writes a Device Status Report / cursor-position query (`ESC[6n`) to the pty
//! master as the very first bytes of any session, then blocks
//! `VtInputThread::DoReadInput`'s `ReadFile` on the input side waiting for the
//! matching CPR reply (`ESC[row;colR`) -- confirmed via WinDbg, unbounded (ran
//! 265+s with nothing else changing it), see
//! [W1](../../docs/15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows).
//! A real terminal emulator answers this automatically; `portable_pty`'s raw
//! `PtyPair` does not -- it does no VT parsing at all. Left unanswered, this
//! wedges conhost's own startup indefinitely, which is why a gracefully-exiting
//! child was never observed as exited: exit detection itself is downstream of
//! a handshake that never finished. `ConptyDsrProbe` below answers this one
//! startup query and then gets out of the way -- see its doc comment.

use std::io::{Read, Result as IoResult, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use portable_pty::{
    native_pty_system, Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize,
};

/// Bytes per chunk read from the pty master (docs/03-pty-layer.md#reader-loop).
const READ_BUFFER_SIZE: usize = 64 * 1024;
/// Capacity of the write-request channel. Writes are caller-paced (keystrokes,
/// pasted input), unlike reads, so a blocking bounded channel here is fine --
/// it is not the reader, and it is on its own thread, so a stuck child cannot
/// wedge `resize`/`terminate` through it (docs/03-pty-layer.md#thread-model,
/// [S3](../../docs/15-open-questions.md#s3--a-blocking-write-wedges-terminate)).
const WRITE_CHANNEL_CAPACITY: usize = 32;
/// Capacity of the control channel (`resize`/`terminate`). Bounded (rather
/// than `mpsc::channel`'s unbounded `Sender`) so `control_tx` is `Sync` and a
/// caller can hold `PtySession` behind a shared reference instead of a
/// `Mutex` -- see the `TerminalSession` trait doc below. Control traffic is
/// rare and short (a handful of resizes, one terminate), so this can't
/// realistically fill; even if it did, `send` would just block the caller
/// briefly, not deadlock -- the control thread only ever blocks waiting on
/// this same channel or a bounded `recv_timeout`, never on this send.
const CONTROL_CHANNEL_CAPACITY: usize = 8;

/// Bounded wait for the graceful signal before a hard kill
/// (docs/03-pty-layer.md#concrete-policy).
const GRACEFUL_WAIT: Duration = Duration::from_secs(5);
/// Bounded wait for the hard kill before giving up.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// Resize dimensions are clamped to this range; anything outside it is a
/// protocol error one layer up, not pty.rs's job to reject
/// (docs/03-pty-layer.md#resize) -- clamping here is just a correctness
/// backstop so a bad value can never reach `ResizePseudoConsole`/`TIOCSWINSZ`.
const SIZE_RANGE: std::ops::RangeInclusive<u16> = 1..=1000;

const STATE_RUNNING: u8 = 0;
const STATE_CLOSING: u8 = 1;
const STATE_EXITED: u8 = 2;

/// Application lifecycle policy on top of `portable_pty`'s platform
/// abstraction (docs/03-pty-layer.md#the-terminalsession-trait). If a caller
/// needs to know the platform, the abstraction has leaked and the fix
/// belongs here, not at the call site.
pub trait TerminalSession {
    /// Enqueues `bytes` onto the write channel (capacity
    /// `WRITE_CHANNEL_CAPACITY`); the writer thread sends them on. Normally
    /// returns immediately, but if the child never reads its pty
    /// ([S3](../../docs/15-open-questions.md#s3--a-blocking-write-wedges-terminate))
    /// and the channel is already full, this call blocks the caller until it
    /// drains or the session exits -- unlike `terminate()`, that bound is not
    /// enforced here. Takes `&self`, not `&mut self`: every method here only
    /// ever sends on a channel or touches an atomic, both already safe to
    /// call from a shared reference, and a caller wrapping `PtySession` in a
    /// `Mutex` just for this would recreate S3 one layer up -- a write stuck
    /// behind a full channel would hold that lock and wedge `resize`/
    /// `terminate` behind it too. A caller running this on an async runtime's
    /// own thread must still not call it inline; wrap it in `spawn_blocking`,
    /// same rule as the reader loop (docs/03-pty-layer.md#the-rule).
    fn write(&self, bytes: &[u8]) -> Result<()>;
    fn resize(&self, cols: u16, rows: u16) -> Result<()>;
    /// Blocks for up to `GRACEFUL_WAIT + KILL_WAIT` (~7s): the bounded wait
    /// is intrinsic to termination, not a detail the caller schedules
    /// separately (docs/03-pty-layer.md#concrete-policy). Idempotent -- a
    /// second call while already closing/exited is a no-op.
    fn terminate(&self) -> Result<()>;
}

/// Why a session ended up without a clean exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostReason {
    /// terminate()'s hard-kill step didn't produce an observed exit within
    /// `KILL_WAIT` (docs/03-pty-layer.md#concrete-policy step 5).
    KillTimeout,
    /// `child.wait()` itself returned an OS error rather than a status.
    WaitError,
}

/// Published exactly once: by the reaper thread when `child.wait()` returns
/// on its own, or by `terminate()`'s policy if it has to give up.
/// `RUNNING -> EXITED` fires directly off this -- independent of reader EOF,
/// which is a separate signal
/// ([S2](../../docs/15-open-questions.md#s2--eof-is-not-exit)).
#[derive(Debug, Clone)]
pub struct PtyExit {
    /// `None` only when `lost_reason` is set -- we gave up without ever
    /// observing a wait() result.
    pub status: Option<ExitStatus>,
    pub lost_reason: Option<LostReason>,
}

/// What to spawn. Argv array, never a concatenated shell string -- a shell is
/// only ever `program` because the caller deliberately chose it
/// (docs/03-pty-layer.md#spawn, docs/06-security.md).
pub struct SpawnSpec<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub cwd: &'a Path,
    /// Overrides layered onto the daemon's own environment, which
    /// `CommandBuilder` inherits by default. Do not pre-flatten the full
    /// daemon environment in here -- pass only explicit overrides
    /// (docs/03-pty-layer.md#spawn).
    pub env: &'a [(String, String)],
    pub cols: u16,
    pub rows: u16,
}

/// Handle plus the two independent one-shot signals a caller needs: the
/// child's eventual exit, and the pty master's eventual EOF. Never wait for
/// one to infer the other ([S2](../../docs/15-open-questions.md#s2--eof-is-not-exit)).
pub struct SpawnedSession {
    pub session: PtySession,
    pub exit_rx: Receiver<PtyExit>,
    pub eof_rx: Receiver<()>,
    /// The child's OS pid, for `GET`'s `pid` field
    /// (docs/04-api-protocol.md#get-apiv1sessions). `None` only on a platform
    /// where `portable_pty::Child::process_id()` itself returns `None`.
    pub pid: Option<u32>,
}

/// Spawns `spec` behind a fresh pty and starts the four session threads.
/// `on_output` runs synchronously on the reader thread for every chunk read
/// -- see the module-level "M1 scope boundary" note on why, and its
/// must-never-block requirement.
pub fn spawn(
    spec: SpawnSpec,
    on_output: impl FnMut(&[u8]) + Send + 'static,
) -> Result<SpawnedSession> {
    let system = native_pty_system();
    let pair = system
        .openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = CommandBuilder::new(spec.program);
    cmd.args(spec.args);
    cmd.cwd(spec.cwd);
    for (k, v) in spec.env {
        cmd.env(k, v);
    }

    let child = pair.slave.spawn_command(cmd).context("spawn_command")?;

    // The master only sees EOF once every slave-side handle is gone; on Unix
    // that includes ours. (docs/03-pty-layer.md#spawn)
    #[cfg(unix)]
    drop(pair.slave);

    let pid = child.process_id();
    let killer = child.clone_killer();

    let reader = pair.master.try_clone_reader().context("clone pty reader")?;
    let writer = pair.master.take_writer().context("take pty writer")?;

    let (write_tx, write_rx) = mpsc::sync_channel::<Vec<u8>>(WRITE_CHANNEL_CAPACITY);
    // Windows only: wrap the reader so the first bytes of the session pass
    // through a one-shot ConPTY startup-handshake responder before anything
    // else sees them -- see ConptyDsrProbe's doc comment and the module doc
    // above.
    #[cfg(windows)]
    let reader: Box<dyn Read + Send> = Box::new(ConptyDsrProbe::new(reader, write_tx.clone()));
    let (control_tx, control_rx) = mpsc::sync_channel::<ControlEvent>(CONTROL_CHANNEL_CAPACITY);
    let (exit_tx, exit_rx) = mpsc::sync_channel::<PtyExit>(1);
    let (eof_tx, eof_rx) = mpsc::sync_channel::<()>(1);
    let state = Arc::new(AtomicU8::new(STATE_RUNNING));

    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || reader_thread_main(reader, on_output, eof_tx))
        .context("spawning reader thread")?;

    std::thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || writer_thread_main(writer, write_rx))
        .context("spawning writer thread")?;

    std::thread::Builder::new()
        .name("pty-reaper".into())
        .spawn({
            let control_tx = control_tx.clone();
            move || reaper_thread_main(child, control_tx)
        })
        .context("spawning reaper thread")?;

    std::thread::Builder::new()
        .name("pty-control".into())
        .spawn({
            let state = Arc::clone(&state);
            move || control_thread_main(pair.master, pid, killer, control_rx, exit_tx, state)
        })
        .context("spawning control thread")?;

    Ok(SpawnedSession {
        session: PtySession {
            write_tx,
            control_tx,
            state,
        },
        exit_rx,
        eof_rx,
        pid,
    })
}

/// The caller-facing handle. Not `Clone` -- one owner drives the session's
/// lifecycle; sharing it is `session.rs`'s job (M2). `TerminalSession`'s
/// `&self` methods mean that owner does not need a `Mutex` to do it -- the
/// session directory (`SessionManager`) just holds this behind its own
/// `Arc<Session>`.
pub struct PtySession {
    write_tx: SyncSender<Vec<u8>>,
    control_tx: SyncSender<ControlEvent>,
    state: Arc<AtomicU8>,
}

impl TerminalSession for PtySession {
    fn write(&self, bytes: &[u8]) -> Result<()> {
        if self.state.load(Ordering::SeqCst) != STATE_RUNNING {
            bail!("session is closing or has exited; input rejected");
        }
        self.write_tx
            .send(bytes.to_vec())
            .context("pty writer thread is gone")
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let (cols, rows) = (
            cols.clamp(*SIZE_RANGE.start(), *SIZE_RANGE.end()),
            rows.clamp(*SIZE_RANGE.start(), *SIZE_RANGE.end()),
        );
        self.control_tx
            .send(ControlEvent::Resize { cols, rows })
            .context("pty control thread is gone")
    }

    fn terminate(&self) -> Result<()> {
        if self
            .state
            .compare_exchange(
                STATE_RUNNING,
                STATE_CLOSING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return Ok(()); // already closing or exited -- idempotent
        }

        let (reply_tx, reply_rx) = mpsc::sync_channel::<PtyExit>(1);
        if self
            .control_tx
            .send(ControlEvent::Terminate { reply_tx })
            .is_err()
        {
            // The control thread is already gone -- it must have processed a
            // spontaneous ChildExited (and dropped control_rx) in the window
            // between our compare_exchange above and this send(). The session
            // is exited either way, so this is the same race the comment
            // below covers, just caught one step earlier -- treat as success.
            return Ok(());
        }

        // Blocks for up to GRACEFUL_WAIT + KILL_WAIT -- that bounded wait is
        // what "terminate" means here (docs/03-pty-layer.md#concrete-policy).
        // A broken reply channel means the control thread finished via a
        // race with a spontaneous ChildExited it processed first -- the
        // session is exited either way, so treat that as success too.
        let _ = reply_rx.recv();
        Ok(())
    }
}

enum ControlEvent {
    Resize { cols: u16, rows: u16 },
    Terminate { reply_tx: SyncSender<PtyExit> },
    ChildExited(IoResult<ExitStatus>),
}

/// Windows only: answers conhost's one-time ConPTY startup DSR (cursor
/// position) query so `VtIo::StartIfNeeded` can finish initializing --
/// see the module doc's "Windows: the ConPTY startup handshake" note and
/// [W1](../../docs/15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows)
/// for the WinDbg evidence and the spike (`spike/src/bin/s9_dsr_reply.rs`)
/// that confirmed this specific fix: with a reply written back, `wait()`
/// returns in single-digit milliseconds instead of hanging indefinitely.
///
/// Scans only the first `PROBE_BYTE_BUDGET` bytes ever read from the master
/// for `ESC[6n`; once it either finds and answers that one query, or gives
/// up without finding it, it stops looking and every later byte -- for the
/// rest of the session -- passes through completely untouched. That budget
/// matters for correctness, not just cost: a real interactive program can
/// itself send `ESC[6n` later, expecting a genuine cursor position back from
/// whatever terminal the session ends up attached to, and this probe must
/// never intercept that. It only targets the handshake burst, which -- per
/// every observed trace -- is the literal first thing conhost ever writes,
/// before the child's own program has had any chance to produce output of
/// its own (the child can't write to a console that hasn't finished
/// starting). If some future/older Windows build doesn't emit this burst at
/// all, the budget is exhausted harmlessly and this is a no-op.
#[cfg(windows)]
struct ConptyDsrProbe {
    inner: Box<dyn Read + Send>,
    write_tx: SyncSender<Vec<u8>>,
    /// Bytes already pulled from `inner` while scanning but not yet handed
    /// back to the caller -- either genuine output that arrived before/after
    /// the match, or (once budget-exhausted with no match) everything read
    /// during scanning, still owed to the caller untouched.
    pending: Vec<u8>,
    scanned: usize,
    done: bool,
}

#[cfg(windows)]
impl ConptyDsrProbe {
    const QUERY: &'static [u8] = b"\x1b[6n";
    /// A fixed, unverified reply -- portable_pty exposes no way to ask what
    /// cursor position it actually set, so this claims row 1, col 1
    /// (`ESC[1;1R`). conhost only needs *a* well-formed reply to unblock its
    /// startup `ReadFile`; nothing observed in the WinDbg trace or the exit
    /// status of the fixture below depends on this being accurate.
    const REPLY: &'static [u8] = b"\x1b[1;1R";
    /// Generous relative to what's ever been observed (the query arrives
    /// alone, within single-digit ms, as the very first read) -- this is a
    /// give-up bound for "future/different conhost build behaves
    /// differently", not a latency budget.
    const PROBE_BYTE_BUDGET: usize = 4096;

    fn new(inner: Box<dyn Read + Send>, write_tx: SyncSender<Vec<u8>>) -> Self {
        Self {
            inner,
            write_tx,
            pending: Vec::new(),
            scanned: 0,
            done: false,
        }
    }
}

#[cfg(windows)]
impl Read for ConptyDsrProbe {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        loop {
            if !self.pending.is_empty() {
                let n = self.pending.len().min(buf.len());
                buf[..n].copy_from_slice(&self.pending[..n]);
                self.pending.drain(..n);
                return Ok(n);
            }
            if self.done {
                return self.inner.read(buf);
            }

            let mut tmp = [0u8; 256];
            let n = self.inner.read(&mut tmp)?;
            if n == 0 {
                return Ok(0); // EOF before the handshake ever showed up -- give up quietly
            }
            self.pending.extend_from_slice(&tmp[..n]);
            self.scanned += n;

            if let Some(pos) = self
                .pending
                .windows(Self::QUERY.len())
                .position(|w| w == Self::QUERY)
            {
                let _ = self.write_tx.send(Self::REPLY.to_vec());
                self.pending.drain(pos..pos + Self::QUERY.len());
                self.done = true;
            } else if self.scanned >= Self::PROBE_BYTE_BUDGET {
                self.done = true;
            }
            // Loop back around: drain whatever's in `pending` into `buf`.
        }
    }
}

fn reader_thread_main(
    mut reader: Box<dyn Read + Send>,
    mut on_output: impl FnMut(&[u8]) + Send,
    eof_tx: SyncSender<()>,
) {
    let mut buf = [0u8; READ_BUFFER_SIZE];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => on_output(&buf[..n]),
            Err(_) => break,
        }
    }
    let _ = eof_tx.try_send(());
}

fn writer_thread_main(mut writer: Box<dyn Write + Send>, write_rx: Receiver<Vec<u8>>) {
    for chunk in write_rx {
        // write_all can block indefinitely if the child never reads
        // ([S3](../../docs/15-open-questions.md#s3--a-blocking-write-wedges-terminate)).
        // That is contained to this thread by design -- it never touches
        // resize/terminate. A permanently stuck write leaks this thread
        // until the session (and `write_tx`) is dropped; accepted for M1,
        // not solved here.
        if writer.write_all(&chunk).is_err() {
            break;
        }
    }
}

fn reaper_thread_main(
    mut child: Box<dyn Child + Send + Sync>,
    control_tx: SyncSender<ControlEvent>,
) {
    let result = child.wait();
    // Ignored if the control thread already finished (e.g. gave up on a
    // hard-kill timeout) and dropped its receiver -- its result stands.
    let _ = control_tx.send(ControlEvent::ChildExited(result));
}

#[allow(unused_mut, unused_variables)]
fn control_thread_main(
    master: Box<dyn MasterPty + Send>,
    pid: Option<u32>,
    mut killer: Box<dyn ChildKiller + Send + Sync>,
    control_rx: Receiver<ControlEvent>,
    exit_tx: SyncSender<PtyExit>,
    state: Arc<AtomicU8>,
) {
    let mut master = Some(master);

    loop {
        let event = match control_rx.recv() {
            Ok(event) => event,
            Err(_) => return, // both session and reaper senders gone
        };

        match event {
            ControlEvent::Resize { cols, rows } => {
                if state.load(Ordering::SeqCst) == STATE_RUNNING {
                    if let Some(master) = &master {
                        let _ = master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                }
            }

            ControlEvent::ChildExited(result) => {
                state.store(STATE_EXITED, Ordering::SeqCst);
                let _ = exit_tx.try_send(pty_exit_from_wait(result));
                return; // post-reap cleanup: `master` drops with this frame
            }

            ControlEvent::Terminate { reply_tx } => {
                // Step 2: graceful signal (docs/03-pty-layer.md#concrete-policy).
                #[cfg(unix)]
                if let Some(pid) = pid {
                    // SAFETY: killpg/kill with a pid we own (this session's
                    // child) and signals that do not affect memory safety.
                    unsafe {
                        libc::killpg(pid as libc::pid_t, libc::SIGHUP);
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
                #[cfg(windows)]
                {
                    // ClosePseudoConsole via dropping the master handle.
                    // Unverified on real Windows --
                    // [S4](../../docs/15-open-questions.md#s4--does-dropping-the-master-close-the-pseudoconsole).
                    drop(master.take());
                }

                let exit = match wait_for_child_exited(&control_rx, Instant::now() + GRACEFUL_WAIT)
                {
                    Some(result) => pty_exit_from_wait(result),
                    None => {
                        // Step 4: hard kill. portable-pty's kill() is a hard
                        // kill on both platforms (SIGKILL / TerminateProcess).
                        let _ = killer.kill();
                        match wait_for_child_exited(&control_rx, Instant::now() + KILL_WAIT) {
                            Some(result) => pty_exit_from_wait(result),
                            None => PtyExit {
                                status: None,
                                lost_reason: Some(LostReason::KillTimeout),
                            },
                        }
                    }
                };

                state.store(STATE_EXITED, Ordering::SeqCst);
                let _ = exit_tx.try_send(exit.clone());
                let _ = reply_tx.send(exit);
                return; // post-reap cleanup: `master` drops with this frame
            }
        }
    }
}

/// Services (and mostly ignores) other control events while waiting
/// specifically for `ChildExited`, up to `deadline`. Returns `None` on
/// timeout.
fn wait_for_child_exited(
    control_rx: &Receiver<ControlEvent>,
    deadline: Instant,
) -> Option<IoResult<ExitStatus>> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match control_rx.recv_timeout(remaining) {
            Ok(ControlEvent::ChildExited(result)) => return Some(result),
            Ok(ControlEvent::Resize { .. }) => continue, // ignored while closing
            Ok(ControlEvent::Terminate { reply_tx }) => {
                // A second terminate() while already closing shouldn't reach
                // here (the caller-side compare_exchange makes it a no-op
                // before it's ever sent) -- but if it somehow does, dropping
                // reply_tx makes that caller's recv() return promptly rather
                // than hang, and this wait continues unaffected.
                drop(reply_tx);
                continue;
            }
            Err(RecvTimeoutError::Timeout) => return None,
            Err(RecvTimeoutError::Disconnected) => return None, // reaper gone without reporting
        }
    }
}

fn pty_exit_from_wait(result: IoResult<ExitStatus>) -> PtyExit {
    match result {
        Ok(status) => PtyExit {
            status: Some(status),
            lost_reason: None,
        },
        Err(_) => PtyExit {
            status: None,
            lost_reason: Some(LostReason::WaitError),
        },
    }
}
