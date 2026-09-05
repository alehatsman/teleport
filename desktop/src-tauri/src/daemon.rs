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
/// well-trodden one; the Windows path needs the spike in
/// docs/11-mvp-plan.md#m10 run against an actual packaged build before this
/// is trusted.
pub fn spawn_detached() -> Result<()> {
    let path = sidecar_path()?;
    let mut cmd = std::process::Command::new(&path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

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

    cmd.spawn()
        .with_context(|| format!("spawning detached daemon at {}", path.display()))?;
    Ok(())
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
/// **Unix only.** Windows has no SIGTERM equivalent for a console-less
/// background process (`GenerateConsoleCtrlEvent` needs a shared console,
/// which a Task-Scheduler-launched, detached `teleportd` won't have) --
/// tracked as an open gap in docs/11-mvp-plan.md#m10, not silently papered
/// over with an ungraceful `taskkill`. Needs either a small authenticated
/// `POST /api/v1/shutdown` on the daemon, or a console-ctrl-handler dance;
/// deferred rather than decided unilaterally since it's a (small) daemon
/// change this milestone otherwise doesn't need.
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
