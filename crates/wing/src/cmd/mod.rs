//! CLI subcommands and dispatch.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::app::run_app;
use crate::config::AppConfig;
use crate::gateway::GatewayClient;
use crate::tui;
use crate::util::logging::init_logging;

mod discover;
mod start;
mod state;
mod status;
mod stop;

/// wing — AI agent CLI
#[derive(Parser, Debug)]
#[command(
    version,
    long_version = concat!(
        env!("CARGO_PKG_VERSION"),
        " (", env!("WING_COMMIT_HASH"), ") ",
        "built ", env!("WING_BUILD_TIME"), " ",
        "[", env!("WING_TARGET"), "]",
    ),
    about,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Available subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch the TUI frontend (default when no subcommand given).
    Tui {
        /// Gateway WebSocket URL.
        #[arg(long, default_value = "ws://127.0.0.1:32523/ws")]
        gateway_url: String,

        /// Dump default configuration to stdout and exit.
        #[arg(long)]
        dump_config: bool,
    },

    /// Start the gateway daemon in the background.
    Start {
        /// Gateway host (default: from config).
        #[arg(long)]
        host: Option<String>,

        /// Gateway port (default: from config).
        #[arg(long)]
        port: Option<u16>,
    },

    /// Stop the gateway daemon.
    Stop,

    /// Show gateway daemon status.
    Status,
}

/// Dispatch CLI command.
pub async fn dispatch(cli: Cli) -> ExitCode {
    match cli.command {
        Some(cmd) => match cmd {
            Command::Tui {
                gateway_url,
                dump_config,
            } => {
                if dump_config {
                    let config = AppConfig::default();
                    print!("{}", config.to_yaml());
                    ExitCode::SUCCESS
                } else {
                    match run_tui(&gateway_url).await {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            eprintln!("wing error: {e}");
                            ExitCode::FAILURE
                        }
                    }
                }
            }
            Command::Start { host, port } => {
                let config = AppConfig::load();
                let host = host.unwrap_or_else(|| config.gateway.host.clone());
                let port = port.unwrap_or(config.gateway.port);
                match start::start_gateway(&host, port) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("wing start error: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            Command::Stop => match stop::stop_gateway() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("wing stop error: {e}");
                    ExitCode::FAILURE
                }
            },
            Command::Status => {
                status::show_status();
                ExitCode::SUCCESS
            }
        },
        None => {
            // Smart default: auto-start gateway if needed, then enter TUI.
            match smart_default_tui().await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("wing error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

/// Smart default: check if gateway is running, start if not, then enter TUI.
async fn smart_default_tui() -> Result<()> {
    let state = state::WingState::load();

    let gateway_url = if let Some(ref gw) = state.gateway {
        if state::is_gateway_running(gw) {
            // Gateway already running, use its endpoint.
            format!("ws://{}:{}/ws", gw.host, gw.port)
        } else {
            // Stale state, gateway not running. Clear and auto-start.
            let host = gw.host.clone();
            let port = gw.port;
            state::WingState::clear_gateway();
            start::start_gateway(&host, port)?;
            format!("ws://{host}:{port}/ws")
        }
    } else {
        // No state file, gateway not running. Auto-start from config.
        let config = AppConfig::load();
        let host = &config.gateway.host;
        let port = config.gateway.port;
        start::start_gateway(host, port)?;
        format!("ws://{host}:{port}/ws")
    };

    run_tui(&gateway_url).await
}

/// Launch TUI: connect to gateway, init terminal, run app.
async fn run_tui(gateway_url: &str) -> Result<()> {
    // Initialize logging (file only, no console output).
    let _log_guard = init_logging();

    tracing::info!("wing starting, gateway: {gateway_url}");

    // Get current working directory as workspace.
    let workspace = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    // Connect to Gateway before entering the TUI.
    let gateway = GatewayClient::connect(gateway_url, workspace.as_deref())
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to connect to gateway at {gateway_url}: {e}\n\
                 Make sure the gateway is running: wing start"
            )
        })?;

    let session_id = gateway.session_id().to_string();
    tracing::info!(session_id = %session_id, "connected, entering TUI");

    // Load user configuration.
    let config = AppConfig::load();

    // Initialize terminal.
    let mut terminal = tui::init_terminal()?;

    // Set up panic hook to restore terminal on panic.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show
        );
        original_hook(panic_info);
    }));

    // Run the app.
    let result = run_app(&mut terminal, gateway, session_id, config).await;

    // Restore terminal.
    tui::restore_terminal(&mut terminal)?;

    // Restore original panic hook.
    let _ = std::panic::take_hook();

    result?;
    tracing::info!("wing exited cleanly");
    Ok(())
}
