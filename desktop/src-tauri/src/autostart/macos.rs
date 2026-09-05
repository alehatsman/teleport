//! `launchd` LaunchAgent (docs/08-packaging.md#autostart-at-login).
//! `~/Library/LaunchAgents/io.github.alehatsman.teleport.teleportd.plist`,
//! `RunAtLoad=true`, `KeepAlive` on crash -- not a Login Item, which doesn't
//! get `KeepAlive` semantics.

use std::fs;

use anyhow::{Context, Result};

const LABEL: &str = "io.github.alehatsman.teleport.teleportd";

fn agents_dir() -> Result<std::path::PathBuf> {
    let base = directories::BaseDirs::new().context("no home directory")?;
    Ok(base.home_dir().join("Library/LaunchAgents"))
}

fn plist_path() -> Result<std::path::PathBuf> {
    Ok(agents_dir()?.join(format!("{LABEL}.plist")))
}

pub fn install() -> Result<()> {
    let exe = crate::daemon::sidecar_path()?;
    let dir = agents_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>Crashed</key>
        <true/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        exe = exe.display()
    );
    let path = plist_path()?;
    fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;

    let _ = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&path)
        .status();
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = plist_path()?;
    if path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&path)
            .status();
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}
