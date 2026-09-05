//! `auth.rs` -- the one seam every request resolves through before any
//! handler runs (docs/12-identity-and-connectivity.md#the-principal).
//! Handlers take a [`Principal`], never headers; when stage 2/3 add real
//! multi-principal authorization, this module and one policy function change,
//! not every handler.
//!
//! Two independent checks live here, deliberately kept apart
//! (docs/06-security.md#browser-origin-defense vs
//! docs/06-security.md#authentication):
//!
//! - [`OriginPolicy::check`] -- Origin/Host allowlisting, a defense against a
//!   malicious *page* in the user's own browser. Enforced only on mutating
//!   HTTP and the WS upgrade (docs/06-security.md#browser-origin-defense).
//! - [`resolve`] -- the bearer-token credential, required on every
//!   `/api/v1` request except unauthenticated `/health`
//!   (docs/06-security.md#authentication).
//! - [`resolve_ws`]/[`TicketStore`] -- the WS-upgrade-specific credential:
//!   a short-lived, single-use ticket in place of the bearer token, so the
//!   long-lived secret never has to ride in a WebSocket URL
//!   (docs/06-security.md#token-on-the-websocket-upgrade, mitigation 2).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::http::{header, HeaderMap};

use crate::session::SessionId;

/// Who is making this request. In the MVP (stage 1) every variant that can
/// actually be produced authorizes identically -- there is one user -- but
/// the *shape* matters: stage 3 changes [`resolve`] and one policy function,
/// not forty handlers (docs/12-identity-and-connectivity.md#the-principal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// Presented the local token from `<data_dir>/token`.
    LocalUser,
    /// Presented a valid bearer token issued to a specific device. Not
    /// distinguished from `LocalUser` yet -- stage 1 has one token, not one
    /// per device -- but the variant exists so the shape is already right.
    #[allow(dead_code)]
    DeviceToken { token_id: String },
    /// Stage 3, established by the cloud backend. Unreachable in the MVP.
    #[allow(dead_code)]
    Account { user_id: String, device_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// Maps to the `unauthorized` error code / `401`
    /// (docs/04-api-protocol.md#error-codes).
    #[error("missing or invalid credential")]
    Unauthorized,
    /// Maps to the `bad_origin` error code / `403`
    /// (docs/04-api-protocol.md#error-codes).
    #[error("Origin or Host rejected")]
    BadOrigin,
}

/// Resolves a [`Principal`] from a bearer token, presented either as
/// `Authorization: Bearer <token>` (every client class) or `?token=`
/// (browsers only, since the WebSocket API cannot set headers --
/// docs/06-security.md#token-on-the-websocket-upgrade). Constant-time
/// compare against `expected_token` -- a naive `==` on a secret is a timing
/// oracle (docs/06-security.md#authentication).
///
/// `auth_required = false` is `auth_token = false` in `config.toml`: every
/// caller becomes `LocalUser` unconditionally. Document it as a
/// single-user-machine convenience; it is not the default
/// (docs/06-security.md#authentication).
pub fn resolve(
    headers: &HeaderMap,
    query_token: Option<&str>,
    expected_token: &str,
    auth_required: bool,
) -> Result<Principal, AuthError> {
    if !auth_required {
        return Ok(Principal::LocalUser);
    }
    let presented = bearer_from_header(headers).or(query_token);
    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), expected_token.as_bytes()) => {
            Ok(Principal::LocalUser)
        }
        _ => Err(AuthError::Unauthorized),
    }
}

/// Resolves the credential for a WS upgrade specifically: a `ticket` (if
/// present) redeemed against `store` and scoped to `session_id`, otherwise
/// the normal bearer/`?token=` path via [`resolve`]. Ticket-checking is
/// independent of `auth_required` -- a valid ticket already proves a very
/// recent, separately-authenticated `POST /api/v1/ws-ticket` call, so there
/// is nothing left for the disabled-auth escape hatch to add
/// (docs/06-security.md#token-on-the-websocket-upgrade, mitigation 2).
#[allow(clippy::too_many_arguments)]
pub fn resolve_ws(
    store: &TicketStore,
    session_id: SessionId,
    ticket: Option<&str>,
    headers: &HeaderMap,
    query_token: Option<&str>,
    expected_token: &str,
    auth_required: bool,
) -> Result<Principal, AuthError> {
    if let Some(ticket) = ticket {
        return if store.redeem(ticket, session_id) {
            Ok(Principal::LocalUser)
        } else {
            Err(AuthError::Unauthorized)
        };
    }
    resolve(headers, query_token, expected_token, auth_required)
}

