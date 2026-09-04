//! Black-box M7 gate (docs/11-mvp-plan.md#m7--sqlite-metadata-and-recovery):
//! "SIGKILL the daemon mid-session; on restart the session reads `lost`, the
//! log is complete and readable, and `output_bytes` matches the file."
//!
//! Same style as `skeleton.rs`: spawns the real `teleportd` binary against a
//! temp data dir and drives it externally. The other M7 persistence/recovery
//! cases (stale-row recovery, `output_bytes` reconciliation, capped-log
//! column-wins) are unit-tested directly against `persistence::Db` in
//! `persistence.rs` -- this file exists for the one thing those can't cover:
//! that it actually holds true across a real process boundary, driven
//! through the real HTTP API. No HTTP client dependency for one test file --
//! a couple of hand-rolled HTTP/1.1 requests over a raw `TcpStream` are
//! simpler than pulling one in (`serde_json` is already a normal dependency,
//! so JSON parsing doesn't need the same justification).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_teleportd"))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "teleportd-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Kills the child unconditionally when dropped, so a failing assertion
/// never leaves a daemon running.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_daemon(data_dir: &Path) -> KillOnDrop {
    KillOnDrop(
        Command::new(bin())
            .args([
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--listen",
                "127.0.0.1:0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn teleportd"),
    )
}

fn read_port(data_dir: &Path) -> u16 {
    assert!(
        wait_for_file(&data_dir.join("port"), Duration::from_secs(5)),
        "port file never appeared"
    );
    std::fs::read_to_string(data_dir.join("port"))
        .unwrap()
        .trim()
        .parse()
        .expect("port file")
}

fn read_token(data_dir: &Path) -> String {
    std::fs::read_to_string(data_dir.join("token"))
        .unwrap()
        .trim()
        .to_string()
}

/// One small blocking HTTP/1.1 request/response against `127.0.0.1:<port>`:
/// no chunked encoding, no keep-alive, no TLS -- all `teleportd`'s API needs
/// from a test. Returns the status code and the parsed JSON body (`Value` on
/// a `GET`/`POST` that returns a body; `Value::Null` on a bodyless response
/// like `204`).
fn http(port: u16, method: &str, path: &str, token: &str, body: Option<&Value>) -> (u16, Value) {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to teleportd");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let response = String::from_utf8_lossy(&response);
    let mut parts = response.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or_default();
    let raw_body = parts.next().unwrap_or_default();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let body = if raw_body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(raw_body).expect("JSON body")
    };
    (status, body)
}

#[cfg(unix)]
#[test]
fn sigkill_mid_session_recovers_as_lost_with_a_readable_log() {
    let data_dir = temp_dir("sigkill-recovery");

    let child = spawn_daemon(&data_dir);
    let port = read_port(&data_dir);
    let token = read_token(&data_dir);

    // A session that keeps producing output until killed.
    let create_body = json!({
        "kind": "shell",
        "command": "/bin/sh",
        "args": ["-c", "yes hello"],
        "cwd": std::env::temp_dir().to_string_lossy(),
        "cols": 80,
        "rows": 24,
    });
    let (status, created) = http(port, "POST", "/api/v1/sessions", &token, Some(&create_body));
    assert_eq!(status, 201, "create failed: {created:?}");
    let id = created["id"].as_str().expect("id").to_string();

    // Give the reader loop time to actually append bytes before killing.
    std::thread::sleep(Duration::from_millis(300));

    let log_path = data_dir.join("sessions").join(&id).join("output.vt");
    assert!(
        wait_for_file(&log_path, Duration::from_secs(2)),
        "output.vt was never created"
    );
    // SIGKILL, not the graceful `terminate()` path -- this is the crash
    // docs/01-architecture.md#the-crash-boundary describes, not a clean
    // shutdown. Poll briefly for bytes to land rather than a fixed sleep.
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0) == 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let file_len_before_kill = std::fs::metadata(&log_path).expect("output.vt").len();
    assert!(
        file_len_before_kill > 0,
        "the session must have actually produced output before the kill"
    );

    // SAFETY: sending SIGKILL to a child process this test just spawned and owns.
    unsafe {
        libc::kill(child.0.id() as libc::pid_t, libc::SIGKILL);
    }
    // `KillOnDrop` will also reap it; wait here so the port is free before
    // the next spawn tries to reuse the same data dir.
    let mut child = child;
    let _ = child.0.wait();
    drop(child);
    // SIGKILL skips `remove_port_file` entirely -- the port file from the
    // killed process is still sitting there naming a now-dead port.
    // `wait_for_file` below would otherwise see it as "already there" and
    // `read_port` would hand back that stale, unlistened-on number instead
    // of waiting for the restarted daemon's real one.
    let _ = std::fs::remove_file(data_dir.join("port"));

    // Restart against the same data dir -- a fresh port and (for this run
    // only) a fresh process, same token (persisted, docs/05-persistence.md#layout).
    let _child2 = spawn_daemon(&data_dir); // KillOnDrop: torn down at the end of the test
    let port2 = read_port(&data_dir);
    let token2 = read_token(&data_dir);
    assert_eq!(token, token2, "the credential must survive a restart");

    let (status, view) = http(
        port2,
        "GET",
        &format!("/api/v1/sessions/{id}"),
        &token2,
        None,
    );
    assert_eq!(
        status, 200,
        "the session must still be resolvable after restart: {view:?}"
    );
    assert_eq!(
        view["state"], "lost",
        "a SIGKILLed session must read back as lost: {view:?}"
    );
    assert_eq!(view["lost_reason"], "daemon_restart");

    let file_len_after_restart = std::fs::metadata(&log_path).expect("output.vt").len();
    assert!(
        file_len_after_restart >= file_len_before_kill,
        "the file on disk must not have shrunk between the kill and the restart"
    );
    assert_eq!(
        view["output_bytes"].as_u64(),
        Some(file_len_after_restart),
        "output_bytes must match the file exactly after recovery reconciles them"
    );

    // `/log` returns raw octets, not JSON -- `http()` above can't parse
    // this response body, so read it over a fresh raw `TcpStream` instead.
    let mut stream = TcpStream::connect(("127.0.0.1", port2)).unwrap();
    stream
        .write_all(
            format!(
                "GET /api/v1/sessions/{id}/log HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token2}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header/body split");
    let status_line = String::from_utf8_lossy(&raw[..split]);
    assert!(
        status_line.starts_with("HTTP/1.1 200"),
        "log fetch must succeed: {status_line}"
    );
    let log_bytes = &raw[split + 4..];
    assert_eq!(
        log_bytes.len() as u64,
        file_len_after_restart,
        "the full log must be readable after recovery, not truncated"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}
