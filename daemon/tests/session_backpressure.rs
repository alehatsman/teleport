//! Session ownership and backpressure --
//! docs/11-mvp-plan.md#m2--session-ownership-and-backpressure, the M2 subset
//! of docs/10-testing.md#2-sessionoffset-unit-tests (the rest of that section
//! needs `log.rs`/M3 and is not in scope here).

#![cfg(unix)]

mod support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use teleportd::pty::SpawnSpec;
use teleportd::session::SessionManager;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// A throwaway `<data_dir>/sessions` root. Each session writes its
/// `output.vt` under here (docs/05-persistence.md#layout); these tests only
/// care that the log exists somewhere private to them.
fn sessions_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "teleportd-sessions-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_nanos()
    ))
}

fn spec<'a>(args: &'a [String], cols: u16, rows: u16, cwd: &'a PathBuf) -> SpawnSpec<'a> {
    SpawnSpec {
        program: "/bin/sh",
        args,
        cwd,
        env: &[],
        cols,
        rows,
        login_shell: false,
    }
}

/// A session with zero subscribers is not a special case that needs to be
/// avoided -- it survives indefinitely (docs/11-mvp-plan.md#m2). Create one,
/// drive it through write/resize/terminate with nobody ever subscribing.
#[tokio::test]
async fn zero_subscribers_session_survives() {
    let manager = SessionManager::new(sessions_root("zero-subs"));
    let cwd = temp_dir();
    let args = vec![];
    let session = manager
        .create(&spec(&args, 80, 24, &cwd), "shell", None)
        .expect("create session");

    session
        .write(b"echo hi\n")
        .expect("write with no subscriber");
    session.resize(40, 120).expect("resize with no subscriber");
    session.terminate().expect("terminate with no subscriber");
}

/// S3 one layer up (docs/03-pty-layer.md#the-terminalsession-trait): a
/// `Session` must not serialize `write`/`resize`/`terminate` behind one lock.
/// A child that never reads its pty eventually blocks a writer thread that
/// keeps calling `write()`; `terminate()` must still return within its
/// bounded policy (~7s) instead of queueing behind that stuck writer.
#[tokio::test]
async fn terminate_is_not_wedged_by_a_stuck_write() {
    let manager = SessionManager::new(sessions_root("stuck-write"));
    let cwd = temp_dir();
    let args = vec!["-c".to_string(), "sleep 30".to_string()];
    let session = manager
        .create(&spec(&args, 80, 24, &cwd), "shell", None)
        .expect("create session");

    // **Complete lines, and this is the whole fixture.** A tty in canonical
    // mode discards an over-long line that never terminates rather than
    // queueing it, so 4 KiB blobs of `x` are swallowed forever and no writer
    // ever blocks -- this test spent its whole life green while terminate()
    // raced nothing at all. Anchor found it in their copy and measured it:
    // a non-reading child took 8.5 MB of blob writes without stalling, and
    // wedged permanently after 1,022 bytes of complete lines (anchor#9).
    let writes = Arc::new(AtomicU64::new(0));
    let gave_up = Arc::new(AtomicBool::new(false));
    let writer_session = Arc::clone(&session);
    let writer_writes = Arc::clone(&writes);
    let writer_gave_up = Arc::clone(&gave_up);
    std::thread::spawn(move || {
        let mut line = vec![b'x'; 255];
        line.push(b'\n');
        // `write` enqueues onto a bounded channel, so once the writer thread
        // is stuck in `write_all` this blocks rather than returning an
        // error. Progress stopping is therefore the signal, not a rejection.
        while writer_session.write(&line).is_ok() {
            writer_writes.fetch_add(1, Ordering::Relaxed);
        }
        // A writer that *returned* also stops the counter, and would look
        // exactly like a wedge to the wait below. Say which happened.
        writer_gave_up.store(true, Ordering::Relaxed);
    });

    // Wait for the writer to be *wedged*, not merely slow. One sample with
    // no progress proves nothing -- a busy machine produces those -- so this
    // waits for an unbroken streak, reset by any write getting through.
    let mut last = 0;
    let mut stalled = 0;
    let deadline = Instant::now() + Duration::from_secs(30);
    while stalled < 20 {
        assert!(
            Instant::now() < deadline,
            "the writer never wedged: {} writes and still moving",
            writes.load(Ordering::Relaxed)
        );
        std::thread::sleep(Duration::from_millis(25));
        assert!(
            !gave_up.load(Ordering::Relaxed),
            "the writer stopped by erroring, not by wedging -- the premise never held"
        );
        let now = writes.load(Ordering::Relaxed);
        if now == last {
            stalled += 1;
        } else {
            stalled = 0;
            last = now;
        }
    }

    println!(
        "wedged after {} complete lines ({} bytes)",
        writes.load(Ordering::Relaxed),
        writes.load(Ordering::Relaxed) * 256
    );

    let t0 = Instant::now();
    session.terminate().expect("terminate should not error");
    assert!(
        t0.elapsed() < Duration::from_secs(9),
        "terminate() must not be wedged behind a stuck write"
    );
}

