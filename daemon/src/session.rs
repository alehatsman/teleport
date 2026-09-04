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
//! **M4 update:** bounded attach (`tail`, `max_replay_bytes`, `truncated`)
//! stays one layer up in `ws.rs` -- it narrows `from` before it ever reaches
//! [`Session::attach`], exactly as the M3 doc above promised. What lands here
//! is everything else M4 needs from this module: session metadata
//! (`kind`/`preset`/`command`/`args`/`cwd`), the `running`/`closing`/`exited`
//! state machine wired to `exit_rx` (docs/03-pty-layer.md#state-machine), and
//! the control lease (docs/04-api-protocol.md#control-lease). SQLite
//! metadata, `session_events` and restart recovery (the `lost` state) remain
//! M7 -- everything below lives in memory only.
//!
//! **`exit_rx` is now consumed** by a dedicated thread per session
//! (`spawn_exit_listener`), the same shape as `pty.rs`'s own reader/writer/
//! reaper/control threads: block on a channel, never poll. `eof_rx` is still
//! unconsumed -- nothing needs "the reader thread saw EOF" as a distinct
//! signal from "the child exited" ([S2](../../docs/15-open-questions.md#s2--eof-is-not-exit)),
//! and the reader thread already keeps draining into `output.vt` on its own.
//!
//! **A conflict this milestone surfaced and resolved (see the M4 commit):**
//! the M2-era `terminate()` removed its session from the `SessionManager`
//! directory immediately, and a M2 test asserted exactly that. M4's own spec
//! (docs/04-api-protocol.md#delete-apiv1sessionsid) requires the opposite --
//! a terminated session stays listed as `exited` until an explicit
//! `?purge=true`. `terminate()` no longer self-removes; [`SessionManager::purge`]
//! does the removal now, and the old test was rewritten to match.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc, watch, Semaphore};
use tracing::warn;
use ulid::Ulid;

use crate::log::{LogEvent, LogLimits, LogReader, LogSyncer, OutputLog, SyncHandle};
use crate::pty::{self, PtySession, SpawnSpec, TerminalSession};

/// Milliseconds since the Unix epoch -- the shape of every `*_at_ms` field
/// here and, from M7 on, of the matching SQLite columns
/// (docs/05-persistence.md#schema).
fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

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

/// Parses a `{id}` path segment back into a `SessionId` -- `api.rs`'s job for
/// every `/api/v1/sessions/{id}*` route. An id that isn't a valid ULID is
/// `404`, same as one that is well-formed but unknown
/// (docs/04-api-protocol.md#delete-apiv1sessionsid: "Reserve 404 for an
/// unknown session id" -- a malformed one is just as unknown).
impl std::str::FromStr for SessionId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Ulid::from_string(s)?))
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

/// `running | closing | exited` -- the MVP subset of
/// docs/05-persistence.md#schema's `state` column. `lost` is M7's: it needs a
/// restart to detect a stale row, and nothing persists across a restart yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Closing,
    Exited,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Running => "running",
            SessionState::Closing => "closing",
            SessionState::Exited => "exited",
        }
    }
}

/// Why a session ended up `exited` without a clean exit code. The MVP subset
/// of docs/05-persistence.md#schema's `lost_reason` column that a session can
/// reach without SQLite: `daemon_restart` needs a restart to detect (M7);
/// `io_error` is a *running*-session reason, not a terminal one, and is not
/// wired here (M7's `session_events`, per the log.rs module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLostReason {
    /// `POST /api/v1/sessions` resolved to an executable that could not
    /// actually be spawned -- validated up front where possible
    /// ([`SessionManager::create`]'s own checks catch the common case as a
    /// clean `422` before this ever applies), this is the residual case
    /// where `pty::spawn` itself fails (docs/04-api-protocol.md#post-apiv1sessions).
    SpawnFailed,
    /// `terminate()`'s hard-kill step didn't produce an observed exit within
    /// `KILL_WAIT` (docs/03-pty-layer.md#concrete-policy step 5).
    KillTimeout,
    /// `child.wait()` itself returned an OS error rather than a status.
    WaitError,
}

