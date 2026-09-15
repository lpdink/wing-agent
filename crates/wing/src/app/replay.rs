//! Session replay — parse SyncSession messages into ChatCells.
//!
//! Converts the message history from a SyncSessionEvent into
//! UI cells for display in the chat view. Both payload shapes decode through
//! the shared handwritten mirrors — no shadow structs live here:
//!
//! - message projection (`messages` / `uncommitted`) → [`SessionMessage`];
//! - fact-event nodes (`events`) → [`WingEvent`], the same typed decoder as
//!   the live stream (unknown types fall back to `Unknown`).

use crate::protocol::SessionMessage;
use crate::protocol::WingEvent;
use crate::shared::constants::TOOL_TODO;
use crate::shared::panels::ask::AskPanel;
use crate::shared::panels::ask::AskPayload;
use crate::ui::cells::ask_msg::AskMessage;
use crate::ui::cells::diff_view::DiffView;
use crate::ui::cells::thinking::ThinkingBlock;
use crate::ui::cells::todo_msg::TodoMessage;
use crate::ui::cells::tool_call::ToolCallBlock;
use crate::ui::chat_view::{ChatCell, ChatView};

/// Replay durable fact-event nodes onto the message-rendered chat view.
///
/// Capability dispatch — render the types that have a renderer, skip the rest
/// (forward tolerant). There is NO policy whitelist: the backend already
/// filters the chain down to fact events (`FACT_EVENTS` + pending asks), so the
/// frontend never encodes "this type is a Message twin, skip it". Adding a new
/// fact event later only needs a renderer arm here, no allow/deny registry.
///
/// Events are anchored by `tool_call_id` onto the ToolCall cells built from the
/// message projection (`messages` + `uncommitted`). The assembly order
/// (`messages → uncommitted → uncommitted_tools → events`) guarantees a diff's
/// anchor cell already exists — the producing tool_use block is *finalized*, so
/// it is in the projection. Unknown anchor (e.g. the assistant message was
/// compacted away) falls back to append, mirroring the live-path DiffContent
/// handler.
///
/// Returns the normalized ask panels it rendered so the caller can register
/// their answerable flows.
pub fn replay_events(chat: &mut ChatView, events: &[serde_json::Value]) -> Vec<AskPanel> {
    let mut asks: Vec<AskPanel> = Vec::new();
    for ev_val in events {
        // Decoded through the shared WingEvent mirror — the event types the
        // frontend does not know fall back to `Unknown` via `#[serde(other)]`.
        let ev = match WingEvent::from_history_value(ev_val) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Failed to decode replay event: {e}");
                continue;
            }
        };
        match ev {
            WingEvent::DiffContent {
                path,
                old_text,
                new_text,
                old_start_line,
                new_start_line,
                tool_call_id,
                ..
            } => {
                let diff = DiffView::new(path, old_text, new_text, old_start_line, new_start_line);
                if let Err(cell) = chat.insert_after_tool_call(&tool_call_id, ChatCell::Diff(diff))
                {
                    // Unknown anchor — same fallback as the live DiffContent handler.
                    chat.push(*cell);
                }
            }
            WingEvent::Ask {
                tool_call_id,
                questions,
                question,
                choices,
                required,
                ..
            } => {
                // Same normalization entry as the live path — the retired
                // shape folds into a panel here too, so the cell rendering and
                // the registered reply state can never disagree.
                let panel = AskPanel::from_ask(AskPayload {
                    tool_call_id: &tool_call_id,
                    questions: &questions,
                    question: &question,
                    choices: &choices,
                    required,
                });
                chat.push(ChatCell::Ask(AskMessage::new(panel.clone())));
                asks.push(panel);
            }
            // No renderer for this type → skip (forward tolerant).
            other => {
                tracing::debug!(
                    event_type = %other.event_type(),
                    "no replay renderer, skipping"
                );
            }
        }
    }
    asks
}

