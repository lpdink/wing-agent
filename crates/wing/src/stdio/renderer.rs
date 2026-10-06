//! StdioRenderer — converts WingEvent to stdout output based on format.
#![allow(clippy::print_stdout)]

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
    done: bool,
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
            done: false,
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

    /// Handle an event. Returns `true` when the renderer is done (TurnResult
    /// received, or the run was interrupted — see [`Self::handle_interrupted`]).
    pub fn handle_event(&mut self, event: &WingEvent) -> bool {
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
                ..
            } => {
                if *is_error {
                    let msg = result.as_deref().unwrap_or("error during execution");
                    eprintln!("Error: {msg}");
                    self.exit_code = std::process::ExitCode::FAILURE;
                } else if let Some(text) = result {
                    self.out_line(text);
                }

                if subtype != "success" {
                    self.exit_code = std::process::ExitCode::FAILURE;
                }

                self.done = true;
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
                num_turns,
                duration_ms,
                usage,
                ..
            } => {
                let msg = ResultMessage {
                    msg_type: "result".into(),
                    subtype: subtype.clone(),
                    is_error: *is_error,
                    result: Some(result.clone().unwrap_or_default()),
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
                    errors: Vec::new(),
                    session_id: self.session_id.clone(),
                    uuid: uuid.clone(),
                };
                if let Ok(json) = serde_json::to_string(&msg) {
                    self.out_line(&json);
                }

                if *is_error || subtype != "success" {
                    self.exit_code = std::process::ExitCode::FAILURE;
                }

                self.done = true;
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
                num_turns,
                duration_ms,
                usage,
                ..
            } => {
                let msg = ResultMessage {
                    msg_type: "result".into(),
                    subtype: subtype.clone(),
                    is_error: *is_error,
                    result: Some(result.clone().unwrap_or_default()),
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
                    errors: Vec::new(),
                    session_id: self.session_id.clone(),
                    uuid: uuid.clone(),
                };
                self.emit_ndjson(&msg);

                if *is_error || subtype != "success" {
                    self.exit_code = std::process::ExitCode::FAILURE;
                }

                self.done = true;
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
        self.done = true;
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