impl SessionLostReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionLostReason::SpawnFailed => "spawn_failed",
            SessionLostReason::KillTimeout => "kill_timeout",
            SessionLostReason::WaitError => "wait_error",
        }
    }
}

/// Everything about a session that is fixed at creation and never changes --
/// the `kind`/`preset`/`command`/`args`/`cwd` columns of
/// docs/05-persistence.md#schema. Deliberately excludes `env`: overrides are
/// held only in the `SpawnSpec` passed to `pty::spawn` and never copied here
/// (docs/06-security.md#secrets-and-environment).
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub kind: String,
    pub preset: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

/// The mutable half of a session's API-visible state -- one lock, because
/// every field here changes together at exactly two moments (creation and
/// exit) or independently but rarely (resize). Not `fanout`'s job: that lock
/// is on the PTY output hot path and must stay tiny
/// (docs/03-pty-layer.md#reader-loop); this one is touched once per
/// resize/exit, never per byte.
struct Runtime {
    state: SessionState,
    pid: Option<u32>,
    cols: u16,
    rows: u16,
    created_at_ms: i64,
    started_at_ms: Option<i64>,
    exited_at_ms: Option<i64>,
    exit_code: Option<i32>,
    lost_reason: Option<SessionLostReason>,
}

/// One controller, or none. `holder`/`holder_name` name who; `grace`
/// distinguishes an actively-connected holder from one within its disconnect
/// grace window -- both are still "the holder" for `is_controller` and
/// `claim_control` purposes, only `attach_control`'s passive-resume check
/// treats them differently (docs/04-api-protocol.md#disconnect-grace).
///
/// `epoch` bumps on every grant (`attach_control` or `claim_control`), and
/// each granted WS connection remembers the epoch *it* was given. That's
/// what tells apart two simultaneous connections sharing one `client_id` --
/// e.g. the same browser tab reloaded before the old socket closed, or two
/// tabs racing a reconnect -- which `holder` alone can't (M4 review: keying
/// the lease purely on `client_id` let both connections pass `is_controller`
/// and write concurrently). Only the connection holding the *current* epoch
/// counts as the controller; an older connection with a stale epoch is
/// treated the same as one that never held control at all.
#[derive(Debug, Clone, Default)]
struct ControlLease {
    holder: Option<String>,
    holder_name: Option<String>,
    grace: bool,
    epoch: u64,
}

/// Asynchronous, out-of-band notifications a WS connection needs beyond raw
/// output bytes: another client resized the PTY, or this client's control
/// was just taken by someone else. Delivered over a `broadcast` channel
/// rather than threaded through `Subscription` because they are rare,
/// session-wide, and not part of the offset-indexed byte stream
/// (docs/04-api-protocol.md#control-messages).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Resized { cols: u16, rows: u16 },
    /// `lost_by` addresses the notification -- only the connection whose
    /// `client_id` matches acts on it. `new_controller_id`/`_name` are the
    /// wire message's content: who control was given *to*
    /// (docs/04-api-protocol.md#control-messages:
    /// `{"type":"control_revoked","to":"aleh's phone","client_id":"01K5Q…"}`
    /// -- both fields describe the new holder, not the one losing it).
    ControlRevoked { lost_by: String, new_controller_id: String, new_controller_name: String },
}

