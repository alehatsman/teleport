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

use axum::http::{header, HeaderMap};

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

fn bearer_from_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
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

        Self { allowed_origins, allowed_hosts }
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
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).ok_or(AuthError::BadOrigin)?;
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
        assert_eq!(resolve(&h, None, "secret", true), Err(AuthError::Unauthorized));
    }

    #[test]
    fn header_bearer_token_is_accepted() {
        let h = headers(&[(header::AUTHORIZATION, "Bearer secret")]);
        assert_eq!(resolve(&h, None, "secret", true), Ok(Principal::LocalUser));
    }

    #[test]
    fn query_token_is_accepted_when_header_is_absent() {
        let h = HeaderMap::new();
        assert_eq!(resolve(&h, Some("secret"), "secret", true), Ok(Principal::LocalUser));
    }

    #[test]
    fn header_takes_precedence_over_a_mismatched_query_token() {
        let h = headers(&[(header::AUTHORIZATION, "Bearer secret")]);
        assert_eq!(resolve(&h, Some("wrong"), "secret", true), Ok(Principal::LocalUser));
    }

    #[test]
    fn wrong_token_is_unauthorized() {
        let h = headers(&[(header::AUTHORIZATION, "Bearer nope")]);
        assert_eq!(resolve(&h, None, "secret", true), Err(AuthError::Unauthorized));
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
        let h = headers(&[(header::HOST, "127.0.0.1:7337"), (header::ORIGIN, "http://127.0.0.1:7337")]);
        assert_eq!(policy.check(&h), Ok(()));
    }

    #[test]
    fn unknown_origin_is_rejected() {
        let policy = OriginPolicy::new(7337, false, &[], &[]);
        let h = headers(&[(header::HOST, "127.0.0.1:7337"), (header::ORIGIN, "https://evil.example")]);
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
        let h = headers(&[(header::HOST, "127.0.0.1:7337"), (header::ORIGIN, "http://localhost:5173")]);
        assert_eq!(debug.check(&h), Ok(()));
        assert_eq!(release.check(&h), Err(AuthError::BadOrigin));
    }
}
