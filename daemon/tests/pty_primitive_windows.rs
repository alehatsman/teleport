//! Windows counterpart to `pty_primitive.rs` --
//! [W2](../../docs/15-open-questions.md#w2--windows-fixture-parity-not-yet-attempted).
//!
//! Started narrow (`clean_exit_zero_is_recorded_via_wait_not_eof`,
//! `nonzero_exit_is_recorded_accurately` only) to prove
//! [W1](../../docs/15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows)'s
//! fix through the real production code path. This file now carries the rest
//! of the Windows-appropriate rewrite of `pty_primitive.rs`'s suite --
//! written and run for real on this machine (`cmd.exe`/`powershell.exe`, no
//! `/bin/sh`, no `stty`, no `libc::kill`), not guessed at from the Unix file.
//!
//! **One fixture is deliberately not attempted, not faked:**
//! `large_write_arrives_intact_and_in_order`'s Unix intent -- an arbitrary
//! byte-exact payload (all 256 byte values, including control bytes) survives
//! a raw-mode round trip -- has no meaningful Windows equivalent through
//! `portable_pty`. Unix raw mode (`stty raw -echo`) turns the pty into a
//! plain byte pipe with *no* interpretation. ConPTY has no equivalent "raw"
//! state for its *input* direction: every byte written to the master always
//! passes through conhost's own VT input parser before the child ever sees
//! it, regardless of what console mode the child sets on its own stdin
//! (`ENABLE_LINE_INPUT` etc. govern how the child's `ReadConsole` batches
//! *already-parsed* key events -- they don't reach back and turn off VT
//! parsing upstream of that). A payload like `(0..N).map(|i| i % 256)`
//! contains `ESC` (0x1B) roughly every 256 bytes, which conhost's parser
//! reads as the start of a control sequence and consumes together with
//! however many following bytes it takes to (fail to) recognize one --
//! this is not a corner case, it is guaranteed by the payload's own
//! construction. There is no cmd.exe/PowerShell knob and no child-side
//! `SetConsoleMode` that bypasses it. `large_text_burst_read_drops_nothing`
//! and `echo_roundtrip` below cover what the platform actually allows:
//! ordinary text through the same reader/writer machinery, at real volume.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command as StdCommand;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use teleportd::pty::{self, SpawnSpec, TerminalSession};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Spawns `cmd.exe /c script` (a one-shot script, not an interactive shell)
/// via the production `pty::spawn` path.
fn spawn_cmd_script(script: &str, cols: u16, rows: u16) -> (pty::SpawnedSession, Receiver<Vec<u8>>) {
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let cwd = temp_dir();
    let args = vec!["/c".to_string(), script.to_string()];
    let spec = SpawnSpec {
        program: "cmd.exe",
        args: &args,
        cwd: &cwd,
        env: &[],
        cols,
        rows,
    };
    let spawned = pty::spawn(spec, move |chunk| {
        let _ = out_tx.send(chunk.to_vec());
    })
    .expect("spawn cmd.exe /c");
    (spawned, out_rx)
}

/// Spawns interactive `cmd.exe` (no `/c`) so the test can drive it with
/// multiple `write()`s, same output wiring as `spawn_cmd_script`.
fn spawn_interactive_cmd(cols: u16, rows: u16) -> (pty::SpawnedSession, Receiver<Vec<u8>>) {
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let cwd = temp_dir();
    let spec = SpawnSpec {
        program: "cmd.exe",
        args: &[],
        cwd: &cwd,
        env: &[],
        cols,
        rows,
    };
    let spawned = pty::spawn(spec, move |chunk| {
        let _ = out_tx.send(chunk.to_vec());
    })
    .expect("spawn interactive cmd.exe");
    (spawned, out_rx)
}

/// Accumulates chunks from `rx` until `pred(&acc)` is true, or panics after
/// `timeout` with whatever was collected so far. Same contract as
/// `pty_primitive.rs`'s helper of the same name.
fn recv_until(
    rx: &Receiver<Vec<u8>>,
    timeout: Duration,
    mut pred: impl FnMut(&[u8]) -> bool,
) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut acc = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "timed out waiting for predicate; got {} bytes: {:?}",
                acc.len(),
                String::from_utf8_lossy(&acc)
            );
        }
        match rx.recv_timeout(remaining) {
            Ok(chunk) => {
                acc.extend_from_slice(&chunk);
                if pred(&acc) {
                    return acc;
                }
            }
            Err(_) => panic!(
                "output channel closed before predicate matched; got {} bytes: {:?}",
                acc.len(),
                String::from_utf8_lossy(&acc)
            ),
        }
    }
}

