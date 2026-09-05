//! Session ownership and backpressure on top of `pty.rs`.
//!
//! See docs/11-mvp-plan.md#m2--session-ownership-and-backpressure and
//! docs/01-architecture.md#session-manager-shape for the design this
//! implements, and docs/03-pty-layer.md#backpressure for the fan-out and
//! queue-bound rules below.
//!
//! **Module layout** (issue #4, split from one 2000+-line file): `types.rs`
//! holds the plain data (`SessionId`, `SessionState`, `SessionMeta`,
//! `Runtime`, `ControlLease`, `SessionEvent`); `fanout.rs` is the
//! `{output log, subscribers}` pair and `Subscription`; `replay.rs` is the
//! catch-up protocol (`Replay`/`ReplayStep`/`Attach`); `control.rs` is the
//! control-lease `impl Session` block; `manager.rs` is `SessionManager` and
//! the exit/EOF listener threads. This file keeps `Session`'s own struct and
//! core methods, and re-exports the public surface so nothing outside this
//! module (`api.rs`, `ws.rs`, `main.rs`) sees the split -- `session::Foo`
//! resolves exactly as it did when this was one file.
//!
//! **M3 update:** the offset counter is no longer a field here -- it moved
//! into `log.rs`'s `OutputLog`, which this module keeps inside the same
//! `Fanout` mutex. That is what makes docs/03-pty-layer.md#reader-loop's
//! ordering -- **persist, then advance the offset, then fan out** -- one
//! call (`OutputLog::append`) instead of three steps a later edit could
//! reorder. The same mutex closes the attach race: the round of
//! [`replay::Replay::next_round`] that registers the subscriber *also* reads
//! the replay boundary `N` under one acquisition, so every chunk that
//! subscriber ever receives starts at or after `N`
//! (docs/04-api-protocol.md#attach-race).
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
//! (`manager::spawn_exit_listener`), the same shape as `pty.rs`'s own reader/
//! writer/reaper/control threads: block on a channel, never poll.
//!
//! **`eof_rx` is consumed too, as of the M4 review's exit-race fix below** --
//! not for `SessionState` (which still derives from `exit_rx` alone, per
//! [S2](../../docs/15-open-questions.md#s2--eof-is-not-exit): `output.vt`
//! keeps growing after `exited` regardless, and always did). What changed is
//! `ws.rs`'s live `exit` frame, which used to fire off `exited` alone and
//! could race ahead of the reader thread: the reaper thread's `wait()` can
//! return before the reader's next `read()` does, so a fast process's last
//! chunk was sometimes still sitting unread when `next_offset()` was read for
//! `final_offset` and the socket closed right after -- a real transcript gap
//! for the live viewer (still recoverable from `output.vt` on reconnect, just
//! not delivered to the connection that had just been told the session was
//! over). `manager::spawn_eof_listener` turns `eof_rx` into a `watch` so
//! `ws.rs` can wait, bounded, for the reader to catch up before finalizing --
//! see its `EXIT_DRAIN_GRACE` comment.
//!
//! **A conflict this milestone surfaced and resolved (see the M4 commit):**
//! the M2-era `terminate()` removed its session from the `SessionManager`
//! directory immediately, and a M2 test asserted exactly that. M4's own spec
//! (docs/04-api-protocol.md#delete-apiv1sessionsid) requires the opposite --
//! a terminated session stays listed as `exited` until an explicit
//! `?purge=true`. `terminate()` no longer self-removes; [`manager::SessionManager::purge`]
//! does the removal now, and the old test was rewritten to match.

mod control;
mod fanout;
mod manager;
mod replay;
mod types;

pub use fanout::{Subscription, MAX_QUEUE_BYTES};
pub use manager::{CreateError, LiveSessions, SessionManager};
pub use replay::{
    Attach, AttachError, HistoryReplay, Replay, ReplayStep, LIVE_GAP_BYTES, REPLAY_ROUND_BYTES,
};
pub use types::{Chunk, SessionEvent, SessionId, SessionLostReason, SessionMeta, SessionState};

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use crate::log::{LogReader, SyncHandle};
use crate::persistence;
use crate::pty::{PtySession, TerminalSession};

use fanout::Fanout;
use types::{ControlLease, Runtime};

/// How long a running session must produce no output before it counts as
/// idle. Not specified numerically anywhere in the docs (docs/13-native-clients.md
/// calls it "tunable per preset, noisy for long builds") -- picked here as a
/// plain default, not a per-preset knob; revisit if it proves noisy. `pub`
/// for the same reason as [`IDLE_SWEEP_INTERVAL_MS`]: `main.rs` needs it.
pub const IDLE_THRESHOLD_MS: i64 = 30_000;

