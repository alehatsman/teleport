//! S6 spike -- W1 follow-up: does a Job Object + I/O completion port see a
//! graceful ConPTY-child exit that `wait()`/`try_wait()` and reader EOF do not?
//!
//! docs/15-open-questions.md#w1 confirmed: `GetExitCodeProcess`/`WaitForSingleObject`
//! on the child's own process handle (what `portable-pty`'s `WinChild::wait()` uses)
//! never returns for a ConPTY child that exits *on its own*, and the pty master never
//! sees EOF either -- even though the process is genuinely OS-resident and idle (0.00
//! CPU), not spinning. Two leads were left open, in priority order; this spike is the
//! first one: `JOB_OBJECT_MSG_EXIT_PROCESS` notifications come from the kernel's job
//! object accounting, a different code path from `GetExitCodeProcess`/
//! `WaitForSingleObject` on the process handle -- worth trying since it's independent
//! of whatever ConPTY-side handshake `ExitProcess` seems to be stuck on.
//!
//! Mechanism (per MSDN "Process Termination Notification" for job objects):
//!   1. CreateJobObjectW -- an unnamed job.
//!   2. AssignProcessToJobObject(job, hProcess) -- hProcess needs PROCESS_SET_QUOTA |
//!      PROCESS_TERMINATE, opened via OpenProcess(pid). portable-pty's Child doesn't
//!      expose the raw handle it holds internally, so this re-opens the process by
//!      pid instead of reusing that handle -- a second, independent handle to the same
//!      process, not a substitute for portable-pty's own.
//!   3. CreateIoCompletionPort(INVALID_HANDLE_VALUE, ...) -- a fresh IOCP, not tied to
//!      any file handle yet.
//!   4. SetInformationJobObject(job, JobObjectAssociateCompletionPortInformation, ...)
//!      wires the job to that IOCP with an arbitrary CompletionKey.
//!   5. GetQueuedCompletionStatus on a dedicated thread. For job notifications the
//!      four out-params are repurposed (this is the confusing part, and exactly why
//!      it's spiked here rather than assumed from memory):
//!        - lpNumberOfBytesTransferred receives the message ID (JOB_OBJECT_MSG_*)
//!        - lpCompletionKey receives our CompletionKey (job identity)
//!        - lpOverlapped's *value* (not what it points to) equals the process ID,
//!          cast through the pointer-sized slot
//!
//! JOB_OBJECT_MSG_* aren't present in windows-sys's JobObjects module (checked:
//! JobObjectAssociateCompletionPortInformation is there as a JOBOBJECTINFOCLASS
//! variant, the message IDs are not, they're a separate #define family in winnt.h)
//! -- hardcoded below from the Win32 header values, with the MSDN citation attached
//! to each so a wrong constant is falsifiable rather than a silent guess.
//!
//! Usage: s6_job_object <exit0|exit7|sigkill>

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::path::PathBuf;
use std::ptr;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectAssociateCompletionPortInformation,
    SetInformationJobObject, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus};

// https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_limit_information
// and https://learn.microsoft.com/en-us/windows/win32/procthread/job-object-notifications --
// these message IDs aren't exposed by windows-sys, hardcoded from winnt.h.
const JOB_OBJECT_MSG_END_OF_JOB_TIME: u32 = 1;
const JOB_OBJECT_MSG_END_OF_PROCESS_TIME: u32 = 2;
const JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT: u32 = 3;
const JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO: u32 = 4;
const JOB_OBJECT_MSG_NEW_PROCESS: u32 = 6;
const JOB_OBJECT_MSG_EXIT_PROCESS: u32 = 7;
const JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;

fn msg_name(msg: u32) -> &'static str {
    match msg {
        JOB_OBJECT_MSG_END_OF_JOB_TIME => "END_OF_JOB_TIME",
        JOB_OBJECT_MSG_END_OF_PROCESS_TIME => "END_OF_PROCESS_TIME",
        JOB_OBJECT_MSG_ACTIVE_PROCESS_LIMIT => "ACTIVE_PROCESS_LIMIT",
        JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO => "ACTIVE_PROCESS_ZERO",
        JOB_OBJECT_MSG_NEW_PROCESS => "NEW_PROCESS",
        JOB_OBJECT_MSG_EXIT_PROCESS => "EXIT_PROCESS",
        JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS => "ABNORMAL_EXIT_PROCESS",
        _ => "UNKNOWN",
    }
}

