//! `GET /api/v1/sessions/{id}/stream` -- the WebSocket protocol
//! (docs/04-api-protocol.md#websocket-protocol). One connection per attached
//! terminal: mixed framing (text = control JSON, binary = raw PTY bytes
//! prefixed with an 8-byte big-endian offset), the attach/catch-up sequence
//! from `session.rs`, and the control lease.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::api::AppState;
use crate::auth::Principal;
use crate::session::{AttachError, ReplayStep, Session, SessionEvent, SessionId};

/// Server sends a `Ping` on this cadence (docs/04-api-protocol.md#keepalive-and-reconnection).
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// Closes the connection if no `Pong` arrives within this long.
const PONG_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    after: Option<u64>,
    tail: Option<u64>,
    #[serde(default)]
    mode: StreamMode,
    client_id: Option<String>,
    client_name: Option<String>,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum StreamMode {
    #[default]
    Observe,
    Control,
}

/// The route handler: validates the upgrade (Origin/Host, credential,
/// session existence, mutually-exclusive `after`/`tail`) and, only once all
/// of that holds, upgrades and hands off to [`run`]. Everything before the
/// upgrade can still return an ordinary HTTP error response; nothing after
/// it can, which is exactly why these checks come first.
pub async fn upgrade(
    State(state): State<Arc<AppState>>,
    principal: Principal,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let _: Principal = principal; // resolved for its side effect: a bad/missing credential already produced a 401 rejection before this handler ran.

    if let Err(e) = state.origin_policy.check(&headers) {
        return crate::api::ApiError::from(e).into_response();
    }
    if q.after.is_some() && q.tail.is_some() {
        return (axum::http::StatusCode::BAD_REQUEST, "after and tail are mutually exclusive").into_response();
    }
    let Ok(session_id) = id.parse::<SessionId>() else {
        return (axum::http::StatusCode::NOT_FOUND, "session not found").into_response();
    };
    let Some(session) = state.sessions.get(session_id) else {
        return (axum::http::StatusCode::NOT_FOUND, "session not found").into_response();
    };

    // A stable per-install id names the controller in the UI and lets a
    // reconnecting client resume its own lease; a client that omits it gets
    // an ephemeral one and loses lease resumption
    // (docs/04-api-protocol.md#client-identity).
    let client_id = q.client_id.clone().unwrap_or_else(|| format!("ephemeral-{}", ulid::Ulid::new()));
    let client_name = q
        .client_name
        .clone()
        .unwrap_or_else(|| client_id.chars().take(8).collect());
    let requested_control = q.mode == StreamMode::Control;
    let default_tail = state.config.default_tail;
    let max_replay_bytes = state.config.max_replay_bytes;
    let control_grace_ms = state.config.control_grace_ms;

    ws.on_upgrade(move |socket| {
        run(
            socket,
            session,
            client_id,
            client_name,
            requested_control,
            q.after,
            q.tail,
            default_tail,
            max_replay_bytes,
            control_grace_ms,
        )
    })
}

