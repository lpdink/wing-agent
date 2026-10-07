//! stdio frontend — non-interactive mode for external orchestrators and scripts.
//!
//! Triggered by `-p` / `--prompt` or `--input-format stream-json`.
//! Supports three output formats: text, json, stream-json.
// stderr 承载诊断（`wing error: …` / session_id 提示）；stdout 已被 stdio 协议占用。
#![allow(clippy::print_stderr)]

pub mod ndjson;
pub mod renderer;
pub mod stdin_handler;
pub mod stdout;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cmd::backend_config;
use crate::cmd::start;
use crate::gateway::GatewayClient;
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::models::{AgentOverride, CreateSessionRequest};

use self::renderer::StdioRenderer;
use self::stdout::StdoutSink;

// ============================================================
// Output / Input format enums
// ============================================================

/// Output format for stdio mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Print only the final result text.
    #[default]
    Text,
    /// Print the final result as a single JSON object.
    Json,
    /// Stream NDJSON messages in real-time.
    StreamJson,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "stream-json" => Ok(Self::StreamJson),
            _ => Err(format!(
                "invalid output format: '{s}' (expected text, json, or stream-json)"
            )),
        }
    }
}

/// Input format for stdio mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InputFormat {
    /// Prompt from CLI argument.
    #[default]
    Text,
    /// Prompt from stdin as NDJSON (sub-task 4).
    StreamJson,
}

impl std::str::FromStr for InputFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "stream-json" => Ok(Self::StreamJson),
            _ => Err(format!(
                "invalid input format: '{s}' (expected text or stream-json)"
            )),
        }
    }
}

// ============================================================
// Stdio arguments
// ============================================================

/// Parsed CLI arguments for stdio mode.
#[derive(Debug, Clone)]
pub struct StdioArgs {
    pub prompt: String,
    pub model: Option<String>,
    pub resume: Option<String>,
    /// `--session-id`: create-or-adopt (mutually exclusive with `resume`).
    pub session_id: Option<String>,
    /// `--resume-session-at`: unsupported, rejected before any side effect.
    pub resume_session_at: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub effort: Option<String>,
    pub provider: Option<String>,
    pub tools: Option<String>,
    /// Tags attached to the session (created tagged; `-r` adds to the resumed one).
    pub tag: Vec<String>,
    pub output_format: OutputFormat,
    pub input_format: InputFormat,
    pub yolo: bool,
}

impl StdioArgs {
    /// 常驻（多轮）判定：输入 stream-json（stdin 是持久消息通道）且输出
    /// stream-json（每轮有独立终态帧，「轮」才有消费方）。
    ///
    /// text/json 输出与非 stream-json 输入都保持一次性语义（见 design.md D1）：
    /// 前者没有逐轮帧可对账（json 的契约是「单个 result 对象」），后者的 stdin
    /// 根本不是消息通道。
    pub fn is_resident(&self) -> bool {
        self.input_format == InputFormat::StreamJson
            && self.output_format == OutputFormat::StreamJson
    }
}

// ============================================================
// Argument gates (before any side effect)
// ============================================================

/// `--resume-session-at` 的拒绝文案（含取值的回显）。
///
/// 该旗标来自 Claude Code，语义是"把会话回退到某个点"——wing 没有历史截断
/// 能力，且**绝不能静默忽略**：编排方会据此认为上下文已回退，而 wing 实际
/// 保留着完整历史，两边的上下文认知就此错位（这才是本步要消灭的失败模式，
/// 不是"少一个功能"）。真实现留待后续评估。
pub fn resume_session_at_error(value: &str) -> String {
    format!(
        "--resume-session-at={value} is not supported: wing cannot truncate a session's \
         history, and pretending it did would desync your view of the conversation from \
         wing's actual state. Drop the flag (or start a clean history with --session-id)."
    )
}

/// stdio 参数闸门：在任何副作用（起网关 / 建会话 / 发请求）之前校验。
///
/// 只查"静默忽略"类问题：
/// 1. `--session-id` 与 `-r/--resume` 互斥——一个是 create-or-adopt、一个是
///    恢复既有会话，语义冲突，同时给出即报错（不猜调用方想要哪个）；
/// 2. resume / adopt 下不生效的覆盖旗标（`--system-prompt` / `--append-system-prompt` /
///    `--max-turns`）打日志警告——它们**不该**改链上前缀，但"给了没反应"
///    必须可诊断；
/// 3. 任何路径下都不生效的 `--provider`（单独给出，没有 `--model`）同样出声。
///
/// 警告只进日志（`$WING_HOME/tui/logs/`）：stderr 留给错误与 `session_id:` 行，
/// stdout 是协议流。
///
/// `Ok(())` = 可以继续；`Err(message)` = 打印 `wing error: {message}` 后以非零
/// 退出码结束。
pub fn validate_stdio_args(args: &StdioArgs) -> Result<(), String> {
    // `--resume-session-at`：wing 未实现会话截断，**绝不静默忽略**（默认的闸门
    // 在 `cmd::dispatch` 里更靠前生效；这里是 stdio 侧的独立保证——无论谁调用
    // `run_stdio`，这个旗标都不会被吃下去）。
    if let Some(value) = args.resume_session_at.as_deref() {
        return Err(resume_session_at_error(value));
    }

    if args.session_id.is_some() && args.resume.is_some() {
        return Err(
            "--session-id and -r/--resume are mutually exclusive: --session-id creates or \
             adopts the given id, -r/--resume restores an existing one. Pick one."
                .to_string(),
        );
    }

    // `--provider` 单独给出：切 provider 需要一个要切过去的模型（与
    // `session/update` 的"model 与 provider 必须成对"同一口径）——它在**任何**
    // 路径上都是 no-op，因此不看是否 resume。
    if provider_without_model(args) {
        tracing::warn!(
            "--provider without --model is a no-op: switching provider needs a model \
             to switch to; the session keeps its current provider"
        );
    }

    if args.resume.is_some() || args.session_id.is_some() {
        let ignored = resume_ignored_flags(args);
        if !ignored.is_empty() {
            tracing::warn!(
                flags = %ignored.join(", "),
                "ignored on resume/adopt: these change the conversation prefix or the \
                 session's own limits and are create-time only"
            );
        }
    }

    Ok(())
}

