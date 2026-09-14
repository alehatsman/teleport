//! `auth_store.rs` -- the SQLite tables behind passkey login
//! (docs/17-passkey-login.md#data-model): the single `owner` row, the
//! `passkeys` enrolled under it, and the `auth_sessions` a successful
//! assertion mints.
//!
//! **Why this is its own module rather than more of `persistence.rs`:** the
//! one-writer invariant (docs/05-persistence.md#one-writer-no-pool) says every
//! statement runs on the single `db-writer` thread, so these queries cannot
//! open their own connection -- but nothing says their *SQL* has to live in
//! the same file. `persistence.rs` keeps one `Command::Auth` variant and one
//! line in `writer_loop`; everything specific to auth is here.
//!
//! **Three properties worth stating up front, because getting any of them
//! wrong is a security bug rather than a bug:**
//!
//! 1. **A session token is never stored.** Only `sha256(token)` is, behind a
//!    `UNIQUE` index, so lookup is an indexed exact match -- neither a table
//!    scan nor a timing oracle on the secret.
//! 2. **`auth_sessions.passkey_id` cascades.** Deleting a passkey signs out
//!    every session it created; that is what "remove this device" has to
//!    mean. Enforced by the `foreign_keys = ON` pragma `Db::open` already
//!    sets.
//! 3. **Challenges are not here.** A 60-second credential must not survive a
//!    restart, so registration/authentication state stays in memory next to
//!    the WS tickets in `auth.rs` (docs/05-persistence.md#auth-tables).

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::oneshot;
use tracing::warn;

/// Migration 3 (docs/05-persistence.md#migrations). Appended to
/// `persistence.rs`'s `MIGRATIONS`; never edited in place once shipped --
/// `user_version` only ever moves forward, so a daemon that already ran
/// migration 2 (session-restore's `title`/`claude_resume_id` columns) picks
/// up exactly this one and nothing else.
pub(crate) const SCHEMA_V3: &str = r"
CREATE TABLE IF NOT EXISTS owner (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    user_handle     BLOB    NOT NULL,
    user_name       TEXT    NOT NULL,
    created_at_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS passkeys (
    id              TEXT    PRIMARY KEY,
    credential_id   BLOB    NOT NULL,
    rp_id           TEXT    NOT NULL,
    credential      TEXT    NOT NULL,
    label           TEXT    NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    last_used_ms    INTEGER,
    UNIQUE (credential_id, rp_id)
);

CREATE TABLE IF NOT EXISTS auth_sessions (
    id              TEXT    PRIMARY KEY,
    token_sha256    BLOB    NOT NULL UNIQUE,
    passkey_id      TEXT    NOT NULL REFERENCES passkeys(id) ON DELETE CASCADE,
    label           TEXT    NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    expires_at_ms   INTEGER NOT NULL,
    last_seen_ms    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_passkeys_rp ON passkeys(rp_id);
CREATE INDEX IF NOT EXISTS idx_auth_sessions_passkey ON auth_sessions(passkey_id);
";

/// The single owner. Exactly zero rows (nothing enrolled yet) or one, which
/// the `CHECK (id = 1)` enforces rather than trusting callers
/// (docs/17-passkey-login.md#scope: one identity, many credentials).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRow {
    /// 32 random bytes -- the `WebAuthn` user handle. Stable for the life of
    /// the daemon: every credential enrolled later belongs to *this* handle,
    /// which is what makes them one identity rather than several.
    pub user_handle: Vec<u8>,
    /// What the user typed at first enrollment. Shown by the authenticator
    /// and the password manager; never used as a credential.
    pub user_name: String,
    /// When the first credential was enrolled.
    pub created_at_ms: i64,
}

/// One enrolled authenticator, for one RP ID. The same physical key enrolled
/// at `localhost` and at a tailnet hostname is two rows, because `WebAuthn`
/// makes it two credentials (docs/06-security.md#an-rp-id-is-a-domain-never-an-ip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasskeyRow {
    /// ULID. The API-facing id -- the raw `credential_id` never leaves the
    /// daemon in a URL.
    pub id: String,
    /// The raw `WebAuthn` credential id, as the authenticator issued it.
    pub credential_id: Vec<u8>,
    /// The relying-party ID this credential is bound to, and outside which
    /// it is worthless (docs/06-security.md#an-rp-id-is-a-domain-never-an-ip).
    pub rp_id: String,
    /// The serialized `webauthn_rs::prelude::Passkey`: public key, signature
    /// counter, transports. Opaque here on purpose -- this module stores and
    /// returns it, `auth.rs` is what understands it.
    pub credential: String,
    /// User-editable display name, defaulted from the user agent at
    /// enrollment. Cosmetic.
    pub label: String,
    /// When this credential was enrolled.
    pub created_at_ms: i64,
    /// Last successful assertion, or `None` if it has never been used since
    /// enrollment. Drives the settings list only.
    pub last_used_ms: Option<i64>,
}

