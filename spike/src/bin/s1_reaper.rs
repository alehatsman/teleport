//! S1 spike — who reaps the child, and how fast is the exit code observable?
//!
//! Usage: s1_reaper <exit0|exit7|sigkill|grandchild> <poll|blocking>
//!
//! `poll`     — try_wait() on a 75ms tick from this thread (stand-in for "control thread ticks")
//! `blocking` — dedicated thread calls child.wait() (stand-in for "third thread per session")
//!
//! Prints: SCENARIO / MECHANISM / observed exit code (or None) / latency from trigger to
//! observed, in ms.

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("exit0");
    let mechanism = args.get(2).map(String::as_str).unwrap_or("blocking");

    let system = native_pty_system();
    let pair = system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let cmd = build_command(scenario).expect("command builder");
    let mut child = pair.slave.spawn_command(cmd)?;
    #[cfg(unix)]
    drop(pair.slave);

    let pid = child.process_id();
    eprintln!("[s1] scenario={scenario} mechanism={mechanism} pid={pid:?}");

    // Keep draining output so a full pty buffer never blocks the child, mirroring the
    // real reader thread. Not the thing under test, just hygiene.
    let mut reader = pair.master.try_clone_reader()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let trigger = Instant::now();
    if scenario == "sigkill" {
        // give the child a moment to be running, then kill it externally
        std::thread::sleep(Duration::from_millis(200));
        external_kill(pid.expect("pid"));
    }
    // reset trigger to the actual kill moment for the sigkill case
    let trigger = if scenario == "sigkill" {
        Instant::now()
    } else {
        trigger
    };

    let (status, observed_at) = match mechanism {
        "poll" => {
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => break (Some(status), Instant::now()),
                    Ok(None) => std::thread::sleep(Duration::from_millis(75)),
                    Err(e) => {
                        eprintln!("[s1] try_wait error: {e}");
                        break (None, Instant::now());
                    }
                }
                if start.elapsed() > Duration::from_secs(10) {
                    eprintln!("[s1] TIMEOUT waiting for exit via poll");
                    break (None, Instant::now());
                }
            }
        }
        "blocking" => match child.wait() {
            Ok(status) => (Some(status), Instant::now()),
            Err(e) => {
                eprintln!("[s1] wait error: {e}");
                (None, Instant::now())
            }
        },
        other => panic!("unknown mechanism {other}"),
    };

    let latency = observed_at.saturating_duration_since(trigger);
    match status {
        Some(status) => eprintln!(
            "[s1] RESULT exit_code={:?} success={} latency_ms={}",
            status.exit_code(),
            status.success(),
            latency.as_millis()
        ),
        None => eprintln!("[s1] RESULT exit_code=NONE latency_ms={}", latency.as_millis()),
    }

    Ok(())
}

#[cfg(unix)]
fn build_command(scenario: &str) -> Option<CommandBuilder> {
    let mut cmd = CommandBuilder::new("/bin/sh");
    let script = match scenario {
        "exit0" => "exit 0",
        "exit7" => "exit 7",
        "sigkill" => "sleep 30",
        "grandchild" => "sleep 60 & exit 0",
        other => panic!("unknown scenario {other}"),
    };
    cmd.arg("-c");
    cmd.arg(script);
    Some(cmd)
}

#[cfg(windows)]
fn build_command(scenario: &str) -> Option<CommandBuilder> {
    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.arg("/c");
    match scenario {
        "exit0" => cmd.arg("exit 0"),
        "exit7" => cmd.arg("exit 7"),
        "sigkill" => cmd.arg("ping -n 30 127.0.0.1 >NUL"),
        "grandchild" => cmd.arg("start /B ping -n 60 127.0.0.1 >NUL & exit 0"),
        other => panic!("unknown scenario {other}"),
    };
    Some(cmd)
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
