# M1 spike

Throwaway experiments answering S1–S4 in
[docs/15-open-questions.md](../docs/15-open-questions.md#the-m1-spike). Not
production code — results are folded into `docs/03-pty-layer.md` and
`docs/15-open-questions.md`; this crate is kept only so the experiments are
reproducible instead of taken on faith.

## Run on Linux / macOS

```sh
cargo build
./target/debug/s1_reaper <exit0|exit7|sigkill|grandchild> <poll|blocking>
./target/debug/s2_eof <basic|grandchild|midburst>
./target/debug/s3_blocking_write <shared|separate>
./target/debug/s4_drop_master <plain|grandchild>
```

All output goes to stderr (unbuffered), not stdout.

## Run on Windows

Findings here are Linux-only; Windows was **not** reliably testable through a WSL2
sandbox (ConPTY children hung indefinitely when launched via WSL process interop —
looks like a missing interactive console/window-station context in that bridge, not
a portable-pty or product issue). Cross-compiled binaries exist
(`cargo build --target x86_64-pc-windows-gnu`, needs `mingw-w64` +
`rustup target add x86_64-pc-windows-gnu` on the Linux side) but need to be run from
a **real interactive Windows session** — Windows Terminal or plain `cmd`/PowerShell
on the actual machine, not through WSL interop:

```powershell
.\target\x86_64-pc-windows-gnu\debug\s1_reaper.exe exit0 blocking
.\target\x86_64-pc-windows-gnu\debug\s2_eof.exe basic
.\target\x86_64-pc-windows-gnu\debug\s3_blocking_write.exe shared
.\target\x86_64-pc-windows-gnu\debug\s4_drop_master.exe plain
```

Or install a Rust toolchain natively on Windows and `cargo build` there directly —
either way, what matters is running from a real console session.

## macOS

Not attempted here (no macOS available). Same binaries, `cargo build` natively.
