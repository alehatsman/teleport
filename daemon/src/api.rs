//! HTTP surface -- `/api/v1/*` (docs/04-api-protocol.md#http-surface). The
//! WebSocket upgrade route is registered here but implemented in `ws.rs`;
//! everything else -- resource lifecycle -- lives in this module.
//!
//! Every handler takes [`Principal`] as an argument (an extractor, not a
//! header read) except `/health`, which is reachable unauthenticated by
//! design so the desktop shell can probe before it holds a credential
//! (docs/04-api-protocol.md#get-apiv1health).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth::{self, AuthError, OriginPolicy, Principal};
use crate::config::Config;
use crate::device::Device;
use crate::presets::Preset;
use crate::pty::SpawnSpec;
use crate::session::{CreateError, SessionId, SessionManager, SessionState};

/// Shared state for every handler and the WS upgrade. One instance, built
/// once in `main.rs` after the listener is bound (the origin policy needs
/// the actual port -- docs/06-security.md#browser-origin-defense) and held
/// behind an `Arc` for the life of the process.
pub struct AppState {
    pub sessions: SessionManager,
    pub config: Config,
    pub device: Device,
    pub token: String,
    pub presets: Vec<Preset>,
    pub origin_policy: OriginPolicy,
    pub started_at: Instant,
    pub version: &'static str,
}

/// `Principal` as an axum extractor: every handler that needs one declares
/// it as a parameter and gets a `401` for free on failure, rather than
/// reading headers itself (docs/12-identity-and-connectivity.md#the-principal:
/// "handlers never inspect headers"). `/health`'s handler calls
/// [`auth::resolve`] directly instead of using this extractor, because it
/// must not reject -- it picks a response *shape* based on the result.
impl FromRequestParts<Arc<AppState>> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let query_token = query_param(parts.uri.query().unwrap_or(""), "token");
        auth::resolve(&parts.headers, query_token, &state.token, state.config.auth_token)
            .map_err(ApiError::from)
    }
}

/// Finds `key` in a raw (already-percent-decode-free) query string. The only
/// value this is ever used for is the bearer token, which `main.rs` always
/// generates as lowercase hex -- no percent-encoding can ever legitimately
/// appear in it, so a full URL-decode is not needed here.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Uniform HTTP error shape and status mapping for everything this module
/// returns. Wire shape is deliberately minimal -- `{"error": "<code>",
/// "message": "<detail>"}` -- there is no cross-team consumer requiring more.
pub enum ApiError {
    Auth(AuthError),
    NotFound,
    BadRequest(String),
    Create(CreateError),
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        ApiError::Auth(e)
    }
}

impl From<CreateError> for ApiError {
    fn from(e: CreateError) -> Self {
        ApiError::Create(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ApiError::Auth(AuthError::Unauthorized) => {
                (StatusCode::UNAUTHORIZED, "unauthorized", "missing or invalid credential".to_string())
            }
            ApiError::Auth(AuthError::BadOrigin) => {
                (StatusCode::FORBIDDEN, "bad_origin", "Origin or Host rejected".to_string())
            }
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found", "session not found".to_string()),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg),
            // docs/04-api-protocol.md#post-apiv1sessions: 422 for a
            // resolvable-at-validation-time problem, 429 for max_sessions,
            // 500 for a spawn failure past that point. Never 404 -- the
            // collection exists, the request is unprocessable.
            ApiError::Create(CreateError::ExecutableNotFound(cmd)) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "unprocessable", format!("executable not found on PATH: {cmd}"))
            }
            ApiError::Create(CreateError::InvalidCwd(cwd)) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "unprocessable",
                format!("cwd does not exist or is not a directory: {}", cwd.display()),
            ),
            ApiError::Create(CreateError::MaxSessions(n)) => {
                (StatusCode::TOO_MANY_REQUESTS, "max_sessions", format!("max_sessions ({n}) reached"))
            }
            ApiError::Create(CreateError::Spawn(e)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "spawn_failed", e.to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": code, "message": message }))).into_response()
    }
}

/// Applies [`OriginPolicy::check`] -- callers use this only on mutating
/// routes and the WS upgrade (docs/06-security.md#browser-origin-defense);
/// GET routes rely on [`Principal`] alone.
fn check_origin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    state.origin_policy.check(headers).map_err(ApiError::from)
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route("/api/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/api/v1/sessions/{id}/log", get(get_log))
        .route("/api/v1/sessions/{id}/stream", get(crate::ws::upgrade))
        .route("/api/v1/presets", get(list_presets))
        .with_state(state)
}

