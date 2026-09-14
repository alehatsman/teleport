//! M4's HTTP gate -- the request/response half of
//! docs/10-testing.md#3-protocol-tests. Drives `api.rs`'s router in-process
//! via `tower::ServiceExt::oneshot`, no real socket needed. WS-specific
//! checks (framing, control lease, Origin on the upgrade) live in
//! `ws_protocol.rs`.
//!
//! `cfg(unix)`-gated for the same reason as `session_replay.rs`: several
//! fixtures drive a real `/bin/sh`, which doesn't exist on Windows -- same
//! boundary as [W2](../../docs/15-open-questions.md#w2--windows-fixture-parity-not-yet-attempted)
//! (docs/11-mvp-plan.md#m4--http--websocket-api: "Windows: cross-compile-checked
//! only, no fixtures run").

#![cfg(unix)]

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

async fn request(daemon: &support::Daemon, req: Request<Body>) -> (StatusCode, Value) {
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = router.oneshot(req).await.expect("router call");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("valid request")
}

fn post_json(uri: &str, token: Option<&str>, body: &Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, "127.0.0.1")
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", token.unwrap_or("")),
        )
        .body(Body::from(body.to_string()))
        .expect("valid request")
}

/// `oneshot` bypasses real HTTP/1.1 framing, which is normally what adds a
/// `Host` header -- these mutating routes need one explicitly for
/// [`teleportd::auth::OriginPolicy::check`] to have anything to check
/// (docs/06-security.md#browser-origin-defense: "Host must be in the
/// allowlist, always"). Bare `127.0.0.1` matches regardless of the daemon's
/// actual bound port -- the check strips the port before comparing.
fn delete_request(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header(header::HOST, "127.0.0.1")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("valid request")
}

