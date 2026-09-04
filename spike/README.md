# M1 spike

Throwaway experiments answering S1–S4 in
[docs/15-open-questions.md](../docs/15-open-questions.md#the-m1-spike). Not
production code — results are folded into `docs/03-pty-layer.md` and
`docs/15-open-questions.md`; this crate is kept only so the experiments are
reproducible instead of taken on faith.

## Run on Linux / macOS

```sh
cargo build
./target/debug/s0_control
./target/debug/s1_reaper <exit0|exit7|sigkill|grandchild> <poll|blocking>
./target/debug/s2_eof <basic|grandchild|midburst>
./target/debug/s3_blocking_write <shared|separate>
./target/debug/s4_drop_master <plain|grandchild>
./target/debug/s5_minimal <exit0|exit7|sigkill>
```

All output goes to stderr (unbuffered), not stdout.

## Run on Windows

Run from a **real interactive Windows session** — Windows Terminal or plain
`cmd`/PowerShell on the actual machine, not through WSL interop (`wsl.exe` /
`powershell.exe` called from inside WSL). WSL interop was tried first and looked
like the cause of an early hang, but re-running the same test from a genuine
interactive session reproduced the identical hang — see
[W1](../docs/15-open-questions.md#w1--conpty-children-are-never-observed-as-exited-on-windows).
It is a real Windows/ConPTY finding, not a bridge artifact.

Easiest path: `run-windows-spike.ps1` in this directory runs every binary with
timeouts and writes a transcript — see the comment at its top for usage.

Cross-compiled binaries are built from the Linux side
(`cargo build --target x86_64-pc-windows-gnu`, needs `mingw-w64` +
`rustup target add x86_64-pc-windows-gnu`; see `.cargo/config.toml` in this
directory for the scoped linker config):

```powershell
.\target\x86_64-pc-windows-gnu\debug\s0_control.exe
.\target\x86_64-pc-windows-gnu\debug\s1_reaper.exe exit0 blocking
.\target\x86_64-pc-windows-gnu\debug\s2_eof.exe basic
.\target\x86_64-pc-windows-gnu\debug\s3_blocking_write.exe shared
.\target\x86_64-pc-windows-gnu\debug\s4_drop_master.exe plain
.\target\x86_64-pc-windows-gnu\debug\s5_minimal.exe exit0
```

Or install a Rust toolchain natively on Windows and `cargo build` there directly —
either way, what matters is running from a real console session.

## macOS

Not attempted here (no macOS available). Same binaries, `cargo build` natively.
