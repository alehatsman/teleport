//! **S12 -- does coalescing macOS pty reads buy anything, and what does it
//! cost the echo path?** ([#33](https://github.com/alehatsman/teleport/issues/33),
//! [N5](../../../docs/15-open-questions.md#n5--macos-pty-reads-average-14-bytes-starving-the-queue-bounds-count-half))
//!
//! N5 measured the read *shape* on macOS -- mean 14 bytes, 737,705 reads per
//! 10 MiB -- and #33 asks the next question: is that actually costing
//! anything, and can an opportunistic drain fix it without paying echo
//! latency. #33 says step one is a number, not a patch. This is the number.
//!
//! Three modes, run against a real pty, not a pipe:
//!
//! * `throughput` -- 10 MiB of `yes` output, read two ways, back to back:
//!   `baseline` (exactly what `pty.rs::reader_thread_main` does today) and
//!   `drain` (blocking read, then keep filling the same buffer while
//!   `poll(POLLIN, 0)` says bytes are *already* waiting, then publish once).
//!   Reports chunk count, mean chunk size, wall time and reader-thread CPU.
//!   The simulated downstream cost per chunk -- an `Arc<[u8]>` allocation, a
//!   mutex acquire, a bounded `try_send` -- is what `fanout.rs` actually
//!   does per publish, and is where the 737k round-trips are meant to hurt.
//! * `echo` -- the acceptance criterion. One byte in, wait for the tty
//!   driver's echo back out, timed, both ways. A drain that waits for more
//!   bytes would show up here as added latency; one that only merges
//!   already-available bytes should not.
//! * `fd` -- proves the unverified blocker in #33 is not a blocker:
//!   `try_clone_reader()` hands back a bare `Box<dyn Read + Send>` with no
//!   fd, but `MasterPty::as_raw_fd()` (portable_pty 0.9 `lib.rs:114`,
//!   `cfg(unix)`) does expose one.
//!
//! ## Result (2026-09-06, M-series Mac, release build, 10 MiB per run)
//!
//! ```text
//!   baseline  reads 3161392 ( 3.3 B/read)  chunks 3161392  cpu 4.97 s  1.6 MiB/s
//!   drain     reads  250208 (41.9 B/read)  chunks     570  cpu 0.96 s  1.9 MiB/s
//!   drain-nb  reads 3585536 ( 2.9 B/read)  chunks   39704  cpu 4.75 s  1.6 MiB/s
//!
//!   echo   baseline  p50  3.2 us  p99     7.6 us
//!   echo   drain     p50 18.5 us  p99  2837.6 us
//!   echo   drain-nb  p50  6.1 us  p99    10.3 us
//! ```
//!
//! **#33's premise does not survive this.** The issue assumed the cost was
//! the per-chunk downstream work -- log append, fan-out mutex, `Arc`
//! allocation, `try_send`. `drain-nb` removes 98.8% of those (3.1M publishes
//! down to 40k) and buys **6% CPU**. The cost is the three million
//! `read(2)`s returning ~3 bytes each, and nothing downstream of them.
//!
//! The only thing that makes reads bigger is *waiting*: `drain` gets 41.9
//! B/read and 80% of the CPU back, and it gets there by blocking -- the same
//! stream read without blocking (`drain-nb`) still averages 2.9 B/read. That
//! wait is exactly what the echo column charges for: p99 2.8 ms against a
//! 7.6 us baseline, ~400x. So the trade the design docs originally recorded
//! is real, and the non-blocking escape hatch #33 proposed does not exist.
//!
//! Throughput never moves: every mode lands at 1.6-1.9 MiB/s. The tty is the
//! ceiling, not the reader.
//!
//! Two caveats on reading these numbers. The `Downstream` here is *lighter*
//! than the real `fanout.rs`, so this reader loops faster and sees even
//! smaller reads than N5 measured in the daemon (3.3 B here vs 14 B there) --
//! meaning this **overstates** the baseline's read count relative to
//! production, and so overstates the available win. And the bracketing
//! `baseline` runs are there to be compared with each other: if they
//! disagree, the machine moved and the middle rows mean nothing.
//!
//! The fd polled here is an independent `dup()` of the master, not the
//! master fd itself: in `pty.rs` the master is owned by the control thread
//! and dropped on terminate, so a reader thread polling *that* fd races a
//! close (EBADF, or worse, fd reuse). A private dup shares the open file
//! description -- so POLLIN means the same thing -- and its lifetime is the
//! reader's own.
//!
//! macOS only in spirit (it is where the read shape is pathological), but it
//! runs anywhere unix; Linux is the useful control.

