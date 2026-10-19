//! The anchor client against a real `anchord`.
//!
//! Skipped unless `ANCHORD_BIN` points at an `anchord` binary: anchor is a
//! separate project with its own release cycle (that is the point of the
//! extraction), so teleport's suite cannot assume it is built here. Run it
//! with, e.g.:
//!
//! ```sh
//! ANCHORD_BIN=../anchor/target/debug/anchord cargo test --test anchor_client
//! ```

#![cfg(unix)]

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use support::{anchord_endpoint, fixture_shell};
use teleportd::anchor_client::{AnchorClient, AnchorFrame, AnchorSpec};

/// `None` when `ANCHORD_BIN` is unset, which skips every test here.
///
/// Set but not a file is a hard failure, not a skip: that combination means
/// someone meant to run these and pointed at the wrong path, and silently
/// reporting a pass for tests that never ran is worse than not having them.
fn anchord_bin() -> Option<PathBuf> {
    let raw = std::env::var_os("ANCHORD_BIN")?;
    let path = PathBuf::from(&raw);
    assert!(
        path.is_file(),
        "ANCHORD_BIN is set to {raw:?}, which is not a file"
    );
    Some(path)
}

/// The only test in this file that is not itself gated on `ANCHORD_BIN`.
/// See the twin in `anchor_http.rs` for why a skip here reports as a pass.
#[test]
fn ci_must_supply_an_anchord_binary() {
    if std::env::var_os("CI").is_some() {
        assert!(
            anchord_bin().is_some(),
            "ANCHORD_BIN is unset under CI: every anchor test would report ok without running"
        );
    }
}

/// One anchord per test, in its own directory and on its own endpoint.
struct Daemon {
    child: Child,
    dir: PathBuf,
    endpoint: PathBuf,
}

impl Daemon {
    fn start(tag: &str) -> Self {
        let tag = format!("{}-{tag}", std::process::id());
        let dir = anchord_endpoint::scratch_root().join(format!("tp-anchor-{tag}"));
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort teardown; nothing to do if it fails"
        )]
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).expect("creating the data dir");
        let endpoint = anchord_endpoint::endpoint_in(&dir, &tag);
        let bin = anchord_bin().expect("checked by the caller");
        let mut command = Command::new(bin);
        anchord_endpoint::point_at(&mut command, &dir, &endpoint)
            .expect("pointing anchord at its test directory");
        let child = command
            .env("ANCHOR_LOG", "warn")
            .spawn()
            .expect("spawning anchord");
        Self {
            child,
            dir,
            endpoint,
        }
    }

    fn socket(&self) -> PathBuf {
        self.endpoint.clone()
    }
}