fn mini_exit_path() -> Result<PathBuf> {
    let mut path = std::env::current_exe().context("current_exe")?;
    path.pop();
    path.push("mini_exit.exe");
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
        "sigkill" => cmd.arg("0"),
        other => panic!("unknown scenario {other}"),
    };

    let mut child = pair.slave.spawn_command(cmd)?;
    let pid = child
        .process_id()
        .expect("spawned child has a pid on windows");
    eprintln!("[s6] scenario={scenario} exe={mini_exit:?} pid={pid}");

    // --- Job Object + IOCP wiring, before the child gets far ---
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    anyhow::ensure!(!job.is_null(), "CreateJobObjectW failed");

    // INVALID_HANDLE_VALUE as the file handle == "create a fresh IOCP, not tied to
    // any file", per MSDN. windows-sys doesn't name that constant for this call
    // site conveniently, so spell it directly: (-1isize) as HANDLE.
    let invalid_handle = (-1isize) as windows_sys::Win32::Foundation::HANDLE;
    const JOB_COMPLETION_KEY: usize = 0xC0FFEE;
    let iocp =
        unsafe { CreateIoCompletionPort(invalid_handle, ptr::null_mut(), JOB_COMPLETION_KEY, 1) };
    anyhow::ensure!(!iocp.is_null(), "CreateIoCompletionPort failed");

    let assoc = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
        CompletionKey: JOB_COMPLETION_KEY as *mut core::ffi::c_void,
        CompletionPort: iocp,
    };
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectAssociateCompletionPortInformation,
            &assoc as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
        )
    };
    anyhow::ensure!(
        ok != 0,
        "SetInformationJobObject failed: {:?}",
        std::io::Error::last_os_error()
    );

    let hprocess = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
    anyhow::ensure!(
        !hprocess.is_null(),
        "OpenProcess({pid}) failed: {:?}",
        std::io::Error::last_os_error()
    );

    let assigned = unsafe { AssignProcessToJobObject(job, hprocess) };
    anyhow::ensure!(
        assigned != 0,
        "AssignProcessToJobObject failed: {:?}",
        std::io::Error::last_os_error()
    );
    eprintln!("[s6] process assigned to job object, IOCP wired");

    // --- The three signals, raced exactly like s5, plus the new one ---
    let t0 = Instant::now();

    // 1. reader EOF
    let mut reader = pair.master.try_clone_reader()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    eprintln!("[s6] reader EOF at {}ms", t0.elapsed().as_millis());
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[s6] reader error at {}ms: {e}", t0.elapsed().as_millis());
                    break;
                }
            }
        }
    });

    // 2. IOCP job-object notifications -- log every message, not just EXIT_PROCESS,
    // since ACTIVE_PROCESS_ZERO or an unexpected message would also be informative.
    // HANDLE (*mut c_void) isn't Send; ship it as a usize and cast back inside the
    // thread -- it's a plain OS handle value, not aliased/dereferenced Rust memory,
    // so this is sound.
    let iocp_addr = iocp as usize;
    let iocp_thread = std::thread::spawn(move || {
        let iocp = iocp_addr as windows_sys::Win32::Foundation::HANDLE;
        loop {
            let mut bytes: u32 = 0;
            let mut key: usize = 0;
            let mut overlapped: *mut windows_sys::Win32::System::IO::OVERLAPPED = ptr::null_mut();
            let ok = unsafe {
                GetQueuedCompletionStatus(iocp, &mut bytes, &mut key, &mut overlapped, 13_000)
            };
            let elapsed = t0.elapsed().as_millis();
            if ok == 0 {
                eprintln!(
                    "[s6] GetQueuedCompletionStatus timed out/failed at {elapsed}ms: {:?}",
                    std::io::Error::last_os_error()
                );
                return None;
            }
            let msg = bytes;
            let observed_pid = overlapped as usize as u32; // per MSDN: the pointer VALUE is the pid
            eprintln!(
                "[s6] IOCP message={} ({}) key={key:#x} pid={observed_pid} at {elapsed}ms",
                msg,
                msg_name(msg)
            );
            if msg == JOB_OBJECT_MSG_EXIT_PROCESS || msg == JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS {
                return Some((msg, elapsed));
            }
            // keep looping -- NEW_PROCESS or ACTIVE_PROCESS_ZERO etc. might arrive first
        }
    });

    let trigger = Instant::now();
    if scenario == "sigkill" {
        std::thread::sleep(Duration::from_millis(200));
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status();
    }
    let trigger = if scenario == "sigkill" {
        Instant::now()
    } else {
        trigger
    };

    // 3. dedicated blocking wait() thread -- the production model, same as s5.
    let waiter = std::thread::spawn(move || child.wait());

    let start = Instant::now();
    loop {
        if waiter.is_finished() {
            break;
        }
        if start.elapsed() > Duration::from_secs(12) {
            eprintln!("[s6] wait() TIMEOUT after 12s -- still waiting on IOCP thread");
            break;
        }
        std::thread::sleep(Duration::from_millis(75));
    }
    if waiter.is_finished() {
        let observed_at = Instant::now();
        let status = waiter.join().expect("waiter thread panicked");
        let latency = observed_at.saturating_duration_since(trigger);
        match status {
            Ok(status) => eprintln!(
                "[s6] wait() RESULT exit_code={:?} success={} latency_ms={}",
                status.exit_code(),
                status.success(),
                latency.as_millis()
            ),
            Err(e) => eprintln!("[s6] wait() error: {e}"),
        }
    } else {
        eprintln!("[s6] wait() never returned (consistent with W1) -- leaving its thread parked, process exits with it");
    }

    // Give the IOCP thread a chance to report even if wait() already gave up --
    // that's the whole point of this spike.
    match iocp_thread.join().expect("iocp thread panicked") {
        Some((msg, elapsed)) => eprintln!(
            "[s6] IOCP RESULT: {} at {elapsed}ms (trigger-relative would need trigger={}ms)",
            msg_name(msg),
            trigger.duration_since(t0).as_millis()
        ),
        None => eprintln!("[s6] IOCP RESULT: no exit notification within its own timeout"),
    }

    unsafe {
        CloseHandle(hprocess);
        CloseHandle(job);
        CloseHandle(iocp);
    }

    // Don't wait on the process itself here beyond what's already been tried above --
    // if wait() is stuck, exiting this spike process is fine, the OS cleans up our
    // handles. Force-exit rather than let a possibly-still-running non-daemon thread
    // (the parked wait() thread) hold the process open.
    std::process::exit(0);
}