/// How often [`Session::tick_idle`] should be polled -- `main.rs`'s sweep
/// task reads this rather than hardcoding its own copy. `pub`, not
/// `pub(crate)`: `main.rs` is a separate crate (the `teleportd` binary)
/// linking against this lib crate, so `pub(crate)` would not reach it.
pub const IDLE_SWEEP_INTERVAL_MS: u64 = 5_000;

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
    /// Kept only so [`manager::SessionManager::create`] can hand the matching
    /// `Sender` to the exit listener thread; no other code sends on it.
    exited_tx: watch::Sender<bool>,
    /// `true` once the reader thread has observed real EOF on the pty
    /// master. Not a `SessionState` signal ([S2](../../docs/15-open-questions.md#s2--eof-is-not-exit))
    /// -- exists only so `ws.rs` can bound how long it waits for trailing
    /// output after `exited` fires. See `manager::spawn_eof_listener`.
    eof: watch::Receiver<bool>,
    /// Kept only so [`manager::SessionManager::create`] can hand the matching
    /// `Sender` to the eof listener thread; no other code sends on it.
    eof_tx: watch::Sender<bool>,
    /// Flushes the log without touching `fanout`. Deliberately *not* reached
    /// through the lock: an `fsync` held under the mutex the reader thread
    /// takes would stall the PTY behind disk latency, which is the whole
    /// thing docs/05-persistence.md#output-log forbids.
    sync: SyncHandle,
    /// `None` unless [`manager::SessionManager::with_db`] was used -- see the
    /// field of the same name there. `terminate()` and the exit listener use
    /// this directly rather than going back through the manager.
    db: Option<persistence::Db>,
    /// D3 attention signals (docs/04-api-protocol.md#get-apiv1sessions).
    /// `Arc<AtomicI64>` rather than fields behind `runtime`'s `Mutex`: all
    /// three are touched from the PTY reader thread's `on_output` closure
    /// (`last_output_at_ms` and `last_bell_ms`) or a periodic sweep
    /// (`idle_since_ms`), and must never contend with -- or wait behind --
    /// anything the hot path already holds. The same `Arc`s are handed to
    /// `pty::spawn`'s closure in `manager::SessionManager::create`, which is
    /// why they're `Arc` rather than bare atomics: that closure is built
    /// before this `Session` exists to hold them.
    last_output_at_ms: Arc<AtomicI64>,
    /// 0 = never rung.
    last_bell_ms: Arc<AtomicI64>,
    /// 0 = not idle; otherwise the `last_output_at_ms` reading at the moment
    /// output stopped (not "when the threshold was crossed" -- "since when
    /// has this been quiet" is the more useful number for a UI badge).
    idle_since_ms: Arc<AtomicI64>,
}

impl Session {
    /// Registers a new subscriber and returns its receive end, with no
    /// replay. Bounded to [`MAX_QUEUE_BYTES`] of queued output, counting a
    /// fixed per-chunk overhead alongside each payload
    /// (docs/03-pty-layer.md#backpressure); a subscriber that falls behind is
    /// disconnected (`recv()` returns `None`), never blocks the reader.
    ///
    /// Use [`attach`](Self::attach) for anything that needs history -- this
    /// one has no defined starting offset, so what happened before the call
    /// is simply lost to that subscriber.
    pub fn subscribe(&self) -> Subscription {
        let mut fanout = self.fanout.lock();
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
        let fanout = self.fanout.lock();

        let next_offset = fanout.log.next_offset();
        if from > next_offset {
            return Err(AttachError::OffsetAhead {
                requested: from,
                next_offset,
            });
        }
        let log_capped_at = fanout.log.log_capped_at();
        let readable_end = fanout.log.readable_end();
        let reader = fanout.log.reader().map_err(AttachError::Open)?;
        drop(fanout);

        // Bytes between a cap and `next_offset` were streamed live and are
        // gone. A client asking for them gets no replay and is told where the
        // stream resumes, rather than being served whatever happens to sit at
        // that file position (docs/05-persistence.md#size-cap).
        let replay_from = if from >= next_offset.min(readable_end) {
            next_offset
        } else {
            from
        };

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
        self.fanout.lock().log.next_offset()
    }

    pub fn log_capped_at(&self) -> Option<u64> {
        self.fanout.lock().log.log_capped_at()
    }

    pub fn log_path(&self) -> PathBuf {
        self.fanout.lock().log.path().to_path_buf()
    }

