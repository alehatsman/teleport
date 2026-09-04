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
//! **Known Windows gap:** a ConPTY child that exits gracefully (not killed)
//! is not currently observed as exited by this file's reaper thread, and the
//! pty master never sees EOF either -- see
//! [W1](../../docs/15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows).
//! `terminate()` still resolves correctly on Windows (its hard-kill step uses
//! `TerminateProcess`, confirmed working), it just always takes the full
//! bounded-wait-then-kill path rather than reaping early. A child that exits
//! on its own, with nobody calling `terminate()`, will not currently surface
//! as exited on Windows at all. Tracked, not fixed here.

use std::io::{Read, Result as IoResult, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
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
    /// enforced here. A caller sharing a `PtySession` behind a `Mutex` (M1
    /// scope note above) must not call this while holding that lock on an
    /// async runtime's own thread; wrap it in `spawn_blocking`, same rule as
    /// the reader loop (docs/03-pty-layer.md#the-rule).
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
    /// Blocks for up to `GRACEFUL_WAIT + KILL_WAIT` (~7s): the bounded wait
    /// is intrinsic to termination, not a detail the caller schedules
    /// separately (docs/03-pty-layer.md#concrete-policy). Idempotent -- a
    /// second call while already closing/exited is a no-op.
    fn terminate(&mut self) -> Result<()>;
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
    let (control_tx, control_rx) = mpsc::channel::<ControlEvent>();
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
        session: PtySession { write_tx, control_tx, state },
        exit_rx,
        eof_rx,
    })
}

/// The caller-facing handle. Not `Clone` -- one owner drives the session's
/// lifecycle; sharing it is `session.rs`'s job (M2), e.g. behind a `Mutex`
/// alongside the subscriber registry.
pub struct PtySession {
    write_tx: SyncSender<Vec<u8>>,
    control_tx: Sender<ControlEvent>,
    state: Arc<AtomicU8>,
}

impl TerminalSession for PtySession {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.state.load(Ordering::SeqCst) != STATE_RUNNING {
            bail!("session is closing or has exited; input rejected");
        }
        self.write_tx
            .send(bytes.to_vec())
            .context("pty writer thread is gone")
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        let (cols, rows) = (cols.clamp(*SIZE_RANGE.start(), *SIZE_RANGE.end()), rows.clamp(*SIZE_RANGE.start(), *SIZE_RANGE.end()));
        self.control_tx
            .send(ControlEvent::Resize { cols, rows })
            .context("pty control thread is gone")
    }

    fn terminate(&mut self) -> Result<()> {
        if self
            .state
            .compare_exchange(STATE_RUNNING, STATE_CLOSING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(()); // already closing or exited -- idempotent
        }

        let (reply_tx, reply_rx) = mpsc::sync_channel::<PtyExit>(1);
        if self.control_tx.send(ControlEvent::Terminate { reply_tx }).is_err() {
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

fn reaper_thread_main(mut child: Box<dyn Child + Send + Sync>, control_tx: Sender<ControlEvent>) {
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
                        let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
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

                let exit = match wait_for_child_exited(&control_rx, Instant::now() + GRACEFUL_WAIT) {
                    Some(result) => pty_exit_from_wait(result),
                    None => {
                        // Step 4: hard kill. portable-pty's kill() is a hard
                        // kill on both platforms (SIGKILL / TerminateProcess).
                        let _ = killer.kill();
                        match wait_for_child_exited(&control_rx, Instant::now() + KILL_WAIT) {
                            Some(result) => pty_exit_from_wait(result),
                            None => PtyExit { status: None, lost_reason: Some(LostReason::KillTimeout) },
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
        Ok(status) => PtyExit { status: Some(status), lost_reason: None },
        Err(_) => PtyExit { status: None, lost_reason: Some(LostReason::WaitError) },
    }
}
