//! StdioRenderer — converts WingEvent to stdout output based on format.
// 错误轮把错误文本写到 stderr（stdout 是协议通道，只能有协议帧）。
#![allow(clippy::print_stderr)]

use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize as _;

use crate::protocol::WingEvent;
use crate::stdio::OutputFormat;
use crate::stdio::ndjson::{
    AssistantMessage, MessageContent, ResultMessage, StreamEventFrame, SystemInitMessage,
    UserMessage,
};
use crate::stdio::stdout::StdoutSink;
use crate::stdio::stream::StreamState;

/// Renders WingEvents to stdout in the specified format.
pub struct StdioRenderer {
    format: OutputFormat,
    start_time: Instant,
    session_id: String,
    exit_code: std::process::ExitCode,
    /// 协议帧的出口。与 stdin pump 共享同一个 sink——stdout 只能有一个写者，
    /// 见 [`StdoutSink`]。
    out: Arc<StdoutSink>,
    /// 已发出、还没等到结果的工具调用数（stream-json 模式下由 assistant /
    /// tool_result 帧维护）。
    pending_tools: i64,
    /// 本相位里是否出现过「中断收口合成的工具结果」（见
    /// [`INTERRUPTED_TOOL_RESULT`]）——工具阶段被打断的判据之一。
    tool_phase_aborted: bool,
    /// 自上一次**关轮**以来，是否已经出过终态帧（`turn_result` / `interrupted`
    /// 合成帧 / `done` 补的失败帧）。关轮点是 `turn_started`、`done` 与 `interrupted`
    /// ——三者不保证成对出现，写入统一走 [`StdioRenderer::reset_terminal_ledger`]。
    ///
    /// `done` 据此决定补不补帧（见 [`StdioRenderer::handle_done`]）。
    terminal_rendered: bool,
    /// 本轮最近一条后端 `Error` 事件原文（随轮边界与轮末清空）。
    ///
    /// `error` 不是轮末信号，但后端异常收口时**真实原因只在它里面**——补帧时把
    /// 它带进 `errors`，否则消费方只知道「这一轮没有结果」。
    last_error: Option<String>,
    /// `--include-partial-messages`：是否输出 `stream_event` 增量帧。
    /// **假 = 零行为变化**（delta 不产帧，快照帧 id 仍是既有形状）。
    include_partial_messages: bool,
    /// 增量帧状态机（仅在 [`Self::include_partial_messages`] 为真时被喂入）。
    /// 跨轮不泄漏：`begin_turn` 防御性收口在途消息。
    stream: StreamState,
}

/// 错误轮写 stderr 的文案。
///
/// `result` 只在成功轮有值；错误轮的后端详情在 `errors`（例如
/// `format_exception_chain` 的全文）。两者都试，别把真实错误丢成通用文案。
fn error_line_text(result: Option<&str>, errors: &[String]) -> String {
    result
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| (!errors.is_empty()).then(|| errors.join("; ")))
        .unwrap_or_else(|| "error during execution".to_string())
}

/// 错误结果帧的 `errors`（`SDKResultError.errors`）。
///
/// **错误帧永不省略该字段**：SDK 对 `is_error=true && subtype != "success"`
/// 的结果帧无条件读它（0.3.165：`e.errors.join("; ")`；0.3.291：
/// `e.errors.map(…)`），缺了会抛 `TypeError`，把后端错误原文换成 JS 内部错误。
/// 后端没给文本时补一条通用文案，保证字段非空。
fn error_texts(is_error: bool, errors: &[String]) -> Vec<String> {
    if !is_error {
        // 成功帧不带（`SDKResultSuccess` 没有 `errors` 字段）。
        return Vec::new();
    }
    if errors.is_empty() {
        vec!["error during execution".to_string()]
    } else {
        errors.to_vec()
    }
}

/// 被打断轮次的错误文案（`SDKResultError.errors`）。
///
/// 参考 CLI 在转写里写 "[Request interrupted by user]"；这里用同一句意的短句
/// 作为 errors 文本（真 CLI 的 `errors` 取值没有可观测样本）。
const INTERRUPTED_BY_USER: &str = "Interrupted by user";

/// 「本轮没有任何终态帧」时合成帧的兜底错误文案。
///
/// 只在后端连 `error` 事件都没给时用得上（有 `error` 时用它的原文——真实原因
/// 在那里）；两段都写清，编排器一看就知道该去翻网关日志。
const NO_TERMINAL_FRAME: &str = "turn ended without a terminal frame: the gateway reported \
                                  done but sent no turn_result (check the gateway log for the cause)";

/// 后端中断收口时给未完成工具调用合成的结果文本
/// （`INTERRUPTED_RESULT`，`libs/core/wing/agent/tool_executor.py`）。
///
/// 工具阶段被打断时，后端**先**把每个未完成的调用按这条文本补成 tool_result
/// 并广播、**再**发 `interrupted` 事件（收口顺序，实测：Bash `sleep` 中打断
/// → `user(tool_result "Tool call interrupted by user.", is_error=true)` →
/// `interrupted`）。因此「中断落在工具阶段」只能靠这条合成结果识别；两侧一致
/// 由 `tests::interrupted_marker_matches_backend` 钉住。
const INTERRUPTED_TOOL_RESULT: &str = "Tool call interrupted by user.";

/// 事件不带 `usage` 时的占位（三个 `result` 发出点同形）。
fn zero_usage() -> serde_json::Value {
    serde_json::json!({
        "input_tokens": 0,
        "output_tokens": 0,
        "cached_tokens": 0
    })
}

/// 一条 `result` 终态帧里**随发出点不同**的字段。
///
/// `msg_type` / `total_cost_usd` / 本次运行已耗时 / 零用量占位 / session_id 兜底
/// 由 [`StdioRenderer::result_frame`] 统一派生——它们对三个发出点（json 轮结果、
/// stream-json 轮结果、打断合成帧）逐字相同。
struct ResultFrameArgs {
    subtype: String,
    is_error: bool,
    /// `None` = 不带 `result` 字段（`SDKResultError` 没有该字段）。
    result: Option<String>,
    duration_api_ms: i64,
    num_turns: i64,
    usage: serde_json::Value,
    terminal_reason: Option<String>,
    errors: Vec<String>,
    /// `None` = 用本进程的 session id。
    session_id: Option<String>,
}

impl Default for ResultFrameArgs {
    fn default() -> Self {
        Self {
            subtype: String::new(),
            is_error: false,
            result: None,
            duration_api_ms: 0,
            num_turns: 0,
            usage: zero_usage(),
            terminal_reason: None,
            errors: Vec::new(),
            session_id: None,
        }
    }
}

impl StdioRenderer {
    /// 组装一条 `result` 终态帧（见 [`ResultFrameArgs`]）。
    ///
    /// `uuid` 是帧级去重/关联 id，必须由调用方给（不放进 `ResultFrameArgs` 的
    /// `Default`：那会让漏填静默变成一条 `"uuid": ""` 的帧）。
    fn result_frame(&self, uuid: String, args: ResultFrameArgs) -> ResultMessage {
        ResultMessage {
            msg_type: "result".into(),
            subtype: args.subtype,
            is_error: args.is_error,
            result: args.result,
            duration_ms: self.start_time.elapsed().as_millis() as i64,
            duration_api_ms: args.duration_api_ms,
            num_turns: args.num_turns,
            total_cost_usd: 0.0,
            usage: args.usage,
            terminal_reason: args.terminal_reason,
            errors: args.errors,
            session_id: args.session_id.unwrap_or_else(|| self.session_id.clone()),
            uuid,
        }
    }

    pub fn new(
        format: OutputFormat,
        start_time: Instant,
        session_id: String,
        out: Arc<StdoutSink>,
        include_partial_messages: bool,
    ) -> Self {
        Self {
            format,
            start_time,
            session_id,
            exit_code: std::process::ExitCode::SUCCESS,
            out,
            pending_tools: 0,
            tool_phase_aborted: false,
            terminal_rendered: false,
            last_error: None,
            include_partial_messages,
            stream: StreamState::new(),
        }
    }

    /// Write one frame line, keeping `println!`'s failure semantics: a stdout
    /// that cannot be written (e.g. the orchestrator closed the pipe) is fatal
    /// rather than silently swallowed.
    fn out_line(&self, text: &str) {
        self.out.line(text);
    }

    pub fn exit_code(&self) -> std::process::ExitCode {
        self.exit_code
    }

