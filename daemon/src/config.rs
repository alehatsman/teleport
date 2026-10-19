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
use std::path::{Path, PathBuf};

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
    auth_passkey: Option<bool>,
    max_sessions: Option<usize>,
    control_grace_ms: Option<u64>,
    default_tail: Option<u64>,
    max_replay_bytes: Option<u64>,
    log_warn_bytes: Option<u64>,
    log_max_bytes: Option<u64>,
    retain_days: Option<u64>,
    use_anchor: Option<bool>,
    anchor_owner: Option<String>,
    anchor_socket: Option<PathBuf>,
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
    /// `false` removes the `/api/v1/auth/*` routes and stops existing
    /// passkey sessions resolving; the master token still works. The
    /// emergency off-switch, not a tuning knob
    /// (docs/17-passkey-login.md#config).
    ///
    /// Meaningless when `auth_token` is `false`, which disables all
    /// authentication -- one flag must never half-disable the other, so
    /// `auth_routes.rs` requires both.
    pub auth_passkey: bool,
    /// Refuse to spawn past this many concurrent sessions -- `429`
    /// (docs/06-security.md#process-spawning).
    pub max_sessions: usize,
    /// How long a disconnected controller keeps its lease before it's up
    /// for grabs (docs/04-api-protocol.md#disconnect-grace).
    pub control_grace_ms: u64,
    /// Default replay window on attach when the client sends neither
    /// `after` nor `tail` (docs/04-api-protocol.md#bounded-attach).
    pub default_tail: u64,
    /// Hard cap on how much history a single attach may replay
    /// (docs/04-api-protocol.md#bounded-attach).
    pub max_replay_bytes: u64,
    /// See [`crate::log::LogLimits::warn_bytes`].
    pub log_warn_bytes: u64,
    /// See [`crate::log::LogLimits::max_bytes`].
    pub log_max_bytes: u64,
    /// GC threshold, in days since `exited_at_ms` (docs/05-persistence.md#garbage-collection).
    pub retain_days: u64,
    /// Run sessions in an external `anchord` instead of this process.
    ///
    /// **Default `false`, deliberately.** The in-process PTY layer stays
    /// authoritative until the anchor path has passed this suite and shipped
    /// once; see anchor's `ROADMAP.md` M4/M5. Flipping this is the whole
    /// point of the extraction — it is what lets teleport ship a release
    /// while agents keep running — but it is not flipped by being written.
    pub use_anchor: bool,
    /// Which sessions in `anchord` are this teleport's.
    ///
    /// anchord is shared by design — teleport and workmanager both drive it
    /// — so teleport lists and counts only sessions carrying this owner.
    /// Another tool's sessions are visible to anyone who asks anchord
    /// directly, but they are not teleport's to show or to count against
    /// its own `max_sessions`.
    ///
    /// Stable across restarts on purpose: a value derived per process would
    /// orphan teleport's own sessions on every restart, which is the exact
    /// thing running on anchor is meant to prevent.
    pub anchor_owner: String,
    /// Where `anchord` listens. Unset uses anchor's own resolution:
    /// `$XDG_RUNTIME_DIR/anchor.sock`, falling back to
    /// `$XDG_DATA_HOME/anchor/anchor.sock`.
    pub anchor_socket: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            allowed_origins: Vec::new(),
            allowed_hosts: Vec::new(),
            auth_token: true,
            auth_passkey: true,
            max_sessions: DEFAULT_MAX_SESSIONS,
            control_grace_ms: DEFAULT_CONTROL_GRACE_MS,
            default_tail: DEFAULT_TAIL,
            max_replay_bytes: DEFAULT_MAX_REPLAY_BYTES,
            log_warn_bytes: DEFAULT_LOG_WARN_BYTES,
            log_max_bytes: DEFAULT_LOG_MAX_BYTES,
            retain_days: DEFAULT_RETAIN_DAYS,
            use_anchor: false,
            anchor_owner: "teleport".to_string(),
            anchor_socket: None,
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
            auth_passkey: file.auth_passkey.unwrap_or(defaults.auth_passkey),
            max_sessions: file.max_sessions.unwrap_or(defaults.max_sessions),
            control_grace_ms: file.control_grace_ms.unwrap_or(defaults.control_grace_ms),
            default_tail: file.default_tail.unwrap_or(defaults.default_tail),
            max_replay_bytes: file.max_replay_bytes.unwrap_or(defaults.max_replay_bytes),
            log_warn_bytes: file.log_warn_bytes.unwrap_or(defaults.log_warn_bytes),
            log_max_bytes: file.log_max_bytes.unwrap_or(defaults.log_max_bytes),
            retain_days: file.retain_days.unwrap_or(defaults.retain_days),
            use_anchor: file.use_anchor.unwrap_or(defaults.use_anchor),
            anchor_owner: file.anchor_owner.unwrap_or(defaults.anchor_owner),
            anchor_socket: file.anchor_socket.or(defaults.anchor_socket),
        })
    }

    /// Where `anchord` listens: `anchor_socket` when set, otherwise anchor's
    /// own default for this platform.
    ///
    /// Anchor's resolution, mirrored rather than imported: teleport does not
    /// depend on the anchor crate (`anchor_client`'s module doc), so this
    /// follows the rule anchor documents in its `docs/04-protocol.md` and,
    /// for Windows, its `docs/09-windows.md` §3.2.
    ///
    /// The field is still called `anchor_socket` on Windows, where it names
    /// a pipe (`\\.\pipe\...`) rather than a file: docs/20 keeps the field,
    /// and an endpoint is a path-shaped string on both platforms, so one
    /// field with a platform-specific meaning beats two fields where one is
    /// always ignored.
    pub fn anchor_endpoint(&self) -> Result<PathBuf> {
        match &self.anchor_socket {
            Some(endpoint) => Ok(endpoint.clone()),
            None => default_anchor_endpoint(),
        }
    }
}

