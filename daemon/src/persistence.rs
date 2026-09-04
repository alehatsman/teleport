//! SQLite metadata store: schema, migrations, single writer actor, startup
//! recovery (docs/05-persistence.md, docs/11-mvp-plan.md#m7--sqlite-metadata-and-recovery).
//!
//! **One writer, no pool** (docs/05-persistence.md#one-writer-no-pool): a
//! dedicated OS thread owns the one [`rusqlite::Connection`] for the life of
//! the daemon, fed by an `mpsc` command channel, replying over `oneshot`.
//! Every other module talks to it through [`Db`], a cheap `Clone` wrapper
//! around the sender half. Reads go through the same actor -- the query
//! volume is trivial, and a single path is simpler than a pool plus a
//! writer.
//!
//! [`Db`] exposes two flavors of the same calls: `_blocking` methods
//! (`blocking_send` + `blocking_recv`) for `session.rs`, which is entirely
//! synchronous (`Mutex`es and OS threads, no `async`); plain `async fn`s for
//! `api.rs`'s handlers, which already run on the Tokio executor and must
//! never block a worker thread. `note_*` methods are fire-and-forget
//! (`try_send`, no reply) -- the one path reachable from the PTY reader
//! thread, which must never wait on SQLite
//! (docs/05-persistence.md#output-log: "an `fsync` held there stalls the
//! PTY... put the reader thread at risk of falling behind").
//!
//! **Scope cut, recorded here rather than made silently:** `session_events`'
//! schema is the full set from docs/05-persistence.md#schema, but M7 only
//! ever inserts `created`, `exited` and `lost` -- the lifecycle events that
//! already have a natural chokepoint in `session.rs` (there is no separate
//! `started` moment to record: `started_at_ms == created_at_ms` always in
//! this codebase, so a `started` event would be a pure duplicate of
//! `created`). `resized`, `control_granted`, `control_revoked`,
//! `subscriber_attached`, `subscriber_detached`, `slow_consumer`, `bell` and
//! `idle` would each need new plumbing through `ws.rs` / the control lease /
//! the reader loop -- out of proportion to this milestone's gate (a
//! SIGKILL-and-restart test, docs/11-mvp-plan.md#m7). The table already has
//! the room; wiring the rest is future work, not a design dead end.
//!
//! **A second scope cut in the same spirit:** `sessions.log_capped_at` is
//! never written by this module -- it stays `NULL` forever, even once a live
//! log actually caps (`log.rs`'s `LogEvent::Capped`/`IoError` reach
//! `session.rs`'s `trace_log_events` today exactly as before M7, a `warn!`
//! and nothing else). This does not corrupt anything: `LogReader::read_range`
//! self-clamps to the real file length regardless of what a caller asks for,
//! so a historical/recovered row's `/log` response is always exactly the
//! bytes on disk, cap or no cap. What's missing is purely informational --
//! `GET`'s `log_capped_at` field reads `null` for a session that actually
//! capped before a restart, instead of the offset. Closing this needs the
//! same `note_*` fire-and-forget wiring as `output_bytes`/`cols`/`rows`
//! above; left for whoever touches capping next; not needed for this
//! milestone's gate.
//!
//! **Recovered sessions are not [`crate::session::Session`]s.** A `Session`
//! always owns a live `PtySession` -- there is no such thing as a `Session`
//! with no PTY (docs/01-architecture.md#the-crash-boundary: "SQLite can
//! remember that a process used to exist; it cannot recreate an OS PTY
//! handle"). So a row surviving a restart with no matching in-process
//! `Session` is served straight from this module: `api.rs` falls back to
//! [`Db::get_session`] / [`Db::list_sessions`] for metadata, and to
//! `log.rs`'s standalone `LogReader::open` for its log, rather than this
//! module reconstructing a fake live session.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as i64
}

/// A `sessions` row, read back. `args` is `argv_json` already parsed --
/// nothing downstream should touch the JSON encoding directly.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub kind: String,
    pub preset: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub state: String,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub output_bytes: u64,
    pub log_capped_at: Option<u64>,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub exited_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    pub lost_reason: Option<String>,
}

