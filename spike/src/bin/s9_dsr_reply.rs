//! S9 spike -- W1 root-cause test: does answering ConPTY's startup DSR
//! (Device Status Report / cursor-position) query unblock the hang?
//!
//! s8_never_timeout's log showed the pty master emitting `\x1b[6n` (a DSR
//! "report cursor position" request) at 0ms, before anything else -- and the
//! WinDbg trace (docs/15-open-questions.md#w1, multi-snapshot follow-up) shows
//! conhost.exe's ConsoleIoThread parked the entire time in
//! `VtIo::StartIfNeeded -> VtInputThread::DoReadInput -> ReadFile`, i.e.
//! blocked reading the *input* side of the same pty. Nothing in every prior
//! spike ever wrote anything back to the master -- so if ConPTY's startup
//! handshake genuinely needs a CPR reply (`ESC [ row ; col R`) written back
//! before it will finish initializing (and, per the stack, before
//! `ConsoleAllocateConsole` can return and the rest of the session -- exit
//! detection included -- can proceed), that would fully explain W1: real
//! terminal emulators (conhost's own window, Windows Terminal, ConEmu) answer
//! this automatically; a raw pipe consumer like portable-pty's `PtyPair`
//! does not, unless the application using it does so itself.
//!
//! This binary is s8 with exactly one addition: a reader thread that watches
//! for `ESC [ 6 n` in the child's output and, when it sees it, immediately
//! writes back `ESC [ 1 ; 1 R` (claim cursor is at row 1, col 1 -- portable-pty
//! doesn't expose "what did I actually set the initial cursor to", so this is
//! a plausible fixed answer, not a tracked one) via `take_writer()`. Otherwise
//! identical to s8: wait() with no timeout, no self-exit, heartbeat thread.
//!
//! Usage: s9_dsr_reply <exit0|exit7>

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
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

    let mut writer = pair.master.take_writer()?;

    let mut child = pair.slave.spawn_command(cmd)?;
    let pid = child.process_id();
    eprintln!("[s9] scenario={scenario} exe={mini_exit:?} pid={pid:?}");
    eprintln!("[s9] this process will NOT self-exit -- kill it manually when done");

    let t0 = Instant::now();
    let mut reader = pair.master.try_clone_reader()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // Scratch buffer so a DSR query split across two read() calls (unlikely
        // for 4 bytes, but let's not assume) still gets matched.
        let mut pending = Vec::<u8>::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    eprintln!("[s9] reader EOF at {}ms", t0.elapsed().as_millis());
                    break;
                }
                Ok(n) => {
                    eprintln!(
                        "[s9] reader got {n} bytes at {}ms: {:?}",
                        t0.elapsed().as_millis(),
                        String::from_utf8_lossy(&buf[..n])
                    );
                    pending.extend_from_slice(&buf[..n]);
                    // Look for ESC [ 6 n anywhere in what we've buffered.
                    if let Some(pos) = find_subslice(&pending, b"\x1b[6n") {
                        eprintln!(
                            "[s9] saw DSR cursor-position query at {}ms -- replying ESC[1;1R",
                            t0.elapsed().as_millis()
                        );
                        if let Err(e) = writer.write_all(b"\x1b[1;1R") {
                            eprintln!("[s9] write reply failed: {e}");
                        } else if let Err(e) = writer.flush() {
                            eprintln!("[s9] flush reply failed: {e}");
                        } else {
                            eprintln!(
                                "[s9] reply written+flushed at {}ms",
                                t0.elapsed().as_millis()
                            );
                        }
                        pending.drain(..pos + 4);
                    }
                }
                Err(e) => {
                    eprintln!("[s9] reader error at {}ms: {e}", t0.elapsed().as_millis());
                    break;
                }
            }
        }
    });

    let hb_t0 = t0;
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        eprintln!(
            "[s9] heartbeat at {}ms -- still running, still waiting",
            hb_t0.elapsed().as_millis()
        );
    });

    eprintln!("[s9] blocking on child.wait() now, no timeout, no self-exit");
    let status = child.wait();
    let elapsed = t0.elapsed().as_millis();
    match status {
        Ok(status) => eprintln!(
            "[s9] RESULT wait() RETURNED at {elapsed}ms exit_code={:?} success={}",
            status.exit_code(),
            status.success()
        ),
        Err(e) => eprintln!("[s9] wait() error at {elapsed}ms: {e}"),
    }

    eprintln!("[s9] done waiting, now parking forever -- kill this process manually");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