/// Anchor's Unix default: `$XDG_RUNTIME_DIR/anchor.sock`, falling back to
/// `$XDG_DATA_HOME/anchor/anchor.sock` and then `~/.local/share`. A
/// variable that is set but empty counts as absent, as it does in anchor.
#[cfg(unix)]
fn default_anchor_endpoint() -> Result<PathBuf> {
    let var = |name| std::env::var_os(name);
    unix_anchor_endpoint(var("XDG_RUNTIME_DIR"), var("XDG_DATA_HOME"), var("HOME"))
        .context("cannot resolve anchord's socket: set anchor_socket in config.toml")
}

/// The pure half of the Unix rule, so it is testable without mutating the
/// process environment. Compiled everywhere: it is string handling, and a
/// Windows run checking it costs nothing.
#[cfg_attr(
    all(not(unix), not(test)),
    expect(
        dead_code,
        reason = "only the Unix default calls it; tests run it everywhere"
    )
)]
fn unix_anchor_endpoint(
    runtime_dir: Option<std::ffi::OsString>,
    data_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let present = |v: Option<std::ffi::OsString>| v.filter(|v| !v.is_empty());
    if let Some(run) = present(runtime_dir) {
        return Some(PathBuf::from(run).join("anchor.sock"));
    }
    let data = present(data_home)
        .map(PathBuf::from)
        .or_else(|| present(home).map(|home| PathBuf::from(home).join(".local/share")))?;
    Some(data.join("anchor/anchor.sock"))
}

/// Anchor's Windows default: `\\.\pipe\anchor-<SID>`, the string form of the
/// user SID on this process's token (anchor `docs/04-protocol.md`, decided
/// in `0134e38`). No environment variable moves it, on either side; a
/// different pipe is `anchor_socket`'s job, and it takes a full `\\.\pipe\`
/// path.
#[cfg(windows)]
fn default_anchor_endpoint() -> Result<PathBuf> {
    crate::anchor_client::default_pipe()
        .context("cannot resolve anchord's pipe from this process's user SID")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "teleportd-config-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_file_is_all_defaults() {
        let dir = scratch_dir("missing");
        let cfg = Config::load(&dir).expect("load");
        assert_eq!(cfg.max_sessions, DEFAULT_MAX_SESSIONS);
        assert!(cfg.auth_token);
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partial_file_only_overrides_what_it_names() {
        let dir = scratch_dir("partial");
        fs::write(
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
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_anchor_path_is_off_unless_asked_for() {
        // The extraction's whole safety property: writing the anchor path
        // does not enable it. Only a deliberate config.toml does.
        let dir = scratch_dir("anchor-default");
        let cfg = Config::load(&dir).expect("load");
        assert!(!cfg.use_anchor);
        assert!(cfg.anchor_socket.is_none());

        fs::write(
            dir.join("config.toml"),
            "use_anchor = true\nanchor_socket = \"/run/anchor.sock\"\n",
        )
        .expect("writing config.toml");
        let cfg = Config::load(&dir).expect("load");
        assert!(cfg.use_anchor);
        assert_eq!(
            cfg.anchor_socket.as_deref(),
            Some(Path::new("/run/anchor.sock"))
        );

        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort test cleanup; nothing to do if it fails"
        )]
        let _ = fs::remove_dir_all(&dir);
    }

    fn os(s: &str) -> std::ffi::OsString {
        s.into()
    }

    #[test]
    fn the_unix_anchor_endpoint_follows_anchors_order() {
        assert_eq!(
            unix_anchor_endpoint(Some(os("/run/u")), Some(os("/d")), Some(os("/h"))),
            Some(PathBuf::from("/run/u/anchor.sock")),
            "the runtime dir wins"
        );
        assert_eq!(
            unix_anchor_endpoint(None, Some(os("/d")), Some(os("/h"))),
            Some(PathBuf::from("/d/anchor/anchor.sock"))
        );
        assert_eq!(
            unix_anchor_endpoint(None, None, Some(os("/h"))),
            Some(PathBuf::from("/h/.local/share/anchor/anchor.sock"))
        );
        assert_eq!(unix_anchor_endpoint(None, None, None), None);
    }

    #[test]
    fn a_set_but_empty_variable_is_absent_as_in_anchor() {
        assert_eq!(
            unix_anchor_endpoint(Some(os("")), Some(os("")), Some(os("/h"))),
            Some(PathBuf::from("/h/.local/share/anchor/anchor.sock"))
        );
    }

    #[test]
    fn an_explicit_anchor_socket_overrides_the_default() {
        let cfg = Config {
            anchor_socket: Some(PathBuf::from("x-endpoint")),
            ..Config::default()
        };
        assert_eq!(
            cfg.anchor_endpoint().expect("explicit"),
            PathBuf::from("x-endpoint")
        );
    }
}
