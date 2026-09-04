//! Helper binary for s5_minimal: does nothing but print one line and exit with the
//! given code (default 0). No shell, no console API calls beyond what Rust's std
//! runtime does implicitly. Used to isolate whether the W1 hang is specific to
//! cmd.exe or happens for any process attached to a ConPTY that exits on its own.

fn main() {
    let code: i32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    println!("[mini_exit] pid={} exiting with code {code}", std::process::id());
    std::process::exit(code);
}
