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
mod release;
pub mod run;
pub(crate) mod start;
mod status;
mod stop;
pub mod tag;
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

    /// Override model id (references `providers[].models` in the gateway config).
    #[arg(short = 'm', long = "model")]
    pub model: Option<String>,

    /// Override tools (comma-separated). If not set, uses template defaults.
    #[arg(long = "tools")]
    pub tools: Option<String>,

    /// Attach tags to the session (stdio mode `wing -p` only — subcommands
    /// like `wing run` / `wing tag` carry their own `--tag`; a misplaced
    /// top-level flag is rejected instead of being silently dropped).
    #[arg(long = "tag", value_delimiter = ',')]
    pub tag: Vec<String>,

    /// Resume an existing session by ID.
    #[arg(short = 'r', long = "resume")]
    pub resume: Option<String>,

    /// Create (or adopt) a session with this ID — create-or-adopt.
    ///
    /// When the ID does not exist yet the new session gets exactly this ID
    /// (orchestrators generate their own UUIDs); when it already exists the
    /// existing session is adopted (resume semantics, with the CLI overrides
    /// applied as the resume subset). Mutually exclusive with `-r/--resume`.
    #[arg(long = "session-id")]
    pub session_id: Option<String>,

    /// NOT SUPPORTED — always rejected (wing cannot truncate a session).
    ///
    /// Claude Code's "resume the session at an earlier point" flag. wing has no
    /// history-truncation support, and silently ignoring it would let an
    /// orchestrator believe its context was rolled back while wing kept the
    /// full history. Passing it exits non-zero with an explicit message.
    #[arg(long = "resume-session-at")]
    pub resume_session_at: Option<String>,

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

    /// Emit `stream_event` frames (Anthropic SSE shape) for token-level streaming.
    ///
    /// Claude CLI's `--include-partial-messages`. stdio mode only, and only with
    /// `--output-format stream-json` (text/json have no NDJSON channel to carry
    /// the frames — the flag is then a no-op with a log warning). Without the
    /// flag the output is byte-identical to before.
    #[arg(long = "include-partial-messages")]
    pub include_partial_messages: bool,

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

    /// List sessions (active + pinned by default; use --all for all).
    Ps {
        /// Show all sessions including inactive ones.
        ///
        /// Pinned sessions are shown even without this flag (pin means
        /// "keep this one in sight").
        #[arg(long = "all")]
        all: bool,
        /// Filter by tag (repeatable / comma-separated; multiple tags = AND).
        ///
        /// Implies --all: tags describe long-term taxonomy (favorites /
        /// task crews are often inactive), so inactive matches are included.
        #[arg(long = "tag", value_delimiter = ',')]
        tag: Vec<String>,
    },

    /// Show session runtime info (model, tools, tokens, status).
    Info {
        /// Session ID.
        session_id: String,
    },

    /// Read or edit session tags (also: --list for the global inventory).
    ///
    /// Tags are opaque strings (lowercase recommended; `k=v` is a namespace
    /// convention). Positional args add tags, --remove removes them; both in
    /// one call is applied atomically. Idempotent; never wakes evicted
    /// sessions; no tags at all = pure read.
    Tag {
        /// Session ID (omit when using --list).
        session_id: Option<String>,
        /// Tags to add (repeatable / comma-separated).
        #[arg(value_delimiter = ',')]
        tags: Vec<String>,
        /// Tags to remove (repeatable / comma-separated).
        #[arg(long = "remove", value_delimiter = ',')]
        remove: Vec<String>,
        /// List all tags across sessions with counts (inactive included).
        #[arg(long = "list")]
        list: bool,
    },

    /// Evict sessions from gateway memory (idle ones only; disk state is kept).
    ///
    /// Releases the in-memory state (worker + provider clients) of idle
    /// sessions immediately, without waiting for the idle TTL. Busy sessions
    /// (working / waiting or with pending input), subscribed ones, and
    /// non-durable backends are refused with 409.
    Release {
        /// Session IDs to release (space-separated).
        session_ids: Vec<String>,
    },

    /// Show last N messages from a session (like `tail`).
    Tail {
        /// Session ID.
        session_id: String,
        /// Number of messages to show (default 10).
        #[arg(short = 'n', long, default_value = "10")]
        n: usize,
        /// Filter by type: all|user|assistant|tool_call|tool_result|reasoning|content
        /// (named filters strip to the selected section in both text and --json
        /// output; tool results print as a 500-char peek in text mode).
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
        /// Filter by type: all|user|assistant|tool_call|tool_result|reasoning|content
        /// (named filters strip to the selected section in both text and --json
        /// output; tool results print as a 500-char peek in text mode).
        #[arg(short = 't', long, default_value = "all")]
        filter: String,
    },

    /// List available models (grouped by provider).
    Models,

    /// List available tools.
    Tools,

    /// List available agent templates.
    Agents,

    /// Serve ACP (Agent Client Protocol) on stdio — for Zed / omnigent and other
    /// ACP clients. Bridges every ACP session to a local wing session.
    Acp {
        /// Agent template for new sessions (default: gateway default template).
        #[arg(long)]
        agent: Option<String>,

        /// Initial model override for new sessions (clients can change it later).
        #[arg(long)]
        model: Option<String>,
    },
}

