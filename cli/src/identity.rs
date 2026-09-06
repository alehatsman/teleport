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
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    format!("{user} (cli)")
}
