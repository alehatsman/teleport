//! `POST /api/v1/shutdown` (docs/04-api-protocol.md#post-apiv1shutdown,
//! docs/11-mvp-plan.md#m10, issue #12) -- the cross-platform trigger added
//! because Windows has no reachable SIGTERM equivalent for a console-less,
//! autostart-launched `teleportd` (`GenerateConsoleCtrlEvent` needs a shared
//! console the daemon never has).
//!
//! No `#![cfg(unix)]` gate here, unlike `http_api.rs` -- nothing below spawns
//! a shell. The auth/origin fixtures drive `api.rs`'s router in-process
//! (same `support::spawn` harness as `http_api.rs`); the last fixture spawns
//! the *real* `teleportd` binary and drives it exactly the way a curl-based
//! Windows caller would, so "this stops the daemon" is checked against an
//! actual process exit, not just the `Notify` wiring.

mod support;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

fn shutdown_request(
    host: Option<&str>,
    origin: Option<&str>,
    token: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder().method("POST").uri("/api/v1/shutdown");
    if let Some(host) = host {
        builder = builder.header(header::HOST, host);
    }
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn missing_token_is_rejected_and_does_not_notify() {
    let daemon = support::spawn(support::default_config()).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = router
        .oneshot(shutdown_request(Some("127.0.0.1"), None, None))
        .await
        .expect("router call");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A bad credential must not shut anything down -- race the (unfired)
    // Notify against a short timeout instead of just trusting the status
    // code alone.
    let fired = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        daemon.state.shutdown.notified(),
    )
    .await;
    assert!(
        fired.is_err(),
        "an unauthorized request must not wake shutdown_signal()"
    );
}

#[tokio::test]
async fn bad_origin_is_rejected_and_does_not_notify() {
    let daemon = support::spawn(support::default_config()).await;
    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = router
        .oneshot(shutdown_request(
            Some("127.0.0.1"),
            Some("https://evil.example"),
            Some(support::TOKEN),
        ))
        .await
        .expect("router call");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let fired = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        daemon.state.shutdown.notified(),
    )
    .await;
    assert!(
        fired.is_err(),
        "a bad-Origin request must not wake shutdown_signal()"
    );
}

#[tokio::test]
async fn valid_request_is_accepted_and_wakes_the_shutdown_trigger() {
    let daemon = support::spawn(support::default_config()).await;

    // Subscribe to the trigger *before* the request -- `Notify::notified()`
    // registers interest on `.await`, and a permit stored by an earlier
    // `notify_one()` (there is none yet here) is only ever consumed by the
    // next `.await`, never lost -- so ordering here just avoids a spurious
    // hang, it isn't masking a race.
    let notified = daemon.state.shutdown.notified();

    let router = teleportd::api::build_router(std::sync::Arc::clone(&daemon.state));
    let response = router
        .oneshot(shutdown_request(
            Some("127.0.0.1"),
            None,
            Some(support::TOKEN),
        ))
        .await
        .expect("router call");
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "shutting_down");

    tokio::time::timeout(std::time::Duration::from_secs(2), notified)
        .await
        .expect("a valid POST /api/v1/shutdown must wake shutdown_signal()'s select");
}

/// The real end-to-end proof: spawns the actual `teleportd` binary (not the
/// in-process router), reads its own startup line for the port/token exactly
/// as a real client would, sends a plain HTTP/1.1 request over a raw
/// `TcpStream` (deliberately not a library -- this is a request simple
/// enough that hand-writing it keeps the dependency list boring, and it
/// doubles as the "testable with curl" claim from the issue), and waits for
/// the *process* to exit. This is the fixture that actually answers issue
/// #12's question for Windows: run for real, not just wired up.
#[test]
fn post_shutdown_stops_the_real_daemon_process() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let data_dir = std::env::temp_dir().join(format!(
        "teleportd-shutdown-e2e-{}-{}",
        std::process::id(),
        ulid::Ulid::new()
    ));

    let exe = env!("CARGO_BIN_EXE_teleportd");
    let mut child = Command::new(exe)
        .args([
            "--data-dir",
            data_dir.to_str().expect("temp dir path is valid UTF-8"),
            "--listen",
            "127.0.0.1:0",
            "--log-level",
            "warn",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the real teleportd binary (CARGO_BIN_EXE_teleportd)");

    // main.rs: "Logs go to stderr; stdout carries only the startup URL
    // below" -- `http://<ip>:<port>/?token=<token>`, one line, on success.
    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut url_line = String::new();
    reader
        .read_line(&mut url_line)
        .expect("read startup URL line from teleportd's stdout");
    let url_line = url_line.trim();

    let without_scheme = url_line
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("unexpected startup line: {url_line:?}"));
    let (host_port, query) = without_scheme
        .split_once('/')
        .expect("startup URL has a path");
    let port: u16 = host_port
        .rsplit(':')
        .next()
        .expect("host:port")
        .parse()
        .expect("port is numeric");
    let token = query
        .strip_prefix("?token=")
        .expect("startup URL carries ?token=");

    let request = format!(
        "POST /api/v1/shutdown HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\r\n"
    );

    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).expect("connect to the real daemon's bound port");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(request.as_bytes())
        .expect("write the raw HTTP/1.1 request");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the HTTP response");
    let status_line = response.lines().next().unwrap_or_default();
    assert!(
        status_line.contains("202"),
        "expected 202 Accepted, got: {status_line:?}"
    );

    // Poll for the *process* to exit -- the actual claim this test makes.
    // Bounded by the documented termination policy plus margin, same
    // reasoning as pty_primitive*.rs's terminate fixtures.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("teleportd did not exit within 10s of POST /api/v1/shutdown");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(
        status.success(),
        "teleportd should exit cleanly after a graceful shutdown, got {status:?}"
    );

    // main.rs's `remove_port_file` runs after the graceful-shutdown future
    // resolves -- its absence is one more confirmation the real shutdown
    // path ran to completion, not just that the process died some other way.
    assert!(
        !data_dir.join("port").exists(),
        "the port file should be removed on clean shutdown"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}
