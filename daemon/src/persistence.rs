//! SQLite metadata store: schema, migrations, and a single writer actor
//! (docs/05-persistence.md).
//!
//! **One writer, no pool** (docs/05-persistence.md#one-writer-no-pool): a
//! dedicated OS thread owns the one [`rusqlite::Connection`] for the life of
//! the daemon, fed by an `mpsc` command channel, replying over `oneshot`.
//! Every other module talks to it through [`Db`], a cheap `Clone` wrapper
//! around the sender half. Reads go through the same actor -- the query
//! volume is trivial, and a single path is simpler than a pool plus a
//! writer. `note_*` methods are fire-and-forget (`try_send`, no reply).
//!
//! **What it holds now:** auth (owner, passkeys, login sessions --
//! `auth_store.rs`), pinned locations, and the titles teleport derives for
//! sessions anchord hosts (`anchor_session_meta`). **What it no longer
//! holds:** session rows. Every session lives in anchord, which persists
//! and recovers its own; the in-process layer that wrote `sessions` and
//! `session_events`, and the restart recovery that read them back, are gone
//! (docs/20-retiring-the-in-process-pty.md). The tables stay in the schema,
//! because a migration never drops what an older database holds, and
//! nothing writes them.

use std::collections::HashMap;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::auth_store::{AuthCommand, AuthSessionRow, OwnerRow, PasskeyRow};
use crate::now_ms;

/// One `pinned_locations` row: a directory the user asked to keep at the top
/// of the launcher, independent of whether any session in it still exists
/// (docs/19-locations.md#pins).
#[derive(Debug, Clone)]
pub struct PinRow {
    /// Absolute, already canonicalized by the caller.
    pub path: String,
    /// When it was first pinned, ms since epoch. Re-pinning does not move it.
    pub pinned_at_ms: i64,
}

