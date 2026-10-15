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
#![allow(
    dead_code,
    reason = "each test binary uses a different subset of this shared support module"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use teleportd::api::AppState;
use teleportd::auth::{OriginPolicy, TicketStore};
use teleportd::config::Config;
use teleportd::device::Device;
use teleportd::session::{Attach, Replay, ReplayStep, SessionManager};
use teleportd::web_assets::WebAssets;
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
pub(crate) async fn catch_up(replay: Replay, round_delay: Duration) -> (Vec<u8>, Attach, u32) {
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

/// Like [`catch_up`], but for a caller that expects some of its concurrent
/// clients to lose the race against the producer -- N4's reconnect-storm
/// fixture, where `STORM_SIZE` clients all attach at once against an
/// unbounded `yes` and some are expected to hit the same
/// stalled/total-round clamp D1's own fixture in `session_catchup.rs`
/// exercises deliberately. Always paced at `Duration::ZERO`: a storm client
/// is modeling "reconnect as fast as possible," not a slow network.
pub(crate) fn catch_up_allow_clamp(replay: Replay) -> (Vec<u8>, Attach, u32) {
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
                step = replay.written(bytes).expect("catch-up round");
            }
            ReplayStep::Live(attach) => {
                acc.extend_from_slice(&attach.replay);
                return (acc, attach, rounds);
            }
        }
    }
}

pub(crate) const TOKEN: &str = "0123456789abcdef0123456789abcdef";

pub(crate) struct Daemon {
    pub addr: std::net::SocketAddr,
    pub state: Arc<AppState>,
    server: JoinHandle<()>,
}

/// A distinct owner per fixture, so one fixture's sessions are invisible to
/// another's list and uncounted by its `max_sessions`.
fn next_owner_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The pid scopes owners to this run. Without it a reused daemon would
    // hand a later run's "test-0" the sessions an earlier run's "test-0"
    // left behind, and a list assertion would count someone else's work.
    format!("{}-{n}", std::process::id())
}

/// The one `anchord` this test binary shares.
///
/// One per binary, not one per fixture: a daemon per fixture meant dozens
/// of processes with four threads per live session, and creates began
/// failing under that load — a harness that cannot hold still is no way to
/// judge a product. Fixtures stay isolated by owning their sessions under a
/// unique `anchor_owner`, which is the same mechanism that keeps two real
/// tools sharing one daemon out of each other's lists.
///
/// Never dropped: a `static` lives for the process, so the daemon outlives
/// every fixture in the binary and its directory is cleaned up by the OS.
static SHARED: std::sync::OnceLock<Option<Anchord>> = std::sync::OnceLock::new();

/// Wipes every session out of the shared daemon, once per run.
///
/// Reusing a daemon across runs stops it leaking *processes*, but each run
/// leaves its sessions behind — most of them still running, since fixtures
/// rarely terminate what they spawn. Those accumulate into exactly the
/// resource pressure the reuse was meant to end. The daemon belongs
/// entirely to this test binary, so starting each run from empty is both
/// safe and what isolation actually requires.
async fn wipe_shared_daemon(socket: &Path) {
    static WIPED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    WIPED
        .get_or_init(|| async {
            let client = teleportd::anchor_client::AnchorClient::new(socket.to_path_buf());
            let Ok(sessions) = client.list(&std::collections::BTreeMap::new()).await else {
                return;
            };
            for session in sessions {
                // Terminate first: anchor refuses to purge a live session,
                // which is the behaviour this relies on rather than works
                // around. Both are best-effort — a session another run is
                // mid-way through tearing down is already going.
                drop(client.terminate(&session.id).await);
                drop(client.purge(&session.id).await);
            }
        })
        .await;
}

/// The shared daemon's socket, or `None` when not running on anchor.
pub(crate) fn shared_anchord_socket() -> Option<PathBuf> {
    SHARED
        .get_or_init(|| {
            let anchord = Anchord::start_if_requested()?;
            anchord.wait_ready();
            Some(anchord)
        })
        .as_ref()
        .map(Anchord::socket)
}

/// An `anchord` the suite drives, so teleport's own tests can run against
/// the anchor path rather than a suite written to agree with it.
pub(crate) struct Anchord {
    /// `None` when this run reused a daemon an earlier run started.
    child: Option<std::process::Child>,
    dir: PathBuf,
}

