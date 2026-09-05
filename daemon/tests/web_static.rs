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
        .unwrap()
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

fn write_dist(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("index.html"), "<html>spa shell</html>").unwrap();
    std::fs::write(dir.join("assets/app.js"), "console.log('app')").unwrap();
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

    std::fs::remove_dir_all(&dist).ok();
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

    std::fs::remove_dir_all(&dist).ok();
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

    std::fs::remove_dir_all(&dist).ok();
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

    std::fs::remove_dir_all(&dist).ok();
}