enum Command {
    /// Every passkey-login query, in one variant so this enum grows by a
    /// line rather than fourteen (`auth_store.rs` owns the shape and the
    /// SQL).
    Auth(AuthCommand),
    ListPins {
        reply: oneshot::Sender<Result<Vec<PinRow>>>,
    },
    /// Insert-or-keep, and reply with the row that ended up stored
    /// (docs/19-locations.md#post-apiv1locationspins: pinning twice is a
    /// no-op, not an error).
    AddPin {
        path: String,
        pinned_at_ms: i64,
        max_pins: usize,
        reply: oneshot::Sender<Result<AddPinOutcome>>,
    },
    RemovePin {
        path: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Upsert one anchor-hosted session's derived title.
    NoteAnchorTitle {
        session_id: String,
        title: String,
        now_ms: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Every stored anchor title, by session id. Read whole: the list view
    /// needs all of them at once, and one query beats one per session.
    AnchorTitles {
        reply: oneshot::Sender<Result<HashMap<String, String>>>,
    },
    /// Drop one anchor session's derived metadata, on purge.
    ForgetAnchorMeta {
        session_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// What [`Db::add_pin`] did. `Full` is not an `Err` because it is not a
/// failure of the write -- it is a `429` the handler has to turn into a
/// message, and an `anyhow::Error` string is the wrong carrier for that.
#[derive(Debug, Clone)]
pub enum AddPinOutcome {
    /// The row now in the table -- freshly inserted, or the one that was
    /// already there under this path.
    Stored(PinRow),
    /// Already at the cap, and this path is not one of the existing pins.
    Full {
        /// The cap that was hit, so the handler can name it in the `429`.
        max_pins: usize,
    },
}

/// A cheap, `Clone`-able handle to the writer actor's command channel.
#[derive(Debug, Clone)]
pub struct Db {
    tx: mpsc::Sender<Command>,
}

impl Db {
    /// Opens `db_path` (creating it if absent), applies pragmas and
    /// migrations, then spawns the writer thread and returns a handle.
    ///
    /// No restart recovery: there are no session rows left to recover.
    /// Every session lives in anchord, which recovers its own (docs/20).
    /// The `sessions` table stays in the schema -- migrations never drop
    /// what an older database holds -- and nothing writes it.
    pub fn open(db_path: &Path) -> Result<Db> {
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

        let (tx, rx) = mpsc::channel(256);
        std::thread::Builder::new()
            .name("db-writer".into())
            .spawn(move || writer_loop(&conn, rx))
            .context("spawning db-writer thread")?;

        Ok(Db { tx })
    }

    #[expect(
        clippy::map_err_ignore,
        reason = "SendError/RecvError carry no diagnostic beyond \"the other end is gone\", which the replacement message already says"
    )]
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

    /// Every pin, newest first (docs/19-locations.md#get-apiv1locationspins).
    /// A pinned directory that has since been deleted is still returned --
    /// dropping it here would leave the user no way to unpin it.
    pub async fn list_pins(&self) -> Result<Vec<PinRow>> {
        self.call(|reply| Command::ListPins { reply }).await
    }

    /// Pins `path`, or reports the cap. Idempotent: an already-pinned path
    /// keeps its original `pinned_at_ms` and comes back as `Stored`.
    pub async fn add_pin(
        &self,
        path: &str,
        pinned_at_ms: i64,
        max_pins: usize,
    ) -> Result<AddPinOutcome> {
        self.call(|reply| Command::AddPin {
            path: path.to_string(),
            pinned_at_ms,
            max_pins,
            reply,
        })
        .await
    }

    /// Unpins `path`. Unpinning something that was never pinned is success,
    /// not a `404` -- the end state the caller asked for is the end state.
    pub async fn remove_pin(&self, path: &str) -> Result<()> {
        self.call(|reply| Command::RemovePin {
            path: path.to_string(),
            reply,
        })
        .await
    }

    /// Records the latest derived title for a session anchord owns.
    ///
    /// Throttled by the caller, not here: `anchor_watch` already knows how
    /// often it is seeing titles and this layer does not.
    pub async fn note_anchor_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.call(|reply| Command::NoteAnchorTitle {
            session_id: session_id.to_string(),
            title: title.to_string(),
            now_ms: now_ms(),
            reply,
        })
        .await
    }

    /// Every stored anchor title, by session id.
    pub async fn anchor_titles(&self) -> Result<HashMap<String, String>> {
        self.call(|reply| Command::AnchorTitles { reply }).await
    }

    /// Forgets one anchor session's derived metadata. Called on purge, so a
    /// forgotten session does not leave a title behind.
    pub async fn forget_anchor_meta(&self, session_id: &str) -> Result<()> {
        self.call(|reply| Command::ForgetAnchorMeta {
            session_id: session_id.to_string(),
            reply,
        })
        .await
    }
}

/// Passkey login (docs/17-passkey-login.md). A separate `impl` block from
/// the session methods above purely for readability -- these go through the
/// exact same actor, channel and thread. All `async`: the only callers are
/// `api.rs` handlers and `auth.rs`'s resolve path, both already on the Tokio
/// executor, and none of this is reachable from the PTY reader thread.
impl Db {
    /// The single owner row, or `None` when nothing is enrolled yet --
    /// which is what `/auth/status` reports as `enrolled: false`.
    pub async fn owner(&self) -> Result<Option<OwnerRow>> {
        self.call(|reply| Command::Auth(AuthCommand::OwnerGet { reply }))
            .await
    }

    /// Creates the owner on first enrollment and returns it either way.
    /// A later call's `user_handle` is discarded if a row already exists.
    pub async fn ensure_owner(
        &self,
        user_handle: Vec<u8>,
        user_name: String,
        now_ms: i64,
    ) -> Result<OwnerRow> {
        self.call(|reply| {
            Command::Auth(AuthCommand::OwnerEnsure {
                user_handle,
                user_name,
                now_ms,
                reply,
            })
        })
        .await
    }

