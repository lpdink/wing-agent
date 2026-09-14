//! Gateway event projection lane — `WingEvent` in, chat / turn / status out.
//!
//! Every event the gateway pushes is projected into UI state here and nowhere
//! else: this module is the only place that matches on event variants. The
//! writes are strictly "state in, state out" — chat cells, turn bookkeeping,
//! status data, modal registration. Side effects (title changes, notifications,
//! interruptions) leave through `AppIntent`s, so a projection never performs
//! I/O itself.
//!
//! The session-scoped guards (drop events from other sessions, always accept
//! `SyncSession`, accept the goal checker session) live here too: they decide
//! **what the projection is allowed to see**, which is the projection's own
//! contract.

use super::App;
use super::AppIntent;
use super::constants::TOOL_BASH;
use super::constants::TOOL_TODO;
use super::replay;
use super::title;
use super::turn_state;
use crate::protocol::AgentInfo;
use crate::protocol::EventMeta;
use crate::protocol::WingEvent;
use crate::ui::cells::ask_msg::AskMessage;
use crate::ui::cells::diff_view::DiffView;
use crate::ui::cells::todo_msg::TodoMessage;
use crate::ui::cells::tool_call::ToolCallBlock;
use crate::ui::cells::tool_call::ToolStatus;
use crate::ui::chat_view::ChatCell;
use crate::ui::status_bar::TurnUsage;
use crate::ui::toast::Toast;
use crate::util::title::AttentionKind;