/// How long an unredeemed ticket stays valid (docs/06-security.md: "30-second
/// token"). Generous enough for a slow mobile connect, short enough that a
/// leaked ticket (proxy log, browser history -- the exact exposure this
/// replaces) is worthless within the minute.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// 128 bits (docs/06-security.md's own floor for the credential this
/// stands in for).
const TICKET_BYTES: usize = 16;

struct Ticket {
    session_id: SessionId,
    expires_at: Instant,
}

/// In-memory, single-use tickets for the WS upgrade. Never persisted --
/// restarting the daemon invalidates every outstanding ticket, which is
/// correct: nothing durable should ever depend on a 30-second credential.
pub struct TicketStore {
    tickets: parking_lot::Mutex<HashMap<String, Ticket>>,
}

impl Default for TicketStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TicketStore {
    pub fn new() -> Self {
        Self {
            tickets: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Issues a ticket scoped to `session_id`. Sweeps expired entries first
    /// -- the only cleanup this store needs, since a 30s TTL means the map
    /// never holds more than a few tens of seconds' worth of issuance even
    /// if every ticket goes unredeemed.
    pub fn issue(&self, session_id: SessionId) -> Result<String, getrandom::Error> {
        let mut bytes = [0u8; TICKET_BYTES];
        getrandom::getrandom(&mut bytes)?;
        let ticket = hex_encode(&bytes);

        let mut tickets = self.tickets.lock();
        let now = Instant::now();
        tickets.retain(|_, t| t.expires_at > now);
        tickets.insert(
            ticket.clone(),
            Ticket {
                session_id,
                expires_at: now + TICKET_TTL,
            },
        );
        Ok(ticket)
    }

    /// Redeems `ticket` for `session_id`. Single-use: a ticket found in the
    /// map is removed regardless of whether it actually matches
    /// `session_id` and hasn't expired -- so a replayed ticket (copied URL,
    /// proxy log) fails the second time, *and* a wrong-session guess can't
    /// be retried against the same ticket once it's been tried. Only "found,
    /// right session, not expired" returns `true`; everything else
    /// (mismatch, expiry, already redeemed, or simply unknown) is `false`.
    pub fn redeem(&self, ticket: &str, session_id: SessionId) -> bool {
        let mut tickets = self.tickets.lock();
        match tickets.remove(ticket) {
            Some(t) => t.session_id == session_id && t.expires_at > Instant::now(),
            None => false,
        }
    }
}

/// Shared by every caller that needs to print random secret bytes as a
/// credential -- the daemon token (`main.rs::load_or_create_token`) and WS
/// tickets (`TicketStore::issue`) both go through this one copy, not two
/// independently-maintained ones (code review, PR #22). `pub`, not
/// `pub(crate)`: `main.rs` is the separate `teleportd` *binary* crate calling
/// into this *library* crate, so crate-visibility doesn't reach it.
pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("writing to a String cannot fail");
    }
    s
}

fn bearer_from_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Equal-time byte comparison. The length check short-circuits, but a
/// token's length is not the secret -- its value is -- so that leak is
/// accepted the same way every standard constant-time-compare
/// implementation accepts it.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The computed Origin/Host allowlist, built once at startup from the bound
/// port and `config.toml`'s `allowed_origins`/`allowed_hosts`
/// (docs/06-security.md#browser-origin-defense). Cheap to check per request;
/// nothing here does I/O.
#[derive(Debug, Clone)]
pub struct OriginPolicy {
    allowed_origins: Vec<String>,
    /// Bare hostnames (no port) -- the `Host` header is compared with its
    /// port stripped, since a remote-access transport (Tailscale Serve,
    /// Cloudflare Tunnel) can terminate on a port `config.toml` never named
    /// (docs/07-remote-access.md).
    allowed_hosts: Vec<String>,
}