    /// Records a newly enrolled credential. Fails if this
    /// `(credential_id, rp_id)` pair is already enrolled.
    pub async fn insert_passkey(&self, row: PasskeyRow) -> Result<()> {
        self.call(|reply| Command::Auth(AuthCommand::PasskeyInsert { row, reply }))
            .await
    }

    /// `None` lists every credential; `Some(rp_id)` scopes to one origin.
    pub async fn list_passkeys(&self, rp_id: Option<String>) -> Result<Vec<PasskeyRow>> {
        self.call(|reply| Command::Auth(AuthCommand::PasskeyList { rp_id, reply }))
            .await
    }

    /// One credential by its API-facing ULID.
    pub async fn get_passkey(&self, id: String) -> Result<Option<PasskeyRow>> {
        self.call(|reply| Command::Auth(AuthCommand::PasskeyGet { id, reply }))
            .await
    }

    /// Resolves the credential an assertion names. Scoped by `rp_id`, so a
    /// credential from another origin can never be found here.
    pub async fn get_passkey_by_credential_id(
        &self,
        credential_id: Vec<u8>,
        rp_id: String,
    ) -> Result<Option<PasskeyRow>> {
        self.call(|reply| {
            Command::Auth(AuthCommand::PasskeyGetByCredentialId {
                credential_id,
                rp_id,
                reply,
            })
        })
        .await
    }

    /// `false` means no such row, which `api.rs` turns into a `404`.
    pub async fn rename_passkey(&self, id: String, label: String) -> Result<bool> {
        self.call(|reply| Command::Auth(AuthCommand::PasskeyRename { id, label, reply }))
            .await
    }

    /// Cascades to every `auth_sessions` row this credential minted.
    pub async fn delete_passkey(&self, id: String) -> Result<bool> {
        self.call(|reply| Command::Auth(AuthCommand::PasskeyDelete { id, reply }))
            .await
    }

    /// Fire-and-forget: advances the stored signature counter and
    /// `last_used_ms` after a successful assertion. Dropped with a `warn!`
    /// if the channel is full rather than making a user who has already
    /// authenticated wait on disk.
    pub fn note_passkey_used(&self, id: &str, credential: String, last_used_ms: i64) {
        if self
            .tx
            .try_send(Command::Auth(AuthCommand::PasskeyNoteUsed {
                id: id.to_string(),
                credential,
                last_used_ms,
            }))
            .is_err()
        {
            warn!(
                passkey_id = id,
                "db command channel full or closed; passkey use not recorded"
            );
        }
    }

    /// Mints a login session. `token_sha256` is the hash of the token
    /// handed to the client; the token itself is never stored.
    pub async fn insert_auth_session(
        &self,
        row: AuthSessionRow,
        token_sha256: Vec<u8>,
    ) -> Result<()> {
        self.call(|reply| {
            Command::Auth(AuthCommand::SessionInsert {
                row,
                token_sha256,
                reply,
            })
        })
        .await
    }

    /// The per-request lookup behind `Principal::DeviceToken`. Returns
    /// `None` for unknown *and* for expired -- expiry is applied in SQL, so
    /// there is no window in which a caller can forget to check it.
    pub async fn lookup_auth_session(
        &self,
        token_sha256: Vec<u8>,
        now_ms: i64,
    ) -> Result<Option<AuthSessionRow>> {
        self.call(|reply| {
            Command::Auth(AuthCommand::SessionLookup {
                token_sha256,
                now_ms,
                reply,
            })
        })
        .await
    }

    /// Every live login session, for the "signed-in devices" list.
    pub async fn list_auth_sessions(&self) -> Result<Vec<AuthSessionRow>> {
        self.call(|reply| Command::Auth(AuthCommand::SessionList { reply }))
            .await
    }

    /// Revokes one session. `false` means no such row.
    pub async fn delete_auth_session(&self, id: String) -> Result<bool> {
        self.call(|reply| Command::Auth(AuthCommand::SessionDelete { id, reply }))
            .await
    }

