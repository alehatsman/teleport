//! PTY integration fixtures -- docs/10-testing.md#1-pty-integration-fixtures-daemontestspty_rs.
//!
//! Unix-focused: this is what could actually be run and verified in this
//! environment. Windows needs the same fixtures re-run on real hardware
//! (docs/10-testing.md#platform-matrix); the two exit-code fixtures are
//! expected to fail there until
//! [W1](../../docs/15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows)
//! resolves -- not attempted here, tracked instead.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use teleportd::pty::{self, SpawnSpec, TerminalSession};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Spawns `/bin/sh -c script` and returns the session handle plus its output
/// channel. `on_output` (docs/pty.rs's reader-thread callback) just forwards
/// chunks into an unbounded `mpsc` channel -- non-blocking `send`, satisfying
/// the "never block the reader" contract.
fn spawn_sh(script: &str, cols: u16, rows: u16) -> (pty::SpawnedSession, Receiver<Vec<u8>>) {
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let cwd = temp_dir();
    let spec = SpawnSpec {
        program: "/bin/sh",
        args: &["-c".to_string(), script.to_string()],
        cwd: &cwd,
        env: &[],
        cols,
        rows,
    };
    let spawned = pty::spawn(spec, move |chunk| {
        let _ = out_tx.send(chunk.to_vec());
    })
    .expect("spawn /bin/sh");
    (spawned, out_rx)
}

/// Spawns interactive `/bin/sh` (no `-c`) so the test can drive it with
/// multiple `write()`s, same output wiring as `spawn_sh`.
fn spawn_interactive_sh(cols: u16, rows: u16) -> (pty::SpawnedSession, Receiver<Vec<u8>>) {
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let cwd = temp_dir();
    let spec = SpawnSpec {
        program: "/bin/sh",
        args: &[],
        cwd: &cwd,
        env: &[],
        cols,
        rows,
    };
    let spawned = pty::spawn(spec, move |chunk| {
        let _ = out_tx.send(chunk.to_vec());
    })
    .expect("spawn /bin/sh");
    (spawned, out_rx)
}

/// Accumulates chunks from `rx` until `pred(&acc)` is true, or panics after
/// `timeout` with whatever was collected so far.
fn recv_until(rx: &Receiver<Vec<u8>>, timeout: Duration, mut pred: impl FnMut(&[u8]) -> bool) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut acc = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "timed out waiting for predicate; got {} bytes: {:?}",
                acc.len(),
                String::from_utf8_lossy(&acc)
            );
        }
        match rx.recv_timeout(remaining) {
            Ok(chunk) => {
                acc.extend_from_slice(&chunk);
                if pred(&acc) {
                    return acc;
                }
            }
            Err(_) => panic!(
                "output channel closed before predicate matched; got {} bytes: {:?}",
                acc.len(),
                String::from_utf8_lossy(&acc)
            ),
        }
    }
}