impl Anchord {
    /// `None` unless `TELEPORT_TEST_ANCHOR` and `ANCHORD_BIN` are both set.
    fn start_if_requested() -> Option<Self> {
        std::env::var_os("TELEPORT_TEST_ANCHOR")?;
        let raw = std::env::var_os("ANCHORD_BIN").expect("TELEPORT_TEST_ANCHOR needs ANCHORD_BIN");
        let bin = PathBuf::from(&raw);
        assert!(bin.is_file(), "ANCHORD_BIN is set to {raw:?}, not a file");

        // A fixed directory per test binary, not a unique one per run.
        //
        // This daemon lives in a `static` and statics are never dropped, so
        // nothing runs its teardown when the test process exits — a
        // per-run path meant every run abandoned a daemon that nothing
        // would ever reap. Dozens accumulated, held PTYs, and eventually
        // wedged the machine; I misread the resulting failures as product
        // flakiness twice before finding it.
        //
        // A fixed path makes the next run find the previous daemon and
        // reuse it, so at most one exists per binary no matter how often
        // the suite runs. Isolation does not depend on the daemon being
        // fresh: fixtures own their sessions under a unique owner, and the
        // run nonce below keeps a later run's owners from colliding with
        // an earlier one's.
        let stem = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "suite".to_owned());
        // Trimmed: the socket path underneath this is capped near 104 bytes.
        let stem: String = stem.chars().take(24).collect();
        let dir = PathBuf::from(format!("/tmp/tp-anchor-{stem}"));

        // A daemon already listening here is reused as-is.
        let socket = dir.join("run/anchor.sock");
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            return Some(Self { child: None, dir });
        }

        // Otherwise start fresh: whatever is there is a dead daemon's
        // leftovers, and reusing its database would carry an earlier run's
        // sessions into this one.
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(dir.join("run")).expect("creating the data dir");
        // Captured, so a daemon that dies on startup says why instead of
        // being diagnosed as "slow" by a timeout.
        let log = std::fs::File::create(dir.join("anchord.log")).expect("creating the log");
        let child = std::process::Command::new(&bin)
            .env("XDG_DATA_HOME", &dir)
            .env("XDG_RUNTIME_DIR", dir.join("run"))
            .env("ANCHOR_LOG", "info")
            .stderr(std::process::Stdio::from(log))
            .spawn()
            .expect("spawning anchord");
        Some(Self {
            child: Some(child),
            dir,
        })
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("run/anchor.sock")
    }

    /// Blocks until the socket accepts, so a fixture never races startup.
    ///
    /// Generous, because every fixture starts its own anchord and a full
    /// parallel run starts dozens at once. The wait is for a process to
    /// bind a unix socket, so a slow one here means a loaded machine, not a
    /// broken daemon.
    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::os::unix::net::UnixStream::connect(self.socket()).is_err() {
            assert!(
                Instant::now() < deadline,
                "anchord never came up; its log said: {}",
                std::fs::read_to_string(self.dir.join("anchord.log"))
                    .unwrap_or_else(|e| format!("<unreadable: {e}>"))
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

// Deliberately no `Drop`. This lives in a `static`, which is never
// dropped, so an impl here would be dead code that reads as cleanup — the
// most misleading kind. The daemon is reused by the next run instead; to
// clear it by hand: `pkill -f tp-anchor- && rm -rf /tmp/tp-anchor-*`.

impl Daemon {
    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub(crate) fn ws_url(&self, path_and_query: &str) -> String {
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
            .expect("system clock before 1970")
            .as_nanos()
    ))
}

/// Boots a real daemon (real `TcpListener`, real `axum::serve`) on an
/// ephemeral loopback port with the given config, for tests that need an
/// actual WebSocket connection rather than an in-process request. Dropping
/// the returned [`Daemon`] aborts the server task.
pub(crate) async fn spawn(config: Config) -> Daemon {
    spawn_with_web_dist(config, None, None).await
}

/// Like [`spawn`], but with a real SQLite store behind `AppState::db` -- for
/// the routes that have nowhere to read or write without one (the pin routes,
/// docs/19-locations.md#pins). Its files land under `dir`, which the caller
/// owns and cleans up.
pub(crate) async fn spawn_with_db(config: Config, dir: &Path) -> Daemon {
    let (db, _) = teleportd::persistence::Db::open(&dir.join("state.db"), &dir.join("sessions"))
        .expect("open test db");
    spawn_with_web_dist(config, None, Some(db)).await
}

/// Like [`spawn`], but with `AppState::web_dist` set -- for the SPA-fallback
/// tests, which need a router that actually serves `web/dist`
/// (docs/08-packaging.md#build-pipeline).
pub(crate) async fn spawn_with_web_dist(
    config: Config,
    web_dist: Option<PathBuf>,
    db: Option<teleportd::persistence::Db>,
) -> Daemon {
    spawn_with_web_assets(config, WebAssets::new(web_dist, None), db).await
}

/// Like [`spawn_with_web_dist`], but takes the whole [`WebAssets`] -- for
/// the version-slot tests (docs/18-ui-upgrades.md), which need a
/// `<data_dir>/web` root rather than a fixed dist directory.
pub(crate) async fn spawn_with_web_assets(
    config: Config,
    web: WebAssets,
    db: Option<teleportd::persistence::Db>,
) -> Daemon {
    spawn_inner(config, web, db, None).await
}

/// A daemon whose sessions live in an external `anchord` on `socket` — the
/// anchor path, exercised through teleport's own HTTP surface.
pub(crate) async fn spawn_on_anchor(
    config: Config,
    db: Option<teleportd::persistence::Db>,
    socket: PathBuf,
) -> Daemon {
    spawn_inner(
        config,
        WebAssets::new(None, None),
        db,
        Some(teleportd::anchor_client::AnchorClient::new(socket)),
    )
    .await
}

async fn spawn_inner(
    mut config: Config,
    web: WebAssets,
    db: Option<teleportd::persistence::Db>,
    anchor: Option<teleportd::anchor_client::AnchorClient>,
) -> Daemon {
    // On the anchor path every fixture shares one daemon and is isolated by
    // owning its sessions: teleport lists and counts only its own, so a
    // unique owner per fixture is exactly the isolation the product already
    // gives two real tools sharing a daemon.
    let anchor = match (anchor, shared_anchord_socket()) {
        (Some(client), _) => Some(client),
        (None, Some(socket)) => {
            wipe_shared_daemon(socket.as_path()).await;
            config.anchor_owner = format!("test-{}", next_owner_id());
            Some(teleportd::anchor_client::AnchorClient::new(socket))
        }
        (None, None) => None,
    };

    // Fixtures get the supervisor too, so the anchor path they exercise is
    // the one that ships rather than a quieter variant of it.
    let watch = Arc::new(teleportd::anchor_watch::WatchRegistry::new());
    if let Some(client) = &anchor {
        teleportd::anchor_watch::spawn_supervisor(
            client.clone(),
            Arc::clone(&watch),
            config.anchor_owner.clone(),
        );
    }

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
    // Cloned before `config` is moved into the state below.
    let config_origins = config.allowed_origins.clone();
    let config_hosts = config.allowed_hosts.clone();
    let state = Arc::new(AppState {
        sessions,
        // `None` for the in-process path, which is what most fixtures
        // drive; `Some` routes sessions to a real anchord.
        anchor,
        watch: Arc::clone(&watch),
        leases: Arc::new(teleportd::anchor_lease::LeaseRegistry::new()),
        db,
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
        web,
        shutdown: Arc::new(tokio::sync::Notify::new()),
        ws_tickets: TicketStore::new(),
        bound_port: addr.port(),
        passkeys: teleportd::auth_routes::PasskeyState::new(
            addr.port(),
            &config_origins,
            &config_hosts,
        ),
    });

    let app = teleportd::api::build_router(Arc::clone(&state));
    let server = tokio::spawn(async move {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "background server task; an error here (or the task being aborted when the test's Daemon handle drops) isn't something the test needs to observe"
        )]
        let _ = axum::serve(listener, app).await;
    });

    Daemon {
        addr,
        state,
        server,
    }
}