fn row_from_sql(row: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    let argv_json: String = row.get("argv_json")?;
    let args: Vec<String> = serde_json::from_str(&argv_json).unwrap_or_default();
    Ok(SessionRow {
        id: row.get("id")?,
        kind: row.get("kind")?,
        preset: row.get("preset")?,
        command: row.get("command")?,
        args,
        cwd: row.get("cwd")?,
        state: row.get("state")?,
        pid: row.get::<_, Option<i64>>("pid")?.map(|v| v as u32),
        cols: row.get::<_, i64>("cols")? as u16,
        rows: row.get::<_, i64>("rows")? as u16,
        output_bytes: row.get::<_, i64>("output_bytes")? as u64,
        log_capped_at: row
            .get::<_, Option<i64>>("log_capped_at")?
            .map(|v| v as u64),
        created_at_ms: row.get("created_at_ms")?,
        started_at_ms: row.get("started_at_ms")?,
        exited_at_ms: row.get("exited_at_ms")?,
        exit_code: row.get("exit_code")?,
        lost_reason: row.get("lost_reason")?,
    })
}

/// What `SessionManager::create` has in hand at the point
/// docs/01-architecture.md#session-creation-sequence inserts the row --
/// after `output.vt` is open, before `openpty`/`spawn_command`. Always
/// inserts as `state='running'`, `started_at_ms == created_at_ms` (see the
/// module doc on why there is no separate `started` write).
#[derive(Debug, Clone)]
pub struct NewSessionRow {
    pub id: String,
    pub kind: String,
    pub preset: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub created_at_ms: i64,
}

/// What a startup [`Db::open`] found and fixed, for `main.rs` to log.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecoverySummary {
    /// Rows that were `running`/`closing` and are now `lost` /
    /// `daemon_restart` (docs/01-architecture.md#the-crash-boundary).
    pub recovered_lost: usize,
}

