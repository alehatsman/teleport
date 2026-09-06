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

/// Jittered exponential backoff -- same shape and constants as
/// `web/src/lib/stream.ts`'s reconnect logic, so a dropped link behaves the
/// same regardless of which client is attached.
const BACKOFF_MIN_MS: u64 = 250;
const BACKOFF_MAX_MS: u64 = 8000;

pub async fn run(
    conn: &Connection,
    session_id: &str,
    client_id: &str,
    client_name: &str,
) -> Result<i32> {
    let interactive = std::io::stdin().is_tty() && std::io::stdout().is_tty();
    let mut raw_mode_entered = false;
    let mut after: Option<u64> = None;
    let mut backoff_ms = BACKOFF_MIN_MS;

    let result = loop {
        match connect_and_run(
            conn,
            session_id,
            client_id,
            client_name,
            after,
            interactive,
            &mut raw_mode_entered,
            &mut backoff_ms,
        )
        .await
        {
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
                if raw_mode_entered {
                    eprintln!("\r\n[teleport: connection lost ({e}), reconnecting...]\r");
                } else {
                    eprintln!("teleport: connect failed ({e}), retrying...");
                }
                let jitter = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_millis())
                    .unwrap_or(0) as u64)
                    % (backoff_ms / 4 + 1);
                tokio::time::sleep(Duration::from_millis(backoff_ms + jitter)).await;
                backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
                continue;
            }
        }
    };

    if raw_mode_entered {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    result
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
    /// Where replay actually starts -- the offset the *first* binary frame
    /// after `ready` carries, not where the live stream begins. Tracking
    /// `next_offset` from here (not from this struct's own `next_offset`
    /// field) is what makes the gap check below correct instead of flagging
    /// every attach with any replay at all as a discontinuity.
    replay_from: u64,
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

#[allow(clippy::too_many_arguments)]
async fn connect_and_run(
    conn: &Connection,
    session_id: &str,
    client_id: &str,
    client_name: &str,
    after: Option<u64>,
    interactive: bool,
    raw_mode_entered: &mut bool,
    backoff_ms: &mut u64,
) -> Result<Outcome, ConnectError> {
    let ws_scheme = if conn.base_url.starts_with("https://") {
        "wss://"
    } else {
        "ws://"
    };
    let rest = conn
        .base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&conn.base_url);
    let mut url = reqwest::Url::parse(&format!(
        "{ws_scheme}{rest}/api/v1/sessions/{session_id}/stream"
    ))
    .context("building the WebSocket URL")?;
    {
        let mut q = url.query_pairs_mut();
        if let Some(after) = after {
            q.append_pair("after", &after.to_string());
        }
        q.append_pair("mode", "control");
        q.append_pair("client_id", client_id);
        q.append_pair("client_name", client_name);
    }

    let mut request = url
        .as_str()
        .into_client_request()
        .context("building the WebSocket handshake request")?;
    // Native clients "have no excuse" not to use the header -- never
    // `?token=`/`?ticket=` (docs/06-security.md#token-on-the-websocket-upgrade).
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", conn.token))
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

    let ready = match ws.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
            Ok(ServerMessage::Ready(ready)) => ready,
            Ok(_) => return Err(anyhow::anyhow!("expected `ready` as the first frame").into()),
            Err(e) => return Err(anyhow::anyhow!("parsing `ready` frame: {e}").into()),
        },
        Some(Ok(_)) => return Err(anyhow::anyhow!("expected a text `ready` frame first").into()),
        Some(Err(e)) => return Err(anyhow::anyhow!("reading `ready` frame: {e}").into()),
        None => return Err(anyhow::anyhow!("connection closed before `ready`").into()),
    };

    *backoff_ms = BACKOFF_MIN_MS;
    let mut next_offset = ready.replay_from;
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

    if interactive && !*raw_mode_entered {
        crossterm::terminal::enable_raw_mode().context("enabling raw mode")?;
        *raw_mode_entered = true;
        eprintln!("teleport: attached. ~. to detach, ~? for help.\r");
    }

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
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
                        if offset != next_offset {
                            eprintln!(
                                "\r\n[teleport: {} byte gap in the stream -- scrollback truncated]\r",
                                offset.saturating_sub(next_offset)
                            );
                        }
                        stdout.write_all(payload).await.context("writing to stdout")?;
                        stdout.flush().await.context("flushing stdout")?;
                        next_offset = offset + payload.len() as u64;
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
                    None => return Ok(Outcome::Reconnect { after: next_offset }),
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
/// Polling is the honest fallback there: cheap, and only active while this
/// client holds the control lease.
struct ResizeWatcher {
    last: (u16, u16),
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
}

impl ResizeWatcher {
    fn new() -> Self {
        let last = crossterm::terminal::size().unwrap_or((80, 24));
        ResizeWatcher {
            last,
            #[cfg(unix)]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                .ok(),
        }
    }

    #[cfg(unix)]
    async fn changed(&mut self) -> (u16, u16) {
        loop {
            match &mut self.signal {
                Some(sig) => {
                    sig.recv().await;
                }
                None => std::future::pending::<()>().await,
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
