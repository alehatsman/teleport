//! `auth_routes.rs` -- the `/api/v1/auth/*` surface: passkey enrollment,
//! passkey login, and management of both
//! (docs/17-passkey-login.md#api-surface, docs/04-api-protocol.md#apiv1auth).
//!
//! **Where the trust actually sits.** `webauthn-rs` verifies the
//! cryptography; this module's job is the three policy decisions around it,
//! and those are the ones worth reviewing:
//!
//! 1. **Enrollment requires an existing credential.** Every `register/*`
//!    handler takes a [`Principal`], so on a fresh daemon the only way to
//!    enroll is the `0600` token file. That is what keeps another OS user on
//!    the host from enrolling their own key
//!    (docs/06-security.md#loopback-is-not-a-user-boundary).
//! 2. **Every ceremony is pinned to one RP ID**, taken from the request's
//!    `Host` and never from the body. A challenge issued for `localhost`
//!    cannot be finished against a tailnet hostname, and a credential stored
//!    for one RP is invisible to the other's queries.
//! 3. **Failures are indistinguishable.** Unknown credential, bad signature,
//!    expired challenge and wrong RP all return `unauthorized`. The daemon
//!    never tells a caller which.
//!
//! The two `login/*` routes and `status` are unauthenticated by necessity --
//! a client has no credential before it logs in -- but all three are still
//! Origin-checked, unlike `/health`
//! (docs/04-api-protocol.md#apiv1auth).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::warn;
use url::Url;
use uuid::Uuid;
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Webauthn, WebauthnBuilder,
};

use crate::api::{check_origin, ApiError, AppState};
use crate::auth::{hex_encode, ChallengeStore, Principal, RpPolicy};
use crate::auth_store::{AuthSessionRow, PasskeyRow};
use crate::now_ms;

/// A login session lasts 30 days from issue, with no sliding renewal
/// (docs/17-passkey-login.md#session-lifetime). Re-authenticating a phone
/// that has sat unopened for a month is one fingerprint, not a burden;
/// sliding expiry would make every request a database write.
const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// 128 bits, the same floor `auth.rs` uses for every other credential.
const SESSION_TOKEN_BYTES: usize = 32;

/// A bound on enrolled credentials per RP, in the spirit of `max_sessions`
/// rather than as a security control -- nothing should be able to grow this
/// table without limit (docs/17-passkey-login.md#edge-cases).
const MAX_PASSKEYS_PER_RP: usize = 20;

/// Everything `WebAuthn`-specific `AppState` holds, built once at startup.
///
/// One [`Webauthn`] per RP ID, because the crate ties an instance to exactly
/// one relying party -- which is also the cleanest possible encoding of the
/// rule that these origins do not share credentials.
pub struct PasskeyState {
    /// Which RP IDs this daemon will run a ceremony for.
    pub policy: RpPolicy,
    /// One configured instance per entry in `policy.rp_ids()`.
    instances: HashMap<String, Webauthn>,
    /// In-flight enrollment ceremonies.
    registrations: ChallengeStore<PasskeyRegistration>,
    /// In-flight login ceremonies.
    authentications: ChallengeStore<PasskeyAuthentication>,
}

impl std::fmt::Debug for PasskeyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasskeyState")
            .field("rp_ids", &self.policy.rp_ids())
            .finish_non_exhaustive()
    }
}

impl PasskeyState {
    /// Builds one instance per usable RP ID. `bound_port` is the port the
    /// listener actually got, never the 7337 default
    /// (docs/08-packaging.md#port-discovery--do-not-hardcode-7337).
    ///
    /// An RP ID whose origin cannot be constructed is **skipped with a
    /// warning, not a startup failure**: a malformed `allowed_origins` entry
    /// should cost the user passkeys on that one hostname, not a daemon that
    /// refuses to boot.
    pub fn new(bound_port: u16, allowed_origins: &[String], allowed_hosts: &[String]) -> Self {
        let policy = RpPolicy::new(allowed_hosts);
        let mut instances = HashMap::new();

        for rp_id in policy.rp_ids() {
            let origins = origins_for(rp_id, bound_port, allowed_origins);
            let Some((first, rest)) = origins.split_first() else {
                warn!(
                    rp_id,
                    "no usable origin for this RP ID; passkeys disabled there"
                );
                continue;
            };
            let mut builder = match WebauthnBuilder::new(rp_id, first) {
                Ok(builder) => builder,
                Err(e) => {
                    warn!(rp_id, error = %e, "building the WebAuthn instance failed; passkeys disabled there");
                    continue;
                }
            };
            for origin in rest {
                builder = builder.append_allowed_origin(origin);
            }
            match builder.rp_name("teleport").build() {
                Ok(instance) => {
                    instances.insert(rp_id.clone(), instance);
                }
                Err(e) => {
                    warn!(rp_id, error = %e, "building the WebAuthn instance failed; passkeys disabled there");
                }
            }
        }

        Self {
            policy,
            instances,
            registrations: ChallengeStore::new(),
            authentications: ChallengeStore::new(),
        }
    }

