//! Black-box tests for the M0 skeleton: spawn the real `teleportd` binary
//! against a temp data dir and check its externally-observable behavior —
//! the files it writes, their permissions, and what it prints. No HTTP
//! surface exists yet (that's M4), so process + filesystem is the interface.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_teleportd"))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "teleportd-test-{name}-{}-{}",
        std::process::id(),
        ulid_like()
    ));
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

// Not worth pulling in a dependency for test-only uniqueness.
fn ulid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_nanos()
}

/// How long a spawned `teleportd` gets to boot and write its `port` file.
///
/// A condition wait, not a volume budget
/// (docs/10-testing.md#deadlines-preconditions-and-premises): it fails only if
/// the daemon never arrives, so a slow box arrives later rather than going red
/// and the generous number costs nothing on the passing path.
///
/// 30s matches `restart_recovery.rs`, which took this same fix first -- both of
/// its tests failed together at "port file never appeared" under a 5s budget
/// during a full parallel `cargo test`, 3/3 green standalone. This file kept
/// the 5s and produced the identical signature on a cold `windows-latest`
/// runner: the three daemon-spawning tests red at once, green on a rerun of the
/// same commit (#123). Spawning a debug-build daemon is a startup, and a
/// startup alongside 20-odd other test binaries is not a 5s operation.
const DAEMON_WAIT: Duration = Duration::from_secs(30);

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
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort cleanup of a process this test owns; nothing to do if it fails"
        )]
        let _ = self.0.kill();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort cleanup of a process this test owns; nothing to do if it fails"
        )]
        let _ = self.0.wait();
    }
}

impl KillOnDrop {
    /// Kills the child and renders its captured output for a failure message.
    /// Takes the pipe handles, so it is for failure paths only -- a caller that
    /// still wants to read the daemon's output must not have called this.
    fn kill_and_report(&mut self) -> String {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort cleanup of a process this test owns; nothing to do if it fails"
        )]
        let _ = self.0.kill();
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort cleanup of a process this test owns; nothing to do if it fails"
        )]
        let _ = self.0.wait();
        format!(
            "--- daemon stdout ---\n{}\n--- daemon stderr ---\n{}",
            drain(self.0.stdout.take()),
            drain(self.0.stderr.take())
        )
    }
}

/// Reads a dead child's pipe to the end, naming the empty cases rather than
/// returning a blank that reads as "it printed nothing".
fn drain<R: Read>(stream: Option<R>) -> String {
    let Some(mut stream) = stream else {
        return "<not piped>".to_owned();
    };
    let mut buf = String::new();
    match stream.read_to_string(&mut buf) {
        Err(e) => format!("<unreadable: {e}>"),
        Ok(_) if buf.trim().is_empty() => "<empty>".to_owned(),
        Ok(_) => buf,
    }
}

/// Waits for a spawned daemon's `port` file; on timeout kills it and panics
/// with whatever it managed to write first.
///
/// The bare `assert!(wait_for_file(..))` this replaces said exactly `port file
/// never appeared` whether the daemon panicked at startup, failed to bind, or
/// was merely slow. Those three are the whole diagnosis and they were
/// indistinguishable from CI logs, which is why #123 needed a rerun of the same
/// commit to classify at all. Every spawn feeding this helper pipes both
/// streams so there is something to print.
fn wait_for_port_file(child: &mut KillOnDrop, port_file: &Path) {
    // `assert!` rather than `panic!` in an `if`: clippy's panic-surface lints
    // count a non-`#[test]` helper in tests/ as production code
    // (daemon/clippy.toml). The message arguments are evaluated only when the
    // condition fails, so the child is killed and drained on that path alone.
    assert!(
        wait_for_file(port_file, DAEMON_WAIT),
        "port file {} never appeared within {DAEMON_WAIT:?}\n{}",
        port_file.display(),
        child.kill_and_report()
    );
}