/// A live login session. The token that addresses this row is **not** a
/// field: only its hash is stored (see the module header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSessionRow {
    /// ULID, and the `token_id` carried in `Principal::DeviceToken`
    /// (docs/12-identity-and-connectivity.md#the-principal).
    pub id: String,
    /// The credential that minted this session. `ON DELETE CASCADE`, so
    /// removing the passkey revokes the session.
    pub passkey_id: String,
    /// Client-supplied, for the "signed-in devices" list. Not a credential
    /// and not trusted for anything.
    pub label: String,
    /// When the login happened.
    pub created_at_ms: i64,
    /// Absolute expiry -- 30 days from issue, not sliding
    /// (docs/17-passkey-login.md#session-lifetime).
    pub expires_at_ms: i64,
    /// Last request this session authenticated, written at most hourly.
    pub last_seen_ms: i64,
}

/// Every auth query, as one variant of `persistence.rs`'s `Command`. Split
/// out so that enum grows by one line, not fourteen.
///
/// Variants with no `reply` are fire-and-forget, the same convention
/// `note_*` uses in `persistence.rs`: a failure is logged and dropped,
/// because no caller can do anything useful about a failed "update the
/// last-seen timestamp."
pub(crate) enum AuthCommand {
    OwnerGet {
        reply: oneshot::Sender<Result<Option<OwnerRow>>>,
    },
    /// Creates the owner row if absent and returns it either way. The
    /// second enrollment must reuse the first's `user_handle` -- a fresh
    /// handle would silently create a *second* `WebAuthn` identity that
    /// authenticators treat as an unrelated account.
    OwnerEnsure {
        user_handle: Vec<u8>,
        user_name: String,
        now_ms: i64,
        reply: oneshot::Sender<Result<OwnerRow>>,
    },
    PasskeyInsert {
        row: PasskeyRow,
        reply: oneshot::Sender<Result<()>>,
    },
    /// `rp_id: None` lists every enrolled credential (the settings screen);
    /// `Some` scopes to one origin (building `allowCredentials` for a login
    /// ceremony, where offering another origin's credential is useless).
    PasskeyList {
        rp_id: Option<String>,
        reply: oneshot::Sender<Result<Vec<PasskeyRow>>>,
    },
    PasskeyGet {
        id: String,
        reply: oneshot::Sender<Result<Option<PasskeyRow>>>,
    },
    /// Scoped by `rp_id` as well as `credential_id`: the pair is what the
    /// `UNIQUE` index is on, and an assertion is only ever valid for the RP
    /// it was issued to.
    PasskeyGetByCredentialId {
        credential_id: Vec<u8>,
        rp_id: String,
        reply: oneshot::Sender<Result<Option<PasskeyRow>>>,
    },
    PasskeyRename {
        id: String,
        label: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    PasskeyDelete {
        id: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    /// Fire-and-forget. Writes back the re-serialized credential after a
    /// successful assertion, which is how the signature counter advances,
    /// plus `last_used_ms` for the UI.
    PasskeyNoteUsed {
        id: String,
        credential: String,
        last_used_ms: i64,
    },
    SessionInsert {
        row: AuthSessionRow,
        token_sha256: Vec<u8>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// The hot path: one indexed lookup per authenticated request. Expiry is
    /// applied here rather than by the caller, so there is no window where a
    /// stale row authorizes anything.
    SessionLookup {
        token_sha256: Vec<u8>,
        now_ms: i64,
        reply: oneshot::Sender<Result<Option<AuthSessionRow>>>,
    },
    SessionList {
        reply: oneshot::Sender<Result<Vec<AuthSessionRow>>>,
    },
    SessionDelete {
        id: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    /// Fire-and-forget, and throttled by the caller to at most once an hour
    /// per session -- a "signed-in devices" list is not worth a disk write on
    /// every GET (docs/17-passkey-login.md#session-lifetime).
    SessionTouch { id: String, now_ms: i64 },
    /// Run by the existing GC task rather than a second timer.
    SessionSweepExpired {
        now_ms: i64,
        reply: oneshot::Sender<Result<usize>>,
    },
}

/// `writer_loop`'s single `Command::Auth` arm. Runs on the `db-writer`
/// thread and nowhere else.
pub(crate) fn handle(conn: &Connection, cmd: AuthCommand) {
    /// Every reply receiver may already be gone -- its caller stopped
    /// waiting. The write already happened; only the notification is lost.
    /// Same rule, and same reasoning, as `persistence.rs`'s session commands.
    fn reply_to<T>(reply: oneshot::Sender<Result<T>>, result: Result<T>) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "see this function's own doc comment"
        )]
        let _ = reply.send(result);
    }

    match cmd {
        AuthCommand::OwnerGet { reply } => reply_to(reply, owner_get(conn)),
        AuthCommand::OwnerEnsure {
            user_handle,
            user_name,
            now_ms,
            reply,
        } => reply_to(reply, owner_ensure(conn, &user_handle, &user_name, now_ms)),
        AuthCommand::PasskeyInsert { row, reply } => reply_to(reply, passkey_insert(conn, &row)),
        AuthCommand::PasskeyList { rp_id, reply } => {
            reply_to(reply, passkey_list(conn, rp_id.as_deref()));
        }
        AuthCommand::PasskeyGet { id, reply } => reply_to(reply, passkey_get(conn, &id)),
        AuthCommand::PasskeyGetByCredentialId {
            credential_id,
            rp_id,
            reply,
        } => reply_to(
            reply,
            passkey_get_by_credential_id(conn, &credential_id, &rp_id),
        ),
        AuthCommand::PasskeyRename { id, label, reply } => {
            reply_to(reply, passkey_rename(conn, &id, &label));
        }
        AuthCommand::PasskeyDelete { id, reply } => reply_to(reply, passkey_delete(conn, &id)),
        AuthCommand::PasskeyNoteUsed {
            id,
            credential,
            last_used_ms,
        } => {
            if let Err(e) = conn.execute(
                "UPDATE passkeys SET credential = ?1, last_used_ms = ?2 WHERE id = ?3",
                params![credential, last_used_ms, id],
            ) {
                // Losing this write means the signature counter does not
                // advance, which costs a clone-detection signal on the next
                // assertion -- worth a warning, not worth failing a login
                // the user already completed.
                warn!(passkey_id = id, error = %e, "persisting passkey use failed");
            }
        }
        AuthCommand::SessionInsert {
            row,
            token_sha256,
            reply,
        } => reply_to(reply, session_insert(conn, &row, &token_sha256)),
        AuthCommand::SessionLookup {
            token_sha256,
            now_ms,
            reply,
        } => reply_to(reply, session_lookup(conn, &token_sha256, now_ms)),
        AuthCommand::SessionList { reply } => reply_to(reply, session_list(conn)),
        AuthCommand::SessionDelete { id, reply } => reply_to(reply, session_delete(conn, &id)),
        AuthCommand::SessionTouch { id, now_ms } => {
            if let Err(e) = conn.execute(
                "UPDATE auth_sessions SET last_seen_ms = ?1 WHERE id = ?2",
                params![now_ms, id],
            ) {
                warn!(auth_session_id = id, error = %e, "persisting last_seen_ms failed");
            }
        }
        AuthCommand::SessionSweepExpired { now_ms, reply } => {
            reply_to(reply, session_sweep_expired(conn, now_ms));
        }
    }
}

fn owner_get(conn: &Connection) -> Result<Option<OwnerRow>> {
    Ok(conn
        .query_row(
            "SELECT user_handle, user_name, created_at_ms FROM owner WHERE id = 1",
            [],
            |r| {
                Ok(OwnerRow {
                    user_handle: r.get(0)?,
                    user_name: r.get(1)?,
                    created_at_ms: r.get(2)?,
                })
            },
        )
        .optional()?)
}

fn owner_ensure(
    conn: &Connection,
    user_handle: &[u8],
    user_name: &str,
    now_ms: i64,
) -> Result<OwnerRow> {
    // `OR IGNORE`, not `OR REPLACE`: if a row already exists it wins, and
    // the caller's freshly-generated handle is discarded. Replacing would
    // orphan every credential already enrolled against the old handle.
    conn.execute(
        "INSERT OR IGNORE INTO owner (id, user_handle, user_name, created_at_ms)
         VALUES (1, ?1, ?2, ?3)",
        params![user_handle, user_name, now_ms],
    )?;
    owner_get(conn)?.ok_or_else(|| anyhow::anyhow!("owner row missing immediately after insert"))
}

/// The column list every `SELECT` below shares, so a schema change touches
/// one string rather than four.
const PASSKEY_COLS: &str =
    "id, credential_id, rp_id, credential, label, created_at_ms, last_used_ms";

fn passkey_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PasskeyRow> {
    Ok(PasskeyRow {
        id: r.get(0)?,
        credential_id: r.get(1)?,
        rp_id: r.get(2)?,
        credential: r.get(3)?,
        label: r.get(4)?,
        created_at_ms: r.get(5)?,
        last_used_ms: r.get(6)?,
    })
}

fn passkey_insert(conn: &Connection, row: &PasskeyRow) -> Result<()> {
    conn.execute(
        "INSERT INTO passkeys (id, credential_id, rp_id, credential, label, created_at_ms, last_used_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            row.id,
            row.credential_id,
            row.rp_id,
            row.credential,
            row.label,
            row.created_at_ms,
            row.last_used_ms,
        ],
    )?;
    Ok(())
}