impl Drop for Daemon {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort teardown in Drop; a failure here cannot be propagated"
    )]
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Waits until the daemon answers, so a test never races its startup.
async fn ready(client: &AnchorClient) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.list(&BTreeMap::new()).await.is_err() {
        assert!(Instant::now() < deadline, "anchord never came up");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn spec(script: &str) -> AnchorSpec {
    AnchorSpec {
        command: fixture_shell::PROGRAM.to_owned(),
        args: fixture_shell::args(script),
        cwd: std::env::temp_dir(),
        env: BTreeMap::new(),
        cols: 80,
        rows: 24,
        login_shell: false,
    }
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn control_ops_round_trip_against_a_real_anchord() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let daemon = Daemon::start("control");
    let client = AnchorClient::new(daemon.socket());
    ready(&client).await;

    let id = client
        .create(
            &spec(fixture_shell::pick(
                "stty raw -echo; echo hello-teleport; sleep 30",
                "Write-Output hello-teleport; Start-Sleep 30",
            )),
            &labels(&[("teleport.kind", "shell")]),
        )
        .await
        .expect("create");

    let session = client.get(&id).await.expect("get");
    assert_eq!(session.state, "running");
    assert_eq!(session.command, fixture_shell::PROGRAM);
    assert_eq!(
        session.labels.get("teleport.kind").map(String::as_str),
        Some("shell")
    );
    assert!(session.pid.is_some());

    // The label-filtered query is the capability teleport does not have
    // today — no side-table needed to find a session by what it is for.
    let found = client
        .list(&labels(&[("teleport.kind", "shell")]))
        .await
        .expect("list");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, id);
    assert!(client
        .list(&labels(&[("teleport.kind", "nope")]))
        .await
        .expect("list")
        .is_empty());

    // The log is readable by range, and carries the child's output.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let range = client.log(&id, 0, 65536).await.expect("log");
        if range.bytes.windows(14).any(|w| w == b"hello-teleport") {
            assert_eq!(range.from, 0);
            assert_eq!(range.to, range.bytes.len() as u64);
            break;
        }
        assert!(Instant::now() < deadline, "no output arrived");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    client.resize(&id, 132, 50).await.expect("resize");
    let updated = client
        .label(&id, &labels(&[("teleport.x", "1")]), &[])
        .await;
    assert_eq!(
        updated
            .expect("label")
            .get("teleport.x")
            .map(String::as_str),
        Some("1")
    );

    client.terminate(&id).await.expect("terminate");
    let after = client.get(&id).await.expect("get after terminate");
    assert_eq!(after.state, "exited");

    // An unknown session is an error carrying anchor's own code, so a
    // caller can map it onto teleport's 404 rather than parsing prose.
    let missing = client.get("01JNOPE0000000000000000000").await;
    let message = missing.expect_err("unknown session").to_string();
    assert!(message.starts_with("not_found:"), "got {message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn attach_replays_then_streams_and_reports_the_exit() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let daemon = Daemon::start("attach");
    let client = AnchorClient::new(daemon.socket());
    ready(&client).await;

    let id = client
        .create(
            &spec(fixture_shell::pick(
                "stty raw -echo; echo ready-for-input; cat; exit 3",
                // `cat`, by hand: echo each line back until input ends.
                "Write-Output ready-for-input; while ($null -ne ($l = [Console]::ReadLine())) { Write-Output $l }; exit 3",
            )),
            &BTreeMap::new(),
        )
        .await
        .expect("create");

    let mut attach = client.attach(&id, 0, true).await.expect("attach");
    assert!(attach.has_control(), "the lease was free");

    // Replay carries what the session produced before this attach existed.
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen.windows(15).any(|w| w == b"ready-for-input") {
        assert!(Instant::now() < deadline, "no replay arrived");
        match attach.next_frame().await.expect("a frame") {
            Some(AnchorFrame::Output { bytes, .. }) => seen.extend_from_slice(&bytes),
            Some(AnchorFrame::Lease { held }) => assert!(held),
            // The initial size, sent whether or not anyone resized.
            Some(AnchorFrame::Resized { cols, rows }) => {
                assert!(cols > 0 && rows > 0, "a zero-sized pty");
            }
            Some(AnchorFrame::Exit { .. }) | None => panic!("ended before replaying"),
        }
    }

    // Input reaches the PTY and comes back through the same stream.
    attach
        .send_input(fixture_shell::pick("echo round-trip\n", "echo round-trip\r").as_bytes())
        .await
        .expect("send_input");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !seen.windows(10).any(|w| w == b"round-trip") {
        assert!(Instant::now() < deadline, "input never echoed back");
        match attach.next_frame().await.expect("a frame") {
            Some(AnchorFrame::Output { bytes, .. }) => seen.extend_from_slice(&bytes),
            Some(_) => {}
            None => panic!("stream ended early"),
        }
    }

    // Ending `cat`'s input lets the script exit, and the exit arrives as a
    // frame carrying the code.
    client.terminate(&id).await.expect("terminate");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "no exit frame arrived");
        match attach.next_frame().await.expect("a frame") {
            Some(AnchorFrame::Exit { code: _ }) => {
                // The code's value depends on the signal that terminated it,
                // rather than the script's own `exit 3`. What matters is
                // that an exit is reported at all, exactly once, and that
                // the stream ends immediately after it.
                assert!(
                    attach.next_frame().await.expect("after exit").is_none(),
                    "exit must be the last frame"
                );
                break;
            }
            Some(_) => {}
            None => panic!("the stream ended without an exit frame"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_observer_attaches_without_taking_the_lease() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let daemon = Daemon::start("observe");
    let client = AnchorClient::new(daemon.socket());
    ready(&client).await;

    let id = client
        .create(
            &spec(fixture_shell::pick(
                "stty raw -echo; echo up; sleep 30",
                "Write-Output up; Start-Sleep 30",
            )),
            &BTreeMap::new(),
        )
        .await
        .expect("create");

    let controller = client.attach(&id, 0, true).await.expect("attach");
    assert!(controller.has_control());

    // Anchor never preempts: a second connection asking for control still
    // attaches, as an observer. Teleport's own lease layer sits above this.
    let observer = client.attach(&id, 0, true).await.expect("attach");
    assert!(
        !observer.has_control(),
        "attach must not preempt a held lease"
    );

    client.terminate(&id).await.expect("terminate");
}
