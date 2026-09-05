//! systemd **user** unit (docs/08-packaging.md#autostart-at-login).
//! `~/.config/systemd/user/teleportd.service`, `WantedBy=default.target`.
//! Lingering (`loginctl enable-linger`) is a separate, explicit opt-in for
//! surviving logout -- not enabled here; that's a bigger promise (the unit
//! keeps running with no user session at all) than "start at login".

use std::fs;

use anyhow::{Context, Result};

fn unit_dir() -> Result<std::path::PathBuf> {
    let base = directories::BaseDirs::new().context("no home directory")?;
    Ok(base.home_dir().join(".config/systemd/user"))
}

fn unit_path() -> Result<std::path::PathBuf> {
    Ok(unit_dir()?.join("teleportd.service"))
}

pub fn install() -> Result<()> {
    let exe = crate::daemon::sidecar_path()?;
    let dir = unit_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let unit = format!(
        "[Unit]\n\
         Description=Teleport session daemon\n\
         \n\
         [Service]\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    );
    let path = unit_path()?;
    fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;

    // Reload the user manager and enable, but don't fail install() if the
    // caller has no active systemd --user session (e.g. a minimal
    // container) -- autostart is a convenience, not a launch dependency
    // (docs/11-mvp-plan.md#m10 edge cases).
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "enable", "teleportd.service"])
        .status();
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", "teleportd.service"])
        .status();
    let path = unit_path()?;
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    Ok(())
}
