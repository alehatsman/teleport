//! Connection resolution (docs/11-mvp-plan.md#m11--cli-client): `--url`/
//! `--token`/`TELEPORT_TOKEN`, else local-daemon auto-discovery via
//! `<data_dir>/port` + `<data_dir>/token` -- the same two files
//! `desktop/src-tauri/src/daemon.rs` already reads for M10.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// A resolved daemon endpoint: an HTTP base URL (no trailing slash) and the
/// bearer token to present on every request. `teleport` never sends an
/// `Origin` header on any request it makes -- native clients "have no
/// excuse" not to use the header instead
/// (docs/06-security.md#token-on-the-websocket-upgrade), so the credential
/// here is the whole authentication story for this client, not a fallback.
#[derive(Debug)]
pub struct Connection {
    pub base_url: String,
    pub token: String,
}

/// Resolves `<data_dir>` exactly as `daemon/src/main.rs::resolve_data_dir`
/// and `desktop/src-tauri`'s own copy do: `--data-dir` overrides, otherwise
/// the platform's local (not roaming) data dir. Three independent copies of
/// this logic, deliberately -- `cli/`, `daemon/` and `desktop/src-tauri/`
/// are three crates with no shared workspace
/// (docs/11-mvp-plan.md#m11's interfaces note) -- but a weaker duplication
/// than that tradeoff usually implies: all three delegate the actual
/// per-OS logic to the same `directories` crate call
/// (`BaseDirs::data_local_dir()`), so the only thing repeated here is the
/// ~3-line wrapper (override, then `.join("teleport")`), not a
/// reimplementation of platform path resolution. The test below pins that
/// wrapper's exact behavior so a drift in the join segment specifically
/// (the one part a shared call can't catch) fails loudly here rather than
/// showing up as "no local teleportd found" against a directory the daemon
/// never wrote to.
pub fn data_dir(override_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir);
    }
    let base = directories::BaseDirs::new()
        .context("could not determine the platform data directory (no home directory?)")?;
    Ok(base.data_local_dir().join("teleport"))
}

/// `--url`/`--token` win outright; `TELEPORT_TOKEN` (and, for symmetry,
/// `TELEPORT_URL`) come next; local auto-discovery is the fallback and only
/// ever applies to *both* url and token together -- a `<data_dir>/token`
/// on this machine is never a credential for someone else's `--url`.
pub fn resolve(
    url_flag: Option<String>,
    token_flag: Option<String>,
    data_dir_override: Option<PathBuf>,
) -> Result<Connection> {
    resolve_inner(
        url_flag,
        token_flag,
        data_dir_override,
        std::env::var("TELEPORT_URL").ok(),
        std::env::var("TELEPORT_TOKEN").ok(),
    )
}

/// Env values passed in rather than read here, so tests can exercise every
/// precedence path deterministically -- `std::env::var` is global process
/// state, and `cargo test` runs unit tests in one process, multi-threaded.
fn resolve_inner(
    url_flag: Option<String>,
    token_flag: Option<String>,
    data_dir_override: Option<PathBuf>,
    url_env: Option<String>,
    token_env: Option<String>,
) -> Result<Connection> {
    if let Some(base_url) = url_flag.or(url_env) {
        let token = token_flag
            .or(token_env)
            .context("--url was given but no token was -- pass --token or set TELEPORT_TOKEN")?;
        return Ok(Connection {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
        });
    }

    let dir = data_dir(data_dir_override)?;
    let port = std::fs::read_to_string(dir.join("port"))
        .with_context(|| {
            format!(
                "no --url given and no local teleportd found at {} -- is it running? \
                 (pass --url/--token to connect to a remote one)",
                dir.join("port").display()
            )
        })?
        .trim()
        .to_string();
    let token = token_flag
        .or(token_env)
        .or_else(|| {
            std::fs::read_to_string(dir.join("token"))
                .ok()
                .map(|s| s.trim().to_string())
        })
        .with_context(|| {
            format!(
                "no token given (--token/TELEPORT_TOKEN) and none found at {}",
                dir.join("token").display()
            )
        })?;

    Ok(Connection {
        base_url: format!("http://127.0.0.1:{port}"),
        token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_override_wins_outright() {
        let dir = data_dir(Some(PathBuf::from("/tmp/explicit"))).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/explicit"));
    }

    /// Pins the one part of `data_dir` that's actually duplicated across
    /// `cli/`, `daemon/` and `desktop/src-tauri/` -- the `directories` crate
    /// call itself is shared, upstream logic, not reimplemented here.
    #[test]
    fn data_dir_joins_the_teleport_segment() {
        let dir = data_dir(None).unwrap();
        assert_eq!(dir.file_name().unwrap(), "teleport");
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("teleport-cli-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn url_flag_wins_and_strips_trailing_slash() {
        let conn = resolve_inner(
            Some("https://example.ts.net/".to_string()),
            Some("tok".to_string()),
            None,
            Some("https://env-should-lose.example".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(conn.base_url, "https://example.ts.net");
        assert_eq!(conn.token, "tok");
    }

    #[test]
    fn url_env_used_when_no_flag() {
        let conn = resolve_inner(
            None,
            Some("tok".to_string()),
            None,
            Some("https://from-env.example".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(conn.base_url, "https://from-env.example");
    }

    #[test]
    fn url_without_any_token_is_an_error() {
        let err = resolve_inner(
            Some("https://example.ts.net".to_string()),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no token"));
    }

    #[test]
    fn token_env_used_over_url_env_precedence_is_independent() {
        // token comes from env even though url came from the flag -- the
        // two resolve independently of each other.
        let conn = resolve_inner(
            Some("https://example.ts.net".to_string()),
            None,
            None,
            None,
            Some("tok-from-env".to_string()),
        )
        .unwrap();
        assert_eq!(conn.token, "tok-from-env");
    }

    #[test]
    fn local_auto_discovery_reads_port_and_token_files() {
        let dir = scratch_dir("auto-discovery");
        std::fs::write(dir.join("port"), "12345\n").unwrap();
        std::fs::write(dir.join("token"), "file-token\n").unwrap();

        let conn = resolve_inner(None, None, Some(dir.clone()), None, None).unwrap();
        assert_eq!(conn.base_url, "http://127.0.0.1:12345");
        assert_eq!(conn.token, "file-token");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn token_flag_overrides_local_token_file() {
        let dir = scratch_dir("token-override");
        std::fs::write(dir.join("port"), "12345").unwrap();
        std::fs::write(dir.join("token"), "file-token").unwrap();

        let conn = resolve_inner(
            None,
            Some("flag-token".to_string()),
            Some(dir.clone()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(conn.token, "flag-token");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_local_daemon_and_no_url_is_an_error() {
        let dir = scratch_dir("no-daemon");
        // no port file written
        let err = resolve_inner(None, None, Some(dir.clone()), None, None).unwrap_err();
        assert!(err.to_string().contains("no local teleportd found"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
