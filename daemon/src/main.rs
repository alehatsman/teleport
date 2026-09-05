//! `teleportd` — CLI, config, startup sequence, graceful shutdown.
//!
//! M0 bound a listener and generated identity/token with no HTTP routes.
//! **M4** adds the routes: loads `config.toml`/`presets.toml`, builds the
//! `SessionManager` and `AppState`, and mounts `api.rs`'s router
//! (docs/11-mvp-plan.md#m4--http--websocket-api).

use std::fs;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};

use teleportd::api::{build_router, AppState};
use teleportd::auth::{OriginPolicy, TicketStore};
use teleportd::log::LogLimits;
use teleportd::session::{SessionManager, IDLE_SWEEP_INTERVAL_MS, IDLE_THRESHOLD_MS};
use teleportd::{config, now_ms, presets};

const DEFAULT_PORT: u16 = 7337;
/// 256 bits, per docs/06-security.md#the-credential.
const TOKEN_BYTES: usize = 32;

/// teleportd — the teleport session daemon.
#[derive(Parser, Debug)]
#[command(name = "teleportd", version)]
struct Cli {
    /// Address to bind. Must be loopback unless --i-know-what-im-doing is set
    /// (docs/06-security.md#listener).
    #[arg(long, default_value_t = SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)))]
    listen: SocketAddr,

    /// Override the resolved data directory (docs/05-persistence.md).
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// tracing-subscriber filter directive, e.g. "info" or "teleportd=debug".
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Allow binding a non-loopback address. Remote reachability should come
    /// from Tailscale Serve instead (docs/06-security.md#listener).
    #[arg(long)]
    i_know_what_im_doing: bool,

    /// Built SPA assets to serve at `/` (docs/08-packaging.md#build-pipeline).
    /// Relative to the current working directory. Missing is not an error --
    /// the `npm run dev` workflow ([09](../docs/09-frontend.md#dev-workflow))
    /// never touches this path.
    #[arg(long, default_value = "web/dist")]
    web_dist: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Logs go to stderr; stdout carries only the startup URL below, so a
    // script can capture it cleanly.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&cli.log_level))
        .with_writer(std::io::stderr)
        .init();

    if !cli.listen.ip().is_loopback() {
        if !cli.i_know_what_im_doing {
            bail!(
                "refusing to bind non-loopback address {} without --i-know-what-im-doing \
                 (see docs/06-security.md#listener)",
                cli.listen
            );
        }
        warn!(
            addr = %cli.listen,
            "binding a non-loopback address — this host is reachable from the network; \
             use Tailscale Serve instead unless you know what you are doing"
        );
    }

    let data_dir = resolve_data_dir(cli.data_dir.clone())?;
    create_data_dir(&data_dir)?;
    info!(path = %data_dir.display(), "data directory ready");

    let device = teleportd::device::load_or_create(&data_dir)?;
    info!(device_id = %device.device_id, device_name = %device.device_name, "device identity");

    let token = load_or_create_token(&data_dir)?;

    let config = config::Config::load(&data_dir)?;
    let presets = presets::load_or_create(&data_dir)?;

    let sessions_root = data_dir.join("sessions");
    // docs/01-architecture.md#startup-sequence: open SQLite, run migrations,
    // mark stale sessions lost, reconcile output_bytes -- all before binding,
    // so nothing can attach to a session the daemon hasn't finished
    // recovering yet.
    let (db, recovery) =
        teleportd::persistence::Db::open(&data_dir.join("state.db"), &sessions_root)?;
    if recovery.recovered_lost > 0 {
        warn!(
            count = recovery.recovered_lost,
            "sessions recovered as lost after a restart"
        );
    }

    let listener = bind_with_fallback(cli.listen).await?;
    let bound_addr = listener
        .local_addr()
        .context("reading bound local address")?;
    write_port_file(&data_dir, bound_addr.port())?;
    info!(addr = %bound_addr, "teleportd listening");

    println!(
        "http://{}:{}/?token={}",
        bound_addr.ip(),
        bound_addr.port(),
        token
    );

    let log_limits = LogLimits {
        warn_bytes: config.log_warn_bytes,
        max_bytes: config.log_max_bytes,
        ..LogLimits::default()
    };
    let sessions = SessionManager::with_limits(sessions_root.clone(), log_limits)
        .with_max_sessions(config.max_sessions)
        .with_db(db.clone());
    spawn_gc_task(
        db.clone(),
        sessions_root,
        config.retain_days,
        sessions.live_handle(),
    );
    // The Vite dev origin is only ever legitimate against a debug build of
    // this binary itself (docs/06-security.md#browser-origin-defense).
    let origin_policy = OriginPolicy::new(
        bound_addr.port(),
        cfg!(debug_assertions),
        &config.allowed_origins,
        &config.allowed_hosts,
    );