enum Command {
    Insert(NewSessionRow, oneshot::Sender<Result<()>>),
    MarkClosing {
        id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    MarkExited {
        id: String,
        exited_at_ms: i64,
        exit_code: Option<i32>,
        lost_reason: Option<&'static str>,
        output_bytes: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Fire-and-forget throttled write (docs/05-persistence.md#when-output_bytes-is-written).
    NoteOutputBytes {
        id: String,
        output_bytes: u64,
    },
    /// Fire-and-forget; `resize()` is a rare user action, not hot-path, but
    /// there is still no reason to make the caller wait on disk for it.
    NoteSize {
        id: String,
        cols: u16,
        rows: u16,
    },
    NoteEvent {
        id: String,
        event_type: &'static str,
        ts_ms: i64,
    },
    Delete {
        id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Get {
        id: String,
        reply: oneshot::Sender<Result<Option<SessionRow>>>,
    },
    List {
        reply: oneshot::Sender<Result<Vec<SessionRow>>>,
    },
    /// `exited`/`lost` rows whose `exited_at_ms` is older than the cutoff --
    /// GC candidates (docs/05-persistence.md#garbage-collection).
    GcCandidates {
        older_than_ms: i64,
        reply: oneshot::Sender<Result<Vec<SessionRow>>>,
    },
}

/// A cheap, `Clone`-able handle to the writer actor's command channel.
#[derive(Clone)]
pub struct Db {
    tx: mpsc::Sender<Command>,
}

impl Db {
    /// Opens `db_path` (creating it if absent), applies pragmas and
    /// migrations, runs restart recovery against `sessions_root`, then
    /// spawns the writer thread and returns a handle plus what recovery
    /// found (docs/01-architecture.md#startup-sequence).
    pub fn open(db_path: &Path, sessions_root: &Path) -> Result<(Db, RecoverySummary)> {
        let conn =
            Connection::open(db_path).with_context(|| format!("opening {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("setting journal_mode=WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("setting synchronous=NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("setting foreign_keys=ON")?;
        conn.pragma_update(None, "busy_timeout", 5000i64)
            .context("setting busy_timeout")?;

        run_migrations(&conn).context("running migrations")?;
        let summary = recover(&conn, sessions_root).context("running restart recovery")?;

        let (tx, rx) = mpsc::channel(256);
        std::thread::Builder::new()
            .name("db-writer".into())
            .spawn(move || writer_loop(conn, rx))
            .context("spawning db-writer thread")?;

        Ok((Db { tx }, summary))
    }

    fn call_blocking<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
    ) -> Result<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .blocking_send(make(reply_tx))
            .map_err(|_| anyhow!("db-writer thread is gone"))?;
        reply_rx
            .blocking_recv()
            .map_err(|_| anyhow!("db-writer thread dropped the reply"))?
    }

    async fn call<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Command) -> Result<T>
    where
        T: Send,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(make(reply_tx))
            .await
            .map_err(|_| anyhow!("db-writer thread is gone"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("db-writer thread dropped the reply"))?
    }

    /// Blocking insert -- `SessionManager::create` runs on a blocking-pool
    /// thread already (`spawn_blocking` in `api.rs`), and this must complete
    /// before `pty::spawn` so the row exists before the child can produce a
    /// single byte (docs/01-architecture.md#session-creation-sequence).
    pub fn insert_session_blocking(&self, row: NewSessionRow) -> Result<()> {
        self.call_blocking(|reply| Command::Insert(row, reply))
    }

    /// `running`/`lost` -> `closing`, from `Session::terminate()`. Best
    /// effort at the call site -- a DB write failing here must not block
    /// tearing down the PTY.
    pub fn mark_closing_blocking(&self, id: &str) -> Result<()> {
        self.call_blocking(|reply| Command::MarkClosing {
            id: id.to_string(),
            reply,
        })
    }

    /// The one terminal-state write a live `Session` ever makes -- always
    /// `state='exited'` (docs/05-persistence.md: `lost_reason` can be set
    /// here too, for `spawn_failed`/`kill_timeout`/`wait_error`, but `state`
    /// only becomes `'lost'` via restart recovery, never from a live
    /// process -- see [`recover`]).
    #[allow(clippy::too_many_arguments)]
    pub fn mark_exited_blocking(
        &self,
        id: &str,
        exited_at_ms: i64,
        exit_code: Option<i32>,
        lost_reason: Option<&'static str>,
        output_bytes: u64,
    ) -> Result<()> {
        self.call_blocking(|reply| Command::MarkExited {
            id: id.to_string(),
            exited_at_ms,
            exit_code,
            lost_reason,
            output_bytes,
            reply,
        })
    }

    pub fn delete_session_blocking(&self, id: &str) -> Result<()> {
        self.call_blocking(|reply| Command::Delete {
            id: id.to_string(),
            reply,
        })
    }

    /// Fire-and-forget: dropped silently (after a `warn!`) if the channel is
    /// momentarily full rather than ever blocking the caller -- the reader
    /// thread's own throttle already caps how often this fires
    /// (docs/05-persistence.md#when-output_bytes-is-written: "at most once
    /// per second per session").
    pub fn note_output_bytes(&self, id: &str, output_bytes: u64) {
        if self
            .tx
            .try_send(Command::NoteOutputBytes {
                id: id.to_string(),
                output_bytes,
            })
            .is_err()
        {
            warn!(
                session_id = id,
                "db-writer channel full or gone; dropped an output_bytes update"
            );
        }
    }

    pub fn note_size(&self, id: &str, cols: u16, rows: u16) {
        if self
            .tx
            .try_send(Command::NoteSize {
                id: id.to_string(),
                cols,
                rows,
            })
            .is_err()
        {
            warn!(
                session_id = id,
                "db-writer channel full or gone; dropped a size update"
            );
        }
    }

    pub fn note_event(&self, id: &str, event_type: &'static str) {
        if self
            .tx
            .try_send(Command::NoteEvent {
                id: id.to_string(),
                event_type,
                ts_ms: now_ms(),
            })
            .is_err()
        {
            warn!(
                session_id = id,
                event_type, "db-writer channel full or gone; dropped an event"
            );
        }
    }

    /// For `api.rs`'s async handlers: a session id not held live by
    /// `SessionManager` falls back to this for `GET`.
    pub async fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        self.call(|reply| Command::Get {
            id: id.to_string(),
            reply,
        })
        .await
    }

    /// Newest-first is `api.rs`'s job (same convention as
    /// `SessionManager::list`); this returns every row, unordered.
    pub async fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        self.call(|reply| Command::List { reply }).await
    }

    pub async fn delete_session(&self, id: &str) -> Result<()> {
        self.call(|reply| Command::Delete {
            id: id.to_string(),
            reply,
        })
        .await
    }

    pub async fn gc_candidates(&self, older_than_ms: i64) -> Result<Vec<SessionRow>> {
        self.call(|reply| Command::GcCandidates {
            older_than_ms,
            reply,
        })
        .await
    }
}

/// Ordered, additive migrations. `PRAGMA user_version` is the counter: on
/// open, every migration at an index `>= user_version` runs, then
/// `user_version` is set to `MIGRATIONS.len()` (docs/05-persistence.md#migrations).
/// No migration framework, no external tool.
const MIGRATIONS: &[&str] = &[SCHEMA_V1];

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    preset          TEXT,
    command         TEXT NOT NULL,
    argv_json       TEXT NOT NULL,
    cwd             TEXT NOT NULL,

    state           TEXT NOT NULL,
    pid             INTEGER,

    cols            INTEGER NOT NULL,
    rows            INTEGER NOT NULL,

    output_bytes    INTEGER NOT NULL DEFAULT 0,
    log_capped_at   INTEGER,

    created_at_ms   INTEGER NOT NULL,
    started_at_ms   INTEGER,
    exited_at_ms    INTEGER,
    exit_code       INTEGER,
    lost_reason     TEXT
);

CREATE TABLE IF NOT EXISTS session_events (
    event_id        INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL,
    ts_ms           INTEGER NOT NULL,
    event_type      TEXT NOT NULL,
    data_json       TEXT,
    FOREIGN KEY(session_id) REFERENCES sessions(id)
);

CREATE INDEX IF NOT EXISTS idx_sessions_state ON sessions(state);
CREATE INDEX IF NOT EXISTS idx_events_session ON session_events(session_id, event_id);
"#;

fn run_migrations(conn: &Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let current = current.max(0) as usize;
    for migration in MIGRATIONS.iter().skip(current) {
        conn.execute_batch(migration)?;
    }
    if MIGRATIONS.len() > current {
        conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
    }
    Ok(())
}

/// docs/01-architecture.md#startup-sequence's middle two steps, run once
/// synchronously before the writer thread starts (nothing else can be
/// touching the DB yet):
///
/// 1. every row still `running`/`closing` -> `lost` / `daemon_restart`,
///    with a matching `session_events(lost)` row.
/// 2. every session directory's `output.vt` length reconciled against the
///    stored `output_bytes`, **file wins** if larger
///    (docs/05-persistence.md#restart-recovery: "the file cannot lie about
///    bytes it holds"; a capped log is the one case the column can be ahead,
///    and `MAX()` in the `UPDATE` below leaves those alone).
fn recover(conn: &Connection, sessions_root: &Path) -> Result<RecoverySummary> {
    let now = now_ms();

    let stale_ids: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT id FROM sessions WHERE state IN ('running', 'closing')")?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids
    };

