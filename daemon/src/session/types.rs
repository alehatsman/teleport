//! Plain data types shared across the `session` module -- ids, session state,
//! metadata, and the two small locked structs (`Runtime`, `ControlLease`)
//! that `mod.rs`'s `Session` and `control.rs`'s lease methods both touch.
//! No logic beyond trivial accessors lives here; see the parent module doc
//! (`session/mod.rs`) for the design this implements.

use std::path::PathBuf;
use std::sync::Arc;

use ulid::Ulid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(Ulid);

impl SessionId {
    pub(super) fn new() -> Self {
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
    /// ([`super::manager::SessionManager::create`]'s own checks catch the
    /// common case as a clean `422` before this ever applies), this is the
    /// residual case where `pty::spawn` itself fails
    /// (docs/04-api-protocol.md#post-apiv1sessions).
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
///
/// `pub(super)` throughout: constructed in `manager.rs`'s `create()`, read
/// and written from `mod.rs`'s `Session` methods and from
/// `manager.rs`'s exit listener -- every one of those is a sibling module
/// under `session`, so `pub(super)` (visible to the whole `session` subtree)
/// is exactly the reach this needs, no wider.
pub(super) struct Runtime {
    pub(super) state: SessionState,
    pub(super) pid: Option<u32>,
    pub(super) cols: u16,
    pub(super) rows: u16,
    pub(super) created_at_ms: i64,
    pub(super) started_at_ms: Option<i64>,
    pub(super) exited_at_ms: Option<i64>,
    pub(super) exit_code: Option<i32>,
    pub(super) lost_reason: Option<SessionLostReason>,
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
///
/// `pub(super)`: the lease methods live in `control.rs`, a sibling of this
/// file -- same reach as `Runtime` above, for the same reason.
#[derive(Debug, Clone, Default)]
pub(super) struct ControlLease {
    pub(super) holder: Option<String>,
    pub(super) holder_name: Option<String>,
    pub(super) grace: bool,
    pub(super) epoch: u64,
}

/// Asynchronous, out-of-band notifications a WS connection needs beyond raw
/// output bytes: another client resized the PTY, or this client's control
/// was just taken by someone else. Delivered over a `broadcast` channel
/// rather than threaded through `Subscription` because they are rare,
/// session-wide, and not part of the offset-indexed byte stream
/// (docs/04-api-protocol.md#control-messages).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Resized {
        cols: u16,
        rows: u16,
    },
    /// `lost_by` addresses the notification -- only the connection whose
    /// `client_id` matches acts on it. `new_controller_id`/`_name` are the
    /// wire message's content: who control was given *to*
    /// (docs/04-api-protocol.md#control-messages:
    /// `{"type":"control_revoked","to":"aleh's phone","client_id":"01K5Q…"}`
    /// -- both fields describe the new holder, not the one losing it).
    ControlRevoked {
        lost_by: String,
        new_controller_id: String,
        new_controller_name: String,
    },
}

/// Capacity for the [`SessionEvent`] broadcast channel. Resize and
/// control-lease changes are both rare and human-paced (a resize on window
/// change, a control claim on a tap) -- nothing here is a hot path, so a
/// small bound is a correctness backstop against a wedged receiver, not a
/// throughput concern. A lagged receiver just means a WS task missed a
/// `resized`/`control_revoked` notification; the next control message it
/// sends is re-checked against the authoritative lease/size regardless (see
/// `Session::is_controller`, `Session::size`), so a miss here is not a
/// correctness bug, only a delayed UI update.
pub(super) const EVENT_CHANNEL_CAPACITY: usize = 32;
