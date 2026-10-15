//! Translating between teleport's API surface and anchor's.
//!
//! Kept out of `api.rs` so the mapping is one reviewable place rather than a
//! branch inside every handler. See anchor's `docs/06-teleport-seam.md`.
//!
//! Two things this file encodes:
//!
//! **Teleport's session metadata becomes anchor labels.** anchor has no
//! `kind` or `preset` column and should not grow one — labels are exactly
//! the arbitrary caller-owned metadata that absence used to force teleport
//! to keep in a side-table. They are namespaced `teleport.*` by convention;
//! anchor reserves no prefix and never interprets one.
//!
//! **Anchor's error codes become teleport's status codes.** The codes are
//! part of anchor's published protocol, so this matches on them rather than
//! on message text.

use std::collections::BTreeMap;

use crate::anchor_client::{AnchorSession, AnchorSpec};
use crate::api::ApiError;
use crate::session::CreateError;

/// Label naming which teleport owns a session. See `Config::anchor_owner`.
pub const LABEL_OWNER: &str = "teleport.owner";
/// Label carrying teleport's `kind`.
pub const LABEL_KIND: &str = "teleport.kind";
/// Label carrying the preset a session was launched from, if any.
pub const LABEL_PRESET: &str = "teleport.preset";

/// The labels teleport attaches to a session it owns.
pub fn labels_for(owner: &str, kind: &str, preset: Option<&str>) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_OWNER.to_owned(), owner.to_owned());
    labels.insert(LABEL_KIND.to_owned(), kind.to_owned());
    if let Some(preset) = preset {
        labels.insert(LABEL_PRESET.to_owned(), preset.to_owned());
    }
    labels
}

/// The filter that selects this teleport's sessions and nobody else's.
pub fn owned_by(owner: &str) -> BTreeMap<String, String> {
    let mut filter = BTreeMap::new();
    filter.insert(LABEL_OWNER.to_owned(), owner.to_owned());
    filter
}

/// A spawn spec in anchor's shape.
pub fn spec_for(
    command: String,
    args: Vec<String>,
    cwd: std::path::PathBuf,
    env: Vec<(String, String)>,
    cols: u16,
    rows: u16,
    login_shell: bool,
) -> AnchorSpec {
    AnchorSpec {
        command,
        args,
        cwd,
        env: env.into_iter().collect(),
        cols,
        rows,
        login_shell,
    }
}

/// Maps an anchor client error onto teleport's API error.
///
/// Anchor's errors are `"<code>: <message>"`, and the codes are protocol,
/// not prose — matching on them is the stable thing to do. An unrecognised
/// code is `Internal` rather than a guess: a newer daemon saying something
/// this build does not understand is exactly when not to improvise.
pub fn api_error(e: &anyhow::Error) -> ApiError {
    let text = e.to_string();
    let code = text.split(':').next().unwrap_or_default();
    match code {
        "not_found" => ApiError::NotFound,
        // The session exists but the request does not apply to it as it
        // stands, or asks for more than a response may carry. Teleport's
        // clients already treat all three as a 400.
        "already_exited" | "lease_held" | "too_large" => ApiError::BadRequest(text),
        "invalid_spec" | "invalid_label" => {
            ApiError::Create(CreateError::InvalidCwd(std::path::PathBuf::from(text)))
        }
        "spawn_failed" => ApiError::Create(CreateError::Spawn(anyhow::anyhow!("{text}"))),
        _ => ApiError::Internal(text),
    }
}

/// What teleport knows about an anchor-hosted session, for its API views.
///
/// The four fields teleport serves that anchor has no equivalent for —
/// `title`, `claude_resume_id`, `last_bell_ms`, `idle_since_ms` — are not
/// here. They are derived from the output stream, which teleport sees
/// anyway when it attaches, and they stay teleport's to compute: each is a
/// policy about what a UI shows, and a second anchor client would want
/// different ones (anchor `docs/01-scope.md`).
#[derive(Debug, Clone)]
pub struct BridgedSession {
    /// The session as anchor reports it.
    pub session: AnchorSession,
}

impl BridgedSession {
    /// Wraps an anchor session.
    pub fn new(session: AnchorSession) -> Self {
        Self { session }
    }

    /// Teleport's `kind`, from the label it set at create.
    ///
    /// Defaults to `"shell"` for a session anchor holds that teleport did
    /// not create — another client's session is still visible, and a
    /// missing label is not an error.
    pub fn kind(&self) -> String {
        self.session
            .labels
            .get(LABEL_KIND)
            .cloned()
            .unwrap_or_else(|| "shell".to_owned())
    }

    /// The preset this session was launched from, if teleport launched it.
    pub fn preset(&self) -> Option<String> {
        self.session.labels.get(LABEL_PRESET).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teleport_metadata_round_trips_through_labels() {
        let labels = labels_for("teleport", "agent", Some("claude"));
        assert_eq!(labels.get(LABEL_KIND).map(String::as_str), Some("agent"));
        assert_eq!(labels.get(LABEL_PRESET).map(String::as_str), Some("claude"));

        // The keys are valid anchor labels: lowercase, dotted, short.
        for key in labels.keys() {
            assert!(key.len() <= 64);
            assert!(key.bytes().all(|b| b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || b == b'.'
                || b == b'_'
                || b == b'-'));
        }

        // No preset means no label at all, rather than an empty one.
        assert!(!labels_for("teleport", "shell", None).contains_key(LABEL_PRESET));
        assert_eq!(
            labels.get(LABEL_OWNER).map(String::as_str),
            Some("teleport")
        );
    }

    #[test]
    fn anchor_codes_map_onto_teleport_statuses() {
        let cases = [
            ("not_found: no such session", "NotFound"),
            ("already_exited: session is not running", "BadRequest"),
            ("lease_held: someone else is typing", "BadRequest"),
            ("too_large: max must be at most", "BadRequest"),
            ("spawn_failed: no such file", "Create"),
            ("invalid_spec: cwd is not a directory", "Create"),
            // A code this build does not know is not guessed at.
            ("some_future_code: who knows", "Internal"),
        ];
        for (text, expected) in cases {
            let mapped = api_error(&anyhow::anyhow!("{text}"));
            let actual = match mapped {
                ApiError::NotFound => "NotFound",
                ApiError::BadRequest(_) => "BadRequest",
                ApiError::Create(_) => "Create",
                ApiError::Internal(_) => "Internal",
                _ => "other",
            };
            assert_eq!(actual, expected, "for {text}");
        }
    }
}