impl OriginPolicy {
    /// `include_dev_server` should be `cfg!(debug_assertions)` -- the Vite
    /// dev origin is only ever legitimate in a debug build
    /// (docs/06-security.md#browser-origin-defense).
    pub fn new(
        bound_port: u16,
        include_dev_server: bool,
        extra_origins: &[String],
        extra_hosts: &[String],
    ) -> Self {
        let mut allowed_origins = vec![
            format!("http://127.0.0.1:{bound_port}"),
            format!("http://localhost:{bound_port}"),
            "tauri://localhost".to_string(),
            "https://tauri.localhost".to_string(),
        ];
        if include_dev_server {
            allowed_origins.push("http://localhost:5173".to_string());
        }
        allowed_origins.extend(extra_origins.iter().cloned());

        let mut allowed_hosts = vec!["127.0.0.1".to_string(), "localhost".to_string()];
        allowed_hosts.extend(extra_hosts.iter().cloned());

        Self {
            allowed_origins,
            allowed_hosts,
        }
    }

    /// Enforce on mutating HTTP (`POST`, `DELETE`) and the WS upgrade only
    /// (docs/06-security.md#browser-origin-defense) -- callers decide when
    /// to invoke this, it is not applied globally.
    ///
    /// ```text
    /// Host present and allowed        -> continue
    /// Host missing or not allowed     -> bad_origin
    /// Origin present and allowed      -> continue
    /// Origin present and not allowed  -> bad_origin
    /// Origin absent                   -> continue (not a browser; resolve() enforces the credential)
    /// ```
    pub fn check(&self, headers: &HeaderMap) -> Result<(), AuthError> {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthError::BadOrigin)?;
        let bare_host = host.split(':').next().unwrap_or(host);
        if !self.allowed_hosts.iter().any(|h| h == bare_host) {
            return Err(AuthError::BadOrigin);
        }

