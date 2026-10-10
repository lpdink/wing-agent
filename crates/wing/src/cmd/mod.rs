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
pub mod argv;
pub mod asks;
pub(crate) mod backend_config;
pub mod branch;
pub mod common;
pub mod config;
pub mod control;
mod discover;
pub mod lifecycle;
pub mod messages;
pub mod ps;
pub mod query;
mod release;
pub mod reload;
mod restart;
pub mod run;
pub(crate) mod setup;
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
    /// An orchestrator's "resume the session at an earlier point" flag. wing has
    /// no history-truncation support, and silently ignoring it would let an
    /// orchestrator believe its context was rolled back while wing kept the full
    /// history. Passing it exits non-zero with an explicit message.
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

    /// Answer a pending Ask by its tool_call_id (stdio mode).
    ///
    /// Routes the prompt to that ask's feedback waiter instead of the session
    /// inbox; find the id with `wing asks <sid>`. (`wing run` carries the same
    /// flag on the subcommand; this top-level copy exists so the stdio argument
    /// filter keeps it — see `filter_unknown_args`.)
    #[arg(long = "tool-call-id")]
    pub tool_call_id: Option<String>,

    /// Output format: text (default), json, stream-json.
    #[arg(long = "output-format", default_value = "text")]
    pub output_format: String,
    /// Input format: text (default), stream-json.
    #[arg(long = "input-format", default_value = "text")]
    pub input_format: String,

    /// Emit `stream_event` frames (Anthropic SSE shape) for token-level streaming.
    ///
    /// The NDJSON stdio protocol's `--include-partial-messages` flag. stdio mode
    /// only, and only with `--output-format stream-json` (text/json have no
    /// NDJSON channel to carry the frames — the flag is then a no-op with a log
    /// warning). Without the flag the output is byte-identical to before.
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

        /// Dump the *current* TUI config file as canonical (commented) YAML to stdout and exit.
        ///
        /// Reads `$WING_HOME/tui/config.yaml` (a broken file is reported on stderr instead of
        /// being dumped as defaults). Secret leaves are MASKED by default: `api_key` becomes
        /// `null` plus a `# 已掩码（•••••••• 1234）` comment — pass --show-secrets for the real
        /// values.
        #[arg(long)]
        dump_config: bool,

        /// Write secret values (`api_key`) verbatim instead of masking them (--dump-config only).
        ///
        /// WARNING: stdout then carries the real secret. Use it for the round-trip
        /// (`wing tui --dump-config --show-secrets > config.yaml`) and be aware that anything
        /// capturing stdout — a redirected file, a log, CI output — now holds your key.
        #[arg(long = "show-secrets")]
        show_secrets: bool,
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

    /// Restart the gateway daemon (stop + start; safe when it is not running).
    Restart {
        /// Gateway host (default: from config).
        #[arg(long)]
        host: Option<String>,

        /// Gateway port (default: from config).
        #[arg(long)]
        port: Option<u16>,
    },

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

    /// Interrupt the session's current task (the TUI's Esc).
    #[command(visible_alias = "int")]
    Interrupt {
        /// Session ID.
        session_id: String,
    },

    /// Fork a session at a message node — the new session keeps the prefix.
    ///
    /// The node's own text is returned as `draft` (it is left out of the new
    /// session so it can be re-sent, edited). Get uuids from `wing branches`;
    /// `--at current` copies the whole chain.
    Fork {
        /// Session ID.
        session_id: String,
        /// Target message uuid (`current` = the latest state).
        #[arg(long, value_name = "UUID")]
        at: String,
    },

    /// Rewind a session to just before a message node.
    ///
    /// The cut message comes back as `draft` for re-sending. Get uuids from
    /// `wing branches`; `--to current` is a no-op (already the latest state).
    Rewind {
        /// Session ID.
        session_id: String,
        /// Target message uuid (`current` = no-op).
        #[arg(long, value_name = "UUID")]
        to: String,
    },

    /// List a session's forkable / rewindable message nodes (the uuids `fork` / `rewind` take).
    Branches {
        /// Session ID.
        session_id: String,
    },

    /// Show a session's pending Ask question(s) — with the tool_call_id to answer them.
    ///
    /// A pending ask's tool call is not on the chain yet, so `wing tail` cannot
    /// see it; this reads the live session snapshot (read-only, one shot).
    Asks {
        /// Session ID.
        session_id: String,
        /// Block up to N seconds for an ask to appear (default: report the
        /// current snapshot only).
        #[arg(long, default_value = "0")]
        wait: u64,
    },

    /// Compact a session's context (manual compaction).
    Compact {
        /// Session ID.
        session_id: String,
        /// Optional compaction focus, appended to the compact prompt (without
        /// it the default strategy runs).
        instruction: Option<String>,
    },

    /// Update session state — only the fields you pass are changed.
    Update(control::UpdateArgs),

    /// Hot-reload the gateway configuration (config / hooks / commands /
    /// providers / skills & rules / log level; per-item results, in order).
    Reload,

    /// Create an empty session and print its id.
    New(lifecycle::NewArgs),

    /// Load an evicted session back into gateway memory and print its state.
    Resume {
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

    /// Read and change the gateway configuration (Setting API).
    ///
    /// Works while the gateway is degraded (setup mode), which is exactly when
    /// the TUI panel is unreachable. Never auto-starts the gateway.
    Config {
        #[command(subcommand)]
        command: config::ConfigCommand,
    },

    /// Show last N messages from a session (like `tail`).
    Tail {
        /// Session ID.
        session_id: String,
        /// Number of messages to show (default 10).
        #[arg(short = 'n', long, default_value = "10")]
        n: usize,
        #[arg(
            short = 't',
            long,
            default_value = "all",
            value_delimiter = ',',
            help = messages::FILTER_HELP
        )]
        filter: Vec<messages::FilterArg>,
    },

    /// Show first N messages from a session (like `head`).
    Head {
        /// Session ID.
        session_id: String,
        /// Number of messages to show (default 10).
        #[arg(short = 'n', long, default_value = "10")]
        n: usize,
        #[arg(
            short = 't',
            long,
            default_value = "all",
            value_delimiter = ',',
            help = messages::FILTER_HELP
        )]
        filter: Vec<messages::FilterArg>,
    },

    /// List available models (grouped by provider).
    Models,

    /// List available tools.
    Tools,

    /// List available agent templates.
    Agents,

    /// Serve ACP (Agent Client Protocol) on stdio — for ACP clients (editors,
    /// agent orchestrators). Bridges every ACP session to a local wing session.
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