    /// A fresh read handle for a one-shot range read -- `GET
    /// /api/v1/sessions/{id}/log`'s job (docs/04-api-protocol.md#get-apiv1sessionsidlog).
    /// Opening under the lock, same as [`attach`](Self::attach), saves the
    /// caller from reasoning about the file moving; reading the bytes does
    /// not need the lock at all.
    pub fn log_reader(&self) -> std::io::Result<LogReader> {
        self.fanout.lock().log.reader()
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
            let mut runtime = self.runtime.lock();
            runtime.cols = cols;
            runtime.rows = rows;
        }
        // Fire-and-forget, same as the throttled `output_bytes` write --
        // resizes are rare (a user action, not the hot path), but there is
        // still no reason to make the caller wait on disk for it
        // (docs/05-persistence.md#when-output_bytes-is-written).
        if let Some(db) = &self.db {
            db.note_size(&self.id.to_string(), cols, rows);
        }
        let _ = self.events.send(SessionEvent::Resized { cols, rows });
        Ok(())
    }

    /// The PTY's current size -- `ready.cols`/`ready.rows` and every `GET`
    /// response's `cols`/`rows` read this, not the size the session was
    /// created with (docs/04-api-protocol.md#control-messages: "there is
    /// exactly one PTY geometry per session").
    pub fn size(&self) -> (u16, u16) {
        let r = self.runtime.lock();
        (r.cols, r.rows)
    }

    pub fn state(&self) -> SessionState {
        self.runtime.lock().state
    }

    pub fn pid(&self) -> Option<u32> {
        self.runtime.lock().pid
    }

    pub fn created_at_ms(&self) -> i64 {
        self.runtime.lock().created_at_ms
    }

    pub fn started_at_ms(&self) -> Option<i64> {
        self.runtime.lock().started_at_ms
    }

    pub fn exited_at_ms(&self) -> Option<i64> {
        self.runtime.lock().exited_at_ms
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.runtime.lock().exit_code
    }

    pub fn lost_reason(&self) -> Option<SessionLostReason> {
        self.runtime.lock().lost_reason
    }

    /// Live subscriber count -- `GET /api/v1/sessions`'s `subscribers` field
    /// (docs/04-api-protocol.md#get-apiv1sessions).
    pub fn subscriber_count(&self) -> usize {
        self.fanout.lock().subscribers.len()
    }

    /// `GET /api/v1/sessions`'s `last_bell_ms` (docs/04-api-protocol.md,
    /// D3). `None` if a BEL byte has never appeared in this session's
    /// output.
    pub fn last_bell_ms(&self) -> Option<i64> {
        match self.last_bell_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        }
    }

    /// `GET /api/v1/sessions`'s `idle_since_ms` (docs/04-api-protocol.md,
    /// D3). `None` while output is flowing or the session isn't running;
    /// otherwise the timestamp output last stopped.
    pub fn idle_since_ms(&self) -> Option<i64> {
        match self.idle_since_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        }
    }

    /// Called from `main.rs`'s idle-sweep task, once per session per tick
    /// (docs/04-api-protocol.md#get-apiv1sessions). Not
    /// on the PTY hot path -- `state()` and `note_event` are fine to touch
    /// here even though they'd be wrong to touch from `on_output`.
    ///
    /// `threshold_ms` is a parameter rather than reading the `IDLE_THRESHOLD_MS`
    /// constant directly so tests can drive this with synthetic clocks and a
    /// short threshold instead of sleeping 30 real seconds; `main.rs` always
    /// passes the constant.
    pub fn tick_idle(&self, now_ms: i64, threshold_ms: i64) {
        if self.state() != SessionState::Running {
            return;
        }
        let last_output = self.last_output_at_ms.load(Ordering::Relaxed);
        let was_idle = self.idle_since_ms.load(Ordering::Relaxed) != 0;
        if now_ms - last_output >= threshold_ms {
            if !was_idle {
                self.idle_since_ms.store(last_output, Ordering::Relaxed);
                if let Some(db) = &self.db {
                    db.note_event(&self.id.to_string(), "idle");
                }
            }
        } else if was_idle {
            self.idle_since_ms.store(0, Ordering::Relaxed);
        }
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

    /// An owned `watch::Receiver` for a caller (`ws.rs`) that needs to hold
    /// it across a `tokio::select!` loop. Distinct from [`watch_exited`],
    /// and not a substitute for it: this says "the reader thread has drained
    /// the pty to EOF," which `ws.rs` uses only to bound the `exit` frame's
    /// drain grace after `exited` fires -- never for `SessionState`
    /// ([S2](../../docs/15-open-questions.md#s2--eof-is-not-exit)).
    pub fn watch_eof(&self) -> watch::Receiver<bool> {
        self.eof.clone()
    }

    /// A fresh receiver for [`SessionEvent`]s -- one per WS connection, so a
    /// slow or absent reader on one connection cannot back up another's
    /// (docs/04-api-protocol.md#control-messages).
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
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
    /// see [`manager::SessionManager::purge`]. Idempotent, same as the
    /// underlying `pty.terminate()`.
    pub fn terminate(&self) -> Result<()> {
        {
            let mut runtime = self.runtime.lock();
            if runtime.state == SessionState::Running {
                runtime.state = SessionState::Closing;
            }
        }
        // Best effort, same as every other DB write from this struct: a
        // stale `closing`/`running` row is exactly what restart recovery
        // already knows how to fix (docs/05-persistence.md#restart-recovery),
        // so a failure here degrades to "recovery has slightly stale
        // information," never to a stuck PTY teardown.
        if let Some(db) = &self.db {
            if let Err(e) = db.mark_closing_blocking(&self.id.to_string()) {
                warn!(session_id = %self.id, error = %e, "persisting the closing transition failed");
            }
        }
        let result = self.pty.terminate();
        self.sync_log();
        result
    }
}
