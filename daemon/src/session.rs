//! Session ownership and backpressure on top of `pty.rs`.
//!
//! See docs/11-mvp-plan.md#m2--session-ownership-and-backpressure and
//! docs/01-architecture.md#session-manager-shape for the design this
//! implements, and docs/03-pty-layer.md#backpressure for the fan-out and
//! queue-bound rules below.
//!
//! **M3 update:** the offset counter is no longer a field here -- it moved
//! into `log.rs`'s `OutputLog`, which this module keeps inside the same
//! `Fanout` mutex. That is what makes docs/03-pty-layer.md#reader-loop's
//! ordering -- **persist, then advance the offset, then fan out** -- one
//! call (`OutputLog::append`) instead of three steps a later edit could
//! reorder. The same mutex closes the attach race: the round of
//! [`Replay::next_round`] that registers the subscriber *also* reads the
//! replay boundary `N` under one acquisition, so every chunk that subscriber
//! ever receives starts at or after `N` (docs/04-api-protocol.md#attach-race).
//!
//! **D1 (resolved before M4):** registering the subscriber and *then* writing
//! the replay spends the 8 MiB queue bound twice -- the subscriber buffers
//! live output for the whole duration of a replay that is itself capped at
//! 8 MiB, overflows, and is disconnected as a slow consumer before it ever
//! goes live. [`Session::attach`] therefore does not register at all: it
//! returns a [`Replay`] that hands history back in bounded rounds, and only
//! the round that finds the remaining gap small enough registers
//! (docs/04-api-protocol.md#catch-up--register-late-not-early).
//!
//! **Still out of scope:** bounded attach (`tail`, `max_replay_bytes`,
//! `truncated`) and the WS close code that distinguishes a slow consumer
//! from a dropped one are M4; SQLite metadata, `session_events` and restart
//! recovery are M7 -- the [`LogEvent`]s the log hands back here are traced,
//! not stored.
//!
//! `exit_rx`/`eof_rx` from `pty::spawn` are intentionally left unconsumed --
//! dropping them is safe (the sender threads see a closed channel and move
//! on, they never block on it) and wiring session state / exit events to
//! them is M4/M7 API-surface work, not backpressure.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use anyhow::Result;
use tokio::sync::{mpsc, Semaphore};
use tracing::warn;
use ulid::Ulid;

use crate::log::{LogEvent, LogLimits, LogReader, LogSyncer, OutputLog, SyncHandle};
use crate::pty::{self, PtySession, SpawnSpec, TerminalSession};

/// Queue bound per subscriber: whichever trips first
/// (docs/03-pty-layer.md#backpressure).
const MAX_QUEUE_CHUNKS: usize = 256;
const MAX_QUEUE_BYTES: usize = 8 * 1024 * 1024;

/// The most history a subscriber may still owe its client at the moment it is
/// registered. One eighth of the queue bound, so seven eighths stay free for
/// live output while the client writes that last stretch out -- which is the
/// headroom D1 says the design was missing
/// (docs/04-api-protocol.md#catch-up--register-late-not-early).
pub const LIVE_GAP_BYTES: u64 = MAX_QUEUE_BYTES as u64 / 8;

/// Bytes one catch-up round reads off the fan-out mutex. Doubles as the
/// pacing unit the client paints at, instead of one write of the whole log
/// (docs/15-open-questions.md#n3--xtermjs-write-pacing-on-reattach).
pub const REPLAY_ROUND_BYTES: u64 = LIVE_GAP_BYTES;

/// Consecutive rounds the gap may fail to shrink before the daemon stops
/// trying to catch this client up. A client that loses ground four rounds
/// running is not going to survive the live stream either; clamping and
/// reporting the hole beats an unbounded reconnect loop.
const MAX_STALLED_ROUNDS: u32 = 4;

/// Absolute ceiling on catch-up rounds, independent of `MAX_STALLED_ROUNDS`.
/// A client whose throughput barely exceeds the producer's shrinks the gap
/// by a few bytes every round -- resetting `stalled_rounds` to 0 each time
/// -- and can defer registration indefinitely, re-acquiring the fan-out
/// mutex every round with no bound in sight. Sized at four full backlogs of
/// the default log cap so a legitimate one-shot catch-up of the largest
/// on-disk log never trips it; a client that still hasn't converged after
/// this many rounds gets the same clamp-and-report-the-hole treatment a
/// stalled one does.
const MAX_CATCHUP_ROUNDS: u32 = 4 * (crate::log::DEFAULT_LOG_MAX_BYTES / REPLAY_ROUND_BYTES) as u32;