/// Error message when the top-level `--tool-call-id` is used outside stdio mode.
///
/// Stdio mode (`wing -p ... --tool-call-id ...`) is one consumer; the other is
/// `wing run`'s own `--tool-call-id` (placed **after** the subcommand, where it
/// binds to `RunArgs`). The flag must live on the top-level `Cli` as well,
/// otherwise the stdio argument filter would silently drop it — the exact
/// failure this step exists to remove — which means a misplaced use now parses
/// fine and would reach dispatch with nobody reading it. Refuse with a pointer
/// instead of silently ignoring. `None` = invocation is fine.
fn misplaced_tool_call_id_error(tool_call_id: Option<&str>) -> Option<String> {
    let tool_call_id = tool_call_id?;
    Some(format!(
        "top-level --tool-call-id only applies to stdio mode (wing -p ... --tool-call-id {tool_call_id}); \
         with a subcommand put it after the subcommand, e.g. `wing run --tool-call-id {tool_call_id} ...`. \
         wing refuses it here instead of ignoring it."
    ))
}

/// Error message when `--show-secrets` is passed without `--dump-config`.
///
/// Same reasoning as the other misplaced-flag guards: the flag lives on
/// `Command::Tui` (it selects how `--dump-config` emits secret leaves), so
/// `wing tui --show-secrets` parses fine and would otherwise be a silently
/// ignored no-op — the user would believe the flag did something. Refuse with
/// a pointer instead. `None` = invocation is fine.
fn misplaced_show_secrets_error(show_secrets: bool, dump_config: bool) -> Option<String> {
    if !show_secrets || dump_config {
        return None;
    }
    Some(format!(
        "--show-secrets only applies to --dump-config \
         (`{}`); wing refuses it here instead of ignoring it.",
        crate::config::catalog::SHOW_SECRETS_HINT
    ))
}

