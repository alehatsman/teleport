//! `SessionManager` -- owns every live session, enforces `max_sessions`,
//! spawns the exit/EOF listener threads, and resolves an executable before
//! ever forking. See the parent module doc (`session/mod.rs`) and
//! docs/01-architecture.md#session-manager-shape for the design this
//! implements.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use tokio::sync::{broadcast, watch};
use tracing::warn;

use crate::log::{LogEvent, LogLimits, LogSyncer, OutputLog};
use crate::now_ms;
use crate::persistence;
use crate::pty::{self, SpawnSpec};

use super::fanout::Fanout;
use super::types::{
    ControlLease, Runtime, SessionId, SessionLostReason, SessionMeta, SessionState,
    EVENT_CHANNEL_CAPACITY,
};
use super::Session;

/// docs/05-persistence.md#when-output_bytes-is-written: "at most once per
/// second per session".
const OUTPUT_BYTES_PERSIST_INTERVAL_MS: i64 = 1000;

/// D3 (docs/04-api-protocol.md#get-apiv1sessions): a BEL
/// byte can repeat fast (a spinner, a broken script) -- throttle the
/// `session_events` write the same way output_bytes is throttled. The
/// in-memory `last_bell_ms` (what `GET` actually reports) always reflects the
/// most recent bell regardless of this throttle.
const BELL_PERSIST_INTERVAL_MS: i64 = 1000;

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
        *self.reserved.lock() -= 1;
    }
}

type SessionDirectory = Mutex<HashMap<SessionId, Arc<Session>>>;

/// A cheap, `Clone`-able handle onto `SessionManager`'s live map -- just
/// enough for `main.rs`'s GC pass to ask "is this id still live" without
/// pulling in the rest of `SessionManager` (the `LogSyncer` thread, cap
/// state). Shares the same underlying map, so it always reflects current
/// membership; see [`SessionManager::live_handle`].
#[derive(Clone)]
pub struct LiveSessions(Arc<SessionDirectory>);

impl LiveSessions {
    /// Whether `id` is still tracked live (`Running`, `Closing`, or
    /// `Exited` but not yet purged). GC must never delete a row/directory
    /// this says yes to (docs/05-persistence.md#garbage-collection):
    /// `api.rs`'s `find_session` checks this same map first, so a live id
    /// is always served from here, never the DB fallback -- deleting its
    /// directory out from under that would break `/log` while `GET` keeps
    /// returning 200. An id that fails to parse as a `SessionId` can't be
    /// live by construction.
    pub fn contains(&self, id: &str) -> bool {
        let Ok(id) = id.parse::<SessionId>() else {
            return false;
        };
        self.0.lock().contains_key(&id)
    }
}