    let web_dist = if cli.web_dist.is_dir() {
        info!(path = %cli.web_dist.display(), "serving web UI");
        Some(cli.web_dist.clone())
    } else {
        // A binary built with `--features embedded-web` still serves the UI
        // from its own baked-in bundle when `--web-dist` doesn't resolve to
        // a real directory (docs/16-release-pipeline.md) -- the log line
        // says which is actually about to happen.
        if cfg!(feature = "embedded-web") {
            info!(path = %cli.web_dist.display(), "no built web UI at this path; serving embedded web UI");
        } else {
            info!(path = %cli.web_dist.display(), "no built web UI at this path; serving API only");
        }
        None
    };

    // The one trigger `POST /api/v1/shutdown` has (docs/11-mvp-plan.md#m10):
    // shared into `AppState` so the handler can wake `shutdown_signal()`
    // below, which otherwise only listens for Ctrl+C / SIGTERM.
    let shutdown_trigger = Arc::new(tokio::sync::Notify::new());

    let state = Arc::new(AppState {
        sessions,
        db: Some(db),
        origin_policy,
        token,
        presets,
        device,
        config,
        started_at: Instant::now(),
        version: env!("CARGO_PKG_VERSION"),
        web_dist,
        shutdown: Arc::clone(&shutdown_trigger),
        ws_tickets: TicketStore::new(),
    });
    spawn_idle_sweep_task(Arc::clone(&state));
    let app = build_router(state);

    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_trigger))
        .await;

    remove_port_file(&data_dir)?;
    serve_result.context("server error")?;
    Ok(())
}

/// Resolves `<data_dir>` per docs/05-persistence.md: `--data-dir` overrides
/// everything; otherwise `BaseDirs::data_local_dir()/teleport`, which is
/// `$XDG_DATA_HOME/teleport` on Linux, `~/Library/Application Support/teleport`
/// on macOS, and `%LOCALAPPDATA%\teleport` on Windows — deliberately the
/// *local* (not roaming) dir on Windows, and with no extra path segment
/// `ProjectDirs` would add.
fn resolve_data_dir(override_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = override_dir {
        return Ok(dir);
    }
    let base = directories::BaseDirs::new()
        .context("could not determine the platform data directory (no home directory?)")?;
    Ok(base.data_local_dir().join("teleport"))
}

/// Creates `<data_dir>` with owner-only permissions (`0700` on Unix).
fn create_data_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating data dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting owner-only permissions on {}", dir.display()))?;
    }
    // TODO(windows): restrict the data dir ACL to the owning user
    // (docs/06-security.md#terminal-logs-are-sensitive). No ACL crate is
    // pinned yet; tracked as a known M0 gap, not silently assumed safe.
    Ok(())
}

/// Loads `<data_dir>/token` if present, otherwise generates 256 bits from the
/// OS CSPRNG and writes it `0600` (docs/06-security.md#the-credential).
fn load_or_create_token(data_dir: &Path) -> Result<String> {
    let path = data_dir.join("token");

    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow::anyhow!("reading OS CSPRNG for token: {e}"))?;
    let token = hex_encode(&bytes);
    write_owner_only(&path, token.as_bytes())?;
    Ok(token)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("writing to a String cannot fail");
    }
    s
}

/// Writes `contents` to `path`, then (on Unix) restricts it to `0600`.
fn write_owner_only(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting owner-only permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Binds `addr`; on `EADDRINUSE` falls back to an ephemeral port on the same
/// IP (docs/08-packaging.md#port-discovery--do-not-hardcode-7337).
async fn bind_with_fallback(addr: SocketAddr) -> Result<TcpListener> {
    match TcpListener::bind(addr).await {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            warn!(addr = %addr, "port in use, falling back to an ephemeral port");
            let fallback = SocketAddr::new(addr.ip(), 0);
            TcpListener::bind(fallback)
                .await
                .with_context(|| format!("binding ephemeral fallback on {}", fallback.ip()))
        }
        Err(e) => Err(e).with_context(|| format!("binding {addr}")),
    }
}

fn port_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("port")
}

fn write_port_file(data_dir: &Path, port: u16) -> Result<()> {
    write_owner_only(&port_file_path(data_dir), port.to_string().as_bytes())
}