#[test]
fn first_run_creates_expected_files_and_prints_url() {
    let data_dir = temp_dir("first-run");

    let mut child = KillOnDrop(
        Command::new(bin())
            .args([
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--listen",
                "127.0.0.1:0",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn teleportd"),
    );

    let port_file = data_dir.join("port");
    wait_for_port_file(&mut child, &port_file);

    assert!(data_dir.join("device.json").exists());
    assert!(data_dir.join("token").exists());

    let port: u16 = std::fs::read_to_string(&port_file)
        .unwrap()
        .trim()
        .parse()
        .expect("port file should contain a bare port number");
    assert_ne!(port, 0);

    let token = std::fs::read_to_string(data_dir.join("token")).unwrap();
    let token = token.trim();
    assert_eq!(
        token.len(),
        64,
        "256 bits hex-encoded is 64 chars: {token:?}"
    );
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    let stdout = child.0.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    // `localhost`, not `127.0.0.1`: a WebAuthn relying-party ID cannot be
    // an IP address, so the URL the user actually opens has to be the one a
    // passkey can be enrolled against (docs/17-passkey-login.md, and
    // main.rs::url_host). The bind address is unchanged and still loopback.
    assert_eq!(
        line.trim(),
        format!("http://localhost:{port}/?token={token}")
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&data_dir), 0o700);
        assert_eq!(mode(&port_file), 0o600);
        assert_eq!(mode(&data_dir.join("token")), 0o600);
    }

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn device_id_and_token_survive_a_restart() {
    let data_dir = temp_dir("restart");

    let run_once = |data_dir: &Path| -> (String, String) {
        let mut child = KillOnDrop(
            Command::new(bin())
                .args([
                    "--data-dir",
                    data_dir.to_str().unwrap(),
                    "--listen",
                    "127.0.0.1:0",
                ])
                // Piped, not null: `wait_for_port_file` prints these on a
                // boot timeout, and null left it nothing to say (#123).
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn teleportd"),
        );
        wait_for_port_file(&mut child, &data_dir.join("port"));
        let device = std::fs::read_to_string(data_dir.join("device.json")).unwrap();
        let token = std::fs::read_to_string(data_dir.join("token")).unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        (device, token)
    };

    let (device1, token1) = run_once(&data_dir);
    let (device2, token2) = run_once(&data_dir);

    assert_eq!(
        device1, device2,
        "device.json must not change across restarts"
    );
    assert_eq!(token1, token2, "token must not change across restarts");

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn falls_back_to_an_ephemeral_port_when_the_configured_one_is_taken() {
    let data_dir = temp_dir("port-fallback");

    // Reserve a real port and hold it open so the daemon's configured port
    // collides.
    let holder = TcpListener::bind("127.0.0.1:0").expect("bind a port to hold");
    let held_port = holder.local_addr().unwrap().port();

    let mut child = KillOnDrop(
        Command::new(bin())
            .args([
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--listen",
                &format!("127.0.0.1:{held_port}"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn teleportd"),
    );

    let port_file = data_dir.join("port");
    wait_for_port_file(&mut child, &port_file);
    let bound_port: u16 = std::fs::read_to_string(&port_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    assert_ne!(
        bound_port, held_port,
        "should not have bound the already-held port"
    );

    drop(holder);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn refuses_non_loopback_without_the_escape_hatch() {
    let data_dir = temp_dir("non-loopback");

    let status = Command::new(bin())
        .args([
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--listen",
            "0.0.0.0:0",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run teleportd");

    assert!(!status.success());
    assert!(
        !data_dir.join("port").exists(),
        "must not have bound anything"
    );

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[cfg(unix)]
#[test]
fn sigterm_triggers_graceful_shutdown_and_removes_the_port_file() {
    let data_dir = temp_dir("sigterm");

    let mut child = KillOnDrop(
        Command::new(bin())
            .args([
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--listen",
                "127.0.0.1:0",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn teleportd"),
    );

    let port_file = data_dir.join("port");
    wait_for_port_file(&mut child, &port_file);

    #[expect(
        unsafe_code,
        reason = "kill(2) via libc; no safe wrapper for signaling an arbitrary pid"
    )]
    // SAFETY: sending SIGTERM to a child process we just spawned and own.
    unsafe {
        libc::kill(
            libc::pid_t::try_from(child.0.id()).expect("a real OS pid fits pid_t"),
            libc::SIGTERM,
        );
    }

    let status = child.0.wait().expect("wait for child");
    assert!(
        status.success(),
        "graceful shutdown should exit 0, got {status:?}"
    );
    assert!(
        !port_file.exists(),
        "port file should be removed on clean shutdown"
    );

    #[expect(
        clippy::let_underscore_must_use,
        reason = "best-effort test cleanup; nothing to do if it fails"
    )]
    let _ = std::fs::remove_dir_all(&data_dir);
}
