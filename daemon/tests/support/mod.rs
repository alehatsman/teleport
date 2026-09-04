//! Shared test-only helpers for `daemon/tests/*.rs`.
//!
//! Not a test binary itself -- cargo only auto-discovers `.rs` files
//! directly under `tests/` as separate integration-test crates, so a
//! `tests/support/mod.rs` reached via `mod support;` is invisible to that
//! discovery and just becomes a private module inside whichever binary
//! declares it. Each of those binaries gets its own copy compiled in (there
//! is no way to share a compiled crate between `tests/*.rs` binaries without
//! promoting this to a dev-dependency), but the *source* -- and the
//! assertions baked into it -- is written once.
//!
//! Two unrelated groups of consumers share this file rather than each
//! getting their own: [`catch_up`] for `session_replay.rs`/
//! `session_catchup.rs`/`session_backpressure.rs` (drives a `Replay` to the
//! live boundary against an in-process `SessionManager`, no HTTP involved),
//! and everything below it for `http_api.rs`/`ws_protocol.rs`
//! (docs/10-testing.md#3-protocol-tests -- boots a real daemon on a loopback
//! port). Each consumer only needs some of what's here (`catch_up` never
//! touches HTTP; `http_api.rs` never opens a WebSocket; `ws_protocol.rs`
//! never inspects `base_url`), and each is compiled as its own separate test
//! binary -- hence the blanket `dead_code` allow rather than per-item ones.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use teleportd::api::AppState;
use teleportd::auth::OriginPolicy;
use teleportd::config::Config;
use teleportd::device::Device;
use teleportd::session::{Attach, Replay, ReplayStep, SessionManager};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Generous upper bound on rounds any fixture using [`catch_up`] should ever
/// need. The "never converges" failure mode has its own deterministic
/// coverage in session.rs's unit tests; this exists only so a real
/// regression here hangs a test loudly and boundedly instead of forever.
const MAX_TEST_ROUNDS: u32 = 1024;

/// Drives a `Replay` to the live boundary the way M4's WS loop will: write
/// each catch-up round out before asking for the next
/// (docs/04-api-protocol.md#catch-up--register-late-not-early). `round_delay`
/// paces each round -- `Duration::ZERO` for "as fast as possible" (most
/// fixtures, where a `Vec` client always outruns the producer), nonzero to
/// simulate a slow client's network round-trip
/// (`session_catchup.rs`'s D1 gate, which needs the producer to gain ground
/// on a genuinely slow consumer, not just an unrealistically instant one).
///
/// Returns every replayed byte (catch-up rounds and the final stretch
/// together), the live handover, and how many bounded rounds it took.
/// Rounds are asserted contiguous with each other; the join onto
/// `Attach::replay_from` is the caller's to check, because a cap can
/// legitimately move it forward. `attach.caught_up` is asserted here too --
/// every caller of this helper drives a client that outruns the producer, so
/// a catch-up that gives up and clamps means the fixture stopped testing
/// what it says it tests, not a real assertion about the product.
pub async fn catch_up(replay: Replay, round_delay: Duration) -> (Vec<u8>, Attach, u32) {
    let mut acc = Vec::new();
    let mut next = replay.replay_from;
    let mut rounds = 0u32;
    let mut step = replay.next_round().expect("first catch-up round");
    loop {
        match step {
            ReplayStep::History {
                offset,
                bytes,
                replay,
            } => {
                assert_eq!(offset, next, "catch-up rounds must be contiguous");
                next = offset + bytes.len() as u64;
                acc.extend_from_slice(&bytes);
                rounds += 1;
                assert!(
                    rounds <= MAX_TEST_ROUNDS,
                    "catch-up did not converge within {MAX_TEST_ROUNDS} rounds"
                );
                tokio::time::sleep(round_delay).await; // the "network"
                step = replay.written(bytes).expect("catch-up round");
            }
            ReplayStep::Live(attach) => {
                assert!(
                    attach.caught_up,
                    "a client that always outruns the producer must converge"
                );
                acc.extend_from_slice(&attach.replay);
                return (acc, attach, rounds);
            }
        }
    }
}

pub const TOKEN: &str = "0123456789abcdef0123456789abcdef";

pub struct Daemon {
    pub addr: std::net::SocketAddr,
    pub state: Arc<AppState>,
    server: JoinHandle<()>,
}

impl Daemon {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn ws_url(&self, path_and_query: &str) -> String {
        format!("ws://{}{}", self.addr, path_and_query)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn sessions_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "teleportd-protocol-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Boots a real daemon (real `TcpListener`, real `axum::serve`) on an
/// ephemeral loopback port with the given config, for tests that need an
/// actual WebSocket connection rather than an in-process request. Dropping
/// the returned [`Daemon`] aborts the server task.
pub async fn spawn(config: Config) -> Daemon {
    spawn_with_web_dist(config, None).await
}

/// Like [`spawn`], but with `AppState::web_dist` set -- for the SPA-fallback
/// tests, which need a router that actually serves `web/dist`
/// (docs/08-packaging.md#build-pipeline).
pub async fn spawn_with_web_dist(config: Config, web_dist: Option<PathBuf>) -> Daemon {
    let sessions = SessionManager::new(sessions_root("ws")).with_max_sessions(config.max_sessions);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");

    let origin_policy = OriginPolicy::new(
        addr.port(),
        false,
        &config.allowed_origins,
        &config.allowed_hosts,
    );
    let state = Arc::new(AppState {
        sessions,
        origin_policy,
        token: TOKEN.to_string(),
        presets: vec![],
        device: Device {
            device_id: "01TESTDEVICE00000000000000".to_string(),
            device_name: "test-device".to_string(),
            platform: "test".to_string(),
        },
        config,
        started_at: Instant::now(),
        version: "test",
        web_dist,
    });

    let app = teleportd::api::build_router(Arc::clone(&state));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Daemon {
        addr,
        state,
        server,
    }
}

pub fn default_config() -> Config {
    Config::default()
}

/// Creates a `shell`-kind `/bin/sh` session directly through the
/// `SessionManager` (bypassing HTTP) -- the fastest way for a WS-focused
/// test to get a session id to attach to.
pub fn create_shell_session(daemon: &Daemon, args: Vec<String>) -> teleportd::session::SessionId {
    let cwd = std::env::temp_dir();
    let spec = teleportd::pty::SpawnSpec {
        program: "/bin/sh",
        args: &args,
        cwd: &cwd,
        env: &[],
        cols: 80,
        rows: 24,
    };
    let session = daemon
        .state
        .sessions
        .create(spec, "shell", None)
        .expect("create session");
    session.id
}
