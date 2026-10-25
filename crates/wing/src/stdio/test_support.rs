//! stdio 单测共用的事件 fixture 与渲染器装配。
//!
//! 两个测试模块用的是同一批后端事件形状：`stdio::tests`（驱动侧的退出裁决）与
//! `stdio::renderer::tests`（三个输出格式的帧契约）。形状只留一份——事件字段随
//! 协议变动时不会只改到一半。

use std::time::Instant;

use serde_json::json;

use crate::protocol::WingEvent;
use crate::stdio::OutputFormat;
use crate::stdio::renderer::StdioRenderer;
use crate::stdio::stdout::CaptureSink;

/// fixture 里的会话 id（`session_id:` 行与终态帧的 `session_id` 都用它）。
pub(crate) const SESSION_ID: &str = "sess-1";

/// 一条事件 fixture（解不出来 = fixture 自己就是错的）。
pub(crate) fn event(value: serde_json::Value) -> WingEvent {
    serde_json::from_value(value).expect("fixture decodes as a WingEvent")
}

/// 轮边界（`run_turn` 入口 emit，先于该轮任何内容帧）。
pub(crate) fn turn_started() -> WingEvent {
    event(json!({
        "type": "turn_started",
        "session_id": SESSION_ID,
        "created_at": "2025-01-01T00:00:00",
        "request_id": "req-1",
    }))
}

/// 一轮的终态帧（成功 / `max_turns` / 异常收口三种 subtype）。
pub(crate) fn turn_result(result: &str, is_error: bool, subtype: &str) -> WingEvent {
    event(json!({
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

/// 一次 assistant 快照（`content_blocks` 是 Anthropic 块数组原样透传）。
pub(crate) fn assistant(text: &str) -> WingEvent {
    event(json!({
        "type": "assistant_turn",
        "uuid": "u-1",
        "content_blocks": [{"type": "text", "text": text}],
        "model": "test-model",
        "stop_reason": "end_turn",
        "session_id": SESSION_ID,
        "created_at": "2025-01-01T00:00:00",
        "request_id": "req-1",
    }))
}

/// 后端错误帧（不是轮末信号，只是原因文本）。
pub(crate) fn backend_error(message: &str) -> WingEvent {
    event(json!({
        "type": "error",
        "message": message,
        "session_id": SESSION_ID,
        "created_at": "2025-01-01T00:00:00",
        "request_id": "req-1",
    }))
}

/// 轮末信号（`run_turn` 每条收口路径末尾都发）。
pub(crate) fn done() -> WingEvent {
    event(json!({
        "type": "done",
        "session_id": SESSION_ID,
        "created_at": "2025-01-01T00:00:00",
        "request_id": "req-1",
    }))
}

/// 被打断的轮次（后端对它**不发** `turn_result`）。
pub(crate) fn interrupted() -> WingEvent {
    event(json!({
        "type": "interrupted",
        "dropped_request_ids": [],
        "session_id": SESSION_ID,
        "created_at": "2025-01-01T00:00:00",
        "request_id": "req-9",
    }))
}

/// 以捕获件为出口的渲染器（`format` 决定输出契约）。
pub(crate) fn renderer(format: OutputFormat) -> (StdioRenderer, CaptureSink) {
    renderer_with(format, false)
}

/// 带 `--include-partial-messages` 的 stream-json 渲染器。
pub(crate) fn streaming_renderer() -> (StdioRenderer, CaptureSink) {
    renderer_with(OutputFormat::StreamJson, true)
}

fn renderer_with(
    format: OutputFormat,
    include_partial_messages: bool,
) -> (StdioRenderer, CaptureSink) {
    let buf = CaptureSink::default();
    let renderer = StdioRenderer::new(
        format,
        Instant::now(),
        SESSION_ID.into(),
        buf.sink(),
        include_partial_messages,
    );
    (renderer, buf)
}
