//! StdioRenderer — converts WingEvent to stdout output based on format.
#![allow(clippy::print_stdout)]

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
}

impl StdioRenderer {
    pub fn new(format: OutputFormat, start_time: Instant, session_id: String) -> Self {
        Self {
            format,
            start_time,
            session_id,
            exit_code: std::process::ExitCode::SUCCESS,
            done: false,
        }
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
                    // println! adds trailing newline automatically.
                    println!("{text}");
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
                    println!("{json}");
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

    fn emit_ndjson<T: serde::Serialize>(&self, msg: &T) {
        if let Ok(json) = serde_json::to_string(msg) {
            println!("{json}");
        }
    }
}