/// Error message when the top-level `--tag` is used outside stdio mode.
///
/// Stdio mode (`wing -p --tag ...`) consumes it legitimately; every other
/// path would silently drop it (clap accepts the flag before the subcommand
/// but nothing reads it), which recreates the "dispatched but untagged"
/// gap this feature exists to close. `None` = invocation is fine.
fn misplaced_global_tag_error(tags: &[String]) -> Option<String> {
    if tags.is_empty() {
        return None;
    }
    Some(
        "top-level --tag only applies to stdio mode (wing -p --tag ...); with a \
         subcommand put it after the subcommand, e.g. `wing run --tag executor ...`"
            .to_string(),
    )
}

/// Error message when the top-level `--session-id` is used outside stdio mode.
///
/// Stdio mode (`wing -p --session-id ...`) is the only consumer. The flag must be
/// part of the clap definition (otherwise the stdio argument filter would silently
/// drop it), which means a misplaced use now parses fine and would reach dispatch
/// with nobody reading it — the "silently ignored" shape this step exists to
/// remove. Refuse with a pointer instead: exit 1 + this message. (Before the flag
/// existed clap rejected it as an unknown argument — also exit ≠ 0, just with no
/// explanation of the stdio-only intent.)
/// `None` = invocation is fine.
fn misplaced_session_id_error(session_id: Option<&str>) -> Option<String> {
    let session_id = session_id?;
    Some(format!(
        "top-level --session-id only applies to stdio mode (wing -p --session-id {session_id} ...); \
         wing refuses it here instead of ignoring it. On other paths the session id is \
         backend-generated (wing run) or positional (wing info/tail/head/release)."
    ))
}

/// Error message when the top-level `--include-partial-messages` is used outside
/// stdio mode.
///
/// Same reasoning as the `--session-id` / `--tag` guards: the flag must live on
/// the top-level `Cli` (otherwise the stdio argument filter would silently drop
/// it — the exact failure this feature removes), which means clap accepts it
/// next to subcommands where nobody reads it. Refuse with a pointer instead of
/// silently ignoring. `None` = invocation is fine.
fn misplaced_include_partial_messages_error(flag: bool) -> Option<String> {
    if !flag {
        return None;
    }
    Some(
        "top-level --include-partial-messages only applies to stdio mode \
         (wing -p ... --output-format stream-json); wing refuses it here instead of \
         silently ignoring it."
            .to_string(),
    )
}

