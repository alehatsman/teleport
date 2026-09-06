// Prevents an additional console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Teleport desktop shell -- the M10 milestone
//! (docs/11-mvp-plan.md#m10--tauri-shell, docs/08-packaging.md).
//!
//! What this process is allowed to do
//! (docs/08-packaging.md#what-tauri-is-allowed-to-do): open the UI,
//! start/attach `teleportd`, poll its health, tray + notifications +
//! updates, nothing that owns a PTY or session or speaks a private RPC.
//!
//! The webview loads the daemon's own web app over plain HTTP -- the same
//! thing a phone's browser gets.

mod autostart;
mod daemon;
mod updater;

use std::path::Path;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::{error, info, warn};

/// docs/08-packaging.md's flow diagram: "poll for the port file, then
/// /health, up to 10 s" after spawning.
const STARTUP_POLL_TIMEOUT: Duration = Duration::from_secs(10);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(300);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    tauri::Builder::default()
        // Must be first -- tauri-plugin-single-instance's own requirement,
        // so it runs before anything else can interfere.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            info!("second instance launched; focusing the existing window");
            show_or_recheck(app.clone());
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            build_tray(app.handle())?;
            show_or_recheck(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Teleport desktop shell");
}

/// Entry point for both first launch and "focus the existing window"
/// (single-instance relaunch, tray "Open Teleport"): re-probes rather than
/// trusting cached state, since the daemon can appear or disappear out from
/// under this app (docs/08-packaging.md's ownership model has no notion of
/// this shell being the source of truth).
fn show_or_recheck(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = startup(&app).await {
            error!(error = %e, "startup flow failed");
        }
    });
}

async fn startup(app: &AppHandle) -> anyhow::Result<()> {
    let dir = daemon::data_dir()?;
    match daemon::probe(&dir).await {
        daemon::Probe::Ours(health) => {
            info!(
                version = %health.version,
                device_name = health.device_name.as_deref().unwrap_or("?"),
                sessions_running = health.sessions_running.unwrap_or(0),
                "attaching to our daemon"
            );
            open_main_window(app, &dir)?;
        }
        daemon::Probe::NotOurs => {
            // Per docs/08-packaging.md: something is listening but didn't
            // accept our token. Do not attach to a stranger's daemon.
            warn!(
                "a daemon is listening on our port but did not accept our token \
                 -- not attaching (docs/08-packaging.md#daemon-lifecycle----the-important-part)"
            );
            show_not_ours_dialog(app);
        }
        daemon::Probe::NoDaemon => {
            info!("no daemon reachable; starting teleportd detached");
            daemon::spawn_detached(&dir)?;
            if poll_until_up(&dir).await {
                open_main_window(app, &dir)?;
            } else {
                error!(
                    timeout_ms = STARTUP_POLL_TIMEOUT.as_millis() as u64,
                    "teleportd did not come up in time after spawning"
                );
                show_startup_timeout_dialog(app, &dir);
            }
        }
    }
    Ok(())
}

/// Issue #15's first case: a daemon answered `/health` but not with our
/// token's shape (docs/08-packaging.md#daemon-lifecycle----the-important-part).
/// `blocking_show` mirrors [`stop_daemon_flow`]'s existing use of the same
/// dialog plugin -- both only ever run inside a task spawned via
/// `tauri::async_runtime::spawn`, never on tauri's own event-loop thread, so
/// blocking that task is fine.
fn show_not_ours_dialog(app: &AppHandle) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

    app.dialog()
        .message(
            "A daemon is already running on Teleport's expected port, but it \
             didn't accept our token, so this isn't the daemon this app \
             manages. Not attaching, rather than risk taking over someone \
             else's process.\n\n\
             This usually means another OS user has a Teleport daemon \
             running, or this app's saved token is stale. Stop that other \
             daemon (or clear the stale token) and reopen Teleport.",
        )
        .title("Can't attach to daemon")
        .kind(MessageDialogKind::Error)
        .blocking_show();
}

