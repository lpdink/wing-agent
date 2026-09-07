//! Session replay — parse SyncSession messages into ChatCells.
//!
//! Converts the message history from a SyncSessionEvent into
//! UI cells for display in the chat view.

use serde::Deserialize;

use crate::app::constants::TOOL_TODO;
use crate::ui::cells::diff_view::DiffView;
use crate::ui::cells::thinking::ThinkingBlock;
use crate::ui::cells::todo_msg::TodoMessage;
use crate::ui::cells::tool_call::ToolCallBlock;
use crate::ui::chat_view::{ChatCell, ChatView};

/// A tool call from the assistant's message.
#[derive(Debug, Deserialize)]
struct ReplayToolCall {
    id: String,
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

/// A message from the session history.
#[derive(Debug, Deserialize)]
struct ReplayMessage {
    role: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ReplayToolCall>>,
    #[serde(default)]
    tool_call_id: Option<String>,
}

/// A durable event node from the session's mixed chain.
#[derive(Debug, Deserialize)]
struct ReplayEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
    #[serde(default)]
    tool_call_id: Option<String>,
}

/// Replay durable event nodes onto the message-rendered chat view.
///
/// Events are anchored by `tool_call_id` onto the ToolCall cells built from
/// the message projection. Chain order places events before their assistant
/// message, but replay runs the message pass first — so anchors exist by the
/// time events are applied. Unknown anchor (e.g. a tool call whose assistant
/// message was truncated away) falls back to append, mirroring the live-path
/// DiffContent handler.
///
/// Twin events (tool_call_result etc.) are skipped: their facts are already
/// rendered from the message projection — the log keeps them for future
/// consumers (export/analysis), not for the TUI replay.
pub fn replay_events(chat: &mut ChatView, events: &[serde_json::Value]) {
    for ev_val in events {
        let ev: ReplayEvent = match serde_json::from_value(ev_val.clone()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Failed to parse replay event: {e}");
                continue;
            }
        };
        if ev.event_type != "diff_content" {
            continue;
        }
        let (Some(path), Some(new_text)) = (ev.path, ev.new_text) else {
            tracing::warn!(event_type = %ev.event_type, "diff event missing fields");
            continue;
        };
        let diff = DiffView::new(path, ev.old_text, new_text);
        let tool_call_id = ev.tool_call_id.unwrap_or_default();
        if let Err(cell) = chat.insert_after_tool_call(&tool_call_id, ChatCell::Diff(diff)) {
            // Unknown anchor — same fallback as the live DiffContent handler.
            chat.push(*cell);
        }
    }
}

