//! SPA static serving -- M5's daemon-side deliverable
//! (docs/08-packaging.md#build-pipeline: "SPA fallback: unknown non-/api
//! paths return index.html").

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("valid request")
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

fn write_dist(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("assets")).expect("create test dist dir");
    std::fs::write(dir.join("index.html"), "<html>spa shell</html>")
        .expect("write test index.html");
    std::fs::write(dir.join("assets/app.js"), "console.log('app')").expect("write test app.js");
}

#[tokio::test]
async fn no_web_dist_configured_serves_api_only() {
    let daemon = support::spawn_with_web_dist(support::default_config(), None).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = router.oneshot(get("/")).await.expect("router call");
    // No fallback_service registered at all -- axum's own default 404.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn known_asset_is_served_from_web_dist() {
    let dist =
        std::env::temp_dir().join(format!("teleportd-web-dist-{}-assets", std::process::id()));
    write_dist(&dist);
    let daemon = support::spawn_with_web_dist(support::default_config(), Some(dist.clone())).await;

    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = router
        .oneshot(get("/assets/app.js"))
        .await
        .expect("router call");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "console.log('app')");

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&dist);
}

#[tokio::test]
async fn unknown_client_route_falls_back_to_index_html() {
    let dist = std::env::temp_dir().join(format!(
        "teleportd-web-dist-{}-fallback",
        std::process::id()
    ));
    write_dist(&dist);
    let daemon = support::spawn_with_web_dist(support::default_config(), Some(dist.clone())).await;

    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    // A client-side route with no matching file on disk -- e.g. deep-linking
    // straight to a session view -- must still get the SPA shell so Svelte's
    // router can take over, not a raw 404.
    let response = router
        .oneshot(get("/sessions/01ARZ3NDEKTSV4RRFFQ69G5FAV"))
        .await
        .expect("router call");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "<html>spa shell</html>");

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&dist);
}

#[tokio::test]
async fn unknown_api_route_is_a_plain_404_not_the_spa_shell() {
    let dist = std::env::temp_dir().join(format!(
        "teleportd-web-dist-{}-apinotfound",
        std::process::id()
    ));
    write_dist(&dist);
    let daemon = support::spawn_with_web_dist(support::default_config(), Some(dist.clone())).await;

    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    // `nest("/api/v1", ...)` gives the mount point its own 404 for an
    // unmatched sub-path -- it must never fall through to the SPA fallback,
    // or a typo'd API path would silently return HTML instead of an error.
    let response = router
        .oneshot(get("/api/v1/this-route-does-not-exist"))
        .await
        .expect("router call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_ne!(body_string(response).await, "<html>spa shell</html>");

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&dist);
}

/// docs/06-security.md's CSP applies to *every* response -- a single
/// `Router::layer`, not threaded through the static path alone -- so this
/// checks it on both an API response and the SPA shell, not just one.
#[tokio::test]
async fn every_response_carries_the_content_security_policy() {
    let dist = std::env::temp_dir().join(format!("teleportd-web-dist-{}-csp", std::process::id()));
    write_dist(&dist);
    let daemon = support::spawn_with_web_dist(support::default_config(), Some(dist.clone())).await;

    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let api_response = router
        .clone()
        .oneshot(get("/api/v1/health"))
        .await
        .expect("router call");
    let csp = api_response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("CSP header on an API response")
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("object-src 'none'"));
    assert!(csp.contains("frame-ancestors 'none'"));

    let spa_response = router.oneshot(get("/")).await.expect("router call");
    assert!(spa_response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .is_some());

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&dist);
}

// --- version slot (docs/18-ui-upgrades.md) ---------------------------------

/// A `<data_dir>/web` slot root with one version directory in it, `current`
/// pointing at it, and `body` as the content of its one hashed asset.
fn write_slot(root: &std::path::Path, version: &str, body: &str) {
    let dir = root.join(version);
    std::fs::create_dir_all(dir.join("assets")).expect("create version dir");
    std::fs::write(dir.join("index.html"), format!("<html>{version}</html>"))
        .expect("write index.html");
    std::fs::write(dir.join(format!("assets/index-{version}.js")), body).expect("write asset");
    flip(root, version);
}

/// The upgrade's one irreversible step, exactly as `teleport ui upgrade`
/// performs it: symlink to a temp name, then `rename(2)` over `current`.
fn flip(root: &std::path::Path, version: &str) {
    let staged = root.join(".current.staged");
    #[expect(
        clippy::let_underscore_must_use,
        reason = "clearing a leftover from an earlier flip in the same test; fine if absent"
    )]
    let _ = std::fs::remove_file(&staged);
    std::os::unix::fs::symlink(version, &staged).expect("symlink");
    std::fs::rename(&staged, root.join("current")).expect("rename over current");
}

fn slot_root(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("teleportd-web-slot-{}-{name}", std::process::id()));
    #[expect(
        clippy::let_underscore_must_use,
        reason = "clearing a stale dir from a previous run; fine if absent"
    )]
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create slot root");
    dir
}

fn slot_daemon(root: &std::path::Path) -> impl std::future::Future<Output = support::Daemon> {
    support::spawn_with_web_assets(
        support::default_config(),
        teleportd::web_assets::WebAssets::new(None, Some(root.to_path_buf())),
    )
}