    /// 新一轮开始：清掉上一轮的相位账本。
    ///
    /// **轮边界由后端事件给出**（`turn_started`，`ReActLoop.run_turn` 每轮必发、
    /// 先于该轮任何内容帧），不由驱动侧的「转发了消息」推断——排队成轮的那一轮
    /// 没有对应的转发动作，而 steer 中途重置又会把当前轮的账本清坏
    /// （`aborted_tools` 判据依赖它）。
    ///
    /// 常驻模式下这是 `turn_started` 那条轮边界的重置入口（轮末还有 `done` /
    /// `interrupted` 两个关轮点，见 [`Self::reset_terminal_ledger`]）；一次性流程里
    /// 它只是一次幂等清空（构造时账本为空）。
    ///
    /// 同时收口在途的流式消息（`--include-partial-messages`）：正常路径上上一轮的
    /// 终态帧已经把它收口（no-op），这里是"帧序异常时也不给消费方留悬空
    /// message_start"的防御（跨轮不泄漏）。
    pub fn begin_turn(&mut self) {
        self.pending_tools = 0;
        self.tool_phase_aborted = false;
        self.reset_terminal_ledger();
        let leaked = self.stream.close_message(None, None);
        self.emit_stream_events(leaked);
    }

    /// 终态账本归零（「这一轮要不要补终态帧」的账）：轮边界与轮末共用一份。
    ///
    /// 只在**关轮**时清：`turn_started`（后端给出的轮边界）、`done`（轮末信号）、
    /// `interrupted`（后端对被打断的轮次既不发 `turn_result` 也不发 `done`，合成帧
    /// 即轮末）。三者都不保证成对出现——兜底路径可能没有 `turn_started`，被打断的
    /// 轮次没有 `done`——所以每个关轮点都要各自清一次。
    ///
    /// `pending_tools` / `tool_phase_aborted` 不在这里：它们是**相位**账（只喂
    /// `terminal_reason` 的 aborted_tools / aborted_streaming 判据），由 `begin_turn`
    /// 与每条 assistant 快照各自维护，与「这一轮有没有终态帧」无关。
    fn reset_terminal_ledger(&mut self) {
        self.terminal_rendered = false;
        self.last_error = None;
    }

    /// Handle an event.
    ///
    /// Returns `true` when **this call** closes the current turn — a
    /// `turn_result` frame, the synthesized terminal frame of an interrupted
    /// turn (see [`Self::handle_interrupted`]), or the failure frame synthesized
    /// for a turn the backend ended without any terminal frame (see
    /// [`Self::handle_done`]). Whether the *process* then exits is the driver's
    /// call: the one-shot flow exits on it; the resident flow
    /// (`crate::stdio::ExitPolicy`) exits only once stdin is closed.
    pub fn handle_event(&mut self, event: &WingEvent) -> bool {
        // 每轮重置：`turn_started` 是后端给出的轮边界（每轮必发一次）。
        if matches!(event, WingEvent::TurnStarted { .. }) {
            self.begin_turn();
            return false;
        }
        // `error` 不是轮末信号（同轮的终态帧 / 轮末 `done` 才是），但它带着
        // 本轮真实的失败原因——记下来，供 `handle_done` 补帧时带出。
        if let WingEvent::Error { message, .. } = event {
            self.last_error = Some(message.clone());
        }
        // 轮末信号：后端异常收口时本轮可能一个终态帧都没有（见 `handle_done`）。
        if matches!(event, WingEvent::Done { .. }) {
            let ended = self.handle_done(event);
            self.reset_terminal_ledger();
            return ended;
        }
        // 被打断的轮次后端**不发** `turn_result`、也**不发** `done`（半截内容作为
        // partial assistant 提交进链）：前端在这里补一条终态帧，否则编排器永远
        // 等不到终态——SDK 的 `streamInput` 在 canUseTool/hooks 存在时以「首个
        // result」门住 `endInput()`，缺了这条帧 stdin 永不关闭、CLI 进程与 WS
        // 订阅一起滞留。合成帧即轮末（后端不会再来一条 `done`）。
        if matches!(event, WingEvent::Interrupted { .. }) {
            let ended = self.handle_interrupted(event);
            self.reset_terminal_ledger();
            return ended;
        }
        let ended = match &self.format {
            OutputFormat::Text => self.handle_text(event),
            OutputFormat::Json => self.handle_json(event),
            OutputFormat::StreamJson => self.handle_stream_json(event),
        };
        // `turn_result`（三格式）的终态帧只在这里记账；它的轮末是紧随其后的 `done`。
        if ended {
            self.terminal_rendered = true;
        }
        ended
    }

    // ============================================================
    // text mode
    // ============================================================

    fn handle_text(&mut self, event: &WingEvent) -> bool {
        match event {
            WingEvent::TurnResult {
                subtype,
                is_error,
                result,
                errors,
                ..
            } => {
                if *is_error {
                    // 错误详情在后端的 `errors` 里（`result` 只在成功轮有值）——
                    // 先前只看 `result`，把真实错误文本丢成了通用文案。
                    eprintln!("Error: {}", error_line_text(result.as_deref(), errors));
                    self.exit_code = std::process::ExitCode::FAILURE;
                } else if let Some(text) = result {
                    self.out_line(text);
                }

                if subtype != "success" {
                    self.exit_code = std::process::ExitCode::FAILURE;
                }

                true
            }
            _ => false,
        }
    }

    // ============================================================
    // json mode
    // ============================================================

    fn handle_json(&mut self, event: &WingEvent) -> bool {
        match event {
            WingEvent::TurnResult {
                uuid,
                subtype,
                is_error,
                result,
                errors,
                num_turns,
                duration_ms,
                usage,
                ..
            } => {
                let msg = self.result_frame(
                    uuid.clone(),
                    ResultFrameArgs {
                        subtype: subtype.clone(),
                        is_error: *is_error,
                        // 一条规则：错误帧不带 `result`（`SDKResultError` 没有该字段）。
                        result: (!*is_error).then(|| result.clone().unwrap_or_default()),
                        duration_api_ms: *duration_ms,
                        num_turns: *num_turns,
                        usage: usage.clone().unwrap_or_else(zero_usage),
                        errors: error_texts(*is_error, errors),
                        ..Default::default()
                    },
                );
                if let Ok(json) = serde_json::to_string(&msg) {
                    self.out_line(&json);
                }

                if *is_error || subtype != "success" {
                    self.exit_code = std::process::ExitCode::FAILURE;
                }

                true
            }
            _ => false,
        }
    }

    // ============================================================
    // stream-json mode
    // ============================================================