/// Capacity for the [`SessionEvent`] broadcast channel. Resize and
/// control-lease changes are both rare and human-paced (a resize on window
/// change, a control claim on a tap) -- nothing here is a hot path, so a
/// small bound is a correctness backstop against a wedged receiver, not a
/// throughput concern. A lagged receiver just means a WS task missed a
/// `resized`/`control_revoked` notification; the next control message it
/// sends is re-checked against the authoritative lease/size regardless (see
/// [`Session::is_controller`], [`Session::size`]), so a miss here is not a
/// correctness bug, only a delayed UI update.
const EVENT_CHANNEL_CAPACITY: usize = 32;

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

            let register = gap <= LIVE_GAP_BYTES || self.stalled_rounds >= MAX_STALLED_ROUNDS;
            let subscription = register.then(|| fanout.register(&self.fanout));
            (next_offset, end, log_capped_at, subscription)
        };

        let Some(subscription) = subscription else {
            let to = (self.cursor + REPLAY_ROUND_BYTES).min(end);
            let offset = self.cursor;
            let bytes = self.reader.read_range(offset, to)?;
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
        let replay = self.reader.read_range(replay_from, end)?;

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
    #[error("opening the log for replay: {0}")]
    Io(#[from] std::io::Error),
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
    pub meta: SessionMeta,
    pty: PtySession,
    fanout: Arc<Mutex<Fanout>>,
    runtime: Mutex<Runtime>,
    control: Mutex<ControlLease>,
    events: broadcast::Sender<SessionEvent>,
    /// `true` once the exit listener thread has recorded a final state.
    /// `watch` rather than `broadcast` -- unlike [`SessionEvent`], "has this
    /// session exited yet" is a level, not an edge: a WS task that attaches
    /// after the exit must still see it, which `changed()` alone would miss.
    exited: watch::Receiver<bool>,
    /// Kept only so [`SessionManager::create`] can hand the matching
    /// `Sender` to the exit listener thread; no other code sends on it.
    exited_tx: watch::Sender<bool>,
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
        let reader = fanout.log.reader()?;
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

    /// A fresh read handle for a one-shot range read -- `GET
    /// /api/v1/sessions/{id}/log`'s job (docs/04-api-protocol.md#get-apiv1sessionsidlog).
    /// Opening under the lock, same as [`attach`](Self::attach), saves the
    /// caller from reasoning about the file moving; reading the bytes does
    /// not need the lock at all.
    pub fn log_reader(&self) -> std::io::Result<LogReader> {
        self.fanout.lock().unwrap().log.reader()
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

    /// `pty.write()` already rejects input once the session is `closing` or
    /// `exited` (docs/03-pty-layer.md#concrete-policy step 1) -- callers
    /// (`ws.rs`) map that `Err` to the `session_closing` error frame
    /// (docs/04-api-protocol.md#error-codes). No separate state check is
    /// needed here.
    pub fn write(&self, bytes: &[u8]) -> Result<()> {
        self.pty.write(bytes)
    }

    /// Resizes the PTY and records the new size for `ready`/`GET` to report,
    /// then tells every other attached client via `resized`
    /// (docs/04-api-protocol.md#control-messages). Controller-only
    /// enforcement is `ws.rs`'s job ([`Session::is_controller`]) -- this
    /// method does not check who is calling.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.pty.resize(cols, rows)?;
        {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.cols = cols;
            runtime.rows = rows;
        }
        let _ = self.events.send(SessionEvent::Resized { cols, rows });
        Ok(())
    }

    /// The PTY's current size -- `ready.cols`/`ready.rows` and every `GET`
    /// response's `cols`/`rows` read this, not the size the session was
    /// created with (docs/04-api-protocol.md#control-messages: "there is
    /// exactly one PTY geometry per session").
    pub fn size(&self) -> (u16, u16) {
        let r = self.runtime.lock().unwrap();
        (r.cols, r.rows)
    }

    pub fn state(&self) -> SessionState {
        self.runtime.lock().unwrap().state
    }

    pub fn pid(&self) -> Option<u32> {
        self.runtime.lock().unwrap().pid
    }

    pub fn created_at_ms(&self) -> i64 {
        self.runtime.lock().unwrap().created_at_ms
    }

    pub fn started_at_ms(&self) -> Option<i64> {
        self.runtime.lock().unwrap().started_at_ms
    }

    pub fn exited_at_ms(&self) -> Option<i64> {
        self.runtime.lock().unwrap().exited_at_ms
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.runtime.lock().unwrap().exit_code
    }

    pub fn lost_reason(&self) -> Option<SessionLostReason> {
        self.runtime.lock().unwrap().lost_reason
    }

    /// Live subscriber count -- `GET /api/v1/sessions`'s `subscribers` field
    /// (docs/04-api-protocol.md#get-apiv1sessions).
    pub fn subscriber_count(&self) -> usize {
        self.fanout.lock().unwrap().subscribers.len()
    }

    /// Resolves once the exit listener thread has recorded a final state.
    /// `ws.rs` selects on this alongside `Subscription::recv` to know when to
    /// send the `exit` frame; a task attaching after the session already
    /// exited sees `true` immediately (`watch` holds its last value), rather
    /// than waiting on an edge it already missed.
    pub async fn exited(&self) {
        let mut rx = self.watch_exited();
        if *rx.borrow() {
            return;
        }
        let _ = rx.changed().await;
    }

    /// An owned `watch::Receiver` for a caller (`ws.rs`) that needs to hold
    /// it across a `tokio::select!` loop rather than await it once.
    pub fn watch_exited(&self) -> watch::Receiver<bool> {
        self.exited.clone()
    }

    /// A fresh receiver for [`SessionEvent`]s -- one per WS connection, so a
    /// slow or absent reader on one connection cannot back up another's
    /// (docs/04-api-protocol.md#control-messages).
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// `mode=control` on attach. Grants the lease only when it is free or
    /// already held (actively or within grace) by `client_id` -- attach must
    /// never preempt (docs/04-api-protocol.md#why-attach-must-not-preempt).
    /// Returns the epoch this connection now holds (the caller must present
    /// it back to [`Self::write_if_controller`]/[`Self::release_control`]/
    /// [`Self::begin_control_grace`]), or `None` if control was not granted.
    pub fn attach_control(&self, client_id: &str, client_name: &str) -> Option<u64> {
        let mut lease = self.control.lock().unwrap();
        match lease.holder.as_deref() {
            None => {
                lease.holder = Some(client_id.to_string());
                lease.holder_name = Some(client_name.to_string());
                lease.grace = false;
                lease.epoch += 1;
                Some(lease.epoch)
            }
            Some(holder) if holder == client_id => {
                lease.holder_name = Some(client_name.to_string());
                lease.grace = false;
                lease.epoch += 1;
                Some(lease.epoch)
            }
            Some(_) => None,
        }
    }

    /// `claim_control`. Always preempts, including during another holder's
    /// grace window (docs/04-api-protocol.md#disconnect-grace: "the lease is
    /// still preemptible"). Notifies the previous holder, if any and if
    /// different, via `control_revoked`. Returns the epoch this connection
    /// now holds.
    pub fn claim_control(&self, client_id: &str, client_name: &str) -> u64 {
        let (previous, epoch) = {
            let mut lease = self.control.lock().unwrap();
            let previous = match (lease.holder.take(), lease.holder_name.take()) {
                (Some(holder), Some(name)) if holder != client_id => Some((holder, name)),
                _ => None,
            };
            lease.holder = Some(client_id.to_string());
            lease.holder_name = Some(client_name.to_string());
            lease.grace = false;
            lease.epoch += 1;
            (previous, lease.epoch)
        };
        if let Some((lost_by, _lost_name)) = previous {
            let _ = self.events.send(SessionEvent::ControlRevoked {
                lost_by,
                new_controller_id: client_id.to_string(),
                new_controller_name: client_name.to_string(),
            });
        }
        epoch
    }

    /// Explicit `release_control`. A no-op unless `client_id` still holds
    /// `epoch` -- a stale release from a connection that already lost the
    /// lease (superseded by a reconnect or a `claim_control`, both of which
    /// bump the epoch) must not clear whoever holds it now.
    pub fn release_control(&self, client_id: &str, epoch: u64) {
        let mut lease = self.control.lock().unwrap();
        if lease.holder.as_deref() == Some(client_id) && lease.epoch == epoch {
            lease.holder = None;
            lease.holder_name = None;
            lease.grace = false;
        }
    }

    /// Whether `client_id`'s connection holding `epoch` is still the
    /// controller. Checking `epoch` alongside `client_id` is what tells
    /// apart two simultaneous connections that happen to share a
    /// `client_id` -- only the one holding the *current* epoch (the most
    /// recent `attach_control`/`claim_control` grant) counts.
    pub fn is_controller(&self, client_id: &str, epoch: u64) -> bool {
        let lease = self.control.lock().unwrap();
        lease.holder.as_deref() == Some(client_id) && lease.epoch == epoch
    }

    /// Atomically checks `is_controller` and writes, holding the lease lock
    /// across both. Checking and writing as two separate calls left a window
    /// where a concurrent `claim_control` could move the lease in between,
    /// so a just-preempted connection's input could still reach the PTY (M4
    /// review). `Err(None)` means "not the controller"; `Err(Some(e))` means
    /// the write itself failed (session closing).
    pub fn write_if_controller(&self, client_id: &str, epoch: u64, bytes: &[u8]) -> Result<(), Option<anyhow::Error>> {
        let lease = self.control.lock().unwrap();
        if lease.holder.as_deref() != Some(client_id) || lease.epoch != epoch {
            return Err(None);
        }
        // Still holding `lease`: a concurrent `claim_control`/`attach_control`
        // blocks on the same mutex and cannot move the holder until this
        // write has gone out.
        self.write(bytes).map_err(Some)
    }

    pub fn controller_name(&self) -> Option<String> {
        self.control.lock().unwrap().holder_name.clone()
    }

    /// Starts `client_id`'s disconnect grace window, if the connection
    /// holding `epoch` is still the lease holder at the moment its WS
    /// connection ends. A background task frees the lease after `grace_ms`
    /// unless the same `client_id` reclaims it first via
    /// [`attach_control`](Self::attach_control) (which bumps `epoch` and
    /// clears `grace`) -- the lease is **never** auto-granted to anyone else
    /// when the window expires (docs/04-api-protocol.md#disconnect-grace).
    pub fn begin_control_grace(self: &Arc<Self>, client_id: String, epoch: u64, grace_ms: u64) {
        {
            let mut lease = self.control.lock().unwrap();
            if lease.holder.as_deref() != Some(client_id.as_str()) || lease.epoch != epoch {
                return; // already lost the lease before disconnecting; nothing to hold.
            }
            lease.grace = true;
        }
        let session = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(grace_ms)).await;
            let mut lease = session.control.lock().unwrap();
            // Only free it if grace is still what's protecting this same
            // epoch -- a reconnect bumps `epoch` and clears `grace`, and a
            // `claim_control` from someone else already replaced `holder`
            // (and `epoch`) entirely.
            if lease.grace && lease.holder.as_deref() == Some(client_id.as_str()) && lease.epoch == epoch {
                lease.holder = None;
                lease.holder_name = None;
                lease.grace = false;
            }
        });
    }

    /// Begins termination: state becomes `closing` immediately (visible to
    /// `GET` right away), then runs `pty.rs`'s bounded terminate policy,
    /// which blocks for up to ~7s (docs/03-pty-layer.md#concrete-policy).
    /// The exit listener thread -- not this method -- makes the final
    /// `running`/`closing` -> `exited` transition, because a spontaneous
    /// child exit must reach `exited` the same way a requested one does.
    ///
    /// **Does not remove the session from its `SessionManager`.** A
    /// terminated session stays listed as `exited` until an explicit
    /// `?purge=true` (docs/04-api-protocol.md#delete-apiv1sessionsid) --
    /// see [`SessionManager::purge`]. Idempotent, same as the underlying
    /// `pty.terminate()`.
    pub fn terminate(&self) -> Result<()> {
        {
            let mut runtime = self.runtime.lock().unwrap();
            if runtime.state == SessionState::Running {
                runtime.state = SessionState::Closing;
            }
        }
        let result = self.pty.terminate();
        self.sync_log();
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

/// Refuse to spawn past this many concurrent sessions -- `429`, not an OOM
/// discovered the hard way (docs/06-security.md#process-spawning). `config.toml`
/// overrides it via [`SessionManager::with_max_sessions`].
const DEFAULT_MAX_SESSIONS: usize = 50;

/// Why [`SessionManager::create`] refused a request, shaped for `api.rs` to
/// map onto the HTTP status docs/04-api-protocol.md#post-apiv1sessions
/// specifies -- `422` for the two validation variants, `429` for
/// `MaxSessions`, `500` for `Spawn`.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("executable not found on PATH: {0}")]
    ExecutableNotFound(String),
    #[error("cwd does not exist or is not a directory: {}", .0.display())]
    InvalidCwd(PathBuf),
    #[error("max_sessions ({0}) reached")]
    MaxSessions(usize),
    #[error("spawning the session: {0}")]
    Spawn(#[from] anyhow::Error),
}

/// Holds one `SessionManager::reserve_slot` reservation for the lifetime of
/// a `create()` call. Releases it on every exit path -- an early `?`/`return
/// Err`, or `create()`'s final `insert` -- so a slot is never leaked, and
/// never double-counted once the session it reserved for is itself in
/// `sessions` and counted by `reserve_slot`'s own scan.
struct ReservationGuard<'a> {
    reserved: &'a Mutex<usize>,
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        *self.reserved.lock().unwrap() -= 1;
    }
}

