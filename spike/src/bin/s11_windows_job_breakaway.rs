//! S11 -- M10 Windows detached-spawn spike: does `CREATE_BREAKAWAY_FROM_JOB`
//! actually let a child survive its parent's job closing?
//!
//! `desktop/src-tauri/src/daemon.rs::spawn_detached`'s whole Windows defense
//! (`CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB`)
//! was written against the MSDN description of job-object nesting
//! (docs/11-mvp-plan.md#m10's edge-case bullet: "if this app's own process
//! is inside a job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`... children join
//! that job by default since Windows 8 and die with it") but flagged
//! "unverified on real Windows -- spike this first, on a packaged Windows
//! build, not `cargo run`... it can look fine in dev and fail only once
//! bundled." This is that spike, run on real hardware (native Windows,
//! `x86_64-pc-windows-gnu` host toolchain, no cross-compile), not trusted
//! from the MSDN description alone.
//!
//! Not literally `tauri build`-packaged -- this exercises the identical
//! Win32 API call and the identical creation flags `spawn_detached` uses,
//! standalone, which is the part that was actually in question (whether
//! `CREATE_BREAKAWAY_FROM_JOB` does what the doc comment says on this real
//! machine). The packaging step around it doesn't change what
//! `CreateProcessW` does with these flags.
//!
//! Scenario, four arms:
//!
//!   1. The "parent" process creates an unnamed Job Object with
//!      `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assigns *itself* to it --
//!      simulating "launched from an IDE, CI runner, or installer context"
//!      without needing an actual IDE to reproduce it (nested job objects
//!      are supported since Windows 8, so this works regardless of whatever
//!      job this spike's own process might already be in). Skipped
//!      entirely for the `no_job` arm -- see below.
//!   2. It spawns a long-lived "child" (a second invocation of this same
//!      binary, in `child` mode) with the flags named by the arm.
//!   3. It prints the child's pid, then returns from `main` -- an ordinary
//!      process exit, which drops this process's job handle (if any).
//!      Since it's the job's only handle, that's exactly what triggers
//!      `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
//!   4. The child, meanwhile, appends a timestamped line to `marker_file`
//!      every 200ms for up to 6s, then exits on its own. A driver script
//!      (not this binary) reads the marker file a couple of seconds after
//!      the parent has exited: still growing means the child survived;
//!      empty/frozen means the job killed it (or the spawn never happened).
//!
//! The four arms, and what each actually showed on this machine
//! (`x86_64-pc-windows-gnu`, native, 2026-09-05):
//!
//!   - `no_breakaway` (control: detached, no breakaway flag) -- child dies
//!     essentially instantly. Confirms the job's kill-on-close mechanism
//!     genuinely fires in this setup, so the other arms aren't vacuous.
//!   - `breakaway` (the exact flags `daemon.rs::spawn_detached` shipped
//!     with) against a job that does **not** grant
//!     `JOB_OBJECT_LIMIT_BREAKAWAY_OK` -- **the spawn itself fails**,
//!     `CreateProcess`/`Command::spawn` returning `ERROR_ACCESS_DENIED`
//!     (`io::ErrorKind::PermissionDenied`). This was the actual finding:
//!     `CREATE_BREAKAWAY_FROM_JOB` is not a silent no-op when the
//!     containing job disallows it, it is a hard failure of the whole
//!     spawn -- see `daemon.rs::spawn_windows_with_breakaway_retry`, added
//!     in response.
//!   - `breakaway_allowed` (same job, but with
//!     `JOB_OBJECT_LIMIT_BREAKAWAY_OK` also set) -- spawn succeeds, child
//!     outlives the parent. The mechanism itself works correctly once
//!     permitted; the earlier failure is specifically a permission gate.
//!   - `no_job` (no containing job at all -- a plain double-clicked
//!     desktop launch, the common real-world case) -- spawn succeeds,
//!     child outlives the parent. `CREATE_BREAKAWAY_FROM_JOB` is harmless
//!     when there's nothing to break away from.
//!
//! Usage:
//!   s11_windows_job_breakaway.exe parent <breakaway|breakaway_allowed|no_breakaway|no_job> <marker_file>
//!   s11_windows_job_breakaway.exe child <marker_file>   (internal -- spawned by `parent`)

#![cfg(windows)]

use std::fs::OpenOptions;
use std::io::Write;
use std::ptr;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
};

