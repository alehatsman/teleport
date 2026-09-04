//! Session ownership and backpressure --
//! docs/11-mvp-plan.md#m2--session-ownership-and-backpressure, the M2 subset
//! of docs/10-testing.md#2-sessionoffset-unit-tests (the rest of that section
//! needs `log.rs`/M3 and is not in scope here).

#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use teleportd::pty::SpawnSpec;
use teleportd::session::SessionManager;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

fn spec<'a>(args: &'a [String], cols: u16, rows: u16, cwd: &'a PathBuf) -> SpawnSpec<'a> {
    SpawnSpec { program: "/bin/sh", args, cwd, env: &[], cols, rows }
}

/// A session with zero subscribers is not a special case that needs to be
/// avoided -- it survives indefinitely (docs/11-mvp-plan.md#m2). Create one,
/// drive it through write/resize/terminate with nobody ever subscribing.
#[tokio::test]
async fn zero_subscribers_session_survives() {
    let manager = SessionManager::new();
    let cwd = temp_dir();
    let args = vec![];
    let session = manager.create(spec(&args, 24, 80, &cwd)).expect("create session");

    session.write(b"echo hi\n").expect("write with no subscriber");
    session.resize(40, 120).expect("resize with no subscriber");
    session.terminate().expect("terminate with no subscriber");
}

/// A subscriber that reads gets exactly what the session wrote, in order,
/// with monotonically increasing, non-overlapping offsets between
/// consecutive chunks.
///
/// Uses an interactive shell and subscribes *before* writing the command
/// that produces the output under test -- with a `-c "printf ..."` spec the
/// child can run and exit before `subscribe()` gets scheduled, and a
/// subscriber that registers after the fact sees nothing (no replay exists
/// yet; that's M3/M4, not this module's job). Even so, the shell's own
/// spawn-time chatter can land before `subscribe()`'s lock is acquired, so
/// this checks contiguity chunk-to-chunk, not that the first one starts at
/// offset 0 -- what happened before you subscribed is exactly the boundary
/// M2 doesn't promise anything about.
#[tokio::test]
async fn subscriber_receives_output_in_order() {
    let manager = SessionManager::new();
    let cwd = temp_dir();
    let args = vec![];
    let session = manager.create(spec(&args, 24, 80, &cwd)).expect("create session");

    let mut sub = session.subscribe();
    session.write(b"printf 'hello world'\n").expect("write");

    // Cooked-mode echo means `acc` also carries the echoed input line, so
    // check for content with `contains` rather than an exact match.
    let mut acc = Vec::new();
    let mut next_expected_offset: Option<u64> = None;
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while !contains(&acc, "hello world") {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out; got {:?}", String::from_utf8_lossy(&acc));
        let chunk = tokio::time::timeout(remaining, sub.recv())
            .await
            .expect("recv timed out")
            .expect("subscriber disconnected before all output arrived");
        if let Some(expected) = next_expected_offset {
            assert_eq!(chunk.offset, expected, "offsets must be contiguous, no gap or overlap");
        }
        next_expected_offset = Some(chunk.offset + chunk.bytes.len() as u64);
        acc.extend_from_slice(&chunk.bytes);
    }
}

fn contains(acc: &[u8], needle: &str) -> bool {
    acc.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// The M2 gate itself: a subscriber that never reads must not slow the PTY
/// drain. A second, actively-draining subscriber on the same session proves
/// the reader thread kept moving; the never-reading one must eventually be
/// disconnected (bounded queue, docs/03-pty-layer.md#backpressure) instead
/// of accumulating unboundedly.
#[tokio::test]
async fn slow_subscriber_is_disconnected_and_never_blocks_the_reader() {
    const N: usize = 10 * 1024 * 1024; // 10 MiB, comfortably past the 8 MiB bound.
    let manager = SessionManager::new();
    let cwd = temp_dir();
    let args = vec!["-c".to_string(), format!("stty raw -echo; yes | head -c {N}")];
    let session = manager.create(spec(&args, 24, 80, &cwd)).expect("create session");

    let slow = session.subscribe(); // never read from this one.
    let mut fast = session.subscribe();

    let t0 = Instant::now();
    let mut received = 0usize;
    while received < N {
        let chunk = tokio::time::timeout(DEFAULT_TIMEOUT, fast.recv())
            .await
            .unwrap_or_else(|_| panic!("fast subscriber stalled after {received}/{N} bytes -- reader was blocked by the slow one"))
            .expect("fast subscriber disconnected before receiving all output");
        received += chunk.bytes.len();
    }
    let elapsed = t0.elapsed();
    assert!(elapsed < Duration::from_secs(10), "drain of {N} bytes took {elapsed:?}, reader looks backpressured");

    // The never-reading subscriber must have been dropped once it exceeded
    // the bound, not silently buffered -- that's what keeps memory flat.
    let disconnected = tokio::time::timeout(DEFAULT_TIMEOUT, async {
        let mut slow = slow;
        while slow.recv().await.is_some() {}
    })
    .await;
    assert!(disconnected.is_ok(), "slow subscriber was never disconnected -- unbounded buffering");
}
