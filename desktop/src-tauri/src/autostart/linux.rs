//! systemd **user** unit (docs/08-packaging.md#autostart-at-login).
//! `~/.config/systemd/user/teleportd.service`, `WantedBy=default.target`,
//! plus `loginctl enable-linger` so the daemon survives logout and a full
//! reboot, not just re-login -- the promise Tailscale Serve's `--bg`
//! persistence implies (issue #40; the daemon side of it was the missing
//! half). No privilege change: linger is a per-user systemd-logind flag,
//! and enabling *your own* is unprivileged by default
//! (`org.freedesktop.login1.set-self-linger`'s polkit rule ships
//! `allow_any=yes`) -- confirmed on this repo's dev box (systemd 255).
//! `daemon/src/service.rs` has the same install/uninstall for a box that
//! has never run this desktop app at all.

use std::fs;
use std::process::Command;

use anyhow::{Context, Result};

fn unit_dir() -> Result<std::path::PathBuf> {
    let base = directories::BaseDirs::new().context("no home directory")?;
    Ok(base.home_dir().join(".config/systemd/user"))
}

fn unit_path() -> Result<std::path::PathBuf> {
    Ok(unit_dir()?.join("teleportd.service"))
}

// Dotfile: systemd's unit loader ignores hidden files in this directory, so
// it can live next to teleportd.service without being mistaken for one.
// Its presence/absence is the only record of whether *this* install() call
// was the one that flipped lingering on -- see the linger comments in
// install()/uninstall() below.
fn linger_marker_path() -> Result<std::path::PathBuf> {
    Ok(unit_dir()?.join(".teleportd-linger-owner"))
}

/// Quotes a value for a systemd unit `Key=value` line (`systemd.service(5)`
/// documents shell-style quoting support for this). Without it, a sidecar
/// path containing a space splits `ExecStart` into multiple words and the
/// unit fails to start with `install()` reported as successful.
fn quote_unit_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Current lingering state for this user, queried rather than assumed --
/// see install()/uninstall() for why. `None` means the query itself failed
/// (older systemd without `--value`, no `id` binary, etc.); the caller
/// treats that the same as "not currently lingering".
fn linger_enabled() -> Option<bool> {
    let uid = Command::new("id").arg("-u").output().ok()?;
    if !uid.status.success() {
        return None;
    }
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let out = Command::new("loginctl")
        .args(["show-user", &uid, "--value", "-p", "Linger"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim() == "yes")
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
        quote_unit_value(&exe.display().to_string())
    );
    let path = unit_path()?;
    fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;

    // Reload the user manager and enable, but don't fail install() if the
    // caller has no active systemd --user session (e.g. a minimal
    // container) -- autostart is a convenience, not a launch dependency
    // (docs/11-mvp-plan.md#m10 edge cases).
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "enable", "teleportd.service"])
        .status();
    // See the module doc comment: no USER argument targets the caller, and
    // needs no privilege beyond enabling one's own lingering.
    //
    // Only flip it -- and only claim ownership via the marker file -- if it
    // wasn't already on. Otherwise uninstall() would turn off lingering
    // this user enabled for an unrelated reason.
    if !linger_enabled().unwrap_or(false) {
        let _ = Command::new("loginctl").arg("enable-linger").status();
        let _ = fs::write(linger_marker_path()?, "");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "teleportd.service"])
        .status();
    let path = unit_path()?;
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    // Symmetric with install(): only disable lingering if the marker says
    // *this* install() was the one that turned it on. If the user
    // separately wanted lingering for an unrelated reason, it's untouched.
    let marker = linger_marker_path()?;
    if marker.exists() {
        let _ = Command::new("loginctl").arg("disable-linger").status();
        let _ = fs::remove_file(&marker);
    }
    Ok(())
}
