//! Incremental scanner for two OSC (Operating System Command) escape
//! sequences PTY output can carry, across chunk boundaries: OSC 0/2 ("set
//! window title") and OSC 8 (hyperlink), the latter filtered down to a
//! Claude Code resumable-conversation link. Not a general VT/ANSI parser --
//! every other escape sequence, including OSC codes this doesn't care
//! about, is recognized only well enough to skip cleanly past without
//! misreading its payload as one of these two.
//!
//! Both are genuinely useful (a short live "what is this session doing"
//! summary for the session list, and a one-tap way to resume the exact
//! conversation a dead session was having -- docs/11-mvp-plan.md#m8--agent-presets),
//! but both rest on Claude Code's own current terminal output format, which
//! is not a documented or versioned contract. A future CLI release changing
//! or dropping either sequence degrades silently back to "no title" / "no
//! resume id" -- never a parse error, never a broken session.

use std::mem;

/// Caps how much of a single OSC payload this ever buffers. A malformed or
/// adversarial stream that opens a sequence and never closes it must not
/// grow this unbounded in memory; real titles and URLs are a few dozen
/// bytes, this is generous headroom, not a tight fit.
const MAX_PAYLOAD_BYTES: usize = 512;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    /// Just saw ESC; still deciding whether this is an OSC (`]`) sequence
    /// at all.
    Esc,
    /// Saw "ESC ]"; accumulating the numeric `Ps` parameter before the
    /// first `;`.
    Param(u16),
    /// Saw "ESC ] Ps ;"; buffering `Pt` until a terminator.
    Payload { code: u16, buf: Vec<u8> },
    /// Mid-payload, just saw ESC -- checking whether it's `ESC \` (the
    /// String Terminator) or just a byte that happens to be 0x1b.
    PayloadEsc { code: u16, buf: Vec<u8> },
}

/// One update found in a chunk of PTY output.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum OscUpdate {
    /// OSC 0 or 2 -- xterm "set window title" (0 sets the icon name too;
    /// nothing here has a use for that half).
    Title(String),
    /// OSC 8 whose URI is a Claude Code resumable-conversation link
    /// (`https://claude.ai/code/session_<id>...`) -- already extracted
    /// down to just `session_<id>`, not the whole URL.
    ClaudeResumeId(String),
}

#[derive(Default)]
pub(super) struct OscScanner {
    state: State,
}

impl OscScanner {
    /// Feeds one chunk of raw PTY output. Returns every update found in
    /// this chunk, in order -- almost always zero or one in practice, but
    /// nothing stops a chunk from carrying more than one full sequence.
    pub(super) fn feed(&mut self, chunk: &[u8]) -> Vec<OscUpdate> {
        let mut updates = Vec::new();
        for &b in chunk {
            match &mut self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    self.state = if b == b']' {
                        State::Param(0)
                    } else {
                        State::Ground
                    };
                }
                State::Param(code) => match b {
                    b'0'..=b'9' => {
                        *code = code.saturating_mul(10).saturating_add(u16::from(b - b'0'));
                    }
                    b';' => {
                        self.state = State::Payload {
                            code: *code,
                            buf: Vec::new(),
                        }
                    }
                    // No ';' before something else entirely (including
                    // another ESC, a BEL with no param, ...) -- malformed
                    // as far as this scanner's concerned; bail to Ground
                    // rather than guess.
                    _ => self.state = State::Ground,
                },
                State::Payload { code, buf } => match b {
                    0x07 => {
                        updates.extend(Self::finish(*code, buf));
                        self.state = State::Ground;
                    }
                    0x1b => {
                        self.state = State::PayloadEsc {
                            code: *code,
                            buf: mem::take(buf),
                        }
                    }
                    _ if buf.len() < MAX_PAYLOAD_BYTES => buf.push(b),
                    // Cap hit -- this sequence has gone on far longer than
                    // any real title or URL would; abandon it rather than
                    // buffer forever.
                    _ => self.state = State::Ground,
                },
                State::PayloadEsc { code, buf } => {
                    if b == b'\\' {
                        updates.extend(Self::finish(*code, buf));
                    }
                    // Either a real ST (String Terminator) just closed the
                    // sequence, or this ESC wasn't one and the sequence is
                    // malformed -- either way, nothing more to do with it.
                    self.state = State::Ground;
                }
            }
        }
        updates
    }

    fn finish(code: u16, buf: &[u8]) -> Option<OscUpdate> {
        let text = String::from_utf8_lossy(buf);
        match code {
            0 | 2 => Some(OscUpdate::Title(text.into_owned())),
            // OSC 8's payload is "params;URI" -- params (often empty, or an
            // "id=..." grouping key for a link split across output calls)
            // is irrelevant here, only the URI matters.
            8 => {
                let uri = text.split_once(';').map_or(text.as_ref(), |(_, uri)| uri);
                extract_claude_resume_id(uri).map(OscUpdate::ClaudeResumeId)
            }
            _ => None,
        }
    }
}

