//! `teleportd service install|uninstall` — autostart-at-login without the
//! desktop app (docs/08-packaging.md#autostart-at-login, issue #40).
//!
//! `desktop/src-tauri/src/autostart/` already does this from the Tauri
//! tray, but `autostart::install()` is reachable only from there
//! (`desktop/src-tauri/src/main.rs`'s `autostart_on` tray item) — so a
//! headless box, exactly the machine you'd reach from a phone over
//! Tailscale, can never install it at all. This gives `teleportd` its own
//! install path that needs no GUI.
//!
//! Linux only for now. macOS/Windows autostart is deliberately login-scoped
//! (a LaunchDaemon or a boot trigger both need root/elevation, which
//! docs/06-security.md#privilege rejects), so there is no headless story to
//! give them here — `install()`/`uninstall()` say so explicitly instead of
//! silently no-op'ing.
//!
//! The Linux logic mirrors `desktop/src-tauri/src/autostart/linux.rs`
//! rather than sharing code with it: `daemon/` and `desktop/src-tauri/` are
//! already two independent crates with no shared workspace or `Cargo.lock`
//! (docs/11-mvp-plan.md#m11 — cli client's interfaces note makes the same
//! call for `cli/`), and this is ~30 lines. The one difference is the exe
//! path: this resolves its own `current_exe()` rather than a sidecar's.

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result};

    fn unit_dir() -> Result<PathBuf> {
        let base = directories::BaseDirs::new().context("no home directory")?;
        Ok(base.home_dir().join(".config/systemd/user"))
    }

    fn unit_path() -> Result<PathBuf> {
        Ok(unit_dir()?.join("teleportd.service"))
    }

    // Dotfile: systemd's unit loader ignores hidden files in this directory,
    // so it can live next to teleportd.service without being mistaken for
    // one. Its presence/absence is the only record of whether *this*
    // install() call was the one that flipped lingering on -- see the
    // linger comments in install()/uninstall() below.
    fn linger_marker_path() -> Result<PathBuf> {
        Ok(unit_dir()?.join(".teleportd-linger-owner"))
    }

    /// Quotes a value for a systemd unit `Key=value` line (`systemd.service(5)`
    /// documents shell-style quoting support for this). Without it, a path
    /// containing a space splits `ExecStart` into multiple words and the
    /// unit fails to start `install()` reported as successful.
    fn quote_unit_value(value: &str) -> String {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// `current_exe()` is whatever binary is running *right now* -- for a
    /// `cargo build` output that's `target/debug|release/teleportd`, which
    /// `cargo clean` or the next build can remove out from under an
    /// installed unit with no error until the next boot. There's no single
    /// "real" install location to fall back to (daemon/ ships no installer,
    /// docs/08-packaging.md), so warn rather than silently trust it.
    fn warn_if_build_output(exe: &Path) {
        let comps: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
        let is_build_output = comps
            .windows(2)
            .any(|w| w[0] == "target" && (w[1] == "debug" || w[1] == "release"));
        if is_build_output {
            eprintln!(
                "warning: {} looks like a `cargo build` output path, not a stable install \
                 location -- a later build or `cargo clean` can remove it, silently breaking \
                 this autostart unit. Install teleportd somewhere stable first if this is meant \
                 to persist.",
                exe.display()
            );
        }
    }

    /// Current lingering state for this user, queried rather than assumed --
    /// see install()/uninstall() for why. `None` means the query itself
    /// failed (older systemd without `--value`, no `id` binary, etc.); the
    /// caller treats that the same as "not currently lingering".
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
        let exe = std::env::current_exe().context("resolving this binary's own path")?;
        warn_if_build_output(&exe);
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

        // Best-effort, like desktop's copy of this: don't fail install() if
        // there's no active systemd --user session (docs/11-mvp-plan.md#m10
        // edge cases) -- autostart is a convenience, not a launch dependency.
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let _ = Command::new("systemctl")
            .args(["--user", "enable", "teleportd.service"])
            .status();
        // The headless-reboot half of #40: `WantedBy=default.target` only
        // starts the unit at *login*. `loginctl enable-linger` (no USER
        // argument targets the caller) keeps the user's systemd instance,
        // and this unit with it, running after logout and across reboot.
        // No privilege needed -- systemd-logind's own polkit rule
        // (org.freedesktop.login1.set-self-linger) defaults to
        // allow_any=yes for a user enabling their *own* lingering,
        // confirmed on this repo's dev box (systemd 255); it never touches
        // 06-security.md's privilege boundary.
        //
        // Only flip it -- and only claim ownership via the marker file --
        // if it wasn't already on. Otherwise uninstall() would turn off
        // lingering this user enabled for an unrelated reason.
        let already_lingering = linger_enabled().unwrap_or(false);
        if !already_lingering {
            let _ = Command::new("loginctl").arg("enable-linger").status();
            let _ = fs::write(linger_marker_path()?, "");
        }

        println!(
            "installed {} and {} lingering for this user",
            path.display(),
            if already_lingering {
                "left already-enabled"
            } else {
                "enabled"
            }
        );
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
        // Symmetric with install(): only disable lingering if the marker
        // says *this* install() was the one that turned it on. If the user
        // separately wanted lingering for an unrelated reason, it's untouched.
        let marker = linger_marker_path()?;
        let we_enabled_linger = marker.exists();
        if we_enabled_linger {
            let _ = Command::new("loginctl").arg("disable-linger").status();
            let _ = fs::remove_file(&marker);
        }

        println!(
            "removed {} and {} lingering for this user",
            path.display(),
            if we_enabled_linger {
                "disabled"
            } else {
                "left untouched (not enabled by this install)"
            }
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use linux::{install, uninstall};

#[cfg(not(target_os = "linux"))]
pub fn install() -> anyhow::Result<()> {
    anyhow::bail!(
        "`teleportd service install` is Linux-only for now -- on this platform, autostart \
         is only reachable from the desktop app's tray menu (\"Start at login\"), which \
         needs a real login session anyway (docs/08-packaging.md#autostart-at-login)"
    );
}

#[cfg(not(target_os = "linux"))]
pub fn uninstall() -> anyhow::Result<()> {
    anyhow::bail!(
        "`teleportd service uninstall` is Linux-only for now -- use the desktop app's \
         tray menu instead"
    );
}
