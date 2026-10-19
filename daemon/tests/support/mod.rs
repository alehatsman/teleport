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
//! It boots a real daemon on a loopback port (docs/10-testing.md#3-protocol-tests)
//! whose sessions live in one `anchord` per test binary, started from
//! `ANCHORD_BIN`. There is no other session path to boot: the in-process PTY
//! layer is gone (docs/20). Each consumer uses a different subset, and each
//! is compiled as its own test binary -- hence the blanket `dead_code` allow
//! rather than per-item ones.
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
use teleportd::web_assets::WebAssets;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

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
    endpoint: PathBuf,
}

impl Anchord {
    /// `None` unless `ANCHORD_BIN` is set. Every fixture that creates a
    /// session then fails loudly against an unreachable endpoint rather than
    /// passing by not running (#108): anchord is a requirement, not an option.
    fn start_if_requested() -> Option<Self> {
        let raw = std::env::var_os("ANCHORD_BIN")?;
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
        let dir = anchord_endpoint::scratch_root().join(format!("tp-anchor-{stem}"));
        let endpoint = anchord_endpoint::endpoint_in(&dir, &stem);

        // A daemon already listening here is reused as-is.
        if anchord_endpoint::answers(&endpoint) {
            return Some(Self {
                child: None,
                dir,
                endpoint,
            });
        }

        // Otherwise start fresh: whatever is there is a dead daemon's
        // leftovers, and reusing its database would carry an earlier run's
        // sessions into this one.
        drop(std::fs::remove_dir_all(&dir));
        std::fs::create_dir_all(dir.join("run")).expect("creating the data dir");
        // Captured, so a daemon that dies on startup says why instead of
        // being diagnosed as "slow" by a timeout.
        let log = std::fs::File::create(dir.join("anchord.log")).expect("creating the log");
        let child = anchord_endpoint::command(&bin, &dir, &endpoint)
            .env("ANCHOR_LOG", "info")
            .stderr(std::process::Stdio::from(log))
            .spawn()
            .expect("spawning anchord");
        Some(Self {
            child: Some(child),
            dir,
            endpoint,
        })
    }

    fn socket(&self) -> PathBuf {
        self.endpoint.clone()
    }

    /// Blocks until the endpoint accepts, so a fixture never races startup.
    ///
    /// Generous, because every fixture starts its own anchord and a full
    /// parallel run starts dozens at once. The wait is for a process to
    /// bind a socket or create a pipe, so a slow one here means a loaded
    /// machine, not a broken daemon.
    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !anchord_endpoint::answers(&self.endpoint) {
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

/// Where a test anchord lives, and how to tell whether it is up -- the
/// platform half of [`Anchord`], kept apart so the fixture logic above is
/// written once.
///
/// Two traps this exists to avoid, both from #107. Reaching for
/// `std::os::unix::net` directly made the whole module Unix-only without a
/// gate saying so; and a check that flips cfg flags on a Unix host cannot
/// see that, because the Unix API is real there whatever the flags say.
pub(crate) mod anchord_endpoint {
    use std::path::{Path, PathBuf};

    /// Where fixture directories go. `/tmp` on Unix rather than
    /// `temp_dir()`, because macOS's `$TMPDIR` is long enough to push a
    /// socket path past `sun_path`'s ~104 bytes.
    pub(crate) fn scratch_root() -> PathBuf {
        #[cfg(unix)]
        {
            PathBuf::from("/tmp")
        }
        #[cfg(windows)]
        {
            std::env::temp_dir()
        }
    }

    /// The endpoint a test anchord rooted at `dir` listens on.
    #[cfg_attr(
        unix,
        expect(unused_variables, reason = "a pipe is named, a socket is placed")
    )]
    pub(crate) fn endpoint_in(dir: &Path, tag: &str) -> PathBuf {
        #[cfg(unix)]
        {
            dir.join("run/anchor.sock")
        }
        // A pipe name, not a path under `dir`: the pipe namespace is flat
        // and machine-global, which is why it carries a tag and this
        // process's user is the only one that can open it (anchor's
        // `docs/09-windows.md` §3.2).
        #[cfg(windows)]
        {
            let _ = dir;
            PathBuf::from(format!(r"\\.\pipe\tp-anchor-{tag}"))
        }
    }

