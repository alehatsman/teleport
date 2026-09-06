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
    use std::path::PathBuf;

    use anyhow::{Context, Result};

    fn unit_dir() -> Result<PathBuf> {
        let base = directories::BaseDirs::new().context("no home directory")?;
        Ok(base.home_dir().join(".config/systemd/user"))
    }

    fn unit_path() -> Result<PathBuf> {
        Ok(unit_dir()?.join("teleportd.service"))
    }

    pub fn install() -> Result<()> {
        let exe = std::env::current_exe().context("resolving this binary's own path")?;
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

        // Best-effort, like desktop's copy of this: don't fail install() if
        // there's no active systemd --user session (docs/11-mvp-plan.md#m10
        // edge cases) -- autostart is a convenience, not a launch dependency.
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        let _ = std::process::Command::new("systemctl")
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
        let _ = std::process::Command::new("loginctl")
            .arg("enable-linger")
            .status();

        println!(
            "installed {} and enabled lingering for this user",
            path.display()
        );
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
        // Symmetric with install(). Known limitation, not silently assumed
        // fine: if this user separately wanted lingering for an unrelated
        // reason, uninstalling teleport's autostart also turns it off --
        // there is no per-unit lingering flag to scope this to, only a
        // per-user one.
        let _ = std::process::Command::new("loginctl")
            .arg("disable-linger")
            .status();

        println!(
            "removed {} and disabled lingering for this user",
            path.display()
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
