//! The WebSocket proxy for anchor-hosted sessions.
//!
//! Teleport's clients keep speaking teleport's protocol
//! (docs/04-api-protocol.md); this translates it onto anchor's attach
//! stream. It is the only stream path: the in-process loop it was written
//! beside (`ws::run`) went with the in-process layer (docs/20).
//!
//! Two leases are in play and they are not the same thing. Teleport's
//! (`anchor_lease`) decides which *browser* may type, and carries the
//! disconnect grace a reconnecting client depends on. Anchor's decides
//! which *connection to anchord* may write, and this connection claims it
//! whenever it wins teleport's.

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use serde::Deserialize;
use serde_json::json;
use tokio::time::{Duration, Instant};

use crate::anchor_client::{AnchorAttach, AnchorClient, AnchorFrame, AnchorSession};
use crate::anchor_lease::{LeaseRegistry, SessionEvent};

/// How often to ping an idle client, matching `ws.rs`.
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// A client that has not ponged within this is gone.
const PONG_TIMEOUT: Duration = Duration::from_secs(60);

/// Everything one proxied connection needs.
#[derive(Debug)]
pub struct Proxy {
    /// The browser's socket.
    pub socket: WebSocket,
    /// The daemon holding the session.
    pub anchor: AnchorClient,
    /// Teleport's own lease layer.
    pub leases: Arc<LeaseRegistry>,
    /// The session being attached to.
    pub session_id: String,
    /// Stable per-install client id, for lease resumption.
    pub client_id: String,
    /// Display name for the controller badge.
    pub client_name: String,
    /// Whether this client asked for control.
    pub requested_control: bool,
    /// Explicit replay cursor.
    pub after: Option<u64>,
    /// Replay the last N bytes instead.
    pub tail: Option<u64>,
    /// Default tail when neither is given.
    pub default_tail: u64,
    /// Hard cap on replayed history.
    pub max_replay_bytes: u64,
    /// How long a disconnected controller keeps its lease.
    pub control_grace_ms: u64,
}

/// What a client may send.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Resize { cols: u16, rows: u16 },
    ClaimControl,
    ReleaseControl,
}

/// Tell the client its cursor is past the head, and close.
///
/// Same frame as the in-process path's (`ws.rs`): `next_offset`, no message,
/// so a client cannot tell the two apart.
async fn reject_offset_ahead(socket: &mut WebSocket, next_offset: u64) {
    send_json(
        socket,
        &json!({
            "type": "error",
            "code": "offset_ahead",
            "next_offset": next_offset,
        }),
    )
    .await;
    drop(socket.send(close(1008, "offset_ahead")).await);
}

/// The `ready` frame: what the client needs before the first byte of replay.
async fn send_ready(
    socket: &mut WebSocket,
    session_id: &str,
    from: u64,
    truncated: bool,
    session: &AnchorSession,
    attach: &AnchorAttach,
    control: bool,
    leases: &LeaseRegistry,
) {
    send_json(
        socket,
        &json!({
            "type": "ready",
            "session_id": session_id,
            "replay_from": from,
            "next_offset": attach.next_offset(),
            "truncated": truncated,
            "log_capped_at": session.log_capped_at,
            // The attach reply's size, not the earlier `get`'s: the attach
            // is the later observation, and between the two the session may
            // have been resized by someone else. Falls back to `get` against
            // an anchord too old to report it in the reply.
            "cols": attach.size().map_or(session.cols, |(cols, _)| cols),
            "rows": attach.size().map_or(session.rows, |(_, rows)| rows),
            "control": control,
            "controller": leases.controller_name(session_id),
        }),
    )
    .await;
}

