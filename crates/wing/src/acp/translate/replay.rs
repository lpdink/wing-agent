//! `session/load` 的历史回放投影：`sync_session` 快照 → `session/update` 序列。
//!
//! 输入是 [`WingEvent::SyncSession`] 的两个重放组（顺序 `messages → uncommitted →
//! uncommitted_tools → events`）；**只回放 `messages` 与 `events`**，`uncommitted*`
//! （进行中的半成品）跳过并记 debug——回放是「一次性投影」，把半成品状态发给客户端
//! 正是 `session/load` 要避免的。
//!
//! 映射（与 05 步骤任务书的表格逐行对应）：
//!
//! | 输入 | 输出 |
//! |------|------|
//! | `role=user` | `UserMessageChunk(Text)` |
//! | `role=assistant` | `reasoning_content` → `AgentThoughtChunk`；`content` → `AgentMessageChunk`；`tool_calls[]` → `ToolCall`（创建，in_progress） |
//! | `role=tool` | 该 `tool_call_id` 的 `ToolCallUpdate`（completed + content） |
//! | `role=system` / 其它 / 解码失败 | 跳过（debug） |
//! | `events` 组的 `diff_content` | 对应卡片的 `ToolCallUpdate`（content 追加 `Diff`） |
//! | `events` 组的其它事件 | 忽略（debug） |
//!
//! 工具卡片写进**会话级** [`ToolCards`]（与实时映射共用同一份记忆与
//! title/kind/locations 规则）：回放建好的卡片，后续实时 `tool_call_result` /
//! `diff_content` 能继续锚定；回放之后出现的 tool_call_id 仍是新卡片。
//!
//! 已知口径：历史消息投影没有失败标记（`SessionMessage` 无 `is_error`）→ 回放的工具
//! 结果一律 `completed`；没有匹配 tool 消息的 tool_call 保持 `in_progress`
//! （中断轮次的真实状态）。

use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::schema::v1::ContentChunk;
use agent_client_protocol::schema::v1::SessionUpdate;

use crate::protocol::SessionMessage;
use crate::protocol::WingEvent;

use super::ToolCards;

/// 回放投影入口：`sync_session` 快照 → update 序列（写进会话级卡片记忆）。
///
/// 非 `sync_session` 事件 → 空序列（调用方只会在快照上调用它）。
pub fn replay_updates(cards: &mut ToolCards, event: &WingEvent) -> Vec<SessionUpdate> {
    let WingEvent::SyncSession {
        messages,
        uncommitted,
        uncommitted_tools,
        events,
        ..
    } = event
    else {
        return Vec::new();
    };

    if uncommitted.is_some() || !uncommitted_tools.is_empty() {
        tracing::debug!(
            uncommitted = uncommitted.is_some(),
            uncommitted_tools = uncommitted_tools.len(),
            "acp: in-progress replay materials skipped (not sent to the client)",
        );
    }

    let mut updates = Vec::new();
    for message in messages {
        updates.extend(message_updates(cards, message));
    }
    for value in events {
        updates.extend(fact_event_updates(cards, value));
    }
    updates
}

/// 一条历史 Message 投影（`SessionMessage`）→ update 序列。
fn message_updates(cards: &mut ToolCards, value: &serde_json::Value) -> Vec<SessionUpdate> {
    let message = match SessionMessage::from_json(value) {
        Ok(message) => message,
        Err(err) => {
            tracing::debug!(
                error = %err,
                "acp: history record skipped (not a Message projection)",
            );
            return Vec::new();
        }
    };
    match message.role.as_str() {
        "user" => user_chunk(&message.content),
        "assistant" => assistant_updates(cards, &message),
        "tool" => tool_updates(cards, &message),
        role => {
            // system / 空 role / 未知角色：ACP 侧没有对应物。
            tracing::debug!(role, "acp: history message role skipped");
            Vec::new()
        }
    }
}

/// user 消息 → `UserMessageChunk`（空内容不产帧，与实时文本映射同口径）。
fn user_chunk(content: &str) -> Vec<SessionUpdate> {
    if content.is_empty() {
        return Vec::new();
    }
    vec![SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::from(content.to_string()),
    ))]
}

