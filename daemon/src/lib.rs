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
