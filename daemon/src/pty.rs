//! What is left of teleport's PTY layer: one function.
//!
//! teleport spawns no PTY of its own. Every session lives in `anchord`
//! (docs/20-retiring-the-in-process-pty.md), and the in-process layer that
//! was here -- `portable-pty`, the reader/writer/reaper/control threads, the
//! bounded terminate policy -- went with it.
//!
//! **`ConptyDsrProbe` and `tests/pty_primitive_windows.rs` were transferred,
//! not discarded.** The probe is the fix for #W1 (conhost's `ConPTY` startup
//! handshake blocks on an `ESC[6n` nobody answers, so a child that exits is
//! never observed as exited), and that knowledge does not survive reading
//! the code that remains. It lives in anchor's `src/pty.rs` now, under test
//! in anchor's `tests/pty_primitive_windows.rs` and tripwired so a run that
//! skips those fixtures is red (anchor `e1c51d0`, `80d2e98`; docs/20,
//! Constraint 3). The last teleport copy is in this file's history.

/// The user's login shell, for the `"$SHELL"` placeholder `presets.toml` may
/// use as a command (`presets::Preset::resolved_command`). One resolution,
/// one fallback, one place.
#[cfg(unix)]
pub fn login_shell_path() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// Windows has no login shell; this is the interpreter `"$SHELL"` resolves to
/// there. A session's `login_shell` flag is anchord's to honour or ignore,
/// and it ignores it on Windows, as teleport did.
#[cfg(windows)]
pub fn login_shell_path() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
}