/// assistant 消息 → 思考块 + 文本块 + 逐个工具卡片（创建）。
///
/// 顺序固定：先 `reasoning_content`（思考在文本之前），再 `content`，最后 `tool_calls`
/// （工具卡片跟在它所属的文本块之后）。
fn assistant_updates(cards: &mut ToolCards, message: &SessionMessage) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    if let Some(reasoning) = message.reasoning_content.as_deref() {
        updates.extend(super::text_update(reasoning, true));
    }
    updates.extend(super::text_update(&message.content, false));
    for call in message.tool_calls() {
        updates.extend(cards.upsert_call(&call.id, &call.name, call.arguments.clone()));
    }
    updates
}

/// tool 消息 → 该卡片的结果收口。
///
/// 历史投影没有失败标记 → 一律 `completed`；没有匹配 cards 记忆的 id 也照发 update
/// （状态与内容是主信号，客户端会自行补卡片——与实时 `tool_call_result` 同口径）。
fn tool_updates(cards: &mut ToolCards, message: &SessionMessage) -> Vec<SessionUpdate> {
    let Some(tool_call_id) = message.tool_call_id.as_deref().filter(|id| !id.is_empty()) else {
        tracing::debug!("acp: tool history record without tool_call_id; skipped");
        return Vec::new();
    };
    cards.apply_result(tool_call_id, &message.content, true)
}

