//! Teleport's own HTTP surface, served by an external `anchord`.
//!
//! This is the shape of anchor's M4 acceptance: the same routes teleport's
//! clients already call, answered from a session store in another process.
//! Gated on `ANCHORD_BIN` for the same reason as `anchor_client.rs` — anchor
//! is a separate project and this suite cannot assume it is built here.

#![cfg(unix)]

mod support;

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
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

/// Short path: a unix socket path is capped near 104 bytes.
struct Anchord {
    child: Child,
    dir: PathBuf,
}

impl Anchord {
    fn start(tag: &str) -> Self {
        let dir = PathBuf::from(format!("/tmp/tp-http-{}-{tag}", std::process::id()));
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort cleanup of a previous run"
        )]
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("run")).expect("creating the data dir");
        let child = Command::new(anchord_bin().expect("checked by the caller"))
            .env("XDG_DATA_HOME", &dir)
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("ANCHOR_LOG", "warn")
            .spawn()
            .expect("spawning anchord");
        Self { child, dir }
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
        self.child = Command::new(anchord_bin().expect("checked by the caller"))
            .env("XDG_DATA_HOME", &self.dir)
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("ANCHOR_LOG", "warn")
            .spawn()
            .expect("respawning anchord");
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("run/anchor.sock")
    }

    /// Waits for the socket to accept, so a test never races startup.
    async fn ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while tokio::net::UnixStream::connect(self.socket())
            .await
            .is_err()
        {
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
                "command": "/bin/sh",
                "args": ["-c", "stty raw -echo; echo from-anchor; sleep 30"],
                "cwd": "/tmp",
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
            "cwd": "/tmp",
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
            "command": "/bin/sh",
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
            "command": "/bin/sh",
            "args": ["-c", script],
            "cwd": "/tmp",
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

    let a = create(&daemon, "stty raw -echo; echo one; sleep 30", "agent").await;
    let b = create(&daemon, "stty raw -echo; echo two; sleep 30", "shell").await;

    let (status, body) = get_json(&daemon, &format!("/api/v1/sessions/{a}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], a.as_str());
    assert_eq!(body["state"], "running");
    // teleport's own metadata survives the round trip as an anchor label.
    assert_eq!(body["kind"], "agent");
    assert_eq!(body["command"], "/bin/sh");

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
        "stty raw -echo; echo log-me-please; sleep 30",
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
    let id = create(&daemon, "stty raw -echo; echo bye; sleep 30", "shell").await;
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
        "stty raw -echo; printf '\\033]2;deriving-this\\007'; sleep 30",
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
        "stty raw -echo; sleep 1; printf 'needs you\\007'; sleep 30",
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
        PathBuf::from("/tmp/anchor-does-not-exist-for-this-test.sock"),
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
        PathBuf::from("/tmp/anchor-does-not-exist-for-this-test.sock"),
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
            "command": "/bin/sh",
            "args": ["-c", "stty raw -echo; echo agent-is-up; sleep 300"],
            "cwd": "/tmp",
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
    let id = create(&daemon, "stty raw -echo; echo up; sleep 300", "agent").await;

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
    let id = create(&daemon, "stty raw -echo; sleep 30", "shell").await;

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
    let id = create(&daemon, "stty raw -echo; sleep 30", "shell").await;

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
