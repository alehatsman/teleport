//! `GET /api/v1/sessions/{id}/stream` -- the WebSocket protocol
//! (docs/04-api-protocol.md#websocket-protocol). One connection per attached
//! terminal: mixed framing (text = control JSON, binary = raw PTY bytes
//! prefixed with an 8-byte big-endian offset), the attach/catch-up sequence
//! sequence, and the control lease.
//!
//! Only the upgrade lives here: the connection itself is `ws_anchor`'s
//! proxy over an anchor attach, since every session lives in anchord
//! (docs/20). `bound_attach` stays because that proxy uses it.

use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::api::AppState;
use crate::auth;
use crate::session::SessionId;

/// Query params on `GET /api/v1/sessions/{id}/stream`
/// (docs/04-api-protocol.md#websocket-protocol).
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    after: Option<u64>,
    tail: Option<u64>,
    #[serde(default)]
    mode: StreamMode,
    client_id: Option<String>,
    client_name: Option<String>,
    /// Path 3 (docs/06-security.md#token-on-the-websocket-upgrade): the
    /// long-lived bearer token directly in the query string. Ignored when
    /// `ticket` is also present -- `stream.ts` no longer sends both, but a
    /// future/native client that does gets the safer credential honored.
    token: Option<String>,
    /// Path 2, preferred: a short-lived, single-use ticket from
    /// `POST /api/v1/ws-ticket`, scoped to this exact session id.
    ticket: Option<String>,
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
/// of that holds, upgrades and hands off to `ws_anchor::run_anchor`. Everything before the
/// upgrade can still return an ordinary HTTP error response; nothing after
/// it can, which is exactly why these checks come first.
///
/// Origin/Host is checked *before* the id is even parsed, unconditionally,
/// same as every other mutating route -- a code-review finding on the PR
/// this shipped in caught an earlier draft skipping it for a malformed id.
///
/// Unlike every other route, the credential isn't resolved through the
/// generic [`Principal`](crate::auth::Principal) extractor: a ticket is
/// scoped to a session id, which only this handler's own `Path<String>` has
/// -- so credential resolution happens explicitly, via [`auth::resolve_ws`],
/// once the id is parsed (format only, not looked up yet -- a malformed id
/// is not sensitive, so it's fine for that parse to happen before the
/// credential check, just not before Origin).
pub async fn upgrade(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(e) = state.origin_policy.check(&headers) {
        return crate::api::ApiError::from(e).into_response();
    }
    let Ok(session_id) = id.parse::<SessionId>() else {
        return (axum::http::StatusCode::NOT_FOUND, "session not found").into_response();
    };
    if let Err(e) = auth::resolve_ws_with_sessions(
        &state.ws_tickets,
        session_id,
        q.ticket.as_deref(),
        &headers,
        q.token.as_deref(),
        &state.token,
        state.config.auth_token,
        state.db.as_ref(),
        crate::now_ms(),
    )
    .await
    {
        return crate::api::ApiError::from(e).into_response();
    }
    if q.after.is_some() && q.tail.is_some() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "after and tail are mutually exclusive",
        )
            .into_response();
    }
    let anchor = state.anchor.clone();

    // A stable per-install id names the controller in the UI and lets a
    // reconnecting client resume its own lease; a client that omits it gets
    // an ephemeral one and loses lease resumption
    // (docs/04-api-protocol.md#client-identity).
    let client_id = q
        .client_id
        .clone()
        .unwrap_or_else(|| format!("ephemeral-{}", ulid::Ulid::new()));
    let client_name = q
        .client_name
        .clone()
        .unwrap_or_else(|| client_id.chars().take(8).collect());
    let requested_control = q.mode == StreamMode::Control;
    let default_tail = state.config.default_tail;
    let max_replay_bytes = state.config.max_replay_bytes;
    let control_grace_ms = state.config.control_grace_ms;

    {
        let id = session_id.to_string();
        // Checked before the upgrade, so an unknown session is a 404 on the
        // HTTP response rather than an error frame after a successful
        // upgrade -- the shape clients have always been given.
        //
        // Only `not_found` is a 404. An unreachable anchord is an outage, and
        // reporting it as "session not found" tells a browser its work is
        // gone at the one moment the session is most likely still alive --
        // a teleportd or anchord restart, which is what this path exists to
        // survive. A client that believes a 404 stops reconnecting.
        // `list_sessions` already draws this line (`api.rs`, the comment
        // about "a visible outage and silently vanished work"); the two
        // surfaces must not answer the same condition differently.
        if let Err(e) = anchor.get(&id).await {
            return if crate::anchor_bridge::is_not_found(&e) {
                (axum::http::StatusCode::NOT_FOUND, "session not found").into_response()
            } else {
                tracing::warn!(session_id = %id, error = %e, "cannot reach anchord for a stream upgrade");
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "cannot reach anchord",
                )
                    .into_response()
            };
        }
        let leases = Arc::clone(&state.leases);
        ws.on_upgrade(move |socket| {
            crate::ws_anchor::run_anchor(crate::ws_anchor::Proxy {
                socket,
                anchor,
                leases,
                session_id: id,
                client_id,
                client_name,
                requested_control,
                after: q.after,
                tail: q.tail,
                default_tail,
                max_replay_bytes,
                control_grace_ms,
            })
        })
    }
}

