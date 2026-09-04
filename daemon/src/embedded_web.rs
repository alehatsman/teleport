//! Web UI assets baked into the binary, for release builds only
//! (docs/16-release-pipeline.md#embedding-the-web-ui-embedded-web-feature).
//!
//! Compiled in only with `--features embedded-web`. The release workflow
//! runs `npm run build` in `web/` before building the daemon with this
//! feature, so `../web/dist` exists when `RustEmbed` reads it -- with the
//! feature on and the folder missing or empty, that's a compile-time
//! failure, not a silently empty UI. Normal dev builds (`cargo build`,
//! `cargo test`, `npm run dev`) never enable this feature and never touch
//! this module.

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../web/dist"]
struct Assets;

/// Serves `path` from the embedded bundle, falling back to `index.html` for
/// anything not found -- the same SPA client-side-routing rule
/// `api.rs::spa_fallback` applies to the disk-backed `ServeDir` path.
pub fn serve(path: &str) -> Response {
    let path = path.trim_start_matches('/');
    let (served_path, file) = match Assets::get(path) {
        Some(file) => (path, file),
        None => match Assets::get("index.html") {
            Some(file) => ("index.html", file),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let mime = mime_guess::from_path(served_path).first_or_octet_stream();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, mime.as_ref().to_string())],
        Body::from(file.data.into_owned()),
    )
        .into_response()
}
