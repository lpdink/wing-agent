//! Session replay — parse SyncSession messages into ChatCells.
//!
//! Converts the message history from a SyncSessionEvent into
//! UI cells for display in the chat view.

use serde::Deserialize;
use std::collections::HashMap;

use crate::app::constants::TOOL_TODO;
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

/// Replay session messages into the chat view.
///
/// Line counts are computed at render time by `Paragraph::line_count(width)`,
/// so no batch optimization is needed here.
pub fn replay_messages(chat: &mut ChatView, messages: &[serde_json::Value]) {
    // Map tool_call_id → cell index for pairing results.
    let mut tool_call_indices: HashMap<String, usize> = HashMap::new();

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
                        let idx = chat.len();
                        let block = ToolCallBlock::new(
                            tc.name.clone(),
                            tc.arguments.clone(),
                            tc.id.clone(),
                        );
                        chat.push(ChatCell::ToolCall(block));
                        tool_call_indices.insert(tc.id, idx);
                    }
                }

                // Assistant text content.
                if !msg.content.is_empty() {
                    chat.push(ChatCell::AssistantMessage(msg.content));
                }
            }
            "tool" => {
                // Match tool result to its call.
                if let Some(tool_call_id) = msg.tool_call_id {
                    if let Some(&idx) = tool_call_indices.get(&tool_call_id) {
                        // Check if this is a TodoWrite — keep ToolCallBlock + append TodoMessage.
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
                            chat.push(cell);
                        }
                    } else {
                        // Orphan tool result — render as system message.
                        chat.push(ChatCell::SystemMessage(format!(
                            "Tool result (orphan): {}",
                            msg.content
                        )));
                    }
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
}