/// Bounds the requested cursor before it ever reaches [`Session::attach`]
/// (docs/04-api-protocol.md#bounded-attach). Returns the clamped `from` and
/// whether it moved -- `tail`/default-tail clamping is normal tail semantics
/// and never counts as truncation; only an `after` moved forward by
/// `max_replay_bytes` does (docs/04-api-protocol.md#bounded-attach: "When a
/// replay is clamped, `ready` says so").
fn bound_attach(
    after: Option<u64>,
    tail: Option<u64>,
    default_tail: u64,
    max_replay_bytes: u64,
    next_offset: u64,
) -> (u64, bool) {
    match after {
        Some(after) => {
            let earliest = next_offset.saturating_sub(max_replay_bytes);
            let from = after.max(earliest);
            (from, from > after)
        }
        None => {
            let tail = tail.unwrap_or(default_tail).min(max_replay_bytes);
            (next_offset.saturating_sub(tail), false)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    mut socket: WebSocket,
    session: Arc<Session>,
    client_id: String,
    client_name: String,
    requested_control: bool,
    after: Option<u64>,
    tail: Option<u64>,
    default_tail: u64,
    max_replay_bytes: u64,
    control_grace_ms: u64,
) {
    let next_offset_hint = session.next_offset();
    let (from, mut truncated) = bound_attach(after, tail, default_tail, max_replay_bytes, next_offset_hint);

    let replay = match session.attach(from) {
        Ok(replay) => replay,
        Err(AttachError::OffsetAhead { next_offset, .. }) => {
            send_json(&mut socket, &json!({ "type": "error", "code": "offset_ahead", "next_offset": next_offset })).await;
            let _ = socket.send(close(1008, "offset_ahead")).await;
            return;
        }
        Err(AttachError::Io(e)) => {
            tracing::warn!(session_id = %session.id, error = %e, "opening replay");
            let _ = socket.send(close(1011, "internal error")).await;
            return;
        }
    };

    // Drive the catch-up loop, writing each round before asking for the
    // next -- the loop's own convergence measurement depends on that
    // (docs/04-api-protocol.md#catch-up--register-late-not-early).
    let mut replay = replay;
    let attach = loop {
        match replay.next_round() {
            Ok(ReplayStep::History { offset, bytes, replay: rest }) => {
                if send_binary(&mut socket, offset, &bytes).await.is_err() {
                    return;
                }
                replay = rest;
            }
            Ok(ReplayStep::Live(attach)) => break attach,
            Err(e) => {
                tracing::warn!(session_id = %session.id, error = %e, "replay round failed");
                let _ = socket.send(close(1011, "internal error")).await;
                return;
            }
        }
    };
    truncated = truncated || !attach.caught_up;

    let control_epoch = if requested_control { session.attach_control(&client_id, &client_name) } else { None };
    let (cols, rows) = session.size();

    let ready = json!({
        "type": "ready",
        "session_id": session.id.to_string(),
        "replay_from": attach.replay_from,
        "next_offset": attach.next_offset,
        "truncated": truncated,
        "log_capped_at": attach.log_capped_at,
        "cols": cols,
        "rows": rows,
        "control": control_epoch.is_some(),
        "controller": session.controller_name(),
    });
    send_json(&mut socket, &ready).await;
    if !attach.replay.is_empty() && send_binary(&mut socket, attach.replay_from, &attach.replay).await.is_err() {
        return;
    }

    let mut subscription = attach.subscription;
    let mut exited_rx = session.watch_exited();
    let mut events_rx = session.subscribe_events();
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // the first tick fires immediately; consume it.
    let mut last_pong = Instant::now();
    // `Some(epoch)` while this connection holds the control lease -- the
    // epoch it was granted, re-checked against the authoritative lease on
    // every write/resize so a lease moved out from under it (another
    // connection's `claim_control`, including one sharing this `client_id`)
    // is never missed (docs/04-api-protocol.md#control-lease).
    let mut is_controlling: Option<u64> = control_epoch;

    loop {
        tokio::select! {
            chunk = subscription.recv() => {
                match chunk {
                    Some(chunk) => {
                        if send_binary(&mut socket, chunk.offset, &chunk.bytes).await.is_err() {
                            break;
                        }
                    }
                    // The only way a subscriber slot disappears while this
                    // task still holds its own `Arc<Session>` (keeping the
                    // fan-out alive) is `Fanout::publish`'s backpressure
                    // eviction -- this is the slow-consumer signal
                    // (docs/04-api-protocol.md#error-codes).
                    None => {
                        let _ = socket.send(close(1013, "slow_consumer")).await;
                        break;
                    }
                }
            }

            _ = exited_rx.changed() => {
                if *exited_rx.borrow() {
                    let exit = json!({
                        "type": "exit",
                        "code": session.exit_code(),
                        "final_offset": session.next_offset(),
                    });
                    send_json(&mut socket, &exit).await;
                    let _ = socket.send(close(1000, "session exited")).await;
                    break;
                }
            }

            event = events_rx.recv() => {
                match event {
                    Ok(SessionEvent::Resized { cols, rows }) => {
                        send_json(&mut socket, &json!({ "type": "resized", "cols": cols, "rows": rows })).await;
                    }
                    Ok(SessionEvent::ControlRevoked { lost_by, new_controller_id, new_controller_name }) => {
                        if lost_by == client_id {
                            is_controlling = None;
                            send_json(&mut socket, &json!({ "type": "control_revoked", "to": new_controller_name, "client_id": new_controller_id })).await;
                        }
                    }
                    // A lagged receiver only means a missed notification --
                    // every subsequent control/resize check re-reads
                    // authoritative state (`is_controller`, `size`), so
                    // there is nothing to resync here.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }

            _ = ping_interval.tick() => {
                if last_pong.elapsed() > PONG_TIMEOUT {
                    let _ = socket.send(close(1001, "ping timeout")).await;
                    break;
                }
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }

            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Binary(bytes))) => {
                        // `write_if_controller` checks the lease and writes under
                        // one lock, so a `claim_control` racing this check (M4
                        // review) can never land in between (Err(None) covers both
                        // "never had control" and "lost it just now").
                        let result = match is_controlling {
                            Some(epoch) => session.write_if_controller(&client_id, epoch, &bytes),
                            None => Err(None),
                        };
                        match result {
                            Ok(()) => {}
                            Err(Some(e)) => {
                                tracing::debug!(session_id = %session.id, error = %e, "write rejected");
                                send_json(&mut socket, &json!({ "type": "error", "code": "session_closing", "message": "input rejected: session is closing" })).await;
                            }
                            Err(None) => {
                                send_json(&mut socket, &json!({ "type": "error", "code": "not_controller", "message": "input rejected: observer mode" })).await;
                            }
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        handle_control_message(&text, &session, &client_id, &client_name, &mut is_controlling, &mut socket).await;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_pong = Instant::now();
                    }
                    Some(Ok(Message::Ping(_) | Message::Close(_))) => {}
                    Some(Err(_)) | None => break,
                }
            }
        }
    }

    if let Some(epoch) = is_controlling {
        session.begin_control_grace(client_id, epoch, control_grace_ms);
    } else {
        // An observer that never held control has nothing to release; an
        // explicit `release_control` already cleared it if it applied.
    }
}

async fn handle_control_message(
    text: &str,
    session: &Arc<Session>,
    client_id: &str,
    client_name: &str,
    is_controlling: &mut Option<u64>,
    socket: &mut WebSocket,
) {
    #[derive(Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum ClientMessage {
        Resize { cols: u16, rows: u16 },
        ClaimControl,
        ReleaseControl,
    }

    let message: ClientMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            send_json(socket, &json!({ "type": "error", "code": "bad_request", "message": e.to_string() })).await;
            return;
        }
    };

    match message {
        ClientMessage::Resize { cols, rows } => {
            let controller = is_controlling.is_some_and(|epoch| session.is_controller(client_id, epoch));
            if controller {
                if let Err(e) = session.resize(cols, rows) {
                    tracing::debug!(session_id = %session.id, error = %e, "resize rejected");
                }
            } else {
                send_json(socket, &json!({ "type": "error", "code": "not_controller", "message": "resize rejected: observer mode" })).await;
            }
        }
        ClientMessage::ClaimControl => {
            let epoch = session.claim_control(client_id, client_name);
            *is_controlling = Some(epoch);
            send_json(socket, &json!({ "type": "control_granted" })).await;
        }
        ClientMessage::ReleaseControl => {
            if let Some(epoch) = is_controlling.take() {
                session.release_control(client_id, epoch);
            }
        }
    }
}

