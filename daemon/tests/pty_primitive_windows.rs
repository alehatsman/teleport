//! Windows counterpart to the two exit-code fixtures in `pty_primitive.rs`
//! (`clean_exit_zero_is_recorded_via_wait_not_eof`,
//! `nonzero_exit_is_recorded_accurately`) -- those are Unix-only there and
//! were "expected to fail [on Windows] until W1 resolves, not attempted
//! there, tracked instead."
//!
//! W1 is now understood and has a fix in `pty.rs` (`ConptyDsrProbe`) -- see
//! [W1](../../docs/15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows).
//! This file exists to prove that fix through the real production code path
//! (`teleportd::pty::spawn`), not just the throwaway `spike/` binaries that
//! found it. It is deliberately narrow -- just the two fixtures the fix
//! targets -- not the full Windows pass of `pty_primitive.rs`'s suite
//! (raw-mode `stty`, SIGHUP/grandchild semantics, etc. are POSIX-shell
//! specific and need their own Windows-appropriate recipes; tracked as
//! remaining W2 work, not done here).

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use teleportd::pty::{self, SpawnSpec};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Spawns `cmd.exe /c exit N` under a real ConPTY via the production
/// `pty::spawn` path -- the same one `session.rs` uses, not a spike binary.
fn spawn_cmd_exit(code: i32) -> pty::SpawnedSession {
    let (out_tx, _out_rx) = mpsc::channel::<Vec<u8>>();
    let cwd = temp_dir();
    let args = vec!["/c".to_string(), format!("exit {code}")];
    let spec = SpawnSpec {
        program: "cmd.exe",
        args: &args,
        cwd: &cwd,
        env: &[],
        cols: 80,
        rows: 24,
    };
    pty::spawn(spec, move |chunk| {
        let _ = out_tx.send(chunk.to_vec());
    })
    .expect("spawn cmd.exe")
}

#[test]
fn clean_exit_zero_is_recorded_via_wait_not_eof() {
    let spawned = spawn_cmd_exit(0);
    let exit = spawned
        .exit_rx
        .recv_timeout(DEFAULT_TIMEOUT)
        .expect("exit_rx should fire -- this is exactly the case W1 found unobserved");
    let status = exit
        .status
        .expect("clean exit must carry a status, not a lost_reason");
    assert_eq!(status.exit_code(), 0);
    assert!(status.success());
    assert!(exit.lost_reason.is_none());
}

#[test]
fn nonzero_exit_is_recorded_accurately() {
    let spawned = spawn_cmd_exit(7);
    let exit = spawned
        .exit_rx
        .recv_timeout(DEFAULT_TIMEOUT)
        .expect("exit_rx should fire");
    let status = exit.status.expect("exit must carry a status");
    assert_eq!(status.exit_code(), 7);
    assert!(!status.success());
}