    /// Fire-and-forget, and the caller throttles it to at most hourly per
    /// session (docs/17-passkey-login.md#session-lifetime).
    pub fn note_auth_session_seen(&self, id: &str, now_ms: i64) {
        if self
            .tx
            .try_send(Command::Auth(AuthCommand::SessionTouch {
                id: id.to_string(),
                now_ms,
            }))
            .is_err()
        {
            warn!(
                auth_session_id = id,
                "db command channel full or closed; last_seen_ms not recorded"
            );
        }
    }

    /// Called by the existing GC task, not a second timer.
    pub async fn sweep_expired_auth_sessions(&self, now_ms: i64) -> Result<usize> {
        self.call(|reply| Command::Auth(AuthCommand::SessionSweepExpired { now_ms, reply }))
            .await
    }
}

/// Ordered, additive migrations. `PRAGMA user_version` is the counter: on
/// open, every migration at an index `>= user_version` runs, then
/// `user_version` is set to `MIGRATIONS.len()` (docs/05-persistence.md#migrations).
/// No migration framework, no external tool.
const MIGRATIONS: &[&str] = &[
    SCHEMA_V1,
    SCHEMA_V2,
    crate::auth_store::SCHEMA_V3,
    SCHEMA_V4,
    SCHEMA_V5,
];

const SCHEMA_V1: &str = r"
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
";

/// Agent-reported metadata (docs/05-persistence.md#agent-reported-metadata).
/// Two nullable columns, no backfill: a row written before this migration
/// simply never carried either value.
const SCHEMA_V2: &str = r"
ALTER TABLE sessions ADD COLUMN title TEXT;
ALTER TABLE sessions ADD COLUMN claude_resume_id TEXT;
";

/// Pinned launcher locations (docs/19-locations.md#pins). Deliberately no
/// foreign key to `sessions`: a pin outliving every session that ever ran in
/// that directory -- and outliving the GC window that deletes them -- is the
/// entire point of pinning. The path is the key; there is nothing else to
/// identify a directory by.
/// V4, not V3: this branch wrote the pins migration as V3, but V3 reached
/// `main` first as the auth tables (`auth_store::SCHEMA_V3`). `user_version`
/// is a count, so a daemon that already ran the auth migration would skip an
/// index-2 entry outright and never create this table. Migrations append;
/// they are never renumbered in place.
const SCHEMA_V4: &str = r"
CREATE TABLE IF NOT EXISTS pinned_locations (
    path            TEXT PRIMARY KEY,
    pinned_at_ms    INTEGER NOT NULL
);
";

/// Derived metadata for a session **anchord** owns
/// (docs/20-retiring-the-in-process-pty.md, gap 2).
///
/// Its own table rather than a `sessions` row: teleport writes no session
/// rows on the anchor path -- anchord is the source of truth for a session's
/// existence -- and inventing half a row to hang a title off would create a
/// second answer to "does this session exist".
///
/// Deliberately no foreign key, for the same reason `pinned_locations` has
/// none: the thing being referenced lives in another process's database.
///
/// **Title only.** A `claude_resume_id` is written once, in an agent's
/// startup banner, so it rides as an anchor label instead and survives a
/// teleport restart with nothing to keep in step. A title is re-emitted on
/// every prompt, and labels have no rate protection -- every label write is a
/// SQLite write on anchor's side -- so the chatty field is the one that stays
/// local.
const SCHEMA_V5: &str = r"
CREATE TABLE IF NOT EXISTS anchor_session_meta (
    session_id      TEXT PRIMARY KEY,
    title           TEXT,
    updated_at_ms   INTEGER NOT NULL
);
";

fn run_migrations(conn: &Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // SQLite's `user_version` is a 32-bit signed field by definition, so this
    // always fits `usize` -- `usize::MAX` on the (impossible) alternative
    // just means `.skip()` below runs no migrations, not a panic.
    let current = usize::try_from(current.max(0)).unwrap_or(usize::MAX);
    for migration in MIGRATIONS.iter().skip(current) {
        conn.execute_batch(migration)?;
    }
    if MIGRATIONS.len() > current {
        conn.pragma_update(
            None,
            "user_version",
            i64::try_from(MIGRATIONS.len()).expect("a handful of migrations, not billions"),
        )?;
    }
    Ok(())
}

fn list_pins(conn: &Connection) -> Result<Vec<PinRow>> {
    let mut stmt =
        conn.prepare("SELECT path, pinned_at_ms FROM pinned_locations ORDER BY pinned_at_ms DESC")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PinRow {
                path: r.get("path")?,
                pinned_at_ms: r.get("pinned_at_ms")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn add_pin(
    conn: &Connection,
    path: &str,
    pinned_at_ms: i64,
    max_pins: usize,
) -> Result<AddPinOutcome> {
    // The cap is checked here, inside the writer thread, rather than by the
    // handler reading a count first: this is the only writer, so a count
    // taken here cannot be stale by the time the INSERT runs.
    if let Some(existing) = conn
        .query_row(
            "SELECT path, pinned_at_ms FROM pinned_locations WHERE path = ?1",
            params![path],
            |r| {
                Ok(PinRow {
                    path: r.get("path")?,
                    pinned_at_ms: r.get("pinned_at_ms")?,
                })
            },
        )
        .optional()?
    {
        // Already pinned: keep the original timestamp so re-tapping the star
        // on a second device doesn't reshuffle the list.
        return Ok(AddPinOutcome::Stored(existing));
    }
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM pinned_locations", [], |r| r.get(0))?;
    if usize::try_from(count).unwrap_or(usize::MAX) >= max_pins {
        return Ok(AddPinOutcome::Full { max_pins });
    }
    conn.execute(
        "INSERT INTO pinned_locations (path, pinned_at_ms) VALUES (?1, ?2)",
        params![path, pinned_at_ms],
    )?;
    Ok(AddPinOutcome::Stored(PinRow {
        path: path.to_string(),
        pinned_at_ms,
    }))
}

fn remove_pin(conn: &Connection, path: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM pinned_locations WHERE path = ?1",
        params![path],
    )?;
    Ok(())
}

fn writer_loop(conn: &Connection, mut rx: mpsc::Receiver<Command>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Command::Auth(cmd) => crate::auth_store::handle(conn, cmd),
            Command::ListPins { reply } => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "reply's receiver may already be gone (its caller stopped waiting); the write itself already happened, only the notification is lost"
                )]
                let _ = reply.send(list_pins(conn));
            }
            Command::AddPin {
                path,
                pinned_at_ms,
                max_pins,
                reply,
            } => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "reply's receiver may already be gone (its caller stopped waiting); the write itself already happened, only the notification is lost"
                )]
                let _ = reply.send(add_pin(conn, &path, pinned_at_ms, max_pins));
            }
            Command::RemovePin { path, reply } => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "reply's receiver may already be gone (its caller stopped waiting); the write itself already happened, only the notification is lost"
                )]
                let _ = reply.send(remove_pin(conn, &path));
            }
            Command::NoteAnchorTitle {
                session_id,
                title,
                now_ms,
                reply,
            } => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "reply's receiver may already be gone (its caller stopped waiting); the write itself already happened, only the notification is lost"
                )]
                let _ = reply.send(note_anchor_title(conn, &session_id, &title, now_ms));
            }
            Command::AnchorTitles { reply } => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "reply's receiver may already be gone (its caller stopped waiting); the write itself already happened, only the notification is lost"
                )]
                let _ = reply.send(anchor_titles(conn));
            }
            Command::ForgetAnchorMeta { session_id, reply } => {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "reply's receiver may already be gone (its caller stopped waiting); the write itself already happened, only the notification is lost"
                )]
                let _ = reply.send(forget_anchor_meta(conn, &session_id));
            }
        }
    }
}