    /// Whether something accepts a connection on `endpoint` right now.
    pub(crate) fn answers(endpoint: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::net::UnixStream::connect(endpoint).is_ok()
        }
        // Opening the pipe is connecting to it. `ERROR_PIPE_BUSY` (231)
        // means every instance is taken, which is a server that exists.
        #[cfg(windows)]
        {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(endpoint)
            {
                Ok(_) => true,
                Err(e) => e.raw_os_error() == Some(231),
            }
        }
    }

    /// A command that starts an anchord with `dir` for its data and
    /// `endpoint` for its listener. `anchord` is `ANCHORD_BIN`.
    ///
    /// Unix moves both with the XDG variables, as anchor resolves them.
    ///
    /// Windows cannot: anchor's pipe name is fixed per user and no
    /// environment variable moves it (its `docs/04-protocol.md`), and
    /// `anchord` takes no arguments. The one endpoint override is
    /// `anchor daemon --socket <pipe>`, which runs anchord's own startup, so
    /// that is what a Windows fixture starts -- the `anchor` CLI found next
    /// to `ANCHORD_BIN` (one `cargo build` produces both), or `ANCHOR_BIN`
    /// when set. Data goes where anchor's Windows resolution puts it,
    /// `%LOCALAPPDATA%\anchor`, so `LOCALAPPDATA` is pointed at `dir`.
    pub(crate) fn command(anchord: &Path, dir: &Path, endpoint: &Path) -> std::process::Command {
        #[cfg(unix)]
        {
            let _ = endpoint;
            let mut command = std::process::Command::new(anchord);
            command
                .env("XDG_DATA_HOME", dir)
                .env("XDG_RUNTIME_DIR", dir.join("run"));
            command
        }
        #[cfg(windows)]
        {
            let cli = std::env::var_os("ANCHOR_BIN")
                .map_or_else(|| anchord.with_file_name("anchor.exe"), PathBuf::from);
            assert!(
                cli.is_file(),
                "a Windows fixture starts anchord as `anchor daemon --socket`, and there is no anchor CLI at {} (set ANCHOR_BIN)",
                cli.display()
            );
            let mut command = std::process::Command::new(cli);
            command
                .arg("--socket")
                .arg(endpoint)
                .arg("daemon")
                .env("LOCALAPPDATA", dir);
            command
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
        (Some(client), _) => client,
        (None, Some(socket)) => {
            wipe_shared_daemon(socket.as_path()).await;
            config.anchor_owner = format!("test-{}", next_owner_id());
            teleportd::anchor_client::AnchorClient::new(socket)
        }
        // No ANCHORD_BIN: an endpoint nothing listens on. Fixtures that never
        // touch a session (static assets, shutdown, health) still run; any
        // that do get the 503 an absent anchord deserves.
        (None, None) => teleportd::anchor_client::AnchorClient::new(anchord_endpoint::endpoint_in(
            &anchord_endpoint::scratch_root().join("tp-no-anchord"),
            "no-anchord",
        )),
    };

    // Fixtures get the supervisor too, so the anchor path they exercise is
    // the one that ships rather than a quieter variant of it.
    let watch = Arc::new(teleportd::anchor_watch::WatchRegistry::new());
    teleportd::anchor_watch::spawn_supervisor(
        anchor.clone(),
        Arc::clone(&watch),
        config.anchor_owner.clone(),
        db.clone(),
    );

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

/// A session's authoritative output offset, as anchord reports it.
pub(crate) async fn session_offset(daemon: &Daemon, id: teleportd::session::SessionId) -> u64 {
    daemon
        .state
        .anchor
        .get(&id.to_string())
        .await
        .expect("anchord knows the session")
        .output_bytes
}

/// Creates a `/bin/sh` session straight in anchord (bypassing HTTP) -- the
/// fastest way for a WS-focused test to get a session id to attach to.
pub(crate) async fn create_shell_session(
    daemon: &Daemon,
    args: &[String],
) -> teleportd::session::SessionId {
    let cwd = std::env::temp_dir();
    let id = daemon
        .state
        .anchor
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
    id.parse().expect("anchord returns a ULID")
}

/// The shell an anchor fixture spawns its scripts in, per platform.
///
/// `/bin/sh -c` on Unix. `PowerShell` on Windows rather than `cmd.exe`,
/// because the fixtures need to write raw control bytes (OSC titles, BEL)
/// and `[Console]::Write([char]27 ...)` says so legibly where `cmd`'s
/// `echo` cannot carry them at all. Every Windows script here was run
/// through teleport's own `ConPTY` path (`pty::spawn`) on Windows 11 build
/// 26200 before being written down, 2026-10-19 -- see the `pick` call sites
/// for what that showed.
pub(crate) mod fixture_shell {
    /// The program a fixture session runs.
    #[cfg(unix)]
    pub(crate) const PROGRAM: &str = "/bin/sh";
    /// The program a fixture session runs.
    #[cfg(windows)]
    pub(crate) const PROGRAM: &str = "powershell.exe";

    /// `PROGRAM`'s argv for one script.
    pub(crate) fn args(script: &str) -> Vec<String> {
        #[cfg(unix)]
        let prefix: &[&str] = &["-c"];
        #[cfg(windows)]
        let prefix: &[&str] = &["-NoProfile", "-NonInteractive", "-Command"];
        prefix
            .iter()
            .map(|s| (*s).to_owned())
            .chain(std::iter::once(script.to_owned()))
            .collect()
    }

    /// A directory every fixture may spawn in.
    pub(crate) fn cwd() -> String {
        #[cfg(unix)]
        {
            "/tmp".to_owned()
        }
        #[cfg(windows)]
        {
            std::env::temp_dir().display().to_string()
        }
    }

    /// The script for this platform: one written for `/bin/sh`, one for
    /// PowerShell, chosen at compile time so each file states both side by
    /// side.
    #[cfg_attr(unix, expect(unused_variables, reason = "the other platform's script"))]
    #[cfg_attr(
        windows,
        expect(unused_variables, reason = "the other platform's script")
    )]
    pub(crate) fn pick<'a>(unix: &'a str, windows: &'a str) -> &'a str {
        #[cfg(unix)]
        {
            unix
        }
        #[cfg(windows)]
        {
            windows
        }
    }
}