impl App {
    /// Handle a gateway event.
    pub(super) fn handle_event(&mut self, event: WingEvent) {
        // Defense-in-depth: skip events from other sessions.
        // Protects against backend events emitted without scope="session".
        //
        // Session lifecycle events (SyncSession) are always accepted —
        // their session_id may intentionally differ from the current one.
        //
        // Goal mode: also accept events from the checker session.
        if !matches!(event, WingEvent::SyncSession { .. })
            && let Some(event_sid) = event.session_id()
            && event_sid != self.session_id
            && !self.is_goal_session(event_sid)
        {
            tracing::debug!(
                event_type = %event.event_type(),
                event_session = event_sid,
                current_session = %self.session_id,
                "skipping event from different session"
            );
            return;
        }

        tracing::debug!(event_type = %event.event_type(), "handling event");

        match event {
            // ---- Lifecycle events ----
            WingEvent::Delivered { .. } => {
                // Transport-level ack — does NOT enter working state.
                // Working is triggered by TurnStarted (agent-level event).
                tracing::debug!("message delivered");
            }
            WingEvent::TurnStarted { .. } => {
                // Agent begins processing a user message.
                // Guard: only initialize timer if not already working
                // (Ask answers may trigger a second turn within the same logical flow).
                tracing::debug!("turn started, was_working={}", self.turn.working);
                self.turn.start();
                self.turn.last_result = None;
                self.chat.reset_thinking_count();
                // Set title to working state with initial spinner frame.
                let working_title = title::title_working(
                    self.turn.spinner.frame_str(),
                    self.dir_label().as_deref(),
                );
                self.turn.last_title = Some(working_title.clone());
                self.push_intent(AppIntent::SetTitle(working_title));
            }
            WingEvent::UserMessageAccepted {
                origin_request_id, ..
            } => {
                // The message has actually been fed to the model — only now
                // may it move up into chat history. Untracked ids (goal
                // orchestration sends, other clients) are ignored: this
                // client does not render them.
                if !self.chat.promote_pending(&origin_request_id) {
                    tracing::debug!(
                        origin_request_id,
                        "user_message_accepted for untracked message, ignoring"
                    );
                }
            }
            WingEvent::Done { .. } => {
                // Turn-end safety net: promote anything still queued (e.g.
                // accepted events lost across a disconnect/reconnect).
                self.chat.promote_all_pending();
                self.finish_turn();
                self.clear_ask_state();
                // Restore idle title — but if the user is not focused and a
                // TurnResult just set an attention title (✓/⚠), preserve it
                // until the user refocuses (Focus handler restores idle).
                let had_result = self.turn.last_result.take().is_some();
                if self.focused || !had_result {
                    self.push_intent(AppIntent::SetTitle(title::title_idle(
                        self.dir_label().as_deref(),
                    )));
                }
            }
            WingEvent::Interrupted { meta, .. } => {
                // Interrupt clears the backend inbox — pending messages never
                // reached the model. Commit them as discarded (dim + struck
                // through) rather than silently vanishing.
                self.chat.discard_all_pending();
                self.finish_turn();
                self.clear_ask_state();
                // Goal mode: record which role was interrupted.
                let goal_role = self.goal_role_for_session(meta.session_id.as_deref());
                if let Some(goal) = self.goal.as_mut()
                    && let Some(role) = goal_role
                {
                    let actions = goal.on_interrupted(role);
                    self.execute_goal_actions(actions);
                }
                self.show_toast(Toast::info(
                    "Agent interrupted",
                    std::time::Duration::from_secs(2),
                ));
                // Restore idle title (no BEL — user triggered this).
                self.push_intent(AppIntent::SetTitle(title::title_idle(
                    self.dir_label().as_deref(),
                )));
            }
            WingEvent::Notice {
                level,
                message,
                attempt,
                max_attempts,
                retry_in_s,
                ..
            } => {
                // Informational only. Deliberately does NOT call finish_turn():
                // retrying a failed LLM call means the turn is still running —
                // treating it as an error used to fake an end-of-turn in the UI
                // while the backend kept working.
                let text = if message.is_empty() {
                    "notice".to_string()
                } else {
                    message
                };
                let text = match (attempt, max_attempts, retry_in_s) {
                    (Some(a), Some(max), Some(delay)) => {
                        format!("{text} (attempt {a}/{max}, retrying in {delay:.0}s)")
                    }
                    _ => text,
                };
                if level.eq_ignore_ascii_case("warning") || level.eq_ignore_ascii_case("error") {
                    self.chat.push(ChatCell::WarningMessage(text));
                } else {
                    // `info` (or a level from a newer gateway we don't know):
                    // degrade to the plain system style rather than guessing.
                    self.chat.push(ChatCell::SystemMessage(text));
                }
            }

            WingEvent::Error { message, .. } => {
                self.finish_turn();
                self.chat.push(ChatCell::ErrorMessage(message.clone()));
                self.notify_unfocused(message.clone(), AttentionKind::Error);
            }

            // ---- Reasoning events ----
            WingEvent::Reasoning { content, .. } => {
                self.chat.append_to_last_thinking(&content);
                self.ctx.current_thinking = Some(self.chat.len().saturating_sub(1));
                // Increment thinking event count for hidden mode indicator.
                self.chat.increment_thinking_count();
            }

            // ---- Text events ----
            WingEvent::Text { content, .. } => {
                self.ctx.current_thinking = None;
                self.chat.append_to_last_assistant(&content);
                self.ctx.current_assistant = Some(self.chat.len().saturating_sub(1));
                self.ctx.last_usage_target = self.ctx.current_assistant;
            }

            // ---- Tool events ----
            WingEvent::ToolCallStream {
                tool_call_id,
                tool_name,
                args_fragment,
                is_final,
                ..
            } => {
                self.ctx.current_thinking = None;
                self.ctx.current_assistant = None;

                if let Some(idx) = self.chat.tool_call_index(&tool_call_id) {
                    // Update existing streaming cell
                    self.chat
                        .append_tool_args_fragment_by_index(idx, &args_fragment);
                    if is_final {
                        self.chat.set_tool_status_by_index(idx, ToolStatus::Pending);
                    }
                } else {
                    // Create new streaming cell
                    let mut block = ToolCallBlock::new_streaming(tool_name, tool_call_id.clone());
                    block.append_args_fragment(&args_fragment);
                    if is_final {
                        block.status = ToolStatus::Pending;
                    }
                    self.chat.push(ChatCell::ToolCall(block));
                }
            }
            WingEvent::ToolCall {
                tool_name,
                tool_args,
                tool_call_id,
                ..
            } => {
                self.ctx.current_thinking = None;
                self.ctx.current_assistant = None;

                // If a streaming cell already exists for this id, update it
                if let Some(idx) = self.chat.tool_call_index(&tool_call_id) {
                    self.chat.update_tool_args_by_index(idx, tool_args);
                    self.chat.set_tool_status_by_index(idx, ToolStatus::Pending);
                    // Start timer for Bash tools on execution start.
                    if tool_name == TOOL_BASH {
                        self.chat.set_tool_started_at_by_index(idx);
                    }
                } else {
                    let mut block =
                        ToolCallBlock::new(tool_name.clone(), tool_args, tool_call_id.clone());
                    // Start timer for Bash tools.
                    if tool_name == TOOL_BASH {
                        block.started_at = Some(std::time::Instant::now());
                    }
                    self.chat.push(ChatCell::ToolCall(block));
                }

                tracing::debug!(tool = %tool_name, "tool call started");
            }
            WingEvent::ToolCallResult {
                tool_name,
                tool_args,
                tool_call_id,
                tool_result,
                tool_success,
                ..
            } => {
                self.handle_tool_result(
                    tool_name,
                    tool_args,
                    tool_call_id,
                    tool_result,
                    tool_success,
                );
            }

            // ---- Diff events ----
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
                // Anchor the diff directly after the ToolCall cell that
                // produced it — concurrent edits complete out of order, so
                // appending would interleave diffs arbitrarily. Unknown id
                // (older gateway / lost ToolCall event) falls back to append.
                if let Err(cell) = self
                    .chat
                    .insert_after_tool_call(&tool_call_id, ChatCell::Diff(diff))
                {
                    self.chat.push(*cell);
                }
            }

            // ---- Metrics events ----
            WingEvent::LlmCallMetrics {
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                tokens_per_sec,
                first_chunk_rt_ms,
                ..
            } => {
                self.status.session_prompt_tokens += prompt_tokens;
                self.status.session_completion_tokens += completion_tokens;
                self.status.session_cached_tokens += cached_tokens;

                self.turn.usage = TurnUsage {
                    prompt_tokens,
                    completion_tokens,
                    cached_tokens,
                    tokens_per_sec,
                    ttft_ms: first_chunk_rt_ms,
                };

                tracing::debug!(
                    prompt_tokens,
                    completion_tokens,
                    cached_tokens,
                    tokens_per_sec,
                    first_chunk_rt_ms,
                    "LLM metrics"
                );
            }

            // ---- Ask events ----
            WingEvent::Ask {
                tool_call_id,
                questions,
                question,
                choices,
                required,
                ..
            } => {
                if !questions.is_empty() {
                    // Multi-question panel (AskUserQuestion tool).
                    let panel =
                        super::ask_panel::AskPanel::new(tool_call_id.clone(), questions.clone());
                    let msg = AskMessage::new_panel(tool_call_id.clone(), panel.clone());
                    self.chat.push(ChatCell::Ask(msg));
                    self.ask_panels.push_back(panel);
                    self.refresh_ask_placeholder();
                    let notify_text = questions
                        .first()
                        .map(|q| q.question.clone())
                        .unwrap_or_default();
                    self.notify_unfocused(notify_text, AttentionKind::Ask);
                } else {
                    // Legacy single-question (Bash dangerous command confirmation).
                    let msg = AskMessage::new_legacy(
                        tool_call_id.clone(),
                        question.clone(),
                        choices.clone(),
                    );
                    self.chat.push(ChatCell::Ask(msg));
                    if required && !choices.is_empty() {
                        let sel =
                            crate::ui::ask_select::AskSelection::new(tool_call_id.clone(), choices);
                        self.ask_selections.push_back(sel);
                        self.refresh_ask_placeholder();
                        if let Some(front) = self.ask_selections.front()
                            && front.tool_call_id == tool_call_id
                        {
                            self.chat.update_ask_selection(&tool_call_id, 0);
                        }
                    }
                    self.notify_unfocused(question.clone(), AttentionKind::Ask);
                }
                // Bring the ask into view so the user sees it immediately and
                // understands why Up/Down now navigate the selection menu.
                self.chat.jump_bottom();
            }

            // ---- Candidate list events (for popup) ----
            // Note: CommandList, ModelList, AgentList, SessionList removed — now via HTTP.
            // BranchTargets is still emitted by /rewind <uuid> after execution.
            WingEvent::BranchTargets { targets, .. } => {
                self.popup.cache.branches = targets
                    .iter()
                    .map(|t| {
                        let preview =
                            crate::ui::cells::tool_call::truncate_by_chars(&t.content, 80);
                        (t.uuid.clone(), preview)
                    })
                    .collect();
                self.update_popup();
            }

            // ---- State events ----
            WingEvent::ContextStats {
                total_tokens,
                context_window_tokens,
                message_count,
                ..
            } => {
                self.status.total_tokens = total_tokens;
                if context_window_tokens > 0 {
                    self.status.context_window_tokens = context_window_tokens;
                }
                tracing::debug!(
                    total_tokens,
                    context_window_tokens,
                    message_count,
                    "context stats"
                );
            }
            WingEvent::SessionStateChanged {
                model,
                thinking,
                reasoning_effort,
                yolo,
                title,
                agent,
                ..
            } => {
                self.status.apply_session_update(
                    model,
                    agent,
                    title,
                    thinking,
                    reasoning_effort,
                    yolo,
                );
            }
            WingEvent::SyncSession {
                session_id,
                messages,
                uncommitted,
                uncommitted_tools,
                events,
                turn_started_at,
                draft,
                name,
                agent,
                ..
            } => {
                self.apply_sync_session(
                    session_id,
                    &messages,
                    uncommitted.as_ref(),
                    &uncommitted_tools,
                    &events,
                    turn_started_at.as_deref(),
                    draft,
                    name,
                    agent,
                );
            }
            // ---- Turn result (rich completion data) ----
            WingEvent::TurnResult {
                subtype,
                is_error,
                duration_ms,
                num_turns,
                result,
                usage,
                meta,
                ..
            } => {
                // Extract total tokens from usage JSON.
                // Note: cached_tokens is a subset of input_tokens (cache hit),
                // so total = input + output (not input + output + cached).
                let total_tokens = usage.as_ref().map(|u| {
                    let get = |key| u.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
                    get("input_tokens") + get("output_tokens")
                });

                // Store turn result for Done handler consumption.
                self.turn.last_result = Some(turn_state::TurnResultSummary {
                    subtype: subtype.clone(),
                    is_error,
                    duration_ms,
                    num_turns,
                    result: result.clone(),
                    total_tokens,
                });
                tracing::debug!(
                    subtype = %subtype,
                    is_error,
                    duration_ms,
                    num_turns,
                    "turn result received"
                );

                // Goal mode: drive the orchestration loop.
                let goal_role = self.goal_role_for_session(meta.session_id.as_deref());
                if let Some(goal) = self.goal.as_mut()
                    && let Some(role) = goal_role
                {
                    let actions = goal.on_turn_result(role, result.clone());
                    self.execute_goal_actions(actions);
                }

                // Notify user if terminal is not focused.
                let msg = crate::util::osc9::fmt_turn_result(
                    result.as_deref(),
                    num_turns,
                    duration_ms,
                    total_tokens,
                );
                let kind = if is_error {
                    AttentionKind::Error
                } else {
                    AttentionKind::Done
                };
                self.notify_unfocused(msg, kind);
            }
            _ => {
                tracing::debug!(
                    event_type = %event.event_type(),
                    "unhandled event"
                );
            }
        }
    }

    /// Replace the whole view with a session snapshot (`SyncSession`).
    ///
    /// A full state replacement, not an append: clear → banner → working state
    /// → replay in the chain order that makes diff anchoring structural
    /// (messages → uncommitted → uncommitted tools → fact events) → draft /
    /// name / agent snapshot.
    #[allow(clippy::too_many_arguments)]
    fn apply_sync_session(
        &mut self,
        session_id: String,
        messages: &[serde_json::Value],
        uncommitted: Option<&serde_json::Value>,
        uncommitted_tools: &[serde_json::Value],
        events: &[serde_json::Value],
        turn_started_at: Option<&str>,
        draft: Option<String>,
        name: Option<String>,
        agent: Option<Box<AgentInfo>>,
    ) {
        // Goal mode: ignore SyncSession from checker session.
        // We subscribe to checker for events only — it must NOT
        // replace the current session or clear the chat.
        if self.goal.is_some() && self.is_goal_session(&session_id) {
            tracing::debug!(session_id, "ignoring checker SyncSession in goal mode");
            return;
        }

        // Update session_id to the new session (Phase 3c).
        self.session_id = session_id;

        // Clear old content + reset render context before replay.
        // SyncSession is a full state replacement — not an append.
        self.chat.clear();
        self.ctx.reset();

        // Loaded skills/rules banner: one summary line pinned at the
        // top of the chat, mirrored on every sync (connect / resume /
        // switch / fork) — counts only; details stay behind /skills
        // so the sync payload carries lists, not rendered blobs.
        if let Some(agent_info) = &agent {
            let line = format!(
                "loaded {} skills, {} rules · /skills for details",
                agent_info.skills.len(),
                agent_info.rules.len()
            );
            self.chat.push(ChatCell::SystemMessage(line));
        }

        // Ask flows are session-scoped interactive state, not rendered
        // history: without this, a reconnect / session switch while an
        // ask is pending would leave the live-registered flow in the
        // deque AND re-register it from the replayed pending ask —
        // the duplicate stale front entry would later swallow the
        // answer of a subsequent ask (posted to a dead tool_call_id).
        self.clear_ask_state();
        // The model panel targets the previous session (preselect +
        // apply go to its session_id) — a sync means the session
        // changed, so the panel must not survive it.
        self.model_panel = None;

        // Restore working state BEFORE feeding uncommitted content, so
        // the spinner / Bash timers / terminal title reflect an
        // in-progress turn (a mid-turn resume). `turn_started_at`
        // restores the real elapsed instead of recounting from resume.
        let turn_instant = turn_started_at.and_then(turn_state::instant_from_utc_iso);
        let mid_turn = uncommitted.is_some() || !uncommitted_tools.is_empty();
        if mid_turn {
            self.turn.start();
            if let Some(instant) = turn_instant {
                self.turn.started_at = Some(instant);
            }
            let working_title =
                title::title_working(self.turn.spinner.frame_str(), self.dir_label().as_deref());
            self.turn.last_title = Some(working_title.clone());
            self.push_intent(AppIntent::SetTitle(working_title));
        }

        // Replay order: messages → uncommitted → uncommitted_tools →
        // events. This makes diff anchoring structural: the tool_use
        // block that produced a diff is a *finalized* block, so it is
        // built as a ToolCall cell by step 1/2 before step 4 applies
        // the diff — the anchor always exists first.
        //
        // 1. Committed Message projections (text/thinking/ToolCall cells).
        replay::replay_messages(&mut self.chat, messages);
        // 2. Uncommitted assistant Message projection — SAME replay path
        //    (a single Message payload, not a list). Never fed to
        //    handle_event as a pseudo-event.
        if let Some(uncommitted_msg) = uncommitted {
            replay::replay_messages(&mut self.chat, std::slice::from_ref(uncommitted_msg));
            // A mid-execution Bash card (finalized tool_use, no result
            // yet) shows elapsed anchored to the turn start.
            if let Some(instant) = turn_instant {
                self.chat.mark_pending_bash_running(instant);
            }
        }
        // 3. Unfinished tool calls' raw args fragments — through the
        //    EXISTING live ToolCallStream branch (client-side partial
        //    parse via partial_json.rs); zero new rendering logic. The
        //    subsequent live tool_call_stream deltas append seamlessly.
        for tool in uncommitted_tools {
            let tool_call_id = tool
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if tool_call_id.is_empty() {
                continue;
            }
            let tool_name = tool
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let args_fragment = tool
                .get("args_fragment")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            self.handle_event(WingEvent::ToolCallStream {
                tool_call_id: tool_call_id.to_string(),
                tool_name: tool_name.to_string(),
                args_fragment: args_fragment.to_string(),
                is_final: false,
                // session_id None → bypasses the cross-session filter
                // (this is the session we just switched to).
                meta: EventMeta {
                    created_at: String::new(),
                    session_id: None,
                    request_id: String::new(),
                },
            });
        }
        // 4. Durable fact events (diff anchored onto cells above; ask
        //    rendered as an answerable card). replay_events builds the
        //    ask cells; register their reply state so a resumed pending
        //    ask is answerable through the same channel as live.
        for ask in replay::replay_events(&mut self.chat, events) {
            self.register_ask_panel(
                &ask.tool_call_id,
                &ask.questions,
                &ask.choices,
                ask.required,
            );
        }

        tracing::info!(
            message_count = messages.len(),
            has_uncommitted = uncommitted.is_some(),
            uncommitted_tools_count = uncommitted_tools.len(),
            event_count = events.len(),
            mid_turn,
            has_draft = draft.is_some(),
            "session replayed"
        );

        // Restore draft to input box if present.
        if let Some(draft_text) = draft {
            self.input.set_text(&draft_text);
        }

        // Update session name if provided.
        if let Some(session_name) = name {
            self.status.session_name = Some(session_name);
        }

        // Restore model + provider + workdir from agent snapshot.
        if let Some(agent_info) = agent {
            self.status.model = agent_info.model_name;
            self.status.provider = agent_info.provider_name;
            self.status.workdir = agent_info.workspace;
        }

        self.refresh_copy_candidates();
    }

    /// Handle tool call results with tool-specific routing.
    ///
    /// Rendering decisions (hidden, full, truncated) are delegated to
    /// `ToolCallBlock::to_lines()` via the `ToolRenderer` strategy pattern.
    pub(super) fn handle_tool_result(
        &mut self,
        tool_name: String,
        tool_args: serde_json::Value,
        tool_call_id: String,
        tool_result: String,
        tool_success: bool,
    ) {
        // Special handling for TodoWrite — render as TodoMessage cell
        // anchored directly after its ToolCall cell. Concurrent tool
        // execution makes results arrive out of order, so appending would
        // misplace the todo list below unrelated cells.
        if tool_name == TOOL_TODO
            && tool_success
            && let Some(todo) = TodoMessage::from_tool_args(&tool_args)
        {
            let todo_cell = ChatCell::Todo(todo);
            if let Some(idx) = self.chat.tool_call_index(&tool_call_id) {
                // Mark the ToolCall cell Success (mirrors replay behavior;
                // the early return previously left it Pending forever).
                self.chat
                    .set_tool_result_by_index(idx, tool_result, tool_success);
                // Cannot fail — the ToolCall cell was just located above.
                let _ = self.chat.insert_after_tool_call(&tool_call_id, todo_cell);
            } else {
                // Orphan result (lost ToolCall event) — mirror the unified
                // path below: show the ToolCall block, then the todo list.
                let mut block = ToolCallBlock::new(tool_name, tool_args, tool_call_id);
                block.set_result(tool_result, tool_success);
                self.chat.push(ChatCell::ToolCall(block));
                self.chat.push(todo_cell);
            }
            tracing::debug!("todo updated");
            return;
        }

        // Unified path: set result on existing ToolCallBlock, or create orphan.
        if let Some(idx) = self.chat.tool_call_index(&tool_call_id) {
            self.chat
                .set_tool_result_by_index(idx, tool_result, tool_success);
        } else {
            let mut block = ToolCallBlock::new(tool_name, tool_args, tool_call_id);
            block.set_result(tool_result, tool_success);
            self.chat.push(ChatCell::ToolCall(block));
        }
    }
}