/// Owns every live session. One lock for the session directory itself,
/// separate from each `Session`'s own `fanout` lock -- creating or looking up
/// a session never contends with another session's hot output path.
/// `Session` keeps no back-link to it: [`SessionManager::purge`] removes by
/// id through this map directly, and `terminate()` no longer self-removes
/// (see the M4 module doc above).
pub struct SessionManager {
    /// `<data_dir>/sessions`. Each session's log is `<root>/<id>/output.vt`
    /// (docs/05-persistence.md#layout).
    root: PathBuf,
    limits: LogLimits,
    max_sessions: usize,
    /// One thread for every session's periodic `fsync`, so the reader threads
    /// never do one. Dropped with the manager, which flushes a last time.
    syncer: LogSyncer,
    sessions: Arc<SessionDirectory>,
    /// In-flight `create()` calls that passed the cap check but haven't
    /// inserted into `sessions` yet (still validating `cwd`/`command` or
    /// mid-`fork`/`exec`). Counted alongside live sessions so concurrent
    /// creates can't all pass the check before any of them inserts (M4
    /// review: the cap wasn't enforced atomically).
    reserved: Mutex<usize>,
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
            max_sessions: DEFAULT_MAX_SESSIONS,
            syncer: LogSyncer::new(limits.sync_interval),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            reserved: Mutex::new(0),
        }
    }

    /// Overrides `max_sessions` from `config.toml` (default 50,
    /// docs/07-remote-access.md#daemon-configuration-surface). A builder
    /// method rather than a `with_limits` parameter so every existing call
    /// site -- tests included -- keeps working unchanged.
    pub fn with_max_sessions(mut self, max_sessions: usize) -> Self {
        self.max_sessions = max_sessions;
        self
    }

    /// Atomically checks `max_sessions` against live sessions (`Running` or
    /// `Closing` -- an `exited`-but-unpurged entry holds no PTY or child
    /// process, so it doesn't count against the cap; otherwise routine
    /// create/DELETE traffic with no `?purge=true` would eventually wedge
    /// `create()` at 429 forever even with nothing actually running) plus
    /// any other `create()` call already past this check but not yet
    /// inserted, and reserves a slot if there's room. Both locks are held
    /// together for the whole check-then-increment so no second caller can
    /// slip in between.
    fn reserve_slot(&self) -> Result<ReservationGuard<'_>, CreateError> {
        let sessions = self.sessions.lock().unwrap();
        let mut reserved = self.reserved.lock().unwrap();
        let live = sessions
            .values()
            .filter(|s| s.runtime.lock().unwrap().state != SessionState::Exited)
            .count();
        if live + *reserved >= self.max_sessions {
            return Err(CreateError::MaxSessions(self.max_sessions));
        }
        *reserved += 1;
        Ok(ReservationGuard { reserved: &self.reserved })
    }

    /// Spawns `spec` behind a fresh PTY and registers the resulting session,
    /// tagged with the API-level `kind`/`preset` metadata `SpawnSpec` itself
    /// has no room for. Wires `Fanout::publish` in as `pty::spawn`'s
    /// `on_output` closure -- this is the plug point docs/03-pty-layer.md's
    /// reader loop and the M1 module doc on `pty.rs` both point at.
    ///
    /// Validates `cwd` and resolves `command` against `$PATH` *before*
    /// spawning (docs/06-security.md#process-spawning): a bad executable
    /// would otherwise fork successfully and exit immediately, surfacing as
    /// a session that flickers to `exited` instead of a clean `422`
    /// (docs/04-api-protocol.md#post-apiv1sessions). `max_sessions` is
    /// enforced first, before either check, so a saturated daemon fails fast.
    pub fn create(
        &self,
        spec: SpawnSpec,
        kind: impl Into<String>,
        preset: Option<String>,
    ) -> Result<Arc<Session>, CreateError> {
        // Reserve a slot before doing anything else, so two concurrent
        // creates can't both pass the cap check before either inserts. The
        // guard's Drop decrements `reserved` on every exit path (an early
        // `?`/`return Err` here, or the final `insert` below), so the slot
        // is never leaked or double-counted.
        let _reservation = self.reserve_slot()?;
        if !spec.cwd.is_dir() {
            return Err(CreateError::InvalidCwd(spec.cwd.to_path_buf()));
        }
        if !resolve_executable(spec.program, spec.cwd) {
            return Err(CreateError::ExecutableNotFound(spec.program.to_string()));
        }

        let id = SessionId::new();
        let meta = SessionMeta {
            kind: kind.into(),
            preset,
            command: spec.program.to_string(),
            args: spec.args.to_vec(),
            cwd: spec.cwd.to_path_buf(),
        };
        let (cols, rows) = (spec.cols, spec.rows);

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
        })
        .map_err(CreateError::Spawn)?;

        let created_at_ms = now_ms();
        let (exited_tx, exited) = watch::channel(false);
        let pid = spawned.pid;
        let session = Arc::new(Session {
            id,
            meta,
            pty: spawned.session,
            fanout,
            runtime: Mutex::new(Runtime {
                state: SessionState::Running,
                pid,
                cols,
                rows,
                created_at_ms,
                started_at_ms: Some(created_at_ms),
                exited_at_ms: None,
                exit_code: None,
                lost_reason: None,
            }),
            control: Mutex::new(ControlLease::default()),
            events: broadcast::channel(EVENT_CHANNEL_CAPACITY).0,
            exited,
            exited_tx,
            sync,
        });
        self.sessions.lock().unwrap().insert(id, Arc::clone(&session));
        spawn_exit_listener(Arc::clone(&session), spawned.exit_rx);
        Ok(session)
    }

    pub fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    /// Every live session, for `GET /api/v1/sessions`. No ordering promise --
    /// `api.rs` sorts newest-first (docs/04-api-protocol.md#get-apiv1sessions).
    pub fn list(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().unwrap().values().cloned().collect()
    }

    /// `?purge=true`: removes the session from the directory (directory
    /// entry first is not a concern here -- `api.rs` deletes
    /// `data/sessions/{id}/` before calling this, matching the collector's
    /// own directory-first-row-second ordering,
    /// docs/05-persistence.md#garbage-collection). A no-op if the id is
    /// already gone. Idempotent by construction (`HashMap::remove`).
    pub fn purge(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().remove(&id)
    }
}