/// Issue #15's second case: `teleportd` didn't come up within
/// [`STARTUP_POLL_TIMEOUT`] of being spawned. Shows the tail of its log
/// (`daemon::log_path`/`read_log_tail` -- the only persisted copy of its
/// stdout/stderr, since `spawn_detached` redirects there instead of to
/// `/dev/null`) and offers a retry, per docs/08-packaging.md's flow diagram.
fn show_startup_timeout_dialog(app: &AppHandle, dir: &Path) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let log_path = daemon::log_path(dir);
    let tail = daemon::read_log_tail(dir, 4000);
    let message = format!(
        "teleportd didn't start within {}s.\n\nLog ({}):\n\n{tail}",
        STARTUP_POLL_TIMEOUT.as_secs(),
        log_path.display(),
    );

    let retry = app
        .dialog()
        .message(message)
        .title("Teleport daemon didn't start")
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Retry".into(),
            "Close".into(),
        ))
        .blocking_show();
    if retry {
        show_or_recheck(app.clone());
    }
}

async fn poll_until_up(dir: &Path) -> bool {
    let deadline = tokio::time::Instant::now() + STARTUP_POLL_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if matches!(daemon::probe(dir).await, daemon::Probe::Ours(_)) {
            return true;
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    }
    false
}

/// Opens (or focuses/shows) the main window, pointed at the daemon's own
/// served UI -- `http://127.0.0.1:<port>/?token=<token>`, the same URL
/// shape `daemon/src/main.rs` prints at its own startup. No bundled
/// frontend, no `frontendDist`: this window's content comes entirely from
/// the running daemon, browser-only-mode's exact experience
/// (docs/08-packaging.md#browser-only-mode-is-a-first-class-deployment).
fn open_main_window(app: &AppHandle, dir: &Path) -> anyhow::Result<()> {
    if let Some(w) = app.get_webview_window("main") {
        w.show()?;
        w.set_focus()?;
        return Ok(());
    }

    let port = daemon::read_port(dir)
        .ok_or_else(|| anyhow::anyhow!("no port file despite a successful health probe"))?;
    let url = match daemon::read_token(dir) {
        Some(token) => format!("http://127.0.0.1:{port}/?token={token}"),
        None => format!("http://127.0.0.1:{port}/"),
    };

    // `WebviewUrl::External`, not a bundled `frontendDist` -- this window is
    // just a webview navigated to the daemon's own server, the same page a
    // browser would load. `tauri.conf.json`'s `security.csp` only injects a
    // policy into pages Tauri itself serves over its `tauri://` asset
    // protocol, so it has no effect here and is deliberately left `null`;
    // the CSP this page actually gets is the daemon's own response header
    // (docs/06-security.md#add-a-strict-content-security-policy, `api.rs`'s
    // `CONTENT_SECURITY_POLICY`), same as it is for a browser client.
    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse()?))
        .title("Teleport")
        .inner_size(1100.0, 720.0)
        .build()?;

    // Closing the window hides it; it does not stop the daemon, and it
    // does not exit this shell -- the tray is still there to reopen it or
    // to actually quit (docs/08-packaging.md's daemon-lifecycle rule).
    let hide_handle = window.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = hide_handle.hide();
        }
    });
    Ok(())
}

// On Linux, install()/uninstall() also toggle `loginctl` lingering
// (autostart/linux.rs), so the daemon keeps running with nobody logged in
// at all -- not just "at login" like macOS/Windows. Label it accurately
// there instead of letting the tray understate what the toggle now does.
#[cfg(target_os = "linux")]
const AUTOSTART_ON_LABEL: &str = "Start automatically (even after reboot)";
#[cfg(target_os = "linux")]
const AUTOSTART_OFF_LABEL: &str = "Don't start automatically";