fn passkey_list(conn: &Connection, rp_id: Option<&str>) -> Result<Vec<PasskeyRow>> {
    // One statement either way: the filter is appended, not branched into
    // two near-identical query paths.
    let mut sql = format!("SELECT {PASSKEY_COLS} FROM passkeys");
    if rp_id.is_some() {
        sql.push_str(" WHERE rp_id = ?1");
    }
    sql.push_str(" ORDER BY created_at_ms");

    let mut stmt = conn.prepare(&sql)?;
    let mut out = Vec::new();
    for row in stmt.query_map(rusqlite::params_from_iter(rp_id), passkey_from_row)? {
        out.push(row?);
    }
    Ok(out)
}

fn passkey_get(conn: &Connection, id: &str) -> Result<Option<PasskeyRow>> {
    let sql = format!("SELECT {PASSKEY_COLS} FROM passkeys WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![id], passkey_from_row)
        .optional()?)
}

fn passkey_get_by_credential_id(
    conn: &Connection,
    credential_id: &[u8],
    rp_id: &str,
) -> Result<Option<PasskeyRow>> {
    let sql =
        format!("SELECT {PASSKEY_COLS} FROM passkeys WHERE credential_id = ?1 AND rp_id = ?2");
    Ok(conn
        .query_row(&sql, params![credential_id, rp_id], passkey_from_row)
        .optional()?)
}