/// `--dump-config` 的输出（纯函数：读文件由调用方做，这里只管**模式**这一个分岔）。
///
/// 默认 `Masked`：stdout 会被重定向、日志与 CI 捕获，而密文在项目其余地方的姿态是
/// "只写不回显"（后端 `get` 恒返回 `null` + 末 4 位 hint，`wing config get` 显示
/// `•••••••• 1234`）。真值只在显式 `--show-secrets` 时给（round-trip 用途）。
fn dump_config_output(doc: &serde_json::Value, show_secrets: bool) -> String {
    let mode = if show_secrets {
        crate::config::catalog::DumpMode::Raw
    } else {
        crate::config::catalog::DumpMode::Masked
    };
    crate::config::catalog::dump_config_yaml(doc, mode)
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

    // 同理：顶层 --tool-call-id 只服务 stdio 模式（`wing -p --tool-call-id ...`）。
    // 子命令形态在 `wing run` 上有自己的同名旗标（出现在子命令之后即绑定到它，
    // 顶层保持 None），所以这条闸门只拦"放在子命令之前"或"没走 stdio"的写法。
    if let Some(message) = misplaced_tool_call_id_error(cli.tool_call_id.as_deref()) {
        eprintln!("wing error: {message}");
        return ExitCode::FAILURE;
    }

    match cli.command {
        Some(cmd) => match cmd {
            Command::Tui {
                host,
                port,
                dump_config,
                show_secrets,
            } => {
                if let Some(message) = misplaced_show_secrets_error(show_secrets, dump_config) {
                    eprintln!("wing error: {message}");
                    return ExitCode::FAILURE;
                }
                if dump_config {
                    // Canonical form of the *current* file (`$WING_HOME/tui/config.yaml`): the
                    // emitter is catalog-driven and shared with the settings panel's save path,
                    // so there is exactly one declaration. A broken file is reported instead of
                    // silently dumped as defaults.
                    //
                    // Secret leaves are masked unless `--show-secrets`: this output goes to
                    // stdout, which shells redirect and CI captures.
                    match crate::config::store::read_interface_doc() {
                        Ok(read) => {
                            print!("{}", dump_config_output(&read.doc, show_secrets));
                            ExitCode::SUCCESS
                        }
                        Err(e) => {
                            eprintln!("wing error: {e}");
                            ExitCode::FAILURE
                        }
                    }
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
            Command::Restart { host, port } => crate::cmd::restart::run(host, port, cli.json).await,
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
            Command::Interrupt { session_id } => {
                crate::cmd::control::run_interrupt(&session_id, cli.json).await
            }
            Command::Fork { session_id, at } => {
                crate::cmd::branch::run_fork(&session_id, &at, cli.json).await
            }
            Command::Rewind { session_id, to } => {
                crate::cmd::branch::run_rewind(&session_id, &to, cli.json).await
            }
            Command::Branches { session_id } => {
                crate::cmd::branch::run_branches(&session_id, cli.json).await
            }
            Command::Asks { session_id, wait } => {
                crate::cmd::asks::run_asks(&session_id, wait, cli.json).await
            }
            Command::Compact {
                session_id,
                instruction,
            } => {
                crate::cmd::control::run_compact(&session_id, instruction.as_deref(), cli.json)
                    .await
            }
            Command::Update(args) => crate::cmd::control::run_update(args, cli.json).await,
            Command::Reload => crate::cmd::reload::run_reload(cli.json).await,
            Command::New(args) => crate::cmd::lifecycle::run_new(args, cli.json).await,
            Command::Resume { session_id } => {
                crate::cmd::lifecycle::run_resume(&session_id, cli.json).await
            }
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
            Command::Config { command } => crate::cmd::config::run(command, cli.json).await,
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
        tool_call_id: cli.tool_call_id,
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

/// 装一个"panic 时先恢复终端"的 hook。
///
/// 恢复序列就是 `tui::leave_sequence`（先关鼠标上报再离开备用屏）——与干净退出路径
/// 写的是同一串字节，panic 因此永远不会把"正在上报鼠标"的终端留给 shell。
/// 失败一律忽略：垂死的终端不能再 panic。原 hook 链在其后。
///
/// 与 [`restore_panic_hook`] 成对；调用方（[`run_tui`]）负责装卸——setup 循环只借终端，
/// 这样 setup 阶段的 panic 与正常阶段走的是同一套恢复。
fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = tui::leave_sequence(&mut std::io::stdout());
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(""));
        original_hook(panic_info);
    }));
}