use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const TOTAL: usize = 10 * 1024 * 1024;
const READ_BUFFER_SIZE: usize = 64 * 1024;
const ECHO_SAMPLES: usize = 200;

/// Process CPU (user + sys) so far. `RUSAGE_SELF` excludes the child, which
/// is what we want -- `yes` burning a core is not the cost under study.
fn cpu_now() -> Duration {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let secs = (ru.ru_utime.tv_sec + ru.ru_stime.tv_sec) as u64;
    let usecs = (ru.ru_utime.tv_usec + ru.ru_stime.tv_usec) as u64;
    Duration::from_secs(secs) + Duration::from_micros(usecs)
}

/// True if the fd has bytes available *right now*. Zero timeout: this never
/// waits, which is the whole point -- an idle interactive session gets one
/// immediate "no" and publishes, unchanged.
fn readable_now(fd: libc::c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    n > 0 && (pfd.revents & libc::POLLIN) != 0
}

/// Wait indefinitely for the fd to become readable. The `drain2` loop's
/// replacement for the blocking `read` -- it does the waiting, so every
/// `read` afterwards can be non-blocking.
fn wait_readable(fd: libc::c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let n = unsafe { libc::poll(&mut pfd, 1, -1) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        return n > 0;
    }
}

fn set_nonblocking(fd: libc::c_int) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        bail!("fcntl(O_NONBLOCK): {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// `read(2)` straight on our own fd. `Ok(None)` is EAGAIN -- nothing there
/// after all, which is the case the whole `drain2` design hinges on.
fn read_nonblocking(fd: libc::c_int, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n >= 0 {
        return Ok(Some(n as usize));
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        // EWOULDBLOCK is the same value as EAGAIN on every unix we target.
        Some(libc::EAGAIN) => Ok(None),
        _ => Err(e),
    }
}

/// Stands in for what `fanout.rs` does on every publish: allocate the shared
/// `Arc<[u8]>`, take the fan-out lock, hand a clone to each subscriber
/// through a bounded queue. One subscriber, drained continuously -- the
/// cheapest realistic case, so this *understates* the per-chunk cost.
struct Downstream {
    lock: Mutex<()>,
    tx: std::sync::mpsc::SyncSender<Arc<[u8]>>,
    published: AtomicUsize,
}

impl Downstream {
    fn new() -> (Arc<Self>, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Arc<[u8]>>(1024);
        let drain = std::thread::spawn(move || while rx.recv().is_ok() {});
        (
            Arc::new(Self {
                lock: Mutex::new(()),
                tx,
                published: AtomicUsize::new(0),
            }),
            drain,
        )
    }

    fn publish(&self, bytes: &[u8]) {
        let payload: Arc<[u8]> = Arc::from(bytes);
        let _g = self.lock.lock().unwrap();
        let _ = self.tx.try_send(payload);
        self.published.fetch_add(1, Ordering::Relaxed);
    }
}

/// Which read loop is under test.
#[derive(Clone, Copy, PartialEq)]
enum Loop {
    /// Exactly `pty.rs::reader_thread_main` today: one blocking read, publish.
    Baseline,
    /// #33's sketch: blocking read, then `poll(POLLIN, 0)` + another
    /// *blocking* read while bytes look available.
    Drain,
    /// The same idea with the fd in non-blocking mode and the wait moved into
    /// `poll(-1)`, so a poll that lies costs an EAGAIN instead of a block.
    DrainNonblocking,
}

struct Run {
    label: &'static str,
    reads: usize,
    chunks: usize,
    bytes: usize,
    wall: Duration,
    cpu: Duration,
}

impl Run {
    fn report(&self) {
        let mib = self.bytes as f64 / (1024.0 * 1024.0);
        println!(
            "  {:8}  reads {:>9}  ({:>6.1} B/read)  chunks {:>9}  mean {:>8.1} B  wall {:>6.3} s  cpu {:>6.3} s  {:>5.1} MiB/s",
            self.label,
            self.reads,
            self.bytes as f64 / self.reads.max(1) as f64,
            self.chunks,
            self.bytes as f64 / self.chunks.max(1) as f64,
            self.wall.as_secs_f64(),
            self.cpu.as_secs_f64(),
            mib / self.wall.as_secs_f64().max(f64::EPSILON),
        );
    }
}

/// Reads `TOTAL` bytes off a fresh pty. `coalesce` picks the loop under test;
/// everything else -- buffer size, the downstream publish -- is identical, so
/// the only difference between the two runs is the drain.
fn throughput(label: &'static str, mode: Loop) -> Result<Run> {
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg(format!("stty raw -echo; yes | head -c {TOTAL}"));
    cmd.cwd(std::env::temp_dir());
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let master_fd = pair.master.as_raw_fd().context("master exposes no fd")?;
    // Independent dup: same open file description, lifetime ours. See the
    // module comment on why we do not poll the master's own fd.
    let poll_fd = unsafe { libc::dup(master_fd) };
    if poll_fd < 0 {
        bail!("dup(master): {}", std::io::Error::last_os_error());
    }

    let mut reader = pair.master.try_clone_reader()?;
    let (down, drain_thread) = Downstream::new();

    if mode == Loop::DrainNonblocking {
        set_nonblocking(poll_fd)?;
    }

    let mut buf = [0u8; READ_BUFFER_SIZE];
    let mut chunks = 0usize;
    let mut bytes = 0usize;
    let mut reads = 0usize;
    let mut lying_polls = 0usize;

    let wall0 = Instant::now();
    let cpu0 = cpu_now();
    loop {
        let mut end;
        if mode == Loop::DrainNonblocking {
            // The wait happens here, once, and every read after it is
            // non-blocking -- so a POLLIN that turns out to be wrong costs
            // one EAGAIN, not an unbounded stall.
            if !wait_readable(poll_fd) {
                break;
            }
            reads += 1;
            match read_nonblocking(poll_fd, &mut buf) {
                Ok(Some(0)) | Err(_) => break,
                Ok(Some(n)) => end = n,
                Ok(None) => {
                    lying_polls += 1;
                    continue;
                }
            }
            while end < buf.len() {
                reads += 1;
                match read_nonblocking(poll_fd, &mut buf[end..]) {
                    Ok(Some(0)) | Ok(None) | Err(_) => break,
                    Ok(Some(more)) => end += more,
                }
            }
        } else {
            reads += 1;
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            end = n;
            if mode == Loop::Drain {
                // Merge only what is *already* waiting. No timeout, no wait.
                while end < buf.len() && readable_now(poll_fd) {
                    reads += 1;
                    match reader.read(&mut buf[end..]) {
                        Ok(0) => break,
                        Ok(more) => end += more,
                        Err(_) => break,
                    }
                }
            }
        }
        down.publish(&buf[..end]);
        chunks += 1;
        bytes += end;
        if bytes >= TOTAL {
            break;
        }
    }
    let wall = wall0.elapsed();
    let cpu = cpu_now() - cpu0;
    if lying_polls > 0 {
        println!("            (poll said readable but read returned EAGAIN {lying_polls}x)");
    }

    unsafe { libc::close(poll_fd) };
    let _ = child.kill();
    let _ = child.wait();
    drop(down);
    drop(reader);
    let _ = drain_thread.join();

    Ok(Run {
        label,
        reads,
        chunks,
        bytes,
        wall,
        cpu,
    })
}

/// The acceptance criterion. Default termios (echo on, canonical): the tty
/// driver echoes each byte written to the master straight back, with no help
/// from the child -- so this times the echo path itself, not `cat`'s
/// scheduling.
fn echo(label: &str, mode: Loop) -> Result<()> {
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut cmd = CommandBuilder::new("/bin/cat");
    cmd.cwd(std::env::temp_dir());
    let mut child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let master_fd = pair.master.as_raw_fd().context("master exposes no fd")?;
    let poll_fd = unsafe { libc::dup(master_fd) };
    if poll_fd < 0 {
        bail!("dup(master): {}", std::io::Error::last_os_error());
    }
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    // Let cat come up; a keystroke racing exec would measure the wrong thing.
    std::thread::sleep(Duration::from_millis(300));

    if mode == Loop::DrainNonblocking {
        set_nonblocking(poll_fd)?;
    }

    let mut buf = [0u8; READ_BUFFER_SIZE];
    let mut samples = Vec::with_capacity(ECHO_SAMPLES);
    for _ in 0..ECHO_SAMPLES {
        use std::io::Write;
        let t0 = Instant::now();
        writer.write_all(b"x")?;
        writer.flush()?;
        let mut end;
        if mode == Loop::DrainNonblocking {
            if !wait_readable(poll_fd) {
                break;
            }
            end = match read_nonblocking(poll_fd, &mut buf) {
                Ok(Some(n)) => n,
                _ => break,
            };
            while end < buf.len() {
                match read_nonblocking(poll_fd, &mut buf[end..]) {
                    Ok(Some(0)) | Ok(None) | Err(_) => break,
                    Ok(Some(more)) => end += more,
                }
            }
        } else {
            end = reader.read(&mut buf)?;
            if mode == Loop::Drain {
                while end < buf.len() && readable_now(poll_fd) {
                    match reader.read(&mut buf[end..]) {
                        Ok(0) => break,
                        Ok(more) => end += more,
                        Err(_) => break,
                    }
                }
            }
        }
        samples.push(t0.elapsed());
        // Drain the newline-less canonical buffer's echo backlog between
        // samples so each iteration starts clean.
        std::thread::sleep(Duration::from_millis(1));
        while readable_now(poll_fd) {
            if mode == Loop::DrainNonblocking {
                match read_nonblocking(poll_fd, &mut buf) {
                    Ok(Some(n)) if n > 0 => continue,
                    _ => break,
                }
            }
            let _ = reader.read(&mut buf);
        }
    }

    samples.sort();
    let p = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "  {:8}  n {}  p50 {:>8.1} us  p90 {:>8.1} us  p99 {:>8.1} us  max {:>8.1} us",
        label,
        samples.len(),
        p(0.50).as_secs_f64() * 1e6,
        p(0.90).as_secs_f64() * 1e6,
        p(0.99).as_secs_f64() * 1e6,
        samples[samples.len() - 1].as_secs_f64() * 1e6,
    );

    unsafe { libc::close(poll_fd) };
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn main() -> Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "all".into());

    if mode == "fd" || mode == "all" {
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        match pair.master.as_raw_fd() {
            Some(fd) => {
                println!("fd: MasterPty::as_raw_fd() -> Some({fd}) -- #33's blocker is not one")
            }
            None => println!("fd: MasterPty::as_raw_fd() -> None -- #33's blocker is real"),
        }
        println!();
    }

    if mode == "throughput" || mode == "all" {
        println!("throughput: {} MiB per run", TOTAL / (1024 * 1024));
        // Baseline first, then drain, then baseline again: if the second
        // baseline does not match the first, the machine moved under us and
        // the comparison is worthless.
        throughput("baseline", Loop::Baseline)?.report();
        throughput("drain", Loop::Drain)?.report();
        throughput("drain-nb", Loop::DrainNonblocking)?.report();
        throughput("baseline", Loop::Baseline)?.report();
        println!();
    }

    if mode == "echo" || mode == "all" {
        println!("echo: one byte in, tty driver echo out");
        echo("baseline", Loop::Baseline)?;
        echo("drain", Loop::Drain)?;
        echo("drain-nb", Loop::DrainNonblocking)?;
        echo("baseline", Loop::Baseline)?;
        println!();
    }

    Ok(())
}