async fn send_json(socket: &mut WebSocket, value: &serde_json::Value) {
    if socket.send(Message::Text(value.to_string().into())).await.is_err() {
        tracing::debug!("send failed; connection is going away");
    }
}

/// Every server->client binary frame: an 8-byte big-endian offset of the
/// first payload byte, then the raw bytes -- never JSON-encoded
/// (docs/04-api-protocol.md#framing).
async fn send_binary(socket: &mut WebSocket, offset: u64, bytes: &[u8]) -> Result<(), axum::Error> {
    let mut frame = Vec::with_capacity(8 + bytes.len());
    frame.extend_from_slice(&offset.to_be_bytes());
    frame.extend_from_slice(bytes);
    socket.send(Message::Binary(frame.into())).await
}

fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame { code, reason: reason.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_TAIL: u64 = 1024 * 1024;
    const MAX_REPLAY: u64 = 8 * 1024 * 1024;

    /// docs/10-testing.md#3-protocol-tests: "the same with `tail` unset,
    /// exercising `default_tail`".
    #[test]
    fn no_cursor_replays_default_tail() {
        let next_offset = 500_000_000; // the "500 MB log" case
        let (from, truncated) = bound_attach(None, None, DEFAULT_TAIL, MAX_REPLAY, next_offset);
        assert_eq!(from, next_offset - DEFAULT_TAIL, "must replay default_tail, not the whole log");
        assert!(!truncated, "tail semantics are not a broken promise -- never flagged truncated");
    }

    /// docs/10-testing.md: "`tail=N` starts at exactly `max(0, next_offset -
    /// N)`".
    #[test]
    fn tail_starts_at_exactly_next_offset_minus_n() {
        let (from, truncated) = bound_attach(None, Some(1000), DEFAULT_TAIL, MAX_REPLAY, 5000);
        assert_eq!(from, 4000);
        assert!(!truncated);

        // N bigger than the log: clamps to 0, not a negative/wrapped offset.
        let (from, _) = bound_attach(None, Some(1000), DEFAULT_TAIL, MAX_REPLAY, 500);
        assert_eq!(from, 0);
    }

    /// docs/10-testing.md: "`after=0` on a huge log is clamped to
    /// `max_replay_bytes` and `ready` reports `truncated: true` with a
    /// correct `replay_from`".
    #[test]
    fn after_zero_on_a_huge_log_is_clamped_and_flagged_truncated() {
        let next_offset = 500_000_000;
        let (from, truncated) = bound_attach(Some(0), None, DEFAULT_TAIL, MAX_REPLAY, next_offset);
        assert_eq!(from, next_offset - MAX_REPLAY, "replay_from must be exactly the max_replay_bytes boundary");
        assert!(truncated);
    }

    /// An `after` that already falls inside `max_replay_bytes` of the
    /// boundary needs no clamp -- not every `after` request is truncated.
    #[test]
    fn after_within_the_replay_bound_is_not_truncated() {
        let next_offset = 10_000_000;
        let after = next_offset - 100; // well within max_replay_bytes
        let (from, truncated) = bound_attach(Some(after), None, DEFAULT_TAIL, MAX_REPLAY, next_offset);
        assert_eq!(from, after);
        assert!(!truncated);
    }

    /// `max_replay_bytes` caps every replay "regardless of parameters"
    /// (docs/04-api-protocol.md#bounded-attach) -- including an oversized
    /// explicit `tail`.
    #[test]
    fn an_oversized_tail_is_also_capped_by_max_replay_bytes() {
        let next_offset = 500_000_000;
        let (from, _) = bound_attach(None, Some(50 * 1024 * 1024), DEFAULT_TAIL, MAX_REPLAY, next_offset);
        assert_eq!(from, next_offset - MAX_REPLAY);
    }
}
