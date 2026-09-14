//! `Session::title`/`Session::claude_resume_id` -- terminal-title and
//! Claude-resume-link detection (`session/osc.rs`), end-to-end through a
//! real spawned process. `daemon/src/session/osc.rs`'s own unit tests cover
//! the scanner's parsing edge cases directly; this file is the "does it
//! actually reach `Session` from real PTY output" gate, same division of
//! labor as `attention_signals.rs`'s BEL test vs. the byte-scan itself.

#![cfg(unix)]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use teleportd::pty::SpawnSpec;
use teleportd::session::SessionManager;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

fn sessions_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "teleportd-osc-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_nanos()
    ))
}

fn spec<'a>(args: &'a [String], cwd: &'a PathBuf) -> SpawnSpec<'a> {
    SpawnSpec {
        program: "/bin/sh",
        args,
        cwd,
        env: &[],
        cols: 80,
        rows: 24,
        login_shell: false,
    }
}

#[tokio::test]
async fn a_title_escape_sequence_in_the_output_sets_title() {
    let manager = SessionManager::new(sessions_root("title"));
    let cwd = temp_dir();
    let args = vec![];
    let session = manager
        .create(&spec(&args, &cwd), "shell", None)
        .expect("create session");

    assert_eq!(session.title(), None, "nothing has set a title yet");

    // OSC 0 ("set icon name and window title"), BEL-terminated -- the exact
    // form Claude Code's own startup banner uses.
    session
        .write(b"printf '\\033]0;Say hello\\007'\n")
        .expect("write");

    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while session.title().as_deref() != Some("Say hello") {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the title, last seen: {:?}",
            session.title()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A later title replaces the earlier one outright -- only the most
    // recent is ever useful.
    session
        .write(b"printf '\\033]0;Now doing something else\\007'\n")
        .expect("write");
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while session.title().as_deref() != Some("Now doing something else") {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the updated title, last seen: {:?}",
            session.title()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_claude_resume_link_in_the_output_sets_claude_resume_id() {
    let manager = SessionManager::new(sessions_root("resume-id"));
    let cwd = temp_dir();
    let args = vec![];
    let session = manager
        .create(&spec(&args, &cwd), "shell", None)
        .expect("create session");

    assert_eq!(session.claude_resume_id(), None);

    // OSC 8 hyperlink, same shape Claude Code's own startup banner emits.
    session
        .write(b"printf '\\033]8;id=x;https://claude.ai/code/session_ABC123?from=cli\\007'\n")
        .expect("write");

    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while session.claude_resume_id().as_deref() != Some("session_ABC123") {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the resume id, last seen: {:?}",
            session.claude_resume_id()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A resumable-conversation link is a Claude Code thing specifically -- an
/// unrelated hyperlink (anything else built on OSC 8, which has plenty of
/// legitimate uses) must never be mistaken for one.
#[tokio::test]
async fn an_unrelated_hyperlink_does_not_set_claude_resume_id() {
    let manager = SessionManager::new(sessions_root("unrelated-link"));
    let cwd = temp_dir();
    let args = vec![];
    let session = manager
        .create(&spec(&args, &cwd), "shell", None)
        .expect("create session");

    session
        .write(b"printf '\\033]8;;https://example.com/whatever\\007'\n")
        .expect("write");

    // No positive event to wait on for a "this must stay None" assertion --
    // wait for the write to have visibly landed instead (next_offset moves
    // once the shell's own output, including the OSC bytes, is logged),
    // then assert.
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while session.next_offset() == 0 {
        assert!(Instant::now() < deadline, "timed out waiting for output");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(session.claude_resume_id(), None);
}