/// resume / adopt 下不生效的创建期旗标（纯函数，便于单测）。
fn resume_ignored_flags(args: &StdioArgs) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if args.system_prompt.is_some() {
        flags.push("--system-prompt");
    }
    if args.append_system_prompt.is_some() {
        flags.push("--append-system-prompt");
    }
    if args.max_turns.is_some() {
        flags.push("--max-turns");
    }
    flags
}

/// `--provider` 给了但没有 `--model`：no-op（纯函数，便于单测）。
fn provider_without_model(args: &StdioArgs) -> bool {
    args.provider.is_some() && args.model.is_none()
}

/// `--session-id` 未被网关兑现时的错误文案（纯函数，便于单测）。
///
/// 两种成因都要说清，否则调用方会把"id 别名"误判成"网关太旧"：
/// 1. 运行中的网关早于 create-or-adopt（重启即可）；
/// 2. 大小写 / Unicode 归一化不敏感的文件系统上，请求的 id 解析到**已存在**的
///    那个会话（``team-a`` 与 ``Team-A`` 是同一份日志）——网关按磁盘真名回应，
///    前端拒绝在别的 id 名下继续。
pub fn session_id_mismatch_error(requested: &str, returned: &str) -> String {
    format!(
        "gateway did not honour --session-id: asked for {requested}, got {returned} — \
         either the running gateway predates create-or-adopt (restart it: wing stop && \
         wing start), or {requested} resolves to the existing session {returned} on a \
         case / Unicode-normalization-insensitive filesystem (use the exact id)"
    )
}

// ============================================================
// Override assembly
// ============================================================

/// 创建语义的覆盖：全字段（stdio 一律 yolo）。
fn create_override(args: &StdioArgs) -> AgentOverride {
    AgentOverride {
        model: args.model.clone(),
        provider: args.provider.clone(),
        system_prompt: args.system_prompt.clone(),
        append_system_prompt: args.append_system_prompt.clone(),
        tools: normalize_tools(args.tools.as_deref()),
        max_turns: args.max_turns,
        effort: args.effort.clone(),
        yolo: Some(true),
    }
}

/// resume（含 adopt）语义的覆盖：**只装** model / provider / effort / tools。
///
/// `--system-prompt` / `--append-system-prompt` / `--max-turns` 不在其中：它们
/// 会改请求前缀或会话既有限额（网关侧也照此口径，见 `apply_resume_override`）。
/// 四个字段全空时返回 `None`——不发一个全空的覆盖体。
fn resume_override(args: &StdioArgs) -> Option<AgentOverride> {
    let override_ = AgentOverride {
        model: args.model.clone(),
        provider: args.provider.clone(),
        system_prompt: None,
        append_system_prompt: None,
        tools: normalize_tools(args.tools.as_deref()),
        max_turns: None,
        effort: args.effort.clone(),
        yolo: None,
    };
    let is_empty = override_.model.is_none()
        && override_.provider.is_none()
        && override_.tools.is_none()
        && override_.effort.is_none();
    (!is_empty).then_some(override_)
}

// ============================================================
// Argument filtering (for stdio mode compatibility with SDK)
// ============================================================

use std::collections::HashSet;

use clap::CommandFactory;

use crate::cmd::Cli;

/// Known flags derived from clap's [`Cli`] definition.
///
/// Built once at startup. Eliminates the need for hand-maintained
/// whitelists — adding a new `#[arg]` to [`Cli`] automatically
/// makes the filter aware.
struct KnownFlags {
    /// Long flags that take a value argument (e.g. `--model`, `--max-turns`).
    long_with_value: HashSet<String>,
    /// Long boolean flags (e.g. `--yolo`, `--help`, `--version`).
    long_bool: HashSet<String>,
    /// Short flags that take a value argument (e.g. `-p`, `-m`, `-r`).
    short_with_value: HashSet<char>,
    /// Short boolean flags.
    short_bool: HashSet<char>,
}

impl KnownFlags {
    fn from_cli() -> Self {
        let cmd = Cli::command();
        let mut long_with_value = HashSet::new();
        let mut long_bool = HashSet::new();
        let mut short_with_value = HashSet::new();
        let mut short_bool = HashSet::new();

        for arg in cmd.get_arguments() {
            let takes_value = arg.get_action().takes_values();
            if let Some(long) = arg.get_long() {
                let flag = format!("--{long}");
                if takes_value {
                    long_with_value.insert(flag);
                } else {
                    long_bool.insert(flag);
                }
            }
            if let Some(short) = arg.get_short() {
                if takes_value {
                    short_with_value.insert(short);
                } else {
                    short_bool.insert(short);
                }
            }
        }

        Self {
            long_with_value,
            long_bool,
            short_with_value,
            short_bool,
        }
    }
}

