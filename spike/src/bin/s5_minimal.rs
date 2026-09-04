//! S5 spike — W1 follow-up: is the "graceful exit never observed" hang specific to
//! cmd.exe, or does it happen for ANY process attached to a ConPTY that exits on its
//! own?
//!
//! s1_reaper's exit0/exit7 scenarios run `cmd.exe /c "exit N"` under ConPTY and never
//! see wait()/try_wait() return. s0_control proved the identical `cmd.exe /c "exit 0"`
//! reaps fine with NO ConPTY involved. This binary removes cmd.exe from the equation
//! entirely: it spawns `mini_exit(.exe)` -- a trivial Rust binary that does nothing
//! but print one line and call std::process::exit(N) -- under ConPTY instead.
//!
//! If mini_exit ALSO never gets reaped: the hang is general to "any process exiting
//! on its own while attached to a ConPTY", not cmd.exe-specific -- points at ConPTY /
//! the console subsystem's exit signaling itself.
//!
//! If mini_exit reaps fine: the hang is specific to cmd.exe's own exit path under
//! ConPTY (e.g. its console-detach handling) -- a narrower, more actionable finding.
//!
//! Usage: s5_minimal <exit0|exit7|sigkill>

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn mini_exit_path() -> Result<PathBuf> {
    let mut path = std::env::current_exe().context("current_exe")?;
    path.pop(); // drop this binary's own filename
    let name = if cfg!(windows) { "mini_exit.exe" } else { "mini_exit" };
    path.push(name);
    anyhow::ensure!(path.exists(), "expected {path:?} to exist -- build the whole spike crate first");
    Ok(path)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("exit0");

    let mini_exit = mini_exit_path()?;

    let system = native_pty_system();
    let pair = system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new(&mini_exit);
    match scenario {
        "exit0" => cmd.arg("0"),
        "exit7" => cmd.arg("7"),
        "sigkill" => cmd.arg("0"), // will be killed before it gets a chance to exit on its own
        other => panic!("unknown scenario {other}"),
    };

    let mut child = pair.slave.spawn_command(cmd)?;
    #[cfg(unix)]
    drop(pair.slave);

    let pid = child.process_id();
    eprintln!("[s5] scenario={scenario} exe={mini_exit:?} pid={pid:?}");

    // Drain output in the background, same hygiene as every other spike binary --
    // but also log EOF with a timestamp. Does the master ever see EOF for a
    // voluntarily-exiting child even when wait() doesn't return? That's a second,
    // independent exit signal we could fall back to if wait() alone can't be
    // trusted on Windows.
    let t0 = Instant::now();
    let mut reader = pair.master.try_clone_reader()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    eprintln!("[s5] reader EOF at {}ms", t0.elapsed().as_millis());
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[s5] reader error at {}ms: {e}", t0.elapsed().as_millis());
                    break;
                }
            }
        }
    });

    let trigger = Instant::now();
    if scenario == "sigkill" {
        std::thread::sleep(Duration::from_millis(200));
        external_kill(pid.expect("pid"));
    }
    let trigger = if scenario == "sigkill" { Instant::now() } else { trigger };

    // Dedicated blocking wait() thread -- the production model.
    let waiter = std::thread::spawn(move || child.wait());

    let start = Instant::now();
    loop {
        if waiter.is_finished() {
            break;
        }
        if start.elapsed() > Duration::from_secs(8) {
            eprintln!("[s5] TIMEOUT after 8s -- wait() never returned");
            // Give the reader thread a few more seconds in case EOF shows up late,
            // even though wait() itself gave up -- that's a separate signal.
            std::thread::sleep(Duration::from_secs(4));
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(75));
    }
    let observed_at = Instant::now();
    let status = waiter.join().expect("waiter thread panicked");

    let latency = observed_at.saturating_duration_since(trigger);
    match status {
        Ok(status) => eprintln!(
            "[s5] RESULT exit_code={:?} success={} latency_ms={}",
            status.exit_code(),
            status.success(),
            latency.as_millis()
        ),
        Err(e) => eprintln!("[s5] wait error: {e}"),
    }

    Ok(())
}

#[cfg(unix)]
fn external_kill(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn external_kill(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status();
}