/// Attach through anchord, then claim its lease if teleport granted one and
/// anchor gave it to an earlier connection.
///
/// `None` is "anchord's problem, not the cursor's" — it is down, restarting,
/// or the session went away mid-upgrade. The in-process path closes these
/// `1011 internal error` with no frame at all, and a client treats the close
/// as an ordinary drop and reconnects from where it was.
async fn attach_through_anchor(
    anchor: &AnchorClient,
    session_id: &str,
    from: u64,
    control: bool,
) -> Option<AnchorAttach> {
    let mut attach = match anchor.attach(session_id, from, control).await {
        Ok(attach) => attach,
        Err(e) => {
            tracing::warn!(session_id = %session_id, error = %e, "attaching through anchor");
            return None;
        }
    };
    // Attach never preempts, which is why this is a separate act.
    if control && !attach.has_control() {
        if let Err(e) = attach.claim_control().await {
            tracing::warn!(session_id = %session_id, error = %e, "claiming anchor's lease failed");
        }
    }
    Some(attach)
}

/// Tell the client the session is gone, and close.
///
/// `session_gone`, not `not_found`: that is the code
/// docs/04-api-protocol.md#error-codes defines for "id unknown or purged",
/// and clients match these by value. Reaching here at all is a race —
/// `upgrade` 404s an unknown id before the upgrade, so this is a session
/// purged between that check and this one.
async fn reject_session_gone(socket: &mut WebSocket, e: &impl std::fmt::Display) {
    send_json(
        socket,
        &json!({ "type": "error", "code": "session_gone", "message": e.to_string() }),
    )
    .await;
    drop(socket.send(close(1011, "session not found")).await);
}

/// Where this client's replay starts, and whether it was shortened.
///
/// `None` is "the cursor is past the head", which is the one attach failure
/// teleport can name itself — and naming it matters: the client drops its
/// cursor and restarts at the default tail on that code
/// (web/src/api/stream.ts#onErrorFrame). Reporting it for *every* attach
/// failure — anchord down, session exited mid-upgrade — threw away a
/// perfectly good cursor over an error that had nothing to do with it.
///
/// Past a cap the log stops while the offset stream keeps advancing (anchor
/// `docs/02-model.md#the-size-cap`), so a replay starting in that region has
/// nothing to read and resumes at the head instead of at a file position that
/// no longer means what the client thinks. Same clamp as `Session::attach`
/// makes in-process (`session/mod.rs`), with `log_capped_at` standing in for
/// the readable end — it is where the file stopped.
fn replay_window(
    after: Option<u64>,
    tail: Option<u64>,
    default_tail: u64,
    max_replay_bytes: u64,
    session: &AnchorSession,
) -> Option<(u64, bool)> {
    let (from, truncated) = crate::ws::bound_attach(
        after,
        tail,
        default_tail,
        max_replay_bytes,
        session.output_bytes,
    );
    if from > session.output_bytes {
        return None;
    }
    Some((
        clamp_past_cap(from, session.output_bytes, session.log_capped_at),
        truncated,
    ))
}