#[cfg(not(target_os = "linux"))]
const AUTOSTART_ON_LABEL: &str = "Start at login";
#[cfg(not(target_os = "linux"))]
const AUTOSTART_OFF_LABEL: &str = "Don't start at login";

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Teleport", true, None::<&str>)?;
    let logs = MenuItem::with_id(app, "logs", "Show data folder", true, None::<&str>)?;
    let autostart_on =
        MenuItem::with_id(app, "autostart_on", AUTOSTART_ON_LABEL, true, None::<&str>)?;
    let autostart_off = MenuItem::with_id(
        app,
        "autostart_off",
        AUTOSTART_OFF_LABEL,
        true,
        None::<&str>,
    )?;
    let stop_daemon = MenuItem::with_id(app, "stop_daemon", "Stop daemon…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Teleport", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &logs,
            &autostart_on,
            &autostart_off,
            &stop_daemon,
            &quit,
        ],
    )?;

    TrayIconBuilder::new()
        .menu(&menu)
        // A tray icon needs its own fixed-size image -- `default_window_icon()`
        // returned whatever `bundle.icon` entry the platform picked for the
        // *window*, which doesn't match the tray backend's expected buffer
        // size ("wrong data size, expected 4096 got 8192" on Linux, caught by
        // actually running this scaffold rather than assumed). Load the
        // 32x32 icon explicitly instead.
        .icon(tauri::include_image!("icons/32x32.png"))
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_or_recheck(app.clone()),
            "logs" => {
                if let Ok(dir) = daemon::data_dir() {
                    let _ = tauri_plugin_opener::open_path(dir, None::<&str>);
                }
            }
            "autostart_on" => {
                if let Err(e) = autostart::install() {
                    error!(error = %e, "installing autostart");
                }
            }
            "autostart_off" => {
                if let Err(e) = autostart::uninstall() {
                    error!(error = %e, "uninstalling autostart");
                }
            }
            "stop_daemon" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move { stop_daemon_flow(&app).await });
            }
            "quit" => {
                // Quits this shell only. The daemon is a separate process
                // by construction (docs/08-packaging.md) and is untouched.
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// "Stop daemon…" -- the one tray action that actually ends running
/// sessions, so it always confirms and always names the count
/// (docs/08-packaging.md, docs/11-mvp-plan.md#m10 edge cases).
async fn stop_daemon_flow(app: &AppHandle) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let dir = match daemon::data_dir() {
        Ok(d) => d,
        Err(e) => {
            error!(error = %e, "resolving data dir for stop-daemon");
            return;
        }
    };
    let health = match daemon::probe(&dir).await {
        daemon::Probe::Ours(h) => h,
        _ => {
            info!("stop-daemon requested but no daemon of ours is running");
            return;
        }
    };
    let sessions = health.sessions_running.unwrap_or(0);
    let Some(pid) = health.pid else {
        error!("authenticated /health had no pid; refusing to guess one");
        return;
    };

    let message = if sessions == 0 {
        "No sessions are running. Stop the Teleport daemon?".to_string()
    } else {
        format!(
            "{sessions} session(s) are running and will be lost. Stop the Teleport daemon anyway?"
        )
    };

    let confirmed = app
        .dialog()
        .message(message)
        .title("Stop Teleport daemon")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Stop".into(),
            "Cancel".into(),
        ))
        .blocking_show();
    if !confirmed {
        return;
    }

    #[cfg(unix)]
    {
        if let Err(e) = daemon::terminate_gracefully(pid) {
            error!(error = %e, pid, "failed to stop daemon");
        }
    }
    #[cfg(windows)]
    {
        // Windows has no console-ctrl-event path to a console-less,
        // autostart-launched teleportd, so this goes over HTTP instead of a
        // signal -- see `daemon::shutdown_gracefully`'s doc comment (issue
        // #12, docs/11-mvp-plan.md#m10). `pid` isn't needed here (the
        // request is authenticated by token, not by process identity); it
        // was only ever used to name the SIGTERM target on Unix.
        let _ = pid;
        if let Err(e) = daemon::shutdown_gracefully(&dir).await {
            error!(error = %e, "failed to stop daemon");
        }
    }
}
