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
    DeviceToken {
        /// The device the presented token was issued to.
        token_id: String,
    },
    /// Stage 3, established by the cloud backend. Unreachable in the MVP.
    Account {
        /// The authenticated cloud account.
        user_id: String,
        /// The specific device within that account.
        device_id: String,
    },
}

/// Why [`resolve`]/[`resolve_ws`] refused a request.
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

/// `sha256(token)` -- how a login session is addressed in SQLite. The token
/// itself is never stored (docs/17-passkey-login.md#data-model), so this is
/// the only form the daemon keeps at rest, and lookup is an indexed exact
/// match on the digest rather than a scan or a comparison against a secret.
pub fn token_digest(token: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).to_vec()
}

/// [`resolve`], plus the passkey-session branch
/// (docs/17-passkey-login.md#principal-mapping). Async because that branch
/// is a SQLite lookup; every other path is the same constant-time compare
/// [`resolve`] already does and never touches `db`.
///
/// Order matters and is deliberate: the **master token wins**. It is
/// compared first, in constant time, so a daemon whose database is
/// unreachable still authenticates its owner, and so the recovery path can
/// never be shadowed by session state (docs/06-security.md#passkeys-a-second-stage-12-credential).
///
/// A session that is unknown, revoked or expired is `Unauthorized`, with no
/// way for the caller to tell which -- expiry is applied inside the query
/// (`auth_store.rs`), so there is no arrangement of code here that can
/// forget to check it.
pub async fn resolve_with_sessions(
    headers: &HeaderMap,
    query_token: Option<&str>,
    expected_token: &str,
    auth_required: bool,
    db: Option<&crate::persistence::Db>,
    now_ms: i64,
) -> Result<Principal, AuthError> {
    if !auth_required {
        return Ok(Principal::LocalUser);
    }
    let Some(presented) = bearer_from_header(headers).or(query_token) else {
        return Err(AuthError::Unauthorized);
    };
    if constant_time_eq(presented.as_bytes(), expected_token.as_bytes()) {
        return Ok(Principal::LocalUser);
    }
    let Some(db) = db else {
        return Err(AuthError::Unauthorized);
    };
    match db
        .lookup_auth_session(token_digest(presented), now_ms)
        .await
    {
        Ok(Some(row)) => Ok(Principal::DeviceToken { token_id: row.id }),
        Ok(None) => Err(AuthError::Unauthorized),
        Err(e) => {
            // A database error is not an authorization decision. Fail
            // closed and say so -- silently degrading to "unauthorized"
            // with no trace would make a broken DB look like a wrong
            // password to whoever is debugging it at 2am.
            tracing::warn!(error = %e, "auth session lookup failed; refusing the request");
            Err(AuthError::Unauthorized)
        }
    }
}

/// [`resolve_ws`] with the same passkey-session branch as
/// [`resolve_with_sessions`]. A valid ticket still short-circuits everything
/// else, for the reason [`resolve_ws`] documents.
#[expect(
    clippy::too_many_arguments,
    reason = "every argument is a distinct credential source or policy input; bundling them into a struct would hide which ones a given call actually uses"
)]
pub async fn resolve_ws_with_sessions(
    store: &TicketStore,
    session_id: SessionId,
    ticket: Option<&str>,
    headers: &HeaderMap,
    query_token: Option<&str>,
    expected_token: &str,
    auth_required: bool,
    db: Option<&crate::persistence::Db>,
    now_ms: i64,
) -> Result<Principal, AuthError> {
    if let Some(ticket) = ticket {
        return if store.redeem(ticket, session_id) {
            Ok(Principal::LocalUser)
        } else {
            Err(AuthError::Unauthorized)
        };
    }
    resolve_with_sessions(
        headers,
        query_token,
        expected_token,
        auth_required,
        db,
        now_ms,
    )
    .await
}