pub(crate) fn default_config() -> Config {
    Config::default()
}

/// Creates a `shell`-kind `/bin/sh` session directly through the
/// `SessionManager` (bypassing HTTP) -- the fastest way for a WS-focused
/// test to get a session id to attach to.
/// A session's authoritative output offset, from whichever backend holds
/// it.
///
/// Reaching into `state.sessions` asserts the same thing but only on the
/// in-process path; a test that does so is pinned to an implementation
/// rather than to the protocol it means to check.
pub(crate) async fn session_offset(daemon: &Daemon, id: teleportd::session::SessionId) -> u64 {
    match &daemon.state.anchor {
        Some(anchor) => {
            anchor
                .get(&id.to_string())
                .await
                .expect("anchord knows the session")
                .output_bytes
        }
        None => daemon
            .state
            .sessions
            .get(id)
            .expect("session still listed")
            .next_offset(),
    }
}

/// Creates a session in whichever backend this daemon uses.
///
/// Routing by backend matters: creating in the in-process manager while the
/// daemon serves from anchord gives a session the API cannot see, and every
/// test that then attaches gets `not_found` — which looks like a protocol
/// bug and is really a fixture talking to the wrong store.
pub(crate) async fn create_shell_session(
    daemon: &Daemon,
    args: &[String],
) -> teleportd::session::SessionId {
    let cwd = std::env::temp_dir();
    if let Some(anchor) = &daemon.state.anchor {
        let id = anchor
            .create(
                &teleportd::anchor_client::AnchorSpec {
                    command: "/bin/sh".to_string(),
                    args: args.to_vec(),
                    cwd,
                    env: std::collections::BTreeMap::new(),
                    cols: 80,
                    rows: 24,
                    login_shell: false,
                },
                &std::collections::BTreeMap::new(),
            )
            .await
            .expect("create session in anchord");
        return id.parse().expect("anchord returns a ULID");
    }

    let spec = teleportd::pty::SpawnSpec {
        program: "/bin/sh",
        args,
        cwd: &cwd,
        env: &[],
        cols: 80,
        rows: 24,
        login_shell: false,
    };
    let session = daemon
        .state
        .sessions
        .create(&spec, "shell", None)
        .expect("create session");
    session.id
}