/// Owns every live session. One lock for the session directory itself,
/// separate from each `Session`'s own `fanout` lock -- creating or looking up
/// a session never contends with another session's hot output path.
/// `Session` keeps no back-link to it: [`SessionManager::purge`] removes by
/// id through this map directly, and `terminate()` no longer self-removes
/// (see the M4 module doc on `session/mod.rs`).
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
    /// `None` in every test that has no reason to touch SQLite (the large
    /// majority -- PTY/backpressure/replay/protocol coverage predates M7 and
    /// stays that way). Every write site below is a no-op when this is
    /// `None`; `main.rs` is the only caller that sets it via
    /// [`SessionManager::with_db`].
    db: Option<persistence::Db>,
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
            db: None,
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

    /// Wires SQLite persistence in (docs/11-mvp-plan.md#m7). `main.rs` is
    /// the only real caller; tests that don't exercise M7 leave `db: None`
    /// and every write below quietly no-ops.
    pub fn with_db(mut self, db: persistence::Db) -> Self {
        self.db = Some(db);
        self
    }

    /// `<data_dir>/sessions` -- `api.rs`'s log-fallback path for a session id
    /// not held live needs this to open `<root>/<id>/output.vt` directly
    /// (docs/05-persistence.md#layout), without going through a `Session`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A persistent, cheap-to-clone handle for `main.rs`'s GC task to check
    /// live membership against, without a full `SessionManager` clone.
    pub fn live_handle(&self) -> LiveSessions {
        LiveSessions(Arc::clone(&self.sessions))
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
        let sessions = self.sessions.lock();
        let mut reserved = self.reserved.lock();
        let live = sessions
            .values()
            .filter(|s| s.runtime.lock().state != SessionState::Exited)
            .count();
        if live + *reserved >= self.max_sessions {
            return Err(CreateError::MaxSessions(self.max_sessions));
        }
        *reserved += 1;
        Ok(ReservationGuard {
            reserved: &self.reserved,
        })
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
        let created_at_ms = now_ms();

        // A brand-new session has no stored row; `None` is the fresh-start
        // case of docs/05-persistence.md#restart-recovery. Recovered rows
        // never reach this path -- they are not `Session`s at all
        // (persistence.rs's module doc explains why).
        let log = OutputLog::open(&self.root.join(id.to_string()), self.limits, None)?;
        let sync = log.sync_handle();
        self.syncer.register(&sync);
        let fanout = Arc::new(Mutex::new(Fanout::new(log)));

        // Insert *before* `pty::spawn` -- the row must exist before the
        // child can produce a single byte
        // (docs/01-architecture.md#session-creation-sequence). Best effort:
        // a DB write failing here does not stop a session from working live,
        // it just means a restart won't know this one ever existed (`db` is
        // `None` in most tests, and a real runtime DB failure is already
        // fatal at daemon startup, not here).
        if let Some(db) = &self.db {
            let row = persistence::NewSessionRow {
                id: id.to_string(),
                kind: meta.kind.clone(),
                preset: meta.preset.clone(),
                command: meta.command.clone(),
                args: meta.args.clone(),
                cwd: meta.cwd.display().to_string(),
                // Not yet known -- `pty::spawn` hasn't run (the row must
                // exist before it does). Left `NULL`; nothing in the MVP
                // needs a recovered/historical row's `pid`, only a live
                // `Session::pid()` does, and that never reads this column.
                pid: None,
                cols,
                rows,
                created_at_ms,
            };
            if let Err(e) = db.insert_session_blocking(row) {
                warn!(session_id = %id, error = %e, "persisting the new session row failed");
            }
        }

        // Fire-and-forget, throttled to at most once/second while output is
        // flowing (docs/05-persistence.md#when-output_bytes-is-written) --
        // `last_persist_ms` is shared with nothing else, so the check is a
        // lock-free compare-and-swap, never the reader thread waiting on
        // SQLite.
        let db_for_output = self.db.clone();
        let last_persist_ms = Arc::new(AtomicI64::new(0));
        let publish_fanout = Arc::clone(&fanout);
        // D3 (docs/04-api-protocol.md#get-apiv1sessions):
        // built here, before `Session` exists, same reason `last_persist_ms`
        // above is -- the closure has to own its own handles to what it
        // updates.
        let last_output_at_ms = Arc::new(AtomicI64::new(created_at_ms));
        let last_bell_ms = Arc::new(AtomicI64::new(0));
        let idle_since_ms = Arc::new(AtomicI64::new(0));
        let last_bell_persist_ms = Arc::new(AtomicI64::new(0));
        let output_for_closure = Arc::clone(&last_output_at_ms);
        let bell_for_closure = Arc::clone(&last_bell_ms);
        let spawned = pty::spawn(spec, move |bytes| {
            let (events, next_offset) = {
                let mut fanout = publish_fanout.lock();
                let events = fanout.publish(bytes);
                (events, fanout.log.next_offset())
            };
            trace_log_events(id, &events);
            let now = now_ms();
            output_for_closure.store(now, Ordering::Relaxed);
            // BEL detection (docs/13-native-clients.md#detection-heuristics:
            // "the reader loop already scans every byte"). Every occurrence
            // updates the in-memory reading; the `session_events` write is
            // throttled below so a spinner or broken script can't flood it.
            if bytes.contains(&0x07) {
                bell_for_closure.store(now, Ordering::Relaxed);
                if let Some(db) = &db_for_output {
                    let last = last_bell_persist_ms.load(Ordering::Relaxed);
                    if now - last >= BELL_PERSIST_INTERVAL_MS
                        && last_bell_persist_ms
                            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        db.note_event(&id.to_string(), "bell");
                    }
                }
            }
            if let Some(db) = &db_for_output {
                let last = last_persist_ms.load(Ordering::Relaxed);
                if now - last >= OUTPUT_BYTES_PERSIST_INTERVAL_MS
                    && last_persist_ms
                        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    db.note_output_bytes(&id.to_string(), next_offset);
                }
            }
        });
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(e) => {
                // The row inserted above must not survive a spawn that never
                // happened -- left behind, it would sit forever as a phantom
                // `running`/pid-less row: `list`/`get` would serve it via
                // the historical-row fallback (this id was never inserted
                // into `self.sessions`), and GC's candidates are only
                // `exited`/`lost`, so nothing would ever clean it up short
                // of a restart forcing it to `lost`.
                if let Some(db) = &self.db {
                    if let Err(e) = db.delete_session_blocking(&id.to_string()) {
                        warn!(session_id = %id, error = %e, "rolling back the session row after a failed spawn");
                    }
                }
                return Err(CreateError::Spawn(e));
            }
        };

        let (exited_tx, exited) = watch::channel(false);
        let (eof_tx, eof) = watch::channel(false);
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
            eof,
            eof_tx,
            sync,
            db: self.db.clone(),
            last_output_at_ms,
            last_bell_ms,
            idle_since_ms,
        });
        self.sessions.lock().insert(id, Arc::clone(&session));
        spawn_exit_listener(Arc::clone(&session), spawned.exit_rx);
        spawn_eof_listener(Arc::clone(&session), spawned.eof_rx);
        Ok(session)
    }

    pub fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().get(&id).cloned()
    }

    /// Every live session, for `GET /api/v1/sessions`. No ordering promise --
    /// `api.rs` sorts newest-first (docs/04-api-protocol.md#get-apiv1sessions).
    pub fn list(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().values().cloned().collect()
    }

    /// `?purge=true`: removes the session from the directory (directory
    /// entry first is not a concern here -- `api.rs` deletes
    /// `data/sessions/{id}/` before calling this, matching the collector's
    /// own directory-first-row-second ordering,
    /// docs/05-persistence.md#garbage-collection). A no-op if the id is
    /// already gone. Idempotent by construction (`HashMap::remove`).
    pub fn purge(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().remove(&id)
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
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        return is_executable_file(&resolved);
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| is_executable_file(&dir.join(command)))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
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
    ["exe", "cmd", "bat", "ps1"]
        .iter()
        .any(|ext| path.with_extension(ext).is_file())
}