fn contains(acc: &[u8], needle: &str) -> bool {
    acc.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Removes VT/ANSI escape sequences from `input`, leaving everything else
/// (order, byte content) untouched. Recognizes the two forms conhost is
/// actually observed to emit around a session's own output: CSI (`ESC [`
/// ... parameter/intermediate bytes ... one final byte in `@`-`~`) and OSC
/// (`ESC ]` ... up to a BEL terminator). Anything else starting with `ESC`
/// is dropped as a single two-byte sequence -- not a complete VT parser,
/// just enough to see through what this fixture actually encounters (its
/// own session-init burst and the process-exit teardown sequence, see the
/// fixture below).
fn strip_vt_sequences(input: &[u8]) -> Vec<u8> {
    const ESC: u8 = 0x1B;
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] != ESC || i + 1 >= input.len() {
            out.push(input[i]);
            i += 1;
            continue;
        }
        match input[i + 1] {
            b'[' => {
                let mut j = i + 2;
                while j < input.len() && (0x30..=0x3F).contains(&input[j]) {
                    j += 1;
                }
                while j < input.len() && (0x20..=0x2F).contains(&input[j]) {
                    j += 1;
                }
                i = (j + 1).min(input.len()); // consume the final byte, if present
            }
            b']' => {
                let mut j = i + 2;
                while j < input.len() && input[j] != 0x07 {
                    j += 1;
                }
                i = (j + 1).min(input.len()); // consume the BEL terminator, if present
            }
            _ => i += 2,
        }
    }
    out
}

/// Parses the first run of ASCII digits that follows `label` in `text` --
/// enough to read `mode con`'s `Lines:`/`Columns:` fields without depending
/// on its exact column alignment (which varies with the value's own digit
/// count).
fn number_after(text: &str, label: &str) -> Option<u32> {
    let start = text.find(label)? + label.len();
    let rest = &text[start..];
    let digits_start = rest.find(|c: char| c.is_ascii_digit())?;
    let digits_end = rest[digits_start..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| digits_start + i)
        .unwrap_or(rest.len());
    rest[digits_start..digits_end].parse().ok()
}

