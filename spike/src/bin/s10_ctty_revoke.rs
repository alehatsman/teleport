//! S10 spike -- why does a detached grandchild lose the pty on macOS?
//!
//! Usage:
//!   s10_ctty_revoke run [ctty|noctty]     # parent: build a pty, spawn sh, watch EOF vs exit
//!   s10_ctty_revoke gc <log> <slave_path>  # the detached grandchild (spawned by `run`)
//!
//! `ctty` (the default) is the real configuration: the direct child is a session
//! leader and the pty is its *controlling* terminal, exactly as portable-pty sets
//! a session up. `noctty` is the control: same pty, same detached grandchild, but
//! the child never calls TIOCSCTTY, so the pty is an ordinary pair of fds and no
//! session leader owns it. If `noctty` behaves like Linux while `ctty` does not,
//! the mechanism is specifically the controlling-terminal teardown.
//!
//! S5 (docs/15-open-questions.md) says a detached grandchild that ignores SIGHUP
//! keeps a pty's master open past the direct child's exit on Linux, but not on
//! macOS. The leading (unverified) hypothesis was BSD `revoke(2)` semantics on
//! session-leader exit. This spike discriminates the two candidate mechanisms:
//!
//!   death   -- the grandchild is killed (SIGHUP or otherwise) and the slave fd
//!              closes because the process is gone.
//!   revoke  -- the grandchild is alive and well, but every descriptor pointing
//!              at the controlling tty was forcibly invalidated by the kernel.
//!
//! They are distinguishable: under `revoke`, the grandchild survives, and its
//! inherited `write(1, ...)` starts failing with a specific errno at exactly the
//! moment the session leader exits. It also tells us whether a *fresh* open of
//! the same slave path still works afterwards -- i.e. whether any userspace
//! workaround exists at all.

use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str).unwrap_or("run") {
        "run" => run(args.get(2).map(String::as_str).unwrap_or("ctty")),
        "gc" => grandchild(&args[2], &args[3]),
        other => panic!("unknown mode {other}"),
    }
}

#[cfg(unix)]
fn grandchild(log_path: &str, slave_path: &str) {
    use std::io::Write;

    // Detach: own session, own process group, so a process-group-wide SIGHUP
    // cannot be what reaches us. Plus an explicit ignore, belt and braces.
    unsafe {
        libc::setsid();
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .expect("gc log");
    let t0 = Instant::now();
    let pid = unsafe { libc::getpid() };
    let sid = unsafe { libc::getsid(0) };
    let _ = writeln!(log, "[gc] pid={pid} sid={sid} slave={slave_path}");

    // fd 1 is the inherited pty slave (the direct parent's stdout).
    for i in 0..40 {
        let msg = format!("gc-tick-{i}\n");
        let n = unsafe {
            libc::write(1, msg.as_ptr() as *const libc::c_void, msg.len())
        };
        let err = std::io::Error::last_os_error();
        let _ = writeln!(
            log,
            "[gc] t_ms={:>4} write(1)={} errno={}",
            t0.elapsed().as_millis(),
            n,
            if n < 0 { format!("{:?}", err.raw_os_error()) } else { "-".into() }
        );

        // Do we still have a controlling terminal?
        if i % 10 == 0 {
            let ctty = unsafe {
                libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK)
            };
            let e = std::io::Error::last_os_error();
            let _ = writeln!(
                log,
                "[gc] t_ms={:>4} open(/dev/tty)={} errno={}",
                t0.elapsed().as_millis(),
                ctty,
                if ctty < 0 { format!("{:?}", e.raw_os_error()) } else { "-".into() }
            );
            if ctty >= 0 {
                unsafe { libc::close(ctty) };
            }
        }

        // Can a *fresh* open of the same slave path still reach the master?
        if i == 20 {
            let c_path = std::ffi::CString::new(slave_path).unwrap();
            let fd = unsafe {
                libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK)
            };
            let e = std::io::Error::last_os_error();
            let _ = writeln!(
                log,
                "[gc] t_ms={:>4} reopen({slave_path})={} errno={}",
                t0.elapsed().as_millis(),
                fd,
                if fd < 0 { format!("{:?}", e.raw_os_error()) } else { "-".into() }
            );
            if fd >= 0 {
                let m = b"gc-via-reopened-slave\n";
                let w = unsafe { libc::write(fd, m.as_ptr() as *const libc::c_void, m.len()) };
                let e2 = std::io::Error::last_os_error();
                let _ = writeln!(
                    log,
                    "[gc] t_ms={:>4} write(reopened)={} errno={}",
                    t0.elapsed().as_millis(),
                    w,
                    if w < 0 { format!("{:?}", e2.raw_os_error()) } else { "-".into() }
                );
                unsafe { libc::close(fd) };
            }
        }

        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = writeln!(log, "[gc] t_ms={:>4} exiting normally", t0.elapsed().as_millis());
}

