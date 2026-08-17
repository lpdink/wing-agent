//! CLI subcommands and dispatch.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::app::run_app;
use crate::app::transport::Transport;
use crate::config::AppConfig;
use crate::gateway::GatewayClient;
use crate::tui;
use crate::util::logging::init_logging;
use wing_api_client::GatewayClient as GatewayApiClient;

pub mod args;
pub(crate) mod backend_config;
pub mod common;
mod discover;
pub mod messages;
pub mod ps;
pub mod query;
pub mod run;
pub(crate) mod start;
mod status;
mod stop;
pub mod wait;

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

    // ---- stdio mode arguments ----
    /// Prompt text (triggers stdio mode).
    #[arg(short = 'p', long = "prompt")]
    pub prompt: Option<String>,

    /// Override model name.
    #[arg(short = 'm', long = "model")]
    pub model: Option<String>,

    /// Override provider name (references config `providers[].name`).
    #[arg(long = "provider")]
    pub provider: Option<String>,

    /// Override tools (comma-separated). If not set, uses template defaults.
    #[arg(long = "tools")]
    pub tools: Option<String>,

    /// Resume an existing session by ID.
    #[arg(short = 'r', long = "resume")]
    pub resume: Option<String>,

    /// Replace system prompt.
    #[arg(long = "system-prompt")]
    pub system_prompt: Option<String>,

    /// Append to system prompt.
    #[arg(long = "append-system-prompt")]
    pub append_system_prompt: Option<String>,

    /// Maximum agent loop turns.
    #[arg(long = "max-turns")]
    pub max_turns: Option<u32>,

    /// Reasoning effort level.
    #[arg(long = "effort")]
    pub effort: Option<String>,

    /// Output format: text (default), json, stream-json.
    #[arg(long = "output-format", default_value = "text")]
    pub output_format: String,

    /// Input format: text (default), stream-json.
    #[arg(long = "input-format", default_value = "text")]
    pub input_format: String,

    /// Skip dangerous command review (YOLO mode).
    #[arg(long = "yolo")]
    pub yolo: bool,

    // ---- global output flags (available to all subcommands) ----
    /// Output JSON (for agent consumption). Default is table/text.
    #[arg(global = true, long = "json")]
    pub json: bool,

    /// Watch mode: refresh every 2 seconds.
    #[arg(global = true, short = 'w', long = "watch")]
    pub watch: bool,
}

impl Cli {
    /// Check if this CLI invocation triggers stdio mode.
    ///
    /// Must stay in sync with `stdio::is_stdio_mode()` in `stdio/mod.rs`.
    /// See that function's doc comment for the contract.
    pub fn is_stdio_mode(&self) -> bool {
        self.prompt.is_some() || self.input_format == "stream-json"
    }
}

/// Available subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch the TUI frontend (default when no subcommand given).
    Tui {
        /// Gateway host (default: from backend config).
        #[arg(long)]
        host: Option<String>,

        /// Gateway port (default: from backend config).
        #[arg(long)]
        port: Option<u16>,

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

    /// Launch a task in the background (non-blocking).
    ///
    /// Creates a session, sends the prompt, and returns immediately
    /// with the session ID. Use `wing wait` to block until completion.
    Run(args::RunArgs),

    /// Block until specified sessions finish.
    ///
    /// Accepts multiple session IDs. Polls session status via HTTP
    /// and listens for TurnResult events via WebSocket. Returns when
    /// all sessions reach a terminal state (idle/inactive).
    Wait {
        /// Session IDs to wait for (space-separated).
        session_ids: Vec<String>,
        /// Maximum wait time in seconds (default 600).
        #[arg(long, default_value = "600")]
        timeout: u64,
    },

    /// List all sessions.
    Ps,

    /// Show session runtime info (model, tools, tokens, status).
    Info {
        /// Session ID.
        session_id: String,
    },

    /// Show last N messages from a session (like `tail`).
    Tail {
        /// Session ID.
        session_id: String,
        /// Number of messages to show (default 10).
        #[arg(short = 'n', long, default_value = "10")]
        n: usize,
        /// Filter by type: all|user|assistant|tool_call|tool_result|reasoning|content.
        #[arg(short = 't', long, default_value = "all")]
        filter: String,
    },

    /// Show first N messages from a session (like `head`).
    Head {
        /// Session ID.
        session_id: String,
        /// Number of messages to show (default 10).
        #[arg(short = 'n', long, default_value = "10")]
        n: usize,
        /// Filter by type: all|user|assistant|tool_call|tool_result|reasoning|content.
        #[arg(short = 't', long, default_value = "all")]
        filter: String,
    },

    /// List available models (grouped by provider).
    Models,

    /// List available tools.
    Tools,

    /// List available agent templates.
    Agents,
}