/// 摘掉 [`install_panic_hook`] 装的自定义 hook（装回默认 hook）。
fn restore_panic_hook() {
    let _ = std::panic::take_hook();
}

/// Launch TUI: connect to gateway, init terminal, run app.
///
/// 启动前的预检（§16.1）：配置不可用时先跑一个**无 session 的 setup 循环**，
/// 用户修好并保存后继续走下面与今天逐字相同的启动链。配置可用 ⇒ 一个分支都不进。
async fn run_tui(host: &str, port: u16) -> Result<()> {
    // Initialize logging (file only, no console output).
    let _log_guard = init_logging();

    // Load user configuration (needed early for api_key). `mut`: setup 阶段会预览
    // Interface 根（颜色等改动当场生效），随后原样交给 run_app。
    let mut config = AppConfig::load();
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

    // HTTP client（预检与后面的建会话共用同一个；伪码 §16.1 的构造位置就在预检之前）。
    let http = GatewayApiClient::new(http_base.clone(), api_key_ref)
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;

    // ── 预检：配置可用吗？（最便宜的一次调用；不要把它与"网关不可达"混为一谈）──
    match setup::preflight_config(&http).await {
        setup::Preflight::Ready => {}
        setup::Preflight::Unusable { .. } => {
            let endpoint = crate::app::transport::GatewayEndpoint {
                ws_url: ws_url.clone(),
                http_base: http_base.clone(),
                api_key: api_key.clone(),
            };
            let mut terminal = tui::init_terminal()?;
            install_panic_hook();
            // 先无损收尾再 `?`：setup 内部报错（如拉数据失败）也绝不能把终端留在 raw mode。
            let outcome = setup::run_setup_tui(&mut terminal, &http, &mut config, &endpoint).await;
            restore_panic_hook();
            tui::restore_terminal(&mut terminal)?;
            match outcome? {
                setup::SetupOutcome::Quit => {
                    // 终端恢复后打印（§16.2）：配置仍不可用时的出路。路径取后端权威值，
                    // 拿不到就省略那半句。
                    let config_path = http
                        .settings_get()
                        .await
                        .ok()
                        .map(|state| state.config_path);
                    eprintln!("{}", setup::quit_note(config_path.as_deref()));
                    return Ok(());
                }
                setup::SetupOutcome::Ready => {}
            }
        }
        setup::Preflight::Failed(e) => {
            anyhow::bail!("{}", setup::preflight_failure_message(&http_base, &e));
        }
    }

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

    // Set up panic hook to restore terminal on panic (see `install_panic_hook`).
    install_panic_hook();

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
    restore_panic_hook();

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

    /// `--dump-config` 的默认方向是**掩码**：CLI 这一处必须传 `Masked`
    /// （保存路径那一处传 `Raw`，见 `config/store.rs::the_save_path_writes_the_real_secret_never_the_mask`）。
    #[test]
    fn dump_config_output_masks_by_default_and_raw_with_show_secrets() {
        let doc = serde_json::json!({"api_key": "sk-dump-secret-9999"});

        let masked = dump_config_output(&doc, false);
        assert!(!masked.contains("sk-dump-secret-9999"), "{masked}");
        assert!(masked.contains("已掩码（•••••••• 9999）"), "{masked}");
        assert!(
            masked.contains(crate::config::catalog::SHOW_SECRETS_HINT),
            "{masked}"
        );

        let raw = dump_config_output(&doc, true);
        assert!(raw.contains("api_key: sk-dump-secret-9999"), "{raw}");
        assert!(!raw.contains('•'), "{raw}");
    }

    /// `--show-secrets` 只在 `--dump-config` 上有意义；单独出现时**拒绝**（不是静默忽略）。
    #[test]
    fn clap_parses_show_secrets_only_on_dump_config() {
        let cli = Cli::try_parse_from(["wing", "tui", "--dump-config"]).expect("parses");
        match cli.command {
            Some(Command::Tui {
                dump_config,
                show_secrets,
                ..
            }) => {
                assert!(dump_config);
                assert!(!show_secrets, "默认掩码");
                assert!(misplaced_show_secrets_error(show_secrets, dump_config).is_none());
            }
            other => panic!("expected the tui subcommand, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["wing", "tui", "--dump-config", "--show-secrets"])
            .expect("parses");
        match cli.command {
            Some(Command::Tui {
                dump_config,
                show_secrets,
                ..
            }) => {
                assert!(dump_config && show_secrets);
                assert!(misplaced_show_secrets_error(show_secrets, dump_config).is_none());
            }
            other => panic!("expected the tui subcommand, got {other:?}"),
        }

        // 单独出现：clap 收下（flag 绑在 Tui 上），guard 必须拒绝并指向 --dump-config。
        let cli = Cli::try_parse_from(["wing", "tui", "--show-secrets"]).expect("parses");
        match cli.command {
            Some(Command::Tui {
                dump_config,
                show_secrets,
                ..
            }) => {
                assert!(!dump_config && show_secrets);
                let message = misplaced_show_secrets_error(show_secrets, dump_config)
                    .expect("outside --dump-config the flag must be rejected");
                assert!(message.contains("--dump-config"), "{message}");
                assert!(message.contains("--show-secrets"), "{message}");
            }
            other => panic!("expected the tui subcommand, got {other:?}"),
        }

        // 不误伤：没传旗标时永远放行。
        assert!(misplaced_show_secrets_error(false, false).is_none());
        assert!(misplaced_show_secrets_error(false, true).is_none());

        // 顶层形态不存在（`wing --dump-config` / `wing --show-secrets` 都是 clap 错误）：
        // 真形态是 `wing tui --dump-config`，任务书里的顶层写法要按真值改写。
        for argv in [
            vec!["wing", "--dump-config"],
            vec!["wing", "--dump-config", "--show-secrets"],
            vec!["wing", "--show-secrets"],
        ] {
            let Err(err) = Cli::try_parse_from(argv.clone()) else {
                panic!("top-level form must not parse, got {argv:?}");
            };
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::UnknownArgument,
                "{argv:?}"
            );
        }
    }

    // ── 控制面子命令（clap 形态） ──────────────────────────────

    #[test]
    fn clap_parses_the_branch_commands() {
        let cli = Cli::try_parse_from(["wing", "branches", "sid-1"]).expect("branches");
        assert!(matches!(
            cli.command,
            Some(Command::Branches { ref session_id }) if session_id == "sid-1"
        ));

        let cli = Cli::try_parse_from(["wing", "fork", "sid-1", "--at", "u-1"]).expect("fork");
        match cli.command {
            Some(Command::Fork { session_id, at }) => {
                assert_eq!((session_id.as_str(), at.as_str()), ("sid-1", "u-1"));
            }
            other => panic!("expected fork, got {other:?}"),
        }
        // uuid 是 --at / --to 的**值**（不是位置参数）：缺了它 clap 直接报错。
        let err = Cli::try_parse_from(["wing", "fork", "sid-1"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

        let cli =
            Cli::try_parse_from(["wing", "rewind", "sid-1", "--to", "current"]).expect("rewind");
        match cli.command {
            Some(Command::Rewind { session_id, to }) => {
                assert_eq!((session_id.as_str(), to.as_str()), ("sid-1", "current"));
            }
            other => panic!("expected rewind, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_interrupt_and_its_int_alias() {
        for name in ["interrupt", "int"] {
            let cli = Cli::try_parse_from(["wing", name, "sid-1"]).expect(name);
            assert!(
                matches!(cli.command, Some(Command::Interrupt { ref session_id }) if session_id == "sid-1"),
                "{name}"
            );
        }
    }

    #[test]
    fn clap_parses_compact_with_and_without_instruction() {
        let cli = Cli::try_parse_from(["wing", "compact", "sid-1"]).expect("no instruction");
        assert!(matches!(
            cli.command,
            Some(Command::Compact { ref session_id, instruction: None }) if session_id == "sid-1"
        ));

        let cli =
            Cli::try_parse_from(["wing", "compact", "sid-1", "keep the TODOs"]).expect("with");
        match cli.command {
            Some(Command::Compact { instruction, .. }) => {
                assert_eq!(instruction.as_deref(), Some("keep the TODOs"));
            }
            other => panic!("expected compact, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_all_update_flags() {
        let cli = Cli::try_parse_from([
            "wing",
            "update",
            "sid-1",
            "--model",
            "ds-flash",
            "--agent",
            "executor",
            "--title",
            "nightly",
            "--effort",
            "high",
            "--workspace",
            "/tmp/ws",
            "--tools",
            "core.Bash,Read",
        ])
        .expect("update");
        match cli.command {
            Some(Command::Update(args)) => {
                assert_eq!(args.session_id, "sid-1");
                assert_eq!(args.model.as_deref(), Some("ds-flash"));
                assert_eq!(args.agent.as_deref(), Some("executor"));
                assert_eq!(args.title.as_deref(), Some("nightly"));
                assert_eq!(args.effort.as_deref(), Some("high"));
                assert_eq!(args.workspace.as_deref(), Some("/tmp/ws"));
                assert_eq!(args.tools.as_deref(), Some("core.Bash,Read"));
                assert!(args.thinking.is_none() && args.yolo.is_none());
            }
            other => panic!("expected update, got {other:?}"),
        }
    }

    /// on/off 三形态：裸旗标（= on）、空格取值、`=` 取值；`true` / `false` 是别名。
    /// 裸旗标后紧跟另一个旗标不能被吞成它的值（clap 的 `num_args(0..=1)` 语义）。
    #[test]
    fn clap_parses_update_on_off_flags_in_every_form() {
        use crate::cmd::control::OnOff;

        let cli = Cli::try_parse_from(["wing", "update", "sid", "--yolo"]).expect("bare");
        match cli.command {
            Some(Command::Update(args)) => assert_eq!(args.yolo, Some(OnOff::On)),
            other => panic!("expected update, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["wing", "update", "sid", "--yolo", "off"]).expect("space");
        match cli.command {
            Some(Command::Update(args)) => assert_eq!(args.yolo, Some(OnOff::Off)),
            other => panic!("expected update, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["wing", "update", "sid", "--thinking=off"]).expect("equals");
        match cli.command {
            Some(Command::Update(args)) => assert_eq!(args.thinking, Some(OnOff::Off)),
            other => panic!("expected update, got {other:?}"),
        }

        // 别名形态。
        let cli = Cli::try_parse_from(["wing", "update", "sid", "--thinking", "false"])
            .expect("true/false alias");
        match cli.command {
            Some(Command::Update(args)) => assert_eq!(args.thinking, Some(OnOff::Off)),
            other => panic!("expected update, got {other:?}"),
        }

        // 裸旗标 + 下一个旗标：`--thinking` 不能被吃掉 `--title` 的值。
        let cli = Cli::try_parse_from(["wing", "update", "sid", "--thinking", "--title", "t"])
            .expect("bare followed by another flag");
        match cli.command {
            Some(Command::Update(args)) => {
                assert_eq!(args.thinking, Some(OnOff::On));
                assert_eq!(args.title.as_deref(), Some("t"));
            }
            other => panic!("expected update, got {other:?}"),
        }
    }

    #[test]
    fn clap_parses_reload_new_and_resume() {
        let cli = Cli::try_parse_from(["wing", "reload"]).expect("reload");
        assert!(matches!(cli.command, Some(Command::Reload)));

        let cli = Cli::try_parse_from([
            "wing",
            "new",
            "--workspace",
            "/tmp/ws",
            "--template",
            "executor",
            "--tag",
            "a,b",
        ])
        .expect("new");
        match cli.command {
            Some(Command::New(args)) => {
                assert_eq!(args.workspace.as_deref(), Some("/tmp/ws"));
                assert_eq!(args.template.as_deref(), Some("executor"));
                assert_eq!(args.tag, vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected new, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["wing", "resume", "sid-1"]).expect("resume");
        assert!(matches!(
            cli.command,
            Some(Command::Resume { ref session_id }) if session_id == "sid-1"
        ));
    }

    /// `wing run --tool-call-id`（Ask 定向回答）：值是 call id，可重复给出最后一次生效。
    #[test]
    fn clap_parses_run_tool_call_id() {
        let cli = Cli::try_parse_from([
            "wing",
            "run",
            "-r",
            "sid",
            "-p",
            "answer",
            "--tool-call-id",
            "call-42",
        ])
        .expect("run with tool call id");
        match cli.command {
            Some(Command::Run(args)) => {
                assert_eq!(args.tool_call_id.as_deref(), Some("call-42"));
            }
            other => panic!("expected run, got {other:?}"),
        }

        let cli = Cli::try_parse_from(["wing", "run", "-p", "hi"]).expect("run without");
        match cli.command {
            Some(Command::Run(args)) => assert!(args.tool_call_id.is_none()),
            other => panic!("expected run, got {other:?}"),
        }
    }

    /// 顶层 `--tool-call-id`（stdio 模式）与 `wing run` 的子命令版并存：
    /// 出现在子命令之后绑定到 `RunArgs`，顶层保持 None——两条路径互不干扰。
    ///
    /// 顶层那份存在的理由是 stdio 参数过滤器（`filter_unknown_args`）：`-p`
    /// 一出现它就按 stdio 语义丢未知长旗标，而它只认顶层定义——子命令旗标
    /// 会被静默丢掉（这正是本步要消灭的形态）。
    #[test]
    fn clap_parses_tool_call_id_on_both_levels() {
        // stdio 形态：顶层字段。guard 不在这条路径上被询问（dispatch 先走
        // is_stdio_mode 分支返回），`is_stdio_mode` 就是它的保护。
        let cli =
            Cli::try_parse_from(["wing", "-p", "answer", "-r", "sid", "--tool-call-id", "c1"])
                .expect("stdio form");
        assert!(cli.is_stdio_mode());
        assert_eq!(cli.tool_call_id.as_deref(), Some("c1"));

        // 子命令形态：绑定到 RunArgs，顶层为 None（guard 因此不会误伤）。
        let cli = Cli::try_parse_from(["wing", "run", "-p", "answer", "--tool-call-id", "c2"])
            .expect("run form");
        assert_eq!(cli.tool_call_id, None);
        match cli.command {
            Some(Command::Run(args)) => assert_eq!(args.tool_call_id.as_deref(), Some("c2")),
            other => panic!("expected run, got {other:?}"),
        }
    }

    /// 放错位置（子命令之前 / 没有子命令却不在 stdio）的顶层 `--tool-call-id`
    /// 被显式拒绝，而不是静默忽略。
    #[test]
    fn misplaced_tool_call_id_is_rejected_outside_stdio() {
        assert!(misplaced_tool_call_id_error(None).is_none());

        let message = misplaced_tool_call_id_error(Some("c1")).expect("must be rejected");
        assert!(message.contains("stdio"), "{message}");
        assert!(message.contains("--tool-call-id"), "{message}");

        // clap 接受这种写法（flag 绑在顶层）——guard 是唯一防线。
        let cli = Cli::try_parse_from(["wing", "--tool-call-id", "c1", "ps"]).expect("parses");
        assert_eq!(cli.tool_call_id.as_deref(), Some("c1"));
        assert!(!cli.is_stdio_mode());
        assert!(misplaced_tool_call_id_error(cli.tool_call_id.as_deref()).is_some());
    }
}
