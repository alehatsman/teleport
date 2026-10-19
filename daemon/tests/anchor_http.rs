//! Teleport's own HTTP surface, served by an external `anchord`.
//!
//! This is the shape of anchor's M4 acceptance: the same routes teleport's
//! clients already call, answered from a session store in another process.
//! Gated on `ANCHORD_BIN` for the same reason as `anchor_client.rs` — anchor
//! is a separate project and this suite cannot assume it is built here.
//! Runs on Windows too, against an anchord serving a named pipe; the
//! scripts are stated per platform (`support::fixture_shell`).

mod support;

use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use support::{anchord_endpoint, fixture_shell};
use teleportd::config::Config;
use tower::ServiceExt;

/// Issues a request against teleport's real router.
async fn request(daemon: &support::Daemon, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = router.oneshot(req).await.expect("router call");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, bytes.to_vec())
}

fn authed(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1")
        .header(header::AUTHORIZATION, format!("Bearer {}", support::TOKEN))
        .body(Body::empty())
        .expect("valid request")
}

async fn get_json(daemon: &support::Daemon, uri: &str) -> (StatusCode, Value) {
    let (status, bytes) = request(daemon, authed("GET", uri)).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Drives teleport's real router, the same way `http_api.rs` does.
async fn post_json(daemon: &support::Daemon, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "127.0.0.1")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", support::TOKEN))
        .body(Body::from(body.to_string()))
        .expect("valid request");
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = router.oneshot(req).await.expect("router call");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// `None` when `ANCHORD_BIN` is unset. Set but not a file is a hard failure:
/// a green run for tests that never executed is worse than no tests.
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
///
/// Every other test here early-returns when the variable is unset, and an
/// early return is counted as a pass — so with no anchord the binary reports
/// `ok. 17 passed` in 0.01s, naming tests that did nothing. That is the green
/// run for tests that never executed which `anchord_bin` above exists to
/// refuse. Locally it is the intended way to run; under CI it is a lie, so
/// under CI it is an error.
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
struct Anchord {
    child: Child,
    dir: PathBuf,
    endpoint: PathBuf,
}

impl Anchord {
    fn start(tag: &str) -> Self {
        let tag = format!("{}-{tag}", std::process::id());
        let dir = anchord_endpoint::scratch_root().join(format!("tp-http-{tag}"));
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort cleanup of a previous run"
        )]
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).expect("creating the data dir");
        let endpoint = anchord_endpoint::endpoint_in(&dir, &tag);
        let child = Self::spawn(&dir, &endpoint);
        Self {
            child,
            dir,
            endpoint,
        }
    }

    fn spawn(dir: &std::path::Path, endpoint: &std::path::Path) -> Child {
        let bin = anchord_bin().expect("checked by the caller");
        anchord_endpoint::command(&bin, dir, endpoint)
            .env("ANCHOR_LOG", "warn")
            .spawn()
            .expect("spawning anchord")
    }

    /// Stops and starts anchord **keeping its data directory**, which is
    /// what makes this a restart rather than a fresh daemon: the store
    /// carries over, so the recovery path that settles sessions whose ptys
    /// died with the old process is the one under test.
    fn restart(&mut self) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort stop of a process about to be replaced"
        )]
        let _ = self.child.kill();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort stop of a process about to be replaced"
        )]
        let _ = self.child.wait();
        self.child = Self::spawn(&self.dir, &self.endpoint);
    }

    fn socket(&self) -> PathBuf {
        self.endpoint.clone()
    }

    /// Waits for the endpoint to accept, so a test never races startup.
    async fn ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !anchord_endpoint::answers(&self.endpoint) {
            assert!(Instant::now() < deadline, "anchord never came up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Anchord {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort teardown in Drop; a failure cannot be propagated"
    )]
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn post_sessions_spawns_in_anchord_not_in_process() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("create");
    anchord.ready().await;

    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;

    let (status, body) = post_json(
        &daemon,
        "/api/v1/sessions",
        &json!({
                "kind": "agent",
                "command": fixture_shell::PROGRAM,
                "args": fixture_shell::args(fixture_shell::pick(
                    "stty raw -echo; echo from-anchor; sleep 30",
                    "Write-Output from-anchor; Start-Sleep 30",
                )),
                "cwd": fixture_shell::cwd(),
            "cols": 80,
            "rows": 24,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {body}");

    let id = body["id"].as_str().expect("an id").to_owned();
    assert_eq!(body["state"], "running");
    assert!(body["pid"].as_u64().is_some());

    // The session is in anchord, not in this process: teleport's own
    // in-process manager never saw it.
    assert!(
        daemon.state.sessions.list().is_empty(),
        "the in-process manager must hold nothing on the anchor path"
    );

    // And anchord really has it, carrying teleport's metadata as labels.
    let client = teleportd::anchor_client::AnchorClient::new(anchord.socket());
    let session = client.get(&id).await.expect("anchord knows the session");
    assert_eq!(session.state, "running");
    assert_eq!(
        session.labels.get("teleport.kind").map(String::as_str),
        Some("agent"),
        "teleport's kind travels as a label"
    );

    client.terminate(&id).await.expect("terminate");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_spawn_keeps_its_status_code_across_the_boundary() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("errors");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;

    // anchord refuses these, and its codes must land as the same statuses
    // teleport's clients already handle — not as a 500.
    let (status, _body) = post_json(
        &daemon,
        "/api/v1/sessions",
        &json!({
            "kind": "shell",
            "command": "definitely-not-on-path-xyz",
            "cwd": fixture_shell::cwd(),
            "cols": 80,
            "rows": 24,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unresolvable command is a 422"
    );

    let (status, _body) = post_json(
        &daemon,
        "/api/v1/sessions",
        &json!({
            "kind": "shell",
            "command": fixture_shell::PROGRAM,
            "cwd": "/nope/not/a/directory",
            "cols": 80,
            "rows": 24,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unusable cwd is a 422"
    );
}

/// Creates a session through teleport and returns its id.
async fn create(daemon: &support::Daemon, script: &str, kind: &str) -> String {
    let (status, body) = post_json(
        daemon,
        "/api/v1/sessions",
        &json!({
            "kind": kind,
            "command": fixture_shell::PROGRAM,
            "args": fixture_shell::args(script),
            "cwd": fixture_shell::cwd(),
            "cols": 80,
            "rows": 24,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {body}");
    body.get("id")
        .and_then(Value::as_str)
        .expect("a created session carries an id")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn get_and_list_serve_anchords_sessions() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("read");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;

    let a = create(
        &daemon,
        fixture_shell::pick(
            "stty raw -echo; echo one; sleep 30",
            "Write-Output one; Start-Sleep 30",
        ),
        "agent",
    )
    .await;
    let b = create(
        &daemon,
        fixture_shell::pick(
            "stty raw -echo; echo two; sleep 30",
            "Write-Output two; Start-Sleep 30",
        ),
        "shell",
    )
    .await;

    let (status, body) = get_json(&daemon, &format!("/api/v1/sessions/{a}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], a.as_str());
    assert_eq!(body["state"], "running");
    // teleport's own metadata survives the round trip as an anchor label.
    assert_eq!(body["kind"], "agent");
    assert_eq!(body["command"], fixture_shell::PROGRAM);

    let (status, body) = get_json(&daemon, "/api/v1/sessions").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));
    // Newest first, same ordering contract as the in-process path.
    assert_eq!(ids[0], b.as_str());

    let (status, _) = get_json(&daemon, "/api/v1/sessions/01JNOPE0000000000000000000").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unknown id is still a 404"
    );

    let client = teleportd::anchor_client::AnchorClient::new(anchord.socket());
    for id in [&a, &b] {
        client.terminate(id).await.expect("terminate");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_log_route_serves_bytes_from_anchord() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("log");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(
        &daemon,
        fixture_shell::pick(
            "stty raw -echo; echo log-me-please; sleep 30",
            "Write-Output log-me-please; Start-Sleep 30",
        ),
        "shell",
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(10);
    let body = loop {
        let (status, bytes) = request(
            &daemon,
            authed("GET", &format!("/api/v1/sessions/{id}/log")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        if bytes.windows(13).any(|w| w == b"log-me-please") {
            break bytes;
        }
        assert!(
            Instant::now() < deadline,
            "the log never carried the output"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // A byte range is honoured the same way as on the in-process path.
    let start = body
        .windows(13)
        .position(|w| w == b"log-me-please")
        .expect("found above");
    let (status, ranged) = request(
        &daemon,
        authed(
            "GET",
            &format!("/api/v1/sessions/{id}/log?from={}&to={}", start, start + 6),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ranged, b"log-me".to_vec());

    let client = teleportd::anchor_client::AnchorClient::new(anchord.socket());
    client.terminate(&id).await.expect("terminate");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_terminates_and_purge_forgets() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("delete");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let client = teleportd::anchor_client::AnchorClient::new(anchord.socket());

    // Without purge: the session ends but stays listed, which is the
    // contract teleport's clients already rely on.
    let id = create(
        &daemon,
        fixture_shell::pick(
            "stty raw -echo; echo bye; sleep 30",
            "Write-Output bye; Start-Sleep 30",
        ),
        "shell",
    )
    .await;
    let (status, _) = request(&daemon, authed("DELETE", &format!("/api/v1/sessions/{id}"))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let session = client.get(&id).await.expect("still known to anchord");
    assert_eq!(session.state, "exited");

    // With purge: gone from anchord entirely.
    let (status, body) = request(
        &daemon,
        authed("DELETE", &format!("/api/v1/sessions/{id}?purge=true")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "purge failed: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        client.get(&id).await.is_err(),
        "a purged session must be gone from anchord"
    );
    let (status, _) = get_json(&daemon, &format!("/api/v1/sessions/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_title_is_derived_from_its_output() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("title");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;

    // Sets a terminal title the way an agent does, then goes quiet. Nobody
    // attaches to this session from the UI at any point — the title has to
    // come from teleport watching the stream on its own, which is the
    // whole reason the watcher exists.
    let id = create(
        &daemon,
        fixture_shell::pick(
            "stty raw -echo; printf '\\033]2;deriving-this\\007'; sleep 30",
            // conhost re-emits OSC 2 as OSC 0, after an OSC 0 of its own
            // naming the program; the watcher reads both as titles and the
            // last one wins, so the wait below still ends on this one.
            "[Console]::Write([char]27 + ']2;deriving-this' + [char]7); Start-Sleep 30",
        ),
        "agent",
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (status, body) = get_json(&daemon, &format!("/api/v1/sessions/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        if body["title"] == "deriving-this" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the title was never derived; got {:?}",
            body["title"]
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let client = teleportd::anchor_client::AnchorClient::new(anchord.socket());
    client.terminate(&id).await.expect("terminate");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bell_is_noticed_without_anyone_attached() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("bell");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;

    let id = create(
        &daemon,
        fixture_shell::pick(
            "stty raw -echo; sleep 1; printf 'needs you\\007'; sleep 30",
            // conhost passes BEL straight through, ahead of the text it was
            // written after -- order is not preserved, presence is.
            "Start-Sleep 1; [Console]::Write('needs you' + [char]7); Start-Sleep 30",
        ),
        "agent",
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (_status, body) = get_json(&daemon, &format!("/api/v1/sessions/{id}")).await;
        if body["last_bell_ms"].as_i64().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the bell was never noticed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let client = teleportd::anchor_client::AnchorClient::new(anchord.socket());
    client.terminate(&id).await.expect("terminate");
}

/// The whole payoff of running sessions in anchord, stated on the wire:
/// restarting this daemon no longer kills them, and the updater reads
/// exactly this to decide whether it may restart under load
/// (docs/08-packaging.md). The in-process path's half of this pair is in
/// `http_api.rs`, which asserts the promise is *absent* there.
#[tokio::test(flavor = "multi_thread")]
async fn the_anchor_path_advertises_that_sessions_survive_a_restart() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("caps");
    anchord.ready().await;

    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let (status, body) = get_json(&daemon, "/api/v1/health").await;

    assert_eq!(status, StatusCode::OK, "health: {body}");
    let caps = body["capabilities"].as_array().expect("a capability list");
    assert!(
        caps.iter().any(|c| c == "sessions_survive_restart"),
        "sessions live in anchord, so a restart does not kill them: {caps:?}"
    );
}

/// A down anchord must never read as "you have no sessions".
///
/// The two are indistinguishable to a client if the daemon answers an
/// outage with `200 []`, and on this path that is the difference between a
/// visible outage and work that looks silently deleted.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_anchord_is_an_error_not_an_empty_list() {
    // No anchord is started: the socket path simply does not exist, which
    // is exactly what a client sees when anchord is not running.
    let daemon = support::spawn_on_anchor(
        Config::default(),
        None,
        anchord_endpoint::endpoint_in(
            &anchord_endpoint::scratch_root().join("anchor-does-not-exist-for-this-test"),
            "does-not-exist-for-this-test",
        ),
    )
    .await;

    let (status, body) = get_json(&daemon, "/api/v1/sessions").await;

    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "an outage must not answer 200: {body}"
    );
    assert_eq!(body["error"], "backend_unavailable", "body: {body}");
}

/// `/health` must report an unreachable anchord as *unknown*, never as
/// zero running sessions. The updater reads this field alongside
/// `sessions_survive_restart`, and a zero would be a confident claim that
/// there is nothing to lose, made exactly when nothing can be asked.
#[tokio::test(flavor = "multi_thread")]
async fn health_reports_an_unreachable_anchord_as_unknown() {
    let daemon = support::spawn_on_anchor(
        Config::default(),
        None,
        anchord_endpoint::endpoint_in(
            &anchord_endpoint::scratch_root().join("anchor-does-not-exist-for-this-test"),
            "does-not-exist-for-this-test",
        ),
    )
    .await;

    let (status, body) = get_json(&daemon, "/api/v1/health").await;

    assert_eq!(status, StatusCode::OK, "health never rejects: {body}");
    assert!(
        body["sessions_running"].is_null(),
        "an unreachable anchord is unknown, not zero: {body}"
    );
    assert_eq!(body["anchor_reachable"], false, "body: {body}");
}

/// **The reason anchor exists**, asserted end to end: a running agent
/// survives teleport being stopped and started again.
///
/// Before this, sessions lived in teleportd, so shipping a release killed
/// every running agent -- which is why the updater refused to restart while
/// any session was live (docs/08-packaging.md). An agent that runs for
/// hours never offers the idle moment that refusal waits for, so in
/// practice teleport could not ship at all while it was being used.
///
/// The restart here is the real one a release performs: the old daemon is
/// dropped, a new one starts against the same anchord with the same owner,
/// and nothing hands state from one to the other -- the new daemon learns
/// everything by asking anchord.
///
/// What this does *not* claim: that a dying anchord is survivable. Its
/// ptys die with it (anchor's docs/07-lifecycle.md). Updates are made
/// survivable, crashes are not.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_survives_teleport_restarting_under_it() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("survive");
    anchord.ready().await;

    // The teleport that spawns the agent.
    let before = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let (status, created) = post_json(
        &before,
        "/api/v1/sessions",
        &json!({
            "kind": "agent",
            "command": fixture_shell::PROGRAM,
            "args": fixture_shell::args(fixture_shell::pick(
                "stty raw -echo; echo agent-is-up; sleep 300",
                "Write-Output agent-is-up; Start-Sleep 300",
            )),
            "cwd": fixture_shell::cwd(),
            "cols": 80,
            "rows": 24,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create: {created}");
    let id = created["id"].as_str().expect("a session id").to_string();

    // Let the agent produce something, so the assertion after the restart
    // is about a session that really ran rather than one that merely exists.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, bytes) = request(
            &before,
            authed("GET", &format!("/api/v1/sessions/{id}/log")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        if bytes.windows(12).any(|w| w == b"agent-is-up\n")
            || bytes.windows(11).any(|w| w == b"agent-is-up")
        {
            break;
        }
        assert!(Instant::now() < deadline, "the agent never started");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Ship a release: teleport goes away.
    drop(before);

    // The new teleport. Same anchord, same owner, no shared state.
    let after = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;

    let (status, listed) = get_json(&after, "/api/v1/sessions").await;
    assert_eq!(status, StatusCode::OK, "list: {listed}");
    let sessions = listed["sessions"].as_array().expect("a session list");
    let found = sessions
        .iter()
        .find(|s| s["id"] == id.as_str())
        .unwrap_or_else(|| panic!("the agent did not survive the restart: {listed}"));
    assert_eq!(
        found["state"], "running",
        "the agent survived but is not running: {found}"
    );

    // Still readable, which is the part that makes survival useful: the
    // transcript is intact from byte zero, not merely the row.
    let (status, bytes) =
        request(&after, authed("GET", &format!("/api/v1/sessions/{id}/log"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        bytes.windows(11).any(|w| w == b"agent-is-up"),
        "the transcript from before the restart is gone: {:?}",
        String::from_utf8_lossy(&bytes)
    );

    let client = teleportd::anchor_client::AnchorClient::new(anchord.socket());
    client.terminate(&id).await.expect("terminate");
}

/// M5's second exit criterion: **anchord restarts without teleport
/// noticing beyond a reconnect.**
///
/// Teleport holds no long-lived connection it cannot rebuild, so a daemon
/// that goes away and comes back must leave it serving requests again --
/// not wedged on a dead socket, and not permanently broken until teleport
/// itself is restarted.
///
/// The sessions themselves do **not** survive, and the test says so
/// explicitly: anchord's ptys die with it (anchor's docs/07-lifecycle.md),
/// so the agent comes back as `lost`, not `running`. That is the honest
/// boundary -- updates are survivable, crashes are not -- and asserting it
/// here keeps anyone from "fixing" this test later into a claim anchor
/// does not make.
#[tokio::test(flavor = "multi_thread")]
async fn anchord_restarting_leaves_teleport_serving() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let mut anchord = Anchord::start("reconnect");
    anchord.ready().await;

    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(
        &daemon,
        fixture_shell::pick(
            "stty raw -echo; echo up; sleep 300",
            "Write-Output up; Start-Sleep 300",
        ),
        "agent",
    )
    .await;

    // anchord goes away and comes back, keeping its store.
    anchord.restart();
    anchord.ready().await;

    // Teleport was never restarted and never told anything. The next
    // request has to work: a client reconnects, it does not give up.
    let deadline = Instant::now() + Duration::from_secs(10);
    let found = loop {
        let (status, listed) = get_json(&daemon, "/api/v1/sessions").await;
        if status == StatusCode::OK {
            if let Some(session) = listed["sessions"]
                .as_array()
                .and_then(|all| all.iter().find(|s| s["id"] == id.as_str()).cloned())
            {
                break session;
            }
        }
        assert!(
            Instant::now() < deadline,
            "teleport never served again after anchord restarted (last status {status})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // Reported honestly: the pty died with the daemon that held it.
    assert_eq!(
        found["state"], "lost",
        "a session whose pty died must not still read as running: {found}"
    );
}

/// A log past anchor's 4 MiB per-response cap is served whole.
///
/// `get_log` assembles a wide range from several `log` calls because one
/// anchor response is capped at 4 MiB. That paging only works if an
/// oversized `max` is *clamped*; anchord used to refuse it with `too_large`,
/// which failed the very first call and so never let the loop run (#98,
/// fixed in anchor 8b47537). Under 4 MiB nothing notices, which is why the
/// test above did not.
#[tokio::test(flavor = "multi_thread")]
async fn a_log_past_one_response_cap_is_assembled_from_several() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("biglog");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;

    // Comfortably past the 4 MiB cap, so the range needs at least two calls.
    // `yes` is the cheapest way to make a PTY produce megabytes quickly.
    let id = create(
        &daemon,
        fixture_shell::pick(
            "stty raw -echo; yes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa | head -c 6000000; echo big-log-done; sleep 30",
            // 95 blocks of 2000 33-byte lines. Through ConPTY this came to
            // 6.46 MB on build 26200, still past the cap: conhost re-renders
            // rather than relays, and adds bytes rather than dropping lines.
            "$l = 'a' * 32; $b = [Text.StringBuilder]::new(); for ($i = 0; $i -lt 2000; $i++) { [void]$b.AppendLine($l) }; $s = $b.ToString(); for ($j = 0; $j -lt 95; $j++) { [Console]::Out.Write($s) }; [Console]::Out.WriteLine('big-log-done'); Start-Sleep 30",
        ),
        "shell",
    )
    .await;

    let cap = 4 * 1024 * 1024;
    // Waits on the log *growing*, not on a clock: this is 6 MB through a pty
    // and back out over HTTP, so a fixed budget measures the machine (the
    // #68/#102/#103/#104 family, docs/10-testing.md#deadlines-preconditions-
    // and-premises). A producer that has actually stopped still fails, after
    // an unbroken streak of polls with the body unchanged.
    let mut longest = 0;
    let mut stalled = 0;
    let body = loop {
        let (status, bytes) = request(
            &daemon,
            authed("GET", &format!("/api/v1/sessions/{id}/log")),
        )
        .await;
        // The bug surfaced here as a 400: anchord answered the first call
        // `too_large`, and anchor_bridge maps that onto BadRequest.
        assert_eq!(status, StatusCode::OK, "the whole-log request was refused");
        if bytes.windows(12).any(|w| w == b"big-log-done") {
            break bytes;
        }
        if bytes.len() > longest {
            longest = bytes.len();
            stalled = 0;
        } else {
            stalled += 1;
            assert!(
                stalled < 100,
                "the log stopped growing at {longest} bytes and stayed there"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    assert!(
        body.len() > cap,
        "the log came back {} bytes, at or under the {cap}-byte cap the \
         paging loop exists to get past",
        body.len()
    );
}

/// A ws-ticket is issued for a session anchord owns.
///
/// The route trades the long-lived bearer token for a short-lived one scoped
/// to a single session, so the real credential never rides in a `stream.ts`
/// reconnect URL (docs/06-security.md mitigation 2). It resolved the session
/// through the in-process map, which is empty on the anchor path — so every
/// request 404'd for sessions that plainly exist, and clients were pushed
/// back onto `?token=`, silently losing the mitigation (#100).
///
/// A mitigation that is off is worse than one that was never there, so this
/// asserts the ticket is issued *and* that an unknown id is still refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_ws_ticket_is_issued_for_a_session_in_anchord() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("wsticket");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(
        &daemon,
        fixture_shell::pick("stty raw -echo; sleep 30", "Start-Sleep 30"),
        "shell",
    )
    .await;

    let (status, body) =
        post_json(&daemon, "/api/v1/ws-ticket", &json!({ "session_id": id })).await;
    assert_eq!(status, StatusCode::OK, "the ticket was refused: {body}");
    assert!(
        body["ticket"].as_str().is_some_and(|t| !t.is_empty()),
        "no ticket in {body}"
    );
    assert!(
        body["expires_in"].as_u64().is_some_and(|s| s > 0),
        "no TTL in {body}"
    );

    // A well-formed id nobody knows is still a 404: the ticket is scoped to a
    // session, so one that does not exist must not get a credential.
    let (status, _) = post_json(
        &daemon,
        "/api/v1/ws-ticket",
        &json!({ "session_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unknown id got a ticket");

    // A malformed id is the same 404, not a 500.
    let (status, _) = post_json(
        &daemon,
        "/api/v1/ws-ticket",
        &json!({ "session_id": "not-a-ulid" }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a malformed id was not a 404"
    );
}

/// Purging a session releases its lease, not just its derived signals.
///
/// `LeaseRegistry::forget` existed and documented exactly this, but nothing
/// called it: the delete handler forgot the watch entry and left the `Lease`
/// and its `broadcast::Sender` behind for the life of the process (#101).
/// Create-and-purge is the normal workload, so it grew without bound.
#[tokio::test(flavor = "multi_thread")]
async fn purging_a_session_releases_its_lease() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("leasepurge");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;
    let id = create(
        &daemon,
        fixture_shell::pick("stty raw -echo; sleep 30", "Start-Sleep 30"),
        "shell",
    )
    .await;

    // Claiming is what puts the session in the registry; a session nobody
    // ever controlled would pass this test without the fix.
    daemon.state.leases.claim_control(&id, "browser-a", "A");
    assert!(
        daemon.state.leases.is_tracking(&id),
        "the claim did not register"
    );

    let (status, _) = request(
        &daemon,
        authed("DELETE", &format!("/api/v1/sessions/{id}?purge=true")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(
        !daemon.state.leases.is_tracking(&id),
        "the purged session's lease and event channel are still held"
    );
}

/// Derived agent metadata survives teleport restarting, for a session that
/// has **exited**.
///
/// On the anchor path `title` and `claude_resume_id` lived only in
/// `WatchRegistry` memory. A *running* session hides that: the new teleport
/// starts a watcher from offset 0 and re-derives both from the replayed log,
/// so it recovers without any persistence at all. An exited session gets no
/// watcher (`anchor_watch::reconcile` skips anything not running or closing),
/// so nothing re-derives and both fields come back null.
///
/// That is the case `api.rs`'s own comment is about: "a resume id is only
/// ever useful once the session that had it is gone, and the usual way it
/// goes is the daemon restarting under it." A test against a running session
/// passes either way and proves nothing -- it was written that way first.
///
/// The two are stored differently and this asserts both survive. The title
/// goes to teleport's SQLite (chatty, re-emitted every prompt); the resume id
/// becomes an anchor label (written once, in a startup banner).
#[tokio::test(flavor = "multi_thread")]
async fn derived_agent_metadata_survives_a_teleport_restart() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("metasurvive");
    anchord.ready().await;
    let dir = anchord_endpoint::scratch_root().join(format!("tp-meta-{}", std::process::id()));
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort cleanup of a previous run"
    )]
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating the state dir");
    let (db, _guard) =
        teleportd::persistence::Db::open(&dir.join("state.db"), &dir.join("sessions"))
            .expect("open test db");

    // Whether the resume link can reach the log at all. Not on Windows:
    // conhost swallows OSC 8 rather than relaying it -- written, and absent
    // from what ConPTY emitted, on Windows 11 build 26200 (2026-10-19). The
    // title half still runs there. The link has no live producer on any
    // platform anyway (`session/osc.rs`'s module doc); this keeps the Unix
    // half of the check honest rather than pretending Windows has it.
    let resume_reaches_the_log = cfg!(unix);

    // An agent that sets a terminal title and prints a resume link, the way
    // a real one does in its opening banner.
    let before =
        support::spawn_on_anchor(Config::default(), Some(db.clone()), anchord.socket()).await;
    let (status, created) = post_json(
        &before,
        "/api/v1/sessions",
        &json!({
            "kind": "agent",
            "command": fixture_shell::PROGRAM,
            "args": fixture_shell::args(fixture_shell::pick(
                "stty raw -echo; \
                 printf '\\033]0;Fixing the parser\\007'; \
                 printf '\\033]8;id=gq2qz2;https://claude.ai/code/session_013YMNvvZ2U1fUjQsxAbqRjj?from=cli\\007'; \
                 sleep 2",
                "[Console]::Write([char]27 + ']0;Fixing the parser' + [char]7); \
                 [Console]::Write([char]27 + ']8;id=gq2qz2;https://claude.ai/code/session_013YMNvvZ2U1fUjQsxAbqRjj?from=cli' + [char]7); \
                 Start-Sleep 2",
            )),
            "cwd": fixture_shell::cwd(),
            "cols": 80,
            "rows": 24,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create: {created}");
    let id = created["id"].as_str().expect("a session id").to_string();

    // Wait for the watcher to derive both before restarting; asserting on a
    // restart that happened before anything was derived proves nothing.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, view) = get_json(&before, &format!("/api/v1/sessions/{id}")).await;
        if view["title"] == "Fixing the parser"
            && (!resume_reaches_the_log || !view["claude_resume_id"].is_null())
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the first teleport never derived them: {view}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Let it exit. An exited session is the one that loses its metadata,
    // because nothing re-derives it after a restart.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, view) = get_json(&before, &format!("/api/v1/sessions/{id}")).await;
        if view["state"] == "exited" {
            break;
        }
        assert!(Instant::now() < deadline, "the agent never exited: {view}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Ship a release: teleport goes away, the record does not.
    drop(before);

    // The new teleport. Same anchord, same SQLite, no in-memory state.
    let after =
        support::spawn_on_anchor(Config::default(), Some(db.clone()), anchord.socket()).await;

    let deadline = Instant::now() + Duration::from_secs(20);
    let view = loop {
        let (_, view) = get_json(&after, &format!("/api/v1/sessions/{id}")).await;
        if !view["title"].is_null()
            && (!resume_reaches_the_log || !view["claude_resume_id"].is_null())
        {
            break view;
        }
        assert!(
            Instant::now() < deadline,
            "metadata did not survive the restart: {view}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        view["title"], "Fixing the parser",
        "the title did not survive: {view}"
    );
    if resume_reaches_the_log {
        assert_eq!(
            view["claude_resume_id"], "session_013YMNvvZ2U1fUjQsxAbqRjj",
            "the resume id did not survive: {view}"
        );
    }

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort teardown of the test's state dir"
    )]
    let _ = std::fs::remove_dir_all(&dir);
}

/// The title an exited session keeps across a restart is the **last** one it
/// set, even when it set two inside the persistence throttle's second.
///
/// The write-through was leading-edge only, so the second title of a burst
/// lived in memory and nowhere else. Found running this suite on Windows,
/// where conhost emits a title of its own before the agent's and every
/// exited session came back titled with the program's path; reproduced here
/// on both platforms by setting two titles back to back.
#[tokio::test(flavor = "multi_thread")]
async fn the_last_title_of_a_burst_is_the_one_that_survives() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("lasttitle");
    anchord.ready().await;
    let dir = anchord_endpoint::scratch_root().join(format!("tp-lasttitle-{}", std::process::id()));
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort cleanup of a previous run"
    )]
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating the state dir");
    let (db, _guard) =
        teleportd::persistence::Db::open(&dir.join("state.db"), &dir.join("sessions"))
            .expect("open test db");

    let before =
        support::spawn_on_anchor(Config::default(), Some(db.clone()), anchord.socket()).await;
    let id = create(
        &before,
        fixture_shell::pick(
            "stty raw -echo; printf '\\033]0;first-guess\\007'; printf '\\033]0;the-last-word\\007'; sleep 2",
            "[Console]::Write([char]27 + ']0;first-guess' + [char]7); [Console]::Write([char]27 + ']0;the-last-word' + [char]7); Start-Sleep 2",
        ),
        "agent",
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, view) = get_json(&before, &format!("/api/v1/sessions/{id}")).await;
        if view["state"] == "exited" && view["title"] == "the-last-word" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the first teleport never saw it exit titled: {view}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The trailing write happens as the watcher ends, after the exit frame;
    // a teleport dropped in the same instant can still race it, which is a
    // different and narrower window than the one this test is about.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(before);

    let after =
        support::spawn_on_anchor(Config::default(), Some(db.clone()), anchord.socket()).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let view = loop {
        let (_, view) = get_json(&after, &format!("/api/v1/sessions/{id}")).await;
        if !view["title"].is_null() {
            break view;
        }
        assert!(
            Instant::now() < deadline,
            "no title after the restart: {view}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        view["title"], "the-last-word",
        "a restarted teleport must show the last title, not the first: {view}"
    );

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort teardown of the test's state dir"
    )]
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `create` whose control line is past anchor's 1 MiB cap comes back a
/// 400 naming `too_large`, not a 500.
///
/// Anchord answers an oversized control line and *then* closes, so
/// teleport's own write usually fails with the reply already on the wire.
/// Acting on the write error instead of reading that reply turned the one
/// failure anchor took care to name into a broken pipe -- `ApiError::
/// Internal`, indistinguishable from anchord having died (#99).
///
/// `env` is the reachable way to get there: the spec carries the
/// environment, so the control line grows with it. The body stays under
/// axum's 2 MB default so the 400 comes from anchor rather than from the
/// extractor.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_create_is_refused_as_too_large_not_as_an_internal_error() {
    if anchord_bin().is_none() {
        eprintln!("skipping: ANCHORD_BIN is not set");
        return;
    }
    let anchord = Anchord::start("toolarge");
    anchord.ready().await;
    let daemon = support::spawn_on_anchor(Config::default(), None, anchord.socket()).await;

    // Past 1 MiB once serialised, comfortably under the 2 MB body limit.
    let (status, body) = post_json(
        &daemon,
        "/api/v1/sessions",
        &json!({
            "kind": "shell",
            "command": fixture_shell::PROGRAM,
            "args": fixture_shell::args(fixture_shell::pick("sleep 30", "Start-Sleep 30")),
            "cwd": fixture_shell::cwd(),
            "cols": 80,
            "rows": 24,
            "env": { "TELEPORT_BIG": "x".repeat(1_400_000) },
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an oversized control line must be a 400, not a 500: {body}"
    );
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        message.contains("too_large"),
        "the 400 must carry anchor's code: {body}"
    );
}