    fn handle_stream_json(&mut self, event: &WingEvent) -> bool {
        match event {
            WingEvent::SessionInit {
                uuid,
                tools,
                model,
                permission_mode,
                cwd,
                meta,
                ..
            } => {
                // 增量事件不携带模型名：记住最近已知值，给 `message_start`
                // 当占位（真值在快照帧里）。
                self.stream.set_model(model);
                let msg = SystemInitMessage {
                    msg_type: "system".into(),
                    subtype: "init".into(),
                    tools: tools.clone(),
                    model: model.clone(),
                    permission_mode: permission_mode.clone(),
                    cwd: cwd.clone(),
                    session_id: meta
                        .session_id
                        .clone()
                        .unwrap_or_else(|| self.session_id.clone()),
                    uuid: uuid.clone(),
                };
                self.emit_ndjson(&msg);
                false
            }

            // ── 增量帧（`--include-partial-messages`） ──
            //
            // wing 的三路流式事件翻译成 Anthropic SSE 事件（见 `stream.rs`）。
            // **门控在这里**：不开 flag 时这些事件与改造前一样被忽略（既不产
            // 帧也不喂状态机），输出逐字节不变。
            WingEvent::Text { content, .. } if self.include_partial_messages => {
                let events = self.stream.text(content);
                self.emit_stream_events(events);
                false
            }

            WingEvent::Reasoning { content, .. } if self.include_partial_messages => {
                let events = self.stream.reasoning(content);
                self.emit_stream_events(events);
                false
            }

            WingEvent::ToolCallStream {
                tool_call_id,
                tool_name,
                args_fragment,
                is_final,
                ..
            } if self.include_partial_messages => {
                let events =
                    self.stream
                        .tool_call(tool_call_id, tool_name, args_fragment, *is_final);
                self.emit_stream_events(events);
                false
            }

            WingEvent::AssistantTurn {
                uuid,
                content_blocks,
                model,
                stop_reason,
                usage,
                meta,
                ..
            } => {
                let content: Vec<MessageContent> = content_blocks
                    .iter()
                    .map(|block| {
                        // Pass through the content block as-is (it's already
                        // in Anthropic Messages API format from the backend)
                        MessageContent::deserialize(block)
                            .unwrap_or(MessageContent::Raw { raw: block.clone() })
                    })
                    .collect();

                // 增量帧收口：`--include-partial-messages` 下，这条消息的
                // `message_start…message_stop` 必须在快照帧**之前**闭合
                // （SDK 契约：最终完整消息仍会作为独立消息到来）。无在途
                // 消息时是 no-op（不开 flag / 这条消息没有增量）。
                self.stream.set_model(model);
                let stream_id = self.stream.current_id().map(str::to_string);
                let snapshot_stop_reason = stop_reason
                    .clone()
                    .unwrap_or_else(|| "end_turn".to_string());
                let stream_close = self
                    .stream
                    .close_message(Some(&snapshot_stop_reason), usage.as_ref());
                self.emit_stream_events(stream_close);

                // 相位账本：本条 assistant 消息里的 tool_use 数（等 tool_result
                // 帧逐条平账）。新相位开始 → 清掉上一个相位的打断标记。
                self.tool_phase_aborted = false;
                self.pending_tools += content_blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                    .count() as i64;

                let msg = AssistantMessage {
                    msg_type: "assistant".into(),
                    message: crate::stdio::ndjson::AssistantMessageInner {
                        // id 一致性（硬要求）：有流式在途消息时复用它的
                        // `message.id`（消费方的流式/快照去重键）；否则保持
                        // 既有形状 `msg_{uuid}`（不开 flag 时逐字节不变）。
                        id: stream_id.unwrap_or_else(|| format!("msg_{uuid}")),
                        msg_type: "message".into(),
                        role: "assistant".into(),
                        model: model.clone(),
                        content,
                        stop_reason: snapshot_stop_reason,
                        usage: usage.clone().unwrap_or_else(|| {
                            serde_json::json!({
                                "input_tokens": 0,
                                "output_tokens": 0,
                                "cached_tokens": 0
                            })
                        }),
                    },
                    parent_tool_use_id: None,
                    session_id: meta
                        .session_id
                        .clone()
                        .unwrap_or_else(|| self.session_id.clone()),
                    uuid: uuid.clone(),
                };
                self.emit_ndjson(&msg);
                false
            }

            WingEvent::ToolResultTurn {
                uuid,
                tool_use_id,
                tool_name,
                content,
                is_error,
                meta,
                ..
            } => {
                // `content` deliberately lands in two protocol fields:
                // `message.content[0]` (the tool_result block) and
                // `tool_use_result` — the SDK's per-tool metadata slot, which
                // wing fills with its own flat shape rather than the native
                // CLI's tool-specific payload. Two contract slots, one
                // rendering: no consumer in this repo reads both.
                // 合成结果 = 中断收口的痕迹（结果先于 interrupted 事件到达，
                // 见 `INTERRUPTED_TOOL_RESULT`）。
                if *is_error && content == INTERRUPTED_TOOL_RESULT {
                    self.tool_phase_aborted = true;
                }
                self.pending_tools = (self.pending_tools - 1).max(0);
                let msg = UserMessage {
                    msg_type: "user".into(),
                    message: crate::stdio::ndjson::UserMessageInner {
                        role: "user".into(),
                        content: vec![crate::stdio::ndjson::ToolResultContent {
                            content_type: "tool_result".into(),
                            tool_use_id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: *is_error,
                        }],
                    },
                    parent_tool_use_id: None,
                    tool_use_result: crate::stdio::ndjson::ToolUseResult {
                        tool_use_id: tool_use_id.clone(),
                        tool_name: tool_name.clone(),
                        content: content.clone(),
                        is_error: *is_error,
                    },
                    session_id: meta
                        .session_id
                        .clone()
                        .unwrap_or_else(|| self.session_id.clone()),
                    uuid: uuid.clone(),
                };
                self.emit_ndjson(&msg);
                false
            }

            WingEvent::TurnResult {
                uuid,
                subtype,
                is_error,
                result,
                errors,
                num_turns,
                duration_ms,
                usage,
                ..
            } => {
                // 终态帧之前先收口在途流式消息：错误轮可能落在流式中途
                // （快照帧永远不来），消费方不得拿到悬空的 `message_start`。
                // 正常轮这里已无在途消息（快照帧处已收口）——no-op。
                let stream_close = self.stream.close_message(None, None);
                self.emit_stream_events(stream_close);

                let msg = self.result_frame(
                    uuid.clone(),
                    ResultFrameArgs {
                        subtype: subtype.clone(),
                        is_error: *is_error,
                        // 一条规则：错误帧不带 `result`（`SDKResultError` 没有该字段）。
                        result: (!*is_error).then(|| result.clone().unwrap_or_default()),
                        duration_api_ms: *duration_ms,
                        num_turns: *num_turns,
                        usage: usage.clone().unwrap_or_else(zero_usage),
                        errors: error_texts(*is_error, errors),
                        ..Default::default()
                    },
                );
                self.emit_ndjson(&msg);

                // 退出码 = 最后一轮结果：每个终态帧**覆盖**（不累积）——常驻多轮
                // 下前面失败、最后一轮成功 = SUCCESS；逐轮的权威状态由各自的
                // result 帧承载（subtype / is_error / terminal_reason）。
                self.exit_code = if *is_error || subtype != "success" {
                    std::process::ExitCode::FAILURE
                } else {
                    std::process::ExitCode::SUCCESS
                };

                true
            }

            // All other events are ignored in stream-json mode
            _ => false,
        }
    }

    fn emit_ndjson<T: serde::Serialize>(&mut self, msg: &T) {
        if let Ok(json) = serde_json::to_string(msg) {
            self.out_line(&json);
        }
    }

    /// 发一批 `RawMessageStreamEvent`：一条事件一帧（`stream_event` 信封）。
    ///
    /// `uuid` 是帧级去重/关联 id（事件不带链 uuid，现铸一个稳定可追溯的值——
    /// 与打断合成终态帧同一口径）。状态机未被喂过东西时列表为空（不开 flag /
    /// 无在途消息），本方法即 no-op。
    fn emit_stream_events(&mut self, events: Vec<serde_json::Value>) {
        for event in events {
            let frame = StreamEventFrame {
                msg_type: "stream_event".into(),
                event,
                parent_tool_use_id: None,
                session_id: self.session_id.clone(),
                uuid: crate::protocol::generate_request_id(),
            };
            self.emit_ndjson(&frame);
        }
    }

    // ============================================================
    // interrupted terminal frame
    // ============================================================

    /// 被打断轮次的终态帧。
    ///
    /// 形状对齐 SDK 的类型（`SDKResultError` + `TerminalReason`，`sdk.d.ts`）：
    /// `subtype="error_during_execution"`、`is_error=true`、
    /// `terminal_reason="aborted_streaming" | "aborted_tools"`、`errors=[…]`，
    /// 不带 `result`（`SDKResultError` 没有这个字段）。
    /// 退出码 **SUCCESS**：中止是编排器主动要的动作，而 SDK 把非零退出码当进程
    /// 错误（`getProcessExitError` → 消费方 run loop 会再报一次失败）。
    fn handle_interrupted(&mut self, event: &WingEvent) -> bool {
        let WingEvent::Interrupted { meta, .. } = event else {
            unreachable!("handle_interrupted 只处理 Interrupted 事件")
        };
        // 打断落在流式中途时（快照帧不会来），先对已打开块做收口——消费方不得
        // 拿到悬空的 `message_start`；收口不改变终态帧纪律：合成 result 仍是最后
        // 一条帧。
        let stream_close = self.stream.close_message(None, None);
        self.emit_stream_events(stream_close);

        let terminal_reason = if self.pending_tools > 0 || self.tool_phase_aborted {
            "aborted_tools"
        } else {
            "aborted_streaming"
        };
        // text 模式没有"最终文本"可打；中止不是错误，stdout 保持干净（与
        // 错误轮的 text 行为一致：错误文本走 stderr）。
        if !matches!(self.format, OutputFormat::Text) {
            self.emit_synthesized_error(
                &[INTERRUPTED_BY_USER.to_string()],
                Some(terminal_reason.to_string()),
                meta.session_id.clone(),
            );
        }
        self.exit_code = std::process::ExitCode::SUCCESS;
        true
    }

    /// 前端补的终态帧（`SDKResultError` 形状）：打断合成帧与「本轮没有终态帧」的
    /// 补帧共用这一处——`subtype` / `is_error` / `errors` 非空纪律 / 帧 id 只在这里
    /// 定义一次。
    ///
    /// 事件不带链 uuid（两者都不是链上事实）：帧 id 现铸一个稳定可追溯的值，供
    /// 消费方去重 / 关联。
    fn emit_synthesized_error(
        &mut self,
        errors: &[String],
        terminal_reason: Option<String>,
        session_id: Option<String>,
    ) {
        let msg = self.result_frame(
            crate::protocol::generate_request_id(),
            ResultFrameArgs {
                subtype: "error_during_execution".into(),
                is_error: true,
                terminal_reason,
                errors: error_texts(true, errors),
                session_id,
                ..Default::default()
            },
        );
        self.emit_ndjson(&msg);
    }