/// Which relying-party IDs this daemon will run a `WebAuthn` ceremony for,
/// derived once at startup (docs/06-security.md#an-rp-id-is-a-domain-never-an-ip).
///
/// **Derived, never configured separately.** `localhost` is always present;
/// everything else comes from `config.toml`'s `allowed_hosts`, which the
/// user already had to set for remote access to work at all
/// (docs/07-remote-access.md#passkeys-are-per-hostname). A second setting
/// would be a second source of truth for the same fact.
#[derive(Debug, Clone)]
pub struct RpPolicy {
    rp_ids: Vec<String>,
}

impl RpPolicy {
    /// `allowed_hosts` is `config.toml`'s list verbatim; entries that cannot
    /// be relying-party IDs are dropped here rather than failing later
    /// inside a ceremony the user has already started.
    pub fn new(allowed_hosts: &[String]) -> Self {
        let mut rp_ids = vec!["localhost".to_string()];
        for host in allowed_hosts {
            let candidate = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
            if is_valid_rp_id(&candidate) && !rp_ids.contains(&candidate) {
                rp_ids.push(candidate);
            }
        }
        Self { rp_ids }
    }

    /// The RP ID for a request's `Host` header, or `None` if this origin can
    /// never do `WebAuthn` -- which is what `/auth/status` reports as
    /// `passkey_supported: false`, and why the UI shows the token path
    /// instead of a button that cannot work.
    pub fn rp_id_for_host(&self, host: &str) -> Option<&str> {
        let bare = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
        self.rp_ids
            .iter()
            .find(|id| **id == bare)
            .map(String::as_str)
    }

    /// Every RP ID a ceremony may be built for. `main.rs` uses this to
    /// construct one `Webauthn` instance per entry at startup.
    pub fn rp_ids(&self) -> &[String] {
        &self.rp_ids
    }
}

