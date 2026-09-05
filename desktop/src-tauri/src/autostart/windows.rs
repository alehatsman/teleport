//! Task Scheduler task with a **logon trigger**
//! (docs/08-packaging.md#autostart-at-login) -- the mechanism that exists
//! specifically for starting an executable when a user logs in, as opposed
//! to the Registry `Run` key a generic autostart crate typically reaches
//! for (no crash-restart semantics, and less visible to the user in Task
//! Scheduler's UI than a named task).
//!
//! Built on the `schtasks` CLI rather than the Windows Task Scheduler COM
//! API: one dependency-free `std::process::Command` call each way, and the
//! exact commands are copy-pasteable for anyone debugging autostart by hand.
//! **Unverified on real Windows as of this scaffold** -- flagged in
//! docs/11-mvp-plan.md#m10 alongside the detached-spawn spike.

use anyhow::{Context, Result};

const TASK_NAME: &str = "TeleportTeleportd";

pub fn install() -> Result<()> {
    let exe = crate::daemon::sidecar_path()?;
    let status = std::process::Command::new("schtasks")
        .args(["/create", "/tn", TASK_NAME, "/tr"])
        .arg(format!("\"{}\"", exe.display()))
        .args(["/sc", "onlogon", "/rl", "limited", "/f"])
        .status()
        .context("running schtasks /create")?;
    anyhow::ensure!(status.success(), "schtasks /create exited with {status}");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let status = std::process::Command::new("schtasks")
        .args(["/delete", "/tn", TASK_NAME, "/f"])
        .status()
        .context("running schtasks /delete")?;
    // A missing task is not a failure to uninstall.
    if !status.success() {
        anyhow::bail!("schtasks /delete exited with {status}");
    }
    Ok(())
}