/// Resolves `command` the same way spawning eventually will -- a literal
/// path if it contains a separator, otherwise a `$PATH` scan -- so a bad
/// executable is caught as a clean `422` before a child is ever forked
/// (docs/04-api-protocol.md#post-apiv1sessions), rather than surfacing as a
/// session that spawns and immediately exits. Existence only; permission
/// bits beyond "the executable bit is set on Unix" are not checked -- the
/// exec call itself is the authoritative check for anything subtler, same as
/// every shell's own `$PATH` lookup.
fn resolve_executable(command: &str, cwd: &Path) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        // A literal path (contains a separator). A relative one resolves
        // against the session's `cwd` -- what `pty::spawn` actually spawns
        // in -- not the daemon's own cwd (M4 review: checking against the
        // daemon's cwd could wrongly 422 a command that would have spawned
        // fine, e.g. `./run.sh` with `cwd` pointing at its directory).
        let resolved = if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) };
        return is_executable_file(&resolved);
    }
    let Some(path_var) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path_var).any(|dir| is_executable_file(&dir.join(command)))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    // No PATHEXT scan -- accept an exact match, or one of the extensions the
    // shipped presets actually use (docs/04-api-protocol.md#get-apiv1presets).
    // A full PATHEXT implementation is not worth the complexity for the MVP;
    // see docs/15-open-questions.md for the Windows gaps that are.
    if path.is_file() {
        return true;
    }
    ["exe", "cmd", "bat", "ps1"].iter().any(|ext| path.with_extension(ext).is_file())
}