    conn.execute(
        "UPDATE sessions SET state = 'lost', lost_reason = 'daemon_restart', exited_at_ms = ?1
         WHERE state IN ('running', 'closing')",
        params![now],
    )?;
    for id in &stale_ids {
        conn.execute(
            "INSERT INTO session_events (session_id, ts_ms, event_type, data_json) VALUES (?1, ?2, 'lost', NULL)",
            params![id, now],
        )?;
    }

    let ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM sessions")?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids
    };
    for id in ids {
        let log_path = sessions_root.join(&id).join("output.vt");
        let Ok(meta) = std::fs::metadata(&log_path) else {
            continue;
        };
        conn.execute(
            "UPDATE sessions SET output_bytes = MAX(output_bytes, ?1) WHERE id = ?2",
            params![meta.len() as i64, id],
        )?;
    }

    Ok(RecoverySummary {
        recovered_lost: stale_ids.len(),
    })
}

fn insert_session(conn: &Connection, row: &NewSessionRow) -> Result<()> {
    let argv_json = serde_json::to_string(&row.args)?;
    conn.execute(
        "INSERT INTO sessions
            (id, kind, preset, command, argv_json, cwd, state, pid, cols, rows,
             output_bytes, log_capped_at, created_at_ms, started_at_ms, exited_at_ms,
             exit_code, lost_reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, ?8, ?9, 0, NULL, ?10, ?10, NULL, NULL, NULL)",
        params![
            row.id,
            row.kind,
            row.preset,
            row.command,
            argv_json,
            row.cwd,
            row.pid.map(|p| p as i64),
            row.cols as i64,
            row.rows as i64,
            row.created_at_ms,
        ],
    )?;
    conn.execute(
        "INSERT INTO session_events (session_id, ts_ms, event_type, data_json) VALUES (?1, ?2, 'created', NULL)",
        params![row.id, row.created_at_ms],
    )?;
    Ok(())
}