/// An IP literal is not a registrable domain, and `WebAuthn` rejects one as
/// a relying-party ID -- which is the whole reason the startup URL prints
/// `localhost` (`main.rs::url_host`). Checked by parsing rather than by
/// pattern-matching digits: `::1`, `2001:db8::1` and `127.0.0.1` all have to
/// fail, and only one of them looks like an IP to a regex.
fn is_valid_rp_id(host: &str) -> bool {
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    // A bracketed v6 literal never parses as an IpAddr with the brackets on,
    // so reject the syntax outright rather than letting it through as a
    // "domain" named "[::1]".
    if host.starts_with('[') || host.contains(':') {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// How long an unredeemed ticket stays valid (docs/06-security.md: "30-second
/// token"). Generous enough for a slow mobile connect, short enough that a
/// leaked ticket (proxy log, browser history -- the exact exposure this
/// replaces) is worthless within the minute.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// 128 bits (docs/06-security.md's own floor for the credential this
/// stands in for).
const TICKET_BYTES: usize = 16;

#[derive(Debug)]
struct Ticket {
    session_id: SessionId,
    expires_at: Instant,
}

/// In-memory, single-use tickets for the WS upgrade. Never persisted --
/// restarting the daemon invalidates every outstanding ticket, which is
/// correct: nothing durable should ever depend on a 30-second credential.
#[derive(Debug)]
pub struct TicketStore {
    tickets: parking_lot::Mutex<HashMap<String, Ticket>>,
}

impl Default for TicketStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TicketStore {
    /// An empty store.
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

/// How long a `WebAuthn` challenge stays valid
/// (docs/17-passkey-login.md#edge-cases). Longer than a WS ticket's 30s
/// because a user has to physically reach for a key or a fingerprint in the
/// middle of the ceremony; short enough that a challenge is worthless by the
/// time it could be replayed.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(60);

/// A cap on concurrent in-flight ceremonies. Not a security control -- a
/// challenge is single-use and expires in a minute -- but a bound, in the
/// same spirit as `max_sessions`, so nothing can grow this map without
/// limit by starting ceremonies it never finishes.
const MAX_CHALLENGES: usize = 64;

struct Challenge<T> {
    state: T,
    /// The RP the ceremony was started for. Carried so that finishing it
    /// against a *different* origin is impossible even if everything else
    /// lines up.
    rp_id: String,
    expires_at: Instant,
}

/// In-memory, single-use `WebAuthn` ceremony state, keyed by an opaque id the
/// client echoes back. Generic over the state type so this module owns the
/// lifetime rules and `webauthn-rs` owns the cryptography -- and so these
/// tests need no authenticator.
///
/// Never persisted, for exactly the reason [`TicketStore`] is not: nothing
/// durable should depend on a 60-second credential, and a restart
/// mid-enrollment just means clicking the button again
/// (docs/05-persistence.md#auth-tables).
#[derive(Debug)]
pub struct ChallengeStore<T> {
    challenges: parking_lot::Mutex<HashMap<String, Challenge<T>>>,
}

impl<T> std::fmt::Debug for Challenge<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ceremony state is deliberately not printed: it is not a
        // credential, but it has no business in a log line either.
        f.debug_struct("Challenge")
            .field("rp_id", &self.rp_id)
            .finish_non_exhaustive()
    }
}

impl<T> Default for ChallengeStore<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ChallengeStore<T> {
    /// An empty store.
    pub fn new() -> Self {
        Self {
            challenges: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Stores `state` for `rp_id` and returns the id the client echoes back.
    /// Sweeps expired entries first -- the only cleanup this needs, given a
    /// 60-second TTL. Refuses once [`MAX_CHALLENGES`] live entries remain
    /// *after* that sweep.
    pub fn issue(&self, rp_id: &str, state: T) -> Result<String, AuthError> {
        let mut bytes = [0u8; TICKET_BYTES];
        getrandom::getrandom(&mut bytes).map_err(|_| AuthError::Unauthorized)?;
        let id = hex_encode(&bytes);

        let mut challenges = self.challenges.lock();
        let now = Instant::now();
        challenges.retain(|_, c| c.expires_at > now);
        if challenges.len() >= MAX_CHALLENGES {
            return Err(AuthError::Unauthorized);
        }
        challenges.insert(
            id.clone(),
            Challenge {
                state,
                rp_id: rp_id.to_string(),
                expires_at: now + CHALLENGE_TTL,
            },
        );
        Ok(id)
    }

    /// Redeems `id` for `rp_id`. Single-use with the same rule
    /// [`TicketStore::redeem`] uses and for the same reason: an entry found
    /// in the map is removed **regardless** of whether the RP matches or it
    /// has expired, so a wrong-origin attempt cannot be retried against the
    /// same challenge.
    pub fn redeem(&self, id: &str, rp_id: &str) -> Option<T> {
        let mut challenges = self.challenges.lock();
        let challenge = challenges.remove(id)?;
        (challenge.rp_id == rp_id && challenge.expires_at > Instant::now())
            .then_some(challenge.state)
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
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();

        assert!(!store.redeem(&ticket, session));
    }

    #[test]
    fn rp_policy_always_offers_localhost() {
        let policy = RpPolicy::new(&[]);
        assert_eq!(policy.rp_id_for_host("localhost:7337"), Some("localhost"));
        assert_eq!(policy.rp_ids(), ["localhost"]);
    }

    #[test]
    fn rp_policy_never_offers_an_ip_literal() {
        // The reason main.rs prints `localhost`: WebAuthn refuses an IP as
        // a relying-party ID, so this must report "unsupported" rather than
        // letting the UI start a ceremony that cannot succeed.
        let policy = RpPolicy::new(&[
            "127.0.0.1".to_string(),
            "192.168.1.10".to_string(),
            "::1".to_string(),
            "[::1]".to_string(),
        ]);
        assert_eq!(policy.rp_ids(), ["localhost"]);
        assert_eq!(policy.rp_id_for_host("127.0.0.1:7337"), None);
        assert_eq!(policy.rp_id_for_host("192.168.1.10:7337"), None);
    }

    #[test]
    fn rp_policy_derives_hostnames_from_allowed_hosts() {
        let policy = RpPolicy::new(&["desktop.tail1234.ts.net".to_string()]);
        assert_eq!(
            policy.rp_id_for_host("desktop.tail1234.ts.net"),
            Some("desktop.tail1234.ts.net")
        );
        // Port-stripped and case-insensitive: a Host header carries both.
        assert_eq!(
            policy.rp_id_for_host("Desktop.Tail1234.TS.NET:443"),
            Some("desktop.tail1234.ts.net")
        );
        // An origin nobody configured gets nothing, even though it is a
        // perfectly valid domain.
        assert_eq!(policy.rp_id_for_host("evil.example"), None);
    }

    #[test]
    fn rp_policy_does_not_duplicate_localhost() {
        let policy = RpPolicy::new(&["localhost".to_string(), "LOCALHOST:7337".to_string()]);
        assert_eq!(policy.rp_ids(), ["localhost"]);
    }

    #[test]
    fn token_digest_is_stable_and_not_the_token() {
        let digest = token_digest("hunter2");
        assert_eq!(digest.len(), 32);
        assert_eq!(digest, token_digest("hunter2"));
        assert_ne!(digest, token_digest("hunter3"));
        assert_ne!(digest, b"hunter2".to_vec());
    }

    #[test]
    fn a_challenge_redeems_once_for_the_right_rp() {
        let store = ChallengeStore::new();
        let id = store.issue("localhost", "ceremony-state").unwrap();

        assert_eq!(store.redeem(&id, "localhost"), Some("ceremony-state"));
        assert_eq!(store.redeem(&id, "localhost"), None, "single use");
    }

    #[test]
    fn a_challenge_is_consumed_even_when_the_rp_is_wrong() {
        // Same rule as TicketStore: a wrong-origin guess must not be
        // retryable against the same challenge.
        let store = ChallengeStore::new();
        let id = store.issue("localhost", "ceremony-state").unwrap();

        assert_eq!(store.redeem(&id, "evil.example"), None);
        assert_eq!(store.redeem(&id, "localhost"), None, "consumed anyway");
    }

    #[test]
    fn an_unknown_challenge_is_rejected() {
        let store = ChallengeStore::<&str>::new();
        assert_eq!(store.redeem("nonexistent", "localhost"), None);
    }

    #[test]
    fn an_expired_challenge_is_rejected() {
        let store = ChallengeStore::new();
        let id = store.issue("localhost", "ceremony-state").unwrap();
        // Backdate past the TTL rather than sleeping 60s in a test.
        store.challenges.lock().get_mut(&id).unwrap().expires_at =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();

        assert_eq!(store.redeem(&id, "localhost"), None);
    }

    #[test]
    fn challenges_are_capped_and_the_cap_frees_up_as_they_expire() {
        let store = ChallengeStore::new();
        let ids: Vec<String> = (0..MAX_CHALLENGES)
            .map(|i| store.issue("localhost", i).expect("under the cap"))
            .collect();
        assert!(
            store.issue("localhost", 999).is_err(),
            "an unbounded map of unfinished ceremonies is the thing being prevented"
        );

        // Expiring one makes room again -- the cap is a bound, not a latch.
        store.challenges.lock().get_mut(&ids[0]).unwrap().expires_at =
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        store
            .issue("localhost", 999)
            .expect("the cap is a bound, not a latch");
    }

    #[tokio::test]
    async fn the_master_token_still_wins_with_no_database_at_all() {
        // The recovery path (docs/06-security.md): a daemon with no db
        // handle must still authenticate its owner.
        let h = headers(&[(header::AUTHORIZATION, "Bearer secret")]);
        assert_eq!(
            resolve_with_sessions(&h, None, "secret", true, None, 0).await,
            Ok(Principal::LocalUser)
        );
    }

    #[tokio::test]
    async fn a_non_master_token_without_a_database_is_unauthorized() {
        let h = headers(&[(header::AUTHORIZATION, "Bearer nope")]);
        assert_eq!(
            resolve_with_sessions(&h, None, "secret", true, None, 0).await,
            Err(AuthError::Unauthorized)
        );
    }

    #[tokio::test]
    async fn disabled_auth_short_circuits_before_any_lookup() {
        assert_eq!(
            resolve_with_sessions(&HeaderMap::new(), None, "secret", false, None, 0).await,
            Ok(Principal::LocalUser)
        );
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
