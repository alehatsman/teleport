//! Library surface for `teleportd`. `main.rs` is the binary entry point;
//! this crate exists so `daemon/tests/*.rs` integration tests can drive
//! internals like `pty.rs` directly instead of only black-box through the
//! compiled binary (docs/11-mvp-plan.md#m1--pty-primitive: "drive it from
//! integration tests").

pub mod api;
pub mod auth;
pub mod config;
pub mod device;
pub mod log;
pub mod persistence;
pub mod presets;
pub mod pty;
pub mod session;
pub mod ws;

/// Milliseconds since the Unix epoch -- the shape of every `*_at_ms` field
/// across `session.rs`/`persistence.rs`/`main.rs` and the matching SQLite
/// columns (docs/05-persistence.md#schema). One definition so a future fix
/// to its clock-skew handling can't land in some call sites and not others.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_millis() as i64
}