/// Replay session messages into the chat view.
///
/// Line counts are computed at render time by `Paragraph::line_count(width)`,
/// so no batch optimization is needed here.
pub fn replay_messages(chat: &mut ChatView, messages: &[serde_json::Value]) {
    for msg_val in messages {
        let msg: ReplayMessage = match serde_json::from_value(msg_val.clone()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Failed to parse replay message: {e}");
                continue;
            }
        };

        match msg.role.as_str() {
            "user" => {
                if !msg.content.is_empty() {
                    chat.push(ChatCell::UserMessage(msg.content));
                }
            }
            "assistant" => {
                // Reasoning content (thinking).
                if let Some(reasoning) = msg.reasoning_content
                    && !reasoning.is_empty()
                {
                    let mut block = ThinkingBlock::new();
                    block.append(&reasoning);
                    chat.push(ChatCell::Thinking(block));
                }

                // Tool calls.
                if let Some(tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        let block = ToolCallBlock::new(
                            tc.name.clone(),
                            tc.arguments.clone(),
                            tc.id.clone(),
                        );
                        chat.push(ChatCell::ToolCall(block));
                    }
                }

                // Assistant text content.
                if !msg.content.is_empty() {
                    chat.push(ChatCell::AssistantMessage(msg.content));
                }
            }
            "tool" => {
                // Match tool result to its call, addressed by id — tool
                // messages are persisted in completion order (concurrent
                // execution), so positional pairing is not possible.
                let Some(tool_call_id) = msg.tool_call_id else {
                    continue;
                };
                let Some(idx) = chat.tool_call_index(&tool_call_id) else {
                    // Orphan tool result — render as system message.
                    chat.push(ChatCell::SystemMessage(format!(
                        "Tool result (orphan): {}",
                        msg.content
                    )));
                    continue;
                };

                // Check if this is a TodoWrite — keep ToolCallBlock,
                // insert TodoMessage directly after it.
                let todo_cell = chat.cells.get(idx).and_then(|c| {
                    if let ChatCell::ToolCall(block) = c.cell()
                        && block.tool_name == TOOL_TODO
                    {
                        TodoMessage::from_tool_args(&block.tool_args).map(ChatCell::Todo)
                    } else {
                        None
                    }
                });

                chat.set_tool_result_by_index(idx, msg.content, true);

                if let Some(cell) = todo_cell {
                    // Cannot fail — the ToolCall cell was located above.
                    let _ = chat.insert_after_tool_call(&tool_call_id, cell);
                }
            }
            _ => {
                tracing::debug!("Unknown role in replay: {}", msg.role);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_replay_user_message() {
        let mut chat = ChatView::new();
        let messages = vec![json!({"role": "user", "content": "hello"})];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 1);
        assert!(matches!(chat.cells[0].cell(), ChatCell::UserMessage(_)));
    }

    #[test]
    fn test_replay_assistant_with_thinking() {
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "reasoning_content": "Let me think...",
            "content": "Here is my answer."
        })];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 2);
        assert!(matches!(chat.cells[0].cell(), ChatCell::Thinking(_)));
        assert!(matches!(
            chat.cells[1].cell(),
            ChatCell::AssistantMessage(_)
        ));
    }

    #[test]
    fn test_replay_tool_call_and_result() {
        let mut chat = ChatView::new();
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "tc1",
                    "name": "Read",
                    "arguments": {"path": "main.rs"}
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "tc1",
                "content": "fn main() {}"
            }),
        ];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 1);
        if let ChatCell::ToolCall(block) = chat.cells[0].cell() {
            assert_eq!(block.tool_name, "Read");
            assert!(block.result.is_some());
            assert_eq!(
                block.status,
                crate::ui::cells::tool_call::ToolStatus::Success
            );
        } else {
            panic!("Expected ToolCall");
        }
    }

    #[test]
    fn test_replay_orphan_tool_result() {
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "tool",
            "tool_call_id": "nonexistent",
            "content": "orphan result"
        })];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 1);
        assert!(matches!(chat.cells[0].cell(), ChatCell::SystemMessage(_)));
    }

    #[test]
    fn test_replay_empty_content() {
        let mut chat = ChatView::new();
        let messages = vec![
            json!({"role": "user", "content": ""}),
            json!({"role": "assistant", "content": ""}),
        ];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 0); // Empty messages should be skipped
    }

    #[test]
    fn test_replay_invalid_message() {
        let mut chat = ChatView::new();
        let messages = vec![json!({"invalid": "message"})];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 0); // Should skip invalid messages
    }

    #[test]
    fn test_replay_todo_write_renders_tool_call_and_todo_message() {
        let mut chat = ChatView::new();
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "tc_todo",
                    "name": "TodoWrite",
                    "arguments": {
                        "todos": [
                            {"content": "task 1", "status": "completed"},
                            {"content": "task 2", "status": "in_progress"}
                        ]
                    }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "tc_todo",
                "content": "Todo updated."
            }),
        ];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 2);
        // First: ToolCallBlock (shows tool name + status).
        assert!(
            matches!(chat.cells[0].cell(), ChatCell::ToolCall(_)),
            "Expected ToolCallBlock"
        );
        // Second: TodoMessage (shows the list).
        assert!(
            matches!(chat.cells[1].cell(), ChatCell::Todo(_)),
            "Expected TodoMessage"
        );
    }

    #[test]
    fn test_replay_todo_anchored_under_out_of_order_results() {
        // Concurrent execution: results are persisted in completion order
        // (Bash finished first), but the Todo cell must still land directly
        // after its own TodoWrite ToolCall cell.
        let mut chat = ChatView::new();
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "id": "tc_todo",
                        "name": "TodoWrite",
                        "arguments": {
                            "todos": [{"content": "task", "status": "pending"}]
                        }
                    },
                    {
                        "id": "tc_bash",
                        "name": "Bash",
                        "arguments": {"command": "ls"}
                    }
                ]
            }),
            // Bash result arrives first (faster completion).
            json!({
                "role": "tool",
                "tool_call_id": "tc_bash",
                "content": "file.txt"
            }),
            json!({
                "role": "tool",
                "tool_call_id": "tc_todo",
                "content": "Todo updated."
            }),
        ];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 3);
        // Order: ToolCall(TodoWrite), Todo, ToolCall(Bash).
        match chat.cells[0].cell() {
            ChatCell::ToolCall(block) => assert_eq!(block.tool_name, "TodoWrite"),
            other => panic!("Expected TodoWrite ToolCall, got {other:?}"),
        }
        assert!(
            matches!(chat.cells[1].cell(), ChatCell::Todo(_)),
            "Todo cell must be anchored directly after its ToolCall"
        );
        match chat.cells[2].cell() {
            ChatCell::ToolCall(block) => {
                assert_eq!(block.tool_name, "Bash");
                assert!(block.result.is_some(), "Bash result must still be set");
            }
            other => panic!("Expected Bash ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn test_replay_multiple_todos_out_of_order() {
        // Two TodoWrites + one Bash, results persisted in reverse completion
        // order — each todo list must land under its own ToolCall cell.
        let mut chat = ChatView::new();
        let todo_args = |task: &str| json!({"todos": [{"content": task, "status": "pending"}]});
        let messages = vec![
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"id": "tc_todo1", "name": "TodoWrite", "arguments": todo_args("first")},
                    {"id": "tc_bash", "name": "Bash", "arguments": {"command": "ls"}},
                    {"id": "tc_todo2", "name": "TodoWrite", "arguments": todo_args("second")}
                ]
            }),
            json!({"role": "tool", "tool_call_id": "tc_todo2", "content": "ok"}),
            json!({"role": "tool", "tool_call_id": "tc_bash", "content": "ok"}),
            json!({"role": "tool", "tool_call_id": "tc_todo1", "content": "ok"}),
        ];
        replay_messages(&mut chat, &messages);

        // Order: TC(todo1), Todo1, TC(bash), TC(todo2), Todo2.
        assert_eq!(chat.len(), 5);
        let names: Vec<String> = chat
            .cells
            .iter()
            .map(|c| match c.cell() {
                ChatCell::ToolCall(b) => format!("TC({})", b.tool_name),
                ChatCell::Todo(_) => "Todo".to_string(),
                _ => "?".to_string(),
            })
            .collect();
        assert_eq!(
            names,
            vec!["TC(TodoWrite)", "Todo", "TC(Bash)", "TC(TodoWrite)", "Todo"]
        );
    }

    // ── replay_events：事件重放（diff 锚定）─────────────

    #[test]
    fn test_replay_events_anchors_diff_to_tool_call() {
        // Message pass builds the ToolCall cell; the event pass anchors the
        // diff directly after it — resume restores the diff view.
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "tc-edit",
                "name": "Edit",
                "arguments": {"path": "main.rs"}
            }]
        })];
        let events = vec![json!({
            "role": "event",
            "type": "diff_content",
            "path": "main.rs",
            "old_text": "fn main() {}",
            "new_text": "fn main() { println!(\"hi\"); }",
            "tool_call_id": "tc-edit"
        })];
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);

        assert_eq!(chat.len(), 2);
        assert!(
            matches!(chat.cells[0].cell(), ChatCell::ToolCall(_)),
            "ToolCall cell first"
        );
        assert!(
            matches!(chat.cells[1].cell(), ChatCell::Diff(_)),
            "Diff cell anchored directly after its ToolCall"
        );
    }

    #[test]
    fn test_replay_events_event_before_anchor_in_chain_order() {
        // Chain order places the diff BEFORE the assistant message (events
        // are persisted during tool execution, ahead of the turn commit).
        // The two-pass replay (messages first, events second) makes the
        // anchor exist by the time the diff is applied — order-independent.
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "tc-write",
                "name": "Write",
                "arguments": {"path": "new.txt"}
            }]
        })];
        // Events list in chain order (diff first) — anchor still resolves.
        let events = vec![json!({
            "type": "diff_content",
            "path": "new.txt",
            "old_text": null,
            "new_text": "hello",
            "tool_call_id": "tc-write"
        })];
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);
        assert_eq!(chat.len(), 2);
        assert!(matches!(chat.cells[1].cell(), ChatCell::Diff(_)));
    }

    #[test]
    fn test_replay_events_unknown_anchor_falls_back_to_append() {
        // A diff whose ToolCall cell is gone (e.g. the assistant message was
        // truncated by max_tokens) appends instead of erroring — mirrors the
        // live DiffContent handler's fallback.
        let mut chat = ChatView::new();
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let events = vec![json!({
            "type": "diff_content",
            "path": "gone.rs",
            "old_text": null,
            "new_text": "content",
            "tool_call_id": "tc-unknown"
        })];
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);
        assert_eq!(chat.len(), 2);
        assert!(matches!(chat.cells[1].cell(), ChatCell::Diff(_)));
    }

    #[test]
    fn test_replay_events_skips_twin_and_unknown_events() {
        // Twin events (tool_call_result) and unknown future event types are
        // skipped — their facts are rendered from the message projection /
        // tolerated for forward compatibility.
        let mut chat = ChatView::new();
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let events = vec![
            json!({
                "type": "tool_call_result",
                "tool_call_id": "tc-1",
                "tool_result": "ok"
            }),
            json!({"type": "from_the_future", "anything": true}),
        ];
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);
        assert_eq!(chat.len(), 1, "twin/unknown events must not render");
    }

    #[test]
    fn test_replay_events_multiple_diffs_keep_anchor_order() {
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "tc-a", "name": "Edit", "arguments": {"path": "a"}},
                {"id": "tc-b", "name": "Edit", "arguments": {"path": "b"}}
            ]
        })];
        let events = vec![
            json!({
                "type": "diff_content",
                "path": "b", "old_text": null, "new_text": "B",
                "tool_call_id": "tc-b"
            }),
            json!({
                "type": "diff_content",
                "path": "a", "old_text": null, "new_text": "A",
                "tool_call_id": "tc-a"
            }),
        ];
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);

        // Order: TC(a), Diff(a), TC(b), Diff(b) — each diff under its own
        // ToolCall regardless of event arrival order.
        let kinds: Vec<String> = chat
            .cells
            .iter()
            .map(|c| match c.cell() {
                ChatCell::ToolCall(b) => format!(
                    "TC({})",
                    b.tool_args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                ),
                ChatCell::Diff(d) => format!("Diff({})", d.path),
                _ => "?".to_string(),
            })
            .collect();
        assert_eq!(kinds, vec!["TC(a)", "Diff(a)", "TC(b)", "Diff(b)"]);
    }
}