const API_VERSIONS: &[&str] = &["v1"];
const CAPABILITIES: &[&str] = &["sessions", "presets", "tail_attach"];

/// `GET /api/v1/health` -- the one route reachable without a credential
/// (docs/04-api-protocol.md#get-apiv1health). Calls [`auth::resolve`]
/// directly rather than taking a [`Principal`] extractor: an extractor
/// rejects on failure, and this handler must not reject, only pick a
/// response shape.
async fn health(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: axum::http::Uri) -> impl IntoResponse {
    let query_token = query_param(uri.query().unwrap_or(""), "token");
    let authenticated =
        auth::resolve(&headers, query_token, &state.token, state.config.auth_token).is_ok();

    let mut body = serde_json::json!({
        "status": "ok",
        "version": state.version,
        "api_versions": API_VERSIONS,
        "capabilities": CAPABILITIES,
    });
    if authenticated {
        // The hostname-derived device name is mildly identifying, so it sits
        // behind the principal -- nothing in the unauthenticated shape above
        // may be sensitive (docs/04-api-protocol.md#get-apiv1health).
        body["device_id"] = state.device.device_id.to_string().into();
        body["device_name"] = state.device.device_name.clone().into();
        body["platform"] = state.device.platform.clone().into();
        body["pid"] = std::process::id().into();
        body["uptime_ms"] = (state.started_at.elapsed().as_millis() as u64).into();
        body["sessions_running"] = state
            .sessions
            .list()
            .iter()
            .filter(|s| s.state() == SessionState::Running)
            .count()
            .into();
    }
    Json(body)
}

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    kind: String,
    preset: Option<String>,
    command: Option<String>,
    /// `None` (the field omitted) falls back to the preset's own args;
    /// `Some(vec![])` is an explicit request for no args and is honored as
    /// such. A plain `Vec<String>` with `#[serde(default)]` can't tell those
    /// two apart -- both deserialize to an empty vec (M4 review).
    args: Option<Vec<String>>,
    cwd: String,
    cols: u16,
    rows: u16,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct CreateSessionResponse {
    id: String,
    state: &'static str,
    pid: Option<u32>,
    output_offset: u64,
}

/// `POST /api/v1/sessions` (docs/04-api-protocol.md#post-apiv1sessions).
/// Merges preset defaults under explicit fields, validates `cols`/`rows`
/// (`400` if out of range -- request-shape validation, distinct from the
/// `422`s [`SessionManager::create`] itself raises for an unresolvable
/// executable or `cwd`), then spawns.
async fn create_session(
    State(state): State<Arc<AppState>>,
    _principal: Principal,
    headers: HeaderMap,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    check_origin(&state, &headers)?;

    if !(1..=1000).contains(&req.cols) || !(1..=1000).contains(&req.rows) {
        return Err(ApiError::BadRequest("cols and rows must be in 1..=1000".to_string()));
    }

    let preset = req.preset.as_deref().and_then(|id| state.presets.iter().find(|p| p.id == id));
    let command = req
        .command
        .or_else(|| preset.map(|p| p.resolved_command()))
        .ok_or_else(|| ApiError::BadRequest("command is required unless a preset supplies it".to_string()))?;
    let args = resolve_args(req.args, preset);
    let env: Vec<(String, String)> = req.env.into_iter().collect();
    let cwd = PathBuf::from(req.cwd);
    let (cols, rows, kind, preset_id) = (req.cols, req.rows, req.kind, req.preset);

    // `SessionManager::create` does blocking filesystem work (validating
    // `cwd`, a `$PATH` scan to resolve the executable) and an OS-level
    // fork/exec -- run it on the blocking pool so a slow/contended
    // filesystem or spawn doesn't stall this worker thread's other
    // in-flight requests (M4 review: this used to run inline on the async
    // handler). `SpawnSpec` borrows `command`/`args`/`cwd`/`env`, so it's
    // built inside the closure rather than moved in already constructed.
    // `state` isn't needed again after this, so it moves in whole.
    let session = tokio::task::spawn_blocking(move || {
        let spec = SpawnSpec { program: &command, args: &args, cwd: &cwd, env: &env, cols, rows };
        state.sessions.create(spec, kind, preset_id)
    })
    .await
    .map_err(|e| ApiError::Create(CreateError::Spawn(e.into())))??;

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            id: session.id.to_string(),
            state: session.state().as_str(),
            pid: session.pid(),
            output_offset: session.next_offset(),
        }),
    ))
}