/// Whether this round should register the subscriber and go live. A pure
/// function of the three catch-up floors so each can be exercised without
/// driving a `Replay` through real rounds: the gap already fits (the common
/// case), the client has stopped gaining ground at all
/// (`MAX_STALLED_ROUNDS`), or it hasn't converged in any number of rounds
/// (`MAX_CATCHUP_ROUNDS`) even though it kept gaining a little each time.
fn should_register(gap: u64, stalled_rounds: u32, total_rounds: u32) -> bool {
    gap <= LIVE_GAP_BYTES || stalled_rounds >= MAX_STALLED_ROUNDS || total_rounds >= MAX_CATCHUP_ROUNDS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(Ulid);

impl SessionId {
    fn new() -> Self {
        Self(Ulid::new())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One chunk of output, tagged with the offset of its first byte -- the
/// contract a subscriber needs to reconnect without a gap or duplicate later
/// (M3/M4). `bytes` is `Arc<[u8]>` so fanning out to N subscribers is N
/// clones of a refcount, not N copies of the chunk.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub offset: u64,
    pub bytes: Arc<[u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SubscriberId(u64);

struct SubscriberSlot {
    id: SubscriberId,
    tx: mpsc::Sender<Chunk>,
    /// Byte budget for this subscriber's queue, shared with the matching
    /// `Subscription` so the receive side can return bytes as it drains
    /// them. Starts at `MAX_QUEUE_BYTES` permits; `publish` acquires a
    /// chunk's length in permits *before* the chunk is made visible via
    /// `try_send`, so a chunk can never reach `Subscription::recv` before
    /// its bytes are already accounted for -- a plain counter bumped
    /// *after* `try_send` can't promise that (the receiver can drain and
    /// give bytes back first). This is the byte half of the bound; the
    /// channel's own capacity (`MAX_QUEUE_CHUNKS`) is the count half -- see
    /// the module doc.
    budget: Arc<Semaphore>,
}

/// The `{output log, subscribers}` pair, guarded by one short-held mutex
/// (docs/03-pty-layer.md#reader-loop: "steps 2-4 happen under one short
/// mutex that also guards subscriber registration"). The log carries
/// `next_offset`, so appending to the file and advancing the offset cannot
/// drift apart, and neither can drift from subscriber registration.
struct Fanout {
    log: OutputLog,
    subscribers: Vec<SubscriberSlot>,
    next_subscriber_id: u64,
}

impl Fanout {
    fn new(log: OutputLog) -> Self {
        Self { log, subscribers: Vec::new(), next_subscriber_id: 0 }
    }

    /// Runs on the PTY reader thread via the `on_output` closure passed to
    /// `pty::spawn`. Must never block: the append is a `write_all` into the
    /// page cache -- the periodic `fsync` is `LogSyncer`'s thread, never this
    /// one -- every send is `try_send`, budget acquisition is `try_acquire`,
    /// and a subscriber that would exceed either bound is dropped instead of
    /// waited on (docs/03-pty-layer.md#the-rule).
    ///
    /// Returns whatever the log wants recorded; the caller traces it off the
    /// hot path rather than this method reaching for a logger under the lock.
    fn publish(&mut self, bytes: &[u8]) -> Vec<LogEvent> {
        let appended = self.log.append(bytes);
        let start = appended.start;

        if self.subscribers.is_empty() {
            return appended.events; // docs/11-mvp-plan.md#m2: zero subscribers is fine, indefinitely.
        }

        let payload: Arc<[u8]> = Arc::from(bytes);
        self.subscribers.retain(|sub| {
            // Chunks are bounded by pty.rs's READ_BUFFER_SIZE (64 KiB), so
            // this always fits u32; MAX_QUEUE_BYTES itself fits comfortably
            // under Semaphore's permit ceiling.
            let len = payload.len() as u32;
            let permit = match sub.budget.try_acquire_many(len) {
                Ok(permit) => permit,
                Err(_) => return false, // byte bound tripped -- disconnect, don't wait.
            };
            let chunk = Chunk { offset: start, bytes: Arc::clone(&payload) };
            match sub.tx.try_send(chunk) {
                Ok(()) => {
                    permit.forget(); // returned by Subscription::recv once this chunk is drained
                    true
                }
                // Full (count bound tripped) or the Subscription was dropped.
                // `permit` drops here too, returning the budget it reserved.
                Err(_) => false,
            }
        });

        appended.events
    }

    /// Registers a subscriber slot. Split out of `Session::subscribe` so
    /// [`Session::attach`] can register *and* read the replay boundary under
    /// one acquisition of the lock -- that atomicity is the whole attach-race
    /// fix (docs/04-api-protocol.md#attach-race).
    fn register(&mut self, fanout: &Arc<Mutex<Fanout>>) -> Subscription {
        let (tx, rx) = mpsc::channel(MAX_QUEUE_CHUNKS);
        let budget = Arc::new(Semaphore::new(MAX_QUEUE_BYTES));

        let id = SubscriberId(self.next_subscriber_id);
        self.next_subscriber_id += 1;
        self.subscribers.push(SubscriberSlot { id, tx, budget: Arc::clone(&budget) });

        Subscription { id, rx, budget, fanout: Arc::clone(fanout) }
    }
}

/// A live subscription to a session's output. Dropping it unregisters from
/// the session's `Fanout` -- an idle session does not accumulate dead slots
/// waiting for the next chunk to notice them.
pub struct Subscription {
    id: SubscriberId,
    rx: mpsc::Receiver<Chunk>,
    budget: Arc<Semaphore>,
    fanout: Arc<Mutex<Fanout>>,
}

impl Subscription {
    /// Waits for the next chunk. `None` means the session's fan-out closure
    /// (and therefore the reader thread's handle to it) is gone.
    pub async fn recv(&mut self) -> Option<Chunk> {
        let chunk = self.rx.recv().await?;
        self.budget.add_permits(chunk.bytes.len());
        Some(chunk)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut fanout = self.fanout.lock().unwrap();
        fanout.subscribers.retain(|s| s.id != self.id);
    }
}

/// The catch-up half of an attach: a read handle plus a cursor, not yet
/// registered with the fan-out.
///
/// The three public fields are exactly what `ready` needs
/// (docs/04-api-protocol.md#control-messages) and they are available before
/// any history has been written, which is why `ready` can still be the first
/// frame. Drive it with [`next_round`](Self::next_round) until it hands back
/// an [`Attach`].
pub struct Replay {
    /// Where replay actually starts -- `ready.replay_from`. The requested
    /// offset, except for a client attaching past a cap: there it is
    /// `next_offset` and nothing is replayed
    /// (docs/05-persistence.md#size-cap).
    pub replay_from: u64,
    /// The boundary captured when the client attached -- `ready.next_offset`.
    /// **Not** necessarily the offset the subscriber is finally registered
    /// at; a client that consumes up to here holds the session's full history
    /// as of the moment it attached, which is all `ready` ever promised
    /// (docs/04-api-protocol.md#catch-up--register-late-not-early).
    pub next_offset: u64,
    /// The cap as of the attach, for `ready`. A cap that lands *during*
    /// catch-up shows up on [`Attach::log_capped_at`] instead; either way the
    /// client sees the hole it creates as a jump in the offset prefix
    /// (docs/04-api-protocol.md#offsets-are-the-replay-index).
    pub log_capped_at: Option<u64>,

    fanout: Arc<Mutex<Fanout>>,
    reader: LogReader,
    /// Next byte still owed to the client.
    cursor: u64,
    /// The gap as of the previous round, for convergence detection. Starts at
    /// `u64::MAX` so the first round can never count as a stall.
    previous_gap: u64,
    stalled_rounds: u32,
    /// Rounds run so far, counted whether or not the gap shrank. A client
    /// that gains a few bytes of ground every round resets `stalled_rounds`
    /// forever without ever registering -- see `MAX_CATCHUP_ROUNDS`.
    total_rounds: u32,
}

/// One step of a catch-up loop. The `History` variant carries the rest of the
/// [`Replay`] rather than borrowing it, so a caller cannot pump a replay that
/// has already gone live and register a second subscriber by accident.
pub enum ReplayStep {
    /// A bounded stretch of history. Write it to the client, then call
    /// [`Replay::next_round`] on `replay` again.
    History { offset: u64, bytes: Vec<u8>, replay: Replay },
    /// The gap closed: the subscriber is registered and the handover is set
    /// up. Write [`Attach::replay`] first, then stream the subscription.
    Live(Attach),
}

/// A subscriber registered at the replay boundary, with the last stretch of
/// history it still owes. Write `replay` (starting at `replay_from`), then
/// every chunk from `subscription`; the two meet exactly once -- no gap, no
/// duplicate.
pub struct Attach {
    /// Where the final stretch of replay starts.
    pub replay_from: u64,
    /// The final stretch of history, read after registration and bounded by
    /// `LIVE_GAP_BYTES`. Empty for a client that was already at the boundary,
    /// or one attaching past a cap.
    pub replay: Vec<u8>,
    /// `N` -- the boundary. Every chunk from `subscription` starts here or
    /// later, guaranteed by the single lock the registering round takes.
    pub next_offset: u64,
    pub log_capped_at: Option<u64>,
    /// False when the catch-up loop gave up: the client kept losing ground,
    /// so `replay_from` was moved forward and the bytes behind it were
    /// dropped. M4 has nothing extra to send -- the hole is visible as a jump
    /// in the offset prefix, which clients must already handle for the
    /// log-cap case (docs/04-api-protocol.md#offsets-are-the-replay-index).
    pub caught_up: bool,
    pub subscription: Subscription,
}

impl Attach {
    /// One past the last replayed byte.
    pub fn replay_to(&self) -> u64 {
        self.replay_from + self.replay.len() as u64
    }
}

impl Replay {
    /// Advances the catch-up loop by one round.
    ///
    /// Either hands back a bounded stretch of history for the caller to write
    /// -- read *off* the fan-out mutex, so a slow client never holds up the
    /// PTY -- or, once the remaining gap fits comfortably inside a
    /// subscriber's queue, registers and returns the live handover.
    ///
    /// The caller must write each `History` round to its client **before**
    /// asking for the next one. That is what makes the gap a measurement of
    /// whether the client is outrunning the producer, and therefore what
    /// makes the loop converge (docs/04-api-protocol.md#catch-up--register-late-not-early).
    pub fn next_round(mut self) -> Result<ReplayStep, AttachError> {
        // Everything under the lock is arithmetic plus, on the last round, a
        // registration: no file I/O, same short hold the reader thread takes.
        let (next_offset, end, log_capped_at, subscription) = {
            let mut fanout = self.fanout.lock().unwrap();
            let next_offset = fanout.log.next_offset();
            let end = next_offset.min(fanout.log.readable_end());
            let log_capped_at = fanout.log.log_capped_at();

            let gap = end.saturating_sub(self.cursor);
            if gap >= self.previous_gap {
                self.stalled_rounds += 1;
            } else {
                self.stalled_rounds = 0;
            }
            self.previous_gap = gap;
            self.total_rounds += 1;

            let register = should_register(gap, self.stalled_rounds, self.total_rounds);
            let subscription = register.then(|| fanout.register(&self.fanout));
            (next_offset, end, log_capped_at, subscription)
        };

        let Some(subscription) = subscription else {
            let to = (self.cursor + REPLAY_ROUND_BYTES).min(end);
            let offset = self.cursor;
            let bytes = self.reader.read_range(offset, to).map_err(AttachError::Read)?;
            // Advance by what was actually read, not by what was asked for: a
            // short read must not leave a hole the client is never told about.
            self.cursor += bytes.len() as u64;
            return Ok(ReplayStep::History { offset, bytes, replay: self });
        };

        let mut replay_from = self.cursor;
        let mut caught_up = true;
        if replay_from >= end {
            // Nothing left, or everything left is behind a cap. Either way the
            // stream resumes at the boundary rather than at a file position
            // that no longer means what the client thinks
            // (docs/05-persistence.md#size-cap).
            replay_from = next_offset;
        } else if end - replay_from > LIVE_GAP_BYTES {
            // Stalled: this client cannot be caught up. Serve it the freshest
            // `LIVE_GAP_BYTES` and leave a reported hole behind them, rather
            // than a queue it is certain to overflow.
            replay_from = end - LIVE_GAP_BYTES;
            caught_up = false;
        }

        // Read off the lock. The subscriber is already registered, so these
        // bytes and the first queued chunk are contiguous by construction:
        // this range ends at or before `end <= next_offset`, and every chunk
        // that subscription will ever see starts at or after `next_offset`.
        let replay = self.reader.read_range(replay_from, end).map_err(AttachError::Read)?;

        Ok(ReplayStep::Live(Attach {
            replay_from,
            replay,
            next_offset,
            log_capped_at,
            caught_up,
            subscription,
        }))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// The client holds an offset the daemon never handed out -- a purged
    /// log, or a stale client after a `lost` session. M4 renders this as the
    /// `offset_ahead` error frame (docs/04-api-protocol.md#attach-race).
    #[error("requested offset {requested} is ahead of next_offset {next_offset}")]
    OffsetAhead { requested: u64, next_offset: u64 },
    /// `Session::attach`'s own `fanout.log.reader()` call failed before any
    /// round ever ran.
    #[error("opening the log for replay: {0}")]
    Open(std::io::Error),
    /// A `read_range` call failed partway through catch-up -- history-round
    /// or the final post-registration stretch alike. Distinct from `Open` so
    /// an incident doesn't read a mid-walk read failure as though the
    /// initial open never succeeded (issue #1 finding 4).
    #[error("reading a replay range: {0}")]
    Read(std::io::Error),
}

type SessionDirectory = Mutex<HashMap<SessionId, Arc<Session>>>;

/// One session: a PTY primitive plus the fan-out state layered on top of it.
/// No lock around `pty` -- `TerminalSession`'s `&self` methods (see
/// docs/03-pty-layer.md#the-terminalsession-trait) already let `write`,
/// `resize` and `terminate` run independently of each other, each blocking
/// (if at all) only as long as its own dedicated pty.rs thread does. Wrapping
/// them in a shared `Mutex` here would serialize them again and reintroduce
/// S3 one layer up: a write stuck behind a full channel would hold the lock
/// and wedge `terminate` behind it.
pub struct Session {
    pub id: SessionId,
    pty: PtySession,
    fanout: Arc<Mutex<Fanout>>,
    /// Back-link so `terminate()` can remove this session from the directory
    /// itself, rather than relying on every caller to remember to. `Weak` so
    /// this isn't a reference cycle -- the directory's `Arc<Session>` is the
    /// only strong owner.
    directory: Weak<SessionDirectory>,
    /// Flushes the log without touching `fanout`. Deliberately *not* reached
    /// through the lock: an `fsync` held under the mutex the reader thread
    /// takes would stall the PTY behind disk latency, which is the whole
    /// thing docs/05-persistence.md#output-log forbids.
    sync: SyncHandle,
}

impl Session {
    /// Registers a new subscriber and returns its receive end, with no
    /// replay. Bounded to `MAX_QUEUE_CHUNKS` chunks / `MAX_QUEUE_BYTES`
    /// bytes, whichever trips first (docs/03-pty-layer.md#backpressure); a
    /// subscriber that falls behind is disconnected (`recv()` returns
    /// `None`), never blocks the reader.
    ///
    /// Use [`attach`](Self::attach) for anything that needs history -- this
    /// one has no defined starting offset, so what happened before the call
    /// is simply lost to that subscriber.
    pub fn subscribe(&self) -> Subscription {
        let mut fanout = self.fanout.lock().unwrap();
        fanout.register(&self.fanout)
    }

    /// Begins an attach: opens a read handle and establishes the replay
    /// boundary, **without** registering a subscriber.
    ///
    /// The returned [`Replay`] carries everything `ready` needs, so the
    /// caller can send that frame immediately and then drive
    /// [`Replay::next_round`] to the live handover. Registration happens in
    /// the round that finds the remaining gap small enough -- under the same
    /// lock that reads `N`, which is what keeps the attach race closed
    /// (docs/04-api-protocol.md#attach-race), and late enough that the
    /// subscriber's queue is never asked to hold history and live output at
    /// the same time (docs/04-api-protocol.md#catch-up--register-late-not-early).
    /// Opening the read handle happens under the lock too: cheap, and it
    /// saves the caller reasoning about the file moving. Reading the bytes
    /// does not -- that is `Replay`'s job, off the lock, one bounded round at
    /// a time.
    ///
    /// Replay here is unbounded by design; `tail` / `max_replay_bytes` are
    /// M4's (docs/04-api-protocol.md#bounded-attach) and narrow `from` before
    /// it reaches this call. Keeping the bound above this line is what lets a
    /// VT state snapshot replace a byte range later without a protocol
    /// change.
    pub fn attach(&self, from: u64) -> Result<Replay, AttachError> {
        let fanout = self.fanout.lock().unwrap();

        let next_offset = fanout.log.next_offset();
        if from > next_offset {
            return Err(AttachError::OffsetAhead { requested: from, next_offset });
        }
        let log_capped_at = fanout.log.log_capped_at();
        let readable_end = fanout.log.readable_end();
        let reader = fanout.log.reader().map_err(AttachError::Open)?;
        drop(fanout);

        // Bytes between a cap and `next_offset` were streamed live and are
        // gone. A client asking for them gets no replay and is told where the
        // stream resumes, rather than being served whatever happens to sit at
        // that file position (docs/05-persistence.md#size-cap).
        let replay_from =
            if from >= next_offset.min(readable_end) { next_offset } else { from };

        Ok(Replay {
            replay_from,
            next_offset,
            log_capped_at,
            fanout: Arc::clone(&self.fanout),
            reader,
            cursor: replay_from,
            previous_gap: u64::MAX,
            stalled_rounds: 0,
            total_rounds: 0,
        })
    }

    /// The authoritative output offset: bytes below it have been handed out,
    /// bytes at or above it do not exist yet.
    pub fn next_offset(&self) -> u64 {
        self.fanout.lock().unwrap().log.next_offset()
    }

    pub fn log_capped_at(&self) -> Option<u64> {
        self.fanout.lock().unwrap().log.log_capped_at()
    }

    pub fn log_path(&self) -> PathBuf {
        self.fanout.lock().unwrap().log.path().to_path_buf()
    }

    /// Flushes the log to disk. This is the close case; the periodic one is
    /// `LogSyncer`'s (docs/05-persistence.md#output-log). `terminate()` calls
    /// it for the user-initiated path; wiring it to a child that exits on its
    /// own needs `exit_rx`, which is M4/M7.
    pub fn sync_log(&self) {
        if let Err(e) = self.sync.sync() {
            warn!(session_id = %self.id, path = %self.sync.path().display(), error = %e, "syncing the output log failed");
        }
    }

    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        self.pty.write(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.pty.resize(cols, rows)
    }

    /// Terminates the child and removes this session from its
    /// `SessionManager`'s directory. Removal is what lets the session (and
    /// its pty.rs writer thread, parked on `write_tx` until every clone of it
    /// drops) actually go away once the last `Arc<Session>` clone is; without
    /// it a terminated session's threads and channels leak for the life of
    /// the daemon. Idempotent, same as the underlying `pty.terminate()`.
    pub fn terminate(&self) -> Result<()> {
        let result = self.pty.terminate();
        self.sync_log();
        if let Some(directory) = self.directory.upgrade() {
            directory.lock().unwrap().remove(&self.id);
        }
        result
    }
}

/// A cap, a warning threshold or a failed write is operator-visible now and a
/// `session_events` row once M7 exists; nothing on the reader thread may
/// touch SQLite, so the log hands these back and they get traced here.
fn trace_log_events(id: SessionId, events: &[LogEvent]) {
    for event in events {
        warn!(session_id = %id, ?event, "output log event");
    }
}

/// Owns every live session. One lock for the session directory itself,
/// separate from each `Session`'s own `fanout` lock -- creating or looking up
/// a session never contends with another session's hot output path. Held
/// behind an `Arc` (rather than plain `Mutex<..>`) so a `Session` can keep a
/// `Weak` back-reference to it and self-remove on `terminate()`.
pub struct SessionManager {
    /// `<data_dir>/sessions`. Each session's log is `<root>/<id>/output.vt`
    /// (docs/05-persistence.md#layout).
    root: PathBuf,
    limits: LogLimits,
    /// One thread for every session's periodic `fsync`, so the reader threads
    /// never do one. Dropped with the manager, which flushes a last time.
    syncer: LogSyncer,
    sessions: Arc<SessionDirectory>,
}

impl SessionManager {
    pub fn new(root: PathBuf) -> Self {
        Self::with_limits(root, LogLimits::default())
    }

    /// Same, with non-default log thresholds -- how a test drives the cap
    /// path without writing a gigabyte.
    pub fn with_limits(root: PathBuf, limits: LogLimits) -> Self {
        Self {
            root,
            limits,
            syncer: LogSyncer::new(limits.sync_interval),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawns `spec` behind a fresh PTY and registers the resulting session.
    /// Wires `Fanout::publish` in as `pty::spawn`'s `on_output` closure --
    /// this is the plug point docs/03-pty-layer.md's reader loop and the M1
    /// module doc on `pty.rs` both point at.
    pub fn create(&self, spec: SpawnSpec) -> Result<Arc<Session>> {
        let id = SessionId::new();

        // A brand-new session has no stored row; `None` is the fresh-start
        // case of docs/05-persistence.md#restart-recovery, and M7 is what
        // will pass `Some` for a recovered one.
        let log = OutputLog::open(&self.root.join(id.to_string()), self.limits, None)?;
        let sync = log.sync_handle();
        self.syncer.register(&sync);
        let fanout = Arc::new(Mutex::new(Fanout::new(log)));

        let publish_fanout = Arc::clone(&fanout);
        let spawned = pty::spawn(spec, move |bytes| {
            let events = publish_fanout.lock().unwrap().publish(bytes);
            trace_log_events(id, &events);
        })?;

        let session = Arc::new(Session {
            id,
            pty: spawned.session,
            fanout,
            directory: Arc::downgrade(&self.sessions),
            sync,
        });
        self.sessions.lock().unwrap().insert(id, Arc::clone(&session));
        Ok(session)
    }

    pub fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Fanout` over a real log in a throwaway directory -- these tests
    /// exercise fan-out mechanics, not PTY spawning.
    fn scratch_fanout(dir: &std::path::Path) -> Arc<Mutex<Fanout>> {
        let log = OutputLog::open(dir, LogLimits::default(), None).expect("open log");
        Arc::new(Mutex::new(Fanout::new(log)))
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teleportd-session-unit-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Dropping a `Subscription` unregisters it immediately, not lazily on
    /// the next chunk -- an idle session must not accumulate dead slots.
    /// Needs `Fanout::subscribers` (private), so this lives here rather than
    /// in `daemon/tests/`.
    #[tokio::test]
    async fn dropped_subscription_is_unregistered_immediately() {
        let dir = scratch_dir("drop");
        let fanout = scratch_fanout(&dir);

        let sub = fanout.lock().unwrap().register(&fanout);
        assert_eq!(fanout.lock().unwrap().subscribers.len(), 1);

        drop(sub);
        assert_eq!(fanout.lock().unwrap().subscribers.len(), 0, "Drop must remove the slot without waiting for output");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Builds a `Replay` straight onto a scratch `Fanout`, skipping the PTY
    /// that `Session::attach` would need. The two D1 fixtures below are about
    /// the catch-up loop's arithmetic, and that is entirely decided by the
    /// log and the subscriber list -- driving it with a real child would add
    /// timing to a question that has none.
    fn scratch_replay(fanout: &Arc<Mutex<Fanout>>) -> Replay {
        let guard = fanout.lock().unwrap();
        let next_offset = guard.log.next_offset();
        let reader = guard.log.reader().expect("reader");
        drop(guard);
        Replay {
            replay_from: 0,
            next_offset,
            log_capped_at: None,
            fanout: Arc::clone(fanout),
            reader,
            cursor: 0,
            previous_gap: u64::MAX,
            stalled_rounds: 0,
            total_rounds: 0,
        }
    }

    fn publish_mib(fanout: &Arc<Mutex<Fanout>>, mib: usize) {
        let block = vec![b'x'; 64 * 1024];
        for _ in 0..(mib * 16) {
            fanout.lock().unwrap().publish(&block);
        }
    }

    /// **D1, structurally.** No subscriber exists while history is still
    /// being served, so the 8 MiB queue bound cannot be spent on replay --
    /// which is the whole failure
    /// (docs/04-api-protocol.md#catch-up--register-late-not-early). Needs
    /// private `Fanout::subscribers`, so it lives here.
    #[tokio::test]
    async fn no_subscriber_exists_until_the_remaining_gap_fits() {
        let dir = scratch_dir("catchup");
        let fanout = scratch_fanout(&dir);
        publish_mib(&fanout, 3);

        let mut replay = scratch_replay(&fanout);
        let next_offset = replay.next_offset;

        let mut rounds = 0u64;
        let attach = loop {
            assert!(
                fanout.lock().unwrap().subscribers.is_empty(),
                "round {rounds}: a subscriber must not be registered while history is still owed"
            );
            match replay.next_round().expect("catch-up round") {
                ReplayStep::History { offset, bytes, replay: rest } => {
                    assert_eq!(offset, rounds * REPLAY_ROUND_BYTES, "rounds must be contiguous");
                    assert_eq!(bytes.len() as u64, REPLAY_ROUND_BYTES, "a round is bounded");
                    rounds += 1;
                    replay = rest;
                }
                ReplayStep::Live(attach) => break attach,
            }
        };

        assert_eq!(rounds, 2, "3 MiB is two 1 MiB rounds, then a 1 MiB gap that fits");
        assert_eq!(fanout.lock().unwrap().subscribers.len(), 1, "the last round registers");
        assert!(attach.caught_up);
        assert_eq!(attach.replay_from, 2 * REPLAY_ROUND_BYTES);
        assert_eq!(attach.replay.len() as u64, LIVE_GAP_BYTES);
        assert_eq!(attach.replay_to(), next_offset, "replay must meet the live boundary exactly");

        drop(attach);
        assert!(fanout.lock().unwrap().subscribers.is_empty(), "dropping the attach unregisters");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of D1: convergence is not guaranteed, so the loop must
    /// have a floor. A producer the client never outruns makes the gap grow
    /// every round; after `MAX_STALLED_ROUNDS` the daemon stops trying,
    /// clamps replay to the freshest `LIVE_GAP_BYTES`, and reports the hole
    /// rather than spinning or handing over a queue that is certain to
    /// overflow.
    #[tokio::test]
    async fn a_catch_up_that_never_converges_gives_up_and_reports_the_hole() {
        let dir = scratch_dir("stall");
        let fanout = scratch_fanout(&dir);
        publish_mib(&fanout, 3);

        let mut replay = scratch_replay(&fanout);
        let mut rounds = 0u64;
        let attach = loop {
            assert!(rounds < 16, "the catch-up loop did not terminate");
            match replay.next_round().expect("catch-up round") {
                ReplayStep::History { bytes, replay: rest, .. } => {
                    rounds += 1;
                    assert_eq!(bytes.len() as u64, REPLAY_ROUND_BYTES);
                    // The producer gains 2 MiB for every 1 MiB served: this
                    // client is losing ground, every round, on purpose.
                    publish_mib(&fanout, 2);
                    replay = rest;
                }
                ReplayStep::Live(attach) => break attach,
            }
        };

        assert_eq!(rounds, MAX_STALLED_ROUNDS as u64, "one round to set the baseline, then four stalls");
        assert!(!attach.caught_up, "a clamped replay must say so");
        let served = rounds * REPLAY_ROUND_BYTES;
        assert!(attach.replay_from > served, "the clamp must leave a hole, not silently rewind");
        assert_eq!(attach.replay.len() as u64, LIVE_GAP_BYTES, "the client gets the freshest MiB");
        assert_eq!(
            attach.replay_to(),
            attach.next_offset,
            "and it still meets the live boundary exactly -- the hole is behind it, not in front"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `MAX_STALLED_ROUNDS` only catches a client that stops gaining ground
    /// *entirely*; one that gains a few bytes every round resets
    /// `stalled_rounds` to 0 forever and would re-acquire the fan-out mutex
    /// indefinitely without a second floor.
    /// Driving that through real rounds would mean actually shrinking a
    /// multi-gigabyte gap one byte at a time, so this exercises the pure
    /// decision `next_round` defers to instead -- deterministic and fast,
    /// same as the arithmetic it's testing.
    #[test]
    fn total_rounds_forces_registration_even_when_the_gap_keeps_shrinking() {
        let gap = LIVE_GAP_BYTES + 1; // never small enough to register on its own
        assert!(
            !should_register(gap, 0, MAX_CATCHUP_ROUNDS - 1),
            "must not register early just because rounds are piling up"
        );
        assert!(
            should_register(gap, 0, MAX_CATCHUP_ROUNDS),
            "a client that never stalls must still be bounded by total rounds"
        );
    }

    /// Issue #1 finding 4: `Open` (the initial `fanout.log.reader()` call)
    /// and `Read` (any `read_range` during catch-up) used to be one `Io`
    /// variant with a message hardcoded to the open case, so a read failure
    /// several rounds into a large backlog read as though the daemon had
    /// never managed to open the log at all. Guards against the two
    /// collapsing back into one variant, or trading messages.
    #[test]
    fn open_and_read_failures_report_distinct_messages() {
        let boom = || std::io::Error::other("boom");
        let open = AttachError::Open(boom()).to_string();
        let read = AttachError::Read(boom()).to_string();
        assert!(open.starts_with("opening the log for replay"), "got: {open}");
        assert!(read.starts_with("reading a replay range"), "got: {read}");
        assert_ne!(open, read);
    }

    /// The ordering rule, checked from the outside: by the time a chunk is
    /// visible to a subscriber, its bytes are already readable at the offset
    /// it was tagged with (docs/03-pty-layer.md#reader-loop). Needs private
    /// `Fanout`, so it lives here.
    #[tokio::test]
    async fn a_published_offset_is_already_on_disk() {
        let dir = scratch_dir("ordering");
        let fanout = scratch_fanout(&dir);
        let mut sub = fanout.lock().unwrap().register(&fanout);

        fanout.lock().unwrap().publish(b"abc");
        fanout.lock().unwrap().publish(b"defgh");

        let path = dir.join(crate::log::LOG_FILE_NAME);
        for expected_offset in [0u64, 3] {
            let chunk = sub.recv().await.expect("chunk");
            assert_eq!(chunk.offset, expected_offset);
            let on_disk = std::fs::read(&path).expect("read log");
            assert!(
                on_disk.len() as u64 >= chunk.offset + chunk.bytes.len() as u64,
                "chunk at offset {} is {} bytes, but only {} are on disk",
                chunk.offset,
                chunk.bytes.len(),
                on_disk.len()
            );
            assert_eq!(&on_disk[chunk.offset as usize..][..chunk.bytes.len()], &*chunk.bytes);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