    // ============================================================
    // done — 后端轮末信号
    // ============================================================

    /// 轮末信号（`done`）：本轮**没有**终态帧时补一条失败终态帧。
    ///
    /// `done` 是后端唯一覆盖全部收口路径的轮末信号——`run_turn` 的成功 /
    /// max_turns / 异常三路都在 `turn_result` 之后发它，而 `agent/core.py::_run`
    /// 的兜底 except 只发 `error` + `done`、**不发 `turn_result`**。消费方等的是
    /// 每轮一条 result，所以缺帧的轮要补上（只让进程退出而不补帧，等于把同一个
    /// 缺口挪到消费方那边）。
    ///
    /// 两条出口：
    /// - 本轮已有终态帧 → **返回 `false`**：这条 `done` 只是把这一轮关掉，
    ///   **不是一次新的收口**。常驻模式下 `done` 可能晚于下一条消息的投递与 stdin
    ///   EOF 到达，报 `true` 会让驱动侧把下一轮的在途标记一起清掉（提前退出、
    ///   丢掉那一轮的输出）。
    /// - 本轮没有任何终态帧 → 补一条 `SDKResultError` 形状的失败终态帧
    ///   （`is_error=true`、`subtype="error_during_execution"`、`errors=[…]`、不带
    ///   `result`；形状同 [`Self::handle_interrupted`]），退出码 FAILURE，返回
    ///   `true`。`errors` 取本轮后端 `Error` 原文（真实原因在那里），没有则用
    ///   [`NO_TERMINAL_FRAME`]。text 模式的错误文本走 stderr。
    fn handle_done(&mut self, event: &WingEvent) -> bool {
        let WingEvent::Done { meta, .. } = event else {
            unreachable!("handle_done 只处理 Done 事件")
        };
        if self.terminal_rendered {
            return false;
        }
        let errors = vec![
            self.last_error
                .clone()
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| NO_TERMINAL_FRAME.to_string()),
        ];
        // 错误轮可能落在流式中途（快照帧永远不来）：终态帧之前先收口，消费方
        // 不得拿到悬空的 `message_start`。
        let stream_close = self.stream.close_message(None, None);
        self.emit_stream_events(stream_close);

        if matches!(self.format, OutputFormat::Text) {
            eprintln!("Error: {}", error_line_text(None, &errors));
        } else {
            self.emit_synthesized_error(&errors, None, meta.session_id.clone());
        }
        self.exit_code = std::process::ExitCode::FAILURE;
        true
    }
}

#[cfg(test)]
mod tests {
    //! The `wing -p` stdout contract: what each output format writes and when
    //! the process fails. These are the assertions the retired e2e rig used to
    //! make through a real process — they live here now, at the format
    //! boundary, so a change to the frames fails offline.

    use super::*;
    use crate::stdio::stdout::CaptureSink;
    use crate::stdio::test_support::{
        backend_error, done, event as decode, interrupted, renderer as setup,
        streaming_renderer as setup_streaming, turn_result, turn_started,
    };
    use serde_json::json;

    #[test]
    fn text_mode_prints_the_final_text_and_exits_zero() {
        let (mut renderer, buf) = setup(OutputFormat::Text);

        assert!(renderer.handle_event(&turn_result("hello", false, "success")));
        assert_eq!(buf.text(), "hello\n");
        assert_eq!(renderer.exit_code(), std::process::ExitCode::SUCCESS);
    }

    #[test]
    fn text_mode_reports_failure_without_writing_stdout() {
        let (mut renderer, buf) = setup(OutputFormat::Text);

        // Error text goes to stderr; stdout stays empty so an orchestrator
        // piping only stdout never mistakes the message for a result.
        assert!(renderer.handle_event(&turn_result("boom", true, "error_during_execution")));
        assert_eq!(buf.text(), "");
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
    }

    #[test]
    fn text_mode_fails_on_a_non_success_subtype() {
        let (mut renderer, _buf) = setup(OutputFormat::Text);

        assert!(renderer.handle_event(&turn_result("partial", false, "error_max_turns")));
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
    }