/// Removes `<data_dir>/port` on clean shutdown. Not finding it is not an
/// error — the file may already be gone, or never got written.
fn remove_port_file(data_dir: &Path) -> Result<()> {
    let path = port_file_path(data_dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// Runs once at startup and then every 6 hours for the life of the process
/// (docs/05-persistence.md#garbage-collection): every `exited`/`lost` row
/// whose `exited_at_ms` is older than `retain_days` has its directory
/// deleted, then its row -- directory first, so a crash mid-GC leaves a row
/// with no log rather than a log with no row. Detached (`tokio::spawn`, not
/// awaited) -- GC is background housekeeping, not on any request path.
/// D3 (docs/04-api-protocol.md#get-apiv1sessions): the
/// only trigger for "output went quiet" is time passing with nothing
/// happening, so unlike every other `session_events` write this one needs a
/// clock, not a callback. Same shape as `spawn_gc_task` below.
fn spawn_idle_sweep_task(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(IDLE_SWEEP_INTERVAL_MS));
        loop {
            interval.tick().await;
            let now = now_ms();
            for session in state.sessions.list() {
                session.tick_idle(now, IDLE_THRESHOLD_MS);
            }
        }
    });
}

fn spawn_gc_task(
    db: teleportd::persistence::Db,
    sessions_root: PathBuf,
    retain_days: u64,
    live: teleportd::session::LiveSessions,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
        loop {
            interval.tick().await;
            run_gc_pass(&db, &sessions_root, retain_days, &live).await;
        }
    });
}

/// Retention is capped to ~100 years -- `retain_days as i64 * a_day_in_ms`
/// would otherwise wrap `i64` for a large enough `u64` config value (which
/// an operator setting a huge number to mean "keep forever" could plausibly
/// write), landing `cutoff_ms` in the *future* and making every exited/lost
/// row an immediate GC candidate. There is no "retain forever" setting;
/// the config doc should point operators at a large-but-finite number
/// instead.
const MAX_RETAIN_DAYS: u64 = 365 * 100;

async fn run_gc_pass(
    db: &teleportd::persistence::Db,
    sessions_root: &Path,
    retain_days: u64,
    live: &teleportd::session::LiveSessions,
) {
    let retain_days = retain_days.min(MAX_RETAIN_DAYS) as i64;
    let cutoff_ms = now_ms() - retain_days * 24 * 60 * 60 * 1000;
    let candidates = match db.gc_candidates(cutoff_ms).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "listing GC candidates failed");
            return;
        }
    };
    for row in candidates {
        // `SessionManager` still holds this id live (an `exited` row not
        // yet `?purge=true`'d) -- `api.rs`'s `find_session` checks that map
        // first, so every request for it is still served from there, never
        // this row. Deleting the directory now would break `/log` while
        // `GET` keeps returning 200. Skip; retried next pass, and it drops
        // out once the id is actually purged.
        if live.contains(&row.id) {
            continue;
        }
        let dir = sessions_root.join(&row.id);
        // `remove_dir_all` is blocking I/O; off the async worker thread so
        // a large or slow-to-delete directory can't stall other work on it
        // (live PTY reads, WS frame delivery) for the duration.
        let remove_result = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || fs::remove_dir_all(&dir)).await
        };
        match remove_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) if e.kind() == io::ErrorKind::NotFound => {}
            Ok(Err(e)) => {
                warn!(session_id = %row.id, error = %e, "GC: removing session directory failed");
                continue; // row stays -- retried next pass, never deleted without its directory gone first.
            }
            Err(e) => {
                warn!(session_id = %row.id, error = %e, "GC: directory-removal task panicked");
                continue;
            }
        }
        if let Err(e) = db.delete_session(&row.id).await {
            warn!(session_id = %row.id, error = %e, "GC: deleting session row failed");
        }
    }
}

/// Resolves once Ctrl+C, (Unix only) SIGTERM, or an authenticated
/// `POST /api/v1/shutdown` (docs/11-mvp-plan.md#m10) is received. The HTTP
/// trigger exists mainly for Windows, which has no SIGTERM equivalent
/// reachable from a console-less daemon, but is wired in on every platform
/// -- one uniform, curl-testable path rather than a Windows-only special
/// case (`api.rs`'s `shutdown` handler doc comment has the full reasoning).
async fn shutdown_signal(shutdown_trigger: Arc<tokio::sync::Notify>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                warn!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
        _ = shutdown_trigger.notified() => {}
    }
    info!("shutdown signal received");
}