/// Check if stdio mode should be triggered based on raw args.
///
/// This runs **before** clap parsing to decide whether to filter unknown
/// arguments. Must stay in sync with `Cli::is_stdio_mode()` in `cmd/mod.rs`
/// which runs **after** parsing to decide dispatch. Both check for the same
/// triggers: `-p`/`--prompt` presence or `--input-format stream-json`.
pub fn is_stdio_mode(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-p" || arg == "--prompt" {
            return true;
        }
        if arg.starts_with("--prompt=") || arg.starts_with("-p=") {
            return true;
        }
        if arg == "--input-format" && i + 1 < args.len() && args[i + 1] == "stream-json" {
            return true;
        }
        if arg.starts_with("--input-format=stream-json") {
            return true;
        }
        i += 1;
    }
    false
}

/// Filter unknown arguments in stdio mode.
///
/// Strategy:
/// - **Known flags** (from clap): kept with correct value handling.
/// - **Unknown `--flag=value`**: dropped (single token).
/// - **Unknown `--flag value`**: heuristic — if the next token starts with
///   `-`, the flag is treated as boolean (dropped, next token preserved).
///   Otherwise the flag and its value are both dropped.
/// - **Positional / value arguments**: always kept.
///
/// This lets wing accept arbitrary SDK-injected flags (e.g. `--verbose`,
/// `--permission-mode bypassPermissions`) without maintaining a manual
/// whitelist of external flags.
pub fn filter_unknown_args(args: Vec<String>) -> Vec<String> {
    let known = KnownFlags::from_cli();
    let mut result = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        if arg.starts_with("--") {
            let flag_name = if let Some(eq_pos) = arg.find('=') {
                &arg[..eq_pos]
            } else {
                arg.as_str()
            };

            if known.long_with_value.contains(flag_name) {
                // Known value-carrying flag: keep flag + value.
                result.push(arg.clone());
                if !arg.contains('=') {
                    i += 1;
                    if i < args.len() {
                        result.push(args[i].clone());
                    }
                }
            } else if known.long_bool.contains(flag_name) {
                // Known boolean flag: keep flag only.
                result.push(arg.clone());
            } else if !arg.contains('=') {
                // Unknown --flag without =.
                // Heuristic: if next token starts with '-', this flag is
                // likely boolean (e.g. --verbose --system-prompt …).
                // Otherwise it likely takes a value (e.g. --permission-mode bypassPermissions).
                let next_is_flag = args.get(i + 1).is_some_and(|s| s.starts_with('-'));
                if !next_is_flag {
                    // Skip the value token.
                    i += 1;
                }
                // Drop the unknown flag itself.
            }
            // Unknown --flag=value (with =): drop this single token.
        } else if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let flag_char = arg.chars().nth(1).unwrap_or(' ');
            if known.short_with_value.contains(&flag_char) {
                result.push(arg.clone());
                if arg.len() == 2 {
                    i += 1;
                    if i < args.len() {
                        result.push(args[i].clone());
                    }
                }
            } else if known.short_bool.contains(&flag_char) {
                result.push(arg.clone());
            }
            // Unknown short flag: drop.
        } else {
            // Positional or value — keep.
            result.push(arg.clone());
        }

        i += 1;
    }

    result
}

// ============================================================
// `--tools` normalization
// ============================================================

/// Claude Agent SDK 的 tools preset 值（`--tools default`）。
const TOOLS_PRESET_DEFAULT: &str = "default";

/// 归一化 `--tools`：返回 `None` = 不覆盖工具集（让会话模板生效）。
///
/// SDK 固定传 `--tools default`（tools preset）或 `--tools ""`（空列表）——它们
/// **不是**工具名列表：照既有语义切分会把模板的工具集覆盖成「名字叫 default 的
/// 工具」/ 空集。其他值（真正的工具名列表）保持 wing 既有语义：逗号切分、trim、
/// 丢弃空项。
fn normalize_tools(raw: Option<&str>) -> Option<Vec<String>> {
    let raw = raw?.trim();
    if raw.is_empty() || raw == TOOLS_PRESET_DEFAULT {
        return None;
    }
    Some(
        raw.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
    )
}

// ============================================================
// Gateway auto-start (extracted from smart_default_tui)
// ============================================================

/// Ensure gateway is running, returning (host, port).
///
/// Checks health endpoint first; if gateway is not reachable, starts it.
pub async fn ensure_gateway_running() -> Result<(String, u16)> {
    let gw_config = backend_config::read_backend_gateway_config();

    // Check if gateway is already running via health check.
    let http_base = format!("http://{}:{}", gw_config.host, gw_config.port);
    let already_running = if let Ok(client) = wing_api_client::GatewayClient::new(&http_base, None)
    {
        matches!(
            client.health().await,
            Ok(h) if h.service == "wing-gateway"
        )
    } else {
        false
    };

    if !already_running {
        start::start_gateway(&gw_config.host, gw_config.port).await?;
    }

    Ok((gw_config.host, gw_config.port))
}

// ============================================================
// Resident (multi-turn) mode
// ============================================================

/// stdin pump 的消息投递策略（纯函数，便于单测）。
///
/// - 常驻：每条 `user` 消息都投递（轮间起新轮、轮中转发给网关 inbox）；
/// - 一次性 + prompt 来自 stdin：只投递首条（它就是 prompt）；
/// - 一次性 + prompt 来自 CLI：一条都不投（既有单轮语义）。
fn message_delivery(resident: bool, prompt_from_cli: bool) -> stdin_handler::MessageDelivery {
    match (resident, prompt_from_cli) {
        (true, _) => stdin_handler::MessageDelivery::All,
        (false, true) => stdin_handler::MessageDelivery::Ignore,
        (false, false) => stdin_handler::MessageDelivery::FirstOnly,
    }
}

