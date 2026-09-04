//! **The D1 gate.** Attaching far behind a session that is still producing
//! must complete -- no `1013`, no gap, no duplicate --
//! docs/04-api-protocol.md#catch-up--register-late-not-early and
//! docs/10-testing.md#2-sessionoffset-unit-tests.
//!
//! The failure this guards against is a livelock, not a crash: register the
//! subscriber first and it spends its 8 MiB / 256-chunk queue buffering live
//! output for the whole duration of a replay it has not finished writing,
//! overflows, and is disconnected *before it ever goes live*. It then
//! reconnects further behind and fails again. An idle session never shows it.
//!
//! The convergence arithmetic is covered deterministically by the unit tests
//! in `session.rs`; this fixture is the end-to-end one, so it pays real time
//! to a real child.
//!
//! `cfg(unix)`-gated for the same reason as `session_replay.rs`: it drives a
//! real `/bin/sh`.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use teleportd::pty::SpawnSpec;
use teleportd::session::{ReplayStep, SessionManager};

const RECV_TIMEOUT: Duration = Duration::from_secs(10);
/// Backlog to attach behind: more than the 8 MiB queue bound, so a
/// pre-registered subscriber could not have held it even if it were idle.
const BACKLOG: u64 = 12 * 1024 * 1024;
/// What one catch-up round costs this "client". Twelve rounds of it is ~3 s,
/// during which the trickle below emits well past the 256-chunk half of the
/// bound -- which is what makes the old ordering fail here, and fail fast.
const ROUND_LATENCY: Duration = Duration::from_millis(400);

fn sessions_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "teleportd-catchup-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ))
}

/// A session in exactly the shape D1 is about: a large backlog already on
/// disk, and a child that is *still emitting* while the client catches up.
/// The trickle is deliberately many small writes rather than a few large
/// ones -- a slow consumer trips the count half of the queue bound long
/// before the byte half, and the count half is the cheaper one to reach in a
/// test.
fn spawn_backlog_then_trickle(manager: &SessionManager) -> std::sync::Arc<teleportd::session::Session> {
    let cwd = std::env::temp_dir();
    let args = vec![
        "-c".to_string(),
        format!("stty raw -echo; yes | head -c {BACKLOG}; while :; do printf 'tick\\n'; sleep 0.01; done"),
    ];
    manager
        .create(SpawnSpec { program: "/bin/sh", args: &args, cwd: &cwd, env: &[], cols: 80, rows: 24 })
        .expect("create session")
}

#[tokio::test]
async fn attaching_far_behind_a_producing_session_reaches_live() {
    let manager = SessionManager::new(sessions_root("d1"));
    let session = spawn_backlog_then_trickle(&manager);

    // Wait for the backlog to actually exist -- attaching before it does
    // would test nothing.
    let deadline = Instant::now() + RECV_TIMEOUT;
    while session.next_offset() < BACKLOG {
        assert!(Instant::now() < deadline, "the child never produced the backlog");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A subscriber registered *before* its replay is written -- the ordering
    // D1 rejects. Nothing reads it until catch-up is over, which is exactly
    // what a client on a slow link does to its own queue. It must not
    // survive; if it does, this fixture has stopped reproducing the failure
    // it exists to guard against, and the assertions below prove nothing.
    let registered_first = session.subscribe();

    let replay = session.attach(0).expect("attach at 0");
    assert_eq!(replay.replay_from, 0);
    let ready_next_offset = replay.next_offset;
    assert!(ready_next_offset >= BACKLOG);

    // Catch up the way a client on a slow link does: write each round out
    // before asking for the next.
    let mut acc: Vec<u8> = Vec::new();
    let mut rounds = 0u32;
    assert!(rounds < 64, "catch-up did not terminate");
    let mut step = replay.next_round().expect("first catch-up round");
    let attach = loop {
        match step {
            ReplayStep::History { offset, bytes, replay: rest } => {
                assert_eq!(offset, acc.len() as u64, "catch-up rounds must be contiguous");
                acc.extend_from_slice(&bytes);
                rounds += 1;
                tokio::time::sleep(ROUND_LATENCY).await; // the "network"
                assert!(rounds < 64, "catch-up did not terminate");
                step = rest.written(bytes).expect("catch-up round");
            }
            ReplayStep::Live(attach) => break attach,
        }
    };

    assert!(rounds > 8, "a {BACKLOG}-byte backlog must take several bounded rounds, got {rounds}");
    assert!(
        attach.caught_up,
        "a client outrunning the producer must converge, not be clamped"
    );
    assert_eq!(attach.replay_from, acc.len() as u64, "the final stretch must continue the rounds");
    acc.extend_from_slice(&attach.replay);
    let mut end = attach.replay_to();
    assert_eq!(end, acc.len() as u64);
    assert!(end >= ready_next_offset, "catch-up must reach at least the boundary `ready` announced");

    // The whole point: this subscriber is still connected, and its first
    // chunk continues the replay exactly.
    let mut subscription = attach.subscription;
    for i in 0..8 {
        let chunk = tokio::time::timeout(RECV_TIMEOUT, subscription.recv())
            .await
            .unwrap_or_else(|_| panic!("live chunk {i} never arrived"))
            .unwrap_or_else(|| panic!("subscriber was disconnected after catch-up -- D1 has regressed"));
        assert_eq!(chunk.offset, end, "live output must continue exactly where replay stopped");
        acc.extend_from_slice(&chunk.bytes);
        end += chunk.bytes.len() as u64;
    }

    // The control: the up-front subscriber blew its queue during the same
    // catch-up window this attach walked through unharmed.
    let old_ordering_died = tokio::time::timeout(RECV_TIMEOUT, async {
        let mut sub = registered_first;
        while sub.recv().await.is_some() {}
    })
    .await;
    assert!(
        old_ordering_died.is_ok(),
        "the pre-registered subscriber survived the catch-up window, so this fixture is no \
         longer reproducing D1 -- make the child noisier or the rounds slower before trusting it"
    );

    let on_disk = std::fs::read(session.log_path()).expect("read output.vt");
    assert!(on_disk.len() as u64 >= end);
    assert_eq!(
        acc,
        on_disk[..acc.len()],
        "replay + live must equal the log byte for byte -- no gap, no duplicate"
    );

    session.terminate().expect("terminate");
}