/// `args: None` (the field omitted from the request) falls back to the
/// preset's own args; `Some(vec![])` is an explicit "no args" request and is
/// honored as such -- a plain `Vec<String>` with `#[serde(default)]` can't
/// tell those two apart, both deserialize to an empty vec (M4 review).
fn resolve_args(explicit: Option<Vec<String>>, preset: Option<&Preset>) -> Vec<String> {
    explicit.unwrap_or_else(|| preset.map(|p| p.args.clone()).unwrap_or_default())
}

#[derive(Debug, Serialize)]
struct SessionView {
    id: String,
    kind: String,
    preset: Option<String>,
    command: String,
    args: Vec<String>,
    cwd: String,
    state: &'static str,
    pid: Option<u32>,
    cols: u16,
    rows: u16,
    output_bytes: u64,
    created_at_ms: i64,
    started_at_ms: Option<i64>,
    exited_at_ms: Option<i64>,
    exit_code: Option<i32>,
    lost_reason: Option<&'static str>,
    controller: Option<String>,
    subscribers: usize,
}

impl SessionView {
    fn from(session: &crate::session::Session) -> Self {
        let (cols, rows) = session.size();
        SessionView {
            id: session.id.to_string(),
            kind: session.meta.kind.clone(),
            preset: session.meta.preset.clone(),
            command: session.meta.command.clone(),
            args: session.meta.args.clone(),
            cwd: session.meta.cwd.display().to_string(),
            state: session.state().as_str(),
            pid: session.pid(),
            cols,
            rows,
            output_bytes: session.next_offset(),
            created_at_ms: session.created_at_ms(),
            started_at_ms: session.started_at_ms(),
            exited_at_ms: session.exited_at_ms(),
            exit_code: session.exit_code(),
            lost_reason: session.lost_reason().map(|r| r.as_str()),
            controller: session.controller_name(),
            subscribers: session.subscriber_count(),
        }
    }
}

/// `GET /api/v1/sessions` (docs/04-api-protocol.md#get-apiv1sessions).
/// Sorted newest-first; `env` never appears (it is never stored on
/// [`crate::session::Session`] in the first place --
/// docs/06-security.md#secrets-and-environment).
async fn list_sessions(State(state): State<Arc<AppState>>, _principal: Principal) -> impl IntoResponse {
    let mut sessions = state.sessions.list();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at_ms()));
    let views: Vec<SessionView> = sessions.iter().map(|s| SessionView::from(s)).collect();
    Json(serde_json::json!({ "sessions": views }))
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    _principal: Principal,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let session = find_session(&state, &id)?;
    Ok(Json(SessionView::from(&session)))
}

#[derive(Debug, Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    purge: bool,
}

/// `DELETE /api/v1/sessions/{id}` (docs/04-api-protocol.md#delete-apiv1sessionsid).
/// Termination runs in the background -- `pty.rs`'s bounded policy can take
/// up to ~7s, and this handler returns `202` immediately, same as the spec's
/// "clients watch the WS `exit` frame or poll `GET`."
///
/// `?purge=true` on an already-`exited` session skips the termination
/// machine and deletes outright; on a still-running one it terminates first,
/// waits for `exited`, then deletes the directory and the in-memory entry --
/// directory first, matching the collector's own ordering
/// (docs/05-persistence.md#garbage-collection). There is no SQLite row yet
/// (M7), so "row second" here is [`SessionManager::purge`] removing the
/// `Arc` from the directory.
async fn delete_session(
    State(state): State<Arc<AppState>>,
    _principal: Principal,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<impl IntoResponse, ApiError> {
    check_origin(&state, &headers)?;
    let session = find_session(&state, &id)?;

    if q.purge {
        if session.state() != SessionState::Exited {
            // `terminate()` can block for the whole graceful-shutdown window
            // (up to ~7s, docs/03-pty-layer.md#termination) -- spawn_blocking
            // keeps that off this worker thread, same as the non-purge path
            // below (M4 review: this used to run inline, and purge needs the
            // result before it can safely delete the directory).
            let terminate_session = Arc::clone(&session);
            match tokio::task::spawn_blocking(move || terminate_session.terminate()).await {
                Ok(Err(e)) => tracing::warn!(session_id = %session.id, error = %e, "terminate (purge) failed"),
                Ok(Ok(())) => {}
                Err(e) => tracing::warn!(session_id = %session.id, error = %e, "terminate (purge) task panicked"),
            }
            session.exited().await;
        }
        let log_dir = session.log_path().parent().map(|p| p.to_path_buf()).unwrap_or_else(|| session.log_path());
        // `remove_dir_all` is blocking fs work too.
        if let Err(e) = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(log_dir)).await {
            tracing::warn!(session_id = %session.id, error = %e, "removing session directory task panicked");
        }
        state.sessions.purge(session.id);
        return Ok(StatusCode::NO_CONTENT);
    }

    let terminate_session = Arc::clone(&session);
    tokio::task::spawn_blocking(move || {
        if let Err(e) = terminate_session.terminate() {
            tracing::warn!(session_id = %terminate_session.id, error = %e, "terminate failed");
        }
    });
    Ok(StatusCode::ACCEPTED)
}