/// `events` 组（链上的 fact 事件）→ update 序列：**只认 `diff_content`**。
///
/// text / reasoning / tool_call / tool_call_result 都已经由 `messages` 组覆盖，
/// 重复投递只会制造重复卡片；diff 是 Message 投影覆盖不到的 UI 事实，必须补。
fn fact_event_updates(cards: &mut ToolCards, value: &serde_json::Value) -> Vec<SessionUpdate> {
    match WingEvent::from_history_value(value) {
        Ok(event @ WingEvent::DiffContent { .. }) => cards.apply(&event),
        Ok(event) => {
            tracing::debug!(
                event_type = event.event_type(),
                "acp: history event skipped"
            );
            Vec::new()
        }
        Err(err) => {
            tracing::debug!(error = %err, "acp: history event skipped (decode failed)");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::Diff;
    use agent_client_protocol::schema::v1::ToolCallContent;
    use agent_client_protocol::schema::v1::ToolCallStatus;
    use agent_client_protocol::schema::v1::ToolKind;
    use serde_json::json;

    /// `sync_session` fixture（真实网关线上的形状）。
    fn sync_event(messages: serde_json::Value, events: serde_json::Value) -> WingEvent {
        let value = json!({
            "type": "sync_session",
            "session_id": "s1",
            "status": "idle",
            "messages": messages,
            "events": events,
            "created_at": "2026-01-01T00:00:00+00:00",
            "request_id": "req-sync",
        });
        serde_json::from_value(value.clone())
            .unwrap_or_else(|err| panic!("sync_session fixture must decode: {err}\n{value}"))
    }

    /// 回放 + 顺手断言输出帧数。
    fn replay(cards: &mut ToolCards, event: &WingEvent) -> Vec<SessionUpdate> {
        replay_updates(cards, event)
    }

    // ---- user ----

    #[test]
    fn user_message_maps_to_user_message_chunk() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{"role": "user", "content": "看下这个 bug"}]),
                json!([]),
            ),
        );
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            SessionUpdate::UserMessageChunk(chunk) => {
                assert_eq!(
                    chunk.content,
                    ContentBlock::from("看下这个 bug".to_string())
                );
            }
            other => panic!("expected user_message_chunk, got {other:?}"),
        }
    }

    #[test]
    fn empty_user_content_produces_no_frame() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(json!([{"role": "user", "content": ""}]), json!([])),
        );
        assert!(updates.is_empty());
    }

    // ---- assistant ----

    #[test]
    fn assistant_maps_thinking_then_text() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{
                    "role": "assistant",
                    "content": "答案",
                    "reasoning_content": "先想一下",
                }]),
                json!([]),
            ),
        );
        assert_eq!(updates.len(), 2, "思考块在前、文本块在后");
        match &updates[0] {
            SessionUpdate::AgentThoughtChunk(chunk) => {
                assert_eq!(chunk.content, ContentBlock::from("先想一下".to_string()));
            }
            other => panic!("expected agent_thought_chunk, got {other:?}"),
        }
        match &updates[1] {
            SessionUpdate::AgentMessageChunk(chunk) => {
                assert_eq!(chunk.content, ContentBlock::from("答案".to_string()));
            }
            other => panic!("expected agent_message_chunk, got {other:?}"),
        }
    }

    #[test]
    fn assistant_without_text_only_emits_what_exists() {
        let mut cards = ToolCards::default();
        // 仅 reasoning。
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{"role": "assistant", "content": "", "reasoning_content": "嗯"}]),
                json!([]),
            ),
        );
        assert_eq!(updates.len(), 1);
        assert!(matches!(updates[0], SessionUpdate::AgentThoughtChunk(_)));

        // 只有 tool_calls（一段工具驱动的回合）。
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{"id": "tc1", "name": "Read", "arguments": {"path": "/tmp/a.rs"}}],
                }]),
                json!([]),
            ),
        );
        assert_eq!(updates.len(), 1);
        assert!(matches!(updates[0], SessionUpdate::ToolCall(_)));
    }

    // ---- tool_calls（创建卡片） ----

    #[test]
    fn assistant_tool_calls_create_in_progress_cards() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {"id": "tc1", "name": "Bash", "arguments": {"command": "ls -la\nsecond"}},
                        {"id": "tc2", "name": "Edit", "arguments": {"path": "src/main.rs"}},
                    ],
                }]),
                json!([]),
            ),
        );
        assert_eq!(updates.len(), 2, "两条 tool_calls 各一张卡片");

        match &updates[0] {
            SessionUpdate::ToolCall(call) => {
                assert_eq!(call.tool_call_id.to_string(), "tc1");
                assert_eq!(call.title, "ls -la", "Bash 标题 = 命令首行");
                assert_eq!(call.kind, ToolKind::Execute);
                assert_eq!(call.status, ToolCallStatus::InProgress);
                assert_eq!(call.raw_input, Some(json!({"command": "ls -la\nsecond"})));
                assert!(call.locations.is_empty());
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
        match &updates[1] {
            SessionUpdate::ToolCall(call) => {
                assert_eq!(call.tool_call_id.to_string(), "tc2");
                assert_eq!(call.title, "src/main.rs");
                assert_eq!(call.kind, ToolKind::Edit);
                assert_eq!(call.locations.len(), 1);
                assert_eq!(call.locations[0].path.to_string_lossy(), "src/main.rs");
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_without_arguments_omits_raw_input() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{"id": "tc1", "name": "SomeTool"}],
                }]),
                json!([]),
            ),
        );
        match &updates[0] {
            SessionUpdate::ToolCall(call) => {
                assert!(call.raw_input.is_none(), "参数缺席 → 不发 rawInput");
                assert_eq!(call.title, "SomeTool", "参数缺席 → 标题退回工具名");
                assert_eq!(call.kind, ToolKind::Other);
            }
            other => panic!("expected tool_call, got {other:?}"),
        }
    }

    #[test]
    fn tool_calls_with_empty_id_are_dropped() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{"id": "", "name": "Bash", "arguments": {}}],
                }]),
                json!([]),
            ),
        );
        assert!(updates.is_empty());
    }

    // ---- tool（结果收口） ----

    #[test]
    fn tool_message_closes_the_card_as_completed() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([
                    {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{"id": "tc1", "name": "Bash", "arguments": {"command": "ls"}}],
                    },
                    {"role": "tool", "content": "file.txt\n", "tool_call_id": "tc1"},
                ]),
                json!([]),
            ),
        );
        assert_eq!(updates.len(), 2);
        match &updates[1] {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.tool_call_id.to_string(), "tc1");
                assert_eq!(
                    update.fields.status,
                    Some(ToolCallStatus::Completed),
                    "历史投影没有失败标记：一律 completed"
                );
                assert_eq!(
                    update.fields.content,
                    Some(vec![ToolCallContent::from("file.txt\n".to_string())])
                );
                assert_eq!(update.fields.raw_output, Some(json!("file.txt\n")));
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn tool_message_without_anchor_is_skipped() {
        let mut cards = ToolCards::default();
        for message in [
            json!({"role": "tool", "content": "out"}),
            json!({"role": "tool", "content": "out", "tool_call_id": null}),
            json!({"role": "tool", "content": "out", "tool_call_id": ""}),
        ] {
            let updates = replay(&mut cards, &sync_event(json!([message]), json!([])));
            assert!(updates.is_empty(), "无 tool_call_id 的 tool 消息不产帧");
        }
    }

    #[test]
    fn tool_message_for_an_unknown_card_still_reports_status() {
        // 链被裁剪过（assistant 消息不在快照里）时仍要报状态：与实时
        // `tool_call_result` 的「未见过的卡片也发 update」同口径。
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{"role": "tool", "content": "boom", "tool_call_id": "ghost"}]),
                json!([]),
            ),
        );
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            SessionUpdate::ToolCallUpdate(update) => {
                assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
                assert_eq!(
                    update.fields.content,
                    Some(vec![ToolCallContent::from("boom".to_string())])
                );
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    // ---- 跳过分支 ----

    #[test]
    fn system_and_unknown_roles_are_skipped() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([
                    {"role": "system", "content": "system prompt"},
                    {"role": "", "content": "no role"},
                    {"role": "from_the_future", "content": "?"},
                ]),
                json!([]),
            ),
        );
        assert!(updates.is_empty());
    }

    #[test]
    fn malformed_history_records_are_skipped_but_neighbours_replay() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([
                    {"role": "assistant", "content": 5},
                    "not-an-object",
                    {"role": "user", "content": "还在"},
                ]),
                json!([]),
            ),
        );
        assert_eq!(updates.len(), 1, "坏记录跳过，好记录照常投影");
        assert!(matches!(updates[0], SessionUpdate::UserMessageChunk(_)));
    }

    // ---- events 组 ----

    #[test]
    fn events_group_projects_only_anchored_diff_content() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([
                    {
                        "role": "assistant",
                        "content": "改好了",
                        "tool_calls": [{"id": "tc1", "name": "Edit", "arguments": {"path": "a.rs"}}],
                    },
                    {"role": "tool", "content": "edited", "tool_call_id": "tc1"},
                ]),
                json!([
                    {"type": "text", "content": "重复的文本", "created_at": "c", "request_id": "r"},
                    {"type": "context_stats", "message_count": 2, "total_tokens": 10,
                     "context_window_tokens": 100, "created_at": "c", "request_id": "r"},
                    {"type": "diff_content", "path": "a.rs", "old_text": "old", "new_text": "new",
                     "tool_call_id": "tc1", "created_at": "c", "request_id": "r"},
                    {"type": "from_the_future", "created_at": "c", "request_id": "r"},
                ]),
            ),
        );
        // [assistant 文本, ToolCall 创建, ToolCallUpdate(completed), Diff 更新]
        assert_eq!(updates.len(), 4);
        match &updates[3] {
            SessionUpdate::ToolCallUpdate(update) => {
                let content = update.fields.content.as_ref().expect("diff 更新带 content");
                assert_eq!(content.len(), 2, "diff 在前、结果行在后（整表替换）");
                match &content[0] {
                    ToolCallContent::Diff(diff) => {
                        assert_eq!(diff.path.to_string_lossy(), "a.rs");
                        assert_eq!(diff.old_text.as_deref(), Some("old"));
                        assert_eq!(diff.new_text, "new");
                    }
                    other => panic!("expected diff, got {other:?}"),
                }
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    #[test]
    fn diff_without_an_anchor_is_dropped() {
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([]),
                json!([
                    {"type": "diff_content", "path": "a.rs", "new_text": "new",
                     "tool_call_id": "ghost", "created_at": "c", "request_id": "r"},
                    {"type": "diff_content", "path": "a.rs", "new_text": "new",
                     "tool_call_id": "", "created_at": "c", "request_id": "r"},
                ]),
            ),
        );
        assert!(updates.is_empty(), "无锚点的 diff 丢弃（不制造假卡片）");
    }

    #[test]
    fn diff_events_anchor_to_cards_created_by_replay() {
        // 回放建卡 → 后面的 events 组 diff 能锚定（同一份卡片记忆）。
        let mut cards = ToolCards::default();
        let updates = replay(
            &mut cards,
            &sync_event(
                json!([{
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{"id": "tc1", "name": "Write", "arguments": {"path": "new.rs"}}],
                }]),
                json!([
                    {"type": "diff_content", "path": "new.rs", "old_text": null, "new_text": "内容",
                     "tool_call_id": "tc1", "created_at": "c", "request_id": "r"},
                ]),
            ),
        );
        assert_eq!(updates.len(), 2);
        match &updates[1] {
            SessionUpdate::ToolCallUpdate(update) => {
                let content = update.fields.content.as_ref().expect("content");
                assert!(matches!(content[0], ToolCallContent::Diff(Diff { .. })));
            }
            other => panic!("expected tool_call_update, got {other:?}"),
        }
    }

    // ---- 半成品与入口守卫 ----

    #[test]
    fn uncommitted_materials_are_skipped() {
        let value = json!({
            "type": "sync_session",
            "session_id": "s1",
            "status": "working",
            "messages": [{"role": "user", "content": "hi"}],
            "uncommitted": {"role": "assistant", "content": "半截"},
            "uncommitted_tools": [{"tool_call_id": "tc9", "tool_name": "Bash", "args_fragment": "{\"comm"}],
            "events": [],
            "created_at": "c",
            "request_id": "r",
        });
        let event: WingEvent = serde_json::from_value(value).expect("fixture decodes");
        let mut cards = ToolCards::default();
        let updates = replay(&mut cards, &event);
        assert_eq!(updates.len(), 1, "只回放 messages/events");
        assert!(matches!(updates[0], SessionUpdate::UserMessageChunk(_)));
    }

    #[test]
    fn non_sync_session_events_produce_nothing() {
        let mut cards = ToolCards::default();
        let text: WingEvent = serde_json::from_value(json!({
            "type": "text",
            "content": "hi",
            "created_at": "c",
            "session_id": "s1",
            "request_id": "r",
        }))
        .expect("fixture decodes");
        assert!(replay(&mut cards, &text).is_empty());
    }

    // ---- 与实时映射共享卡片记忆 ----

    #[test]
    fn replayed_cards_keep_anchoring_live_events() {
        let mut cards = ToolCards::default();
        replay(
            &mut cards,
            &sync_event(
                json!([
                    {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": [{"id": "tc1", "name": "Edit", "arguments": {"path": "a.rs"}}],
                    },
                    {"role": "tool", "content": "edited", "tool_call_id": "tc1"},
                ]),
                json!([]),
            ),
        );

        // 同一 id 的实时 stream 不再重复创建卡片。
        let stream: WingEvent = serde_json::from_value(json!({
            "type": "tool_call_stream",
            "tool_call_id": "tc1",
            "tool_name": "Edit",
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }))
        .expect("fixture decodes");
        assert!(
            cards.apply(&stream).is_empty(),
            "回放建过的卡片在实时路径上是「已创建」"
        );

        // 实时 diff 仍能锚定到回放创建的卡片。
        let diff: WingEvent = serde_json::from_value(json!({
            "type": "diff_content",
            "path": "a.rs",
            "new_text": "again",
            "tool_call_id": "tc1",
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }))
        .expect("fixture decodes");
        assert_eq!(cards.apply(&diff).len(), 1);

        // 回放之后的新 id 仍是新卡片。
        let fresh: WingEvent = serde_json::from_value(json!({
            "type": "tool_call_stream",
            "tool_call_id": "tc2",
            "tool_name": "Bash",
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }))
        .expect("fixture decodes");
        assert_eq!(cards.apply(&fresh).len(), 1);
    }
}