#[tokio::test]
async fn unauthenticated_health_omits_device_fields() {
    let daemon = support::spawn(support::default_config()).await;
    let (status, body) = request(&daemon, get("/api/v1/health", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert!(body.get("api_versions").is_some());
    assert!(body.get("capabilities").is_some());
    assert!(
        body.get("device_id").is_none(),
        "unauthenticated /health must not leak device_id"
    );
    assert!(
        body.get("device_name").is_none(),
        "unauthenticated /health must not leak device_name"
    );
    assert!(
        body.get("home_dir").is_none(),
        "unauthenticated /health must not leak home_dir"
    );
}

#[tokio::test]
async fn authenticated_health_includes_device_fields() {
    let daemon = support::spawn(support::default_config()).await;
    let (status, body) = request(&daemon, get("/api/v1/health", Some(support::TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["device_id"], "01TESTDEVICE00000000000000");
    assert_eq!(body["device_name"], "test-device");
    assert!(body.get("uptime_ms").is_some());
    assert!(body.get("sessions_running").is_some());
    // Same resolution the handler itself uses -- this machine really does
    // have a home directory in CI/dev alike, so this isn't `.is_some()`
    // padding; it pins the exact value.
    let expected_home = directories::BaseDirs::new()
        .expect("this test machine has a home directory")
        .home_dir()
        .display()
        .to_string();
    assert_eq!(body["home_dir"], expected_home);
}

#[tokio::test]
async fn every_route_except_health_rejects_a_request_with_no_token() {
    let daemon = support::spawn(support::default_config()).await;

    let (status, _) = request(&daemon, get("/api/v1/sessions", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = request(&daemon, get("/api/v1/presets", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Including on loopback -- the whole point of
    // docs/06-security.md#loopback-is-not-a-user-boundary.
    let (status, _) = request(
        &daemon,
        get("/api/v1/sessions/01ARZ3NDEKTSV4RRFFQ69G5FAV", None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_session_with_a_nonexistent_executable_is_422_not_404_and_writes_no_row() {
    let daemon = support::spawn(support::default_config()).await;
    let body = json!({
        "kind": "shell",
        "command": "this-executable-does-not-exist-anywhere",
        "cwd": std::env::temp_dir().to_string_lossy(),
        "cols": 80,
        "rows": 24,
    });
    let (status, _) = request(
        &daemon,
        post_json("/api/v1/sessions", Some(support::TOKEN), &body),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, list) = request(&daemon, get("/api/v1/sessions", Some(support::TOKEN))).await;
    assert_eq!(
        list["sessions"].as_array().unwrap().len(),
        0,
        "a rejected create must not leave a session behind"
    );
}

#[tokio::test]
async fn max_sessions_plus_one_is_429_and_the_daemon_stays_healthy() {
    let mut config = support::default_config();
    config.max_sessions = 2;
    let daemon = support::spawn(config).await;

    let make = |cmd: &str| {
        json!({
            "kind": "shell",
            "command": "/bin/sh",
            "args": ["-c", cmd],
            "cwd": std::env::temp_dir().to_string_lossy(),
            "cols": 80,
            "rows": 24,
        })
    };

    let (s1, _) = request(
        &daemon,
        post_json("/api/v1/sessions", Some(support::TOKEN), &make("sleep 5")),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, _) = request(
        &daemon,
        post_json("/api/v1/sessions", Some(support::TOKEN), &make("sleep 5")),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);
    let (s3, _) = request(
        &daemon,
        post_json("/api/v1/sessions", Some(support::TOKEN), &make("sleep 5")),
    )
    .await;
    assert_eq!(s3, StatusCode::TOO_MANY_REQUESTS);

    let (health_status, health_body) =
        request(&daemon, get("/api/v1/health", Some(support::TOKEN))).await;
    assert_eq!(health_status, StatusCode::OK);
    assert_eq!(health_body["sessions_running"], 2);
}

#[tokio::test]
async fn full_lifecycle_create_list_terminate_purge() {
    let daemon = support::spawn(support::default_config()).await;
    let body = json!({
        "kind": "shell",
        "command": "/bin/sh",
        "args": ["-c", "sleep 5"],
        "cwd": std::env::temp_dir().to_string_lossy(),
        "cols": 80,
        "rows": 24,
    });
    let (status, created) = request(
        &daemon,
        post_json("/api/v1/sessions", Some(support::TOKEN), &body),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["state"], "running");

    let (_, one) = request(
        &daemon,
        get(&format!("/api/v1/sessions/{id}"), Some(support::TOKEN)),
    )
    .await;
    assert_eq!(one["id"], id);
    assert_eq!(one["command"], "/bin/sh");

    // Terminate: 202, still listed as exited (not removed) until purge.
    let (status, _) = request(
        &daemon,
        delete_request(&format!("/api/v1/sessions/{id}"), support::TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (status, view) = request(
            &daemon,
            get(&format!("/api/v1/sessions/{id}"), Some(support::TOKEN)),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "must stay listed after terminate, before purge"
        );
        if view["state"] == "exited" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session never reached exited"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // Purge: gone from GET.
    let (status, _) = request(
        &daemon,
        delete_request(&format!("/api/v1/sessions/{id}?purge=true"), support::TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = request(
        &daemon,
        get(&format!("/api/v1/sessions/{id}"), Some(support::TOKEN)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// M4 review: `max_sessions` used to count every directory entry, including
/// `exited`-but-unpurged ones, so routine create/DELETE traffic (without
/// `?purge=true`) could wedge `create()` at 429 forever with nothing
/// actually running.
#[tokio::test]
async fn purging_an_exited_session_frees_a_max_sessions_slot() {
    let mut config = support::default_config();
    config.max_sessions = 1;
    let daemon = support::spawn(config).await;

    let make = |cmd: &str| {
        json!({
            "kind": "shell",
            "command": "/bin/sh",
            "args": ["-c", cmd],
            "cwd": std::env::temp_dir().to_string_lossy(),
            "cols": 80,
            "rows": 24,
        })
    };

    let (status, created) = request(
        &daemon,
        post_json("/api/v1/sessions", Some(support::TOKEN), &make("true")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_string();

    // Let it exit on its own, then terminate (202) without ?purge=true --
    // the M2 contract keeps it listed as `exited`, not removed.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (_, view) = request(
            &daemon,
            get(&format!("/api/v1/sessions/{id}"), Some(support::TOKEN)),
        )
        .await;
        if view["state"] == "exited" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session never reached exited"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // At the cap, but nothing is actually running -- create must not 429.
    let (status, _) = request(
        &daemon,
        post_json("/api/v1/sessions", Some(support::TOKEN), &make("true")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "an exited-but-unpurged session must not count against max_sessions"
    );
}

fn browse_test_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "teleportd-browse-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_nanos()
    ))
}

#[tokio::test]
async fn browse_without_a_credential_is_rejected() {
    let daemon = support::spawn(support::default_config()).await;
    let (status, _) = request(&daemon, get("/api/v1/browse", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn browse_lists_subdirectories_only_dotfiles_and_plain_files_excluded() {
    let daemon = support::spawn(support::default_config()).await;
    let root = browse_test_dir("listing");
    std::fs::create_dir_all(root.join("project-a")).unwrap();
    std::fs::create_dir_all(root.join("project-b")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("README.md"), "not a directory").unwrap();

    let (status, body) = request(
        &daemon,
        get(
            &format!("/api/v1/browse?path={}", root.display()),
            Some(support::TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let names: Vec<&str> = body["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|e| e["name"].as_str().expect("entry name"))
        .collect();
    assert_eq!(
        names,
        vec!["project-a", "project-b"],
        "only real subdirectories, sorted, no dotfiles or files"
    );

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn browse_reports_the_resolved_path_and_its_parent() {
    let daemon = support::spawn(support::default_config()).await;
    let root = browse_test_dir("parent");
    std::fs::create_dir_all(&root).unwrap();
    let canonical_root = root.canonicalize().expect("canonicalize");

    let (status, body) = request(
        &daemon,
        get(
            &format!("/api/v1/browse?path={}", root.display()),
            Some(support::TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["path"], canonical_root.display().to_string());
    assert_eq!(
        body["parent"],
        canonical_root.parent().unwrap().display().to_string()
    );

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn browse_with_no_path_defaults_to_the_daemon_s_home_directory() {
    let daemon = support::spawn(support::default_config()).await;
    let (status, body) = request(&daemon, get("/api/v1/browse", Some(support::TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    let expected_home = directories::BaseDirs::new()
        .expect("this test machine has a home directory")
        .home_dir()
        .canonicalize()
        .expect("canonicalize")
        .display()
        .to_string();
    assert_eq!(body["path"], expected_home);
}

#[tokio::test]
async fn browse_of_a_nonexistent_path_is_a_400_not_a_500() {
    let daemon = support::spawn(support::default_config()).await;
    let root = browse_test_dir("missing");
    let (status, _) = request(
        &daemon,
        get(
            &format!("/api/v1/browse?path={}", root.display()),
            Some(support::TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn browse_of_a_plain_file_not_a_directory_is_a_400() {
    let daemon = support::spawn(support::default_config()).await;
    let root = browse_test_dir("not-a-dir");
    std::fs::create_dir_all(&root).unwrap();
    let file_path = root.join("plain-file.txt");
    std::fs::write(&file_path, "hello").unwrap();

    let (status, _) = request(
        &daemon,
        get(
            &format!("/api/v1/browse?path={}", file_path.display()),
            Some(support::TOKEN),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn bad_origin_on_a_mutating_request_is_rejected() {
    let daemon = support::spawn(support::default_config()).await;
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/sessions")
        .header(header::HOST, "127.0.0.1")
        .header(header::AUTHORIZATION, format!("Bearer {}", support::TOKEN))
        .header(header::ORIGIN, "https://evil.example")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "kind": "shell", "command": "/bin/sh", "cwd": "/tmp", "cols": 80, "rows": 24 })
                .to_string(),
        ))
        .unwrap();
    let (status, _) = request(&daemon, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// --- pinned locations (docs/18-locations.md#pins) -----------------------------

/// A scratch directory tree plus the daemon that stores pins about it. Both
/// live under one root so a single `remove_dir_all` cleans up.
async fn pin_fixture(name: &str) -> (support::Daemon, std::path::PathBuf) {
    let root = browse_test_dir(name);
    std::fs::create_dir_all(root.join("daemon")).expect("create scratch daemon dir");
    std::fs::create_dir_all(root.join("project")).expect("create scratch project dir");
    let daemon = support::spawn_with_db(support::default_config(), &root.join("daemon")).await;
    (daemon, root)
}

fn cleanup(root: &std::path::Path) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn pins_without_a_credential_are_rejected() {
    let daemon = support::spawn(support::default_config()).await;
    let (status, _) = request(&daemon, get("/api/v1/locations/pins", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_pinned_directory_comes_back_canonicalized_and_listed() {
    let (daemon, root) = pin_fixture("pin-add").await;
    // Routed through "project/.." so the response proves canonicalization:
    // a pin has to be spelled the same way a session's `cwd` is, or the
    // launcher can never match the two up.
    let requested = root.join("project").join("..").join("project");
    let canonical = requested.canonicalize().unwrap().display().to_string();

    let (status, body) = request(
        &daemon,
        post_json(
            "/api/v1/locations/pins",
            Some(support::TOKEN),
            &json!({ "path": requested.display().to_string() }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["path"], canonical);

    let (status, body) =
        request(&daemon, get("/api/v1/locations/pins", Some(support::TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pins"][0]["path"], canonical);

    cleanup(&root);
}

#[tokio::test]
async fn pinning_a_path_that_is_not_a_directory_is_rejected() {
    let (daemon, root) = pin_fixture("pin-bad").await;
    std::fs::write(root.join("README.md"), "not a directory").unwrap();

    for path in [
        root.join("README.md").display().to_string(),
        root.join("nope").display().to_string(),
    ] {
        let (status, _) = request(
            &daemon,
            post_json(
                "/api/v1/locations/pins",
                Some(support::TOKEN),
                &json!({ "path": path }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "pinning {path}");
    }

    cleanup(&root);
}

#[tokio::test]
async fn unpinning_uses_the_stored_path_and_is_idempotent() {
    let (daemon, root) = pin_fixture("pin-remove").await;
    let project = root.join("project").canonicalize().unwrap();
    let (status, _) = request(
        &daemon,
        post_json(
            "/api/v1/locations/pins",
            Some(support::TOKEN),
            &json!({ "path": project.display().to_string() }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Deleted out from under the daemon first: a pin whose directory is gone
    // is exactly the one someone needs to remove, so DELETE must not try to
    // resolve it (docs/18-locations.md#delete-apiv1locationspinspath).
    std::fs::remove_dir_all(&project).unwrap();
    let uri = format!(
        "/api/v1/locations/pins?path={}",
        urlencode(&project.display().to_string())
    );

    let (status, _) = request(&daemon, delete_request(&uri, support::TOKEN)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&daemon, delete_request(&uri, support::TOKEN)).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "unpinning twice is still success"
    );

    let (_, body) = request(&daemon, get("/api/v1/locations/pins", Some(support::TOKEN))).await;
    assert_eq!(body["pins"].as_array().expect("pins array").len(), 0);

    cleanup(&root);
}

/// Percent-encodes the few characters a temp path can actually carry into a
/// query string. Not a general-purpose encoder -- there is no URL crate in
/// this dependency set, and one test does not justify adding it.
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}