/// `https://claude.ai/code/session_<id>[?...]` -> `session_<id>`. Any other
/// URI -- OSC 8 has plenty of legitimate uses that have nothing to do with
/// this -- yields `None`.
fn extract_claude_resume_id(uri: &str) -> Option<String> {
    let after = uri.strip_prefix("https://claude.ai/code/")?;
    let id = after.split(['?', '#']).next().unwrap_or(after);
    id.starts_with("session_").then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_in_one_chunk() {
        let mut s = OscScanner::default();
        let updates = s.feed(b"\x1b]0;\xe2\x9c\xb3 Claude Code\x07");
        assert_eq!(updates, vec![OscUpdate::Title("✳ Claude Code".to_string())]);
    }

    #[test]
    fn title_split_across_chunks() {
        // The exact failure mode this scanner exists for -- a PTY read can
        // land anywhere, including mid-escape-sequence.
        let mut s = OscScanner::default();
        assert_eq!(s.feed(b"\x1b]2;Say "), vec![]);
        assert_eq!(
            s.feed(b"hello\x07"),
            vec![OscUpdate::Title("Say hello".to_string())]
        );
    }

    #[test]
    fn title_terminated_by_st_not_bel() {
        let mut s = OscScanner::default();
        let updates = s.feed(b"\x1b]0;title\x1b\\");
        assert_eq!(updates, vec![OscUpdate::Title("title".to_string())]);
    }

    #[test]
    fn osc_8_claude_resume_link_is_extracted() {
        let mut s = OscScanner::default();
        let updates = s.feed(
            b"\x1b]8;id=gq2qz2;https://claude.ai/code/session_013YMNvvZ2U1fUjQsxAbqRjj?from=cli\x07",
        );
        assert_eq!(
            updates,
            vec![OscUpdate::ClaudeResumeId(
                "session_013YMNvvZ2U1fUjQsxAbqRjj".to_string()
            )]
        );
    }

    #[test]
    fn osc_8_hyperlink_close_marker_is_not_a_resume_id() {
        // `\x1b]8;;\x07` -- empty params, empty URI, closes the previous
        // hyperlink run. Must not be misread as anything.
        let mut s = OscScanner::default();
        assert_eq!(s.feed(b"\x1b]8;;\x07"), vec![]);
    }

    #[test]
    fn osc_8_unrelated_hyperlink_is_ignored() {
        let mut s = OscScanner::default();
        let updates = s.feed(b"\x1b]8;;https://example.com/whatever\x07");
        assert_eq!(updates, vec![]);
    }

    #[test]
    fn unrelated_osc_codes_are_skipped_without_confusing_the_scanner() {
        // A title right after some other OSC this scanner has no opinion
        // about must still be found.
        let mut s = OscScanner::default();
        let updates = s.feed(b"\x1b]9;some other OSC entirely\x07\x1b]0;real title\x07");
        assert_eq!(updates, vec![OscUpdate::Title("real title".to_string())]);
    }

    #[test]
    fn plain_output_and_unrelated_escape_sequences_produce_nothing() {
        let mut s = OscScanner::default();
        // A run-of-the-mill shell prompt plus a CSI cursor-movement
        // sequence (ESC [ ... ), never an OSC at all.
        assert_eq!(s.feed(b"$ echo hi\r\nhi\r\n\x1b[2K\x1b[1G$ "), vec![]);
    }

    #[test]
    fn an_unterminated_osc_payload_is_bounded_not_buffered_forever() {
        let mut s = OscScanner::default();
        let mut huge = b"\x1b]0;".to_vec();
        huge.extend(std::iter::repeat_n(b'x', MAX_PAYLOAD_BYTES * 4));
        // No terminator at all -- must not panic, allocate unbounded
        // memory, or (once it gives up) misfire on the next real sequence.
        assert_eq!(s.feed(&huge), vec![]);
        let updates = s.feed(b"\x1b]0;fine\x07");
        assert_eq!(updates, vec![OscUpdate::Title("fine".to_string())]);
    }
}
