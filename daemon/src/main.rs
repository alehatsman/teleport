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
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};

use teleportd::api::{build_router, AppState};
use teleportd::auth::OriginPolicy;
use teleportd::log::LogLimits;
use teleportd::session::SessionManager;
use teleportd::{config, presets};

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

    let listener = bind_with_fallback(cli.listen).await?;
    let bound_addr = listener.local_addr().context("reading bound local address")?;
    write_port_file(&data_dir, bound_addr.port())?;
    info!(addr = %bound_addr, "teleportd listening");

    println!("http://{}:{}/?token={}", bound_addr.ip(), bound_addr.port(), token);

    let log_limits = LogLimits {
        warn_bytes: config.log_warn_bytes,
        max_bytes: config.log_max_bytes,
        ..LogLimits::default()
    };
    let sessions = SessionManager::with_limits(data_dir.join("sessions"), log_limits)
        .with_max_sessions(config.max_sessions);
    // The Vite dev origin is only ever legitimate against a debug build of
    // this binary itself (docs/06-security.md#browser-origin-defense).
    let origin_policy =
        OriginPolicy::new(bound_addr.port(), cfg!(debug_assertions), &config.allowed_origins, &config.allowed_hosts);

    let web_dist = if cli.web_dist.is_dir() {
        info!(path = %cli.web_dist.display(), "serving web UI");
        Some(cli.web_dist.clone())
    } else {
        info!(path = %cli.web_dist.display(), "no built web UI at this path; serving API only");
        None
    };

    let state = Arc::new(AppState {
        sessions,
        origin_policy,
        token,
        presets,
        device,
        config,
        started_at: Instant::now(),
        version: env!("CARGO_PKG_VERSION"),
        web_dist,
    });
    let app = build_router(state);

    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
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

/// Resolves once Ctrl+C or (Unix only) SIGTERM is received.
async fn shutdown_signal() {
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
    }
    info!("shutdown signal received");
}
