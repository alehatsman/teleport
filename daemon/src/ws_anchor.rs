//! The WebSocket proxy for anchor-hosted sessions.
//!
//! Teleport's clients keep speaking teleport's protocol
//! (docs/04-api-protocol.md); this translates it onto anchor's attach
//! stream. A deliberately separate path from `ws::run`: that loop is the
//! authoritative one while the extraction is in flight, and threading an
//! `Option<Session>` through it would put the risk in the code that
//! currently works rather than in the code that is new.
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

use crate::anchor_client::{AnchorClient, AnchorFrame};
use crate::anchor_lease::LeaseRegistry;

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
            send_json(
                &mut socket,
                &json!({ "type": "error", "code": "not_found", "message": e.to_string() }),
            )
            .await;
            drop(socket.send(close(1011, "session not found")).await);
            return;
        }
    };

    let (from, truncated) = crate::ws::bound_attach(
        after,
        tail,
        default_tail,
        max_replay_bytes,
        session.output_bytes,
    );

    // Teleport's lease first: it decides who may type, and anchor's is
    // claimed to match it below.
    let mut control_epoch = if requested_control {
        leases.attach_control(&session_id, &client_id, &client_name)
    } else {
        None
    };

    let mut attach = match anchor
        .attach(&session_id, from, control_epoch.is_some())
        .await
    {
        Ok(attach) => attach,
        Err(e) => {
            send_json(
                &mut socket,
                &json!({ "type": "error", "code": "offset_ahead", "message": e.to_string() }),
            )
            .await;
            drop(socket.send(close(1008, "attach refused")).await);
            return;
        }
    };

    // Teleport granted the lease but anchor gave it to an earlier
    // connection: claim it, so the client teleport says may type actually
    // can. Attach never preempts, which is why this is a separate act.
    if control_epoch.is_some() && !attach.has_control() {
        if let Err(e) = attach.claim_control().await {
            tracing::warn!(session_id = %session_id, error = %e, "claiming anchor's lease failed");
        }
    }

    send_json(
        &mut socket,
        &json!({
            "type": "ready",
            "session_id": session_id,
            "replay_from": from,
            "next_offset": attach.next_offset(),
            "truncated": truncated,
            "log_capped_at": session.log_capped_at,
            "cols": session.cols,
            "rows": session.rows,
            "control": control_epoch.is_some(),
            "controller": leases.controller_name(&session_id),
        }),
    )
    .await;

    let mut lease_events = leases.subscribe(&session_id);
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // the first tick is immediate; consume it.
    let mut last_pong = Instant::now();

    loop {
        tokio::select! {
            frame = attach.next_frame() => match frame {
                Ok(Some(AnchorFrame::Output { offset, bytes })) => {
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
                            "final_offset": attach.next_offset(),
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
                // The stream ended without an exit: anchor dropped this
                // subscriber for falling behind. Closing is the signal to
                // reconnect from the last offset, which is what the client
                // already does after any disconnect.
                Ok(None) => {
                    drop(socket.send(close(1001, "stream ended")).await);
                    break;
                }
                Err(e) => {
                    tracing::debug!(session_id = %session_id, error = %e, "anchor stream failed");
                    break;
                }
            },

            event = lease_events.recv() => {
                if let Ok(event) = event {
                    if event.new_controller_id != client_id {
                        control_epoch = None;
                    }
                    send_json(
                        &mut socket,
                        &json!({
                            "type": "control_revoked",
                            "lost_by": event.lost_by,
                            "new_controller_id": event.new_controller_id,
                            "new_controller_name": event.new_controller_name,
                        }),
                    )
                    .await;
                }
            },

            message = socket.recv() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    // Input, but only if teleport's lease still says so —
                    // re-checked per message, so a connection preempted
                    // mid-stream stops typing immediately.
                    let allowed = control_epoch
                        .is_some_and(|epoch| leases.is_controller(&session_id, &client_id, epoch));
                    if allowed && attach.send_input(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if !handle_control(
                        &text,
                        &anchor,
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
#[expect(
    clippy::too_many_arguments,
    reason = "one proxied connection's state; bundling it into a struct would just move the same fields"
)]
async fn handle_control(
    text: &str,
    anchor: &AnchorClient,
    leases: &Arc<LeaseRegistry>,
    attach: &mut crate::anchor_client::AnchorAttach,
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
            if let Err(e) = anchor.resize(session_id, cols, rows).await {
                tracing::debug!(session_id = %session_id, error = %e, "resize rejected");
            }
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