    /// The RP ID and instance for a request's `Host`, or `None` when this
    /// origin cannot do `WebAuthn` at all.
    fn resolve(&self, headers: &HeaderMap) -> Option<(&str, &Webauthn)> {
        let host = headers.get(axum::http::header::HOST)?.to_str().ok()?;
        let rp_id = self.policy.rp_id_for_host(host)?;
        let instance = self.instances.get(rp_id)?;
        Some((rp_id, instance))
    }
}

/// The origins a given RP ID will accept. A configured `allowed_origins`
/// entry naming this host wins outright -- the user has said exactly what
/// their tunnel terminates on, and guessing over the top of that is how you
/// get a ceremony that fails with an origin mismatch nobody can debug.
///
/// Otherwise: `localhost` gets the actual bound port over `http` (a
/// trustworthy origin without TLS, which is the whole reason the startup URL
/// says `localhost`), and any other hostname gets `https`, since a remote
/// transport that is not HTTPS is not a secure context and could not run a
/// ceremony anyway (docs/07-remote-access.md).
fn origins_for(rp_id: &str, bound_port: u16, allowed_origins: &[String]) -> Vec<Url> {
    let configured: Vec<Url> = allowed_origins
        .iter()
        .filter_map(|o| Url::parse(o).ok())
        .filter(|u| u.host_str() == Some(rp_id))
        .collect();
    if !configured.is_empty() {
        return configured;
    }
    let guess = if rp_id == "localhost" {
        format!("http://localhost:{bound_port}")
    } else {
        format!("https://{rp_id}")
    };
    Url::parse(&guess).into_iter().collect()
}

// ---------------------------------------------------------------- wire types

/// `GET /auth/status`. The only unauthenticated informational route here:
/// a freshly-loaded SPA has to pick a screen before it holds any credential
/// (docs/09-frontend.md#credential-precedence-and-the-login-screen).
#[derive(Debug, Serialize)]
pub struct AuthStatus {
    /// Whether this origin has a usable RP ID at all.
    passkey_supported: bool,
    /// The RP ID in force, or `None` when unsupported.
    rp_id: Option<String>,
    /// Whether any credential is enrolled **for this RP ID**. A passkey at
    /// `localhost` does not make a tailnet hostname enrolled.
    enrolled: bool,
    /// The `localhost` URL to switch to, sent only when this origin cannot
    /// do passkeys, so the UI can offer the fix rather than just the refusal.
    token_url_hint: Option<String>,
}

/// `POST /auth/passkey/register/start`.
#[derive(Debug, Deserialize)]
pub struct RegisterStartRequest {
    /// Shown by the authenticator and the password manager. Used only on the
    /// very first enrollment; later ones reuse the stored owner.
    user_name: Option<String>,
}

/// A started ceremony: the browser passes `options` straight to
/// `navigator.credentials`, and echoes `challenge_id` back on finish.
#[derive(Debug, Serialize)]
pub struct CeremonyStart<T> {
    challenge_id: String,
    options: T,
}

/// `POST /auth/passkey/register/finish`.
#[derive(Debug, Deserialize)]
pub struct RegisterFinishRequest {
    challenge_id: String,
    /// What `navigator.credentials.create()` returned, verbatim.
    credential: RegisterPublicKeyCredential,
    /// Optional display label; defaults to something generic rather than
    /// failing, since this is cosmetic.
    label: Option<String>,
}

