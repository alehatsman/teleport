//! Per-OS autostart-at-login registration
//! (docs/08-packaging.md#autostart-at-login).
//!
//! Hand-written per OS rather than a generic autostart plugin/crate: the
//! design doc calls for specific mechanisms -- a systemd **user** unit, a
//! `launchd` LaunchAgent (not a Login Item), a Task Scheduler **logon
//! trigger** -- chosen for properties a generic autostart helper doesn't
//! reliably guarantee (e.g. `KeepAlive` on crash for the LaunchAgent).
//! Autostart is user-scoped in every case; never a system-level service
//! (docs/06-security.md#privilege).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{install, uninstall};
#[cfg(target_os = "macos")]
pub use macos::{install, uninstall};
#[cfg(target_os = "windows")]
pub use windows::{install, uninstall};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn install() -> anyhow::Result<()> {
    anyhow::bail!("autostart is not implemented on this platform")
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn uninstall() -> anyhow::Result<()> {
    anyhow::bail!("autostart is not implemented on this platform")
}
