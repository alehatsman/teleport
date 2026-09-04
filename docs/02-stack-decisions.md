# 02 — Stack decisions

The chosen stack, why, and what was rejected. Implementation agents: **do not
relitigate these**. If you believe one is wrong, raise it before writing code.

## The stack

| Layer | Choice |
|---|---|
| Session daemon | **Rust** |
| HTTP / WebSocket | **Axum + Tokio** |
| PTY abstraction | **portable-pty** |
| PTY I/O scheduling | **dedicated blocking reader/writer threads per session** |
| Metadata | **SQLite**, one writer, WAL mode |
| Terminal history | **append-only raw VT/output files** |
| Frontend | **Svelte + TypeScript + Vite** |
| Terminal emulator | **xterm.js** |
| Desktop wrapper | **Tauri 2**, thin and optional |
| Phone | **responsive web app / PWA**, not native |
| Private remote access | **Tailscale Serve** |
| Clientless remote access | **Cloudflare Tunnel + Access** |
| Agent model | **normal PTY session + launch preset + metadata** |
| Desktop↔daemon protocol | **the same HTTP + WebSocket API the phone uses** |

## Why Rust — the actual reason

Not "Rust is faster." JSON routing is not the bottleneck and never will be.

The riskiest subsystem in this product is **native PTY and process management**, not
networking. `portable-pty` provides a runtime-selectable cross-platform PTY abstraction
and comes from the WezTerm terminal emulator ecosystem — it is exercised by a real
terminal, not a toy. Rust gives an explicit ownership/lifecycle model for exactly the
kind of OS-handle-and-child-process work that dominates this codebase, and it is the
same language as the Tauri shell, so packaging is natural rather than cross-runtime.

Axum supplies HTTP routing and WebSocket upgrades in one framework, so no second server
library is needed.

## Backend alternatives considered

| Backend | Cross-platform PTY | Complexity | Distribution | Advantages | Disadvantages | Verdict |
|---|---|---:|---:|---|---|---|
| **Rust + portable-pty + Axum** | Excellent — one abstraction, WezTerm ecosystem | Medium | Excellent native binary; natural Tauri fit | Strong ownership/lifecycle model; low overhead; same language as shell; great fit for OS/process work | Learning + compile cost; blocking PTY API needs a deliberate thread bridge | **Chosen** |
| **Go + `aymanbagabas/go-pty`** | Good — Unix + Windows ConPTY in one package | Low–medium | Excellent | Simplest concurrency model for reader goroutines; superb HTTP/process daemon ergonomics | PTY abstraction is a smaller layer than the Rust/Node terminal ecosystems; desktop shell stays a separate technology | Strongest challenger |
| **Node + `node-pty`** | Excellent — Linux/macOS/Windows, ConPTY on Windows, Microsoft-maintained | Low initially | Medium | Fastest prototype; TypeScript end-to-end; mature terminal integration | Native addon with documented Windows C++/SDK build prerequisites; an independently persistent daemon undoes Electron's "one JS app" advantage | Best prototype stack |
| **Python + `ptyprocess`/PyWinpty** | Split — Unix path plus a separate Windows path | Low code / medium packaging | Medium–low | Rapid iteration; good subprocess ecosystem | Two PTY implementations; runtime packaging; PyWinpty is itself Rust/native-backed, so Python does **not** remove the native layer — it adds a binding boundary | Not preferred |

Notes worth keeping honest:

- **Go is closer than a superficial comparison suggests.** The common objection ("Go
  can't do Windows PTYs") is out of date — that applies to `creack/pty`, which is
  Unix-focused. `aymanbagabas/go-pty` adds ConPTY because attaching a child to ConPTY
  needs Windows process attributes that plain `os/exec` doesn't expose. Go would be a
  fine choice; Rust wins on PTY-ecosystem maturity and Tauri alignment.
- **Node is the fastest path to a working prototype.** Its weakness here is deployment
  shape, not JS performance. Once the PTY host must outlive the GUI, you need a
  separately installed Node daemon or a bundled native executable plus `node-pty`.

## Frontend alternatives considered

| Frontend | Complexity | Strength | Weakness | Verdict |
|---|---:|---|---|---|
| Plain TypeScript + Vite | Lowest | Minimum deps; direct xterm.js integration | Hand-rolled state/component organization hurts once sessions, settings, agents, notifications and mobile views accumulate | Great for a terminal-only prototype |
| **Svelte + Vite** | Low | Small component model; compiles component work at build time; far less plumbing than a large SPA framework | Smaller ecosystem than React | **Chosen** |
| React + Vite | Medium | Broad ecosystem, conventional familiarity | No architectural benefit here; more state/component ceremony than needed | Fine, not simplest |
| Next / SvelteKit / any SSR stack | High relative to need | Routing + server features | Duplicates server concerns `teleportd` already owns | **Rejected** |

Framework choice matters far less than the PTY architecture. `xterm.js` is the terminal
layer regardless. **Do not write a terminal renderer.**

## Packaging alternatives considered

| Model | Pros | Cons | Verdict |
|---|---|---|---|
| Daemon + browser only | Smallest core; desktop and phone literally share the UI | Less native polish; startup/tray/install UX needs work | Excellent engineering MVP |
| **Rust daemon + thin Tauri** | Native feel without duplicating the backend; can bundle the daemon via `externalBin` | Needs a daemon lifecycle deliberately separate from the UI | **Chosen product packaging** |
| Node daemon + Electron | Fast JS/TS development; official packaging via Electron Forge | Electron runtime plus independent-daemon lifecycle plus native `node-pty` packaging; larger privileged renderer surface | Best Node option |
| Three native GUI implementations | Maximum platform integration | Highest code and test surface | **Rejected** |
| Tauri owns all PTYs internally | Initially simple | GUI lifecycle becomes terminal lifecycle — violates invariant 2 | **Rejected** |

## Direct dependencies

Keep this list short. Adding a crate is a decision, not a reflex.

```toml
[dependencies]
tokio            = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "signal"] }
axum             = { version = "0.8", features = ["ws"] }
tower-http       = { version = "0.6", features = ["fs", "trace"] }
portable-pty     = "0.9"
rusqlite         = { version = "0.32", features = ["bundled"] }
serde            = { version = "1", features = ["derive"] }
serde_json       = "1"
toml             = "0.8"
anyhow           = "1"
thiserror        = "2"
tracing          = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
ulid             = "1"
clap             = { version = "4", features = ["derive"] }
directories      = "5"

[target.'cfg(unix)'.dependencies]
libc             = "0.2"
```

`rusqlite` uses `bundled` so there is no system SQLite dependency at install time.
`libc` is Unix-only and exists solely for signal delivery in `pty.rs`
([03-pty-layer.md](03-pty-layer.md#termination)).

### API-shape gotchas for the pinned versions

- **axum 0.8** uses `/{id}` path syntax, not `/:id`.
- **axum 0.8** `Message::Text` carries `Utf8Bytes` and `Message::Binary` carries
  `Bytes` — not `String`/`Vec<u8>`.
- **portable-pty** `Child::kill()` is a *hard* kill on Unix. Graceful termination is
  implemented separately; see [03-pty-layer.md](03-pty-layer.md#termination).

Verify these against the versions actually resolved in `Cargo.lock` before writing
handler code — do not trust this table over the compiler.