/// `POST /auth/passkey/login/finish`.
#[derive(Debug, Deserialize)]
pub struct LoginFinishRequest {
    challenge_id: String,
    /// What `navigator.credentials.get()` returned, verbatim.
    credential: PublicKeyCredential,
    /// Client-supplied device name for the signed-in-devices list. Not
    /// trusted for anything.
    label: Option<String>,
}

/// The session credential. Presented thereafter as
/// `Authorization: Bearer <token>`, exactly like the master token.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    token: String,
    expires_at_ms: i64,
    session_id: String,
}

/// One enrolled credential, as the settings screen sees it. The raw
/// `credential_id` is deliberately absent -- nothing outside the daemon
/// needs it.
#[derive(Debug, Serialize)]
pub struct PasskeySummary {
    id: String,
    rp_id: String,
    label: String,
    created_at_ms: i64,
    last_used_ms: Option<i64>,
}

impl From<PasskeyRow> for PasskeySummary {
    fn from(row: PasskeyRow) -> Self {
        Self {
            id: row.id,
            rp_id: row.rp_id,
            label: row.label,
            created_at_ms: row.created_at_ms,
            last_used_ms: row.last_used_ms,
        }
    }
}

/// One signed-in device.
#[derive(Debug, Serialize)]
pub struct SessionSummary {
    id: String,
    passkey_id: String,
    label: String,
    created_at_ms: i64,
    expires_at_ms: i64,
    last_seen_ms: i64,
    /// Whether this is the session making the request -- so the UI can say
    /// "this device" and warn before revoking it.
    current: bool,
}

/// `PATCH /auth/passkeys/{id}`.
#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    label: String,
}

/// A bare `{}` for routes whose only meaningful answer is "it worked".
#[derive(Debug, Serialize)]
pub struct Ok200 {}

// ----------------------------------------------------------------- handlers

/// Passkey routes need a database; a daemon built without one (test
/// fixtures, docs/11-mvp-plan.md#m7) has no auth surface rather than a
/// half-working one.
fn db(state: &AppState) -> Result<&crate::persistence::Db, ApiError> {
    state
        .db
        .as_ref()
        .ok_or_else(|| ApiError::Internal("this daemon has no metadata store".to_string()))
}

/// Passkeys turned off in `config.toml`, or auth turned off entirely --
/// either way `/auth/*` must not half-work
/// (docs/17-passkey-login.md#config).
fn passkeys_enabled(state: &AppState) -> bool {
    state.config.auth_token && state.config.auth_passkey
}

