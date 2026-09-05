//! Health-check / spawn / attach for the bundled `teleportd` sidecar.
//!
//! Implements the flow diagram in docs/08-packaging.md#daemon-lifecycle----the-important-part
//! and the M10 spec in docs/11-mvp-plan.md#m10--tauri-shell.
//!
//! Deliberately does not use `tauri-plugin-shell`'s `Command::sidecar()` to
//! spawn the daemon. Checked against that plugin's source
//! (2026-09-05, tauri-apps/plugins-workspace) rather than assumed: it does
//! *not* use a Windows Job Object (an earlier draft of this file's spec
//! claimed otherwise -- corrected in docs/11-mvp-plan.md#m10). What it
//! *does* do is set `CREATE_NO_WINDOW` and pipe stdio through itself, both
//! wrong for a process meant to keep running headless after this app exits.
//! The real, general Windows risk this guards against is job-object
//! *nesting*: if this app's own process happens to be inside a job with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (common when launched from an IDE,
//! CI runner, or some installer contexts), children are added to that job
//! by default since Windows 8 and die with it -- `CREATE_BREAKAWAY_FROM_JOB`
//! below is the defense, regardless of how this app itself was launched.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Resolves `<data_dir>` exactly as `daemon/src/main.rs::resolve_data_dir`
/// does when no `--data-dir` override is given: `BaseDirs::data_local_dir()
/// /teleport`. Two independent guesses at this path is exactly the class of
/// bug docs/08-packaging.md's port-file section exists to prevent.
pub fn data_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new()
        .context("could not determine the platform data directory (no home directory?)")?;
    Ok(base.data_local_dir().join("teleport"))
}

/// Public so `main.rs` can rebuild the `http://127.0.0.1:<port>/?token=<token>`
/// window URL after a successful [`probe`] without this module threading
/// both values through `Health` (they're already on disk; re-reading two
/// small files is cheaper than widening the struct).
pub fn read_port(data_dir: &Path) -> Option<u16> {
    std::fs::read_to_string(data_dir.join("port"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn read_token(data_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(data_dir.join("token")).ok()?;
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Where [`spawn_detached`] redirects the daemon's stdout/stderr.
/// `teleportd` itself only ever logs to its own stdout/stderr
/// (`daemon/src/main.rs`'s `tracing_subscriber::fmt()`, no file appender) --
/// this file is the *only* persisted copy of that output, and exists purely
/// so this shell has something to show when startup fails
/// (docs/11-mvp-plan.md#m10, issue #15).
pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("teleportd.log")
}

/// Best-effort read of the last `max_bytes` of the daemon log, for a "why
/// didn't it come up" dialog. Never errors the caller -- a missing or
/// unreadable log just becomes an explanatory string in its place.
pub fn read_log_tail(data_dir: &Path, max_bytes: usize) -> String {
    let path = log_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(max_bytes);
            let tail = String::from_utf8_lossy(&bytes[start..]).trim().to_string();
            if tail.is_empty() {
                format!("(log at {} is empty)", path.display())
            } else {
                tail
            }
        }
        Err(e) => format!("(could not read log at {}: {e})", path.display()),
    }
}

/// The authenticated slice of `GET /api/v1/health`'s response
/// (docs/04-api-protocol.md#get-apiv1health). Other fields exist on the
/// wire; only what this shell needs is modeled here.
#[derive(Debug, Deserialize)]
pub struct Health {
    pub version: String,
    pub device_id: Option<String>,
    pub device_name: Option<String>,
    pub sessions_running: Option<u64>,
    pub pid: Option<u32>,
}

impl Health {
    /// Per 08: "the authenticated shape is the entire check." Only a daemon
    /// holding our token can return `device_id` et al, and only a process
    /// running as our OS user can have read that token file -- so the shape
    /// itself proves ownership. Never gate on `device_id`'s *value*.
    fn proves_ownership(&self) -> bool {
        self.device_id.is_some()
    }
}

pub enum Probe {
    /// A daemon holding our token answered. Attach.
    Ours(Health),
    /// Something answered `/health` but not with our token's shape --
    /// another OS user's daemon, or our token file is stale. Do not attach.
    NotOurs,
    /// Refused, timed out, or no port file at all. Nothing we can attach to.
    NoDaemon,
}

/// One health check against `<data_dir>/port` + `<data_dir>/token`, with a
/// short timeout -- called on launch, and after starting the daemon while
/// polling for it to come up (docs/08-packaging.md's flow diagram).
pub async fn probe(data_dir: &Path) -> Probe {
    let Some(port) = read_port(data_dir) else {
        return Probe::NoDaemon;
    };
    let token = read_token(data_dir);
    let url = format!("http://127.0.0.1:{port}/api/v1/health");
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Probe::NoDaemon,
    };
    let mut req = client.get(&url);
    if let Some(t) = &token {
        req = req.bearer_auth(t);
    }
    let resp = match req.send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return Probe::NoDaemon,
    };
    match resp.json::<Health>().await {
        Ok(h) if h.proves_ownership() => Probe::Ours(h),
        _ => Probe::NotOurs,
    }
}