/// `GET /api/v1/sessions/{id}/log` (docs/04-api-protocol.md#get-apiv1sessionsidlog).
/// Raw bytes, clamped to what actually exists. Supports `?from=&to=` byte
/// offsets; a `Range: bytes=start-end` header is honored the same way when
/// no query offsets are given. Authorization is identical to live attach --
/// the [`Principal`] extractor, no separate check
/// (docs/06-security.md#terminal-logs-are-sensitive).
#[derive(Debug, Deserialize)]
struct LogQuery {
    from: Option<u64>,
    to: Option<u64>,
}

async fn get_log(
    State(state): State<Arc<AppState>>,
    _principal: Principal,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let session = find_session(&state, &id)?;
    let end = session.next_offset();

    let (from, to) = if q.from.is_some() || q.to.is_some() {
        (q.from.unwrap_or(0), q.to.unwrap_or(end))
    } else if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        parse_byte_range(range, end).unwrap_or((0, end))
    } else {
        (0, end)
    };
    let to = to.min(end);
    let from = from.min(to);

    let mut reader = session.log_reader().map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let bytes = reader.read_range(from, to).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

/// Parses a single `bytes=start-end` range (the only form this endpoint
/// needs to honor -- no multi-range, no suffix-length `bytes=-N`).
fn parse_byte_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    // `checked_add` rather than `+ 1`: an end of `u64::MAX` (e.g. a
    // malformed/adversarial `Range` header) must reject the range and fall
    // back to the full response, not overflow-panic or silently wrap to 0
    // (M4 review).
    let end: u64 = if end.is_empty() { len } else { end.parse::<u64>().ok()?.checked_add(1)? };
    Some((start, end))
}

async fn list_presets(State(state): State<Arc<AppState>>, _principal: Principal) -> impl IntoResponse {
    Json(serde_json::json!({ "presets": state.presets }))
}

fn find_session(state: &AppState, id: &str) -> Result<Arc<crate::session::Session>, ApiError> {
    let id: SessionId = id.parse().map_err(|_| ApiError::NotFound)?;
    state.sessions.get(id).ok_or(ApiError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset_with_args(args: &[&str]) -> Preset {
        Preset {
            id: "shell".to_string(),
            label: "Shell".to_string(),
            command: "$SHELL".to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            icon: "terminal".to_string(),
        }
    }

    #[test]
    fn omitted_args_falls_back_to_the_preset_default() {
        let preset = preset_with_args(&["-l"]);
        assert_eq!(resolve_args(None, Some(&preset)), vec!["-l".to_string()]);
    }

    /// The M4 review finding: `Some(vec![])` must win over the preset's
    /// default, not be treated the same as an omitted `args` field.
    #[test]
    fn explicit_empty_args_is_honored_not_replaced_by_the_preset_default() {
        let preset = preset_with_args(&["-l"]);
        assert_eq!(resolve_args(Some(vec![]), Some(&preset)), Vec::<String>::new());
    }

    #[test]
    fn explicit_args_override_the_preset_default() {
        let preset = preset_with_args(&["-l"]);
        let explicit = vec!["-c".to_string(), "true".to_string()];
        assert_eq!(resolve_args(Some(explicit.clone()), Some(&preset)), explicit);
    }

    #[test]
    fn no_preset_and_omitted_args_is_empty() {
        assert_eq!(resolve_args(None, None), Vec::<String>::new());
    }
}