/// Consumes `exit_rx` on its own thread -- the same shape as `pty.rs`'s own
/// reader/writer/reaper/control threads (docs/03-pty-layer.md#thread-model):
/// block on a channel, do the minimum work, never poll. Makes the *only*
/// `Running`/`Closing` -> `Exited` transition, whether the exit was
/// spontaneous or `terminate()`-requested, because docs/03-pty-layer.md's
/// state machine requires exactly that: `RUNNING -> EXITED` fires off the
/// reaper thread's `wait()` result directly, never off reader EOF.
fn spawn_exit_listener(session: Arc<Session>, exit_rx: std::sync::mpsc::Receiver<pty::PtyExit>) {
    std::thread::Builder::new()
        .name("session-exit".into())
        .spawn(move || {
            // A closed channel with no message would mean `pty::spawn`'s
            // control thread died without ever publishing an exit -- a bug
            // there, not something to panic this thread over.
            let Ok(exit) = exit_rx.recv() else { return };

            let (exit_code, lost_reason) = match exit.status {
                Some(status) => (Some(status.exit_code() as i32), None),
                None => (
                    None,
                    Some(match exit.lost_reason {
                        Some(pty::LostReason::KillTimeout) => SessionLostReason::KillTimeout,
                        Some(pty::LostReason::WaitError) | None => SessionLostReason::WaitError,
                    }),
                ),
            };

            {
                let mut runtime = session.runtime.lock().unwrap();
                runtime.state = SessionState::Exited;
                runtime.exit_code = exit_code;
                runtime.lost_reason = lost_reason;
                runtime.exited_at_ms = Some(now_ms());
            }
            session.sync_log();
            // `send` on a `watch` never fails as long as this `Sender` (owned
            // by `session`, which this thread also holds a strong ref to) is
            // alive -- it always is here.
            let _ = session.exited_tx.send(true);
        })
        .expect("spawning session-exit thread");
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

    /// M4 review: a relative literal path (contains a separator) used to be
    /// checked against the daemon's own cwd, not the session's requested
    /// `cwd` -- what `pty::spawn` actually spawns in -- so a valid
    /// `./relative/script` could be wrongly rejected with 422.
    #[test]
    fn resolve_executable_checks_a_relative_command_against_the_session_cwd() {
        let dir = scratch_dir("resolve-executable");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Relative to the daemon's own (irrelevant) cwd, "./run.sh" resolves
        // to nothing -- only resolving against the session's `cwd` finds it.
        assert!(!resolve_executable("./run.sh", &std::env::temp_dir().join("not-the-right-place")));
        assert!(resolve_executable("./run.sh", &dir));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
