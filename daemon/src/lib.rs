//! Library surface for `teleportd`. `main.rs` is the binary entry point;
//! this crate exists so `daemon/tests/*.rs` integration tests can drive
//! internals like `pty.rs` directly instead of only black-box through the
//! compiled binary (docs/11-mvp-plan.md#m1--pty-primitive: "drive it from
//! integration tests").

pub mod pty;
