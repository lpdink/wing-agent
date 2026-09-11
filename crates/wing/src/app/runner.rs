//! Intent runner — executes side-effects produced by the App state machine.
//!
//! The App state machine declares *what* should happen by pushing `AppIntent`
//! variants. This module implements *how* each intent is executed, keeping
//! I/O concerns out of the core state machine.
//!
//! Fetch-type intents (read-only HTTP queries) are spawned as background
//! tasks to avoid blocking the main event loop. Results are sent back via
//! an mpsc channel as [`FetchResult`] variants.

use tokio::sync::mpsc;

use crate::app::intent::{AppIntent, FetchPayload, FetchResult};
use crate::app::transport::Transport;
use crate::tui::WingTerminal;
use crate::ui::toast::Toast;
use crate::util::title;

use super::App;

/// Execute a single intent, performing any necessary I/O.
///
/// `transport` is `None` when the gateway connection is lost; intents that
/// require the gateway are silently discarded in that case.
///
/// Fetch-type intents are spawned as background tasks; their results arrive
/// asynchronously via `fetch_tx`.
pub async fn execute_intent(
    app: &mut App,
    transport: &Option<Transport>,
    terminal: &mut WingTerminal,
    intent: AppIntent,
    fetch_tx: &mpsc::Sender<FetchResult>,
) {
    match intent {
        AppIntent::CopyToClipboard(text) => {
            let writer = terminal.backend_mut();
            match crate::util::clipboard::copy_to_clipboard(writer, &text) {
                Ok(()) => {
                    app.show_toast(Toast::info("Copied!", std::time::Duration::from_secs(2)));
                }
                Err(e) => {
                    tracing::warn!("clipboard copy failed: {e}");
                    app.show_toast(Toast::warning(
                        format!("Copy failed: {e}"),
                        std::time::Duration::from_secs(3),
                    ));
                }
            }
        }
        AppIntent::SendMessage {
            content,
            tool_call_id,
            request_id,
        } => {
            match transport {
                Some(t) => {
                    if let Err(e) = t
                        .ws
                        .send_message(&app.session_id, &content, tool_call_id, request_id.clone())
                        .await
                    {
                        tracing::error!("failed to send message: {e}");
                        // Never left the client — drop from the pending queue.
                        app.chat.remove_pending(&request_id);
                        app.show_toast(Toast::warning(
                            format!("Send failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
                None => {
                    // Gateway connection lost — the intent is discarded, so the
                    // pending entry must not linger on screen.
                    app.chat.remove_pending(&request_id);
                    app.show_toast(Toast::warning(
                        "Not sent — gateway disconnected".to_string(),
                        std::time::Duration::from_secs(3),
                    ));
                }
            }
        }
        AppIntent::FetchInfo => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http.get_session_info(&session_id).await {
                        Ok(info) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::Info(Box::new(info)),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get session info failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::FetchCommands => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    match http.get_commands().await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Commands(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get commands failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::FetchModels => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    match http.get_models().await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Models(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get models failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::FetchBranches => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http.get_branches(&session_id).await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::Branches(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get branches failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::FetchAgents => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    match http.get_agents().await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Agents(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get agents failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::UpdateSession {
            model,
            provider,
            agent,
            title,
            thinking,
            reasoning_effort,
            yolo,
            workspace,
        } => {
            if let Some(t) = transport {
                let req = wing_api_client::models::UpdateSessionRequest {
                    session_id: app.session_id.clone(),
                    model: model.clone(),
                    provider: provider.clone(),
                    agent: agent.clone(),
                    title: title.clone(),
                    thinking,
                    reasoning_effort: reasoning_effort.clone(),
                    yolo,
                    workspace: workspace.clone(),
                };
                match t.http.update_session(&req).await {
                    Ok(_) => apply_update_session(
                        app,
                        model,
                        provider,
                        agent,
                        title,
                        thinking,
                        reasoning_effort,
                        yolo,
                        workspace,
                    ),
                    Err(e) => {
                        app.chat
                            .push(crate::ui::chat_view::ChatCell::ErrorMessage(format!(
                                "Update failed: {e}"
                            )));
                    }
                }
            }
        }
        AppIntent::CreateSession { workspace } => {
            if let Some(t) = transport {
                let old_session_id = app.session_id.clone();
                let req = wing_api_client::models::CreateSessionRequest {
                    workspace: workspace.clone(),
                    ..Default::default()
                };
                match t.http.create_session(&req).await {
                    Ok(resp) => {
                        let new_sid = resp.session_id.clone();
                        t.switch_session(&old_session_id, &new_sid).await;
                        app.invalidate_session_cache();
                        tracing::info!(old = old_session_id, new = new_sid, "session created");
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Create failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ResumeSession {
            session_id: target_id,
        } => {
            if let Some(t) = transport {
                let old_session_id = app.session_id.clone();
                match t.http.resume_session(&target_id).await {
                    Ok(resp) => {
                        let new_sid = resp.session_id.clone();
                        t.switch_session(&old_session_id, &new_sid).await;
                        app.invalidate_session_cache();
                        tracing::info!(old = old_session_id, new = new_sid, "session resumed");
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Resume failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ForkSession { target_uuid } => {
            if let Some(t) = transport {
                let old_session_id = app.session_id.clone();
                match t.http.fork_session(&old_session_id, &target_uuid).await {
                    Ok(resp) => {
                        let new_sid = resp.session_id.clone();
                        t.switch_session(&old_session_id, &new_sid).await;
                        app.invalidate_session_cache();
                        tracing::info!(old = old_session_id, new = new_sid, "session forked");
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Fork failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::FetchSessionList => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    match http.list_sessions().await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::SessionList(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("list sessions failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::CompactSession { instruction } => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http
                        .compact_session(&session_id, instruction.as_deref())
                        .await
                    {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::CompactDone {
                                        original: resp.original_tokens,
                                        compressed: resp.compressed_tokens,
                                    },
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Toast {
                                        message: format!("Compact failed: {e}"),
                                        is_error: true,
                                    },
                                })
                                .await;
                        }
                    }
                });
            }
        }
        AppIntent::InterruptSession => {
            if let Some(t) = transport
                && let Err(e) = t.http.interrupt_session(&app.session_id).await
            {
                tracing::warn!("interrupt failed: {e}");
            }
        }
        AppIntent::RewindSession { target_uuid } => {
            if let Some(t) = transport {
                match t.http.rewind_session(&app.session_id, &target_uuid).await {
                    Ok(_) => {
                        app.show_toast(Toast::info(
                            "Session rewound",
                            std::time::Duration::from_secs(2),
                        ));
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Rewind failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ReloadSystem => {
            if let Some(t) = transport {
                match t.http.reload_system().await {
                    Ok(resp) => {
                        let status = if resp.ok { "✅" } else { "⚠️" };
                        let details: Vec<String> = resp
                            .results
                            .iter()
                            .map(|r| {
                                if r.ok {
                                    format!("✅ {}", r.name)
                                } else {
                                    format!(
                                        "❌ {}: {}",
                                        r.name,
                                        r.detail.as_deref().unwrap_or("unknown")
                                    )
                                }
                            })
                            .collect();
                        app.show_toast(Toast::info(
                            format!("{status} Reload: {}", details.join(", ")),
                            std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Reload failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ShowContextInfo => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http.get_session_info(&session_id).await {
                        Ok(info) => {
                            let mut text = format!(
                                "Messages: {}\nTokens: {} / {}\n",
                                info.context_stats.message_count,
                                info.context_stats.total_tokens,
                                info.context_window_tokens,
                            );
                            if !info.system_prompt.is_empty() {
                                text.push_str("\n--- System Prompt ---\n");
                                text.push_str(&info.system_prompt);
                            }
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::ContextInfo(text),
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Toast {
                                        message: format!("Context info failed: {e}"),
                                        is_error: true,
                                    },
                                })
                                .await;
                        }
                    }
                });
            }
        }
        AppIntent::ShowSkillsInfo => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http.get_session_info(&session_id).await {
                        Ok(info) => {
                            let text = if info.skills_info.is_empty() {
                                "No skills loaded.".to_string()
                            } else {
                                info.skills_info
                            };
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::SkillsInfo(text),
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Toast {
                                        message: format!("Skills info failed: {e}"),
                                        is_error: true,
                                    },
                                })
                                .await;
                        }
                    }
                });
            }
        }
        AppIntent::SetTitle(t) => {
            let writer = terminal.backend_mut();
            if let Err(e) = title::set_title(writer, &t) {
                tracing::warn!("failed to set terminal title: {e}");
            }
        }
        AppIntent::Notify(message) => {
            let writer = terminal.backend_mut();
            if let Err(e) = crate::util::osc9::send_notification(writer, &message) {
                tracing::warn!("failed to send OSC 9 notification: {e}");
            }
        }
        AppIntent::GoalSend {
            session_id,
            content,
            tool_call_id,
        } => {
            if let Some(t) = transport
                && let Err(e) = t
                    .http
                    .send_message(&session_id, &content, tool_call_id)
                    .await
            {
                tracing::error!("goal send failed: {e}");
                app.show_toast(Toast::warning(
                    format!("Goal send failed: {e}"),
                    std::time::Duration::from_secs(3),
                ));
            }
        }
        AppIntent::GoalUnsubscribe { session_id } => {
            if let Some(t) = transport
                && let Err(e) = t.http.unsubscribe(&session_id, &t.client_id).await
            {
                tracing::warn!("goal unsubscribe failed: {e}");
            }
        }
        AppIntent::GoalInterrupt { session_id } => {
            if let Some(t) = transport
                && let Err(e) = t.http.interrupt_session(&session_id).await
            {
                tracing::warn!("goal interrupt failed: {e}");
            }
        }
        AppIntent::GoalCreateChecker { system_prompt } => {
            if let Some(t) = transport {
                let req = wing_api_client::models::CreateSessionRequest {
                    workspace: None,
                    template_name: None,
                    backend: None,
                    agent: Some(wing_api_client::models::AgentOverride {
                        tools: Some(vec![
                            "Bash".into(),
                            "Read".into(),
                            "Glob".into(),
                            "Grep".into(),
                        ]),
                        system_prompt: Some(system_prompt),
                        ..Default::default()
                    }),
                };
                match t.http.create_session(&req).await {
                    Ok(resp) => {
                        let checker_id = resp.session_id.clone();
                        // Subscribe to checker session events. Without a successful
                        // subscribe no checker events would ever arrive, leaving the
                        // loop hung in CheckerWorking — so treat failure as fatal.
                        if let Err(e) = t.http.subscribe(&checker_id, &t.client_id).await {
                            tracing::warn!("subscribe checker failed: {e}");
                            app.show_toast(Toast::error(
                                format!("Checker created but subscribe failed: {e}"),
                                std::time::Duration::from_secs(4),
                            ));
                            if let Some(goal) = app.goal.as_mut() {
                                let actions = goal.on_checker_create_failed();
                                app.execute_goal_actions(actions);
                            }
                            return;
                        }
                        // Notify GoalState of successful creation.
                        if let Some(goal) = app.goal.as_mut() {
                            let actions = goal.on_checker_created(checker_id);
                            app.execute_goal_actions(actions);
                        }
                        tracing::info!("checker session created for goal mode");
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Checker creation failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                        if let Some(goal) = app.goal.as_mut() {
                            let actions = goal.on_checker_create_failed();
                            app.execute_goal_actions(actions);
                        }
                    }
                }
            }
        }
    }
}

/// Apply optimistic local status update after a successful UpdateSession HTTP call.
///
/// Updates `app.status` fields and shows a summary toast.
#[allow(clippy::too_many_arguments)]
fn apply_update_session(
    app: &mut App,
    model: Option<String>,
    provider: Option<String>,
    agent: Option<String>,
    title: Option<String>,
    thinking: Option<bool>,
    reasoning_effort: Option<String>,
    yolo: Option<bool>,
    workspace: Option<String>,
) {
    let on_off = |b: bool| if b { "on" } else { "off" };

    // Build toast parts from non-None fields.
    let parts: Vec<String> = [
        model.as_ref().map(|m| match provider.as_deref() {
            Some(p) => format!("Model: {m} ({p})"),
            None => format!("Model: {m}"),
        }),
        agent.as_ref().map(|a| format!("Agent: {a}")),
        title.as_ref().map(|t| format!("Title: {t}")),
        thinking.map(|t| format!("Think: {}", on_off(t))),
        reasoning_effort.as_ref().map(|e| format!("Effort: {e}")),
        yolo.map(|y| format!("YOLO: {}", on_off(y))),
        workspace.as_ref().map(|w| format!("Workdir: {w}")),
    ]
    .into_iter()
    .flatten()
    .collect();

    // Apply to local status.
    let affects_list = title.is_some() || workspace.is_some();
    app.status
        .apply_session_update(model, agent, title, thinking, reasoning_effort, yolo);
    if let Some(p) = provider {
        app.status.provider = Some(p);
    }

    if let Some(w) = workspace {
        app.status.workdir = Some(w);
    }

    // Title/workspace changes affect the session list display — invalidate cache.
    if affects_list {
        app.invalidate_session_cache();
    }

    if !parts.is_empty() {
        app.show_toast(Toast::info(
            parts.join(" · "),
            std::time::Duration::from_secs(3),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::ui::popup::command::SessionCandidate;

    fn app_with_cached_sessions() -> App {
        let mut app = App::new("test-session".into(), AppConfig::default(), None);
        app.popup.cache.sessions = vec![SessionCandidate {
            id: "s1".into(),
            title: "Old Title".into(),
            workspace: "/old".into(),
            status: "idle".into(),
            last_interaction: "2025-01-01T00:00:00Z".into(),
        }];
        app
    }

    #[test]
    fn test_update_session_invalidates_cache_on_title() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            None,
            None,
            None,
            Some("New Title".into()),
            None,
            None,
            None,
            None,
        );
        assert!(app.popup.cache.sessions.is_empty());
    }

    #[test]
    fn test_update_session_invalidates_cache_on_workspace() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("/new/dir".into()),
        );
        assert!(app.popup.cache.sessions.is_empty());
    }

    #[test]
    fn test_update_session_preserves_cache_on_model_only() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            Some("gpt-4o".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!app.popup.cache.sessions.is_empty());
    }

    #[test]
    fn test_update_session_syncs_provider() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            Some("gpt-4o".into()),
            Some("alt".into()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(app.status.model, "gpt-4o");
        assert_eq!(app.status.provider.as_deref(), Some("alt"));
    }

    #[test]
    fn test_update_session_preserves_cache_on_thinking_only() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            None,
            None,
            None,
            None,
            Some(true),
            Some("high".into()),
            None,
            None,
        );
        assert!(!app.popup.cache.sessions.is_empty());
    }
}
