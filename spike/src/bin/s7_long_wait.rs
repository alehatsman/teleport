//! S7 spike -- W1 follow-up: does `wait()` ever return if given more than the ~8-12s
//! every prior test capped it at?
//!
//! The multi-snapshot WinDbg trace (docs/15-open-questions.md#w1, 2026-09-05) found
//! the graceful-exit hang is NOT indefinite: left completely undisturbed, `mini_exit.exe`
//! reliably disappears (per `Get-Process` polling) around 12-13s. Every previous spike
//! that tests `wait()` itself -- s1_reaper, s5_minimal -- capped its own wait-loop at
//! 8s, strictly *before* that natural resolution point. Nobody has tested whether
//! `portable_pty::Child::wait()` actually returns once given long enough. This binary
//! is s5_minimal with that one number changed: 8s -> 30s, and no early exit.
//!
//! Usage: s7_long_wait <exit0|exit7>

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn mini_exit_path() -> Result<PathBuf> {
    let mut path = std::env::current_exe().context("current_exe")?;
    path.pop();
    let name = if cfg!(windows) {
        "mini_exit.exe"
    } else {
        "mini_exit"
    };
    path.push(name);
    anyhow::ensure!(
        path.exists(),
        "expected {path:?} to exist -- build the whole spike crate first"
    );
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
        other => panic!("unknown scenario {other}"),
    };

    let mut child = pair.slave.spawn_command(cmd)?;
    let pid = child.process_id();
    eprintln!("[s7] scenario={scenario} exe={mini_exit:?} pid={pid:?}");

    let t0 = Instant::now();
    let mut reader = pair.master.try_clone_reader()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    eprintln!("[s7] reader EOF at {}ms", t0.elapsed().as_millis());
                    break;
                }
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    eprintln!(
                        "[s7] reader got {n} bytes at {}ms: {text:?}",
                        t0.elapsed().as_millis()
                    );
                }
                Err(e) => {
                    eprintln!("[s7] reader error at {}ms: {e}", t0.elapsed().as_millis());
                    break;
                }
            }
        }
    });

    let waiter = std::thread::spawn(move || child.wait());

    let start = Instant::now();
    const LONG_TIMEOUT: Duration = Duration::from_secs(30);
    loop {
        if waiter.is_finished() {
            break;
        }
        if start.elapsed() > LONG_TIMEOUT {
            eprintln!(
                "[s7] TIMEOUT after {}s -- wait() STILL never returned even with a long timeout",
                LONG_TIMEOUT.as_secs()
            );
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let observed_at = Instant::now();
    let status = waiter.join().expect("waiter thread panicked");
    let latency = observed_at.saturating_duration_since(start);
    match status {
        Ok(status) => eprintln!(
            "[s7] RESULT wait() RETURNED exit_code={:?} success={} latency_ms={}",
            status.exit_code(),
            status.success(),
            latency.as_millis()
        ),
        Err(e) => eprintln!("[s7] wait() error: {e}"),
    }

    // Give the reader a couple more seconds to report EOF too, for the same
    // ordering-relative-to-wait() comparison every other spike here makes.
    std::thread::sleep(Duration::from_secs(2));

    Ok(())
}