fn child_main(marker_file: &str) {
    let pid = std::process::id();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker_file)
        .expect("open marker file");
    let start = Instant::now();
    let mut tick = 0u32;
    while start.elapsed() < Duration::from_secs(6) {
        writeln!(
            file,
            "pid={pid} tick={tick} elapsed_ms={}",
            start.elapsed().as_millis()
        )
        .expect("write marker line");
        file.flush().expect("flush marker file");
        tick += 1;
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn parent_main(arm: &str, marker_file: &str) {
    // Truncate any marker file from a previous run so the driver script
    // isn't fooled by stale content.
    std::fs::write(marker_file, "").expect("truncate marker file");

    // "no_job": the common real-world case for a double-clicked desktop app
    // -- launched directly by explorer.exe, no containing job at all. Skips
    // job setup entirely, to prove CREATE_BREAKAWAY_FROM_JOB is harmless
    // when there is nothing to break away from (vs. the other three arms,
    // which simulate being launched inside one).
    let job = if arm != "no_job" {
        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        assert!(!job.is_null(), "CreateJobObjectW failed");

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = if arm == "breakaway_allowed" {
            // The containing job explicitly permits member processes to
            // break away -- isolates "does CREATE_BREAKAWAY_FROM_JOB work at
            // all" from "is it permitted by *this* job", which the plain
            // `breakaway` arm's ACCESS_DENIED result showed are two
            // different questions.
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK
        } else {
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        };
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        assert!(
            ok != 0,
            "SetInformationJobObject failed: {:?}",
            std::io::Error::last_os_error()
        );

        // Assign *this* process (not the child) to the job -- simulating
        // being launched inside a job-owning parent (an IDE, CI runner,
        // some installer contexts), exactly the scenario daemon.rs's module
        // doc names.
        let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
        assert!(
            assigned != 0,
            "AssignProcessToJobObject(self) failed: {:?}",
            std::io::Error::last_os_error()
        );
        eprintln!(
            "[s11] parent (pid={}) assigned to a KILL_ON_JOB_CLOSE job",
            std::process::id()
        );
        Some(job)
    } else {
        None
    };

    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("child").arg(marker_file);

    use std::os::windows::process::CommandExt;
    let flags = match arm {
        // The exact flags daemon.rs::spawn_detached uses -- including
        // "no_job", the common real-world case of a plain double-clicked
        // launch with no containing job at all.
        "breakaway" | "breakaway_allowed" | "no_job" => {
            CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB
        }
        // The control: same detachment, no breakaway -- should still join
        // this process's job and die with it, proving the job is real.
        "no_breakaway" => CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS,
        other => panic!(
            "unknown arm {other} -- expected breakaway|breakaway_allowed|no_breakaway|no_job"
        ),
    };
    cmd.creation_flags(flags);

    // Deliberately never wait()ed on -- this spike is specifically about
    // what happens when the parent exits *without* waiting on the child, so
    // a real detached process (potential "zombie" by clippy's generic
    // heuristic) is the point, not an oversight.
    #[allow(clippy::zombie_processes)]
    let child = cmd.spawn().expect("spawn child");
    println!("child_pid={}", child.id());
    eprintln!(
        "[s11] spawned child pid={} arm={arm} flags={flags:#x}, parent now exiting normally",
        child.id()
    );

    // Deliberately do not wait() on the child, and deliberately do not close
    // `job` early -- an ordinary `main` return right here, dropping this
    // process's only handle to the job (if any), is the event under test.
    // Closing it any earlier than this would fire KILL_ON_JOB_CLOSE before
    // the child even exists.
    if let Some(job) = job {
        unsafe { CloseHandle(job) };
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("parent") => {
            let arm = args.get(2).expect(
                "usage: parent <breakaway|breakaway_allowed|no_breakaway|no_job> <marker_file>",
            );
            let marker_file = args.get(3).expect("usage: parent <arm> <marker_file>");
            parent_main(arm, marker_file);
        }
        Some("child") => {
            let marker_file = args.get(2).expect("usage: child <marker_file>");
            child_main(marker_file);
        }
        _ => {
            eprintln!(
                "usage:\n  s11_windows_job_breakaway parent <breakaway|no_breakaway> <marker_file>\n  s11_windows_job_breakaway child <marker_file>"
            );
            std::process::exit(2);
        }
    }
}