/// Serves one proxied connection until either side ends it.
#[expect(
    clippy::cognitive_complexity,
    reason = "one select loop over four event sources; splitting the arms into functions would hide the shape that makes it reviewable"
)]
pub async fn run_anchor(proxy: Proxy) {
    let Proxy {
        mut socket,
        anchor,
        leases,
        session_id,
        client_id,
        client_name,
        requested_control,
        after,
        tail,
        default_tail,
        max_replay_bytes,
        control_grace_ms,
    } = proxy;

    let session = match anchor.get(&session_id).await {
        Ok(session) => session,
        Err(e) => {
            reject_session_gone(&mut socket, &e).await;
            return;
        }
    };

    let Some((from, truncated)) =
        replay_window(after, tail, default_tail, max_replay_bytes, &session)
    else {
        reject_offset_ahead(&mut socket, session.output_bytes).await;
        return;
    };

    // Teleport's lease first: it decides who may type, and anchor's is
    // claimed to match it below.
    let mut control_epoch = if requested_control {
        leases.attach_control(&session_id, &client_id, &client_name)
    } else {
        None
    };

    let Some(mut attach) =
        attach_through_anchor(&anchor, &session_id, from, control_epoch.is_some()).await
    else {
        drop(socket.send(close(1011, "internal error")).await);
        return;
    };

    send_ready(
        &mut socket,
        &session_id,
        from,
        truncated,
        &session,
        &attach,
        control_epoch.is_some(),
        &leases,
    )
    .await;

    // One past the last byte handed to this client. `attach.next_offset()`
    // is the boundary as of the attach and never moves, so using it for the
    // exit frame would claim a client received less than it did — the frame
    // is how a client knows it has the whole transcript.
    let mut delivered_to = from;
    let mut lease_events = leases.subscribe(&session_id);
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // the first tick is immediate; consume it.
    let mut last_pong = Instant::now();

    loop {
        tokio::select! {
            frame = attach.next_frame() => match frame {
                Ok(Some(AnchorFrame::Output { offset, bytes })) => {
                    delivered_to = delivered_to.max(offset + bytes.len() as u64);
                    if send_binary(&mut socket, offset, &bytes).await.is_err() {
                        break;
                    }
                }
                Ok(Some(AnchorFrame::Exit { code })) => {
                    send_json(
                        &mut socket,
                        &json!({
                            "type": "exit",
                            "code": code,
                            "final_offset": delivered_to,
                        }),
                    )
                    .await;
                    drop(socket.send(close(1000, "session exited")).await);
                    break;
                }
                Ok(Some(AnchorFrame::Lease { held })) => {
                    // Anchor's lease moved. Teleport's is authoritative for
                    // who may type, so this only keeps the handle honest.
                    attach.set_control(held);
                }
                Ok(Some(AnchorFrame::Resized { cols, rows })) => {
                    // The size anchor actually applied, which is not
                    // necessarily the size anyone asked for -- it survives
                    // clamping and coalescing first. Forwarded to this
                    // browser even when this browser is what caused it: a
                    // client that asked for 5000 and got 1000 must render
                    // 1000, and `resized` is the only thing that says so.
                    //
                    // This is also the only way teleport hears about a
                    // resize by a *different* anchord client -- an `anchor`
                    // CLI, or another teleport -- which it otherwise could
                    // not observe at all, since geometry is not in the
                    // output bytes.
                    send_json(
                        &mut socket,
                        &json!({ "type": "resized", "cols": cols, "rows": rows }),
                    )
                    .await;
                }
                // The stream ended without an exit: anchor dropped this
                // subscriber for falling behind. `1013 slow_consumer` is
                // what the in-process path sends for its own backpressure
                // eviction (`ws.rs`), and what
                // docs/04-api-protocol.md#error-codes tells a client to
                // reconnect with backoff on.
                //
                // An anchord that died mid-stream ends the stream the same
                // way and is reported the same way. The two are not
                // distinguishable from this side, and the client action is
                // identical either way — reconnect from the last offset —
                // so the ambiguity costs a log line, not a behaviour.
                Ok(None) => {
                    drop(socket.send(close(1013, "slow_consumer")).await);
                    break;
                }
                Err(e) => {
                    tracing::debug!(session_id = %session_id, error = %e, "anchor stream failed");
                    break;
                }
            },

            event = lease_events.recv() => match event {
                Ok(SessionEvent::ControlRevoked {
                    new_controller_id,
                    new_controller_name,
                    ..
                }) => {
                    // Everyone except the new controller: the previous
                    // holder and every bystander alike, so an idle client's
                    // "take control (from X)" label is right without
                    // waiting for its next reconnect. The shape matches
                    // ws.rs's frame exactly — a client cannot tell the two
                    // backends apart, which is the whole point.
                    if new_controller_id != client_id {
                        control_epoch = None;
                        send_json(
                            &mut socket,
                            &json!({
                                "type": "control_revoked",
                                "to": new_controller_name,
                                "client_id": new_controller_id,
                            }),
                        )
                        .await;
                    }
                }
                Ok(SessionEvent::ControlReleased { by, resume_hint }) => {
                    // Everyone except the releaser, same frame ws.rs sends
                    // (docs/04-api-protocol.md#releasing-and-handing-the-
                    // lease-back) -- a client cannot tell the two backends
                    // apart, so neither may the hand-back path.
                    if by != client_id {
                        send_json(
                            &mut socket,
                            &json!({
                                "type": "control_released",
                                "by": by,
                                "resume_hint": resume_hint,
                            }),
                        )
                        .await;
                    }
                }
                // `Resized`: nothing announces one through the lease
                // registry on this path any more — anchor's own frame does
                // it, with the applied size rather than the requested one.
                // The variant stays because `ws.rs` still uses it.
                //
                // `Err`: a lagged receiver missed a notification; every
                // later check re-reads authoritative state, so there is
                // nothing to resync. A closed sender means teardown, which
                // the other arms notice.
                Ok(SessionEvent::Resized { .. }) | Err(_) => {}
            },

            message = socket.recv() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    // Input, but only if teleport's lease still says so —
                    // re-checked per message, so a connection preempted
                    // mid-stream stops typing immediately.
                    let allowed = control_epoch
                        .is_some_and(|epoch| leases.is_controller(&session_id, &client_id, epoch));
                    if allowed {
                        if let Err(e) = attach.send_input(&bytes).await {
                            // The lease said yes and the write still failed,
                            // which on an attached connection means the far
                            // end is going away: the session is terminating,
                            // or anchord is. `session_closing` is the code
                            // the in-process path raises for the first of
                            // those (`ws.rs`), and the one that tells a
                            // client to disable input rather than retype
                            // (docs/04-api-protocol.md#error-codes). Sent
                            // before breaking, because the close frame
                            // behind it says nothing about why.
                            tracing::debug!(session_id = %session_id, error = %e, "write rejected");
                            send_json(
                                &mut socket,
                                &json!({
                                    "type": "error",
                                    "code": "session_closing",
                                    "message": "input rejected: session is closing",
                                }),
                            )
                            .await;
                            break;
                        }
                    } else {
                        // Told, not dropped silently: an observer whose
                        // keystrokes vanish without explanation looks like
                        // a broken terminal rather than a taken lease.
                        send_json(
                            &mut socket,
                            &json!({
                                "type": "error",
                                "code": "not_controller",
                                "message": "input rejected: observer mode",
                            }),
                        )
                        .await;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if !handle_control(
                        &text,
                        &leases,
                        &mut attach,
                        &session_id,
                        &client_id,
                        &client_name,
                        &mut control_epoch,
                        &mut socket,
                    )
                    .await
                    {
                        break;
                    }
                }
                Some(Ok(Message::Pong(_))) => last_pong = Instant::now(),
                // Axum answers pings itself; anything else ends the loop.
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },

            _ = ping_interval.tick() => {
                if last_pong.elapsed() > PONG_TIMEOUT {
                    drop(socket.send(close(1001, "pong timeout")).await);
                    break;
                }
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    // The socket is gone. Start teleport's grace window rather than freeing
    // the lease outright: a browser that dropped on a network blip must get
    // its own lease back when it returns, not have to fight for it.
    if let Some(epoch) = control_epoch {
        leases.begin_control_grace(&session_id, client_id, epoch, control_grace_ms);
    }
}

/// Handles one control message. `false` ends the connection.
async fn handle_control(
    text: &str,
    leases: &Arc<LeaseRegistry>,
    attach: &mut AnchorAttach,
    session_id: &str,
    client_id: &str,
    client_name: &str,
    control_epoch: &mut Option<u64>,
    socket: &mut WebSocket,
) -> bool {
    let message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(e) => {
            send_json(
                socket,
                &json!({ "type": "error", "code": "bad_request", "message": e.to_string() }),
            )
            .await;
            return true;
        }
    };

    match message {
        ClientMessage::Resize { cols, rows } => {
            let allowed = control_epoch
                .is_some_and(|epoch| leases.is_controller(session_id, client_id, epoch));
            if !allowed {
                send_json(
                    socket,
                    &json!({
                        "type": "error",
                        "code": "not_controller",
                        "message": "resize rejected: observer mode",
                    }),
                )
                .await;
                return true;
            }
            // On the attached connection: anchor's lease is per-connection,
            // and this is the one holding it. A control op would arrive on
            // a different connection and be refused.
            if let Err(e) = attach.resize(cols, rows).await {
                tracing::debug!(session_id = %session_id, error = %e, "resize rejected");
                return true;
            }
            // Deliberately no local announce. Anchor sends a `resize` frame
            // to every attached connection, this one included, carrying the
            // size it *applied* — after clamping and coalescing. Teleport
            // broadcasting its own would race that frame and carry the
            // requested size instead, so a client asking for 5000 would be
            // told 5000 and then 1000. The authoritative one wins by being
            // the only one.
        }
        ClientMessage::ClaimControl => {
            // Teleport's lease moves first; anchor's follows, so the client
            // teleport now says may type actually can.
            *control_epoch = Some(leases.claim_control(session_id, client_id, client_name));
            if let Err(e) = attach.claim_control().await {
                tracing::warn!(session_id = %session_id, error = %e, "claiming anchor's lease failed");
                return false;
            }
            send_json(socket, &json!({ "type": "control_granted" })).await;
        }
        ClientMessage::ReleaseControl => {
            if let Some(epoch) = control_epoch.take() {
                leases.release_control(session_id, client_id, epoch);
            }
        }
    }
    true
}

async fn send_json(socket: &mut WebSocket, value: &serde_json::Value) {
    drop(socket.send(Message::Text(value.to_string().into())).await);
}

async fn send_binary(socket: &mut WebSocket, offset: u64, bytes: &[u8]) -> Result<(), axum::Error> {
    let mut frame = Vec::with_capacity(8 + bytes.len());
    frame.extend_from_slice(&offset.to_be_bytes());
    frame.extend_from_slice(bytes);
    socket.send(Message::Binary(frame.into())).await
}

fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}

