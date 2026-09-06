//! `teleport attach <id>` -- the ssh-like loop (docs/11-mvp-plan.md#m11--cli-client,
//! docs/04-api-protocol.md#websocket-protocol). Raw-mode passthrough: local
//! keystrokes go to the remote PTY as the exact bytes the local terminal
//! already produced for them, and remote output goes to local stdout
//! unparsed. No VT emulator here -- the user's own terminal is one
//! (docs/13-native-clients.md#the-protocol-is-already-native-ready).

use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::tty::IsTty;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::connect::Connection;

/// Jittered exponential backoff -- same min/max/doubling shape as
/// `web/src/lib/stream.ts`'s reconnect logic. The jitter source itself is
/// not the same (see `jitter_ms`) -- a CLI has no `Math.random()` -- but the
/// pacing a dropped link sees is.
const BACKOFF_MIN_MS: u64 = 250;
const BACKOFF_MAX_MS: u64 = 8000;

/// The per-attach identity a reconnect keeps constant. Bundled so
/// `connect_and_run` doesn't take a fistful of same-typed `&str` params in a
/// row (an easy silent-argument-order mistake that would still type-check).
struct AttachParams<'a> {
    conn: &'a Connection,
    session_id: &'a str,
    client_id: &'a str,
    client_name: &'a str,
}

/// The two bits of state one connection attempt hands to the next.
struct LoopState {
    raw_mode_entered: bool,
    backoff_ms: u64,
}

pub async fn run(
    conn: &Connection,
    session_id: &str,
    client_id: &str,
    client_name: &str,
) -> Result<i32> {
    let params = AttachParams {
        conn,
        session_id,
        client_id,
        client_name,
    };
    let interactive = std::io::stdin().is_tty() && std::io::stdout().is_tty();
    let mut state = LoopState {
        raw_mode_entered: false,
        backoff_ms: BACKOFF_MIN_MS,
    };
    let mut after: Option<u64> = None;

    let result = loop {
        match connect_and_run(&params, after, interactive, &mut state).await {
            Ok(Outcome::Detach) => break Ok(0),
            Ok(Outcome::Exit(code)) => break Ok(code),
            Ok(Outcome::Reconnect { after: next_after }) => {
                after = Some(next_after);
                continue;
            }
            Err(ConnectError::Fatal { status, body }) => {
                let hint = if status == 401 || status == 403 {
                    "\ncheck --token / TELEPORT_TOKEN"
                } else {
                    ""
                };
                break Err(anyhow::anyhow!(
                    "attach failed: {status}{}{hint}",
                    body.map(|b| format!(" -- {b}")).unwrap_or_default()
                ));
            }
            Err(ConnectError::Transient(e)) => {
                if state.raw_mode_entered {
                    eprintln!("\r\n[teleport: connection lost ({e}), reconnecting...]\r");
                } else {
                    eprintln!("teleport: connect failed ({e}), retrying...");
                }
                tokio::time::sleep(Duration::from_millis(
                    state.backoff_ms + jitter_ms(state.backoff_ms),
                ))
                .await;
                state.backoff_ms = (state.backoff_ms * 2).min(BACKOFF_MAX_MS);
                continue;
            }
        }
    };

    if state.raw_mode_entered {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    result
}

/// Up to 25% of `backoff_ms`, mixed with this process's pid so that several
/// `teleport attach` processes on the same machine -- disconnected by the
/// same daemon restart or network blip, so starting from nearly the same
/// wall-clock instant -- don't all compute nearly the same delay and
/// reconnect in a correlated burst. Not a CSPRNG and doesn't need to be:
/// this exists to decorrelate concurrent processes, not to be
/// unpredictable.
fn jitter_ms(backoff_ms: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let mixed = nanos.wrapping_add(std::process::id() as u64 * 0x9E3779B1);
    mixed % (backoff_ms / 4 + 1)
}

enum Outcome {
    Detach,
    Exit(i32),
    Reconnect { after: u64 },
}

enum ConnectError {
    /// Not worth retrying: bad request, unknown session, bad credential.
    Fatal {
        status: u16,
        body: Option<String>,
    },
    Transient(anyhow::Error),
}

impl From<anyhow::Error> for ConnectError {
    fn from(e: anyhow::Error) -> Self {
        ConnectError::Transient(e)
    }
}

#[derive(Debug, Deserialize)]
struct ReadyFrame {
    truncated: bool,
    control: bool,
    controller: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Ready(ReadyFrame),
    ControlGranted,
    ControlRevoked {
        to: String,
        #[allow(dead_code)]
        client_id: String,
    },
    Resized {
        #[allow(dead_code)]
        cols: u16,
        #[allow(dead_code)]
        rows: u16,
    },
    Exit {
        code: Option<i32>,
        #[allow(dead_code)]
        final_offset: u64,
    },
    Error {
        code: String,
        message: Option<String>,
    },
}

/// Builds the `stream` WebSocket URL from an HTTP(S) base URL, swapping the
/// scheme via `Url::set_scheme` rather than string prefix-matching -- a
/// differently-cased scheme or one `connect::resolve` didn't validate still
/// round-trips correctly, or fails loudly instead of silently defaulting to
/// `ws://`. Pure and unit-tested below for exactly that reason.
fn build_stream_url(
    base_url: &str,
    session_id: &str,
    after: Option<u64>,
    client_id: &str,
    client_name: &str,
) -> Result<reqwest::Url> {
    let base = reqwest::Url::parse(base_url).context("parsing the daemon base URL")?;
    let ws_scheme = match base.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => anyhow::bail!("unsupported URL scheme '{other}' (expected http/https)"),
    };
    let mut url = base
        .join(&format!("/api/v1/sessions/{session_id}/stream"))
        .context("building the WebSocket URL")?;
    url.set_scheme(ws_scheme)
        .map_err(|()| anyhow::anyhow!("could not switch '{}' to '{ws_scheme}'", base.scheme()))?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(after) = after {
            q.append_pair("after", &after.to_string());
        }
        q.append_pair("mode", "control");
        q.append_pair("client_id", client_id);
        q.append_pair("client_name", client_name);
    }
    Ok(url)
}