fn contains(acc: &[u8], needle: &str) -> bool {
    acc.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn echo_roundtrip() {
    let (mut spawned, out_rx) = spawn_interactive_sh(24, 80);
    spawned.session.write(b"echo hello\n").unwrap();
    recv_until(&out_rx, DEFAULT_TIMEOUT, |acc| contains(acc, "hello"));
}

#[test]
fn large_write_arrives_intact_and_in_order() {
    // Raw mode: a byte-exact payload can (and here, does) contain bytes that
    // cooked-mode termios treats as control characters (0x04 = EOF, 0x03 =
    // SIGINT, ...), not literal data -- that's line-discipline policy, not
    // pty.rs's job to work around (see the module doc and
    // docs/03-pty-layer.md#spawn). Raw mode also drops ONLCR, so no
    // \n -> \r\n surprises on the way back either. Same lesson the spike
    // already learned the hard way for S3
    // (docs/15-open-questions.md#s3--a-blocking-write-wedges-terminate).
    let (mut spawned, out_rx) = spawn_sh("stty raw -echo; cat", 24, 80);

    let payload: Vec<u8> = (0..1024 * 1024).map(|i| (i % 256) as u8).collect();
    spawned.session.write(&payload).unwrap();

    let got = recv_until(&out_rx, DEFAULT_TIMEOUT, |acc| acc.len() >= payload.len());
    assert_eq!(&got[..payload.len()], &payload[..], "cat's echo must match byte-for-byte, in order");
}

#[test]
fn large_burst_read_drops_nothing() {
    const N: usize = 8 * 1024 * 1024; // 8 MiB -- doc says 100 MiB from `yes`;
    // scaled down so the fixture stays fast without changing what it proves
    // (throughput without loss/stall). Content is `yes`'s repeating "y\n".
    // Raw mode so ONLCR doesn't inflate every "y\n" into "y\r\n" -- see the
    // note on large_write_arrives_intact_and_in_order above.
    let (_spawned, out_rx) = spawn_sh(&format!("stty raw -echo; yes | head -c {N}"), 24, 80);

    let got = recv_until(&out_rx, Duration::from_secs(30), |acc| acc.len() >= N);
    assert_eq!(got.len(), N, "must not drop or duplicate bytes");
    assert!(got.iter().all(|&b| b == b'y' || b == b'\n'), "content must be exactly yes's output, undamaged");
}

#[test]
fn resize_is_observed_by_the_child() {
    let (mut spawned, out_rx) = spawn_interactive_sh(24, 80);
    spawned.session.resize(120, 40).unwrap();
    // resize() hands off to the control thread and returns immediately
    // (docs/03-pty-layer.md#resize: "short and non-blocking") -- it carries
    // no ack, so give it a moment to land before asking the child to query
    // its size, rather than racing the control thread against the writer
    // thread on two independent channels.
    std::thread::sleep(Duration::from_millis(200));
    spawned.session.write(b"stty size\n").unwrap();
    let acc = recv_until(&out_rx, DEFAULT_TIMEOUT, |acc| contains(acc, "40 120"));
    assert!(contains(&acc, "40 120"), "stty size should report the resized rows/cols");
}

#[test]
fn clean_exit_zero_is_recorded_via_wait_not_eof() {
    let (spawned, _out_rx) = spawn_sh("exit 0", 24, 80);
    let exit = spawned.exit_rx.recv_timeout(DEFAULT_TIMEOUT).expect("exit_rx should fire");
    let status = exit.status.expect("clean exit must carry a status, not a lost_reason");
    assert_eq!(status.exit_code(), 0);
    assert!(status.success());
    assert!(exit.lost_reason.is_none());
}

#[test]
fn nonzero_exit_is_recorded_accurately() {
    let (spawned, _out_rx) = spawn_sh("exit 7", 24, 80);
    let exit = spawned.exit_rx.recv_timeout(DEFAULT_TIMEOUT).expect("exit_rx should fire");
    let status = exit.status.expect("exit must carry a status");
    assert_eq!(status.exit_code(), 7);
    assert!(!status.success());
}

#[test]
fn eof_and_exit_are_independent_signals() {
    // The direct child exits almost immediately; a detached grandchild
    // (ignoring SIGHUP, its own session) keeps the pty's slave side open for
    // a few seconds after. wait() must not be blocked on that, and EOF must
    // not arrive early just because the child already exited
    // (docs/15-open-questions.md#s2--eof-is-not-exit, S2's verified recipe).
    let (spawned, _out_rx) =
        spawn_sh("trap '' HUP; setsid sh -c 'trap \"\" HUP; sleep 2' & exit 0", 24, 80);

    let t0 = Instant::now();
    let exit = spawned.exit_rx.recv_timeout(DEFAULT_TIMEOUT).expect("exit_rx should fire promptly");
    let exit_latency = t0.elapsed();
    assert!(exit_latency < Duration::from_millis(500), "wait() should not wait on the grandchild: took {exit_latency:?}");
    assert_eq!(exit.status.unwrap().exit_code(), 0);

    // EOF should arrive later, once the grandchild's sleep ends and it exits too.
    match spawned.eof_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => {
            let eof_latency = t0.elapsed();
            assert!(eof_latency >= Duration::from_millis(1500), "EOF arrived suspiciously early: {eof_latency:?}");
        }
        Err(_) => panic!("EOF never arrived within 5s of the grandchild's sleep ending"),
    }
}

#[test]
fn terminate_reaches_exited_within_the_bounded_policy() {
    let (mut spawned, _out_rx) = spawn_sh("sleep 30", 24, 80);

    let t0 = Instant::now();
    spawned.session.terminate().expect("terminate should not error");
    let terminate_latency = t0.elapsed();

    // SIGTERM should kill a plain `sleep` near-instantly -- well under the
    // full 5s+2s bounded policy (docs/03-pty-layer.md#concrete-policy).
    assert!(terminate_latency < Duration::from_secs(2), "terminate() took {terminate_latency:?}, expected a fast SIGTERM kill");

    let exit = spawned.exit_rx.recv_timeout(Duration::from_secs(1)).expect("exit_rx should already have fired by the time terminate() returns");
    assert!(!exit.status.map(|s| s.success()).unwrap_or(false), "a signal-killed sleep should not report success");
}

#[test]
fn terminate_under_output_load_does_not_deadlock() {
    let (mut spawned, out_rx) = spawn_sh("yes", 24, 80);

    // Let it produce a real burst before terminating, so terminate() is
    // racing an active reader, not an idle one.
    recv_until(&out_rx, DEFAULT_TIMEOUT, |acc| acc.len() >= 256 * 1024);

    let t0 = Instant::now();
    spawned.session.terminate().expect("terminate should not error");
    assert!(t0.elapsed() < Duration::from_secs(8), "terminate() must not deadlock behind the writer/reader threads");

    spawned.exit_rx.recv_timeout(Duration::from_secs(1)).expect("exit_rx should fire once terminate() returns");

    // The reader thread should still be able to drain and reach EOF -- no
    // lost tail, no stuck reader.
    match spawned.eof_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => {}
        Err(_) => panic!("reader never reached EOF after terminate"),
    }
}

#[test]
fn terminate_kills_the_grandchild_process_tree() {
    let (mut spawned, out_rx) = spawn_sh("sleep 30 & echo GRANDCHILD_PID:$!; wait", 24, 80);

    let acc = recv_until(&out_rx, DEFAULT_TIMEOUT, |acc| contains(acc, "GRANDCHILD_PID:"));
    let text = String::from_utf8_lossy(&acc);
    let pid: i32 = text
        .split("GRANDCHILD_PID:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("should have parsed the grandchild pid from output");

    // SAFETY: kill(pid, 0) is a pure liveness probe, sends no signal.
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "grandchild should be alive before terminate");

    spawned.session.terminate().expect("terminate should not error");

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: same liveness probe.
        if unsafe { libc::kill(pid, 0) } == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return; // gone -- the whole tree was killed via killpg, not just the shell
        }
        if Instant::now() > deadline {
            panic!("grandchild pid {pid} was still alive {:?} after terminate", deadline.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
