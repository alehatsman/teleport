//! Thin wrappers over the HTTP surface (docs/04-api-protocol.md#http-surface)
//! for `sessions`/`new`/`kill`. `attach.rs` talks to the WebSocket surface
//! directly and doesn't use this module.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::connect::Connection;

pub(crate) struct Client {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

/// The daemon's error body shape (`daemon/src/api.rs`'s `ApiError`):
/// `{"error": "<code>", "message": "<text>"}`. Printed back to the user
/// close to verbatim -- docs/11-mvp-plan.md#m11's edge cases call for
/// exactly this on `401`/`403`, and there's no reason to treat other
/// non-2xx codes differently.
#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: Option<String>,
    message: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiError {
    #[error("{status}: {}", format_body(error, message))]
    Status {
        status: reqwest::StatusCode,
        error: Option<String>,
        message: Option<String>,
    },
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
}

fn format_body(error: &Option<String>, message: &Option<String>) -> String {
    match (error, message) {
        (Some(e), Some(m)) => format!("{e}: {m}"),
        (Some(e), None) => e.clone(),
        (None, Some(m)) => m.clone(),
        (None, None) => "(no body)".to_string(),
    }
}

impl ApiError {
    /// The one-line hint docs/11-mvp-plan.md#m11 asks for on `401`/`403` --
    /// a native client's whole auth story is the bearer credential, so a
    /// rejection almost always means it's missing or wrong, not an Origin
    /// problem (`teleport` never sends `Origin` at all).
    pub(crate) fn hint(&self) -> Option<&'static str> {
        match self {
            ApiError::Status { status, .. }
                if *status == reqwest::StatusCode::UNAUTHORIZED
                    || *status == reqwest::StatusCode::FORBIDDEN =>
            {
                Some("check --token / TELEPORT_TOKEN")
            }
            _ => None,
        }
    }

    #[expect(
        dead_code,
        reason = "kept for callers that want the HTTP status without matching on the ApiError variant; unused today"
    )]
    pub(crate) fn status(&self) -> Option<reqwest::StatusCode> {
        match self {
            ApiError::Status { status, .. } => Some(*status),
            ApiError::Transport(_) => None,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateSessionRequest {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateSessionResponse {
    pub id: String,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub state: String,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub pid: Option<u32>,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub output_offset: u64,
}

/// Mirrors `daemon/src/api.rs`'s `SessionView` field for field.
#[derive(Debug, Deserialize)]
pub(crate) struct SessionSummary {
    pub id: String,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub kind: String,
    pub preset: Option<String>,
    pub command: String,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub args: Vec<String>,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub cwd: String,
    pub state: String,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub output_bytes: u64,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub created_at_ms: i64,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub started_at_ms: Option<i64>,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub exited_at_ms: Option<i64>,
    pub exit_code: Option<i32>,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub lost_reason: Option<String>,
    pub controller: Option<String>,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub subscribers: usize,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub last_bell_ms: Option<i64>,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub idle_since_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ListSessionsResponse {
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Preset {
    pub id: String,
    pub label: String,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub command: String,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub args: Vec<String>,
    #[expect(
        dead_code,
        reason = "mirrors the daemon's JSON response shape (daemon/src/api.rs); not every field is read"
    )]
    pub icon: String,
}

#[derive(Debug, Deserialize)]
struct PresetsResponse {
    presets: Vec<Preset>,
}

impl Client {
    pub(crate) fn new(conn: &Connection) -> Result<Self> {
        Ok(Client {
            http: reqwest::Client::builder()
                .build()
                .context("building HTTP client")?,
            base_url: conn.base_url.clone(),
            token: conn.token.clone(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Every response funnels through here: non-2xx becomes `ApiError`,
    /// parsed from the daemon's `{"error", "message"}` body when present.
    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body: Option<ErrorBody> = resp.json().await.ok();
        Err(ApiError::Status {
            status,
            error: body.as_ref().and_then(|b| b.error.clone()),
            message: body.and_then(|b| b.message),
        })
    }

    pub(crate) async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ApiError> {
        let resp = self
            .http
            .get(self.url("/api/v1/sessions"))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        Ok(resp.json::<ListSessionsResponse>().await?.sessions)
    }

    pub(crate) async fn create_session(
        &self,
        req: &CreateSessionRequest,
    ) -> Result<CreateSessionResponse, ApiError> {
        let resp = self
            .http
            .post(self.url("/api/v1/sessions"))
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        Ok(resp.json().await?)
    }

    pub(crate) async fn delete_session(&self, id: &str, purge: bool) -> Result<(), ApiError> {
        let mut url = self.url(&format!("/api/v1/sessions/{id}"));
        if purge {
            url.push_str("?purge=true");
        }
        let resp = self
            .http
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .await?;
        Self::check(resp).await?;
        Ok(())
    }

    pub(crate) async fn presets(&self) -> Result<Vec<Preset>, ApiError> {
        let resp = self
            .http
            .get(self.url("/api/v1/presets"))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let resp = Self::check(resp).await?;
        Ok(resp.json::<PresetsResponse>().await?.presets)
    }
}
