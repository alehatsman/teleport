//! S4 spike — does dropping the master close the pseudoconsole / hang up the child?
//!
//! Usage: s4_drop_master <plain|grandchild>
//!
//! Spawns a child producing continuous output, keeps a cloned reader (as the real
//! reader thread would) and a writer (as the real control thread would), then drops
//! only the `MasterPty` handle itself -- mirroring 03-pty-layer.md's termination step 2
//! ("ClosePseudoConsole via dropping the portable-pty master handle" / SIGHUP fallback
//! on Unix). Measures:
//!   - how long the `drop()` call itself takes (does it block the calling thread?)
//!   - how long until the reader sees EOF
//!   - how long until child.wait() reports exit
//!
//! `grandchild` additionally backgrounds a detached process before the parent's loop,
//! to test whether the tree really goes away or just the immediate child.

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("plain");

    eprintln!("[s4] os = {}", std::env::consts::OS);

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

    let mut reader = pair.master.try_clone_reader()?;
    let _writer = pair.master.take_writer()?; // held, mirroring the control thread

    let t0 = Instant::now();
    let (eof_tx, eof_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut total = 0u64;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = eof_tx.send(("eof", total));
                    break;
                }
                Ok(n) => total += n as u64,
                Err(_) => {
                    let _ = eof_tx.send(("read_err", total));
                    break;
                }
            }
        }
    });

    // let output flow for a bit so this is a "close under load" case
    std::thread::sleep(Duration::from_millis(500));

    let master = pair.master; // move out so we control exactly when it drops
    let drop_start = Instant::now();
    drop(master);
    let drop_call_ms = drop_start.elapsed().as_millis();
    let drop_at = t0.elapsed();
    eprintln!("[s4] drop(master) call itself took {drop_call_ms} ms");

    // how long until the reader sees EOF, from the moment of drop
    match eof_rx.recv_timeout(Duration::from_secs(10)) {
        Ok((what, total)) => eprintln!(
            "[s4] {what} observed {} ms after drop; bytes_read_total={total}",
            t0.elapsed().saturating_sub(drop_at).as_millis()
        ),
        Err(_) => eprintln!("[s4] RESULT no EOF within 10s of dropping master"),
    }

    // how long until the child is actually gone, from the moment of drop
    match child.wait() {
        Ok(status) => eprintln!(
            "[s4] RESULT child exited {} ms after drop; exit_code={:?}",
            t0.elapsed().saturating_sub(drop_at).as_millis(),
            status.exit_code()
        ),
        Err(e) => eprintln!("[s4] wait error: {e}"),
    }

    Ok(())
}

#[cfg(unix)]
fn build_command(scenario: &str) -> CommandBuilder {
    let mut cmd = CommandBuilder::new("/bin/sh");
    let script = match scenario {
        "plain" => "while true; do echo tick; sleep 0.05; done",
        "grandchild" => "nohup sh -c 'while true; do sleep 1; done' >/dev/null 2>&1 & while true; do echo tick; sleep 0.05; done",
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
        "plain" => cmd.arg("for /L %i in (0,0,1) do @(echo tick & ping -n 1 -w 50 127.0.0.1 >NUL)"),
        "grandchild" => cmd.arg(
            "start /B cmd /c \"for /L %i in (0,0,1) do @ping -n 2 127.0.0.1 >NUL\" & for /L %i in (0,0,1) do @(echo tick & ping -n 1 -w 50 127.0.0.1 >NUL)",
        ),
        other => panic!("unknown scenario {other}"),
    };
    cmd
}
