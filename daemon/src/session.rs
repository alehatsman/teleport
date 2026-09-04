//! Session ownership and backpressure on top of `pty.rs`.
//!
//! See docs/11-mvp-plan.md#m2--session-ownership-and-backpressure and
//! docs/01-architecture.md#session-manager-shape for the design this
//! implements, and docs/03-pty-layer.md#backpressure for the fan-out and
//! queue-bound rules below.
//!
//! **M2 scope boundary:** this file owns in-memory session lifetime,
//! subscriber fan-out, and the offset counter. It does **not** persist
//! anything to disk -- `output.vt` and `log_capped_at` are `log.rs` (M3);
//! SQLite metadata is `persistence.rs` (M7). `on_output` here only advances
//! `next_offset` and fans out; the "persist first" half of
//! docs/03-pty-layer.md#reader-loop's ordering lands when M3 wires a log
//! writer into the same closure.
//!
//! `exit_rx`/`eof_rx` from `pty::spawn` are intentionally left unconsumed --
//! dropping them is safe (the sender threads see a closed channel and move
//! on, they never block on it) and wiring session state / exit events to
//! them is M4/M7 API-surface work, not backpressure.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use anyhow::Result;
use tokio::sync::{mpsc, Semaphore};
use ulid::Ulid;

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

/// The `{next_offset, subscribers}` pair, guarded by one short-held mutex
/// (docs/03-pty-layer.md#reader-loop: "steps 2-4 happen under one short
/// mutex that also guards subscriber registration").
struct Fanout {
    next_offset: u64,
    subscribers: Vec<SubscriberSlot>,
    next_subscriber_id: u64,
}

impl Fanout {
    fn new() -> Self {
        Self { next_offset: 0, subscribers: Vec::new(), next_subscriber_id: 0 }
    }

    /// Runs on the PTY reader thread via the `on_output` closure passed to
    /// `pty::spawn`. Must never block: every send is `try_send`, budget
    /// acquisition is `try_acquire`, and a subscriber that would exceed
    /// either bound is dropped instead of waited on
    /// (docs/03-pty-layer.md#the-rule).
    fn publish(&mut self, bytes: &[u8]) {
        let start = self.next_offset;
        self.next_offset += bytes.len() as u64;

        if self.subscribers.is_empty() {
            return; // docs/11-mvp-plan.md#m2: zero subscribers is fine, indefinitely.
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
    /// Registers a new subscriber and returns its receive end. Bounded to
    /// `MAX_QUEUE_CHUNKS` chunks / `MAX_QUEUE_BYTES` bytes, whichever trips
    /// first (docs/03-pty-layer.md#backpressure); a subscriber that falls
    /// behind is disconnected (`recv()` returns `None`), never blocks the
    /// reader.
    pub fn subscribe(&self) -> Subscription {
        let (tx, rx) = mpsc::channel(MAX_QUEUE_CHUNKS);
        let budget = Arc::new(Semaphore::new(MAX_QUEUE_BYTES));

        let mut fanout = self.fanout.lock().unwrap();
        let id = SubscriberId(fanout.next_subscriber_id);
        fanout.next_subscriber_id += 1;
        fanout.subscribers.push(SubscriberSlot { id, tx, budget: Arc::clone(&budget) });
        drop(fanout);

        Subscription { id, rx, budget, fanout: Arc::clone(&self.fanout) }
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
        if let Some(directory) = self.directory.upgrade() {
            directory.lock().unwrap().remove(&self.id);
        }
        result
    }
}

/// Owns every live session. One lock for the session directory itself,
/// separate from each `Session`'s own `fanout` lock -- creating or looking up
/// a session never contends with another session's hot output path. Held
/// behind an `Arc` (rather than plain `Mutex<..>`) so a `Session` can keep a
/// `Weak` back-reference to it and self-remove on `terminate()`.
#[derive(Default)]
pub struct SessionManager {
    sessions: Arc<SessionDirectory>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns `spec` behind a fresh PTY and registers the resulting session.
    /// Wires `Fanout::publish` in as `pty::spawn`'s `on_output` closure --
    /// this is the plug point docs/03-pty-layer.md's reader loop and the M1
    /// module doc on `pty.rs` both point at.
    pub fn create(&self, spec: SpawnSpec) -> Result<Arc<Session>> {
        let id = SessionId::new();
        let fanout = Arc::new(Mutex::new(Fanout::new()));

        let publish_fanout = Arc::clone(&fanout);
        let spawned = pty::spawn(spec, move |bytes| {
            publish_fanout.lock().unwrap().publish(bytes);
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

    /// Dropping a `Subscription` unregisters it immediately, not lazily on
    /// the next chunk -- an idle session must not accumulate dead slots.
    /// Needs `Fanout::subscribers` (private), so this lives here rather than
    /// in `daemon/tests/`.
    #[tokio::test]
    async fn dropped_subscription_is_unregistered_immediately() {
        let fanout = Arc::new(Mutex::new(Fanout::new()));

        let (tx, rx) = mpsc::channel(MAX_QUEUE_CHUNKS);
        let budget = Arc::new(Semaphore::new(MAX_QUEUE_BYTES));
        let id = SubscriberId(0);
        fanout.lock().unwrap().subscribers.push(SubscriberSlot { id, tx, budget: Arc::clone(&budget) });
        let sub = Subscription { id, rx, budget, fanout: Arc::clone(&fanout) };
        assert_eq!(fanout.lock().unwrap().subscribers.len(), 1);

        drop(sub);
        assert_eq!(fanout.lock().unwrap().subscribers.len(), 0, "Drop must remove the slot without waiting for output");
    }
}