/// Replay session messages into the chat view.
///
/// Line counts are computed at render time by `Paragraph::line_count(width)`,
/// so no batch optimization is needed here.
pub fn replay_messages(chat: &mut ChatView, messages: &[serde_json::Value]) {
    for msg_val in messages {
        // Decoded through the shared SessionMessage mirror: missing optional
        // fields default, null optional fields count as absent, and a payload
        // that is not a Message projection is skipped (forward tolerant).
        let msg = match SessionMessage::from_json(msg_val) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Failed to decode replay message: {e}");
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
                if let Some(reasoning) = &msg.reasoning_content
                    && !reasoning.is_empty()
                {
                    let mut block = ThinkingBlock::new();
                    block.append(reasoning);
                    chat.push(ChatCell::Thinking(block));
                }

                // Tool calls.
                for tc in msg.tool_calls() {
                    let block = ToolCallBlock::new(
                        tc.name.clone(),
                        tc.arguments.clone().unwrap_or(serde_json::Value::Null),
                        tc.id.clone(),
                    );
                    chat.push(ChatCell::ToolCall(block));
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
    use crate::shared::panels::ask::PanelMode;
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

    // ── SessionMessage tolerance (pinned, decoding moved to protocol/) ──

    #[test]
    fn test_replay_message_missing_fields_default() {
        // A projection with only a role decodes — every other field defaults
        // (empty content produces no cell).
        let mut chat = ChatView::new();
        let messages = vec![json!({"role": "user"}), json!({"role": "assistant"})];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 0);
    }

    #[test]
    fn test_replay_message_null_optionals_tolerated() {
        // `tool_calls: null` / `reasoning_content: null` count as absent —
        // the message still renders its text.
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "content": "text",
            "reasoning_content": null,
            "tool_calls": null,
            "tool_call_id": null
        })];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 1);
        assert!(matches!(
            chat.cells[0].cell(),
            ChatCell::AssistantMessage(_)
        ));
    }

    #[test]
    fn test_replay_empty_tool_calls_array_renders_no_tool_call() {
        let mut chat = ChatView::new();
        let messages = vec![json!({"role": "assistant", "content": "x", "tool_calls": []})];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 1);
        assert!(matches!(
            chat.cells[0].cell(),
            ChatCell::AssistantMessage(_)
        ));
    }

    #[test]
    fn test_replay_message_without_role_is_skipped() {
        // No role → empty role → unknown-role skip (the old shadow struct
        // failed the whole decode here; the visible result is the same).
        let mut chat = ChatView::new();
        let messages = vec![json!({"content": "no role here"})];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 0);
    }

    #[test]
    fn test_replay_unknown_role_is_skipped() {
        let mut chat = ChatView::new();
        let messages = vec![json!({"role": "system", "content": "skipped"})];
        replay_messages(&mut chat, &messages);
        assert_eq!(chat.len(), 0);
    }

    #[test]
    fn test_replay_empty_payloads_are_noop() {
        let mut chat = ChatView::new();
        let empty: Vec<serde_json::Value> = Vec::new();
        replay_messages(&mut chat, &empty);
        let asks = replay_events(&mut chat, &empty);
        assert_eq!(chat.len(), 0);
        assert!(asks.is_empty());
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
    fn test_replay_events_capability_dispatch_skips_unrenderable() {
        // Capability dispatch (NOT a policy whitelist): a type with no renderer
        // here is skipped for forward tolerance. The backend already filters the
        // chain down to fact events, so twins (tool_call_result) never reach
        // replay; this asserts the frontend keeps no "twin must not render"
        // policy of its own — it simply has no renderer for these types.
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
        let asks = replay_events(&mut chat, &events);
        assert_eq!(chat.len(), 1, "types without a renderer are skipped");
        assert!(asks.is_empty());
    }

    #[test]
    fn test_replay_events_malformed_node_does_not_break_batch() {
        // A known type with a malformed payload (missing `new_text`) and a
        // `{}` payload are skipped individually; the valid node still renders.
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "tc-edit", "name": "Edit", "arguments": {"path": "main.rs"}}]
        })];
        let events = vec![
            json!({"type": "diff_content", "path": "main.rs"}),
            json!({}),
            json!({
                "type": "diff_content",
                "path": "main.rs",
                "old_text": null,
                "new_text": "fn main() {}",
                "tool_call_id": "tc-edit"
            }),
        ];
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);
        assert_eq!(chat.len(), 2);
        assert!(matches!(chat.cells[1].cell(), ChatCell::Diff(_)));
    }

    #[test]
    fn test_replay_events_without_meta_still_render() {
        // Chain records carry the event meta (created_at / request_id), but
        // replay never reads it — payload-only records decode (tolerance kept
        // from the old shadow struct, pinned here).
        let mut chat = ChatView::new();
        let events = vec![json!({
            "type": "ask",
            "tool_call_id": "ask-meta",
            "question": "proceed?",
            "choices": ["y", "n"],
        })];
        let asks = replay_events(&mut chat, &events);
        assert_eq!(chat.len(), 1);
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].tool_call_id, "ask-meta");
        assert_eq!(
            asks[0].questions[0].options.len(),
            2,
            "choices are normalized into options"
        );
        assert_eq!(asks[0].mode, PanelMode::Notice, "non-required → static");
    }

    #[test]
    fn test_replay_events_null_tool_call_id_behaves_like_absent() {
        // `tool_call_id: null` is normalized to "" (the old decoders read it
        // as `Option<String>`) — a diff falls back to append, an ask renders
        // with an empty correlation id, and both match the absent-key runs.
        let render = |tool_call_id: Option<serde_json::Value>| {
            let mut chat = ChatView::new();
            let mut diff = json!({
                "type": "diff_content",
                "path": "f.txt",
                "old_text": null,
                "new_text": "n",
            });
            let mut ask = json!({
                "type": "ask",
                "question": "proceed?",
                "choices": ["y", "n"],
            });
            if let Some(id) = tool_call_id {
                diff["tool_call_id"] = id.clone();
                ask["tool_call_id"] = id;
            }
            let asks = replay_events(&mut chat, &[diff, ask]);
            let kinds: Vec<&str> = chat
                .cells
                .iter()
                .map(|c| match c.cell() {
                    ChatCell::Diff(_) => "Diff",
                    ChatCell::Ask(_) => "Ask",
                    _ => "?",
                })
                .collect();
            (kinds, asks)
        };

        let (null_kinds, null_asks) = render(Some(serde_json::Value::Null));
        let (absent_kinds, absent_asks) = render(None);
        assert_eq!(null_kinds, vec!["Diff", "Ask"], "null id still renders");
        assert_eq!(null_kinds, absent_kinds);
        assert_eq!(null_asks.len(), 1);
        assert_eq!(null_asks[0].tool_call_id, "");
        assert_eq!(null_asks[0].tool_call_id, absent_asks[0].tool_call_id);
    }

    #[test]
    fn test_replay_events_renders_ask_cell_and_returns_it() {
        // 6.14: an ask event renders an Ask cell (reusing the live-path cell
        // construction) and is returned so the App can register the answerable
        // flow. Multi-question form carries questions + tool_call_id.
        let mut chat = ChatView::new();
        let events = vec![json!({
            "type": "ask",
            "tool_call_id": "ask-1",
            "questions": [{"id": "q1", "question": "proceed?", "choices": ["y", "n"]}],
        })];
        let asks = replay_events(&mut chat, &events);
        assert_eq!(chat.len(), 1);
        assert!(matches!(chat.cells[0].cell(), ChatCell::Ask(_)));
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].tool_call_id, "ask-1");
        assert_eq!(asks[0].questions.len(), 1);
        assert_eq!(asks[0].questions[0].question, "proceed?");
    }

    #[test]
    fn test_replay_events_normalizes_legacy_required_ask_into_a_panel() {
        // The retired single-question form goes through the same normalization
        // entry as the live path → a required-choice panel that answers with
        // the bare label. (Replay only sees still-pending asks: the backend
        // filters by live feedback waiters.)
        let mut chat = ChatView::new();
        let events = vec![json!({
            "type": "ask",
            "tool_call_id": "ask-2",
            "question": "dangerous, proceed?",
            "choices": ["yes", "no"],
            "required": true,
        })];
        let asks = replay_events(&mut chat, &events);
        assert!(matches!(chat.cells[0].cell(), ChatCell::Ask(_)));
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].mode, PanelMode::RequiredChoice);
        assert_eq!(asks[0].tool_call_id, "ask-2");
        assert_eq!(asks[0].questions.len(), 1);
        assert_eq!(asks[0].questions[0].question, "dangerous, proceed?");
        assert_eq!(
            asks[0].questions[0].options[0].label, "yes",
            "choices become options"
        );
        assert!(asks[0].is_interactive(), "required → answerable");
    }

    #[test]
    fn test_replay_events_legacy_non_required_ask_is_a_notice() {
        // A retired ask that was not required stays a static display: it is
        // rendered, but never registered for answering.
        let mut chat = ChatView::new();
        let events = vec![json!({
            "type": "ask",
            "tool_call_id": "ask-3",
            "question": "heads up",
            "choices": ["a", "b"],
        })];
        let asks = replay_events(&mut chat, &events);
        assert!(matches!(chat.cells[0].cell(), ChatCell::Ask(_)));
        assert_eq!(asks[0].mode, PanelMode::Notice);
        assert!(!asks[0].is_interactive());
    }

    #[test]
    fn test_replay_events_ask_and_diff_coexist_in_chain_order() {
        // Mixed fact events render in chain order: a diff anchors to its
        // ToolCall, an ask appends — no policy ordering between types.
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant", "content": "",
            "tool_calls": [{"id": "tc-e", "name": "Edit", "arguments": {"path": "f"}}],
        })];
        let events = vec![
            json!({
                "type": "diff_content", "path": "f", "old_text": null,
                "new_text": "x", "tool_call_id": "tc-e",
            }),
            json!({
                "type": "ask", "tool_call_id": "ask-1",
                "questions": [{"id": "q1", "question": "continue?", "choices": []}],
            }),
        ];
        replay_messages(&mut chat, &messages);
        let asks = replay_events(&mut chat, &events);
        // Order: ToolCall, Diff (anchored), Ask.
        assert!(matches!(chat.cells[0].cell(), ChatCell::ToolCall(_)));
        assert!(matches!(chat.cells[1].cell(), ChatCell::Diff(_)));
        assert!(matches!(chat.cells[2].cell(), ChatCell::Ask(_)));
        assert_eq!(asks.len(), 1);
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

    // ── windowed payloads (diff-payload-window) ─────────────

    /// A windowed payload keeps its absolute line numbers through replay:
    /// the gutter starts at `old_start_line` / `new_start_line` and the `@@`
    /// header uses them (frontend renders what the backend sent, verbatim).
    #[test]
    fn test_replay_events_windowed_diff_renders_absolute_lines() {
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "tc-edit", "name": "Edit", "arguments": {"path": "main.rs"}}]
        })];
        let events = vec![json!({
            "type": "diff_content",
            "path": "main.rs",
            "old_text": "line 7\nline 8\nline 9\nline 10\nline 11\nline 12\nline 13",
            "new_text": "line 7\nline 8\nline 9\nLINE TEN\nline 11\nline 12\nline 13",
            "old_start_line": 7,
            "new_start_line": 7,
            "tool_call_id": "tc-edit"
        })];
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);

        let ChatCell::Diff(diff) = chat.cells[1].cell() else {
            panic!("expected a Diff cell, got {:?}", chat.cells[1].cell());
        };
        let text: String = diff
            .to_lines(&crate::config::ThemePalette::default(), 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("@@ -7,7 +7,7 @@"), "{text}");
        assert!(text.contains("    7   7 │   line 7"), "{text}");
        assert!(text.contains("   10     │ - line 10"), "{text}");
        assert!(text.contains("       10 │ + LINE TEN"), "{text}");
        assert!(text.contains("   13  13 │   line 13"), "{text}");
    }

    /// A windowed payload without start lines (pre-windowing session) still
    /// renders — as a window starting at line 1.
    #[test]
    fn test_replay_events_payload_without_start_lines_defaults_to_one() {
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "tc-edit", "name": "Edit", "arguments": {"path": "main.rs"}}]
        })];
        let events = vec![json!({
            "type": "diff_content",
            "path": "main.rs",
            "old_text": "fn old() {}",
            "new_text": "fn new() {}",
            "tool_call_id": "tc-edit"
        })];
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);

        let ChatCell::Diff(diff) = chat.cells[1].cell() else {
            panic!("expected a Diff cell");
        };
        assert_eq!(diff.old_start_line, 1);
        assert_eq!(diff.new_start_line, 1);
        let text: String = diff
            .to_lines(&crate::config::ThemePalette::default(), 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("@@ -1,1 +1,1 @@"), "{text}");
    }

    /// `replace_all` produces several events with the SAME tool_call_id; the
    /// anchored insert must keep them in event order (each lands after the
    /// previous diff sibling), so resume shows the same order as live.
    #[test]
    fn test_replay_events_replace_all_windows_keep_order_under_one_anchor() {
        let mut chat = ChatView::new();
        let messages = vec![json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": "tc-all", "name": "Edit", "arguments": {"path": "f.txt"}}]
        })];
        let events: Vec<serde_json::Value> = (0..3)
            .map(|i| {
                json!({
                    "type": "diff_content",
                    "path": "f.txt",
                    "old_text": format!("old {i}"),
                    "new_text": format!("NEW {i}"),
                    "old_start_line": 10 + i,
                    "new_start_line": 10 + i,
                    "tool_call_id": "tc-all"
                })
            })
            .collect();
        replay_messages(&mut chat, &messages);
        replay_events(&mut chat, &events);

        // TC, then the three windows in emission order.
        assert_eq!(chat.len(), 4);
        let window_ids: Vec<usize> = chat
            .cells
            .iter()
            .filter_map(|c| match c.cell() {
                ChatCell::Diff(d) => Some(d.old_start_line),
                _ => None,
            })
            .collect();
        assert_eq!(window_ids, vec![10, 11, 12]);
    }
}