/// Process ids (there can legitimately be more than one, or none) whose
/// command line contains `marker`, via `Get-CimInstance Win32_Process` --
/// `wmic` is gone from this build (confirmed absent, `where wmic` finds
/// nothing), `Get-CimInstance` is its supported replacement and ships on
/// every Windows 10/11 install.
///
/// The marker is passed through `TP_W2_MARKER` in the *query* process's own
/// environment, not interpolated into its `-Command` script text -- an
/// earlier version did the latter and the query matched itself, every time,
/// forever (its own command line necessarily contains the literal marker
/// it's searching for). `Win32_Process.CommandLine` reflects argv, not the
/// environment block, so this sidesteps that self-match entirely.
fn pids_matching(marker: &str) -> Vec<u32> {
    let out = StdCommand::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -like ('*' + $env:TP_W2_MARKER + '*') } | Select-Object -ExpandProperty ProcessId",
        ])
        .env("TP_W2_MARKER", marker)
        .output()
        .expect("run out-of-band powershell probe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

#[test]
fn echo_roundtrip() {
    let (spawned, out_rx) = spawn_interactive_cmd(80, 24);
    spawned.session.write(b"echo hello\r\n").unwrap();
    recv_until(&out_rx, DEFAULT_TIMEOUT, |acc| contains(acc, "hello"));
}

#[test]
fn large_text_burst_read_drops_nothing() {
    // No `yes`, and no raw mode to avoid ONLCR inflation (see the module doc
    // on why raw-mode byte-exactness isn't attempted at all here) -- a
    // `for /L` loop is cmd.exe's own repeat-N-times primitive, and CRLF line
    // endings are just accepted as what Windows text output actually looks
    // like. 200,000 lines of "y" (with CRLF) is ~1.4 MB -- enough to prove
    // sustained multi-chunk reads (READ_BUFFER_SIZE is 64 KiB, so this is
    // guaranteed to span dozens of reads) without loss, not a byte-for-byte
    // rewrite of the Unix 8 MiB figure.
    //
    // No sentinels here, deliberately -- an earlier version of this fixture
    // bracketed the loop with `echo READY`/`echo DONE` lines and asserted on
    // exact byte offsets between them, and that approach fought the
    // platform on two fronts, both confirmed directly during development:
    // wrapping the loop for `& echo DONE` to bind outside it (needed --
    // without it, "DONE" gets swallowed into the repeated body and prints
    // every iteration) made cmd.exe roughly 15x slower per iteration for
    // reasons not otherwise investigated; and *whichever* WriteConsole call
    // is literally last before the process exits races conhost's own
    // process-exit VT teardown sequence -- tried with nothing after the
    // loop (final "y\r\n" arrived as "y\r" + an escape byte + a late "\n")
    // and with `echo DONE` last instead (identical split, just on "DONE"),
    // so it is a property of being last, not of either sentinel.
    //
    // `strip_vt_sequences` below sidesteps both: it removes every VT escape
    // sequence from the captured output first -- both conhost's session-init
    // burst (`ESC[?9001h`, `ESC[?1004h`, `ESC[m`, an OSC window-title write,
    // ...) and the process-exit teardown sequence that raced the last
    // line -- leaving only the loop's own printable output, which should be
    // *exactly* `LINES` copies of "y\r\n" with nothing else interleaved, in
    // order. No sentinel needed because there is nothing left to skip past.
    const LINES: usize = 200_000;
    let (spawned, out_rx) = spawn_cmd_script(
        &format!("for /L %i in (1,1,{LINES}) do @echo y"),
        80,
        24,
    );

    // The script is a one-shot `cmd /c`, so waiting for the process to exit
    // (rather than a content predicate) is the natural "done reading" signal
    // -- the reader thread's own EOF, independent of and not inferred from
    // this exit (docs/15-open-questions.md#s2--eof-is-not-exit), just used
    // here as this test's stopping point once the exit has also happened.
    spawned
        .exit_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("cmd.exe should exit once the loop finishes");
    spawned
        .eof_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("reader should reach EOF once cmd.exe exits");
    let raw: Vec<u8> = out_rx.try_iter().flatten().collect();

    let stripped = strip_vt_sequences(&raw);
    let expected: Vec<u8> = b"y\r\n".repeat(LINES);
    assert_eq!(
        stripped, expected,
        "content, once conhost's own VT init/teardown sequences are stripped, must be \
         exactly the loop's own output, undamaged, undropped, and in order"
    );
}

#[test]
fn resize_is_observed_by_the_child() {
    let (spawned, out_rx) = spawn_interactive_cmd(80, 24);
    spawned.session.resize(120, 40).unwrap();
    // resize() hands off to the control thread and returns immediately
    // (docs/03-pty-layer.md#resize) -- no ack, so give it a moment to land
    // before asking the child to query its own size, same reasoning as the
    // Unix fixture of the same name.
    std::thread::sleep(Duration::from_millis(200));
    spawned.session.write(b"mode con\r\n").unwrap();
    let acc = recv_until(&out_rx, DEFAULT_TIMEOUT, |acc| {
        contains(acc, "Columns:")
    });
    let text = String::from_utf8_lossy(&acc);
    assert_eq!(
        number_after(&text, "Lines:"),
        Some(40),
        "mode con should report the resized row count: {text}"
    );
    assert_eq!(
        number_after(&text, "Columns:"),
        Some(120),
        "mode con should report the resized column count: {text}"
    );
}

#[test]
fn clean_exit_zero_is_recorded_via_wait_not_eof() {
    let (spawned, _out_rx) = spawn_cmd_script("exit 0", 80, 24);
    let exit = spawned
        .exit_rx
        .recv_timeout(DEFAULT_TIMEOUT)
        .expect("exit_rx should fire -- this is exactly the case W1 found unobserved");
    let status = exit
        .status
        .expect("clean exit must carry a status, not a lost_reason");
    assert_eq!(status.exit_code(), 0);
    assert!(status.success());
    assert!(exit.lost_reason.is_none());
}

#[test]
fn nonzero_exit_is_recorded_accurately() {
    let (spawned, _out_rx) = spawn_cmd_script("exit 7", 80, 24);
    let exit = spawned
        .exit_rx
        .recv_timeout(DEFAULT_TIMEOUT)
        .expect("exit_rx should fire");
    let status = exit.status.expect("exit must carry a status");
    assert_eq!(status.exit_code(), 7);
    assert!(!status.success());
}

#[test]
fn terminate_reaches_exited_within_the_bounded_policy() {
    // A direct child with nothing else attached, blocked well past any
    // bound this test waits on -- `ping`'s own countdown, not a background
    // job (that's the next fixture).
    let (spawned, _out_rx) = spawn_cmd_script("ping -n 31 127.0.0.1 >nul", 80, 24);

    let t0 = Instant::now();
    spawned
        .session
        .terminate()
        .expect("terminate should not error");
    let terminate_latency = t0.elapsed();

    // The graceful step here is ClosePseudoConsole via dropping the master
    // (docs/03-pty-layer.md#concrete-policy step 2, Windows leg). Per
    // docs/10-testing.md's platform matrix this build is >= 24H2, documented
    // to return immediately -- and run alone, this fixture does complete in
    // ~10ms, every time (8/8 solo runs during development). But run
    // concurrently with the rest of this file's suite (several ConPTY
    // sessions being torn down at once), it was caught taking the full
    // GRACEFUL_WAIT (5.0131867s, i.e. it fell through the graceful step
    // entirely to the hard-kill path) -- reproduced once, not chased
    // further given the concurrency needed to trigger it (same shape as
    // [S2](../../docs/15-open-questions.md#s2--eof-is-not-exit)'s ws.rs
    // regression, which also only reproduced under real scheduling
    // contention). So this asserts the actual documented contract --
    // "within the bounded policy" is the fixture's own name -- not the
    // *usual* fast path, which is a real but unproven-reliable property of
    // this build, not a guarantee.
    // GRACEFUL_WAIT (5s) + KILL_WAIT (2s), pty.rs's own bound -- not public,
    // so restated here rather than imported; a small margin (1s) covers
    // scheduling slop around the boundary without weakening what's actually
    // being asserted.
    let documented_bound = Duration::from_secs(5 + 2 + 1);
    assert!(
        terminate_latency < documented_bound,
        "terminate() took {terminate_latency:?}, expected to complete within the documented bounded policy"
    );

    let exit = spawned
        .exit_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("exit_rx should already have fired by the time terminate() returns");
    assert!(
        !exit.status.map(|s| s.success()).unwrap_or(false),
        "a forcibly-closed ping should not report a successful exit"
    );
}

#[test]
fn terminate_under_output_load_does_not_deadlock() {
    // A large-but-finite loop stands in for Unix's `yes`: cmd.exe has no
    // infinite-repeat builtin reachable in one `/c` script, but 50 million
    // iterations of `echo y` produces output continuously for far longer
    // than this test needs, which is all "under load" requires.
    let (spawned, out_rx) = spawn_cmd_script("for /L %i in (1,1,50000000) do @echo y", 80, 24);

    // Let it produce a real burst before terminating, so terminate() is
    // racing an active reader, not an idle one.
    recv_until(&out_rx, DEFAULT_TIMEOUT, |acc| acc.len() >= 256 * 1024);

    let t0 = Instant::now();
    spawned
        .session
        .terminate()
        .expect("terminate should not error");
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "terminate() must not deadlock behind the writer/reader threads"
    );

    spawned
        .exit_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("exit_rx should fire once terminate() returns");

    // The reader thread should still be able to drain and reach EOF -- no
    // lost tail, no stuck reader.
    match spawned.eof_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(()) => {}
        Err(_) => panic!("reader never reached EOF after terminate"),
    }
}