/// 常驻循环的退出判定（纯状态机，无 I/O——EOF 边界由单测钉住）。
///
/// 常驻语义（`--input-format stream-json` + `--output-format stream-json`）：
/// 终态帧只结束**当前轮**，进程退出由 stdin EOF 收尾决定：
///
/// - EOF + 空闲（无在途轮）→ 立即退；
/// - EOF + 有在途轮 → 等该轮终态后退出；
/// - 轮间/轮中收到 `user` 消息 → 转发（网关 inbox 决定 steer / 排队）。该消息
///   驱动的轮（无论起没起）在下一次终态前都被观察到——**除了**「在轮中被排队、
///   当前轮结束才轮到它」的那条：EOF 后不等它。裁定见 design.md D5：消费方关
///   stdin 即表示不再关心后续输出。
#[derive(Debug, Clone, Copy)]
struct ExitPolicy {
    /// 有在途轮，或刚转发、尚未见到终态帧的消息。
    in_turn: bool,
    /// stdin 已关闭。
    eof: bool,
}

impl ExitPolicy {
    /// 循环起点：`prompt_sent` = 首轮 prompt 已经发出（它必然驱动一轮，即使
    /// 它的终态帧还没被观察到——首轮 prompt 在进入循环**之前**就发出去了）。
    fn new(prompt_sent: bool) -> Self {
        Self {
            in_turn: prompt_sent,
            eof: false,
        }
    }

    /// 已把一条 `user` 消息交给网关：它必然驱动一轮（起新轮或并入当前轮）。
    fn on_message(&mut self) {
        self.in_turn = true;
    }

    /// stdin 关闭 / EOF：`true` = 现在就退。
    fn on_eof(&mut self) -> bool {
        self.eof = true;
        !self.in_turn
    }

    /// 收到终态帧（本轮结束）：`true` = 现在就退（EOF 已到且无事在途）。
    fn on_terminal(&mut self) -> bool {
        self.in_turn = false;
        self.eof
    }
}

// ============================================================
// stdio entry point
// ============================================================