/// A cap, a warning threshold or a failed write is operator-visible now and a
/// `session_events` row once M7 exists; nothing on the reader thread may
/// touch SQLite, so the log hands these back and they get traced here.
fn trace_log_events(id: SessionId, events: &[LogEvent]) {
    for event in events {
        warn!(session_id = %id, ?event, "output log event");
    }
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

            let exited_at_ms = now_ms();
            {
                let mut runtime = session.runtime.lock();
                runtime.state = SessionState::Exited;
                runtime.exit_code = exit_code;
                runtime.lost_reason = lost_reason;
                runtime.exited_at_ms = Some(exited_at_ms);
            }
            session.sync_log();
            // The one terminal-state write a live `Session` ever makes --
            // always `state='exited'`, never `'lost'` (that only happens via
            // restart recovery, on a row with no live `Session` behind it --
            // see persistence.rs's module doc). Best effort, same reasoning
            // as `terminate()`'s write above.
            if let Some(db) = &session.db {
                if let Err(e) = db.mark_exited_blocking(
                    &session.id.to_string(),
                    exited_at_ms,
                    exit_code,
                    lost_reason.map(|r| r.as_str()),
                    session.next_offset(),
                ) {
                    warn!(session_id = %session.id, error = %e, "persisting the exited transition failed");
                }
            }
            // `send` on a `watch` never fails as long as this `Sender` (owned
            // by `session`, which this thread also holds a strong ref to) is
            // alive -- it always is here.
            let _ = session.exited_tx.send(true);
        })
        .expect("spawning session-exit thread");
}

/// Consumes `eof_rx` on its own thread -- same shape as
/// [`spawn_exit_listener`]. Not a state-machine signal: this exists solely
/// so `ws.rs`'s `exit`-frame finalization can tell "the reader thread has
/// genuinely caught up" apart from "the reader thread just hasn't been
/// scheduled yet" (the race described in the parent module's doc comment).
/// A closed channel with no message (the reader thread panicked before ever
/// reaching EOF) is left as "eof never observed" -- `ws.rs`'s bounded grace
/// timeout is exactly what keeps that from hanging a connection.
fn spawn_eof_listener(session: Arc<Session>, eof_rx: std::sync::mpsc::Receiver<()>) {
    std::thread::Builder::new()
        .name("session-eof".into())
        .spawn(move || {
            if eof_rx.recv().is_ok() {
                let _ = session.eof_tx.send(true);
            }
        })
        .expect("spawning session-eof thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M4 review: a relative literal path (contains a separator) used to be
    /// checked against the daemon's own cwd, not the session's requested
    /// `cwd` -- what `pty::spawn` actually spawns in -- so a valid
    /// `./relative/script` could be wrongly rejected with 422.
    #[test]
    fn resolve_executable_checks_a_relative_command_against_the_session_cwd() {
        let dir = std::env::temp_dir().join(format!(
            "teleportd-session-unit-resolve-executable-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(!resolve_executable(
            "./run.sh",
            &std::env::temp_dir().join("not-the-right-place")
        ));
        assert!(resolve_executable("./run.sh", &dir));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
