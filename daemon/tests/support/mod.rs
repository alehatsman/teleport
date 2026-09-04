//! Shared test harness for the M4 protocol tests
//! (docs/10-testing.md#3-protocol-tests). Not a test binary itself -- `mod
//! support;` from `http_api.rs`/`ws_protocol.rs` pulls this in as a plain
//! module, per the standard `tests/<name>/mod.rs` convention.
//!
//! Each consumer only needs some of what's here (`http_api.rs` never opens a
//! WebSocket; `ws_protocol.rs` never inspects `base_url`), and each is
//! compiled as its own separate test binary -- hence the blanket
//! `dead_code` allow rather than per-item ones.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use teleportd::api::AppState;
use teleportd::auth::OriginPolicy;
use teleportd::config::Config;
use teleportd::device::Device;
use teleportd::session::SessionManager;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

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
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
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
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");

    let origin_policy = OriginPolicy::new(addr.port(), false, &config.allowed_origins, &config.allowed_hosts);
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

    Daemon { addr, state, server }
}

pub fn default_config() -> Config {
    Config::default()
}

/// Creates a `shell`-kind `/bin/sh` session directly through the
/// `SessionManager` (bypassing HTTP) -- the fastest way for a WS-focused
/// test to get a session id to attach to.
pub fn create_shell_session(daemon: &Daemon, args: Vec<String>) -> teleportd::session::SessionId {
    let cwd = std::env::temp_dir();
    let spec = teleportd::pty::SpawnSpec { program: "/bin/sh", args: &args, cwd: &cwd, env: &[], cols: 80, rows: 24 };
    let session = daemon.state.sessions.create(spec, "shell", None).expect("create session");
    session.id
}
