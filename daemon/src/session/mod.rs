//! What is left of teleport's own session layer now that every session lives
//! in `anchord` (docs/20-retiring-the-in-process-pty.md).
//!
//! Three things, each with an anchor-path caller ("What stays"):
//!
//! - [`SessionId`], which the WS-ticket store is keyed by (`auth.rs`).
//! - [`CreateError`], which shapes a refused create into its HTTP status,
//!   whichever daemon did the refusing (`anchor_bridge::api_error`).
//! - [`osc`], the incremental OSC 0/2/8 scanner `anchor_watch` derives titles
//!   and resume ids with.
//!
//! Everything else that lived here -- `Session`, `SessionManager`, the
//! fan-out, replay and control-lease machinery -- was the in-process path,
//! and went with it.

use std::path::PathBuf;

use ulid::Ulid;

pub(crate) mod osc;

/// A session's identity: a ULID, so ids sort by creation time and never
/// collide across a restart. anchord's ids are ULIDs too, which is what lets
/// teleport key its tickets by this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(Ulid);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Parses a `{id}` path segment back into a `SessionId`. An id that isn't a
/// valid ULID is `404`, same as one that is well-formed but unknown
/// (docs/04-api-protocol.md#delete-apiv1sessionsid: "Reserve 404 for an
/// unknown session id" -- a malformed one is just as unknown).
impl std::str::FromStr for SessionId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Ulid::from_string(s)?))
    }
}

/// Why a create was refused, shaped for `api.rs` to map onto the HTTP status
/// docs/04-api-protocol.md#post-apiv1sessions specifies -- `422` for the two
/// validation variants, `429` for `MaxSessions`, `500` for `Spawn`.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// `422` -- the requested `command` isn't resolvable on `PATH`.
    #[error("executable not found on PATH: {0}")]
    ExecutableNotFound(String),
    /// `422` -- the requested `cwd` doesn't exist or isn't a directory.
    #[error("cwd does not exist or is not a directory: {}", .0.display())]
    InvalidCwd(PathBuf),
    /// `429` -- `max_sessions` is already at its cap.
    #[error("max_sessions ({0}) reached")]
    MaxSessions(usize),
    /// `500` -- validation passed but the spawn itself failed.
    #[error("spawning the session: {0}")]
    Spawn(#[from] anyhow::Error),
}