fn enabled_or_404(state: &AppState) -> Result<(), ApiError> {
    if passkeys_enabled(state) {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

/// `GET /api/v1/auth/status` -- unauthenticated, Origin-checked.
pub async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AuthStatus>, ApiError> {
    check_origin(&state, &headers)?;

    let unsupported = |state: &AppState| AuthStatus {
        passkey_supported: false,
        rp_id: None,
        enrolled: false,
        token_url_hint: Some(format!("http://localhost:{}", state.bound_port)),
    };

    if !passkeys_enabled(&state) {
        return Ok(Json(unsupported(&state)));
    }
    let Some((rp_id, _)) = state.passkeys.resolve(&headers) else {
        return Ok(Json(unsupported(&state)));
    };

    // Scoped to this RP: enrolment elsewhere is not enrolment here.
    let enrolled = match db(&state)?.list_passkeys(Some(rp_id.to_string())).await {
        Ok(rows) => !rows.is_empty(),
        Err(e) => return Err(ApiError::Internal(e.to_string())),
    };

    Ok(Json(AuthStatus {
        passkey_supported: true,
        rp_id: Some(rp_id.to_string()),
        enrolled,
        token_url_hint: None,
    }))
}

/// `POST /api/v1/auth/passkey/register/start` -- **authenticated**. The
/// [`Principal`] argument is the bootstrap gate described in this module's
/// header; removing it would let any local process enroll a key.
pub async fn register_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    _principal: Principal,
    body: Option<Json<RegisterStartRequest>>,
) -> Result<Json<CeremonyStart<webauthn_rs::prelude::CreationChallengeResponse>>, ApiError> {
    check_origin(&state, &headers)?;
    enabled_or_404(&state)?;
    let db = db(&state)?;
    let (rp_id, webauthn) = state
        .passkeys
        .resolve(&headers)
        .ok_or_else(|| ApiError::BadRequest(unsupported_origin_message(&state)))?;

    let existing = db
        .list_passkeys(Some(rp_id.to_string()))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if existing.len() >= MAX_PASSKEYS_PER_RP {
        return Err(ApiError::BadRequest(format!(
            "{MAX_PASSKEYS_PER_RP} passkeys already enrolled for {rp_id}; remove one first"
        )));
    }

    // The owner is created on the first enrollment and reused forever after,
    // so every credential belongs to one WebAuthn identity rather than
    // several that authenticators would treat as unrelated accounts.
    let requested_name = body
        .and_then(|Json(b)| b.user_name)
        .unwrap_or_else(|| "teleport".to_string());
    let candidate_handle = Uuid::new_v4();
    let owner = db
        .ensure_owner(
            candidate_handle.as_bytes().to_vec(),
            requested_name,
            now_ms(),
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let user_handle = uuid_from_row(&owner.user_handle)?;

    // Excluding what is already enrolled turns "add a passkey twice" into a
    // clear message from the authenticator instead of a UNIQUE violation
    // after the user has already touched their key.
    let exclude = existing
        .iter()
        .map(|row| row.credential_id.clone().into())
        .collect();

    let (options, registration) = webauthn
        .start_passkey_registration(
            user_handle,
            &owner.user_name,
            &owner.user_name,
            Some(exclude),
        )
        .map_err(|e| ApiError::Internal(format!("starting registration: {e}")))?;

    let challenge_id = state
        .passkeys
        .registrations
        .issue(rp_id, registration)
        .map_err(ApiError::from)?;

    Ok(Json(CeremonyStart {
        challenge_id,
        options,
    }))
}

/// `POST /api/v1/auth/passkey/register/finish` -- **authenticated**.
pub async fn register_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    _principal: Principal,
    Json(body): Json<RegisterFinishRequest>,
) -> Result<Json<PasskeySummary>, ApiError> {
    check_origin(&state, &headers)?;
    enabled_or_404(&state)?;
    let db = db(&state)?;
    let (rp_id, webauthn) = state
        .passkeys
        .resolve(&headers)
        .ok_or_else(|| ApiError::BadRequest(unsupported_origin_message(&state)))?;

    // Redeeming against *this* rp_id is what makes a challenge issued for
    // one origin unusable at another, regardless of anything in the body.
    let registration = state
        .passkeys
        .registrations
        .redeem(&body.challenge_id, rp_id)
        .ok_or(ApiError::Auth(crate::auth::AuthError::Unauthorized))?;

    let passkey = webauthn
        .finish_passkey_registration(&body.credential, &registration)
        .map_err(|e| {
            // Logged, not returned: the caller gets `unauthorized` and no
            // hint about which check failed.
            warn!(error = %e, "passkey registration failed verification");
            ApiError::Auth(crate::auth::AuthError::Unauthorized)
        })?;

    let row = PasskeyRow {
        id: ulid::Ulid::new().to_string(),
        credential_id: passkey.cred_id().as_ref().to_vec(),
        rp_id: rp_id.to_string(),
        credential: serde_json::to_string(&passkey)
            .map_err(|e| ApiError::Internal(format!("serializing the credential: {e}")))?,
        label: body
            .label
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| "Passkey".to_string()),
        created_at_ms: now_ms(),
        last_used_ms: None,
    };
    db.insert_passkey(row.clone())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(row.into()))
}

