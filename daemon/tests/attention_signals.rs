//! D3 attention signals --
//! formerly docs/15-open-questions.md's D3;
//! docs/13-native-clients.md#detection-heuristics, closed out as part of
//! docs/11-mvp-plan.md#m8--agent-presets.

#![cfg(unix)]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use teleportd::pty::SpawnSpec;
use teleportd::session::SessionManager;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

fn sessions_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "teleportd-attention-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn spec<'a>(args: &'a [String], cwd: &'a PathBuf) -> SpawnSpec<'a> {
    SpawnSpec {
        program: "/bin/sh",
        args,
        cwd,
        env: &[],
        cols: 80,
        rows: 24,
    }
}

/// The daemon's own `now_ms()` is private to `main.rs`; tests use
/// `SystemTime` directly, the same way `support/mod.rs`'s directory-naming
/// helpers already do.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A BEL byte (`\x07`) anywhere in the output stream is detected inline in
/// the reader-loop closure (`session.rs::create`) and surfaced on
/// `Session::last_bell_ms`, with no dependency on the idle-sweep task.
#[tokio::test]
async fn a_bel_byte_in_the_output_sets_last_bell_ms() {
    let manager = SessionManager::new(sessions_root("bell"));
    let cwd = temp_dir();
    let args = vec![];
    let session = manager
        .create(spec(&args, &cwd), "shell", None)
        .expect("create session");

    assert_eq!(session.last_bell_ms(), None, "no bell has happened yet");

    session.write(b"printf '\\007'\n").expect("write");

    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while session.last_bell_ms().is_none() {
        assert!(Instant::now() < deadline, "timed out waiting for the bell");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `tick_idle` is the whole detector -- no PTY-side plumbing decides
/// idleness, a caller does, by comparing `now` against the last time output
/// was seen. Driven directly with synthetic clocks and a tiny threshold so
/// this doesn't need to wait out the real (30s) default
/// (`session::IDLE_THRESHOLD_MS`).
#[tokio::test]
async fn tick_idle_sets_and_clears_idle_since_ms() {
    let manager = SessionManager::new(sessions_root("idle"));
    let cwd = temp_dir();
    // `sleep` rather than a bare interactive shell: it produces no output at
    // all, so `last_output_at_ms` never moves out from under the assertions
    // below on its own.
    let args = vec!["-c".to_string(), "sleep 5".to_string()];
    let session = manager
        .create(spec(&args, &cwd), "shell", None)
        .expect("create session");

    let created_at = session.created_at_ms();
    assert_eq!(session.idle_since_ms(), None, "fresh session is not idle");

    // Threshold crossed: idle_since_ms should latch to the last time output
    // was seen (session creation, here), not to `now`.
    session.tick_idle(created_at + 100, 50);
    assert_eq!(
        session.idle_since_ms(),
        Some(created_at),
        "idle_since_ms should read the last-output timestamp, not the tick time"
    );

    // Still under threshold on a second tick: stays idle, doesn't flap.
    session.tick_idle(created_at + 120, 50);
    assert_eq!(session.idle_since_ms(), Some(created_at));

    session.write(b"echo hi\n").expect("write");
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while session.next_offset() == 0 {
        assert!(Instant::now() < deadline, "timed out waiting for output");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Output arrived after the idle latch -- a tick with `now` close to the
    // real clock must see last_output_at_ms move and clear idle_since_ms.
    session.tick_idle(now_ms(), 50);
    assert_eq!(
        session.idle_since_ms(),
        None,
        "output resuming must clear idle_since_ms"
    );
}

/// A session that has already exited is not "waiting for you" -- `tick_idle`
/// must not latch idle state onto a dead session no one will ever see move
/// again.
#[tokio::test]
async fn tick_idle_is_a_no_op_once_the_session_has_exited() {
    let manager = SessionManager::new(sessions_root("idle-exited"));
    let cwd = temp_dir();
    let args = vec!["-c".to_string(), "true".to_string()];
    let session = manager
        .create(spec(&args, &cwd), "shell", None)
        .expect("create session");

    session.exited().await;
    let created_at = session.created_at_ms();

    session.tick_idle(created_at + 100_000, 50);
    assert_eq!(
        session.idle_since_ms(),
        None,
        "an exited session must never be reported as idle"
    );
}