/// **Changed in M4** (docs/04-api-protocol.md#delete-apiv1sessionsid):
/// `terminate()` alone must leave the session resolvable and listed as
/// `exited` -- only an explicit `?purge=true` (`SessionManager::purge`)
/// removes it. The M2-era version of this test asserted the opposite
/// (`terminate()` self-removing); that broke the "stays in the list as
/// exited" contract M4's API needs, so the behavior and this test both
/// changed together.
#[tokio::test]
async fn terminate_leaves_the_session_listed_until_purged() {
    let manager = SessionManager::new(sessions_root("terminate-leaves"));
    let cwd = temp_dir();
    let args = vec![];
    let session = manager
        .create(&spec(&args, 80, 24, &cwd), "shell", None)
        .expect("create session");
    let id = session.id;

    session.terminate().expect("terminate should not error");
    assert!(
        manager.get(id).is_some(),
        "a terminated session must stay listed until purged"
    );

    // `terminate()` returning only guarantees the state left `running`; the
    // final `-> exited` transition is made by the exit listener thread
    // reacting to the same `wait()` result, asynchronously
    // (docs/03-pty-layer.md#state-machine), so give it a moment.
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        if session.state() == teleportd::session::SessionState::Exited {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "session never reached exited after terminate()"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        manager.purge(id).is_some(),
        "purge must return the session it removed"
    );
    assert!(
        manager.get(id).is_none(),
        "purged session must be gone from the manager"
    );
}

