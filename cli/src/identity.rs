//! Persisted client identity (docs/04-api-protocol.md#client-identity),
//! mirroring `web/src/lib/identity.ts`'s role: a stable `client_id` so a
//! reconnecting client resumes its own control lease instead of racing for
//! it, plus a human-readable `client_name` shown as the controller label in
//! every other client's UI. Neither is a credential -- see
//! `connect.rs`/`docs/06-security.md#authentication` for the one that is.

use std::path::Path;

/// Reads `<data_dir>/cli-identity`, or generates and persists a fresh ULID
/// if it doesn't exist yet. Not sensitive -- no `0600` restriction needed,
/// unlike `connect.rs`'s token handling.
pub fn client_id(data_dir: &Path) -> String {
    let path = data_dir.join("cli-identity");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let id = ulid::Ulid::new().to_string();
    // Best-effort: a write failure (read-only data dir, race with another
    // `teleport` invocation) just means this run gets an id nobody
    // persisted -- the next run tries again. Not fatal to attaching now.
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(&path, &id);
    id
}

/// `"aleh (cli)"`-shaped default, mirroring `identity.ts`'s
/// `defaultClientName()` (browser + OS) with the OS username in place of a
/// browser's user-agent string -- the closest CLI equivalent of "who is
/// this". No new dependency for hostname lookup; the username env var is
/// enough to be recognizable in the controller label.
pub fn default_client_name() -> String {
    default_client_name_from(std::env::var("USER").ok(), std::env::var("USERNAME").ok())
}

/// Env values passed in rather than read here, so tests don't mutate
/// process-global env state (`cargo test` runs unit tests in one process,
/// multi-threaded -- same reasoning as `connect.rs`'s `resolve_inner`).
fn default_client_name_from(user: Option<String>, username: Option<String>) -> String {
    // `.filter(|s| !s.is_empty())`, not just presence: some container/sudo
    // setups export `USER=""` rather than leaving it unset, which is
    // `Some(String::new())`, not `None` -- without the filter this would
    // never fall through to `USERNAME`/the default, and the controller
    // label would end up as `" (cli)"`, a blank name with a leading space.
    let user = user
        .filter(|s| !s.is_empty())
        .or_else(|| username.filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".to_string());
    format!("{user} (cli)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_user_when_present() {
        assert_eq!(
            default_client_name_from(Some("aleh".to_string()), Some("someone".to_string())),
            "aleh (cli)"
        );
    }

    #[test]
    fn falls_back_to_username_when_user_unset() {
        assert_eq!(
            default_client_name_from(None, Some("aleh".to_string())),
            "aleh (cli)"
        );
    }

    #[test]
    fn empty_user_falls_through_to_username() {
        assert_eq!(
            default_client_name_from(Some(String::new()), Some("aleh".to_string())),
            "aleh (cli)"
        );
    }

    #[test]
    fn empty_user_and_username_falls_through_to_unknown() {
        assert_eq!(
            default_client_name_from(Some(String::new()), Some(String::new())),
            "unknown (cli)"
        );
    }

    #[test]
    fn neither_set_falls_through_to_unknown() {
        assert_eq!(default_client_name_from(None, None), "unknown (cli)");
    }
}