async fn connect_and_run(
    params: &AttachParams<'_>,
    after: Option<u64>,
    interactive: bool,
    state: &mut LoopState,
) -> Result<Outcome, ConnectError> {
    let url = build_stream_url(
        &params.conn.base_url,
        params.session_id,
        after,
        params.client_id,
        params.client_name,
    )?;

    let mut request = url
        .as_str()
        .into_client_request()
        .context("building the WebSocket handshake request")?;
    // Native clients "have no excuse" not to use the header -- never
    // `?token=`/`?ticket=` (docs/06-security.md#token-on-the-websocket-upgrade).
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", params.conn.token))
            .context("token is not a valid header value")?,
    );

    let mut ws = match tokio_tungstenite::connect_async(request).await {
        Ok((ws, _response)) => ws,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let status = resp.status().as_u16();
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).trim().to_string())
                .filter(|s| !s.is_empty());
            return Err(ConnectError::Fatal { status, body });
        }
        Err(e) => return Err(anyhow::anyhow!("connecting: {e}").into()),
    };

    let mut stdout = tokio::io::stdout();

    // Everything before `ready` isn't necessarily *just* `ready`: a large
    // enough replay gap makes the daemon send one or more binary History
    // frames first, one per bounded catch-up round
    // (docs/04-api-protocol.md#catch-up--register-late-not-early;
    // daemon/src/ws.rs's `run()` -- every `ReplayStep::History` round calls
    // `send_binary` *before* the loop ever breaks to build and send
    // `ready`). Treating a pre-`ready` binary frame as a protocol error, as
    // an earlier version of this function did, means any session with more
    // than one round of backlog can never attach at all: the client
    // reconnects with the same unchanged `after` and hits the exact same
    // frame again, forever. So this reads and displays as many binary
    // frames as arrive, and keeps going until the text `ready` frame shows
    // up -- exactly what the main loop below does with a post-`ready`
    // binary frame, just before raw mode and the rest of the loop exist.
    let ready = loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
                Ok(ServerMessage::Ready(ready)) => break ready,
                Ok(_) => {
                    return Err(anyhow::anyhow!(
                        "expected `ready` before any other control message"
                    )
                    .into())
                }
                Err(e) => return Err(anyhow::anyhow!("parsing `ready` frame: {e}").into()),
            },
            Some(Ok(Message::Binary(data))) => {
                if data.len() < 8 {
                    continue;
                }
                let payload = &data[8..];
                stdout
                    .write_all(payload)
                    .await
                    .context("writing catch-up output to stdout")?;
                stdout.flush().await.context("flushing stdout")?;
            }
            Some(Ok(Message::Ping(payload))) => {
                let _ = ws.send(Message::Pong(payload)).await;
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(anyhow::anyhow!("reading handshake frames: {e}").into()),
            None => return Err(anyhow::anyhow!("connection closed before `ready`").into()),
        }
    };

    state.backoff_ms = BACKOFF_MIN_MS;
    // `ready`'s own `next_offset` is where the *live* stream begins, after
    // any replay; `replay_from` is where replay started. Neither is what we
    // want here -- we want wherever the frames above (zero or more) already
    // left off, which by the protocol's contiguity guarantee ("History and
    // live output are one contiguous byte stream under one set of
    // offsets") is exactly the offset of whatever binary frame comes next.
    // The very next frame after `ready` (if any) carries that offset in its
    // own 8-byte header, so the main loop below learns it there rather than
    // this function tracking it separately.
    let mut next_offset: Option<u64> = None;
    let mut is_controller = ready.control;
    if ready.truncated {
        eprintln!("teleport: scrollback truncated (replay was clamped); use `teleport log` for full history");
    }
    if !is_controller {
        eprintln!(
            "teleport: observing -- {} is in control (~! to take it)",
            ready.controller.as_deref().unwrap_or("nobody")
        );
    }

    if interactive && !state.raw_mode_entered {
        crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
        state.raw_mode_entered = true;
        eprintln!("teleport: attached. ~. to detach, ~? for help.\r");
    }

    let mut stdin = tokio::io::stdin();
    let mut escape = EscapeState::new();
    let mut stdin_eof = false;
    let mut resize = ResizeWatcher::new();
    let mut buf = [0u8; 8192];

    loop {
        tokio::select! {
            n = stdin.read(&mut buf), if !stdin_eof => {
                let n = n.context("reading local stdin")?;
                if n == 0 {
                    stdin_eof = true;
                    continue;
                }
                if !interactive {
                    // Piped stdin: no escape sequences, no tty to restore
                    // (docs/11-mvp-plan.md#m11's edge cases) -- forward
                    // verbatim, same shape as `ssh host cmd | grep foo`.
                    if is_controller {
                        ws.send(Message::binary(buf[..n].to_vec())).await.context("sending input")?;
                    }
                    continue;
                }
                let (bytes, actions) = escape.process(&buf[..n]);
                let mut detach = false;
                for action in actions {
                    match action {
                        Action::Detach => detach = true,
                        Action::ClaimControl => {
                            ws.send(Message::text(json!({"type":"claim_control"}).to_string()))
                                .await
                                .context("sending claim_control")?;
                        }
                        Action::Help => {
                            eprintln!("\r\n~.  detach\r\n~!  claim control\r\n~?  this message\r\n~~  literal ~\r");
                        }
                    }
                }
                if detach {
                    return Ok(Outcome::Detach);
                }
                // Never send input while observing -- the daemon drops it
                // with an `error` frame anyway, but a client that already
                // knows its own role shouldn't rely on the server to say so
                // (docs/11-mvp-plan.md#m11's edge cases).
                if !bytes.is_empty() && is_controller {
                    ws.send(Message::binary(bytes)).await.context("sending input")?;
                }
            }
            (cols, rows) = resize.changed(), if interactive && is_controller => {
                ws.send(Message::text(json!({"type":"resize","cols":cols,"rows":rows}).to_string()))
                    .await
                    .context("sending resize")?;
            }
            frame = ws.next() => {
                match frame {
                    Some(Ok(Message::Binary(data))) => {
                        if data.len() < 8 {
                            continue;
                        }
                        let offset = u64::from_be_bytes(data[0..8].try_into().unwrap());
                        let payload = &data[8..];
                        if let Some(expected) = next_offset {
                            if offset != expected {
                                eprintln!(
                                    "\r\n[teleport: {} byte gap in the stream -- scrollback truncated]\r",
                                    offset.saturating_sub(expected)
                                );
                            }
                        }
                        stdout.write_all(payload).await.context("writing to stdout")?;
                        stdout.flush().await.context("flushing stdout")?;
                        next_offset = Some(offset + payload.len() as u64);
                    }
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(ServerMessage::Exit { code, .. }) => {
                                return Ok(Outcome::Exit(code.unwrap_or(0)));
                            }
                            Ok(ServerMessage::ControlGranted) => {
                                is_controller = true;
                                eprintln!("\r\n[teleport: control granted]\r");
                            }
                            Ok(ServerMessage::ControlRevoked { to, .. }) => {
                                is_controller = false;
                                eprintln!("\r\n[teleport: control taken by {to}]\r");
                            }
                            Ok(ServerMessage::Resized { .. }) => {}
                            Ok(ServerMessage::Ready(_)) => {}
                            Ok(ServerMessage::Error { code, message }) => {
                                if code == "offset_ahead" {
                                    return Ok(Outcome::Reconnect { after: 0 });
                                }
                                eprintln!(
                                    "\r\n[teleport: {code}{}]\r",
                                    message.map(|m| format!(": {m}")).unwrap_or_default()
                                );
                            }
                            Err(e) => {
                                eprintln!("\r\n[teleport: unrecognized server message: {e}]\r");
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(anyhow::anyhow!("websocket error: {e}").into()),
                    None => return Ok(Outcome::Reconnect { after: next_offset.unwrap_or(0) }),
                }
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Action {
    Detach,
    ClaimControl,
    Help,
}

/// ssh-style `~` escapes, typed at the start of a line
/// (docs/11-mvp-plan.md#m11's escape sequences). State persists across
/// `read()` calls -- in raw mode, `~` and its follow-up byte routinely
/// arrive in separate reads, one keystroke per syscall.
struct EscapeState {
    at_line_start: bool,
    pending_tilde: bool,
}

impl EscapeState {
    fn new() -> Self {
        EscapeState {
            at_line_start: true,
            pending_tilde: false,
        }
    }

    fn process(&mut self, input: &[u8]) -> (Vec<u8>, Vec<Action>) {
        let mut out = Vec::with_capacity(input.len());
        let mut actions = Vec::new();
        for &b in input {
            if self.pending_tilde {
                self.pending_tilde = false;
                match b {
                    b'.' => actions.push(Action::Detach),
                    b'!' => actions.push(Action::ClaimControl),
                    b'?' => actions.push(Action::Help),
                    b'~' => out.push(b'~'),
                    other => {
                        out.push(b'~');
                        out.push(other);
                    }
                }
                self.at_line_start = matches!(b, b'\r' | b'\n');
                continue;
            }
            if self.at_line_start && b == b'~' {
                self.pending_tilde = true;
                continue;
            }
            out.push(b);
            self.at_line_start = matches!(b, b'\r' | b'\n');
        }
        (out, actions)
    }
}

/// Local terminal resize detection. Unix has `SIGWINCH`; Windows has no
/// equivalent signal reachable without crossterm's `event-stream` feature
/// (which would also hand us decoded key events on the same fd we're
/// already reading raw bytes from -- two readers on one stdin is a race,
/// not a feature, so it's out for the reasons `Cargo.toml` documents).
/// Polling is the fallback there, and also the fallback on Unix if signal
/// registration itself fails (a resource limit, a conflicting in-process
/// handler) -- an earlier version awaited a `pending::<()>()` future in
/// that case, which silently disabled resize forwarding for the rest of
/// the session with no diagnostic. Polling costs little and at least keeps
/// working.
struct ResizeWatcher {
    last: (u16, u16),
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
}

impl ResizeWatcher {
    fn new() -> Self {
        let last = crossterm::terminal::size().unwrap_or((80, 24));
        #[cfg(unix)]
        let signal =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
                Ok(sig) => Some(sig),
                Err(e) => {
                    eprintln!(
                        "teleport: could not install a SIGWINCH handler ({e}); \
                         falling back to polling for terminal resizes"
                    );
                    None
                }
            };
        ResizeWatcher {
            last,
            #[cfg(unix)]
            signal,
        }
    }

    #[cfg(unix)]
    async fn changed(&mut self) -> (u16, u16) {
        loop {
            match &mut self.signal {
                Some(sig) => {
                    sig.recv().await;
                }
                None => tokio::time::sleep(Duration::from_millis(500)).await,
            }
            if let Ok(size) = crossterm::terminal::size() {
                if size != self.last {
                    self.last = size;
                    return size;
                }
            }
        }
    }

    #[cfg(not(unix))]
    async fn changed(&mut self) -> (u16, u16) {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(size) = crossterm::terminal::size() {
                if size != self.last {
                    self.last = size;
                    return size;
                }
            }
        }
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn http_becomes_ws() {
        let url =
            build_stream_url("http://127.0.0.1:7337", "abc123", None, "cid", "cname").unwrap();
        assert_eq!(
            url.as_str(),
            "ws://127.0.0.1:7337/api/v1/sessions/abc123/stream?mode=control&client_id=cid&client_name=cname"
        );
    }

    #[test]
    fn https_becomes_wss() {
        let url = build_stream_url(
            "https://mainpc.tail1234.ts.net",
            "abc123",
            Some(184221),
            "cid",
            "cname",
        )
        .unwrap();
        assert_eq!(url.scheme(), "wss");
        assert!(url.as_str().contains("after=184221"));
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        let err =
            build_stream_url("ftp://example.com", "abc123", None, "cid", "cname").unwrap_err();
        assert!(err.to_string().contains("unsupported URL scheme"));
    }
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    #[test]
    fn plain_text_passes_through_unchanged() {
        let mut e = EscapeState::new();
        let (bytes, actions) = e.process(b"hello world");
        assert_eq!(bytes, b"hello world");
        assert!(actions.is_empty());
    }

    #[test]
    fn tilde_mid_line_is_not_an_escape() {
        let mut e = EscapeState::new();
        e.process(b"x"); // not at line start any more
        let (bytes, actions) = e.process(b"~.");
        assert_eq!(bytes, b"~.");
        assert!(actions.is_empty());
    }

    #[test]
    fn detach_at_line_start() {
        let mut e = EscapeState::new();
        let (bytes, actions) = e.process(b"~.");
        assert!(bytes.is_empty());
        assert_eq!(actions, vec![Action::Detach]);
    }

    #[test]
    fn claim_control_at_line_start() {
        let mut e = EscapeState::new();
        let (bytes, actions) = e.process(b"~!");
        assert!(bytes.is_empty());
        assert_eq!(actions, vec![Action::ClaimControl]);
    }

    #[test]
    fn help_at_line_start() {
        let mut e = EscapeState::new();
        let (bytes, actions) = e.process(b"~?");
        assert!(bytes.is_empty());
        assert_eq!(actions, vec![Action::Help]);
    }

    #[test]
    fn double_tilde_is_a_literal_tilde() {
        let mut e = EscapeState::new();
        let (bytes, actions) = e.process(b"~~");
        assert_eq!(bytes, b"~");
        assert!(actions.is_empty());
    }

    #[test]
    fn tilde_then_unrecognized_byte_passes_both_through() {
        let mut e = EscapeState::new();
        let (bytes, actions) = e.process(b"~x");
        assert_eq!(bytes, b"~x");
        assert!(actions.is_empty());
    }

    /// In raw mode, one keystroke is one `read()` -- `~` and its follow-up
    /// byte routinely arrive in separate calls. State must persist.
    #[test]
    fn escape_split_across_two_reads() {
        let mut e = EscapeState::new();
        let (bytes1, actions1) = e.process(b"~");
        assert!(bytes1.is_empty());
        assert!(actions1.is_empty());
        let (bytes2, actions2) = e.process(b".");
        assert!(bytes2.is_empty());
        assert_eq!(actions2, vec![Action::Detach]);
    }

    #[test]
    fn newline_resets_line_start_so_escape_works_again() {
        let mut e = EscapeState::new();
        e.process(b"echo hi"); // consumes the initial line-start
        let (bytes, actions) = e.process(b"\r");
        assert_eq!(bytes, b"\r");
        assert!(actions.is_empty());
        let (bytes, actions) = e.process(b"~.");
        assert!(bytes.is_empty());
        assert_eq!(actions, vec![Action::Detach]);
    }

    #[test]
    fn an_escape_action_itself_is_not_a_new_line_start() {
        // "~!x" -- the `!` is consumed as claim_control, and the `x` right
        // after it is NOT treated as a fresh line start (ssh's own rule:
        // you need a real newline before `~` is special again).
        let mut e = EscapeState::new();
        let (_, actions) = e.process(b"~!");
        assert_eq!(actions, vec![Action::ClaimControl]);
        let (bytes, actions) = e.process(b"~x");
        assert_eq!(bytes, b"~x");
        assert!(actions.is_empty());
    }
}