/// `POST /api/v1/auth/passkey/login/start` -- unauthenticated by necessity,
/// Origin-checked like everything else here.
pub async fn login_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<CeremonyStart<webauthn_rs::prelude::RequestChallengeResponse>>, ApiError> {
    check_origin(&state, &headers)?;
    enabled_or_404(&state)?;
    let db = db(&state)?;
    let (rp_id, webauthn) = state
        .passkeys
        .resolve(&headers)
        .ok_or_else(|| ApiError::BadRequest(unsupported_origin_message(&state)))?;

    let rows = db
        .list_passkeys(Some(rp_id.to_string()))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if rows.is_empty() {
        // Nothing enrolled here. `unauthorized` rather than a distinct code:
        // `/auth/status` is where a client learns this, and it should have
        // asked before offering the button.
        return Err(ApiError::Auth(crate::auth::AuthError::Unauthorized));
    }

    let passkeys: Vec<Passkey> = rows
        .iter()
        .filter_map(|row| match serde_json::from_str(&row.credential) {
            Ok(passkey) => Some(passkey),
            Err(e) => {
                // One unreadable row must not lock the user out of the
                // others; skip it loudly.
                warn!(passkey_id = row.id, error = %e, "stored credential is unreadable; skipping it");
                None
            }
        })
        .collect();
    if passkeys.is_empty() {
        return Err(ApiError::Internal(
            "every stored credential for this origin is unreadable".to_string(),
        ));
    }

    let (options, authentication) = webauthn
        .start_passkey_authentication(&passkeys)
        .map_err(|e| ApiError::Internal(format!("starting authentication: {e}")))?;

    let challenge_id = state
        .passkeys
        .authentications
        .issue(rp_id, authentication)
        .map_err(ApiError::from)?;

    Ok(Json(CeremonyStart {
        challenge_id,
        options,
    }))
}

/// `POST /api/v1/auth/passkey/login/finish` -- unauthenticated by necessity.
/// Mints the session credential on success.
pub async fn login_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginFinishRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    check_origin(&state, &headers)?;
    enabled_or_404(&state)?;
    let db = db(&state)?;
    let (rp_id, webauthn) = state
        .passkeys
        .resolve(&headers)
        .ok_or_else(|| ApiError::BadRequest(unsupported_origin_message(&state)))?;

    let authentication = state
        .passkeys
        .authentications
        .redeem(&body.challenge_id, rp_id)
        .ok_or(ApiError::Auth(crate::auth::AuthError::Unauthorized))?;

    // Counter regression (a cloned authenticator) is one of the failures
    // webauthn-rs reports here, and it lands in the same bucket as every
    // other: unauthorized, logged, undifferentiated on the wire.
    let result = webauthn
        .finish_passkey_authentication(&body.credential, &authentication)
        .map_err(|e| {
            warn!(error = %e, "passkey assertion failed verification");
            ApiError::Auth(crate::auth::AuthError::Unauthorized)
        })?;

    let row = db
        .get_passkey_by_credential_id(result.cred_id().as_ref().to_vec(), rp_id.to_string())
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::Auth(crate::auth::AuthError::Unauthorized))?;

    // Write the advanced signature counter back, so the next assertion has
    // something to compare against. Fire-and-forget: the user has already
    // authenticated and must not wait on disk for a clone-detection signal.
    if let Ok(mut passkey) = serde_json::from_str::<Passkey>(&row.credential) {
        if passkey.update_credential(&result).is_some() {
            if let Ok(updated) = serde_json::to_string(&passkey) {
                db.note_passkey_used(&row.id, updated, now_ms());
            }
        }
    }

    let (token, session) = mint_session(&row.id, body.label)?;
    db.insert_auth_session(session.clone(), crate::auth::token_digest(&token))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(LoginResponse {
        token,
        expires_at_ms: session.expires_at_ms,
        session_id: session.id,
    }))
}

/// `GET /api/v1/auth/passkeys` -- every enrolled credential, across all RP
/// IDs, so the settings screen can show "you have one here and one on the
/// tailnet hostname."
pub async fn list_passkeys(
    State(state): State<Arc<AppState>>,
    _principal: Principal,
) -> Result<Json<Vec<PasskeySummary>>, ApiError> {
    enabled_or_404(&state)?;
    let rows = db(&state)?
        .list_passkeys(None)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(rows.into_iter().map(PasskeySummary::from).collect()))
}

/// `PATCH /api/v1/auth/passkeys/{id}` -- cosmetic rename.
pub async fn rename_passkey(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    _principal: Principal,
    Json(body): Json<RenameRequest>,
) -> Result<Json<Ok200>, ApiError> {
    check_origin(&state, &headers)?;
    enabled_or_404(&state)?;
    let label = body.label.trim().to_string();
    if label.is_empty() {
        return Err(ApiError::BadRequest("label must not be empty".to_string()));
    }
    let renamed = db(&state)?
        .rename_passkey(id, label)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if renamed {
        Ok(Json(Ok200 {}))
    } else {
        Err(ApiError::NotFound)
    }
}

