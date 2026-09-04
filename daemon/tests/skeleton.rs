//! Black-box tests for the M0 skeleton: spawn the real `teleportd` binary
//! against a temp data dir and check its externally-observable behavior —
//! the files it writes, their permissions, and what it prints. No HTTP
//! surface exists yet (that's M4), so process + filesystem is the interface.

use std::io::{BufRead, BufReader};
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
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

// Not worth pulling in a dependency for test-only uniqueness.
fn ulid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
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

#[test]
fn first_run_creates_expected_files_and_prints_url() {
    let data_dir = temp_dir("first-run");

    let mut child = KillOnDrop(
        Command::new(bin())
            .args(["--data-dir", data_dir.to_str().unwrap(), "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn teleportd"),
    );

    let port_file = data_dir.join("port");
    assert!(wait_for_file(&port_file, Duration::from_secs(5)), "port file never appeared");

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
    assert_eq!(token.len(), 64, "256 bits hex-encoded is 64 chars: {token:?}");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));

    let stdout = child.0.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), format!("http://127.0.0.1:{port}/?token={token}"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&data_dir), 0o700);
        assert_eq!(mode(&port_file), 0o600);
        assert_eq!(mode(&data_dir.join("token")), 0o600);
    }

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn device_id_and_token_survive_a_restart() {
    let data_dir = temp_dir("restart");

    let run_once = |data_dir: &Path| -> (String, String) {
        let mut child = KillOnDrop(
            Command::new(bin())
                .args(["--data-dir", data_dir.to_str().unwrap(), "--listen", "127.0.0.1:0"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn teleportd"),
        );
        assert!(wait_for_file(&data_dir.join("port"), Duration::from_secs(5)));
        let device = std::fs::read_to_string(data_dir.join("device.json")).unwrap();
        let token = std::fs::read_to_string(data_dir.join("token")).unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        (device, token)
    };

    let (device1, token1) = run_once(&data_dir);
    let (device2, token2) = run_once(&data_dir);

    assert_eq!(device1, device2, "device.json must not change across restarts");
    assert_eq!(token1, token2, "token must not change across restarts");

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
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn teleportd"),
    );

    let port_file = data_dir.join("port");
    assert!(wait_for_file(&port_file, Duration::from_secs(5)));
    let bound_port: u16 = std::fs::read_to_string(&port_file).unwrap().trim().parse().unwrap();

    assert_ne!(bound_port, held_port, "should not have bound the already-held port");

    drop(holder);
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn refuses_non_loopback_without_the_escape_hatch() {
    let data_dir = temp_dir("non-loopback");

    let status = Command::new(bin())
        .args(["--data-dir", data_dir.to_str().unwrap(), "--listen", "0.0.0.0:0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run teleportd");

    assert!(!status.success());
    assert!(!data_dir.join("port").exists(), "must not have bound anything");

    let _ = std::fs::remove_dir_all(&data_dir);
}

#[cfg(unix)]
#[test]
fn sigterm_triggers_graceful_shutdown_and_removes_the_port_file() {
    let data_dir = temp_dir("sigterm");

    let mut child = KillOnDrop(
        Command::new(bin())
            .args(["--data-dir", data_dir.to_str().unwrap(), "--listen", "127.0.0.1:0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn teleportd"),
    );

    let port_file = data_dir.join("port");
    assert!(wait_for_file(&port_file, Duration::from_secs(5)));

    // SAFETY: sending SIGTERM to a child process we just spawned and own.
    unsafe {
        libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM);
    }

    let status = child.0.wait().expect("wait for child");
    assert!(status.success(), "graceful shutdown should exit 0, got {status:?}");
    assert!(!port_file.exists(), "port file should be removed on clean shutdown");

    let _ = std::fs::remove_dir_all(&data_dir);
}