#[cfg(unix)]
fn run(mode: &str) {
    let want_ctty = match mode {
        "ctty" => true,
        "noctty" => false,
        other => panic!("unknown mode {other}, want ctty|noctty"),
    };
    let exe = std::env::current_exe().expect("current_exe");
    let log_path = std::env::temp_dir().join(format!("s10-gc-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    // A raw pty, built by hand: posix_openpt / grantpt / unlockpt / ptsname.
    // No portable-pty here on purpose -- this spike needs the slave's *path*,
    // and needs the ctty setup to be visible rather than behind an abstraction.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    assert!(master >= 0, "posix_openpt: {}", std::io::Error::last_os_error());
    assert_eq!(unsafe { libc::grantpt(master) }, 0);
    assert_eq!(unsafe { libc::unlockpt(master) }, 0);
    let slave_path = unsafe {
        let p = libc::ptsname(master);
        assert!(!p.is_null());
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    };
    eprintln!("[s10] mode={mode} slave={slave_path} log={}", log_path.display());

    let script = format!(
        "trap '' HUP; {} gc {} {} & echo GRANDCHILD_PID:$!; exit 0",
        exe.display(),
        log_path.display(),
        slave_path
    );

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
    if pid == 0 {
        unsafe {
            if want_ctty {
                libc::setsid();
            }
            let c_path = std::ffi::CString::new(slave_path.clone()).unwrap();
            let slave = libc::open(c_path.as_ptr(), libc::O_RDWR);
            assert!(slave >= 0);
            if want_ctty {
                libc::ioctl(slave, libc::TIOCSCTTY as libc::c_ulong, 0);
            }
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            let sh = c"/bin/sh";
            let dash_c = c"-c";
            let script_c = std::ffi::CString::new(script).unwrap();
            libc::execl(
                sh.as_ptr(),
                sh.as_ptr(),
                dash_c.as_ptr(),
                script_c.as_ptr(),
                std::ptr::null::<libc::c_char>(),
            );
            libc::_exit(127);
        }
    }

    let t0 = Instant::now();
    let (tx, rx) = mpsc::channel();
    let master_dup = unsafe { libc::dup(master) };
    std::thread::spawn(move || {
        let mut f = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(master_dup) };
        let mut buf = [0u8; 4096];
        let mut all = Vec::new();
        loop {
            match f.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(("EOF".to_string(), t0.elapsed(), all.clone()));
                    break;
                }
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(e) => {
                    let _ = tx.send((
                        format!("read_err errno={:?}", e.raw_os_error()),
                        t0.elapsed(),
                        all.clone(),
                    ));
                    break;
                }
            }
        }
    });

    let mut status: libc::c_int = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let exit_at = t0.elapsed();
    eprintln!("[s10] direct child reaped at_ms={}", exit_at.as_millis());

    let (what, at, out) = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("master reader should report EOF or an error");
    eprintln!(
        "[s10] master {what} at_ms={} bytes={}",
        at.as_millis(),
        out.len()
    );
    let text = String::from_utf8_lossy(&out);
    eprintln!("[s10] master output: {:?}", text.trim());

    let gc_pid: i32 = text
        .split("GRANDCHILD_PID:")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("grandchild pid");

    // Is the grandchild alive *after* the master saw EOF? This is the whole
    // question: dead process vs. live process with a revoked descriptor.
    for step in 0..6 {
        let alive = unsafe { libc::kill(gc_pid, 0) } == 0;
        eprintln!(
            "[s10] t_ms={:>4} grandchild pid={gc_pid} alive={alive}",
            t0.elapsed().as_millis()
        );
        if step < 5 {
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    // Did anything reach the master after EOF? (It should not -- read() on a
    // revoked/closed pty stays at EOF -- but this proves it rather than assuming.)
    let mut buf = [0u8; 256];
    let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    eprintln!(
        "[s10] post-EOF read(master)={} errno={:?}",
        n,
        if n < 0 { std::io::Error::last_os_error().raw_os_error() } else { None }
    );

    std::thread::sleep(Duration::from_secs(2));
    eprintln!("[s10] ---- grandchild log ----");
    match std::fs::read_to_string(&log_path) {
        Ok(s) => eprintln!("{s}"),
        Err(e) => eprintln!("[s10] no grandchild log: {e}"),
    }
}

#[cfg(not(unix))]
fn run(_: &str) {
    eprintln!("[s10] unix-only spike");
}
#[cfg(not(unix))]
fn grandchild(_: &str, _: &str) {}