fn mark_closing(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET state = 'closing' WHERE id = ?1 AND state = 'running'",
        params![id],
    )?;
    Ok(())
}

fn mark_exited(
    conn: &Connection,
    id: &str,
    exited_at_ms: i64,
    exit_code: Option<i32>,
    lost_reason: Option<&str>,
    output_bytes: u64,
) -> Result<()> {
    conn.execute(
        "UPDATE sessions
         SET state = 'exited', exited_at_ms = ?1, exit_code = ?2, lost_reason = ?3, output_bytes = ?4
         WHERE id = ?5",
        params![exited_at_ms, exit_code, lost_reason, output_bytes as i64, id],
    )?;
    conn.execute(
        "INSERT INTO session_events (session_id, ts_ms, event_type, data_json) VALUES (?1, ?2, 'exited', NULL)",
        params![id, exited_at_ms],
    )?;
    Ok(())
}

fn get_session(conn: &Connection, id: &str) -> Result<Option<SessionRow>> {
    conn.query_row(
        "SELECT * FROM sessions WHERE id = ?1",
        params![id],
        row_from_sql,
    )
    .optional()
    .map_err(Into::into)
}

fn list_sessions(conn: &Connection) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare("SELECT * FROM sessions")?;
    let rows = stmt
        .query_map([], row_from_sql)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn delete_session(conn: &Connection, id: &str) -> Result<()> {
    // `foreign_keys = ON` makes the FK a RESTRICT by default -- deleting a
    // `sessions` row with `session_events` still pointing at it errors
    // rather than cascading. The schema doesn't declare `ON DELETE CASCADE`
    // (docs/05-persistence.md#schema doesn't either), so this deletes the
    // child rows explicitly, in the same spirit as GC's own
    // directory-then-row ordering: nothing is left pointing at a session
    // that no longer exists.
    conn.execute(
        "DELETE FROM session_events WHERE session_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
    Ok(())
}

fn gc_candidates(conn: &Connection, older_than_ms: i64) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM sessions WHERE state IN ('exited', 'lost') AND exited_at_ms IS NOT NULL AND exited_at_ms < ?1",
    )?;
    let rows = stmt
        .query_map(params![older_than_ms], row_from_sql)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn writer_loop(conn: Connection, mut rx: mpsc::Receiver<Command>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Command::Insert(row, reply) => {
                let _ = reply.send(insert_session(&conn, &row));
            }
            Command::MarkClosing { id, reply } => {
                let _ = reply.send(mark_closing(&conn, &id));
            }
            Command::MarkExited { id, exited_at_ms, exit_code, lost_reason, output_bytes, reply } => {
                let _ = reply.send(mark_exited(&conn, &id, exited_at_ms, exit_code, lost_reason, output_bytes));
            }
            Command::NoteOutputBytes { id, output_bytes } => {
                if let Err(e) =
                    conn.execute("UPDATE sessions SET output_bytes = ?1 WHERE id = ?2", params![output_bytes as i64, id])
                {
                    warn!(session_id = id, error = %e, "persisting output_bytes failed");
                }
            }
            Command::NoteSize { id, cols, rows } => {
                if let Err(e) = conn.execute(
                    "UPDATE sessions SET cols = ?1, rows = ?2 WHERE id = ?3",
                    params![cols as i64, rows as i64, id],
                ) {
                    warn!(session_id = id, error = %e, "persisting cols/rows failed");
                }
            }
            Command::NoteEvent { id, event_type, ts_ms } => {
                if let Err(e) = conn.execute(
                    "INSERT INTO session_events (session_id, ts_ms, event_type, data_json) VALUES (?1, ?2, ?3, NULL)",
                    params![id, ts_ms, event_type],
                ) {
                    warn!(session_id = id, event_type, error = %e, "recording session_events row failed");
                }
            }
            Command::Delete { id, reply } => {
                let _ = reply.send(delete_session(&conn, &id));
            }
            Command::Get { id, reply } => {
                let _ = reply.send(get_session(&conn, &id));
            }
            Command::List { reply } => {
                let _ = reply.send(list_sessions(&conn));
            }
            Command::GcCandidates { older_than_ms, reply } => {
                let _ = reply.send(gc_candidates(&conn, older_than_ms));
            }
        }
    }
}

