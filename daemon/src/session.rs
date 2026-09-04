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
//! reorder. The same mutex closes the attach race: [`Session::attach`]
//! registers the subscriber *and* reads the replay boundary `N` under one
//! acquisition, so every chunk that subscriber ever receives starts at or
//! after `N` (docs/04-api-protocol.md#attach-race).
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

use crate::log::{LogEvent, LogLimits, LogReader, OutputLog};
use crate::pty::{self, PtySession, SpawnSpec, TerminalSession};

/// Queue bound per subscriber: whichever trips first
/// (docs/03-pty-layer.md#backpressure).
const MAX_QUEUE_CHUNKS: usize = 256;
const MAX_QUEUE_BYTES: usize = 8 * 1024 * 1024;

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
    /// page cache and never an `fsync` on this path, every send is
    /// `try_send`, budget acquisition is `try_acquire`, and a subscriber
    /// that would exceed either bound is dropped instead of waited on
    /// (docs/03-pty-layer.md#the-rule).
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

/// A subscriber registered at the replay boundary. `replay_from..replay_to`
/// is the byte range to serve out of `reader` before the first chunk from
/// `subscription`; the two meet exactly once -- no gap, no duplicate.
pub struct Attach {
    /// Where replay actually starts. The requested offset, except for a
    /// client attaching past a cap: there it is `next_offset` and the range
    /// is empty (docs/05-persistence.md#size-cap).
    pub replay_from: u64,
    /// One past the last replayable byte: `min(N, readable_end)`.
    pub replay_to: u64,
    /// `N` -- the boundary. Every chunk from `subscription` starts here or
    /// later, guaranteed by the single lock `attach` takes.
    pub next_offset: u64,
    pub log_capped_at: Option<u64>,
    pub reader: LogReader,
    pub subscription: Subscription,
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
    pty: PtySession,
    fanout: Arc<Mutex<Fanout>>,
    /// Back-link so `terminate()` can remove this session from the directory
    /// itself, rather than relying on every caller to remember to. `Weak` so
    /// this isn't a reference cycle -- the directory's `Arc<Session>` is the
    /// only strong owner.
    directory: Weak<SessionDirectory>,
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

    /// Subscribes *and* establishes the replay boundary atomically.
    ///
    /// Registration and the read of `N` happen under one lock -- the same one
    /// the reader loop takes to append and fan out -- which is what
    /// guarantees replay and live output meet exactly once
    /// (docs/04-api-protocol.md#attach-race). Opening the read handle happens
    /// under it too: cheap, and it saves the caller reasoning about the file
    /// moving. Reading the bytes does not -- that is the caller's job, off
    /// the lock, through `Attach::reader`.
    ///
    /// Replay here is unbounded by design; `tail` / `max_replay_bytes` are
    /// M4's (docs/04-api-protocol.md#bounded-attach) and narrow `replay_from`
    /// before the read. Keeping the bound above this line is what lets a VT
    /// state snapshot replace a byte range later without a protocol change.
    pub fn attach(&self, from: u64) -> Result<Attach, AttachError> {
        let mut fanout = self.fanout.lock().unwrap();

        let next_offset = fanout.log.next_offset();
        if from > next_offset {
            return Err(AttachError::OffsetAhead { requested: from, next_offset });
        }
        let log_capped_at = fanout.log.log_capped_at();
        let readable_end = fanout.log.readable_end();
        let reader = fanout.log.reader()?;

        // Bytes between a cap and `next_offset` were streamed live and are
        // gone. A client asking for them gets no replay and is told where the
        // stream resumes, rather than being served whatever happens to sit at
        // that file position (docs/05-persistence.md#size-cap).
        let mut replay_from = from;
        let mut replay_to = next_offset.min(readable_end);
        if replay_from >= replay_to {
            replay_from = next_offset;
            replay_to = next_offset;
        }

        let subscription = fanout.register(&self.fanout);
        drop(fanout);

        Ok(Attach { replay_from, replay_to, next_offset, log_capped_at, reader, subscription })
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

    /// Flushes the log to disk. The periodic case lives inside the append
    /// path; this is the close case (docs/05-persistence.md#output-log).
    /// `terminate()` calls it for the user-initiated path; wiring it to a
    /// child that exits on its own needs `exit_rx`, which is M4/M7.
    pub fn sync_log(&self) {
        let events = self.fanout.lock().unwrap().log.sync();
        trace_log_events(self.id, &events);
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
    sessions: Arc<SessionDirectory>,
}

impl SessionManager {
    pub fn new(root: PathBuf) -> Self {
        Self::with_limits(root, LogLimits::default())
    }

    /// Same, with non-default log thresholds -- how a test drives the cap
    /// path without writing a gigabyte.
    pub fn with_limits(root: PathBuf, limits: LogLimits) -> Self {
        Self { root, limits, sessions: Arc::new(Mutex::new(HashMap::new())) }
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