/// Run stdio mode: create/resume session, send prompt, output results.
pub async fn run_stdio(args: StdioArgs) -> ExitCode {
    // 参数闸门先于任何副作用：不启网关、不建会话、不发请求。
    if let Err(message) = validate_stdio_args(&args) {
        eprintln!("wing error: {message}");
        return ExitCode::FAILURE;
    }
    match run_stdio_inner(args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wing error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// 一次性事件循环（既有语义）：终态帧结束**进程**。
async fn run_one_shot_loop(gateway: &mut GatewayClient, renderer: &mut StdioRenderer) -> ExitCode {
    // 终态有两个来源，都由 renderer 落到 stdout 并返回 `true`：
    // `turn_result`（正常/失败收口），以及 `interrupted`（被打断的轮次后端不发
    // turn_result，前端补一条终态 result 帧——见 `handle_interrupted`）。
    loop {
        match gateway.recv_event().await {
            Some(event) => {
                if renderer.handle_event(&event) {
                    return renderer.exit_code();
                }
            }
            None => {
                tracing::warn!("WS connection closed before TurnResult");
                return ExitCode::FAILURE;
            }
        }
    }
}

/// 常驻事件循环：终态帧只结束当前轮；stdin 消息逐条转发；EOF 收尾。
///
/// 与一次性循环的区别只有两处：终态帧之后由 [`ExitPolicy`] 裁决是否退出；stdin
/// 上的 `user` 消息逐条转发 `POST /api/session/send`（轮间消息起新轮、轮中消息由
/// 网关 inbox 决定 steer / 排队，**不丢弃**）。
async fn run_resident_loop(
    gateway: &mut GatewayClient,
    pump: &mut stdin_handler::StdinPump,
    http: &GatewayApiClient,
    session_id: &str,
    renderer: &mut StdioRenderer,
    prompt_sent: bool,
) -> Result<ExitCode> {
    let mut policy = ExitPolicy::new(prompt_sent);
    // EOF 之后 pump 通道会立刻反复返回 `None`——关掉这个 select 分支，否则
    // select 每次都被它唤醒、空转烧 CPU（与 `wait_fail_fast_e2e` 事故同型）。
    let mut pump_open = true;

    loop {
        tokio::select! {
            event = gateway.recv_event() => match event {
                Some(event) => {
                    // 终态帧（含 interrupted 合成帧）= 本轮结束。
                    if renderer.handle_event(&event) && policy.on_terminal() {
                        return Ok(renderer.exit_code());
                    }
                }
                None => {
                    tracing::warn!("gateway event stream closed before stdin closed");
                    return Ok(ExitCode::FAILURE);
                }
            },
            message = pump.next_message(), if pump_open => match message {
                Some(text) if text.is_empty() => {
                    // 空消息在后端不会驱动任何一轮（`run_turn` 对空 content 直接
                    // return，不发终态帧）——转发它会让 EOF 收尾永远等下去。
                    tracing::warn!("ignoring empty user message (it would never drive a turn, and its terminal frame would never come)");
                }
                Some(text) => {
                    // 转发：网关 inbox 决定 steer（轮到当前）还是排队（成新轮）。
                    http.send_message(session_id, &text, None)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to send message: {e}"))?;
                    policy.on_message();
                }
                None => {
                    pump_open = false;
                    if policy.on_eof() {
                        return Ok(renderer.exit_code());
                    }
                }
            },
        }
    }
}

async fn run_stdio_inner(args: StdioArgs) -> Result<ExitCode> {
    let start_time = Instant::now();

    // 1. Ensure gateway is running.
    let (host, port) = ensure_gateway_running().await?;

    let ws_url = format!("ws://{host}:{port}/ws");
    let http_base = format!("http://{host}:{port}");

    tracing::info!("stdio mode: gateway at {ws_url}");

    // Load API key from TUI config.
    let api_key = crate::config::AppConfig::load()
        .api_key
        .filter(|k| !k.is_empty());
    let api_key_ref = api_key.as_deref();

    // 2. WS connect.
    let mut gateway = GatewayClient::connect(&ws_url, api_key_ref)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to connect to gateway at {ws_url}: {e}\n\
             Make sure the gateway is running: wing start"
            )
        })?;

    let client_id = gateway.client_id().to_string();
    tracing::info!(client_id = %client_id, "WS connected");

    // 3. HTTP client.
    let http = GatewayApiClient::new(http_base, api_key_ref)
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;

    // 4. Create, adopt, or resume the session.
    //
    // 三条路径的语义差：`-r/--resume` 恢复既有会话（覆盖按 resume 子集）；
    // `--session-id` 是 create-or-adopt（同一个端点两种结果，覆盖语义由网关
    // 决定：新建 = 创建语义，收养 = resume 子集）；都不给 = 自生成 id。
    let session_id = if let Some(ref resume_id) = args.resume {
        let resp = http
            .resume_session_with_override(resume_id, resume_override(&args).as_ref())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to resume session: {e}"))?;
        // `-r` + `--tag`: add tags to the resumed session before anything is sent.
        if !args.tag.is_empty() {
            http.tag_session(&resp.session_id, Some(args.tag.clone()), None)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to tag session: {e}"))?;
        }
        tracing::info!(session_id = %resp.session_id, "session resumed");
        // Print session_id to stderr so it doesn't pollute stdout
        // (which carries the Claude Code protocol stream).
        eprintln!("session_id: {}", resp.session_id);
        resp.session_id
    } else {
        let workspace = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string());

        let create_req = CreateSessionRequest {
            template_name: None,
            workspace,
            agent: Some(create_override(&args)),
            backend: None,
            // Atomic: the session is born tagged (no dispatch-without-tags window).
            tags: (!args.tag.is_empty()).then(|| args.tag.clone()),
            // create-or-adopt：给了 id 就是它（已存在则被收养）。
            session_id: args.session_id.clone(),
        };

        let resp = http
            .create_session(&create_req)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create session: {e}"))?;
        // 创建即带标；读回校验——旧网关（早于 tags 端点）在这里响亮失败，
        // 而不是静默丢标后继续把 prompt 发出去。
        crate::cmd::common::ensure_tags_applied(&http, &resp.session_id, &args.tag).await?;
        // 指定 id 时必须精确一致：网关若悄悄换一个 id（旧版本忽略该字段），
        // 编排方的续链认知会立刻错位；别名（大小写 / 归一化不敏感 FS）同理——
        // 在发请求之前就响亮失败。
        if let Some(requested) = args.session_id.as_deref() {
            anyhow::ensure!(
                resp.session_id == requested,
                "{}",
                session_id_mismatch_error(requested, &resp.session_id)
            );
        }
        tracing::info!(
            session_id = %resp.session_id,
            requested_id = ?args.session_id,
            "session ready (created, or adopted when --session-id matched an existing one)"
        );
        // Print session_id to stderr for recovery/reference.
        eprintln!("session_id: {}", resp.session_id);
        resp.session_id
    };

    // 5. HTTP subscribe.
    http.subscribe(&session_id, &client_id)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe to session: {e}"))?;

    tracing::info!("subscribed to session events");

    // 6. Build renderer. stdout 只有一个出口：renderer 的协议帧与 stdin pump 的
    //    control 应答共享同一个 sink（见 `stdout::StdoutSink`）。
    let out = Arc::new(StdoutSink::stdout());
    let mut renderer = StdioRenderer::new(
        args.output_format.clone(),
        start_time,
        session_id.clone(),
        Arc::clone(&out),
    );

    // 7. stdin pump：stream-json 输入模式下 stdin 是常驻通道——turn 期间仍要消费
    //    control_request（interrupt 等）并应答（编排器在 await 它们）；常驻模式下
    //    每条 `user` 消息都要投递给驱动侧转发。
    let resident = args.is_resident();
    let mut pump = None;
    if args.input_format == InputFormat::StreamJson {
        pump = Some(stdin_handler::spawn(stdin_handler::StdinPumpContext {
            interruptor: Arc::new(stdin_handler::GatewayInterruptor::new(
                http.clone(),
                session_id.clone(),
            )),
            out: Arc::clone(&out),
            delivery: message_delivery(resident, !args.prompt.is_empty()),
        }));
    }

    // 8. Resolve prompt: CLI arg > stdin (stream-json) > error.
    let prompt = if !args.prompt.is_empty() {
        args.prompt.clone()
    } else if let Some(pump) = pump.as_mut() {
        // 这里的错误 = stdin 在首条 `user` 之前关闭（见 `wait_prompt` 语义：
        // 通道关闭且没有缓冲消息），无需再 finish。
        pump.wait_prompt().await?
    } else {
        anyhow::bail!("no prompt provided");
    };

    // 9. Send prompt（首轮）。
    //
    // 常驻模式下空文本 prompt 不发：后端对空 content 不起轮（`run_turn` 直接
    // return、不发任何终态帧），发了就等于挂在这里等一个永不发生的终态。跳过它，
    // 后续消息照常成轮；一次性路径保持既有行为不动。
    let prompt_sent = if resident && prompt.is_empty() {
        tracing::warn!("empty prompt in resident mode: skipping the initial send");
        false
    } else {
        if let Err(e) = http.send_message(&session_id, &prompt, None).await {
            // 收尾纪律：还没进事件循环就退出——先把 pump 收干净，别把半收尾的
            // 任务 detach 到运行时回收。
            if let Some(pump) = pump.take() {
                pump.finish().await;
            }
            return Err(anyhow::anyhow!("Failed to send message: {e}"));
        }
        true
    };

    if resident {
        tracing::info!("prompt sent, entering resident event loop (multi-turn)");
    } else {
        tracing::info!("prompt sent, entering event loop");
    }

    // 10. Event loop。区别只在退出裁决：
    //     - 一次性：终态帧结束进程（既有语义）；
    //     - 常驻（`--input-format stream-json` + `--output-format stream-json`）：
    //       终态帧只结束当前轮，EOF 收尾（空闲置退 / 在途轮等终态）。
    let exit_code = if resident {
        let pump = pump
            .as_mut()
            .expect("resident mode always spawns the stdin pump");
        run_resident_loop(
            &mut gateway,
            pump,
            &http,
            &session_id,
            &mut renderer,
            prompt_sent,
        )
        .await
    } else {
        Ok(run_one_shot_loop(&mut gateway, &mut renderer).await)
    };

    // 11. 收尾：通知 stdin pump 停下（有界等待进行中的应答写完——它可能恰好
    //     跨过轮结束，直接 abort 会让编排器收不到响应）。
    if let Some(pump) = pump.take() {
        pump.finish().await;
    }

    exit_code
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_stdio_mode ----

    #[test]
    fn stdio_mode_triggered_by_short_prompt() {
        let args: Vec<String> = vec!["-p", "hello"].into_iter().map(String::from).collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_triggered_by_long_prompt() {
        let args: Vec<String> = vec!["--prompt", "hello"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_triggered_by_prompt_equals() {
        let args: Vec<String> = vec!["--prompt=hello"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_triggered_by_input_format_stream_json() {
        let args: Vec<String> = vec!["--input-format", "stream-json"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_triggered_by_input_format_equals_stream_json() {
        let args: Vec<String> = vec!["--input-format=stream-json"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_not_triggered_by_input_format_text() {
        let args: Vec<String> = vec!["--input-format", "text"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(!is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_not_triggered_by_empty_args() {
        let args: Vec<String> = vec![];
        assert!(!is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_not_triggered_by_tui_subcommand() {
        let args: Vec<String> = vec!["tui"].into_iter().map(String::from).collect();
        assert!(!is_stdio_mode(&args));
    }

    // ---- filter_unknown_args ----

    #[test]
    fn filter_keeps_known_flags() {
        let args: Vec<String> = vec!["-p", "hello", "--output-format", "json"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "--output-format", "json"]);
    }

    #[test]
    fn filter_drops_unknown_value_flags() {
        let args: Vec<String> = vec![
            "-p",
            "hello",
            "--permission-mode",
            "bypassPermissions",
            "--verbose",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello"]);
    }

    #[test]
    fn filter_unknown_bool_before_known_flag() {
        // --verbose is unknown boolean; next token --system-prompt starts with '-'
        // so the heuristic treats --verbose as boolean and preserves --system-prompt.
        let args: Vec<String> = vec!["-p", "hello", "--verbose", "--system-prompt", ""]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "--system-prompt", ""]);
    }

    #[test]
    fn filter_multiple_unknown_bools_before_known_flag() {
        let args: Vec<String> = vec![
            "-p",
            "hello",
            "--verbose",
            "--include-partial-messages",
            "--max-turns",
            "5",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "--max-turns", "5"]);
    }

    #[test]
    fn filter_keeps_yolo_flag() {
        let args: Vec<String> = vec!["-p", "hello", "--yolo"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "--yolo"]);
    }

    #[test]
    fn filter_keeps_equals_style_flags() {
        let args: Vec<String> = vec!["--prompt=hello", "--output-format=json"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["--prompt=hello", "--output-format=json"]);
    }

    #[test]
    fn filter_drops_unknown_equals_style_flags() {
        let args: Vec<String> = vec!["-p", "hello", "--unknown-flag=value"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello"]);
    }

    #[test]
    fn filter_handles_model_flag() {
        let args: Vec<String> = vec!["-p", "hello", "-m", "gpt-4o"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "-m", "gpt-4o"]);
    }

    #[test]
    fn filter_handles_resume_flag() {
        let args: Vec<String> = vec!["-p", "hello", "-r", "abc123"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "-r", "abc123"]);
    }

    #[test]
    fn filter_sdk_command_line() {
        // Full command line as constructed by claude-agent-sdk-python's
        // SubprocessCLITransport._build_command().
        let args: Vec<String> = vec![
            "--output-format",
            "stream-json",
            "--verbose",
            "--system-prompt",
            "",
            "--max-turns",
            "5",
            "--permission-mode",
            "bypassPermissions",
            "--input-format",
            "stream-json",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(
            filtered,
            vec![
                "--output-format",
                "stream-json",
                "--system-prompt",
                "",
                "--max-turns",
                "5",
                "--input-format",
                "stream-json",
            ]
        );
    }

    #[test]
    fn filter_complex_mixed() {
        let args: Vec<String> = vec![
            "-p",
            "list files",
            "--output-format",
            "stream-json",
            "--permission-mode",
            "bypassPermissions",
            "--max-turns",
            "5",
            "--yolo",
            "--some-other-flag",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(
            filtered,
            vec![
                "-p",
                "list files",
                "--output-format",
                "stream-json",
                "--max-turns",
                "5",
                "--yolo",
            ]
        );
    }

    // ---- normalize_tools ----

    #[test]
    fn tools_absent_keeps_template_defaults() {
        assert_eq!(normalize_tools(None), None);
    }

    #[test]
    fn tools_preset_and_empty_values_are_dropped() {
        // SDK 固定传 `--tools default` / `--tools ""`——两者都不是工具名列表，
        // 必须丢弃（= 用模板默认），否则模板工具集被覆盖成空集/假工具名。
        assert_eq!(normalize_tools(Some("")), None);
        assert_eq!(normalize_tools(Some("   ")), None);
        assert_eq!(normalize_tools(Some("default")), None);
        assert_eq!(normalize_tools(Some("  default  ")), None);
    }

    #[test]
    fn real_tool_lists_keep_existing_semantics() {
        assert_eq!(normalize_tools(Some("Read")), Some(vec!["Read".into()]));
        assert_eq!(
            normalize_tools(Some("Read,Bash")),
            Some(vec!["Read".into(), "Bash".into()])
        );
        assert_eq!(
            normalize_tools(Some("Read, ,Bash")),
            Some(vec!["Read".into(), "Bash".into()])
        );
        assert_eq!(
            normalize_tools(Some(" Read , Bash ")),
            Some(vec!["Read".into(), "Bash".into()])
        );
        // 只丢弃"整个值就是语义值"的情形，混合值按既有语义处理。
        assert_eq!(
            normalize_tools(Some("default,Read")),
            Some(vec!["default".into(), "Read".into()])
        );
    }

    // ---- 参数闸门 / 覆盖装配 ----

    /// 测试用 StdioArgs（默认全空 + prompt）。
    fn args() -> StdioArgs {
        StdioArgs {
            prompt: "hi".into(),
            model: None,
            resume: None,
            session_id: None,
            resume_session_at: None,
            system_prompt: None,
            append_system_prompt: None,
            max_turns: None,
            effort: None,
            provider: None,
            tools: None,
            tag: Vec::new(),
            output_format: OutputFormat::Text,
            input_format: InputFormat::Text,
            yolo: false,
        }
    }

    #[test]
    fn resume_session_at_is_rejected_by_the_stdio_gate() {
        let mut a = args();
        a.resume_session_at = Some("3".into());
        let message = validate_stdio_args(&a).expect_err("must be rejected");
        assert!(message.contains("--resume-session-at=3"), "{message}");
    }

    #[test]
    fn session_id_and_resume_are_mutually_exclusive() {
        let mut a = args();
        a.session_id = Some("abc".into());
        assert!(validate_stdio_args(&a).is_ok());

        a.resume = Some("abc".into());
        let message = validate_stdio_args(&a).expect_err("both flags must be rejected");
        assert!(message.contains("--session-id"), "{message}");
        assert!(message.contains("--resume"), "{message}");
    }

    #[test]
    fn ignored_override_flags_do_not_block_resume() {
        // 不改链上前缀的旗标在 resume 下不生效，但只警告、不阻断（否则
        // "SDK 照旧传全套旗标" 的场景会被硬拒绝）。
        let mut a = args();
        a.resume = Some("abc".into());
        a.system_prompt = Some("x".into());
        a.max_turns = Some(3);
        assert!(validate_stdio_args(&a).is_ok());
    }

    #[test]
    fn resume_override_carries_only_the_resume_subset() {
        let mut a = args();
        a.model = Some("m".into());
        a.provider = Some("p".into());
        a.effort = Some("high".into());
        a.tools = Some("Read,Bash".into());
        // 创建期旗标：不得进入 resume 覆盖。
        a.system_prompt = Some("SYS".into());
        a.append_system_prompt = Some("APP".into());
        a.max_turns = Some(7);

        let override_ = resume_override(&a).expect("non-empty override");
        assert_eq!(override_.model.as_deref(), Some("m"));
        assert_eq!(override_.provider.as_deref(), Some("p"));
        assert_eq!(override_.effort.as_deref(), Some("high"));
        assert_eq!(override_.tools, Some(vec!["Read".into(), "Bash".into()]));
        assert_eq!(override_.system_prompt, None);
        assert_eq!(override_.append_system_prompt, None);
        assert_eq!(override_.max_turns, None);
        assert_eq!(override_.yolo, None);
    }

    #[test]
    fn resume_override_is_absent_when_nothing_to_override() {
        let mut a = args();
        a.resume = Some("abc".into());
        assert!(resume_override(&a).is_none());

        // SDK 的语义值（`--tools default` / 空）不算覆盖。
        a.tools = Some("default".into());
        assert!(resume_override(&a).is_none());
        a.tools = Some(String::new());
        assert!(resume_override(&a).is_none());
    }

    #[test]
    fn create_override_keeps_full_semantics_and_yolo() {
        let mut a = args();
        a.system_prompt = Some("SYS".into());
        a.append_system_prompt = Some("APP".into());
        a.max_turns = Some(7);
        a.tools = Some("default".into()); // 语义值丢弃 → None

        let override_ = create_override(&a);
        assert_eq!(override_.system_prompt.as_deref(), Some("SYS"));
        assert_eq!(override_.append_system_prompt.as_deref(), Some("APP"));
        assert_eq!(override_.max_turns, Some(7));
        assert_eq!(override_.tools, None);
        assert_eq!(override_.yolo, Some(true));
    }

    #[test]
    fn resume_session_at_error_is_explicit() {
        let message = resume_session_at_error("3");
        assert!(message.contains("--resume-session-at=3"), "{message}");
        assert!(message.contains("not supported"), "{message}");
    }

    // ---- 不生效旗标的识别（纯函数） ----

    #[test]
    fn provider_without_model_is_recognised() {
        let mut a = args();
        assert!(!provider_without_model(&a)); // 都没有 = 没问题

        a.provider = Some("p".into());
        assert!(provider_without_model(&a), "单独的 --provider 是 no-op");

        a.model = Some("m".into());
        assert!(!provider_without_model(&a), "--provider + --model 成对生效");
    }

    #[test]
    fn resume_ignored_flags_lists_create_time_flags() {
        let mut a = args();
        assert!(resume_ignored_flags(&a).is_empty());

        a.system_prompt = Some("s".into());
        a.append_system_prompt = Some("t".into());
        a.max_turns = Some(3);
        assert_eq!(
            resume_ignored_flags(&a),
            vec!["--system-prompt", "--append-system-prompt", "--max-turns"]
        );

        // provider 不由这里管（它在任何路径上都是 no-op，单独一套消息）。
        a.provider = Some("p".into());
        assert!(!resume_ignored_flags(&a).contains(&"--provider"));
    }

    // ---- session id 兑现闸门（防御旧网关 / 别名） ----

    #[test]
    fn session_id_mismatch_error_names_both_causes() {
        let message = session_id_mismatch_error("team-a", "Team-A");
        assert!(message.contains("asked for team-a"), "{message}");
        assert!(message.contains("got Team-A"), "{message}");
        assert!(message.contains("predates create-or-adopt"), "{message}");
        assert!(message.contains("filesystem"), "{message}");
        assert!(message.contains("use the exact id"), "{message}");
    }

    // ---- 常驻判定 / 消息投递策略 ----

    fn resident_args() -> StdioArgs {
        let mut a = args();
        a.input_format = InputFormat::StreamJson;
        a.output_format = OutputFormat::StreamJson;
        a
    }

    #[test]
    fn resident_requires_stream_json_input_and_output() {
        assert!(resident_args().is_resident());

        // 输入 stream-json 但输出 text/json：一次性语义（json 的契约是单个
        // result 对象，没有逐轮帧可对账）。
        let mut text_out = resident_args();
        text_out.output_format = OutputFormat::Text;
        assert!(!text_out.is_resident());

        let mut json_out = resident_args();
        json_out.output_format = OutputFormat::Json;
        assert!(!json_out.is_resident());

        // 非 stream-json 输入（prompt 一次性给出）：一次性语义。
        let mut text_in = resident_args();
        text_in.input_format = InputFormat::Text;
        assert!(!text_in.is_resident());
        assert!(!args().is_resident());
    }

    #[test]
    fn message_delivery_matrix() {
        use stdin_handler::MessageDelivery;

        // 常驻：无论 prompt 来源，每条消息都要投递（轮间/轮中新轮）。
        assert_eq!(
            message_delivery(true, false),
            MessageDelivery::All,
            "常驻 + prompt 来自 stdin：首条是 prompt，其余是新轮"
        );
        assert_eq!(
            message_delivery(true, true),
            MessageDelivery::All,
            "常驻 + prompt 来自 CLI：stdin 消息全是新轮"
        );

        // 一次性：只认首条（prompt 来自 stdin），或一条都不认（prompt 来自 CLI）。
        assert_eq!(message_delivery(false, false), MessageDelivery::FirstOnly);
        assert_eq!(message_delivery(false, true), MessageDelivery::Ignore);
    }

    // ---- ExitPolicy：EOF 收尾判定 ----

    #[test]
    fn eof_while_idle_exits_immediately() {
        let mut policy = ExitPolicy::new(false);
        assert!(policy.on_eof(), "空闲 + EOF → 立即退出");
    }

    /// 首轮 prompt 在进入常驻循环**之前**就发出去了：EOF 紧接着来也不能退，
    /// 要等它的终态帧（否则第一轮的输出直接丢失）。
    #[test]
    fn eof_right_after_the_prompt_waits_for_the_first_turn() {
        let mut policy = ExitPolicy::new(true);
        assert!(!policy.on_eof(), "首轮在飞：EOF 不能直接退");
        assert!(policy.on_terminal(), "首轮终态到达后才退");
    }

    #[test]
    fn eof_while_in_turn_waits_for_the_terminal_frame() {
        let mut policy = ExitPolicy::new(false);
        policy.on_message();
        assert!(!policy.on_eof(), "有在途轮：EOF 不能直接退");
        assert!(policy.on_terminal(), "该轮终态到达后才退");
    }

    #[test]
    fn terminal_without_eof_keeps_the_process_alive() {
        let mut policy = ExitPolicy::new(false);
        policy.on_message();
        assert!(!policy.on_terminal(), "常驻：终态帧只结束当前轮");
        // 轮间再来一轮，同样不退出。
        policy.on_message();
        assert!(!policy.on_terminal());
        // 直到 EOF。
        assert!(policy.on_eof());
    }

    /// 上一轮结束后才关 stdin（CloudCLI 的 result → release(EOF) → 退出）：
    /// EOF 时已经空闲 → 立即退出（退出码 = 该轮结果）。
    #[test]
    fn eof_after_the_last_turn_exits_immediately() {
        let mut policy = ExitPolicy::new(true);
        assert!(!policy.on_terminal(), "轮结束而 stdin 还开着 → 继续等");
        assert!(policy.on_eof(), "空闲 + EOF → 立即退出");
    }

    /// 轮中投递的第二条消息（steer / 排队）：EOF 后仍在**当前轮**的终态帧上
    /// 收尾——不额外等它自己那一轮（design.md D5 的裁定：排队的那一轮不等）。
    #[test]
    fn eof_settles_on_the_current_turns_terminal_frame() {
        let mut policy = ExitPolicy::new(false);
        policy.on_message();
        policy.on_message(); // 轮中再投一条
        assert!(!policy.on_eof());
        assert!(policy.on_terminal(), "当前轮终态即收尾");
    }

    /// 被中断的轮同样是「终态」：EOF 已到则立即收尾（CloudCLI 的
    /// interrupt → release → 退出 形状）。
    #[test]
    fn interrupted_terminal_closes_a_waiting_eof() {
        let mut policy = ExitPolicy::new(false);
        policy.on_message();
        assert!(!policy.on_eof());
        assert!(
            policy.on_terminal(),
            "合成终态帧同样结束当前轮（EOF 已到 → 退出）"
        );
    }
}