#[test]
fn terminate_kills_the_grandchild_process_tree() {
    // Windows equivalent of the Unix fixture of the same name -- but the
    // mechanism under test is different, per docs/10-testing.md's platform
    // matrix note ("Windows: attached character-mode clients terminate with
    // the pseudoconsole"), not a process-group signal. `start /B` runs the
    // grandchild without a new console/window, i.e. still attached to this
    // session's pseudoconsole, same as a Unix grandchild inheriting the pty
    // by not calling setsid -- the interesting case here is the *opposite*
    // of S2/eof_and_exit_are_independent_signals' detached survivor: this
    // grandchild is deliberately left attached, so closing the pseudoconsole
    // should take it down too.
    //
    // A random marker embedded in the grandchild's own argv (not its output)
    // lets an out-of-band probe (`pids_matching`, a real OS process query --
    // same spirit as the Unix fixture's direct `libc::kill(pid, 0)`, not
    // something read back through the pty) find it precisely, without
    // colliding with an unrelated `powershell.exe`/`Start-Sleep` elsewhere on
    // the machine.
    let marker = format!("TP_W2_MARKER_{}", ulid::Ulid::new());
    let (spawned, _out_rx) = spawn_interactive_cmd(80, 24);
    spawned
        .session
        .write(
            format!(
                "start /B powershell -NoProfile -Command \"Start-Sleep -Seconds 60 # {marker}\"\r\n"
            )
            .as_bytes(),
        )
        .unwrap();

    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    let pids = loop {
        let pids = pids_matching(&marker);
        if !pids.is_empty() {
            break pids;
        }
        assert!(
            Instant::now() < deadline,
            "grandchild process matching the marker never appeared"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    spawned
        .session
        .terminate()
        .expect("terminate should not error");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let still_alive = pids_matching(&marker);
        if still_alive.is_empty() {
            return; // gone -- the whole attached tree closed with the pseudoconsole
        }
        assert!(
            Instant::now() < deadline,
            "grandchild pid(s) {still_alive:?} (originally {pids:?}) were still alive 5s after terminate"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