/// `<data_dir>/sessions` -- shared by `SessionManager` and this module so
/// neither hardcodes the join in more than one place.
pub fn sessions_root(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teleportd-persistence-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        dir
    }

    fn new_row(id: &str) -> NewSessionRow {
        NewSessionRow {
            id: id.to_string(),
            kind: "shell".to_string(),
            preset: None,
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "true".to_string()],
            cwd: "/tmp".to_string(),
            pid: Some(4242),
            cols: 80,
            rows: 24,
            created_at_ms: now_ms(),
        }
    }

    // The `_blocking` methods panic if called on a Tokio runtime thread by
    // design (`blocking_send`/`blocking_recv` -- they exist for
    // `session.rs`'s plain OS threads, never an async handler). Every
    // `#[tokio::test]` below is itself running on a runtime thread, so
    // exercising them here goes through `spawn_blocking`, same as
    // `SessionManager::create` does in `api.rs`.
    async fn insert(db: &Db, row: NewSessionRow) {
        let db = db.clone();
        tokio::task::spawn_blocking(move || db.insert_session_blocking(row))
            .await
            .unwrap()
            .unwrap();
    }

    async fn close(db: &Db, id: &str) {
        let db = db.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || db.mark_closing_blocking(&id))
            .await
            .unwrap()
            .unwrap();
    }

    async fn exit(
        db: &Db,
        id: &str,
        exited_at_ms: i64,
        exit_code: Option<i32>,
        lost_reason: Option<&'static str>,
        output_bytes: u64,
    ) {
        let db = db.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            db.mark_exited_blocking(&id, exited_at_ms, exit_code, lost_reason, output_bytes)
        })
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn insert_then_get_round_trips_argv_json() {
        let dir = scratch_dir("insert-get");
        let (db, summary) = Db::open(&dir.join("state.db"), &dir.join("sessions")).unwrap();
        assert_eq!(summary.recovered_lost, 0);

        let row = new_row("s1");
        insert(&db, row.clone()).await;

        let fetched = db.get_session("s1").await.unwrap().expect("row must exist");
        assert_eq!(fetched.state, "running");
        assert_eq!(fetched.args, row.args);
        assert_eq!(fetched.started_at_ms, Some(row.created_at_ms));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_running_row_is_lost_on_the_next_open_file_length_wins() {
        let dir = scratch_dir("recover");
        let db_path = dir.join("state.db");
        let sessions_root = dir.join("sessions");

        {
            let (db, _) = Db::open(&db_path, &sessions_root).unwrap();
            insert(&db, new_row("s1")).await;
        }
        // Simulate the child having written past the stored `output_bytes`
        // (0 at insert) before the "crash" -- the file must win.
        std::fs::create_dir_all(sessions_root.join("s1")).unwrap();
        std::fs::write(sessions_root.join("s1").join("output.vt"), b"hello world").unwrap();

        let (db, summary) = Db::open(&db_path, &sessions_root).unwrap();
        assert_eq!(summary.recovered_lost, 1);

        let row = db
            .get_session("s1")
            .await
            .unwrap()
            .expect("row must survive");
        assert_eq!(row.state, "lost");
        assert_eq!(row.lost_reason.as_deref(), Some("daemon_restart"));
        assert_eq!(row.output_bytes, "hello world".len() as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_capped_log_keeps_the_column_the_file_does_not_rewind_it() {
        let dir = scratch_dir("capped");
        let db_path = dir.join("state.db");
        let sessions_root = dir.join("sessions");

        {
            let (db, _) = Db::open(&db_path, &sessions_root).unwrap();
            insert(&db, new_row("s1")).await;
            // A capped log: the column raced ahead of the file before the
            // "crash" (the file only holds the first 5 of a logical 1000
            // bytes already handed out to live clients).
            close(&db, "s1").await;
        }
        std::fs::create_dir_all(sessions_root.join("s1")).unwrap();
        std::fs::write(sessions_root.join("s1").join("output.vt"), b"12345").unwrap();
        {
            // Directly set output_bytes ahead, the way the throttled writer
            // would have while the log was capped.
            let conn = Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE sessions SET output_bytes = 1000 WHERE id = 's1'",
                [],
            )
            .unwrap();
        }

        let (db, _) = Db::open(&db_path, &sessions_root).unwrap();
        let row = db.get_session("s1").await.unwrap().unwrap();
        assert_eq!(
            row.state, "lost",
            "a stale `closing` row is recovered the same as `running`"
        );
        assert_eq!(
            row.output_bytes, 1000,
            "the column must not rewind below what clients already hold"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn mark_exited_updates_state_and_records_an_event() {
        let dir = scratch_dir("exit");
        let (db, _) = Db::open(&dir.join("state.db"), &dir.join("sessions")).unwrap();
        insert(&db, new_row("s1")).await;

        exit(&db, "s1", now_ms(), Some(0), None, 123).await;

        let row = db.get_session("s1").await.unwrap().unwrap();
        assert_eq!(row.state, "exited");
        assert_eq!(row.exit_code, Some(0));
        assert_eq!(row.output_bytes, 123);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_removes_the_row() {
        let dir = scratch_dir("delete");
        let (db, _) = Db::open(&dir.join("state.db"), &dir.join("sessions")).unwrap();
        insert(&db, new_row("s1")).await;

        db.delete_session("s1").await.unwrap();

        assert!(db.get_session("s1").await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn gc_candidates_only_returns_old_enough_terminal_rows() {
        let dir = scratch_dir("gc");
        let (db, _) = Db::open(&dir.join("state.db"), &dir.join("sessions")).unwrap();
        insert(&db, new_row("old")).await;
        exit(&db, "old", 1_000, Some(0), None, 0).await;
        insert(&db, new_row("running")).await;
        insert(&db, new_row("recent")).await;
        exit(&db, "recent", now_ms(), Some(0), None, 0).await;

        let candidates = db.gc_candidates(now_ms() - 1_000).await.unwrap();
        let ids: Vec<&str> = candidates.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["old"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_twice_is_idempotent() {
        let dir = scratch_dir("reopen");
        let db_path = dir.join("state.db");
        let sessions_root = dir.join("sessions");
        Db::open(&db_path, &sessions_root).unwrap();
        Db::open(&db_path, &sessions_root).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
