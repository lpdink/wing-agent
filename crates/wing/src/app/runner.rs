//! Intent runner — executes side-effects produced by the App state machine.
//!
//! The App state machine declares *what* should happen by pushing `AppIntent`
//! variants. This module implements *how* each intent is executed, keeping
//! I/O concerns out of the core state machine.

use crate::app::intent::AppIntent;
use crate::app::transport::Transport;
use crate::tui::WingTerminal;
use crate::ui::toast::Toast;
use crate::util::title;

use super::App;

/// Execute a single intent, performing any necessary I/O.
///
/// `transport` is `None` when the gateway connection is lost; intents that
/// require the gateway are silently discarded in that case.
pub async fn execute_intent(
    app: &mut App,
    transport: &Option<Transport>,
    terminal: &mut WingTerminal,
    intent: AppIntent,
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
        AppIntent::SendMessage { content } => {
            if let Some(t) = transport
                && let Err(e) = t.ws.send_message(&app.session_id, &content).await
            {
                tracing::error!("failed to send message: {e}");
                app.show_toast(Toast::warning(
                    format!("Send failed: {e}"),
                    std::time::Duration::from_secs(3),
                ));
            }
        }
        AppIntent::FetchInfo => {
            if let Some(t) = transport {
                match t.http.get_session_info(&app.session_id).await {
                    Ok(info) => {
                        app.status.model = info.model;
                        app.status.total_tokens = info.total_tokens;
                        app.status.context_window_tokens = info.context_window_tokens;
                        app.status.thinking = info.thinking;
                        app.status.reasoning_effort = info.reasoning_effort;
                        app.status.yolo = info.yolo;
                        app.status.session_name = info.session_name;
                        tracing::info!(model = %app.status.model, "session info received");
                    }
                    Err(e) => {
                        tracing::warn!("get session info failed: {e}");
                    }
                }
            }
        }
        AppIntent::FetchCommands => {
            if let Some(t) = transport {
                match t.http.get_commands().await {
                    Ok(resp) => {
                        app.popup.cache.commands = resp
                            .commands
                            .into_iter()
                            .map(|c| crate::protocol::CommandInfo {
                                name: c.name,
                                aliases: c.aliases,
                                description: c.description,
                                params: c.params,
                            })
                            .collect();
                        app.update_popup();
                    }
                    Err(e) => {
                        tracing::warn!("get commands failed: {e}");
                    }
                }
            }
        }
        AppIntent::FetchModels => {
            if let Some(t) = transport {
                match t.http.get_models().await {
                    Ok(resp) => {
                        app.popup.cache.models = resp
                            .models
                            .into_iter()
                            .map(|m| (m, String::new()))
                            .collect();
                        app.update_popup();
                    }
                    Err(e) => {
                        tracing::warn!("get models failed: {e}");
                    }
                }
            }
        }
        AppIntent::FetchBranches => {
            if let Some(t) = transport {
                match t.http.get_branches(&app.session_id).await {
                    Ok(resp) => {
                        app.popup.cache.branches = resp
                            .targets
                            .into_iter()
                            .map(|t| {
                                let preview =
                                    crate::ui::cells::tool_call::truncate_by_chars(&t.content, 80);
                                (t.uuid, preview)
                            })
                            .collect();
                        app.update_popup();
                    }
                    Err(e) => {
                        tracing::warn!("get branches failed: {e}");
                    }
                }
            }
        }
        AppIntent::FetchAgents => {
            if let Some(t) = transport {
                match t.http.get_agents().await {
                    Ok(resp) => {
                        app.popup.cache.agents = resp
                            .agents
                            .into_iter()
                            .map(|a| (a, String::new()))
                            .collect();
                        app.update_popup();
                    }
                    Err(e) => {
                        tracing::warn!("get agents failed: {e}");
                    }
                }
            }
        }
        AppIntent::UpdateSession {
            model,
            agent,
            title,
            thinking,
            reasoning_effort,
            yolo,
        } => {
            if let Some(t) = transport {
                let req = wing_api_client::models::UpdateSessionRequest {
                    session_id: app.session_id.clone(),
                    model: model.clone(),
                    agent: agent.clone(),
                    title: title.clone(),
                    thinking,
                    reasoning_effort: reasoning_effort.clone(),
                    yolo,
                };
                match t.http.update_session(&req).await {
                    Ok(_) => apply_update_session(
                        app,
                        model,
                        agent,
                        title,
                        thinking,
                        reasoning_effort,
                        yolo,
                    ),
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Update failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
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
                match t.http.list_sessions().await {
                    Ok(resp) => {
                        app.popup.cache.sessions = resp
                            .sessions
                            .iter()
                            .map(|s| (s.id.clone(), s.name.clone().unwrap_or_default()))
                            .collect();
                        app.update_popup();
                    }
                    Err(e) => {
                        tracing::warn!("list sessions failed: {e}");
                    }
                }
            }
        }
        AppIntent::CompactSession => {
            if let Some(t) = transport {
                match t.http.compact_session(&app.session_id).await {
                    Ok(resp) => {
                        app.show_toast(Toast::info(
                            format!(
                                "Compact done: {} → {} tokens",
                                resp.original_tokens, resp.compressed_tokens
                            ),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Compact failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
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
                match t.http.get_session_info(&app.session_id).await {
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
                        app.chat
                            .push(crate::ui::chat_view::ChatCell::SystemMessage(text));
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Context info failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ShowSkillsInfo => {
            if let Some(t) = transport {
                match t.http.get_session_info(&app.session_id).await {
                    Ok(info) => {
                        let text = if info.skills_info.is_empty() {
                            "No skills loaded.".to_string()
                        } else {
                            info.skills_info
                        };
                        app.chat
                            .push(crate::ui::chat_view::ChatCell::SystemMessage(text));
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Skills info failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
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
    }
}

/// Apply optimistic local status update after a successful UpdateSession HTTP call.
///
/// Updates `app.status` fields and shows a summary toast.
fn apply_update_session(
    app: &mut App,
    model: Option<String>,
    agent: Option<String>,
    title: Option<String>,
    thinking: Option<bool>,
    reasoning_effort: Option<String>,
    yolo: Option<bool>,
) {
    let on_off = |b: bool| if b { "on" } else { "off" };

    // Build toast parts from non-None fields.
    let parts: Vec<String> = [
        model.as_ref().map(|m| format!("Model: {m}")),
        agent.as_ref().map(|a| format!("Agent: {a}")),
        title.as_ref().map(|t| format!("Title: {t}")),
        thinking.map(|t| format!("Think: {}", on_off(t))),
        reasoning_effort.as_ref().map(|e| format!("Effort: {e}")),
        yolo.map(|y| format!("YOLO: {}", on_off(y))),
    ]
    .into_iter()
    .flatten()
    .collect();

    // Apply to local status.
    app.status
        .apply_session_update(model, agent, title, thinking, reasoning_effort, yolo);

    if !parts.is_empty() {
        app.show_toast(Toast::info(
            parts.join(" · "),
            std::time::Duration::from_secs(3),
        ));
    }
}