    #[test]
    fn json_mode_emits_exactly_one_result_object() {
        let (mut renderer, buf) = setup(OutputFormat::Json);

        assert!(renderer.handle_event(&turn_result("hi", false, "success")));

        let out = buf.text();
        assert_eq!(out.lines().count(), 1, "json mode writes one frame: {out}");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["type"], "result");
        assert_eq!(parsed["subtype"], "success");
        assert_eq!(parsed["is_error"], false);
        assert_eq!(parsed["result"], "hi");
        assert_eq!(parsed["num_turns"], 2);
        assert_eq!(parsed["session_id"], "sess-1");
        assert_eq!(renderer.exit_code(), std::process::ExitCode::SUCCESS);
    }

    #[test]
    fn json_mode_fails_the_exit_code_on_error() {
        let (mut renderer, buf) = setup(OutputFormat::Json);

        assert!(renderer.handle_event(&turn_result("boom", true, "error_during_execution")));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&buf.text()).unwrap()["is_error"],
            true
        );
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
    }

    #[test]
    fn stream_json_emits_system_assistant_and_result_frames() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        let init = decode(json!({
            "type": "session_init",
            "uuid": "u-0",
            "tools": ["Read", "Bash"],
            "model": "test-model",
            "permission_mode": "bypassPermissions",
            "cwd": "/tmp",
            "session_id": "sess-1",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&init), "init does not end the run");

        let assistant = decode(json!({
            "type": "assistant_turn",
            "uuid": "u-1",
            "content_blocks": [{"type": "text", "text": "hi"}],
            "model": "test-model",
            "stop_reason": "end_turn",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&assistant));

        assert!(renderer.handle_event(&turn_result("hi", false, "success")));

        let out = buf.text();
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
            .collect();
        assert_eq!(lines.len(), 3, "one NDJSON frame per event: {out}");
        assert_eq!(lines[0]["type"], "system");
        assert_eq!(lines[0]["subtype"], "init");
        assert_eq!(lines[0]["tools"], json!(["Read", "Bash"]));
        assert_eq!(lines[0]["model"], "test-model");
        assert_eq!(lines[1]["type"], "assistant");
        assert_eq!(lines[1]["message"]["content"][0]["text"], "hi");
        assert_eq!(lines[2]["type"], "result");
        assert_eq!(renderer.exit_code(), std::process::ExitCode::SUCCESS);
    }

    #[test]
    fn stream_json_ignores_events_outside_the_protocol() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        let text_delta = decode(json!({
            "type": "text",
            "content": "partial",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&text_delta));
        assert_eq!(buf.text(), "", "streaming deltas are not protocol frames");
    }

    // ---- interrupted: 被打断轮次的终态帧 ----

    fn assistant_with_tool_call() -> WingEvent {
        decode(json!({
            "type": "assistant_turn",
            "uuid": "u-2",
            "content_blocks": [{"type": "tool_use", "id": "call_1", "name": "Bash", "input": {"command": "sleep 5"}}],
            "model": "test-model",
            "stop_reason": "tool_use",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

    fn tool_result() -> WingEvent {
        decode(json!({
            "type": "tool_result_turn",
            "uuid": "u-3",
            "tool_use_id": "call_1",
            "tool_name": "Bash",
            "content": "done",
            "is_error": false,
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

    /// 被打断的轮次必须有**终态帧**，否则 SDK 的 endInput 门
    /// （canUseTool/hooks 存在时以首个 result 为条件）永不满足，CLI 滞留。
    /// 形状按 `SDKResultError` + `TerminalReason`（sdk.d.ts）钉死。
    #[test]
    fn stream_json_emits_an_aborted_result_for_an_interrupted_turn() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        let assistant = decode(json!({
            "type": "assistant_turn",
            "uuid": "u-1",
            "content_blocks": [{"type": "text", "text": "cut here"}],
            "model": "test-model",
            "stop_reason": "end_turn",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&assistant));

        assert!(
            renderer.handle_event(&interrupted()),
            "被打断 = 终态：事件循环必须据此退出"
        );

        let out = buf.text();
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
            .collect();
        assert_eq!(lines.len(), 2, "assistant + 一条终态 result：{out}");
        let result = &lines[1];
        assert_eq!(result["type"], "result");
        assert_eq!(result["subtype"], "error_during_execution");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["terminal_reason"], "aborted_streaming");
        assert_eq!(result["errors"], json!(["Interrupted by user"]));
        assert!(
            result.get("result").is_none(),
            "SDKResultError 没有 result: {result}"
        );
        assert_eq!(result["session_id"], "sess-1");
        // 中止是编排器的主动动作：退出码 0（非零会被 SDK 当进程错误）。
        assert_eq!(renderer.exit_code(), std::process::ExitCode::SUCCESS);
    }

    /// 工具阶段被打断（assistant 的 tool_use 还没等到 tool_result）→
    /// `terminal_reason = aborted_tools`。
    #[test]
    fn stream_json_marks_aborted_tools_when_tools_were_in_flight() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        assert!(!renderer.handle_event(&assistant_with_tool_call()));
        assert!(renderer.handle_event(&interrupted()));

        let parsed: serde_json::Value =
            serde_json::from_str(buf.text().lines().last().unwrap()).unwrap();
        assert_eq!(parsed["terminal_reason"], "aborted_tools");
    }

    /// 工具已经结算完（tool_result 到齐）后被打断 → 回到流式相位。
    #[test]
    fn stream_json_reports_aborted_streaming_after_tools_settled() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        assert!(!renderer.handle_event(&assistant_with_tool_call()));
        assert!(!renderer.handle_event(&tool_result()));
        assert!(renderer.handle_event(&interrupted()));

        let parsed: serde_json::Value =
            serde_json::from_str(buf.text().lines().last().unwrap()).unwrap();
        assert_eq!(parsed["terminal_reason"], "aborted_streaming");
    }

    /// 真实收口顺序（实测）：中断落在工具阶段时，后端先把未完成的调用补成
    /// 「合成失败结果」广播，再发 `interrupted` —— 此时 `pending_tools` 已归零，
    /// 相位只能靠合成结果识别。
    #[test]
    fn stream_json_marks_aborted_tools_when_results_were_synthesized() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        assert!(!renderer.handle_event(&assistant_with_tool_call()));
        let synthesized = decode(json!({
            "type": "tool_result_turn",
            "uuid": "u-4",
            "tool_use_id": "call_1",
            "tool_name": "Bash",
            "content": INTERRUPTED_TOOL_RESULT,
            "is_error": true,
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&synthesized));
        assert!(renderer.handle_event(&interrupted()));

        let parsed: serde_json::Value =
            serde_json::from_str(buf.text().lines().last().unwrap()).unwrap();
        assert_eq!(parsed["terminal_reason"], "aborted_tools");
    }

    /// 合成结果之后又开了新的一轮（assistant）→ 相位标记复位：那之后的打断
    /// 属于流式阶段。
    #[test]
    fn stream_json_resets_the_tool_phase_after_a_new_assistant_turn() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        assert!(!renderer.handle_event(&assistant_with_tool_call()));
        let synthesized = decode(json!({
            "type": "tool_result_turn",
            "uuid": "u-4",
            "tool_use_id": "call_1",
            "tool_name": "Bash",
            "content": INTERRUPTED_TOOL_RESULT,
            "is_error": true,
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&synthesized));
        let next_round = decode(json!({
            "type": "assistant_turn",
            "uuid": "u-5",
            "content_blocks": [{"type": "text", "text": "next"}],
            "model": "test-model",
            "stop_reason": "end_turn",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&next_round));
        assert!(renderer.handle_event(&interrupted()));

        let parsed: serde_json::Value =
            serde_json::from_str(buf.text().lines().last().unwrap()).unwrap();
        assert_eq!(parsed["terminal_reason"], "aborted_streaming");
    }

    // ---- 常驻多轮：每轮重置与退出码口径 ----

    /// 中断收口的合成工具结果（后端 `INTERRUPTED_RESULT` 文案）。
    fn synthesized_tool_result() -> WingEvent {
        decode(json!({
            "type": "tool_result_turn",
            "uuid": "u-4",
            "tool_use_id": "call_1",
            "tool_name": "Bash",
            "content": INTERRUPTED_TOOL_RESULT,
            "is_error": true,
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

    /// 每轮重置：上一轮留在账本里的相位痕迹（工具在飞 + 合成结果）不得影响
    /// 新轮的 interrupted 判据——否则「上一轮工具被打断」会把下一轮的流式
    /// 打断误报成 `aborted_tools`。
    #[test]
    fn turn_started_resets_the_phase_ledger_of_the_previous_turn() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        // 上一轮：工具在飞（pending_tools=1）+ 合成结果（tool_phase_aborted=true）。
        assert!(!renderer.handle_event(&assistant_with_tool_call()));
        assert!(!renderer.handle_event(&synthesized_tool_result()));
        assert!(renderer.handle_event(&interrupted()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(buf.text().lines().last().unwrap()).unwrap()
                ["terminal_reason"],
            "aborted_tools"
        );

        // 新的一轮：只走了流式阶段就被打断 → 必须是 aborted_streaming。
        assert!(
            !renderer.handle_event(&turn_started()),
            "turn_started 不结束任何一轮"
        );
        assert!(renderer.handle_event(&interrupted()));
        let parsed: serde_json::Value =
            serde_json::from_str(buf.text().lines().last().unwrap()).unwrap();
        assert_eq!(
            parsed["terminal_reason"], "aborted_streaming",
            "新轮的相位必须从零开始：{parsed}"
        );
    }

    /// `begin_turn()` 是显式的重置入口（与 `turn_started` 事件同一条路径）。
    #[test]
    fn begin_turn_is_the_explicit_reset_entry() {
        let (mut renderer, _buf) = setup(OutputFormat::StreamJson);

        // 工具在飞（未结算）→ 账本不为零。
        assert!(!renderer.handle_event(&assistant_with_tool_call()));
        assert_eq!(renderer.pending_tools, 1);
        renderer.begin_turn();
        assert_eq!(renderer.pending_tools, 0);

        // 合成结果留下的相位痕迹同样被清掉。
        assert!(!renderer.handle_event(&synthesized_tool_result()));
        assert!(renderer.tool_phase_aborted);
        renderer.begin_turn();
        assert!(!renderer.tool_phase_aborted);
    }

    /// 退出码 = 最后一轮结果：终态帧**覆盖**（不累积）。
    #[test]
    fn exit_code_follows_the_last_turn() {
        let (mut renderer, _buf) = setup(OutputFormat::StreamJson);

        // 第一轮失败 → FAILURE。
        assert!(renderer.handle_event(&turn_error(vec!["boom"], "error_during_execution")));
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);

        // 第二轮成功 → 覆盖成 SUCCESS（逐轮状态由各自的 result 帧承载）。
        assert!(!renderer.handle_event(&turn_started()));
        assert!(renderer.handle_event(&turn_result("done", false, "success")));
        assert_eq!(
            renderer.exit_code(),
            std::process::ExitCode::SUCCESS,
            "最后一轮成功即 SUCCESS"
        );

        // 再来一轮失败 → 又回到 FAILURE。
        assert!(!renderer.handle_event(&turn_started()));
        assert!(renderer.handle_event(&turn_error(vec!["again"], "error_during_execution")));
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
    }

    /// Drift guard：相位判据依赖的合成文本必须与后端常量逐字一致
    /// （`libs/core/wing/agent/tool_executor.py` 的 `INTERRUPTED_RESULT`）。
    /// 后端改了文案而这里没跟 → 工具阶段的打断会被误报成 aborted_streaming。
    #[test]
    fn interrupted_marker_matches_backend() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../libs/core/wing/agent/tool_executor.py"
        );
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let expected = format!("INTERRUPTED_RESULT = \"{INTERRUPTED_TOOL_RESULT}\"");
        assert!(
            src.contains(&expected),
            "backend constant drifted: expected `{expected}` in {path}"
        );
    }

    /// json 模式同样以终态帧收尾（契约：一个 result 对象）。
    #[test]
    fn json_mode_reports_an_aborted_turn_as_a_result_object() {
        let (mut renderer, buf) = setup(OutputFormat::Json);

        assert!(renderer.handle_event(&interrupted()));

        let out = buf.text();
        assert_eq!(out.lines().count(), 1, "json 模式只出一条：{out}");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["type"], "result");
        assert_eq!(parsed["subtype"], "error_during_execution");
        assert_eq!(parsed["terminal_reason"], "aborted_streaming");
        assert_eq!(renderer.exit_code(), std::process::ExitCode::SUCCESS);
    }

    // ---- done：后端轮末信号的兜底收口（issue #246） ----

    /// 本轮没有任何终态帧就收到轮末信号：`done` 必须收口，并补一条失败终态帧
    /// ——否则驱动侧永远等下一条事件（`recv_event` 不会返回 None），`wing -p`
    /// 永久挂起。
    #[test]
    fn done_without_a_terminal_frame_synthesizes_a_failure_result() {
        for format in [OutputFormat::Json, OutputFormat::StreamJson] {
            let (mut renderer, buf) = setup(format.clone());

            assert!(
                !renderer.handle_event(&backend_error("处理消息失败：异常：boom")),
                "{format:?}: error 不是轮末信号"
            );
            assert!(
                renderer.handle_event(&done()),
                "{format:?}: done 必须结束本轮"
            );

            let out = buf.text();
            assert_eq!(out.lines().count(), 1, "{format:?}: 恰好一条终态帧: {out}");
            let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(parsed["type"], "result", "{format:?}");
            assert_eq!(parsed["subtype"], "error_during_execution", "{format:?}");
            assert_eq!(parsed["is_error"], true, "{format:?}");
            assert_eq!(
                parsed["errors"],
                json!(["处理消息失败：异常：boom"]),
                "{format:?}: 后端错误原文必须进 errors: {parsed}"
            );
            assert!(
                parsed.get("result").is_none(),
                "{format:?}: SDKResultError 没有 result: {parsed}"
            );
            assert_eq!(parsed["session_id"], "sess-1", "{format:?}");
            // 后端确实没能完成这一轮：编排器必须看得见。
            assert_eq!(
                renderer.exit_code(),
                std::process::ExitCode::FAILURE,
                "{format:?}"
            );
        }
    }

    /// 连 `error` 都没有（`done` 是唯一的收口信号）：仍要补帧，用通用文案兜底。
    #[test]
    fn done_without_a_backend_error_carries_the_generic_text() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        assert!(renderer.handle_event(&done()));

        let parsed: serde_json::Value = serde_json::from_str(&buf.text()).unwrap();
        assert_eq!(parsed["is_error"], true);
        // 文案要点：说清这一轮没有终态帧（细节全文见 e2e 的 stderr 断言）。
        let errors = parsed["errors"].as_array().expect("errors array");
        assert_eq!(errors.len(), 1, "errors 非空是 SDK 的硬要求: {parsed}");
        assert!(
            errors[0]
                .as_str()
                .is_some_and(|text| text.contains("without a terminal frame")),
            "兜底文案要说清这一轮没有终态帧: {parsed}"
        );
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
    }

    /// text 模式没有结果帧可打：错误文本走 stderr，stdout 保持干净。
    #[test]
    fn text_mode_done_without_a_terminal_frame_keeps_stdout_clean() {
        let (mut renderer, buf) = setup(OutputFormat::Text);

        assert!(!renderer.handle_event(&backend_error("boom")));
        assert!(renderer.handle_event(&done()));
        assert_eq!(buf.text(), "", "stdout 是协议通道：错误文本只进 stderr");
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
    }

    /// 已经出过终态帧再收到 `done` → 幂等 no-op：不产第二条 result 帧、不动退出码。
    #[test]
    fn done_after_a_terminal_frame_is_a_no_op() {
        for format in [
            OutputFormat::Text,
            OutputFormat::Json,
            OutputFormat::StreamJson,
        ] {
            let (mut renderer, buf) = setup(format.clone());
            assert!(renderer.handle_event(&turn_result("hi", false, "success")));
            let before = buf.text();

            assert!(
                !renderer.handle_event(&done()),
                "{format:?}: 这一轮早就收口了——它不得再报一次「本轮结束」"
            );
            assert_eq!(buf.text(), before, "{format:?}: 不得产出第二条帧");
            assert_eq!(
                renderer.exit_code(),
                std::process::ExitCode::SUCCESS,
                "{format:?}"
            );
        }
    }

    // 账本（「本轮要不要补终态帧」）有三个关轮点，每个都得清一次——后端不保证它们
    // 成对出现：兜底路径可能没有 `turn_started`，被打断的轮次没有 `done`。每条用例
    // 都先把账本弄脏（`turn_result`），否则「清没清」观察不到。

    /// 关轮点 ①：`turn_started`（后端给出的轮边界）。
    #[test]
    fn turn_started_clears_the_ledger() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        // 上一轮的终态帧到了、它的 `done` 丢了（帧丢失），紧接着新的一轮开始。
        assert!(renderer.handle_event(&turn_result("hi", false, "success")));
        assert!(!renderer.handle_event(&turn_started()), "轮边界不结束轮次");

        // 新的一轮没有终态帧 → 它的 `done` 必须补帧（账本已在轮边界清零）。
        assert!(renderer.handle_event(&done()));
        let frames = output_frames(&buf);
        assert_eq!(frames.len(), 2, "{}", buf.text());
        assert_eq!(frames[1]["subtype"], "error_during_execution");
    }

    /// 关轮点 ②：`done`（轮末信号）。下一轮即便**没有** `turn_started`（后端在轮入口
    /// 之前就崩了：`_run` 的兜底 except 只发 `error` + `done`），也要重新补帧。
    #[test]
    fn done_clears_the_ledger() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        // 第一轮：正常收口 + 轮末 done（这一轮关掉）。
        assert!(!renderer.handle_event(&backend_error("first")));
        assert!(renderer.handle_event(&turn_result("hi", false, "success")));
        assert!(!renderer.handle_event(&done()));

        // 第二轮：没有 turn_started、没有 turn_result，只有 done → 必须补帧。
        assert!(!renderer.handle_event(&backend_error("second")));
        assert!(renderer.handle_event(&done()));

        let frames = output_frames(&buf);
        assert_eq!(frames.len(), 2, "{}", buf.text());
        assert_eq!(frames[0]["subtype"], "success");
        assert_eq!(frames[1]["subtype"], "error_during_execution");
        assert_eq!(
            frames[1]["errors"],
            json!(["second"]),
            "后端错误原文进 errors（不跨轮泄漏）"
        );
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
    }

    /// 关轮点 ③：`interrupted`（后端对被打断的轮次既不发 `turn_result` 也不发
    /// `done`，合成帧即轮末）。
    #[test]
    fn interrupted_clears_the_ledger() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        // 账本是脏的（本轮已出过终态帧）时被打断：合成帧仍是轮末，账本随之清零。
        assert!(renderer.handle_event(&turn_result("hi", false, "success")));
        assert!(renderer.handle_event(&interrupted()), "合成帧即轮末");
        assert_eq!(renderer.exit_code(), std::process::ExitCode::SUCCESS);

        // 下一轮没有 turn_started、只有 done → 必须补帧。
        assert!(renderer.handle_event(&done()), "新一轮的 done 必须收口");
        let frames = output_frames(&buf);
        assert_eq!(frames.len(), 3, "{}", buf.text());
        assert_eq!(frames[1]["terminal_reason"], "aborted_streaming");
        assert_eq!(frames[2]["subtype"], "error_during_execution");
        assert!(
            frames[2]["errors"][0]
                .as_str()
                .is_some_and(|text| text.contains("without a terminal frame")),
            "没有 error 时用兜底文案: {}",
            frames[2]
        );
        assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
    }

    // ---- 流式帧（`--include-partial-messages`） ----

    fn text_delta(content: &str) -> WingEvent {
        decode(json!({
            "type": "text",
            "content": content,
            "session_id": "sess-1",
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

    fn reasoning_delta(content: &str) -> WingEvent {
        decode(json!({
            "type": "reasoning",
            "content": content,
            "session_id": "sess-1",
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

    fn tool_call_delta(id: &str, name: &str, fragment: &str, is_final: bool) -> WingEvent {
        decode(json!({
            "type": "tool_call_stream",
            "tool_call_id": id,
            "tool_name": name,
            "args_fragment": fragment,
            "is_final": is_final,
            "session_id": "sess-1",
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

    /// stdout 里已写出的帧（按顺序）。
    fn output_frames(buf: &CaptureSink) -> Vec<serde_json::Value> {
        buf.text()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
            .collect()
    }

    /// 其中的 `stream_event` 帧（按顺序）。
    fn stream_frames(buf: &CaptureSink) -> Vec<serde_json::Value> {
        output_frames(buf)
            .into_iter()
            .filter(|f| f["type"] == "stream_event")
            .collect()
    }

    fn stream_event_types(buf: &CaptureSink) -> Vec<String> {
        stream_frames(buf)
            .iter()
            .map(|f| f["event"]["type"].as_str().unwrap_or("?").to_string())
            .collect()
    }

    /// 门控：不开 flag 时三路 delta 一个字节都不产（改造前的行为）。
    #[test]
    fn streaming_events_are_gated_by_the_flag() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        assert!(!renderer.handle_event(&reasoning_delta("think")));
        assert!(!renderer.handle_event(&text_delta("hi")));
        assert!(!renderer.handle_event(&tool_call_delta("call_1", "Bash", "{}", true)));
        assert_eq!(buf.text(), "", "无 flag：delta 不产任何帧");
    }

    /// id 一致性（硬要求）：`message_start` 的 `message.id` 与同一消息快照帧的
    /// `message.id` 逐字一致；快照收口（message_stop）先于快照帧。
    #[test]
    fn streaming_frames_pair_with_the_snapshot_message_id() {
        let (mut renderer, buf) = setup_streaming();

        assert!(!renderer.handle_event(&reasoning_delta("think")));
        assert!(!renderer.handle_event(&text_delta("Hel")));
        assert!(!renderer.handle_event(&text_delta("lo")));

        let snapshot = decode(json!({
            "type": "assistant_turn",
            "uuid": "u-snapshot",
            "content_blocks": [
                {"type": "thinking", "thinking": "think"},
                {"type": "text", "text": "Hello"},
            ],
            "model": "test-model",
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 12, "output_tokens": 5, "cached_tokens": 0},
            "session_id": "sess-1",
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&snapshot));

        assert_eq!(
            stream_event_types(&buf),
            vec![
                "message_start",
                "content_block_start", // thinking（index 0）
                "content_block_delta",
                "content_block_stop",
                "content_block_start", // text（index 1）
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{}",
            buf.text()
        );

        let frames = stream_frames(&buf);
        let start_id = frames[0]["event"]["message"]["id"].as_str().unwrap();
        assert!(start_id.starts_with("msg_"), "{start_id}");

        // 流式收口在快照帧之前；快照帧复用同一个 message.id。
        let out = output_frames(&buf);
        let stop_at = out
            .iter()
            .position(|f| f["type"] == "stream_event" && f["event"]["type"] == "message_stop")
            .unwrap();
        let snapshot_at = out.iter().position(|f| f["type"] == "assistant").unwrap();
        assert!(
            stop_at < snapshot_at,
            "message_stop 必须先于快照帧: {out:?}"
        );
        assert_eq!(
            out[snapshot_at]["message"]["id"].as_str(),
            Some(start_id),
            "快照帧 message.id == message_start.message.id"
        );
        assert_eq!(
            out[snapshot_at]["uuid"], "u-snapshot",
            "帧 uuid 仍是链事件 uuid（与 message.id 是两回事）"
        );

        // message_delta 与快照同值：stop_reason + output_tokens。
        let delta = &frames[8]["event"];
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
        assert_eq!(delta["delta"]["stop_sequence"], serde_json::Value::Null);
        assert_eq!(delta["usage"], json!({"output_tokens": 5}));

        // 信封字段：parent_tool_use_id 恒 null，session_id 是会话 id，uuid 逐帧
        // 唯一（`generate_request_id` 的翅膀号是进程内单调计数器——不断言绝对值）。
        assert_eq!(frames[0]["parent_tool_use_id"], serde_json::Value::Null);
        assert_eq!(frames[0]["session_id"], "sess-1");
        assert!(frames[0]["uuid"].as_str().unwrap().starts_with("wing_"));
        assert_ne!(frames[0]["uuid"], frames[1]["uuid"]);
    }

    /// tool_use：`content_block_start` 带 id/name，参数以 `input_json_delta`
    /// 增量透传（provider 已切好片段），`is_final` 收口块。
    #[test]
    fn streaming_input_json_delta_is_forwarded_verbatim() {
        let (mut renderer, buf) = setup_streaming();

        assert!(!renderer.handle_event(&tool_call_delta("call_1", "Bash", "{\"cmd", false)));
        assert!(!renderer.handle_event(&tool_call_delta("call_1", "Bash", "\": \"ls\"}", false)));
        assert!(!renderer.handle_event(&tool_call_delta("call_1", "Bash", "", true)));

        let frames = stream_frames(&buf);
        assert_eq!(frames[1]["event"]["content_block"]["type"], "tool_use");
        assert_eq!(frames[1]["event"]["content_block"]["id"], "call_1");
        assert_eq!(frames[1]["event"]["content_block"]["name"], "Bash");
        assert_eq!(
            frames[2]["event"]["delta"],
            json!({"type": "input_json_delta", "partial_json": "{\"cmd"})
        );
        assert_eq!(
            frames[3]["event"]["delta"],
            json!({"type": "input_json_delta", "partial_json": "\": \"ls\"}"})
        );
        assert_eq!(
            stream_event_types(&buf),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
            ],
            "{}",
            buf.text()
        );

        // 收口后的快照帧复用同一个 id。
        let snapshot = decode(json!({
            "type": "assistant_turn",
            "uuid": "u-tool",
            "content_blocks": [
                {"type": "tool_use", "id": "call_1", "name": "Bash", "input": {"cmd": "ls"}},
            ],
            "model": "test-model",
            "stop_reason": "tool_use",
            "session_id": "sess-1",
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&snapshot));

        let frames = stream_frames(&buf);
        let start_id = frames[0]["event"]["message"]["id"].as_str().unwrap();
        let out = output_frames(&buf);
        let snapshot_at = out.iter().position(|f| f["type"] == "assistant").unwrap();
        assert_eq!(out[snapshot_at]["message"]["id"].as_str(), Some(start_id));
    }

    /// 无增量的消息：不伪造流式帧，快照帧保持既有 id（`msg_{uuid}`）。
    #[test]
    fn a_message_without_deltas_keeps_the_legacy_message_id() {
        let (mut renderer, buf) = setup_streaming();

        let snapshot = decode(json!({
            "type": "assistant_turn",
            "uuid": "u-plain",
            "content_blocks": [{"type": "text", "text": "hi"}],
            "model": "test-model",
            "stop_reason": "end_turn",
            "session_id": "sess-1",
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&snapshot));

        let out = output_frames(&buf);
        assert_eq!(out.len(), 1, "没有任何流式帧: {out:?}");
        assert_eq!(out[0]["message"]["id"], "msg_u-plain");
    }

    /// 一轮多条消息：各自独立 `message_start…message_stop`，id 不同。
    #[test]
    fn two_messages_in_one_turn_get_their_own_stream_lifecycle() {
        let (mut renderer, buf) = setup_streaming();

        assert!(!renderer.handle_event(&text_delta("first")));
        assert!(!renderer.handle_event(&snapshot_with_text("u-1", "first")));
        assert!(!renderer.handle_event(&text_delta("second")));
        assert!(!renderer.handle_event(&snapshot_with_text("u-2", "second")));

        let frames = stream_frames(&buf);
        let starts: Vec<&str> = frames
            .iter()
            .filter(|f| f["event"]["type"] == "message_start")
            .map(|f| f["event"]["message"]["id"].as_str().unwrap())
            .collect();
        assert_eq!(starts.len(), 2, "{}", buf.text());
        assert_ne!(starts[0], starts[1], "两条消息两个 id");

        let stops = stream_event_types(&buf)
            .iter()
            .filter(|t| *t == "message_stop")
            .count();
        assert_eq!(stops, 2);

        let out = output_frames(&buf);
        let snapshots: Vec<&serde_json::Value> =
            out.iter().filter(|f| f["type"] == "assistant").collect();
        assert_eq!(
            snapshots[0]["message"]["id"].as_str(),
            Some(starts[0]),
            "第一条快照复用第一个 id"
        );
        assert_eq!(
            snapshots[1]["message"]["id"].as_str(),
            Some(starts[1]),
            "第二条快照复用第二个 id"
        );
    }

    /// 快照永远不来的错误轮（`turn_result`）：终态帧之前先收口，不留悬空
    /// `message_start`。
    #[test]
    fn turn_result_closes_an_open_stream_message_first() {
        let (mut renderer, buf) = setup_streaming();

        assert!(!renderer.handle_event(&text_delta("cut")));
        assert!(renderer.handle_event(&turn_error(vec!["boom"], "error_during_execution")));

        assert_eq!(
            stream_event_types(&buf),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{}",
            buf.text()
        );
        let frames = stream_frames(&buf);
        assert_eq!(
            frames[4]["event"]["delta"]["stop_reason"],
            serde_json::Value::Null
        );

        let out = output_frames(&buf);
        let last = out.last().unwrap();
        assert_eq!(last["type"], "result", "终态帧仍是最后一条: {last}");
    }

    /// 兜底轮（`done` 之前一个终态帧都没有）同样可能落在流式中途：补帧之前先
    /// 收口，消费方不得拿到悬空的 `message_start`。
    #[test]
    fn done_closes_an_open_stream_message_first() {
        let (mut renderer, buf) = setup_streaming();

        assert!(!renderer.handle_event(&backend_error("boom")));
        assert!(!renderer.handle_event(&text_delta("cut")));
        assert!(renderer.handle_event(&done()));

        assert_eq!(
            stream_event_types(&buf),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{}",
            buf.text()
        );
        let out = output_frames(&buf);
        let last = out.last().unwrap();
        assert_eq!(last["type"], "result", "终态帧仍是最后一条: {last}");
        assert_eq!(last["subtype"], "error_during_execution");
    }

    /// 合成帧的 `session_id` 取事件的 `meta`（与打断合成帧同一口径），不是渲染器
    /// 自己那份——两者在多会话客户端里可能不同。
    #[test]
    fn the_synthesized_frame_carries_the_events_session_id() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        let done_elsewhere = decode(json!({
            "type": "done",
            "session_id": "sess-other",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(renderer.handle_event(&done_elsewhere));

        let parsed: serde_json::Value = serde_json::from_str(&buf.text()).unwrap();
        assert_eq!(parsed["session_id"], "sess-other");
    }

    /// 打断落在流式中途：先收口再发合成终态帧（终态帧纪律不变，
    /// `terminal_reason` 判据不变）。
    #[test]
    fn interrupted_turn_closes_an_open_stream_message_first() {
        let (mut renderer, buf) = setup_streaming();

        assert!(!renderer.handle_event(&text_delta("cut")));
        assert!(renderer.handle_event(&interrupted()));

        assert_eq!(
            stream_event_types(&buf),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{}",
            buf.text()
        );
        let out = output_frames(&buf);
        let last = out.last().unwrap();
        assert_eq!(last["type"], "result");
        assert_eq!(last["terminal_reason"], "aborted_streaming");

        // 打断后的下一轮不泄漏状态：新消息新 id。
        assert!(
            !renderer.handle_event(&turn_started()),
            "turn_started 不结束轮次"
        );
        assert!(!renderer.handle_event(&text_delta("next")));
        let frames = stream_frames(&buf);
        let starts: Vec<&str> = frames
            .iter()
            .filter(|f| f["event"]["type"] == "message_start")
            .map(|f| f["event"]["message"]["id"].as_str().unwrap())
            .collect();
        assert_eq!(starts.len(), 2);
        assert_ne!(starts[0], starts[1]);
    }

    /// 轮边界防御性收口：上一轮异常残留的在途消息在 `turn_started` 处收口
    /// （跨轮不泄漏）。
    #[test]
    fn turn_started_closes_a_leaked_stream_message() {
        let (mut renderer, buf) = setup_streaming();

        // delta 之后既没有快照也没有终态帧（异常帧序）——直接进入下一轮。
        assert!(!renderer.handle_event(&text_delta("orphan")));
        assert!(!renderer.handle_event(&turn_started()));

        assert_eq!(
            stream_event_types(&buf),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "{}",
            buf.text()
        );
    }

    /// text / json 输出模式：状态机根本不运行（没有协议通道），flag 无效。
    #[test]
    fn streaming_stays_off_for_text_and_json_output() {
        for format in [OutputFormat::Text, OutputFormat::Json] {
            let buf = CaptureSink::default();
            let mut renderer = StdioRenderer::new(
                format.clone(),
                Instant::now(),
                "sess-1".into(),
                buf.sink(),
                true,
            );

            assert!(!renderer.handle_event(&text_delta("hi")));
            assert!(renderer.handle_event(&turn_result("hi", false, "success")));

            assert!(
                !buf.text().contains("stream_event"),
                "{format:?}: {}",
                buf.text()
            );
        }
    }

    /// 空文本增量：仍要产 delta（消费方按块累积；空片段是真实事物流），
    /// 但不得开新块——这条只钉"不 panic、形状合法"。
    #[test]
    fn empty_text_delta_keeps_the_stream_well_formed() {
        let (mut renderer, buf) = setup_streaming();

        assert!(!renderer.handle_event(&text_delta("")));
        assert_eq!(
            stream_event_types(&buf),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta"
            ]
        );
    }

    /// 快照辅助：一条纯文本 assistant 消息。
    fn snapshot_with_text(uuid: &str, text: &str) -> WingEvent {
        decode(json!({
            "type": "assistant_turn",
            "uuid": uuid,
            "content_blocks": [{"type": "text", "text": text}],
            "model": "test-model",
            "stop_reason": "end_turn",
            "session_id": "sess-1",
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

    /// 模型占位：`message_start` 用最近一次 session_init / assistant_turn 的
    /// 模型名（增量事件不带模型）。
    #[test]
    fn message_start_uses_the_last_known_model() {
        let (mut renderer, buf) = setup_streaming();

        // 没有 session_init 时占位空串（真值在快照帧里）。
        assert!(!renderer.handle_event(&text_delta("a")));
        let frames = stream_frames(&buf);
        assert_eq!(frames[0]["event"]["message"]["model"], "");

        let init = decode(json!({
            "type": "session_init",
            "uuid": "u-init",
            "tools": [],
            "model": "test-model",
            "permission_mode": "bypassPermissions",
            "cwd": "/tmp",
            "session_id": "sess-1",
            "created_at": "2026-01-01T00:00:00",
            "request_id": "req-1",
        }));
        assert!(!renderer.handle_event(&init));

        assert!(!renderer.handle_event(&snapshot_with_text("u-1", "a")));
        assert!(!renderer.handle_event(&text_delta("b")));
        let frames = stream_frames(&buf);
        let second_start = frames
            .iter()
            .rev()
            .find(|f| f["event"]["type"] == "message_start")
            .unwrap();
        assert_eq!(second_start["event"]["message"]["model"], "test-model");
    }

    // ---- 错误轮的 errors 透传（SDK 无条件读它，缺字段会抛 TypeError） ----

    fn turn_error(errors: Vec<&str>, sub: &str) -> WingEvent {
        decode(json!({
            "type": "turn_result",
            "uuid": "u-err",
            "subtype": sub,
            "is_error": true,
            "result": null,
            "num_turns": 1,
            "duration_ms": 12,
            "errors": errors,
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-err",
        }))
    }

    /// 后端错误原文必须逐字进帧（两个 result 格式都一样），且错误帧不带
    /// `result`（`SDKResultError` 没有该字段）。
    #[test]
    fn error_result_forwards_backend_errors() {
        for format in [OutputFormat::Json, OutputFormat::StreamJson] {
            let (mut renderer, buf) = setup(format.clone());
            let event = turn_error(
                vec!["Reached max turns limit: 1", "second detail"],
                "error_max_turns",
            );

            assert!(renderer.handle_event(&event));

            let out = buf.text();
            assert_eq!(out.lines().count(), 1, "{format:?}: {out}");
            let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(parsed["type"], "result", "{format:?}");
            assert_eq!(parsed["subtype"], "error_max_turns", "{format:?}");
            assert_eq!(parsed["is_error"], true, "{format:?}");
            assert_eq!(
                parsed["errors"],
                json!(["Reached max turns limit: 1", "second detail"]),
                "{format:?}: {parsed}"
            );
            assert!(parsed.get("result").is_none(), "{format:?}: {parsed}");
            assert_eq!(renderer.exit_code(), std::process::ExitCode::FAILURE);
        }
    }

    /// 后端没给错误文本时也不能省略 `errors`（SDK 会 `errors.join`）——
    /// 补通用文案，形状仍然合法。
    #[test]
    fn error_result_without_backend_text_still_carries_errors() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        assert!(renderer.handle_event(&turn_error(Vec::new(), "error_during_execution")));

        let parsed: serde_json::Value =
            serde_json::from_str(buf.text().lines().last().unwrap()).unwrap();
        assert_eq!(parsed["errors"], json!(["error during execution"]));
        assert_eq!(parsed["is_error"], true);
    }

    /// 成功轮的形状不因这条规则漂移：有 `result`、无 `errors`/`terminal_reason`。
    #[test]
    fn success_result_keeps_its_shape() {
        let (mut renderer, buf) = setup(OutputFormat::StreamJson);

        assert!(renderer.handle_event(&turn_result("done", false, "success")));

        let parsed: serde_json::Value =
            serde_json::from_str(buf.text().lines().last().unwrap()).unwrap();
        assert_eq!(parsed["result"], "done");
        assert!(parsed.get("errors").is_none(), "{parsed}");
        assert!(parsed.get("terminal_reason").is_none(), "{parsed}");
    }

    /// text 模式的错误文案：`result` 只在成功轮有值，错误详情在后端 `errors`。
    #[test]
    fn error_line_text_prefers_result_then_backend_errors() {
        assert_eq!(error_line_text(Some("boom"), &[]), "boom");
        assert_eq!(error_line_text(Some(""), &["a".into(), "b".into()]), "a; b");
        assert_eq!(error_line_text(None, &["chain".into()]), "chain");
        assert_eq!(error_line_text(None, &[]), "error during execution");
    }

    /// text 模式没有终态文本可打（中止不是错误，stdout 保持干净），但同样
    /// 结束这一轮。
    #[test]
    fn text_mode_ends_on_an_interrupted_turn_without_output() {
        let (mut renderer, buf) = setup(OutputFormat::Text);

        assert!(renderer.handle_event(&interrupted()));
        assert_eq!(buf.text(), "");
        assert_eq!(renderer.exit_code(), std::process::ExitCode::SUCCESS);
    }
}