/// Moves a replay cursor that lands in a capped region up to the head.
///
/// `log_capped_at` is where anchord's log file stopped; the offset stream
/// kept going past it, so bytes at or above it were streamed live and are
/// gone. A client asking for them gets no replay and is told where the
/// stream resumes — the same answer `Session::attach` gives in-process,
/// where the readable end plays this role.
///
/// `None` means nothing was capped, so there is nothing to clamp except the
/// degenerate `from == head`, which is already the head.
fn clamp_past_cap(from: u64, head: u64, log_capped_at: Option<u64>) -> u64 {
    if from >= head.min(log_capped_at.unwrap_or(head)) {
        head
    } else {
        from
    }
}

#[cfg(test)]
mod tests {
    use super::clamp_past_cap;

    #[test]
    fn an_uncapped_log_replays_from_wherever_it_was_asked() {
        assert_eq!(clamp_past_cap(500, 1000, None), 500);
        assert_eq!(clamp_past_cap(0, 1000, None), 0);
    }

    #[test]
    fn a_cursor_below_the_cap_still_has_bytes_to_read() {
        assert_eq!(clamp_past_cap(500, 1000, Some(800)), 500);
        assert_eq!(clamp_past_cap(799, 1000, Some(800)), 799);
    }

    #[test]
    fn a_cursor_in_the_capped_region_resumes_at_the_head() {
        // 800..1000 was streamed live and never written, so there is
        // nothing between 800 and the head to replay.
        assert_eq!(clamp_past_cap(800, 1000, Some(800)), 1000);
        assert_eq!(clamp_past_cap(950, 1000, Some(800)), 1000);
    }

    #[test]
    fn the_head_itself_is_the_head_either_way() {
        assert_eq!(clamp_past_cap(1000, 1000, None), 1000);
        assert_eq!(clamp_past_cap(1000, 1000, Some(800)), 1000);
    }
}
