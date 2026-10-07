//! StdioRenderer — converts WingEvent to stdout output based on format.
// 错误轮把错误文本写到 stderr（stdout 是协议通道，只能有协议帧）。
#![allow(clippy::print_stderr)]

use std::sync::Arc;
use std::time::Instant;

use crate::protocol::WingEvent;
use crate::stdio::OutputFormat;
use crate::stdio::ndjson::{
    AssistantMessage, MessageContent, ResultMessage, SystemInitMessage, UserMessage,
};
use crate::stdio::stdout::StdoutSink;

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
/// 作为 errors 文本（真 CLI 的 `errors` 取值没有可观测样本，见 design「R1 返修」）。
const INTERRUPTED_BY_USER: &str = "Interrupted by user";

/// 后端中断收口时给未完成工具调用合成的结果文本
/// （`INTERRUPTED_RESULT`，`libs/core/wing/agent/tool_executor.py`）。
///
/// 工具阶段被打断时，后端**先**把每个未完成的调用按这条文本补成 tool_result
/// 并广播、**再**发 `interrupted` 事件（收口顺序，实测：Bash `sleep` 中打断
/// → `user(tool_result "Tool call interrupted by user.", is_error=true)` →
/// `interrupted`）。因此「中断落在工具阶段」只能靠这条合成结果识别；两侧一致
/// 由 `tests::interrupted_marker_matches_backend` 钉住。
const INTERRUPTED_TOOL_RESULT: &str = "Tool call interrupted by user.";

