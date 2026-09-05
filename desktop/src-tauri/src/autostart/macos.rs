//! `launchd` LaunchAgent (docs/08-packaging.md#autostart-at-login).
//! `~/Library/LaunchAgents/io.github.alehatsman.teleport.teleportd.plist`,
//! `RunAtLoad=true`, `KeepAlive` on crash -- not a Login Item, which doesn't
//! get `KeepAlive` semantics.

use std::fs;

use anyhow::{Context, Result};

const LABEL: &str = "io.github.alehatsman.teleport.teleportd";

fn agents_dir() -> Result<std::path::PathBuf> {
    let base = directories::BaseDirs::new().context("no home directory")?;
    Ok(base.home_dir().join("Library/LaunchAgents"))
}

fn plist_path() -> Result<std::path::PathBuf> {
    Ok(agents_dir()?.join(format!("{LABEL}.plist")))
}

pub fn install() -> Result<()> {
    let exe = crate::daemon::sidecar_path()?;
    let dir = agents_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>Crashed</key>
        <true/>
    </dict>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        exe = exe.display()
    );
    let path = plist_path()?;
    fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;

    let _ = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&path)
        .status();
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = plist_path()?;
    if path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", "-w"])
            .arg(&path)
            .status();
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// Real-hardware verification for the LaunchAgent
/// (docs/11-mvp-plan.md#m10, [#40](https://github.com/alehatsman/teleport/issues/40)).
/// Drives the *real* [`install`]/[`uninstall`] against the real
/// `~/Library/LaunchAgents` and a real `launchctl`, with a real `teleportd`
/// -- not a reimplementation of the plist, which is the whole point: this
/// file has always *looked* right, and looking right is what needed
/// checking.
///
/// **What a measurement run found (2026-09-06, Darwin 24.6.0 arm64).** Which
/// deaths `KeepAlive { Crashed: true }` actually covers is narrower than
/// "the daemon died":
///
/// ```text
///   SIGKILL   died 0.1s   no relaunch within 40s
///   SIGABRT   died 0.1s   relaunched after 9.9s
///   SIGSEGV   still alive after 40s (see below)
///   SIGTERM   died 0.1s   no relaunch within 40s
/// ```
///
/// * **SIGKILL is not a "crash" to launchd.** `kill -9`, a force-quit, or an
///   OOM kill leaves the daemon down until the next login. That is a real
///   coverage gap, documented rather than papered over -- the alternative,
///   an unconditional `KeepAlive`, would also resurrect the daemon after the
///   tray's deliberate "Stop daemon", which is worse.
/// * **SIGTERM correctly does *not* relaunch**, so the tray's graceful stop
///   (`daemon::terminate_gracefully`) is not undone by having autostart
///   enabled. That interaction is the one this file could most plausibly
///   have got wrong, and it is right. `stop_is_not_undone_by_the_agent`
///   below is the regression test.
/// * A Rust **panic** exits 101 -- an exit code, not a signal -- and this
///   crate builds with the default `panic = "unwind"`, so launchd would not
///   relaunch that either. Inferred from `launchd.plist(5)`'s definition of
///   `Crashed` and consistent with the SIGKILL result above; not separately
///   measured, because the plist runs `teleportd` with no arguments and
///   there is no way to make it exit non-zero from outside.
/// * **SIGSEGV is useless as a test signal here**: macOS's crash reporter
///   suspends the process while it collects a report, so the target was
///   still alive 40s later and nothing could be concluded. `SIGABRT` is what
///   the crash test uses.
///
/// `#[ignore]`d, and for stronger reasons than the Windows suite's:
///
/// * it needs `daemon/target/release/teleportd` prebuilt (see
///   [`real_teleportd_path`]) -- `cargo test` kicking off another crate's
///   release build would make failures here hard to attribute;
/// * `~/Library/LaunchAgents/<LABEL>.plist` is a **single global path per
///   user**, so these must run serially (`--test-threads=1`), and they refuse
///   to run at all if a real installed agent is already sitting there;
/// * `launchctl` needs a real GUI login session. A CI runner or a bare `ssh`
///   session is not one, and a green run there would mean nothing;
/// * they are slow on purpose -- launchd throttles respawns to ~10s, so the
///   crash test cannot be hurried.
///
/// Run: `cargo test -- --ignored --test-threads=1` from `desktop/src-tauri`,
/// in a real terminal on a real logged-in Mac.
#[cfg(all(test, target_os = "macos"))]
mod launchagent_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// launchd's respawn throttle is ~10s (measured: 9.9s), so every "did it
    /// come back" wait has to clear that with room to spare.
    const RELAUNCH_WINDOW: Duration = Duration::from_secs(40);

    /// Both tests own the same global plist path and the same TCP port, so
    /// they cannot interleave. Same pattern and reason as `daemon.rs`'s
    /// `job_breakaway_tests::SERIAL`.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// `../../daemon/target/release/teleportd`, the exact binary
    /// `scripts/copy-sidecar.sh` stages for a real bundle. Built by hand
    /// first, deliberately not by this test.
    fn real_teleportd_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../daemon/target/release/teleportd")
    }

    /// Stages the real daemon where [`sidecar_path`] looks for it: next to
    /// *this test binary's own* `current_exe()`. From here on, `install()`
    /// writes a plist pointing at the genuine `teleportd`.
    fn stage_real_sidecar() {
        let real = real_teleportd_path();
        assert!(
            real.exists(),
            "real teleportd not found at {} -- build it first: \
             (cd daemon && cargo build --release)",
            real.display()
        );
        let dest = crate::daemon::sidecar_path().expect("sidecar_path");
        fs::copy(&real, &dest)
            .unwrap_or_else(|e| panic!("staging {} -> {}: {e}", real.display(), dest.display()));
    }

    /// Refuse to touch a plist this test did not write. A developer with
    /// autostart genuinely enabled must not have it silently replaced and
    /// then deleted by a test run.
    fn refuse_to_clobber() {
        let path = plist_path().expect("plist_path");
        assert!(
            !path.exists(),
            "{} already exists -- this test would overwrite and then delete a real \
             installed agent. Remove it deliberately first if that is what you want.",
            path.display()
        );
    }

    /// `GET /api/v1/health` on the daemon's default port, no credential (the
    /// one route that needs none, docs/04-api-protocol.md). Hand-rolled over
    /// `TcpStream` rather than pulling an HTTP client into this crate's
    /// dev-dependencies for two asserts.
    fn healthy() -> bool {
        let Ok(mut s) = TcpStream::connect_timeout(
            &"127.0.0.1:7337".parse().unwrap(),
            Duration::from_millis(500),
        ) else {
            return false;
        };
        if s.set_read_timeout(Some(Duration::from_secs(2))).is_err() {
            return false;
        }
        if s.write_all(
            b"GET /api/v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .is_err()
        {
            return false;
        }
        let mut buf = String::new();
        s.read_to_string(&mut buf).is_ok() && buf.starts_with("HTTP/1.1 200")
    }

    fn wait_for_health(up: bool, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if healthy() == up {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// The pid of the running daemon, via `pgrep` -- `/api/v1/health` only
    /// reports a pid on an *authenticated* response, and this test has no
    /// token.
    fn daemon_pid() -> Option<u32> {
        let out = std::process::Command::new("pgrep")
            .args(["-x", "teleportd"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }

    /// `kill(pid, 0)` -- does this pid still exist? Used to prove the target
    /// actually died before asking whether launchd brought it back; without
    /// it, "no relaunch" and "never died" look identical.
    fn pid_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Uninstalls on drop, including on an unwinding panic -- an end-of-test
    /// call would never fire if an earlier assert panicked, and what it
    /// leaks is a LaunchAgent that starts a daemon at every login.
    struct UninstallOnDrop;
    impl Drop for UninstallOnDrop {
        fn drop(&mut self) {
            let _ = uninstall();
            // `uninstall()` unloads the agent; make sure the daemon it
            // started is gone too, so the next test starts from nothing.
            let _ = std::process::Command::new("pkill")
                .args(["-x", "teleportd"])
                .output();
        }
    }

    /// Installs, and leaves a serving daemon behind. Shared setup, and the
    /// first test's whole subject.
    fn install_and_wait() -> UninstallOnDrop {
        refuse_to_clobber();
        stage_real_sidecar();
        let cleanup = UninstallOnDrop;
        install().expect("install");
        assert!(
            wait_for_health(true, Duration::from_secs(15)),
            "RunAtLoad did not bring a serving daemon up within 15s"
        );
        cleanup
    }

    /// `install()` writes a plist launchd accepts, registers it in *this
    /// user's* GUI domain, and `RunAtLoad` actually brings the daemon up and
    /// serving.
    #[test]
    #[ignore]
    fn install_registers_an_agent_that_starts_the_daemon() {
        let _serial = SERIAL.lock().unwrap();
        let _cleanup = install_and_wait();

        let path = plist_path().expect("plist_path");
        assert!(path.exists(), "install() wrote no plist");

        // launchd's own parser, not ours: a plist we can read and it cannot
        // is still a broken plist.
        let lint = std::process::Command::new("plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .expect("plutil");
        assert!(
            lint.status.success(),
            "plutil -lint rejected the plist: {}",
            String::from_utf8_lossy(&lint.stdout)
        );

        // `gui/<uid>/` is the per-login-session domain -- the thing that
        // makes this an agent that starts at login rather than a system
        // daemon that starts at boot (#40).
        let uid = unsafe { libc::getuid() };
        let printed = std::process::Command::new("launchctl")
            .arg("print")
            .arg(format!("gui/{uid}/{LABEL}"))
            .output()
            .expect("launchctl print");
        assert!(
            printed.status.success(),
            "launchctl does not know about {LABEL} in gui/{uid}: {}",
            String::from_utf8_lossy(&printed.stderr)
        );
    }

    /// `KeepAlive { Crashed: true }` restarts a daemon that died on a crash
    /// signal. SIGABRT, not SIGKILL: see this module's doc comment for what
    /// launchd does and does not count as a crash.
    #[test]
    #[ignore]
    fn keepalive_relaunches_after_a_crash() {
        let _serial = SERIAL.lock().unwrap();
        let _cleanup = install_and_wait();

        let first = daemon_pid().expect("a running daemon has a pid");
        assert_eq!(
            unsafe { libc::kill(first as libc::pid_t, libc::SIGABRT) },
            0,
            "SIGABRT failed: {}",
            std::io::Error::last_os_error()
        );

        let t0 = Instant::now();
        let mut second = None;
        while t0.elapsed() < RELAUNCH_WINDOW {
            if let Some(pid) = daemon_pid() {
                if pid != first {
                    second = Some(pid);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let second = second.unwrap_or_else(|| {
            panic!(
                "launchd did not relaunch the crashed daemon within {}s (still alive: {})",
                RELAUNCH_WINDOW.as_secs(),
                pid_alive(first)
            )
        });
        assert_ne!(first, second, "a relaunch must be a new process");
        assert!(
            wait_for_health(true, Duration::from_secs(10)),
            "the relaunched daemon is not serving"
        );
    }

    /// The interaction this file could most plausibly have got wrong: the
    /// tray's "Stop daemon" sends SIGTERM (`daemon::terminate_gracefully`),
    /// and an over-eager `KeepAlive` would bring the daemon straight back,
    /// making the menu item look broken. It must stay stopped.
    #[test]
    #[ignore]
    fn stop_is_not_undone_by_the_agent() {
        let _serial = SERIAL.lock().unwrap();
        let _cleanup = install_and_wait();

        let pid = daemon_pid().expect("a running daemon has a pid");
        assert_eq!(
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) },
            0,
            "SIGTERM failed: {}",
            std::io::Error::last_os_error()
        );
        assert!(
            wait_for_health(false, Duration::from_secs(15)),
            "the daemon kept serving after a graceful SIGTERM"
        );

        // Well past launchd's ~10s throttle: if it were going to come back,
        // it would have by now.
        let deadline = Instant::now() + RELAUNCH_WINDOW;
        while Instant::now() < deadline {
            if let Some(new) = daemon_pid() {
                assert_eq!(
                    new, pid,
                    "the agent relaunched the daemon ({new}) after a deliberate stop -- \
                     the tray's Stop daemon would look broken"
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
