//! `teleport` -- the CLI client (docs/11-mvp-plan.md#m11--cli-client). A
//! plain `/api/v1` caller: nothing it does is unavailable to curl or the
//! web UI (docs/04-api-protocol.md).

mod attach;
mod connect;
mod http;
mod identity;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// teleport -- attach to and manage sessions on a teleportd daemon.
#[derive(Parser, Debug)]
#[command(name = "teleport", version)]
struct Cli {
    /// Daemon base URL, e.g. https://mainpc.tail1234.ts.net. Defaults to
    /// local-daemon auto-discovery (docs/11-mvp-plan.md#m11's connection
    /// resolution).
    #[arg(long, global = true)]
    url: Option<String>,

    /// Bearer token. Falls back to TELEPORT_TOKEN, then
    /// `<data_dir>/token` when neither `--url` nor `--token` is given.
    #[arg(long, global = true)]
    token: Option<String>,

    /// Override the resolved data directory (docs/05-persistence.md), same
    /// flag `teleportd` itself takes.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List sessions.
    Sessions,
    /// Create a session.
    New {
        /// Preset id from `teleport presets` / `presets.toml`.
        #[arg(long)]
        preset: Option<String>,
        /// Executable to run (resolved via PATH on the daemon's host).
        #[arg(long)]
        cmd: Option<String>,
        /// Working directory. Defaults to the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Extra arguments, after `--`.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// List available presets.
    Presets,
    /// Attach to a session -- puts this terminal in raw mode and bridges it
    /// to the session 1:1, byte for byte.
    Attach { id: String },
    /// Terminate a session.
    Kill {
        id: String,
        /// Also delete its log and remove it from the list once exited.
        #[arg(long)]
        purge: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            std::env::var("TELEPORT_LOG").unwrap_or_else(|_| "warn".to_string()),
        ))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let conn = connect::resolve(cli.url, cli.token, cli.data_dir.clone())?;

    match cli.command {
        Command::Sessions => sessions(&conn).await,
        Command::New {
            preset,
            cmd,
            cwd,
            args,
        } => new_session(&conn, preset, cmd, cwd, args).await,
        Command::Presets => presets(&conn).await,
        Command::Attach { id } => {
            let data_dir = connect::data_dir(cli.data_dir)?;
            let client_id = identity::client_id(&data_dir);
            let client_name = identity::default_client_name();
            let code = attach::run(&conn, &id, &client_id, &client_name).await?;
            std::process::exit(code);
        }
        Command::Kill { id, purge } => kill(&conn, &id, purge).await,
    }
}

fn print_api_err(e: &http::ApiError) -> anyhow::Error {
    let mut msg = e.to_string();
    if let Some(hint) = e.hint() {
        msg.push('\n');
        msg.push_str(hint);
    }
    anyhow::anyhow!(msg)
}

async fn sessions(conn: &connect::Connection) -> Result<()> {
    let client = http::Client::new(conn)?;
    let sessions = client
        .list_sessions()
        .await
        .map_err(|e| print_api_err(&e))?;
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    println!(
        "{:<28} {:<10} {:<8} {:<20} {:<16} EXIT",
        "ID", "STATE", "SIZE", "COMMAND", "CONTROLLER"
    );
    for s in sessions {
        let size = format!("{}x{}", s.cols, s.rows);
        let controller = s.controller.unwrap_or_else(|| "-".to_string());
        let exit = s
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string());
        let command = s.preset.unwrap_or(s.command);
        println!(
            "{:<28} {:<10} {:<8} {:<20} {:<16} {}",
            s.id, s.state, size, command, controller, exit
        );
    }
    Ok(())
}

async fn presets(conn: &connect::Connection) -> Result<()> {
    let client = http::Client::new(conn)?;
    let presets = client.presets().await.map_err(|e| print_api_err(&e))?;
    for p in presets {
        println!("{:<16} {}", p.id, p.label);
    }
    Ok(())
}

async fn new_session(
    conn: &connect::Connection,
    preset: Option<String>,
    cmd: Option<String>,
    cwd: Option<PathBuf>,
    args: Vec<String>,
) -> Result<()> {
    let cwd = match cwd {
        Some(c) => c,
        None => std::env::current_dir().context("resolving the current directory")?,
    };
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    // kind mirrors the shape presets.toml examples use
    // (docs/04-api-protocol.md#post-apiv1sessions): a preset implies
    // "agent", an explicit command with no preset is "command", and
    // neither means "shell" -- the same three-way split the reference UI
    // makes when a user picks nothing.
    let kind = if preset.is_some() {
        "agent"
    } else if cmd.is_some() {
        "command"
    } else {
        "shell"
    };
    let command = cmd.or_else(|| {
        if preset.is_none() {
            Some(default_shell())
        } else {
            None
        }
    });
    let req = http::CreateSessionRequest {
        kind,
        preset,
        command,
        args: if args.is_empty() { None } else { Some(args) },
        cwd: cwd.display().to_string(),
        cols,
        rows,
        env: HashMap::new(),
    };

    let client = http::Client::new(conn)?;
    let resp = client
        .create_session(&req)
        .await
        .map_err(|e| print_api_err(&e))?;
    println!("{}", resp.id);
    Ok(())
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

async fn kill(conn: &connect::Connection, id: &str, purge: bool) -> Result<()> {
    let client = http::Client::new(conn)?;
    client
        .delete_session(id, purge)
        .await
        .map_err(|e| print_api_err(&e))?;
    Ok(())
}