/// Starts `teleportd` as a fully detached process -- not a child of this
/// process, not in any job/process-group this app belongs to, so it
/// survives this app quitting (docs/08-packaging.md, docs/11-mvp-plan.md#m10
/// edge cases). Uses the `externalBin` sidecar Tauri bundled next to us,
/// resolved via [`sidecar_path`].
///
/// Plain `std::process::Command`, not `tauri_plugin_shell::ShellExt`'s
/// `sidecar()` -- see this module's top comment for why (stdio piping +
/// `CREATE_NO_WINDOW` from that helper are both wrong here; this needs full
/// detachment instead). The Unix path (`process_group(0)`) is the
/// well-trodden one; the Windows path was spiked for real on real hardware
/// 2026-09-05 (`spike/src/bin/s11_windows_job_breakaway.rs`,
/// docs/11-mvp-plan.md#m10) -- see [`spawn_windows_with_breakaway_retry`]
/// for what that found.
pub fn spawn_detached(data_dir: &Path) -> Result<()> {
    let path = sidecar_path()?;

    // stdout/stderr go to `log_path` (truncated on every spawn, so it always
    // holds just the most recent run -- not an unboundedly-growing log)
    // rather than `Stdio::null()`, so a "didn't come up" dialog has
    // something real to show (issue #15). `create_dir_all` covers first run,
    // before `teleportd` itself has ever created this directory.
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let log_file = std::fs::File::create(log_path(data_dir)).with_context(|| {
        format!(
            "creating daemon log file at {}",
            log_path(data_dir).display()
        )
    })?;
    let log_file_stderr = log_file
        .try_clone()
        .context("cloning daemon log file handle")?;

    let mut cmd = std::process::Command::new(&path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file_stderr));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New session + process group: detaches from this process's
        // controlling terminal and process group, so it isn't a target of
        // signals sent to *our* group (e.g. a shell's Ctrl-C, or whatever
        // sends SIGTERM to this app's group on quit).
        cmd.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
        };
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB);
    }

    #[cfg(windows)]
    return spawn_windows_with_breakaway_retry(&mut cmd, &path);

    #[cfg(not(windows))]
    {
        cmd.spawn()
            .with_context(|| format!("spawning detached daemon at {}", path.display()))?;
        Ok(())
    }
}

/// Spawns `cmd` (already configured with `CREATE_BREAKAWAY_FROM_JOB`),
/// retrying without that flag if the OS refuses it.
///
/// **Found on real hardware, 2026-09-05**
/// (`spike/src/bin/s11_windows_job_breakaway.rs`, docs/11-mvp-plan.md#m10):
/// `CREATE_BREAKAWAY_FROM_JOB` does not silently no-op when the *containing*
/// job doesn't allow it -- it fails the entire `CreateProcess` call with
/// `ERROR_ACCESS_DENIED` (`io::ErrorKind::PermissionDenied`) unless that job
/// itself was created with `JOB_OBJECT_LIMIT_BREAKAWAY_OK` (or
/// `_SILENT_BREAKAWAY_OK`). Verified with four arms on this exact machine:
/// a job without that flag makes the breakaway spawn fail outright
/// (confirmed -- this is the case this function exists to survive); the
/// same job *with* that flag lets it succeed and the child outlives the
/// parent; and no containing job at all (the common case for a plain
/// double-clicked desktop launch) also succeeds, since there is nothing to
/// break away from. Many real restrictive jobs (some IDE debuggers, CI
/// runners, some installer/sandboxing contexts) do not grant breakaway --
/// exactly the launch contexts this flag was added to defend against in the
/// first place -- so failing here outright would mean the daemon never
/// starts under precisely those contexts, which is worse than the
/// kill-with-the-job risk this flag exists to prevent. Retry without it:
/// the daemon still starts, at the cost of reintroducing that risk only in
/// the specific case where the OS has just told us breakaway isn't
/// available anyway.
#[cfg(windows)]
fn spawn_windows_with_breakaway_retry(cmd: &mut std::process::Command, path: &Path) -> Result<()> {
    match cmd.spawn() {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                "spawning {} with CREATE_BREAKAWAY_FROM_JOB was denied -- this process's \
                 containing job does not permit breakaway; retrying without it",
                path.display()
            );
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{
                CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
            };
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
            cmd.spawn().map(|_| ()).with_context(|| {
                format!(
                    "spawning detached daemon at {} (retry without CREATE_BREAKAWAY_FROM_JOB)",
                    path.display()
                )
            })
        }
        Err(e) => Err(e).with_context(|| format!("spawning detached daemon at {}", path.display())),
    }
}

