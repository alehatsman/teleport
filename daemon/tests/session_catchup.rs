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
//! `cfg(unix)`, for the ordinary reason: it drives a real `/bin/sh`.
//!
//! It was `target_os = "linux"`-gated on 2026-09-05 because its own guard
//! fired on `macos-latest` -- the pre-registered subscriber survived the
//! catch-up window, so the fixture was no longer reproducing D1. Measured on
//! real macOS hardware 2026-09-05 (#25): the cause was the trickle's rate,
//! not the platform. `sleep 0.01` nominally ticks 100x/s, but each iteration
//! forks `sleep`, and on macOS fork+exec is expensive enough to hold the loop
//! to ~31 iterations/s -- **51 chunks/s**, needing 5.0 s to reach the 256-chunk
//! bound against a catch-up window of only ~4.8 s. The fixture sat directly on
//! that boundary and failed about half of 20 runs.
//!
//! The trickle now emits `TICKS_PER_SLEEP` writes per fork, which decouples
//! the rate from fork cost. The rate must stay inside a band, and both edges
//! are real failures rather than flakes:
//!
//! * **> ~53 chunks/s** -- or `registered_first` never overflows its
//!   256-chunk queue inside the catch-up window, and the guard below fires
//!   because nothing is being reproduced.
//! * **< ~640 chunks/s** -- or the *attaching* subscriber, which is not
//!   draining for `ROUND_LATENCY` at a time, overflows the same bound during
//!   a single round and is disconnected mid-catch-up, failing the test for a
//!   reason D1 is not about.
//!
//! Measured 112 chunks/s on macOS (256 chunks in 2.3 s, comfortably inside the
//! window). Linux forks more cheaply, so it sits higher in the band -- at the
//! nominal 100 iterations/s it is ~400 chunks/s, still under the ceiling, and
//! Linux ptys coalesce small writes far more aggressively than macOS's do
//! (see #25: macOS pty reads average 14 bytes), which pushes it lower still.
//! If this fixture needs re-tuning again, re-measure the chunk rate against
//! that band -- do not adjust the cadence blind.

#![cfg(unix)]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use teleportd::pty::SpawnSpec;
use teleportd::session::SessionManager;

const RECV_TIMEOUT: Duration = Duration::from_secs(10);
/// Backlog to attach behind: more than the 8 MiB queue bound, so a
/// pre-registered subscriber could not have held it even if it were idle.
const BACKLOG: u64 = 12 * 1024 * 1024;
/// What one catch-up round costs this "client". Twelve rounds of it is ~3 s,
/// during which the trickle below emits well past the 256-chunk half of the
/// bound -- which is what makes the old ordering fail here, and fail fast.
const ROUND_LATENCY: Duration = Duration::from_millis(400);
/// Writes the trickle emits between forks of `sleep`. Keeps the live chunk
/// rate inside the band documented in the module doc on both platforms,
/// instead of inheriting whatever `fork` happens to cost.
const TICKS_PER_SLEEP: u32 = 4;

fn sessions_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "teleportd-catchup-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// A session in exactly the shape D1 is about: a large backlog already on
/// disk, and a child that is *still emitting* while the client catches up.
/// The trickle is deliberately many small writes rather than a few large
/// ones -- a slow consumer trips the count half of the queue bound long
/// before the byte half, and the count half is the cheaper one to reach in a
/// test. `TICKS_PER_SLEEP` writes go out per fork so the rate is set by the
/// batch rather than by what `fork` costs on this OS; see the module doc for
/// the band it has to stay inside.
fn spawn_backlog_then_trickle(
    manager: &SessionManager,
) -> std::sync::Arc<teleportd::session::Session> {
    let cwd = std::env::temp_dir();
    let args = vec![
        "-c".to_string(),
        format!(
            "stty raw -echo; yes | head -c {BACKLOG}; \
             while :; do i=0; \
             while [ $i -lt {TICKS_PER_SLEEP} ]; do printf 'tick\\n'; i=$((i+1)); done; \
             sleep 0.01; done"
        ),
    ];
    manager
        .create(
            SpawnSpec {
                program: "/bin/sh",
                args: &args,
                cwd: &cwd,
                env: &[],
                cols: 80,
                rows: 24,
            },
            "shell",
            None,
        )
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
        assert!(
            Instant::now() < deadline,
            "the child never produced the backlog"
        );
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
    // before asking for the next. `ROUND_LATENCY` is the "network" -- slow
    // enough that the trickle above outpaces the count half of the queue
    // bound during the walk, which is what makes the old ordering fail here.
    let (mut acc, attach, rounds) = support::catch_up(replay, ROUND_LATENCY).await;

    assert!(
        rounds > 8,
        "a {BACKLOG}-byte backlog must take several bounded rounds, got {rounds}"
    );
    let mut end = attach.replay_to();
    assert_eq!(
        end,
        acc.len() as u64,
        "replay rounds plus the final stretch must equal every byte served"
    );
    assert!(
        end >= ready_next_offset,
        "catch-up must reach at least the boundary `ready` announced"
    );

    // The whole point: this subscriber is still connected, and its first
    // chunk continues the replay exactly.
    let mut subscription = attach.subscription;
    for i in 0..8 {
        let chunk = tokio::time::timeout(RECV_TIMEOUT, subscription.recv())
            .await
            .unwrap_or_else(|_| panic!("live chunk {i} never arrived"))
            .unwrap_or_else(|| {
                panic!("subscriber was disconnected after catch-up -- D1 has regressed")
            });
        assert_eq!(
            chunk.offset, end,
            "live output must continue exactly where replay stopped"
        );
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