/// The claim the whole design rests on: the bundle changes underneath a
/// *running* daemon, with no restart and no signal anywhere in this test.
#[tokio::test]
async fn flipping_the_slot_changes_what_is_served_without_a_restart() {
    let root = slot_root("flip");
    write_slot(&root, "v1.0.0", "old");
    let daemon = slot_daemon(&root).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));

    let before = router.clone().oneshot(get("/")).await.expect("router call");
    assert_eq!(body_string(before).await, "<html>v1.0.0</html>");

    write_slot(&root, "v1.1.0", "new");

    let after = router.oneshot(get("/")).await.expect("router call");
    assert_eq!(body_string(after).await, "<html>v1.1.0</html>");

    cleanup(&root);
}

/// The `Option<PathBuf>`-decided-at-startup regression: a daemon that booted
/// before the first-ever `teleport ui upgrade` must pick the slot up live.
/// Resolving once at startup would make the first upgrade need the one
/// restart this design exists to avoid.
#[tokio::test]
async fn a_daemon_started_with_no_slot_picks_up_the_first_one() {
    let root = slot_root("first");
    let daemon = slot_daemon(&root).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));

    let before = router.clone().oneshot(get("/")).await.expect("router call");
    assert_eq!(before.status(), StatusCode::NOT_FOUND);

    write_slot(&root, "v1.0.0", "first");

    let after = router.oneshot(get("/")).await.expect("router call");
    assert_eq!(after.status(), StatusCode::OK);
    assert_eq!(body_string(after).await, "<html>v1.0.0</html>");

    cleanup(&root);
}

/// A tab that loaded the previous UI still requests its content-hashed
/// chunks. They live only in the directory the flip moved off of.
#[tokio::test]
async fn a_hashed_asset_from_a_retained_version_is_still_served() {
    let root = slot_root("retained");
    write_slot(&root, "v1.0.0", "old chunk");
    write_slot(&root, "v1.1.0", "new chunk");
    let daemon = slot_daemon(&root).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));

    let stale = router
        .clone()
        .oneshot(get("/assets/index-v1.0.0.js"))
        .await
        .expect("router call");
    assert_eq!(stale.status(), StatusCode::OK);
    assert_eq!(body_string(stale).await, "old chunk");

    let live = router
        .oneshot(get("/assets/index-v1.1.0.js"))
        .await
        .expect("router call");
    assert_eq!(body_string(live).await, "new chunk");

    cleanup(&root);
}

/// An asset that exists nowhere is a `404`. Never the SPA shell: a tab that
/// asked for JavaScript and got `text/html` fails with a parse error that
/// says nothing about what happened.
#[tokio::test]
async fn a_hashed_asset_that_exists_nowhere_is_a_404_not_the_shell() {
    let root = slot_root("missing-asset");
    write_slot(&root, "v1.0.0", "chunk");
    let daemon = slot_daemon(&root).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));

    let response = router
        .oneshot(get("/assets/index-deadbeef.js"))
        .await
        .expect("router call");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        !content_type.starts_with("text/html"),
        "a missing asset came back as HTML: {content_type}"
    );

    cleanup(&root);
}

/// Without these a browser applies heuristic caching to `index.html`, pins
/// the tab to the old bundle, and a correct flip looks broken
/// (docs/18-ui-upgrades.md#cache-headers).
#[tokio::test]
async fn the_shell_revalidates_and_hashed_assets_are_immutable() {
    let root = slot_root("cache");
    write_slot(&root, "v1.0.0", "chunk");
    let daemon = slot_daemon(&root).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));

    let shell = router.clone().oneshot(get("/")).await.expect("router call");
    assert_eq!(
        shell.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache"
    );

    // A client-side route gets the shell too, and must carry the same rule.
    let route = router
        .clone()
        .oneshot(get("/sessions/01ARZ3NDEKTSV4RRFFQ69G5FAV"))
        .await
        .expect("router call");
    assert_eq!(
        route.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache"
    );

    let asset = router
        .oneshot(get("/assets/index-v1.0.0.js"))
        .await
        .expect("router call");
    assert_eq!(
        asset.headers().get(header::CACHE_CONTROL).unwrap(),
        "public, max-age=31536000, immutable"
    );

    cleanup(&root);
}

/// `/health`'s `ui_version` is what the app watches to offer a reload.
#[tokio::test]
async fn health_reports_the_slot_version_and_null_for_a_dev_tree() {
    let root = slot_root("health");
    write_slot(&root, "v1.2.3", "chunk");
    let daemon = slot_daemon(&root).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));

    let body = health_body(router.clone()).await;
    assert_eq!(body["ui_version"], "v1.2.3");

    // A `--web-dist` tree has no release version and must not invent one.
    let dist =
        std::env::temp_dir().join(format!("teleportd-web-dist-{}-health", std::process::id()));
    write_dist(&dist);
    let dev = support::spawn_with_web_dist(support::default_config(), Some(dist.clone())).await;
    let dev_router = teleportd::api::build_router(std::sync::Arc::clone(&dev.state));
    let dev_body = health_body(dev_router).await;
    assert!(dev_body["ui_version"].is_null());

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&dist);
    cleanup(&root);
}

async fn health_body(router: axum::Router) -> serde_json::Value {
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/health")
        .header(header::AUTHORIZATION, format!("Bearer {}", support::TOKEN))
        .body(Body::empty())
        .expect("valid request");
    let response = router.oneshot(request).await.expect("router call");
    serde_json::from_str(&body_string(response).await).expect("json body")
}

fn cleanup(root: &std::path::Path) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(root);
}