/// Upsert, because a title is re-emitted constantly and only the latest
/// matters -- there is no history worth keeping here, only a current value.
fn note_anchor_title(conn: &Connection, id: &str, title: &str, now_ms: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO anchor_session_meta (session_id, title, updated_at_ms)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET title = ?2, updated_at_ms = ?3",
        params![id, title, now_ms],
    )?;
    Ok(())
}

fn anchor_titles(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT session_id, title FROM anchor_session_meta WHERE title IS NOT NULL")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, title) = row?;
        drop(out.insert(id, title));
    }
    Ok(out)
}

fn forget_anchor_meta(conn: &Connection, id: &str) -> Result<()> {
    // The row count is not interesting: forgetting a session that never had
    // a title is a no-op, not a failure.
    conn.execute(
        "DELETE FROM anchor_session_meta WHERE session_id = ?1",
        params![id],
    )?;
    Ok(())
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

    #[tokio::test]
    async fn pins_round_trip_newest_first() {
        let dir = scratch_dir("pins-list");
        let db = Db::open(&dir.join("state.db")).unwrap();

        db.add_pin("/a", 100, 50).await.unwrap();
        db.add_pin("/b", 200, 50).await.unwrap();

        let paths: Vec<String> = db
            .list_pins()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.path)
            .collect();
        assert_eq!(paths, vec!["/b", "/a"], "newest pin first");

        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pinning_the_same_path_twice_keeps_the_first_timestamp() {
        let dir = scratch_dir("pins-idempotent");
        let db = Db::open(&dir.join("state.db")).unwrap();

        db.add_pin("/a", 100, 50).await.unwrap();
        let outcome = db.add_pin("/a", 999, 50).await.unwrap();

        match outcome {
            AddPinOutcome::Stored(row) => assert_eq!(
                row.pinned_at_ms, 100,
                "re-pinning must not reshuffle the list under the other device"
            ),
            AddPinOutcome::Full { .. } => panic!("one pin is not the cap"),
        }
        assert_eq!(db.list_pins().await.unwrap().len(), 1);

        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_pin_cap_refuses_a_new_path_but_not_an_existing_one() {
        let dir = scratch_dir("pins-cap");
        let db = Db::open(&dir.join("state.db")).unwrap();

        db.add_pin("/a", 1, 2).await.unwrap();
        db.add_pin("/b", 2, 2).await.unwrap();

        assert!(
            matches!(
                db.add_pin("/c", 3, 2).await.unwrap(),
                AddPinOutcome::Full { max_pins: 2 }
            ),
            "a third path past a cap of two is refused"
        );
        assert!(
            matches!(
                db.add_pin("/a", 4, 2).await.unwrap(),
                AddPinOutcome::Stored(_)
            ),
            "re-pinning something already stored adds nothing, so the cap can't block it"
        );

        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unpinning_something_that_was_never_pinned_is_not_an_error() {
        let dir = scratch_dir("pins-remove");
        let db = Db::open(&dir.join("state.db")).unwrap();

        db.remove_pin("/never").await.unwrap();
        db.add_pin("/a", 1, 50).await.unwrap();
        db.remove_pin("/a").await.unwrap();

        assert!(db.list_pins().await.unwrap().is_empty());

        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A database written before migration 3 gains the table on open, with
    /// no manual step (docs/05-persistence.md#migrations).
    #[tokio::test]
    async fn a_v2_database_gains_the_pins_table_on_open() {
        let dir = scratch_dir("pins-migration");
        let db_path = dir.join("state.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.pragma_update(None, "user_version", 2i64).unwrap();
        }

        let db = Db::open(&db_path).unwrap();
        db.add_pin("/a", 1, 50).await.unwrap();
        assert_eq!(db.list_pins().await.unwrap().len(), 1);

        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_twice_is_idempotent() {
        let dir = scratch_dir("reopen");
        let db_path = dir.join("state.db");
        Db::open(&db_path).unwrap();
        Db::open(&db_path).unwrap();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
    }
}
