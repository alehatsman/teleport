//! Library surface for `teleportd`. `main.rs` is the binary entry point;
//! this crate exists so `daemon/tests/*.rs` integration tests can drive
//! internals like `pty.rs` directly instead of only black-box through the
//! compiled binary (docs/11-mvp-plan.md#m1--pty-primitive: "drive it from
//! integration tests").

// The anchor path speaks to `anchord` over a Unix socket, and nothing else
// yet: `anchor_client` spells `tokio::net::unix::Owned{Read,Write}Half` into
// its own public surface, so the modules built on it are Unix-only too.
// Windows therefore compiles the in-process PTY layer and only that, which
// is already what it runs -- `use_anchor` defaults off (`main.rs`), so no
// Windows deployment has ever reached anchord.
//
// This is a missing implementation, not a missing attribute. What Windows
// needs is a transport that is not an `AF_UNIX` socket, and the shape of
// that seam is anchor's to decide, not teleport's: docs/20's Constraint 3
// asks whether `anchor_socket` becomes transport-agnostic or each platform
// gets its own, and whether anchor's framing survives a named pipe
// unchanged. Until that answer exists there is nothing honest to gate *in*.
//
// The gate's immediate purpose is narrower than anchor: with the daemon
// failing to build on Windows at all, `tests/pty_primitive_windows.rs` --
// the sole proof of the ConPTY terminate path, and an artifact docs/20
// requires before the in-process layer may be deleted -- could not be
// compiled, let alone run. A retirement gate nothing can execute is not a
// gate. See #107.
#[cfg(unix)]
pub mod anchor_bridge;
#[cfg(unix)]
pub mod anchor_client;
pub mod anchor_lease;
#[cfg(unix)]
pub mod anchor_watch;
pub mod api;
pub mod auth;
pub mod auth_routes;
pub mod auth_store;
pub mod config;
pub mod device;
#[cfg(feature = "embedded-web")]
pub mod embedded_web;
pub mod log;
pub mod persistence;
pub mod presets;
pub mod pty;
pub mod session;
pub mod web_assets;
pub mod ws;
#[cfg(unix)]
pub mod ws_anchor;

/// Milliseconds since the Unix epoch -- the shape of every `*_at_ms` field
/// across `session.rs`/`persistence.rs`/`main.rs` and the matching SQLite
/// columns (docs/05-persistence.md#schema). One definition so a future fix
/// to its clock-skew handling can't land in some call sites and not others.
#[expect(
    clippy::cast_possible_truncation,
    reason = "i64::MAX ms since 1970 is year ~292,471,208 -- this process will not still be running"
)]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as i64
}
