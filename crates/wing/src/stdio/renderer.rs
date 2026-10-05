//! StdioRenderer — converts WingEvent to stdout output based on format.
#![allow(clippy::print_stdout)]

use std::io::Write;
use std::time::Instant;

use crate::protocol::WingEvent;
use crate::stdio::OutputFormat;
use crate::stdio::ndjson::{
    AssistantMessage, MessageContent, ResultMessage, SystemInitMessage, UserMessage,
};

/// Renders WingEvents to stdout in the specified format.
pub struct StdioRenderer {
    format: OutputFormat,
    start_time: Instant,
    session_id: String,
    exit_code: std::process::ExitCode,
    done: bool,
    /// Where rendered frames go. Production: stdout. Tests: a captured buffer
    /// — `wing -p`'s stdout contract (one frame per line, exit code) is the
    /// front-end's user-facing surface, so it has to be assertable without a
    /// real terminal.
    out: Box<dyn Write>,
}

impl StdioRenderer {
    pub fn new(format: OutputFormat, start_time: Instant, session_id: String) -> Self {
        Self::with_writer(format, start_time, session_id, Box::new(std::io::stdout()))
    }

    fn with_writer(
        format: OutputFormat,
        start_time: Instant,
        session_id: String,
        out: Box<dyn Write>,
    ) -> Self {
        Self {
            format,
            start_time,
            session_id,
            exit_code: std::process::ExitCode::SUCCESS,
            done: false,
            out,
        }
    }

    /// Write one frame line, keeping `println!`'s failure semantics: a stdout
    /// that cannot be written (e.g. the orchestrator closed the pipe) is fatal
    /// rather than silently swallowed.
    fn out_line(&mut self, text: &str) {
        writeln!(self.out, "{text}").unwrap_or_else(|e| panic!("failed printing to stdout: {e}"));
    }

    pub fn exit_code(&self) -> std::process::ExitCode {
        self.exit_code
    }

    /// Handle an event. Returns `true` when the renderer is done (TurnResult received).
    pub fn handle_event(&mut self, event: &WingEvent) -> bool {
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
                    result: result.clone().unwrap_or_default(),
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
                    result: result.clone().unwrap_or_default(),
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
}

#[cfg(test)]
mod tests {
    //! The `wing -p` stdout contract: what each output format writes and when
    //! the process fails. These are the assertions the retired e2e rig used to
    //! make through a real process — they live here now, at the format
    //! boundary, so a change to the frames fails offline.

    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// A `Write` sink whose bytes the test can read back.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBuf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn setup(format: OutputFormat) -> (StdioRenderer, SharedBuf) {
        let buf = SharedBuf::default();
        let renderer = StdioRenderer::with_writer(
            format,
            Instant::now(),
            "sess-1".into(),
            Box::new(buf.clone()),
        );
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
}
