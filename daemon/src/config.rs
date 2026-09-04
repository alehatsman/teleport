//! `<data_dir>/config.toml` -- the daemon configuration surface
//! (docs/07-remote-access.md#daemon-configuration-surface). Everything here
//! has a hardcoded default; the file, when present, overrides fields
//! individually -- an empty or partial `config.toml` is valid and fills in
//! only what it names. CLI flags override the file (`main.rs`'s job, not
//! this module's).
//!
//! `listen` is deliberately **not** a field here: `main.rs` already owns
//! that via `--listen`, and docs/12-identity-and-connectivity.md#connectivity-inbound-vs-outbound
//! says the listener setup stays in one function in `main.rs`. Duplicating it
//! into the config file would just be a second source of truth for the same
//! value.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::log::{DEFAULT_LOG_MAX_BYTES, DEFAULT_LOG_WARN_BYTES};

/// `after=0` on a huge log clamps to this many bytes
/// (docs/04-api-protocol.md#bounded-attach).
pub const DEFAULT_MAX_REPLAY_BYTES: u64 = 8 * 1024 * 1024;
/// Replay when a client attaches with no cursor
/// (docs/04-api-protocol.md#bounded-attach).
pub const DEFAULT_TAIL: u64 = 1024 * 1024;
/// A dropped controller may resume its lease this long
/// (docs/04-api-protocol.md#disconnect-grace).
pub const DEFAULT_CONTROL_GRACE_MS: u64 = 15_000;
/// Refuse further spawns past this many concurrent sessions
/// (docs/06-security.md#process-spawning).
pub const DEFAULT_MAX_SESSIONS: usize = 50;
/// GC deletes an `exited`/`lost` session's directory + row once its
/// `exited_at_ms` is this many days old (docs/05-persistence.md#garbage-collection).
pub const DEFAULT_RETAIN_DAYS: u64 = 14;

/// The on-disk shape. Every field optional so a partial file is valid;
/// [`Config::load`] layers it onto [`Config::default`].
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    allowed_origins: Option<Vec<String>>,
    allowed_hosts: Option<Vec<String>>,
    auth_token: Option<bool>,
    max_sessions: Option<usize>,
    control_grace_ms: Option<u64>,
    default_tail: Option<u64>,
    max_replay_bytes: Option<u64>,
    log_warn_bytes: Option<u64>,
    log_max_bytes: Option<u64>,
    retain_days: Option<u64>,
}

/// The resolved configuration every other module reads from -- no `Option`s,
/// defaults already applied.
#[derive(Debug, Clone)]
pub struct Config {
    /// Extra browser origins to accept beyond the loopback/Tauri/dev-server
    /// defaults `auth.rs` always allows -- e.g. a Tailscale hostname
    /// (docs/06-security.md#browser-origin-defense).
    pub allowed_origins: Vec<String>,
    /// Extra `Host` header values to accept beyond `127.0.0.1:<port>` /
    /// `localhost:<port>`, which are always allowed.
    pub allowed_hosts: Vec<String>,
    /// `false` disables the bearer-token requirement entirely -- a
    /// single-user-machine convenience, not the default
    /// (docs/06-security.md#authentication).
    pub auth_token: bool,
    pub max_sessions: usize,
    pub control_grace_ms: u64,
    pub default_tail: u64,
    pub max_replay_bytes: u64,
    pub log_warn_bytes: u64,
    pub log_max_bytes: u64,
    /// GC threshold, in days since `exited_at_ms` (docs/05-persistence.md#garbage-collection).
    pub retain_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            allowed_hosts: Vec::new(),
            auth_token: true,
            max_sessions: DEFAULT_MAX_SESSIONS,
            control_grace_ms: DEFAULT_CONTROL_GRACE_MS,
            default_tail: DEFAULT_TAIL,
            max_replay_bytes: DEFAULT_MAX_REPLAY_BYTES,
            log_warn_bytes: DEFAULT_LOG_WARN_BYTES,
            log_max_bytes: DEFAULT_LOG_MAX_BYTES,
            retain_days: DEFAULT_RETAIN_DAYS,
        }
    }
}

impl Config {
    /// Loads `<data_dir>/config.toml`, layering it onto the defaults. A
    /// missing file is not an error -- first run has none yet, and the
    /// defaults are a complete, valid configuration on their own.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("config.toml");
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let file: ConfigFile =
            toml::from_str(&contents).with_context(|| format!("parsing {}", path.display()))?;
        let defaults = Self::default();
        Ok(Self {
            allowed_origins: file.allowed_origins.unwrap_or(defaults.allowed_origins),
            allowed_hosts: file.allowed_hosts.unwrap_or(defaults.allowed_hosts),
            auth_token: file.auth_token.unwrap_or(defaults.auth_token),
            max_sessions: file.max_sessions.unwrap_or(defaults.max_sessions),
            control_grace_ms: file.control_grace_ms.unwrap_or(defaults.control_grace_ms),
            default_tail: file.default_tail.unwrap_or(defaults.default_tail),
            max_replay_bytes: file.max_replay_bytes.unwrap_or(defaults.max_replay_bytes),
            log_warn_bytes: file.log_warn_bytes.unwrap_or(defaults.log_warn_bytes),
            log_max_bytes: file.log_max_bytes.unwrap_or(defaults.log_max_bytes),
            retain_days: file.retain_days.unwrap_or(defaults.retain_days),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teleportd-config-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_file_is_all_defaults() {
        let dir = scratch_dir("missing");
        let cfg = Config::load(&dir).expect("load");
        assert_eq!(cfg.max_sessions, DEFAULT_MAX_SESSIONS);
        assert!(cfg.auth_token);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_file_only_overrides_what_it_names() {
        let dir = scratch_dir("partial");
        std::fs::write(
            dir.join("config.toml"),
            "max_sessions = 5\nauth_token = false\n",
        )
        .unwrap();
        let cfg = Config::load(&dir).expect("load");
        assert_eq!(cfg.max_sessions, 5);
        assert!(!cfg.auth_token);
        assert_eq!(
            cfg.default_tail, DEFAULT_TAIL,
            "unnamed fields keep their default"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
