//! S2 spike — does EOF on the master mean the child exited?
//!
//! Usage: s2_eof <basic|grandchild|midburst>
//!
//! Spawns a child, reads the master on one thread (recording when read() returns 0,
//! i.e. EOF) while wait()-ing on the child on this thread (recording when the exit
//! status lands). Prints both timestamps relative to process start so the ordering
//! and gap between them is visible.

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("basic");

    let system = native_pty_system();
    let pair = system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let cmd = build_command(scenario);
    let mut child = pair.slave.spawn_command(cmd)?;
    #[cfg(unix)]
    drop(pair.slave);

    let t0 = Instant::now();
    let mut reader = pair.master.try_clone_reader()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut total = 0usize;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(("eof", t0.elapsed(), total));
                    break;
                }
                Ok(n) => total += n,
                Err(e) => {
                    let _ = tx.send(("read_err", t0.elapsed(), total));
                    eprintln!("[s2] read error: {e}");
                    break;
                }
            }
        }
    });

    let status = child.wait()?;
    let exit_at = t0.elapsed();
    eprintln!(
        "[s2] scenario={scenario} wait_returned_at_ms={} exit_code={:?}",
        exit_at.as_millis(),
        status.exit_code()
    );

    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok((what, at, total)) => {
            eprintln!(
                "[s2] {what} at_ms={} bytes_read_before_eof={total}",
                at.as_millis()
            );
            let (first, second, gap) = if exit_at <= at {
                ("wait()", "EOF", at.saturating_sub(exit_at))
            } else {
                ("EOF", "wait()", exit_at.saturating_sub(at))
            };
            eprintln!(
                "[s2] RESULT order={first}_before_{second} gap_ms={}",
                gap.as_millis()
            );
        }
        Err(_) => eprintln!("[s2] RESULT EOF did not arrive within 15s of wait() returning"),
    }

    Ok(())
}

#[cfg(unix)]
fn build_command(scenario: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("/bin/sh");
    let script = match scenario {
        "basic" => "echo hi; exit 0",
        // `nohup sleep 5 &` does NOT reproduce the case S2 is about: portable_pty's
        // child becomes a session leader with the pty as controlling terminal
        // (setsid + TIOCSCTTY, see unix.rs), so when it exits, SIGHUP goes to the
        // whole foreground process group, not just the session leader -- and it
        // kills the nohup'd grandchild too (verified: pgrep finds nothing after).
        // Real detachment needs `setsid`, which un-shares process group as well as
        // session, so the grandchild is out of the pgrp SIGHUP targets. Found
        // empirically -- see docs/15-open-questions.md#s2.
        // Verified empirically (see docs/15-open-questions.md#s2): plain `nohup x &`
        // and bare `setsid x &` both still die when the session leader exits --
        // Linux delivers SIGHUP for the hangup even to a setsid'd descendant if it
        // hasn't ignored SIGHUP *before* the fork races against the exit. `trap ''
        // HUP` ahead of `setsid` is what actually survives.
        "grandchild" => {
            "trap '' HUP; setsid sh -c 'trap \"\" HUP; sleep 5' & exit 0"
        }
        "midburst" => "for i in $(seq 1 20000); do echo line$i; done; exit 0",
        other => panic!("unknown scenario {other}"),
    };
    cmd.arg("-c");
    cmd.arg(script);
    cmd
}

#[cfg(windows)]
fn build_command(scenario: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.arg("/c");
    match scenario {
        "basic" => cmd.arg("echo hi"),
        "grandchild" => cmd.arg("start /B ping -n 6 127.0.0.1 >NUL & exit 0"),
        "midburst" => cmd.arg("for /L %i in (1,1,20000) do @echo line%i"),
        other => panic!("unknown scenario {other}"),
    };
    cmd
}