        if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            if !self.allowed_origins.iter().any(|o| o == origin) {
                return Err(AuthError::BadOrigin);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.insert(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        h
    }

    #[test]
    fn auth_disabled_always_grants_local_user() {
        let h = HeaderMap::new();
        assert_eq!(resolve(&h, None, "secret", false), Ok(Principal::LocalUser));
    }

    #[test]
    fn missing_credential_is_unauthorized() {
        let h = HeaderMap::new();
        assert_eq!(
            resolve(&h, None, "secret", true),
            Err(AuthError::Unauthorized)
        );
    }

    #[test]
    fn header_bearer_token_is_accepted() {
        let h = headers(&[(header::AUTHORIZATION, "Bearer secret")]);
        assert_eq!(resolve(&h, None, "secret", true), Ok(Principal::LocalUser));
    }

    #[test]
    fn query_token_is_accepted_when_header_is_absent() {
        let h = HeaderMap::new();
        assert_eq!(
            resolve(&h, Some("secret"), "secret", true),
            Ok(Principal::LocalUser)
        );
    }

    #[test]
    fn header_takes_precedence_over_a_mismatched_query_token() {
        let h = headers(&[(header::AUTHORIZATION, "Bearer secret")]);
        assert_eq!(
            resolve(&h, Some("wrong"), "secret", true),
            Ok(Principal::LocalUser)
        );
    }

    #[test]
    fn wrong_token_is_unauthorized() {
        let h = headers(&[(header::AUTHORIZATION, "Bearer nope")]);
        assert_eq!(
            resolve(&h, None, "secret", true),
            Err(AuthError::Unauthorized)
        );
    }

    #[test]
    fn constant_time_eq_matches_naive_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn missing_origin_and_valid_host_is_accepted() {
        // A native client: no Origin, but Host still must be present and
        // allowed (docs/06-security.md: "Host must be in the allowlist,
        // always"). The credential check is `resolve`'s job, not this one's.
        let policy = OriginPolicy::new(7337, false, &[], &[]);
        let h = headers(&[(header::HOST, "127.0.0.1:7337")]);
        assert_eq!(policy.check(&h), Ok(()));
    }

    #[test]
    fn allowed_origin_is_accepted() {
        let policy = OriginPolicy::new(7337, false, &[], &[]);
        let h = headers(&[
            (header::HOST, "127.0.0.1:7337"),
            (header::ORIGIN, "http://127.0.0.1:7337"),
        ]);
        assert_eq!(policy.check(&h), Ok(()));
    }

    #[test]
    fn unknown_origin_is_rejected() {
        let policy = OriginPolicy::new(7337, false, &[], &[]);
        let h = headers(&[
            (header::HOST, "127.0.0.1:7337"),
            (header::ORIGIN, "https://evil.example"),
        ]);
        assert_eq!(policy.check(&h), Err(AuthError::BadOrigin));
    }

    #[test]
    fn unknown_host_is_rejected() {
        let policy = OriginPolicy::new(7337, false, &[], &[]);
        let h = headers(&[(header::HOST, "evil.example:7337")]);
        assert_eq!(policy.check(&h), Err(AuthError::BadOrigin));
    }

    #[test]
    fn missing_host_is_rejected() {
        let policy = OriginPolicy::new(7337, false, &[], &[]);
        assert_eq!(policy.check(&HeaderMap::new()), Err(AuthError::BadOrigin));
    }

    #[test]
    fn configured_extra_origin_and_host_are_accepted() {
        let policy = OriginPolicy::new(
            7337,
            false,
            &["https://desktop.tail1234.ts.net".to_string()],
            &["desktop.tail1234.ts.net".to_string()],
        );
        let h = headers(&[
            (header::HOST, "desktop.tail1234.ts.net"),
            (header::ORIGIN, "https://desktop.tail1234.ts.net"),
        ]);
        assert_eq!(policy.check(&h), Ok(()));
    }

    #[test]
    fn dev_server_origin_only_allowed_when_enabled() {
        let debug = OriginPolicy::new(7337, true, &[], &[]);
        let release = OriginPolicy::new(7337, false, &[], &[]);
        let h = headers(&[
            (header::HOST, "127.0.0.1:7337"),
            (header::ORIGIN, "http://localhost:5173"),
        ]);
        assert_eq!(debug.check(&h), Ok(()));
        assert_eq!(release.check(&h), Err(AuthError::BadOrigin));
    }

    fn sid(s: &str) -> SessionId {
        s.parse().expect("valid ULID literal in test")
    }

    #[test]
    fn ticket_redeems_once_for_the_right_session() {
        let store = TicketStore::new();
        let session = sid("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let ticket = store.issue(session).unwrap();

        assert!(store.redeem(&ticket, session));
        // Single-use: the same ticket fails the second time.
        assert!(!store.redeem(&ticket, session));
    }

    #[test]
    fn ticket_rejected_for_the_wrong_session_and_consumed_either_way() {
        let store = TicketStore::new();
        let issued_for = sid("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let wrong = sid("01ARZ3NDEKTSV4RRFFQ69G5FAX");
        let ticket = store.issue(issued_for).unwrap();

        assert!(!store.redeem(&ticket, wrong));
        // Removed on *any* redeem attempt, matched or not -- otherwise a
        // wrong-session guess could be retried indefinitely against the
        // same ticket, turning "single-use" into "single-use per session
        // guessed correctly." The right session gets nothing back either.
        assert!(!store.redeem(&ticket, issued_for));
    }

    #[test]
    fn unknown_ticket_is_rejected() {
        let store = TicketStore::new();
        assert!(!store.redeem("nonexistent", sid("01ARZ3NDEKTSV4RRFFQ69G5FAV")));
    }

    #[test]
    fn expired_ticket_is_rejected() {
        let store = TicketStore::new();
        let session = sid("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let ticket = store.issue(session).unwrap();
        // Backdate it past its TTL directly rather than sleeping 30s in a test.
        store.tickets.lock().get_mut(&ticket).unwrap().expires_at =
            Instant::now() - Duration::from_secs(1);

        assert!(!store.redeem(&ticket, session));
    }

    #[test]
    fn resolve_ws_accepts_a_valid_ticket_even_with_no_header_or_query_token() {
        let store = TicketStore::new();
        let session = sid("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let ticket = store.issue(session).unwrap();
        let h = HeaderMap::new();

        assert_eq!(
            resolve_ws(&store, session, Some(&ticket), &h, None, "secret", true),
            Ok(Principal::LocalUser)
        );
    }

    #[test]
    fn resolve_ws_falls_back_to_bearer_token_when_no_ticket_is_presented() {
        let store = TicketStore::new();
        let session = sid("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let h = headers(&[(header::AUTHORIZATION, "Bearer secret")]);

        assert_eq!(
            resolve_ws(&store, session, None, &h, None, "secret", true),
            Ok(Principal::LocalUser)
        );
    }

    #[test]
    fn resolve_ws_rejects_an_invalid_ticket_without_falling_back_to_the_token() {
        let store = TicketStore::new();
        let session = sid("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        // A header carrying the *correct* master token is present, but an
        // invalid ticket must still fail closed -- it must never silently
        // fall through to the token check.
        let h = headers(&[(header::AUTHORIZATION, "Bearer secret")]);

        assert_eq!(
            resolve_ws(&store, session, Some("bogus"), &h, None, "secret", true),
            Err(AuthError::Unauthorized)
        );
    }
}