/// Bounds the requested cursor before it reaches anchord's attach
/// (docs/04-api-protocol.md#bounded-attach). Returns the clamped `from` and
/// whether it moved -- `tail`/default-tail clamping is normal tail semantics
/// and never counts as truncation; only an `after` moved forward by
/// `max_replay_bytes` does (docs/04-api-protocol.md#bounded-attach: "When a
/// replay is clamped, `ready` says so").
pub(crate) fn bound_attach(
    after: Option<u64>,
    tail: Option<u64>,
    default_tail: u64,
    max_replay_bytes: u64,
    next_offset: u64,
) -> (u64, bool) {
    if let Some(after) = after {
        let earliest = next_offset.saturating_sub(max_replay_bytes);
        let from = after.max(earliest);
        (from, from > after)
    } else {
        let tail = tail.unwrap_or(default_tail).min(max_replay_bytes);
        (next_offset.saturating_sub(tail), false)
    }
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
        assert_eq!(
            from,
            next_offset - DEFAULT_TAIL,
            "must replay default_tail, not the whole log"
        );
        assert!(
            !truncated,
            "tail semantics are not a broken promise -- never flagged truncated"
        );
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
        assert_eq!(
            from,
            next_offset - MAX_REPLAY,
            "replay_from must be exactly the max_replay_bytes boundary"
        );
        assert!(truncated);
    }

    /// An `after` that already falls inside `max_replay_bytes` of the
    /// boundary needs no clamp -- not every `after` request is truncated.
    #[test]
    fn after_within_the_replay_bound_is_not_truncated() {
        let next_offset = 10_000_000;
        let after = next_offset - 100; // well within max_replay_bytes
        let (from, truncated) =
            bound_attach(Some(after), None, DEFAULT_TAIL, MAX_REPLAY, next_offset);
        assert_eq!(from, after);
        assert!(!truncated);
    }

    /// `max_replay_bytes` caps every replay "regardless of parameters"
    /// (docs/04-api-protocol.md#bounded-attach) -- including an oversized
    /// explicit `tail`.
    #[test]
    fn an_oversized_tail_is_also_capped_by_max_replay_bytes() {
        let next_offset = 500_000_000;
        let (from, _) = bound_attach(
            None,
            Some(50 * 1024 * 1024),
            DEFAULT_TAIL,
            MAX_REPLAY,
            next_offset,
        );
        assert_eq!(from, next_offset - MAX_REPLAY);
    }
}