/// Dispatch CLI command.
pub async fn dispatch(cli: Cli) -> ExitCode {
    // Logging is initialized for **every** path — TUI, stdio and all
    // orchestration subcommands — from this single entry point. Without it the
    // CLI failed silently: e.g. a dead WS read task in `wing wait` only left a
    // tracing event that nobody was subscribed to (no subscriber = no file, no
    // stderr). Idempotent, so the TUI / stdio paths keep calling it too.
    let _log_guard = init_logging();

    // `--resume-session-at`（Claude Code 的"会话截断回滚"旗标）：wing 未实现，
    // 且**绝不静默忽略**——消费方会据此认为上下文已回退，与 wing 的实际状态
    // 错位。检查在任何副作用（起网关 / 建会话 / 发请求）之前，且对**任何**调用
    // 形态生效（stdio / 无子命令 / "顶层旗标 + 子命令"）：只要 clap 收下了它，
    // 就一定会被拒绝，不存在被谁静默吃掉的路径。
    if let Some(value) = cli.resume_session_at.as_deref() {
        eprintln!(
            "wing error: {}",
            crate::stdio::resume_session_at_error(value)
        );
        return ExitCode::FAILURE;
    }

    // stdio mode takes priority over subcommands.
    if cli.is_stdio_mode() {
        return dispatch_stdio(cli).await;
    }

    // 顶层 --tag 只服务 stdio 模式（`wing -p --tag ...`）；子命令各自的 --tag
    // 定义在 RunArgs / Command::Tag 上。`wing --tag x run ...` 这类放错位置
    // 的写法会被 clap 静默接受但丢弃标签——显式报错，别让 Agent 以为打上了。
    if let Some(message) = misplaced_global_tag_error(&cli.tag) {
        eprintln!("wing error: {message}");
        return ExitCode::FAILURE;
    }

    // 同理：顶层 --session-id 只服务 stdio 模式（create-or-adopt）。放错位置时
    // 被静默丢弃会让编排方以为自己指定的 id 生效了。
    if let Some(message) = misplaced_session_id_error(cli.session_id.as_deref()) {
        eprintln!("wing error: {message}");
        return ExitCode::FAILURE;
    }

    // 同理：`--include-partial-messages` 只服务 stdio 模式（流式增量帧）。它
    // 在别的路径上不会有任何反应——静默吃掉等于骗消费方"流开着"。
    if let Some(message) = misplaced_include_partial_messages_error(cli.include_partial_messages) {
        eprintln!("wing error: {message}");
        return ExitCode::FAILURE;
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
            Command::Ps { all, tag } => {
                crate::cmd::ps::run_ps(all, &tag, cli.json, cli.watch).await
            }
            Command::Info { session_id } => crate::cmd::ps::run_info(&session_id, cli.json).await,
            Command::Tag {
                session_id,
                tags,
                remove,
                list,
            } => {
                crate::cmd::tag::run_tag(session_id.as_deref(), &tags, &remove, list, cli.json)
                    .await
            }
            Command::Release { session_ids } => {
                crate::cmd::release::run(&session_ids, cli.json).await
            }
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
            Command::Acp { agent, model } => {
                crate::acp::run_acp(crate::acp::AcpArgs { agent, model }).await
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
        session_id: cli.session_id,
        resume_session_at: cli.resume_session_at,
        system_prompt: cli.system_prompt,
        append_system_prompt: cli.append_system_prompt,
        max_turns: cli.max_turns,
        effort: cli.effort,
        tools: cli.tools,
        tag: cli.tag,
        output_format,
        input_format,
        include_partial_messages: cli.include_partial_messages,
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
    //
    // The teardown itself is `tui::leave_sequence` (mouse reporting off before
    // leaving the alternate screen) — the same sequence the clean-exit path
    // writes, so a panic can never leave the terminal reporting mice to the
    // shell. Failures are ignored: a dead terminal must not re-panic.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = tui::leave_sequence(&mut std::io::stdout());
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(""));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misplaced_global_tag_is_rejected_outside_stdio() {
        assert!(misplaced_global_tag_error(&[]).is_none());

        let message = misplaced_global_tag_error(&["executor".to_string()])
            .expect("tags outside stdio mode must be rejected");
        assert!(message.contains("stdio"), "{message}");
        assert!(message.contains("wing run --tag"), "{message}");
    }

    #[test]
    fn clap_parses_top_level_tag_before_subcommand() {
        // 这正是 guard 存在的理由：clap 接受这种写法（flag 绑在顶层），
        // 若不放行闸门，标签会被静默丢弃。
        let cli = Cli::try_parse_from(["wing", "--tag", "executor", "ps"]).expect("parses");
        assert_eq!(cli.tag, vec!["executor".to_string()]);
        assert!(matches!(cli.command, Some(Command::Ps { .. })));
        assert!(misplaced_global_tag_error(&cli.tag).is_some());
    }

    #[test]
    fn clap_parses_session_id_in_both_forms() {
        let cli = Cli::try_parse_from(["wing", "-p", "hi", "--session-id=abc-123"]).expect("=");
        assert_eq!(cli.session_id.as_deref(), Some("abc-123"));

        let cli =
            Cli::try_parse_from(["wing", "-p", "hi", "--session-id", "abc-123"]).expect("space");
        assert_eq!(cli.session_id.as_deref(), Some("abc-123"));

        let cli = Cli::try_parse_from(["wing", "-p", "hi"]).expect("absent");
        assert_eq!(cli.session_id, None);
    }

    #[test]
    fn clap_parses_resume_session_at_in_both_forms() {
        // 两种传送形式都必须命中（否则它会退回"未知参数丢弃"路径——正是本步要
        // 消灭的静默忽略）。
        let cli = Cli::try_parse_from(["wing", "-p", "hi", "--resume-session-at=3"]).expect("=");
        assert_eq!(cli.resume_session_at.as_deref(), Some("3"));

        let cli =
            Cli::try_parse_from(["wing", "-p", "hi", "--resume-session-at", "3"]).expect("space");
        assert_eq!(cli.resume_session_at.as_deref(), Some("3"));
    }

    #[test]
    fn misplaced_session_id_is_rejected_outside_stdio() {
        assert!(misplaced_session_id_error(None).is_none());

        let message = misplaced_session_id_error(Some("abc"))
            .expect("session id outside stdio mode must be rejected");
        assert!(message.contains("stdio"), "{message}");
        assert!(message.contains("--session-id"), "{message}");

        // clap 接受这种写法（flag 绑在顶层）——guard 是唯一防线。
        let cli = Cli::try_parse_from(["wing", "--session-id", "abc", "ps"]).expect("parses");
        assert_eq!(cli.session_id.as_deref(), Some("abc"));
        assert!(misplaced_session_id_error(cli.session_id.as_deref()).is_some());
    }

    #[test]
    fn stdio_invocation_keeps_session_id() {
        // stdio 模式下 `--session-id` 是正式参数：`dispatch` 在 `is_stdio_mode()`
        // 分支之后才走到"放错位置"闸门，因此这条路径不会被拦下。
        let cli = Cli::try_parse_from(["wing", "-p", "hi", "--session-id=abc"]).expect("parses");
        assert!(cli.is_stdio_mode());
        assert_eq!(cli.session_id.as_deref(), Some("abc"));
    }

    #[test]
    fn clap_parses_include_partial_messages() {
        let cli = Cli::try_parse_from(["wing", "-p", "hi", "--include-partial-messages"])
            .expect("parses");
        assert!(cli.include_partial_messages);
        assert!(cli.is_stdio_mode());

        let cli = Cli::try_parse_from(["wing", "-p", "hi"]).expect("absent");
        assert!(!cli.include_partial_messages);
    }

    #[test]
    fn clap_parses_the_model_id_and_rejects_the_removed_provider_flag() {
        // `--model` 的值是 model_id（引用词）——原样进 StdioArgs / override。
        let cli = Cli::try_parse_from(["wing", "-p", "hi", "--model", "ds-flash"]).expect("parses");
        assert_eq!(cli.model.as_deref(), Some("ds-flash"));

        // `--provider` 已删除：clap 直接报未知参数（不写兼容、不写引导）。
        for argv in [
            vec!["wing", "-p", "hi", "--provider", "qoder"],
            vec!["wing", "-p", "hi", "--provider=qoder"],
            vec!["wing", "run", "-p", "hi", "--provider", "qoder"],
        ] {
            let Err(err) = Cli::try_parse_from(argv.clone()) else {
                panic!("--provider must be rejected, got {argv:?}");
            };
            assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
            assert!(err.to_string().contains("--provider"), "{err}");
        }
    }

    #[test]
    fn misplaced_include_partial_messages_is_rejected_outside_stdio() {
        assert!(misplaced_include_partial_messages_error(false).is_none());

        let message = misplaced_include_partial_messages_error(true)
            .expect("outside stdio mode the flag must be rejected");
        assert!(message.contains("stdio"), "{message}");
        assert!(message.contains("--include-partial-messages"), "{message}");

        // clap 接受这种写法（flag 绑在顶层）——guard 是唯一防线。
        let cli =
            Cli::try_parse_from(["wing", "--include-partial-messages", "ps"]).expect("parses");
        assert!(cli.include_partial_messages);
        assert!(misplaced_include_partial_messages_error(cli.include_partial_messages).is_some());

        // stdio 模式：`dispatch` 在 is_stdio_mode() 分支之后才走到闸门，不受影响。
        let cli = Cli::try_parse_from(["wing", "-p", "hi", "--include-partial-messages"])
            .expect("parses");
        assert!(cli.is_stdio_mode());
    }
}
