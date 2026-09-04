//! S3 spike — can a blocking PTY write wedge terminate?
//!
//! Usage: s3_blocking_write <shared|separate>
//!
//! Spawns a child that opens the tty and never reads it, then writes 1 MiB.
//!
//! `shared`   — one worker thread processes Write then Terminate *in order* off one
//!              queue, exactly as 03-pty-layer.md's current two-thread model puts
//!              write/resize/terminate on a single control thread. Expected to show
//!              terminate stuck behind the blocked write.
//! `separate` — the write runs on its own dedicated thread; terminate is issued
//!              directly against the child/master from the control thread, never
//!              going through the writer's queue. Expected to complete fast
//!              regardless of the pending write.

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

enum Cmd {
    WriteOneMib,
    Terminate,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("separate");

    let system = native_pty_system();
    let pair = system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // Disable echo/canonical processing. Default openpty() termios is cooked+echo,
    // which drains an unread write via the echo path -- that hid the real
    // backpressure in an earlier version of this spike (1 MiB "wrote" fine in
    // 170ms with nothing reading stdin). Raw mode is also what a real remote-shell
    // session runs in, so it's the honest scenario to test besides.
    #[cfg(unix)]
    set_raw_mode(&*pair.master)?;

    // Child that opens the tty and never reads stdin: it just parks. Nothing ever
    // drains the pty's input buffer, so a large write to the master will block once
    // the kernel buffer (a few KiB on Linux) fills.
    let mut cmd = CommandBuilder::new(never_reading_child_cmd());
    for a in never_reading_child_args() {
        cmd.arg(a);
    }
    let mut child = pair.slave.spawn_command(cmd)?;
    #[cfg(unix)]
    drop(pair.slave);

    // keep output drained so the child isn't itself blocked producing anything
    let mut reader = pair.master.try_clone_reader()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            use std::io::Read;
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let t0 = Instant::now();

    let terminate_started;
    let terminate_done;

    if mode == "shared" {
        let mut writer = pair.master.take_writer()?;
        let (tx, rx) = mpsc::channel::<Cmd>();
        let done_tx = tx.clone();
        // fire both commands into the queue immediately, exactly as an async
        // handler would forward `write` then a follow-up `terminate` request
        std::thread::spawn(move || {
            done_tx.send(Cmd::WriteOneMib).unwrap();
            std::thread::sleep(Duration::from_millis(50)); // let the write start blocking
            done_tx.send(Cmd::Terminate).unwrap();
        });

        let (result_tx, result_rx) = mpsc::channel();
        let child_pid = child.process_id();
        let worker = std::thread::spawn(move || {
            for cmd in rx {
                match cmd {
                    Cmd::WriteOneMib => {
                        let start = Instant::now();
                        let data = vec![b'x'; 1024 * 1024];
                        let _ = writer.write_all(&data);
                        let _ = writer.flush();
                        result_tx
                            .send(("write_done", start.elapsed()))
                            .ok();
                    }
                    Cmd::Terminate => {
                        let start = Instant::now();
                        hard_kill(child_pid);
                        result_tx
                            .send(("terminate_done", start.elapsed()))
                            .ok();
                        return;
                    }
                }
            }
        });

        terminate_started = t0.elapsed();
        // drain results as they arrive, tagging wall-clock time from t0
        let mut got_write = None;
        let mut got_term = None;
        while got_term.is_none() {
            match result_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(("write_done", d)) => {
                    got_write = Some(d);
                    eprintln!("[s3] write_all completed (blocked for {} ms)", d.as_millis());
                }
                Ok(("terminate_done", _)) => {
                    got_term = Some(t0.elapsed());
                }
                _ => break,
            }
        }
        // Do NOT join here if terminate never landed: the worker thread is still
        // parked inside write_all() with nothing ever going to unblock it (that's
        // the wedge under test), so joining would hang the spike itself forever.
        if got_term.is_some() {
            let _ = worker.join();
        } else {
            std::mem::drop(worker); // leak the stuck thread; process exit reclaims it
        }
        terminate_done = got_term.unwrap_or_else(|| {
            eprintln!("[s3] RESULT terminate did NOT complete within 10s (shared queue wedge)");
            t0.elapsed()
        });
        let _ = got_write;
    } else {
        // separate: writer on its own thread; terminate issued directly, bypassing
        // any queue the writer is on.
        let mut writer = pair.master.take_writer()?;
        std::thread::spawn(move || {
            let data = vec![b'x'; 1024 * 1024];
            let start = Instant::now();
            let _ = writer.write_all(&data);
            let _ = writer.flush();
            eprintln!(
                "[s3] write_all completed (blocked for {} ms) [background, may outlive terminate]",
                start.elapsed().as_millis()
            );
        });

        std::thread::sleep(Duration::from_millis(50)); // let the write start blocking
        terminate_started = t0.elapsed();
        let child_pid = child.process_id();
        hard_kill(child_pid);
        terminate_done = t0.elapsed();
    }

    let terminate_latency = terminate_done.saturating_sub(terminate_started);
    eprintln!(
        "[s3] RESULT mode={mode} terminate_latency_ms={}",
        terminate_latency.as_millis()
    );

    // clean up: in `shared` mode terminate may never have actually run (the wedge
    // under test), so the child could still be alive -- kill it directly rather
    // than let this spike binary itself hang on wait().
    hard_kill(child.process_id());
    let _ = child.wait();
    Ok(())
}

#[cfg(unix)]
fn set_raw_mode(master: &dyn portable_pty::MasterPty) -> Result<()> {
    let fd = master
        .as_raw_fd()
        .ok_or_else(|| anyhow::anyhow!("no raw fd on master"))?;
    unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut term) != 0 {
            anyhow::bail!("tcgetattr failed: {}", std::io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut term);
        if libc::tcsetattr(fd, libc::TCSANOW, &term) != 0 {
            anyhow::bail!("tcsetattr failed: {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn never_reading_child_cmd() -> &'static str {
    "/bin/sh"
}
#[cfg(unix)]
fn never_reading_child_args() -> Vec<&'static str> {
    vec!["-c", "sleep 30"]
}
#[cfg(unix)]
fn hard_kill(pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
fn never_reading_child_cmd() -> &'static str {
    "cmd.exe"
}
#[cfg(windows)]
fn never_reading_child_args() -> Vec<&'static str> {
    vec!["/c", "ping -n 30 127.0.0.1 >NUL"]
}
#[cfg(windows)]
fn hard_kill(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
    }
}