fn passkey_rename(conn: &Connection, id: &str, label: &str) -> Result<bool> {
    let n = conn.execute(
        "UPDATE passkeys SET label = ?1 WHERE id = ?2",
        params![label, id],
    )?;
    Ok(n > 0)
}

fn passkey_delete(conn: &Connection, id: &str) -> Result<bool> {
    // `auth_sessions.passkey_id` is ON DELETE CASCADE and `foreign_keys` is
    // ON, so this signs out every session the credential minted. That is the
    // intended meaning of removing a device, not a side effect.
    let n = conn.execute("DELETE FROM passkeys WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

const SESSION_COLS: &str = "id, passkey_id, label, created_at_ms, expires_at_ms, last_seen_ms";

fn session_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AuthSessionRow> {
    Ok(AuthSessionRow {
        id: r.get(0)?,
        passkey_id: r.get(1)?,
        label: r.get(2)?,
        created_at_ms: r.get(3)?,
        expires_at_ms: r.get(4)?,
        last_seen_ms: r.get(5)?,
    })
}

fn session_insert(conn: &Connection, row: &AuthSessionRow, token_sha256: &[u8]) -> Result<()> {
    conn.execute(
        "INSERT INTO auth_sessions
         (id, token_sha256, passkey_id, label, created_at_ms, expires_at_ms, last_seen_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            row.id,
            token_sha256,
            row.passkey_id,
            row.label,
            row.created_at_ms,
            row.expires_at_ms,
            row.last_seen_ms,
        ],
    )?;
    Ok(())
}

fn session_lookup(
    conn: &Connection,
    token_sha256: &[u8],
    now_ms: i64,
) -> Result<Option<AuthSessionRow>> {
    // Expiry is in the WHERE clause, not a post-filter: there must be no
    // arrangement of code in which an expired row is returned and then
    // forgotten to be checked.
    let sql = format!(
        "SELECT {SESSION_COLS} FROM auth_sessions WHERE token_sha256 = ?1 AND expires_at_ms > ?2"
    );
    Ok(conn
        .query_row(&sql, params![token_sha256, now_ms], session_from_row)
        .optional()?)
}

fn session_list(conn: &Connection) -> Result<Vec<AuthSessionRow>> {
    let sql = format!("SELECT {SESSION_COLS} FROM auth_sessions ORDER BY created_at_ms");
    let mut stmt = conn.prepare(&sql)?;
    let mut out = Vec::new();
    for row in stmt.query_map([], session_from_row)? {
        out.push(row?);
    }
    Ok(out)
}

fn session_delete(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM auth_sessions WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

fn session_sweep_expired(conn: &Connection, now_ms: i64) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM auth_sessions WHERE expires_at_ms <= ?1",
        params![now_ms],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory DB with this module's schema and the one pragma its
    /// correctness depends on. `Db::open` sets `foreign_keys = ON` for the
    /// real connection; without it here the cascade test would silently
    /// pass for the wrong reason.
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("foreign_keys=ON");
        conn.execute_batch(SCHEMA_V3).expect("schema");
        conn
    }

    fn passkey(id: &str, rp_id: &str, cred: &[u8]) -> PasskeyRow {
        PasskeyRow {
            id: id.to_string(),
            credential_id: cred.to_vec(),
            rp_id: rp_id.to_string(),
            credential: "{\"serialized\":\"passkey\"}".to_string(),
            label: "Test key".to_string(),
            created_at_ms: 1_000,
            last_used_ms: None,
        }
    }

    fn auth_session(id: &str, passkey_id: &str, expires_at_ms: i64) -> AuthSessionRow {
        AuthSessionRow {
            id: id.to_string(),
            passkey_id: passkey_id.to_string(),
            label: "Chrome on macOS".to_string(),
            created_at_ms: 1_000,
            expires_at_ms,
            last_seen_ms: 1_000,
        }
    }

    #[test]
    fn owner_is_created_once_and_keeps_its_first_handle() {
        let conn = conn();
        assert_eq!(owner_get(&conn).unwrap(), None, "nothing enrolled yet");

        let first = owner_ensure(&conn, b"handle-one", "aleh", 1_000).unwrap();
        assert_eq!(first.user_handle, b"handle-one".to_vec());

        // The second enrollment generates its own candidate handle. It must
        // lose -- a new handle would make the existing credentials belong to
        // a different WebAuthn identity.
        let second = owner_ensure(&conn, b"handle-two", "someone-else", 2_000).unwrap();
        assert_eq!(second, first, "the first owner row wins, entirely");
    }

    #[test]
    fn the_same_key_enrolls_separately_per_rp_id() {
        // One physical authenticator, two origins, two rows. This is the
        // shape docs/06-security.md#an-rp-id-is-a-domain-never-an-ip forces.
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();
        passkey_insert(&conn, &passkey("p2", "host.ts.net", b"cred-a")).unwrap();

        assert_eq!(passkey_list(&conn, None).unwrap().len(), 2);
        assert_eq!(passkey_list(&conn, Some("localhost")).unwrap().len(), 1);
        assert_eq!(passkey_list(&conn, Some("host.ts.net")).unwrap().len(), 1);
        assert!(passkey_list(&conn, Some("evil.example"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_same_credential_cannot_be_enrolled_twice_for_one_rp_id() {
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();
        assert!(
            passkey_insert(&conn, &passkey("p2", "localhost", b"cred-a")).is_err(),
            "UNIQUE (credential_id, rp_id) must reject a duplicate enrollment"
        );
    }

    #[test]
    fn lookup_by_credential_id_is_scoped_to_its_rp() {
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();

        let found = passkey_get_by_credential_id(&conn, b"cred-a", "localhost").unwrap();
        assert_eq!(found.map(|p| p.id), Some("p1".to_string()));

        // The anti-phishing property, at the storage layer: an assertion
        // from another origin must not resolve to this credential.
        assert_eq!(
            passkey_get_by_credential_id(&conn, b"cred-a", "host.ts.net").unwrap(),
            None
        );
    }

    #[test]
    fn rename_and_delete_report_whether_a_row_matched() {
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();

        assert!(passkey_rename(&conn, "p1", "Laptop Touch ID").unwrap());
        assert!(!passkey_rename(&conn, "nope", "x").unwrap());
        assert_eq!(
            passkey_get(&conn, "p1").unwrap().unwrap().label,
            "Laptop Touch ID"
        );

        assert!(passkey_delete(&conn, "p1").unwrap());
        assert!(!passkey_delete(&conn, "p1").unwrap(), "already gone");
    }

    #[test]
    fn a_session_is_found_by_token_hash_and_never_by_token() {
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();
        session_insert(&conn, &auth_session("s1", "p1", 9_000), b"hash-of-token").unwrap();

        let found = session_lookup(&conn, b"hash-of-token", 5_000).unwrap();
        assert_eq!(found.map(|s| s.id), Some("s1".to_string()));
        assert_eq!(session_lookup(&conn, b"wrong-hash", 5_000).unwrap(), None);
    }

    #[test]
    fn an_expired_session_does_not_resolve() {
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();
        session_insert(&conn, &auth_session("s1", "p1", 9_000), b"hash").unwrap();

        assert!(session_lookup(&conn, b"hash", 8_999).unwrap().is_some());
        // Exactly at expiry is already expired -- `expires_at_ms > now` is
        // the condition, deliberately not `>=`.
        assert_eq!(session_lookup(&conn, b"hash", 9_000).unwrap(), None);
        assert_eq!(session_lookup(&conn, b"hash", 9_001).unwrap(), None);
    }

    #[test]
    fn deleting_a_passkey_signs_out_every_session_it_minted() {
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();
        passkey_insert(&conn, &passkey("p2", "localhost", b"cred-b")).unwrap();
        session_insert(&conn, &auth_session("s1", "p1", 9_000), b"hash-1").unwrap();
        session_insert(&conn, &auth_session("s2", "p1", 9_000), b"hash-2").unwrap();
        session_insert(&conn, &auth_session("s3", "p2", 9_000), b"hash-3").unwrap();

        assert!(passkey_delete(&conn, "p1").unwrap());

        // Both of p1's sessions are gone immediately -- "remove this device"
        // has to mean the device is actually signed out.
        assert_eq!(session_lookup(&conn, b"hash-1", 5_000).unwrap(), None);
        assert_eq!(session_lookup(&conn, b"hash-2", 5_000).unwrap(), None);
        // ...and the unrelated credential's session is untouched.
        assert!(session_lookup(&conn, b"hash-3", 5_000).unwrap().is_some());
    }

    #[test]
    fn a_session_cannot_reference_a_passkey_that_does_not_exist() {
        let conn = conn();
        assert!(
            session_insert(&conn, &auth_session("s1", "ghost", 9_000), b"hash").is_err(),
            "the foreign key must be enforced, not decorative"
        );
    }

    #[test]
    fn revoking_one_session_leaves_the_others_alone() {
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();
        session_insert(&conn, &auth_session("s1", "p1", 9_000), b"hash-1").unwrap();
        session_insert(&conn, &auth_session("s2", "p1", 9_000), b"hash-2").unwrap();

        assert!(session_delete(&conn, "s1").unwrap());
        assert!(!session_delete(&conn, "s1").unwrap(), "already revoked");
        assert_eq!(session_list(&conn).unwrap().len(), 1);
        assert!(session_lookup(&conn, b"hash-2", 5_000).unwrap().is_some());
    }

    #[test]
    fn the_sweep_removes_only_what_has_actually_expired() {
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();
        session_insert(&conn, &auth_session("old", "p1", 1_500), b"h1").unwrap();
        session_insert(&conn, &auth_session("live", "p1", 9_000), b"h2").unwrap();

        assert_eq!(session_sweep_expired(&conn, 5_000).unwrap(), 1);
        let left = session_list(&conn).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "live");
    }

    #[test]
    fn noting_a_use_advances_the_stored_credential_and_timestamp() {
        // The stored credential is what carries the WebAuthn signature
        // counter, so failing to write it back would disable clone
        // detection on every subsequent assertion.
        let conn = conn();
        passkey_insert(&conn, &passkey("p1", "localhost", b"cred-a")).unwrap();

        handle(
            &conn,
            AuthCommand::PasskeyNoteUsed {
                id: "p1".to_string(),
                credential: "{\"counter\":7}".to_string(),
                last_used_ms: 4_242,
            },
        );

        let row = passkey_get(&conn, "p1").unwrap().unwrap();
        assert_eq!(row.credential, "{\"counter\":7}");
        assert_eq!(row.last_used_ms, Some(4_242));
    }
}