/// Locates the `externalBin` sidecar Tauri placed next to this app's own
/// executable at build time. **Not** `AppHandle::path().resource_dir()`:
/// checked against Tauri's own sidecar resolution
/// (`relative_command_path` in tauri-plugin-shell's `process/mod.rs`) rather
/// than assumed, and it resolves relative to `current_exe()`'s directory,
/// which is a different path than `resource_dir()` on both Linux (a
/// installed `.deb`'s `resource_dir()` is `/usr/lib/<name>`, not
/// `/usr/bin`) and macOS (`resource_dir()` is `Contents/Resources`; the
/// bundler places `externalBin` sidecars in `Contents/MacOS`, alongside the
/// main binary). The build step (see `../../scripts/copy-sidecar.sh`) is
/// what appends the target-triple suffix on disk; this reads the plain name
/// because Tauri's bundler strips the suffix back off for the installed
/// build.
pub fn sidecar_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolving this app's own executable path")?;
    let dir = exe
        .parent()
        .context("this app's executable path has no parent directory")?;
    let name = if cfg!(windows) {
        "teleportd.exe"
    } else {
        "teleportd"
    };
    Ok(dir.join(name))
}

/// Graceful stop, reusing the exact shutdown path `daemon/src/main.rs`
/// already listens for (`shutdown_signal()`'s Ctrl+C/SIGTERM branch) --
/// sessions get the same clean persistence-and-close treatment as any other
/// planned stop. Requires the `pid` from an *authenticated* `/health`
/// response ([`Health::pid`]), never a pid guessed some other way.
///
/// **Unix only** -- see [`shutdown_gracefully`] for the Windows leg.
/// `GenerateConsoleCtrlEvent` needs a shared console, which a
/// Task-Scheduler-launched, detached `teleportd` won't have, so a plain
/// `kill(SIGTERM)` has no Windows equivalent at all.
#[cfg(unix)]
pub fn terminate_gracefully(pid: u32) -> Result<()> {
    // SAFETY: `kill(2)` with SIGTERM on a pid we read from our own daemon's
    // authenticated /health response -- no memory involved, a plain syscall.
    let ok = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } == 0;
    if ok {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "kill(SIGTERM) on pid {pid} failed: {}",
            std::io::Error::last_os_error()
        ))
    }
}

/// Graceful stop for Windows: `POST /api/v1/shutdown`
/// (docs/04-api-protocol.md#post-apiv1shutdown, docs/11-mvp-plan.md#m10,
/// issue #12), reusing the same `shutdown_signal()` path
/// [`terminate_gracefully`] drives on Unix, just triggered over the
/// existing authenticated HTTP surface instead of a signal -- a
/// console-less, autostart-launched `teleportd` has no
/// `GenerateConsoleCtrlEvent` target (no shared console) and this shell
/// must never fall back to an ungraceful `taskkill`. Not used on Unix,
/// which keeps its working `kill(SIGTERM)` path unchanged -- `#[cfg(windows)]`
/// here, not a shared cross-platform fn, so an accidental call from the
/// Unix branch is a compile error rather than a silent extra round trip.
///
/// Re-reads `port`/`token` from disk rather than widening [`Health`] with
/// them: whether this shell is *allowed* to ask the daemon to stop is the
/// daemon's own auth check to make on the request, not something to gate on
/// here from a struct built for an unrelated purpose.
#[cfg(windows)]
pub async fn shutdown_gracefully(data_dir: &Path) -> Result<()> {
    let port = read_port(data_dir).context("no port file -- is the daemon running?")?;
    let token = read_token(data_dir).context("no token file -- is the daemon running?")?;
    let url = format!("http://127.0.0.1:{port}/api/v1/shutdown");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building HTTP client")?;
    let resp = client
        .post(&url)
        .bearer_auth(&token)
        .send()
        .await
        .context("sending POST /api/v1/shutdown")?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "POST /api/v1/shutdown returned {}",
            resp.status()
        ))
    }
}