impl StdioRenderer {
    pub fn new(
        format: OutputFormat,
        start_time: Instant,
        session_id: String,
        out: Arc<StdoutSink>,
    ) -> Self {
        Self {
            format,
            start_time,
            session_id,
            exit_code: std::process::ExitCode::SUCCESS,
            out,
            pending_tools: 0,
            tool_phase_aborted: false,
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
    /// 常驻模式下这是「renderer 以轮为单位重置」的唯一入口；一次性流程里它只是
    /// 一次幂等清空（构造时账本为空）。
    pub fn begin_turn(&mut self) {
        self.pending_tools = 0;
        self.tool_phase_aborted = false;
    }

    /// Handle an event.
    ///
    /// Returns `true` when the **current turn** is done — a `turn_result` frame,
    /// or the synthesized terminal frame of an interrupted turn (see
    /// [`Self::handle_interrupted`]). Whether the *process* then exits is the
    /// driver's call: the one-shot flow exits on it as before; the resident flow
    /// (`crate::stdio::ExitPolicy`) exits only once stdin is closed.
    pub fn handle_event(&mut self, event: &WingEvent) -> bool {
        // 每轮重置：`turn_started` 是后端给出的轮边界（每轮必发一次）。
        if matches!(event, WingEvent::TurnStarted { .. }) {
            self.begin_turn();
            return false;
        }
        // 被打断的轮次后端**不发** `turn_result`（半截内容作为 partial
        // assistant 提交进链，见 design D10）：前端在这里补一条终态帧，否则
        // 编排器永远等不到终态——SDK 的 `streamInput` 在 canUseTool/hooks 存在时
        // 以「首个 result」门住 `endInput()`，缺了这条帧 stdin 永不关闭、
        // CLI 进程与 WS 订阅一起滞留（R1 B1）。
        if matches!(event, WingEvent::Interrupted { .. }) {
            return self.handle_interrupted(event);
        }
        match &self.format {
            OutputFormat::Text => self.handle_text(event),
            OutputFormat::Json => self.handle_json(event),
            OutputFormat::StreamJson => self.handle_stream_json(event),
        }
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
                let msg = ResultMessage {
                    msg_type: "result".into(),
                    subtype: subtype.clone(),
                    is_error: *is_error,
                    // 一条规则：错误帧不带 `result`（`SDKResultError` 没有该字段）。
                    result: (!*is_error).then(|| result.clone().unwrap_or_default()),
                    duration_ms: self.start_time.elapsed().as_millis() as i64,
                    duration_api_ms: *duration_ms,
                    num_turns: *num_turns,
                    total_cost_usd: 0.0,
                    usage: usage.clone().unwrap_or_else(|| {
                        serde_json::json!({
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cached_tokens": 0
                        })
                    }),
                    terminal_reason: None,
                    errors: error_texts(*is_error, errors),
                    session_id: self.session_id.clone(),
                    uuid: uuid.clone(),
                };
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
                        serde_json::from_value(block.clone())
                            .unwrap_or(MessageContent::Raw { raw: block.clone() })
                    })
                    .collect();

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
                        id: format!("msg_{uuid}"),
                        msg_type: "message".into(),
                        role: "assistant".into(),
                        model: model.clone(),
                        content,
                        stop_reason: stop_reason.clone().unwrap_or_else(|| "end_turn".into()),
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
                let msg = ResultMessage {
                    msg_type: "result".into(),
                    subtype: subtype.clone(),
                    is_error: *is_error,
                    // 一条规则：错误帧不带 `result`（`SDKResultError` 没有该字段）。
                    result: (!*is_error).then(|| result.clone().unwrap_or_default()),
                    duration_ms: self.start_time.elapsed().as_millis() as i64,
                    duration_api_ms: *duration_ms,
                    num_turns: *num_turns,
                    total_cost_usd: 0.0,
                    usage: usage.clone().unwrap_or_else(|| {
                        serde_json::json!({
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cached_tokens": 0
                        })
                    }),
                    terminal_reason: None,
                    errors: error_texts(*is_error, errors),
                    session_id: self.session_id.clone(),
                    uuid: uuid.clone(),
                };
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
        let terminal_reason = if self.pending_tools > 0 || self.tool_phase_aborted {
            "aborted_tools"
        } else {
            "aborted_streaming"
        };
        // text 模式没有"最终文本"可打；中止不是错误，stdout 保持干净（与
        // 错误轮的 text 行为一致：错误文本走 stderr）。
        if !matches!(self.format, OutputFormat::Text) {
            let msg = ResultMessage {
                msg_type: "result".into(),
                subtype: "error_during_execution".into(),
                is_error: true,
                result: None,
                duration_ms: self.start_time.elapsed().as_millis() as i64,
                duration_api_ms: 0,
                num_turns: 0,
                total_cost_usd: 0.0,
                usage: serde_json::json!({
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cached_tokens": 0
                }),
                terminal_reason: Some(terminal_reason.to_string()),
                errors: vec![INTERRUPTED_BY_USER.to_string()],
                session_id: meta
                    .session_id
                    .clone()
                    .unwrap_or_else(|| self.session_id.clone()),
                // 打断事件不带链 uuid（它不是链上事实）：这里造一个稳定可追溯的
                // 帧 id，供消费方去重/关联。
                uuid: crate::protocol::generate_request_id(),
            };
            self.emit_ndjson(&msg);
        }
        self.exit_code = std::process::ExitCode::SUCCESS;
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
    use serde_json::json;

    fn setup(format: OutputFormat) -> (StdioRenderer, CaptureSink) {
        let buf = CaptureSink::default();
        let renderer = StdioRenderer::new(format, Instant::now(), "sess-1".into(), buf.sink());
        (renderer, buf)
    }

    fn decode(value: serde_json::Value) -> WingEvent {
        serde_json::from_value(value).expect("fixture decodes as a WingEvent")
    }

    fn turn_result(result: &str, is_error: bool, subtype: &str) -> WingEvent {
        decode(json!({
            "type": "turn_result",
            "uuid": "u-1",
            "subtype": subtype,
            "is_error": is_error,
            "result": result,
            "num_turns": 2,
            "duration_ms": 30,
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

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

    // ---- interrupted: 被打断轮次的终态帧（R1 B1） ----

    /// 被打断事件（后端对被打断轮次不发 turn_result，只发这个）。
    fn interrupted() -> WingEvent {
        decode(json!({
            "type": "interrupted",
            "dropped_request_ids": [],
            "session_id": "sess-1",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-9",
        }))
    }

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

    /// B1 的核心：被打断的轮次必须有**终态帧**，否则 SDK 的 endInput 门
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

    fn turn_started() -> WingEvent {
        decode(json!({
            "type": "turn_started",
            "session_id": "sess-1",
            "created_at": "2025-01-01T00:00:00",
            "request_id": "req-1",
        }))
    }

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

    // ---- S1: 错误轮的 errors 透传（SDK 无条件读它，缺字段会抛 TypeError） ----

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