/// Dispatch CLI command.
pub async fn dispatch(cli: Cli) -> ExitCode {
    // stdio mode takes priority over subcommands.
    if cli.is_stdio_mode() {
        return dispatch_stdio(cli).await;
    }

    match cli.command {
        Some(cmd) => match cmd {
            Command::Tui {
                host,
                port,
                dump_config,
            } => {
                if dump_config {
                    let config = AppConfig::default();
                    print!("{}", config.to_yaml());
                    ExitCode::SUCCESS
                } else {
                    let gw = backend_config::read_backend_gateway_config();
                    let host = host.unwrap_or(gw.host);
                    let port = port.unwrap_or(gw.port);
                    match run_tui(&host, port).await {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            eprintln!("wing error: {e}");
                            ExitCode::FAILURE
                        }
                    }
                }
            }
            Command::Start { host, port } => {
                let gw = backend_config::read_backend_gateway_config();
                let host = host.unwrap_or(gw.host);
                let port = port.unwrap_or(gw.port);
                match start::start_gateway(&host, port).await {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("wing start error: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            Command::Stop => match stop::stop_gateway().await {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("wing stop error: {e}");
                    ExitCode::FAILURE
                }
            },
            Command::Status => {
                status::show_status().await;
                ExitCode::SUCCESS
            }
            Command::Run(args) => crate::cmd::run::run(args, cli.json).await,
            Command::Wait {
                session_ids,
                timeout,
            } => crate::cmd::wait::run_wait(&session_ids, timeout, cli.json).await,
            Command::Ps => crate::cmd::ps::run_ps(cli.json, cli.watch).await,
            Command::Info { session_id } => crate::cmd::ps::run_info(&session_id, cli.json).await,
            Command::Tail {
                session_id,
                n,
                filter,
            } => crate::cmd::messages::run_tail(&session_id, n, &filter, cli.json).await,
            Command::Head {
                session_id,
                n,
                filter,
            } => crate::cmd::messages::run_head(&session_id, n, &filter, cli.json).await,
            Command::Models => crate::cmd::query::run_models(cli.json).await,
            Command::Tools => crate::cmd::query::run_tools(cli.json).await,
            Command::Agents => crate::cmd::query::run_agents(cli.json).await,
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

/// Dispatch to stdio mode.
async fn dispatch_stdio(cli: Cli) -> ExitCode {
    use crate::stdio::{InputFormat, OutputFormat, StdioArgs};

    let output_format: OutputFormat = cli.output_format.parse().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let input_format: InputFormat = cli.input_format.parse().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    let prompt = cli.prompt.unwrap_or_default();

    let args = StdioArgs {
        prompt,
        model: cli.model,
        resume: cli.resume,
        system_prompt: cli.system_prompt,
        append_system_prompt: cli.append_system_prompt,
        max_turns: cli.max_turns,
        effort: cli.effort,
        provider: cli.provider,
        tools: cli.tools,
        output_format,
        input_format,
        yolo: cli.yolo,
    };

    // Initialize logging for stdio mode.
    let _log_guard = crate::util::logging::init_logging();

    crate::stdio::run_stdio(args).await
}

/// Smart default: check if gateway is running, start if not, then enter TUI.
async fn smart_default_tui() -> Result<()> {
    let (host, port) = crate::stdio::ensure_gateway_running().await?;
    run_tui(&host, port).await
}

/// Launch TUI: connect to gateway, init terminal, run app.
async fn run_tui(host: &str, port: u16) -> Result<()> {
    // Initialize logging (file only, no console output).
    let _log_guard = init_logging();

    // Load user configuration (needed early for api_key).
    let config = AppConfig::load();
    let api_key = config
        .api_key
        .as_deref()
        .filter(|k| !k.is_empty())
        .map(|k| k.to_string());
    let api_key_ref = api_key.as_deref();

    // Build URLs from host:port — no string replacement needed.
    let ws_url = format!("ws://{host}:{port}/ws");
    let http_base = format!("http://{host}:{port}");

    tracing::info!("wing starting, gateway: {ws_url}");

    // Get current working directory as workspace.
    let workspace = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    // 1. WS connect (get client_id).
    let gateway = GatewayClient::connect(&ws_url, api_key_ref)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to connect to gateway at {ws_url}: {e}\n\
                 Make sure the gateway is running: wing start"
            )
        })?;

    let client_id = gateway.client_id().to_string();
    tracing::info!(client_id = %client_id, "WS connected");

    // 2. HTTP create session.
    let http = GatewayApiClient::new(http_base.clone(), api_key_ref)
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;

    let create_req = wing_api_client::models::CreateSessionRequest {
        workspace: workspace.clone(),
        ..Default::default()
    };
    let session = http
        .create_session(&create_req)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create session: {e}"))?;

    let session_id = session.session_id.clone();
    tracing::info!(session_id = %session_id, "session created");

    // 3. HTTP subscribe.
    http.subscribe(&session_id, &client_id)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe to session: {e}"))?;

    tracing::info!("subscribed to session, entering TUI");

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
            crossterm::event::DisableFocusChange,
            crossterm::terminal::SetTitle(""),
            crossterm::cursor::Show
        );
        original_hook(panic_info);
    }));

    // Run the app.
    let transport = Transport {
        ws: gateway,
        http,
        client_id,
    };
    let result = run_app(
        &mut terminal,
        transport,
        session_id,
        crate::app::transport::GatewayEndpoint {
            ws_url,
            http_base,
            api_key,
        },
        config,
        workspace,
    )
    .await;

    // Restore terminal.
    tui::restore_terminal(&mut terminal)?;

    // Restore original panic hook.
    let _ = std::panic::take_hook();

    result?;
    tracing::info!("wing exited cleanly");
    Ok(())
}