/// `DELETE /api/v1/auth/passkeys/{id}` -- unenroll, which cascades to every
/// session this credential minted (docs/17-passkey-login.md#data-model).
///
/// Deleting the *last* passkey is allowed on purpose: the master token is
/// still there, so there is no lockout to protect the user from.
pub async fn delete_passkey(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    _principal: Principal,
) -> Result<Json<Ok200>, ApiError> {
    check_origin(&state, &headers)?;
    enabled_or_404(&state)?;
    let deleted = db(&state)?
        .delete_passkey(id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if deleted {
        Ok(Json(Ok200 {}))
    } else {
        Err(ApiError::NotFound)
    }
}

/// `GET /api/v1/auth/sessions` -- the signed-in-devices list.
pub async fn list_sessions(
    State(state): State<Arc<AppState>>,
    principal: Principal,
) -> Result<Json<Vec<SessionSummary>>, ApiError> {
    enabled_or_404(&state)?;
    let current = current_session_id(&principal);
    let rows = db(&state)?
        .list_auth_sessions()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        rows.into_iter()
            .map(|row| SessionSummary {
                current: current == Some(row.id.as_str()),
                id: row.id,
                passkey_id: row.passkey_id,
                label: row.label,
                created_at_ms: row.created_at_ms,
                expires_at_ms: row.expires_at_ms,
                last_seen_ms: row.last_seen_ms,
            })
            .collect(),
    ))
}

/// `DELETE /api/v1/auth/sessions/{id}` -- revoke one device, including this
/// one.
pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    _principal: Principal,
) -> Result<Json<Ok200>, ApiError> {
    check_origin(&state, &headers)?;
    enabled_or_404(&state)?;
    let deleted = db(&state)?
        .delete_auth_session(id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if deleted {
        Ok(Json(Ok200 {}))
    } else {
        Err(ApiError::NotFound)
    }
}

/// `POST /api/v1/auth/logout` -- revokes the caller's own session.
///
/// A caller holding the *master token* has no session to revoke. That is a
/// no-op success, not an error: "log out" meaning "stop being the owner of
/// this machine" is not something this route can or should offer.
pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    principal: Principal,
) -> Result<Json<Ok200>, ApiError> {
    check_origin(&state, &headers)?;
    enabled_or_404(&state)?;
    if let Some(id) = current_session_id(&principal) {
        db(&state)?
            .delete_auth_session(id.to_string())
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    Ok(Json(Ok200 {}))
}

// ------------------------------------------------------------------ helpers

/// The session id behind a passkey login, or `None` for the master token.
fn current_session_id(principal: &Principal) -> Option<&str> {
    match principal {
        Principal::DeviceToken { token_id } => Some(token_id),
        Principal::LocalUser | Principal::Account { .. } => None,
    }
}

fn unsupported_origin_message(state: &AppState) -> String {
    format!(
        "passkeys are unavailable on this address (a WebAuthn relying-party ID cannot be an IP); \
         open http://localhost:{} instead",
        state.bound_port
    )
}

/// The stored handle is a `Uuid`'s bytes -- `webauthn-rs` types the user
/// handle as one, so this is a shape check on our own column rather than on
/// anything a caller sent.
fn uuid_from_row(bytes: &[u8]) -> Result<Uuid, ApiError> {
    Uuid::from_slice(bytes)
        .map_err(|e| ApiError::Internal(format!("stored owner handle is not a UUID: {e}")))
}

/// Generates the session token and its row. The token is returned to the
/// caller exactly once, here; only its digest is ever stored.
fn mint_session(
    passkey_id: &str,
    label: Option<String>,
) -> Result<(String, AuthSessionRow), ApiError> {
    let mut bytes = [0u8; SESSION_TOKEN_BYTES];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| ApiError::Internal(format!("generating a session token: {e}")))?;
    let token = hex_encode(&bytes);

    let now = now_ms();
    let row = AuthSessionRow {
        id: ulid::Ulid::new().to_string(),
        passkey_id: passkey_id.to_string(),
        label: label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "Unknown device".to_string()),
        created_at_ms: now,
        expires_at_ms: now.saturating_add(SESSION_TTL_MS),
        last_seen_ms: now,
    };
    Ok((token, row))
}
