//! S8 spike -- W1 follow-up: correcting a mistake in s7's own interpretation.
//!
//! s7_long_wait's 30s run appeared to show wait() never returning while
//! Get-Process showed the child vanish partway through -- but s7 itself calls
//! std::process::exit(1) the moment its timeout fires, and process exit closes
//! EVERY handle the OS holds for it, including the ConPTY master. That's very
//! likely what actually kills the child, not the child completing gracefully --
//! and the ~12s "RESOLVED, undisturbed" result recorded for s5_minimal in the
//! 2026-09-05 multi-snapshot trace matches its own 8s-timeout + 4s-grace = 12s
//! exit schedule too closely to be a coincidence, once you look for it.
//!
//! This binary tests the real question directly: with a parent process that
//! NEVER exits on its own (child.wait() blocks the main thread with no timeout
//! at all -- literally what a long-running daemon's reaper thread does), does
//! the ConPTY child ever get reaped while the parent stays alive the whole
//! time? No self-exit, no grace sleep, nothing to confound the result.
//!
//! Usage: s8_never_timeout <exit0|exit7>
//! Run it, then poll `Get-Process mini_exit` / this binary's own stderr from a
//! SEPARATE process. Ctrl-C or taskkill this binary manually when done --
//! it will not exit on its own no matter how long the child takes.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

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
    eprintln!("[s8] scenario={scenario} exe={mini_exit:?} pid={pid:?}");
    eprintln!("[s8] this process will NOT self-exit -- kill it manually when done");

    let t0 = Instant::now();
    let mut reader = pair.master.try_clone_reader()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    eprintln!("[s8] reader EOF at {}ms", t0.elapsed().as_millis());
                    break;
                }
                Ok(n) => {
                    eprintln!(
                        "[s8] reader got {n} bytes at {}ms: {:?}",
                        t0.elapsed().as_millis(),
                        String::from_utf8_lossy(&buf[..n])
                    );
                }
                Err(e) => {
                    eprintln!("[s8] reader error at {}ms: {e}", t0.elapsed().as_millis());
                    break;
                }
            }
        }
    });

    // Heartbeat thread so external pollers (and this binary's own stderr log)
    // have proof-of-life independent of the blocked waiter -- if this stops
    // printing, the PROCESS died, not just the wait.
    let hb_t0 = t0;
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        eprintln!(
            "[s8] heartbeat at {}ms -- still running, still waiting",
            hb_t0.elapsed().as_millis()
        );
    });

    eprintln!("[s8] blocking on child.wait() now, no timeout, no self-exit");
    let status = child.wait();
    let elapsed = t0.elapsed().as_millis();
    match status {
        Ok(status) => eprintln!(
            "[s8] RESULT wait() RETURNED at {elapsed}ms exit_code={:?} success={}",
            status.exit_code(),
            status.success()
        ),
        Err(e) => eprintln!("[s8] wait() error at {elapsed}ms: {e}"),
    }

    // Now actually block forever (not exit) so the process's own teardown can
    // never be mistaken for the child's exit -- a human has to kill this.
    eprintln!("[s8] done waiting, now parking forever -- kill this process manually");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
