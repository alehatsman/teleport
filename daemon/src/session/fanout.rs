//! The `{output log, subscribers}` pair and the live-subscriber handle over
//! it -- docs/03-pty-layer.md#reader-loop's "steps 2-4 happen under one
//! short mutex that also guards subscriber registration". See the parent
//! module doc (`session/mod.rs`) for the design this implements; `replay.rs`
//! is the other half of the story (catch-up reads off this same mutex).

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{mpsc, Semaphore};

use crate::log::{LogEvent, OutputLog};

use super::types::Chunk;

/// Queue bound per subscriber: whichever trips first
/// (docs/03-pty-layer.md#backpressure).
const MAX_QUEUE_CHUNKS: usize = 256;
pub(super) const MAX_QUEUE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SubscriberId(u64);

pub(super) struct SubscriberSlot {
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
///
/// `pub(super)`: constructed in `manager.rs`'s `create()`, held by
/// `mod.rs`'s `Session` and by `replay.rs`'s `Replay`, so the whole
/// `session` subtree needs to see it -- no wider.
pub(super) struct Fanout {
    pub(super) log: OutputLog,
    pub(super) subscribers: Vec<SubscriberSlot>,
    next_subscriber_id: u64,
}

impl Fanout {
    pub(super) fn new(log: OutputLog) -> Self {
        Self {
            log,
            subscribers: Vec::new(),
            next_subscriber_id: 0,
        }
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
    pub(super) fn publish(&mut self, bytes: &[u8]) -> Vec<LogEvent> {
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
            let chunk = Chunk {
                offset: start,
                bytes: Arc::clone(&payload),
            };
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
    /// `Replay::next_round` can register *and* read the replay boundary under
    /// one acquisition of the lock -- that atomicity is the whole attach-race
    /// fix (docs/04-api-protocol.md#attach-race).
    pub(super) fn register(&mut self, fanout: &Arc<Mutex<Fanout>>) -> Subscription {
        let (tx, rx) = mpsc::channel(MAX_QUEUE_CHUNKS);
        let budget = Arc::new(Semaphore::new(MAX_QUEUE_BYTES));

        let id = SubscriberId(self.next_subscriber_id);
        self.next_subscriber_id += 1;
        self.subscribers.push(SubscriberSlot {
            id,
            tx,
            budget: Arc::clone(&budget),
        });

        Subscription {
            id,
            rx,
            budget,
            fanout: Arc::clone(fanout),
        }
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

    /// Non-blocking drain, used only when finalizing an `exit` frame: even
    /// with the bounded eof/grace wait in `ws.rs`, a chunk can become ready
    /// in the same poll that decides to finalize, so this mops it up before
    /// `final_offset` is read. `None` covers both "nothing queued right now"
    /// and "the fan-out is gone" -- finalizing treats them the same.
    pub fn try_recv(&mut self) -> Option<Chunk> {
        let chunk = self.rx.try_recv().ok()?;
        self.budget.add_permits(chunk.bytes.len());
        Some(chunk)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut fanout = self.fanout.lock();
        fanout.subscribers.retain(|s| s.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::LogLimits;
    use std::path::PathBuf;

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

        let sub = fanout.lock().register(&fanout);
        assert_eq!(fanout.lock().subscribers.len(), 1);

        drop(sub);
        assert_eq!(
            fanout.lock().subscribers.len(),
            0,
            "Drop must remove the slot without waiting for output"
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
        let mut sub = fanout.lock().register(&fanout);

        fanout.lock().publish(b"abc");
        fanout.lock().publish(b"defgh");

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
            assert_eq!(
                &on_disk[chunk.offset as usize..][..chunk.bytes.len()],
                &*chunk.bytes
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
