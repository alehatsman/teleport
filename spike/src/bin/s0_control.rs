//! Control test for the Windows S1/S2 anomaly: does `cmd.exe /c "exit 0"` reap
//! correctly with NO ConPTY involved at all (plain std::process::Command)?
//! If this returns fast, ConPTY is the variable. If this also hangs, it's
//! something about cmd.exe / this machine's process teardown in general.

use std::time::Instant;

fn main() {
    let t0 = Instant::now();
    eprintln!("[s0] spawning cmd.exe /c \"exit 0\" with NO pty involved");
    let mut child = std::process::Command::new("cmd.exe")
        .arg("/c")
        .arg("exit 0")
        .spawn()
        .expect("spawn failed");
    eprintln!(
        "[s0] pid={} spawned at {}ms",
        child.id(),
        t0.elapsed().as_millis()
    );
    let status = child.wait().expect("wait failed");
    eprintln!(
        "[s0] RESULT wait() returned at {}ms exit_code={:?}",
        t0.elapsed().as_millis(),
        status.code()
    );
}