/// M4 review: `max_sessions` used to be checked and released under the lock,
/// then re-acquired to insert -- concurrent creates could all pass the check
/// before any of them inserted, overshooting the cap. `create()` itself does
/// real, non-trivial work between check and insert (validating `cwd`,
/// resolving the executable, forking the child), which is exactly the window
/// real concurrent callers would race in.
#[test]
fn concurrent_creates_never_exceed_max_sessions() {
    let manager =
        Arc::new(SessionManager::new(sessions_root("max-sessions-race")).with_max_sessions(3));
    let cwd = temp_dir();

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let manager = Arc::clone(&manager);
            let cwd = cwd.clone();
            std::thread::spawn(move || {
                let args = vec!["-c".to_string(), "sleep 2".to_string()];
                manager.create(&spec(&args, 80, 24, &cwd), "shell", None)
            })
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("creator thread panicked"))
        .collect();
    let succeeded: Vec<_> = results.into_iter().filter_map(Result::ok).collect();
    assert_eq!(
        succeeded.len(),
        3,
        "at most max_sessions creates may succeed, even racing"
    );

    for session in succeeded {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort teardown; the process is exiting either way"
        )]
        let _ = session.terminate();
    }
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
    let manager = SessionManager::new(sessions_root("in-order"));
    let cwd = temp_dir();
    let args = vec![];
    let session = manager
        .create(&spec(&args, 80, 24, &cwd), "shell", None)
        .expect("create session");

    let mut sub = session.subscribe();
    session.write(b"printf 'hello world'\n").expect("write");

    // Cooked-mode echo means `acc` also carries the echoed input line, so
    // check for content with `contains` rather than an exact match.
    let mut acc = Vec::new();
    let mut next_expected_offset: Option<u64> = None;
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while !contains(&acc, "hello world") {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "timed out; got {:?}",
            String::from_utf8_lossy(&acc)
        );
        let chunk = tokio::time::timeout(remaining, sub.recv())
            .await
            .expect("recv timed out")
            .expect("subscriber disconnected before all output arrived");
        if let Some(expected) = next_expected_offset {
            assert_eq!(
                chunk.offset, expected,
                "offsets must be contiguous, no gap or overlap"
            );
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
///
/// Was gated off macOS, where this fixture's *fast* subscriber was
/// disconnected too, 3 times in 20 runs on real hardware. That was
/// [N5](../../docs/15-open-questions.md#n5--macos-pty-reads-average-14-bytes-starving-the-queue-bounds-count-half):
/// the queue bound was `min(256 chunks, 8 MiB)`, and a macOS pty read
/// averages **14 bytes** (max 1024, the tty output buffer), so 256 chunks
/// held ~3.5 KiB -- roughly 2300x tighter than the byte half was designed to
/// allow, and enough to drop any subscriber that stalled even briefly. Linux
/// ptys coalesce far more, so the byte half governed there and the bound
/// behaved as documented.
///
/// Un-gated once the bound became a single byte budget that charges each
/// chunk a fixed overhead alongside its payload, which is what the count half
/// was really protecting. `fanout.rs`'s
/// `tiny_chunks_get_the_budget_not_a_slot_count` covers the same regression
/// deterministically and without a real pty.
#[tokio::test]
async fn slow_subscriber_is_disconnected_and_never_blocks_the_reader() {
    const N: usize = 10 * 1024 * 1024; // 10 MiB, comfortably past the 8 MiB bound.
    let manager = SessionManager::new(sessions_root("slow-sub"));
    let cwd = temp_dir();
    let args = vec![
        "-c".to_string(),
        format!("stty raw -echo; yes | head -c {N}"),
    ];
    let session = manager
        .create(&spec(&args, 80, 24, &cwd), "shell", None)
        .expect("create session");

    let slow = session.subscribe(); // never read from this one.

    // `attach(0)`, not `subscribe()`. The child is already producing by the
    // time `create` returns, so a plain subscriber silently misses whatever
    // landed before it registered -- tens of KiB under load -- and can never
    // account for all N bytes, which reads as a stall that looks exactly like
    // the backpressure failure this test is trying to detect. `attach` counts
    // the replay too and closes that race
    // (docs/04-api-protocol.md#attach-race).
    let replay = session.attach(0).expect("attach at 0");
    let (received_bytes, mut fast, _) = support::catch_up(replay, Duration::ZERO).await;
    let mut received = received_bytes.len();

    // The invariant, measured by the thing that actually measures it: the
    // fast subscriber never goes more than `DEFAULT_TIMEOUT` without a
    // chunk. A reader blocked behind the never-reading subscriber shows up
    // here, as a stall, immediately.
    //
    // There used to be a total-elapsed assertion underneath this loop --
    // "10 MiB in under 10s or the reader looks backpressured". It measured
    // how fast this machine moves 10 MiB through a pty, which is not the
    // claim: on a loaded box it failed at 54s having received every byte,
    // in order, with no stall longer than a blink (#68). Slow is not
    // blocked.
    while received < N {
        let chunk = tokio::time::timeout(DEFAULT_TIMEOUT, fast.subscription.recv())
            .await
            .unwrap_or_else(|_| {
                panic!("fast subscriber stalled after {received}/{N} bytes -- reader was blocked by the slow one")
            })
            .expect("fast subscriber disconnected before receiving all output");
        received += chunk.bytes.len();
    }

    // The never-reading subscriber must have been dropped once it exceeded
    // the bound, not silently buffered -- that's what keeps memory flat.
    let disconnected = tokio::time::timeout(DEFAULT_TIMEOUT, async {
        let mut slow = slow;
        while slow.recv().await.is_some() {}
    })
    .await;
    assert!(
        disconnected.is_ok(),
        "slow subscriber was never disconnected -- unbounded buffering"
    );
}
