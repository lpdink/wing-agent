//! Slash-command routing lane.
//!
//! One place to read the whole path of a TUI-intercepted command:
//!
//! 1. **Route** — [`COMMANDS`] is the table: name (plus aliases), whether the
//!    command takes an argument tail, and the handler.
//! 2. **Parse** — the handler turns the raw line into an effect: a local
//!    mutation (chat, popup cache, toasts) or an [`AppIntent`] for the runner.
//! 3. **Project the answer** — asynchronous answers (HTTP fetches, session
//!    info, model lists) come back through [`App::handle_fetch_result`], which
//!    lives here as well: "command → request → result" is one path.
//!
//! Only `impl App` methods and the command handlers themselves live in this
//! module; the `App` state stays in the composition root ([`super`]).
//!
//! Call directions: [`super`] (main loop / fetch results) and [`super::modal`]
//! (the composer's submit path, popup refresh) call in; this module calls
//! [`super::modal`] for the `/model` picker and [`super::goal_lane`] for the
//! `/goal` commands.

use super::App;
use super::AppIntent;
use super::constants::CLEAR_COMMAND;
use super::constants::COPY_COMMAND;
use super::constants::GOAL_COMMAND;
use super::constants::GOAL_EXIT_COMMAND;
use super::constants::NEW_COMMAND;
use super::goal_lane;
use super::model_panel::ModelPanel;
use super::title;
use crate::ui::cells::tool_call::truncate_by_chars;
use crate::ui::chat_view::ChatCell;
use crate::ui::popup::command::PopupAction;
use crate::ui::status_bar::TurnUsage;
use crate::ui::toast::Toast;

/// Classified result of parsing a boolean-style argument.
#[derive(Debug)]
enum BoolArg {
    /// No argument provided (empty string).
    Empty,
    /// `"on"`, `"true"`, or `"1"`.
    On,
    /// `"off"`, `"false"`, or `"0"`.
    Off,
    /// Any other value (already lowercased).
    Other(String),
}

/// Classify a boolean toggle argument.
fn parse_bool_arg(raw: &str) -> BoolArg {
    match raw.to_lowercase().as_str() {
        "on" | "true" | "1" => BoolArg::On,
        "off" | "false" | "0" => BoolArg::Off,
        "" => BoolArg::Empty,
        _ => BoolArg::Other(raw.to_lowercase()),
    }
}

/// Strip `prefix` from `text`, trim, and return `Some` if non-empty.
fn parse_string_arg(text: &str, prefix: &str) -> Option<String> {
    let value = text.strip_prefix(prefix)?.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// Handler for one table row: the raw input line in, "consumed" out.
type CommandHandler = fn(&mut App, &str) -> bool;

/// One row of the TUI command table.
pub(super) struct CommandRoute {
    /// Canonical spelling, leading slash included.
    pub(super) name: &'static str,
    /// Accepted alternative spellings (`/ss` for `/session`).
    pub(super) aliases: &'static [&'static str],
    /// `true` when the command accepts an argument tail: `/x` and `/x <args>`
    /// both route here. With `false` only the bare spelling does, so
    /// `/clear ` (trailing space, no argument) stays a plain message.
    pub(super) takes_args: bool,
    /// The implementation.
    pub(super) handler: CommandHandler,
}

impl CommandRoute {
    /// Whether this row claims `text`.
    pub(super) fn matches(&self, text: &str) -> bool {
        if text == self.name || self.aliases.contains(&text) {
            return true;
        }
        if !self.takes_args {
            return false;
        }
        std::iter::once(self.name)
            .chain(self.aliases.iter().copied())
            .any(|name| text.starts_with(&format!("{name} ")))
    }
}

/// Every line the TUI intercepts before it would reach the model.
///
/// Rows are matched in order; names and aliases are unique (pinned by a test),
/// so no row can shadow another.
pub(super) const COMMANDS: &[CommandRoute] = &[
    // ---- Local (frontend-only) commands ----
    CommandRoute {
        name: CLEAR_COMMAND,
        aliases: &[],
        takes_args: false,
        handler: clear_chat,
    },
    CommandRoute {
        name: NEW_COMMAND,
        aliases: &[],
        takes_args: false,
        handler: create_session,
    },
    CommandRoute {
        name: COPY_COMMAND,
        aliases: &[],
        takes_args: true,
        handler: copy_assistant_message,
    },
    CommandRoute {
        name: GOAL_COMMAND,
        aliases: &[],
        takes_args: true,
        handler: goal_lane::start_goal_command,
    },
    CommandRoute {
        name: GOAL_EXIT_COMMAND,
        aliases: &[],
        takes_args: false,
        handler: goal_lane::exit_goal_command,
    },
    // ---- HTTP-migrated commands (side effect = AppIntent) ----
    CommandRoute {
        name: "/context",
        aliases: &[],
        takes_args: false,
        handler: show_context_info,
    },
    CommandRoute {
        name: "/skills",
        aliases: &[],
        takes_args: false,
        handler: show_skills_info,
    },
    CommandRoute {
        name: "/model",
        aliases: &[],
        takes_args: true,
        handler: open_model_picker,
    },
    CommandRoute {
        name: "/agents",
        aliases: &[],
        takes_args: true,
        handler: use_agent,
    },
    CommandRoute {
        name: "/title",
        aliases: &[],
        takes_args: true,
        handler: set_or_show_title,
    },
    CommandRoute {
        name: "/workdir",
        aliases: &[],
        takes_args: true,
        handler: set_or_show_workdir,
    },
    CommandRoute {
        name: "/think",
        aliases: &[],
        takes_args: true,
        handler: set_thinking,
    },
    CommandRoute {
        name: "/yolo",
        aliases: &[],
        takes_args: true,
        handler: set_yolo,
    },
    CommandRoute {
        name: "/compact",
        aliases: &[],
        takes_args: true,
        handler: compact_session,
    },
    CommandRoute {
        name: "/reload",
        aliases: &[],
        takes_args: false,
        handler: reload_system,
    },
    CommandRoute {
        name: "/fork",
        aliases: &[],
        takes_args: true,
        handler: fork_session,
    },
    CommandRoute {
        name: "/rewind",
        aliases: &[],
        takes_args: true,
        handler: rewind_session,
    },
    CommandRoute {
        name: "/session",
        aliases: &["/ss"],
        takes_args: true,
        handler: resume_session,
    },
];

impl App {
    /// Submit text from the input area: try the command table, else send as message.
    ///
    /// Returns `true` if the text was consumed (either as a command or message).
    pub(super) fn submit_message(&mut self, text: &str) -> bool {
        if self.try_frontend_command(text) {
            return true;
        }
        // Goal mode: route user input through Goal state machine.
        if let Some(goal) = self.goal.as_mut() {
            self.chat.push(ChatCell::UserMessage(text.to_string()));
            let actions = goal.on_user_input(text);
            self.execute_goal_actions(actions);
            self.turn.usage = TurnUsage::default();
            return true;
        }
        // Normal path: the message only moves up into chat history when the
        // model actually receives it (user_message_accepted). Until then it
        // queues in the pending area below in-flight streaming output.
        let request_id = crate::protocol::generate_request_id();
        self.chat.push_pending(request_id.clone(), text.to_string());
        self.push_intent(AppIntent::SendMessage {
            content: text.to_string(),
            tool_call_id: None,
            request_id,
        });
        self.turn.usage = TurnUsage::default();
        true
    }

    /// Route `text` through the command table.
    ///
    /// Returns `true` if the command was recognized and handled locally;
    /// `false` if it should be sent to the gateway as usual.
    pub(super) fn try_frontend_command(&mut self, text: &str) -> bool {
        let Some(route) = COMMANDS.iter().find(|route| route.matches(text)) else {
            return false;
        };
        (route.handler)(self, text)
    }

    /// Update popup state based on current input text.
    ///
    /// Skips requests while the agent is streaming to avoid interference.
    /// HTTP calls are idempotent — no dedup needed.
    pub(super) fn update_popup(&mut self) {
        // The modal model panel owns keyboard input — no popup while open.
        if self.model_panel.is_some() {
            return;
        }

        // Streaming guard: skip requests while agent is busy.
        let streaming = self.turn.working;
        let text = self.input.text().to_string();
        if let Some(action) = self.popup.update_from_input(&text) {
            if streaming {
                return;
            }
            match action {
                PopupAction::FetchBranches => {
                    self.push_intent(AppIntent::FetchBranches);
                }
                PopupAction::FetchAgents => {
                    self.push_intent(AppIntent::FetchAgents);
                }
                PopupAction::FetchSessionList => {
                    self.push_intent(AppIntent::FetchSessionList);
                }
            }
        }
    }

    /// Refresh `/copy` candidate cache from current chat state.
    pub(super) fn refresh_copy_candidates(&mut self) {
        self.popup.cache.copies = self.chat.collect_assistant_messages();
    }

    /// Invalidate the cached session list so the next `/session` popup re-fetches.
    ///
    /// Called after session-mutating operations (resume, create, fork, title
    /// update) succeed, ensuring the popup always shows fresh data.
    pub(super) fn invalidate_session_cache(&mut self) {
        self.popup.cache.sessions.clear();
    }

    /// Apply a background fetch result to app state.
    ///
    /// Called from the main event loop when a spawned fetch task completes.
    /// Discards stale results whose `session_id` doesn't match the current session.
    pub(super) fn handle_fetch_result(&mut self, result: crate::app::intent::FetchResult) {
        use crate::app::intent::FetchPayload;

        // Session guard: discard results from a previous session.
        if result.session_id != self.session_id {
            tracing::debug!(
                stale = %result.session_id,
                current = %self.session_id,
                "discarding stale fetch result"
            );
            return;
        }

        match result.payload {
            FetchPayload::Info(info) => {
                // Update model; clear provider when the model changes, since
                // Info does not carry provider info and the old provider
                // may be stale (e.g. the model was changed via another path).
                if info.model != self.status.model {
                    self.status.provider = None;
                }
                self.status.model = info.model;
                self.status.total_tokens = info.total_tokens;
                self.status.context_window_tokens = info.context_window_tokens;
                self.status.thinking = info.thinking;
                self.status.reasoning_effort = info.reasoning_effort;
                self.status.yolo = info.yolo;
                self.status.session_name = info.session_name;
                self.status.workdir = info.workdir;
                // Refresh title so the workdir suffix appears once known.
                let dir = self.dir_label();
                let title = if self.turn.working {
                    title::title_working(self.turn.spinner.frame_str(), dir.as_deref())
                } else {
                    title::title_idle(dir.as_deref())
                };
                self.turn.last_title = Some(title.clone());
                self.push_intent(AppIntent::SetTitle(title));
                tracing::info!(model = %self.status.model, "session info received");
            }
            FetchPayload::Commands(resp) => {
                self.popup.cache.commands = resp
                    .commands
                    .into_iter()
                    .map(|c| crate::protocol::CommandInfo {
                        name: c.name,
                        aliases: c.aliases,
                        description: c.description,
                        params: c.params,
                    })
                    .collect();
                self.update_popup();
            }
            FetchPayload::Models(resp) => {
                if resp.providers.is_empty() {
                    // Nothing to choose from — tell the user, keep any open
                    // panel as it was.
                    self.show_toast(Toast::warning(
                        "No models available",
                        std::time::Duration::from_secs(3),
                    ));
                } else {
                    self.model_sources = resp.providers;
                    if let Some(panel) = self.model_panel.as_mut() {
                        // Panel already open: refresh in place, keeping the
                        // current page and cursor, and mirror it into its cell.
                        panel.set_sources(self.model_sources.clone());
                        self.sync_model_panel_cell();
                    } else if self.model_panel_pending {
                        // The user typed `/model` before the cache was
                        // available — deliver the panel now that the fetch
                        // completed.  Clear the flag so a subsequent fetch
                        // (refresh) does NOT reopen after the user closes it.
                        self.model_panel_pending = false;
                        let panel =
                            ModelPanel::new(self.model_sources.clone(), self.current_model_pair());
                        self.present_model_panel(panel);
                    }
                    // If the panel was closed (Esc) while the fetch was in
                    // flight, do NOT reopen it — the user's explicit action
                    // takes priority over the stale fetch result.
                }
            }
            FetchPayload::Branches(resp) => {
                self.popup.cache.branches = resp
                    .targets
                    .into_iter()
                    .map(|t| {
                        let preview = truncate_by_chars(&t.content, 80);
                        (t.uuid, preview)
                    })
                    .collect();
                self.update_popup();
            }
            FetchPayload::Agents(resp) => {
                self.popup.cache.agents = resp
                    .agents
                    .into_iter()
                    .map(|a| (a, String::new()))
                    .collect();
                self.update_popup();
            }
            FetchPayload::SessionList(resp) => {
                use crate::ui::popup::command::SessionCandidate;
                use crate::ui::popup::selection::SessionStatus;

                // Normalize a path for workdir comparison (strip trailing slashes).
                let norm = |p: &str| {
                    let t = p.trim_end_matches('/');
                    if t.is_empty() {
                        "/".to_string()
                    } else {
                        t.to_string()
                    }
                };
                let launch_norm = norm(self.launch_workspace.as_deref().unwrap_or(""));

                // A session workspace "matches" the launch dir if it equals it or
                // is a subdirectory (prefix match on path components).
                let ws_matches = |ws: &str| {
                    let n = norm(ws);
                    n == launch_norm || n.starts_with(&format!("{launch_norm}/"))
                };

                let mut candidates: Vec<SessionCandidate> = resp
                    .sessions
                    .iter()
                    .map(|s| SessionCandidate {
                        id: s.id.clone(),
                        title: s.name.clone().unwrap_or_default(),
                        workspace: s.workspace.clone().unwrap_or_default(),
                        status: s.status.clone(),
                        last_interaction: s.last_interaction.clone().unwrap_or_default(),
                    })
                    .collect();

                // Stable sort: ① workdir 匹配当前启动目录者优先（前缀匹配）→
                // ② 状态优先级 (waiting > working > idle > inactive) →
                // ③ 保持后端时间降序。
                candidates.sort_by_key(|c| {
                    let ws_mismatch = !ws_matches(&c.workspace);
                    (ws_mismatch, SessionStatus::parse(&c.status).rank())
                });

                self.popup.cache.sessions = candidates;
                self.update_popup();
            }
            FetchPayload::ContextInfo(text) => {
                self.chat.push(ChatCell::SystemMessage(text));
            }
            FetchPayload::SkillsInfo(text) => {
                self.chat.push(ChatCell::SystemMessage(text));
            }
            FetchPayload::CompactDone {
                original,
                compressed,
            } => {
                self.show_toast(Toast::info(
                    format!("Compact done: {original} → {compressed} tokens"),
                    std::time::Duration::from_secs(3),
                ));
            }
            FetchPayload::Toast { message, is_error } => {
                if is_error {
                    self.show_toast(Toast::error(message, std::time::Duration::from_secs(3)));
                } else {
                    self.show_toast(Toast::info(message, std::time::Duration::from_secs(3)));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers — local commands
// ---------------------------------------------------------------------------

/// `/clear` — wipe the chat view (history stays on the gateway).
fn clear_chat(app: &mut App, _text: &str) -> bool {
    app.chat.clear();
    app.refresh_copy_candidates();
    app.show_toast(Toast::info(
        "Chat cleared",
        std::time::Duration::from_secs(2),
    ));
    true
}

/// `/new` — create a session in the launch workspace.
fn create_session(app: &mut App, _text: &str) -> bool {
    app.push_intent(AppIntent::CreateSession {
        workspace: app.launch_workspace.clone(),
    });
    true
}

/// `/copy [N]` — copy the N-th (or last) assistant message to clipboard.
fn copy_assistant_message(app: &mut App, text: &str) -> bool {
    use std::time::Duration;

    let index = text
        .strip_prefix("/copy ")
        .and_then(|s| s.trim().parse::<usize>().ok());

    let content = match index {
        Some(n) => app.chat.nth_assistant_text(n).map(String::from),
        None => app.chat.last_assistant_text().map(String::from),
    };

    let Some(content) = content else {
        app.show_toast(Toast::warning(
            "No assistant message to copy",
            Duration::from_secs(2),
        ));
        return true;
    };

    app.push_intent(AppIntent::CopyToClipboard(content));
    true
}

// ---------------------------------------------------------------------------
// Handlers — HTTP-migrated commands
// ---------------------------------------------------------------------------

/// `/context` — show the context window usage.
fn show_context_info(app: &mut App, _text: &str) -> bool {
    app.push_intent(AppIntent::ShowContextInfo);
    true
}

/// `/skills` — show loaded skills / rules.
fn show_skills_info(app: &mut App, _text: &str) -> bool {
    app.push_intent(AppIntent::ShowSkillsInfo);
    true
}

/// `/model [ignored]` — open the picker panel.
///
/// Args are ignored (BREAKING): model selection goes through the panel so the
/// (provider, model) pair is explicit.
fn open_model_picker(app: &mut App, _text: &str) -> bool {
    app.open_model_panel();
    true
}

/// `/agents [name]` — switch agent template, or list the available ones.
fn use_agent(app: &mut App, text: &str) -> bool {
    match parse_string_arg(text, "/agents") {
        Some(agent) => {
            app.push_intent(AppIntent::set_agent(agent));
        }
        None => {
            app.push_intent(AppIntent::FetchAgents);
        }
    }
    true
}

/// `/title [name]` — set the session title, or show the current one.
fn set_or_show_title(app: &mut App, text: &str) -> bool {
    let args = text.strip_prefix("/title").unwrap().trim();
    if args.is_empty() {
        let title = app.status.session_name.as_deref().unwrap_or("(not set)");
        app.show_toast(Toast::info(
            format!("title: {title}"),
            std::time::Duration::from_secs(3),
        ));
    } else {
        app.push_intent(AppIntent::set_title(args.to_string()));
    }
    true
}

/// `/workdir [path]` — set the session workspace, or show the current one.
fn set_or_show_workdir(app: &mut App, text: &str) -> bool {
    let args = text.strip_prefix("/workdir").unwrap().trim();
    if args.is_empty() {
        let wd = app.status.workdir.as_deref().unwrap_or("(not set)");
        app.show_toast(Toast::info(
            format!("workdir: {wd}"),
            std::time::Duration::from_secs(3),
        ));
    } else {
        app.push_intent(AppIntent::set_workdir(args.to_string()));
    }
    true
}

/// `/think [on|off|<effort>]` — toggle reasoning, or show its state.
fn set_thinking(app: &mut App, text: &str) -> bool {
    let args = text.strip_prefix("/think").unwrap().trim();
    match parse_bool_arg(args) {
        BoolArg::Empty => {
            let effort = app.status.reasoning_effort.as_deref().unwrap_or("default");
            let msg = format!("think: {} (effort: {})", app.status.thinking, effort);
            app.show_toast(Toast::info(&msg, std::time::Duration::from_secs(3)));
            true
        }
        BoolArg::On => {
            app.push_intent(AppIntent::set_thinking(true, None));
            true
        }
        BoolArg::Off => {
            app.push_intent(AppIntent::set_thinking(false, None));
            true
        }
        BoolArg::Other(effort)
            if matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max") =>
        {
            app.push_intent(AppIntent::set_thinking(true, Some(effort)));
            true
        }
        BoolArg::Other(_) => {
            app.show_toast(Toast::warning(
                "Usage: /think [on|off|low|medium|high|xhigh|max]",
                std::time::Duration::from_secs(3),
            ));
            true
        }
    }
}

/// `/yolo [on|off]` — toggle yolo mode, or show its state.
fn set_yolo(app: &mut App, text: &str) -> bool {
    let args = text.strip_prefix("/yolo").unwrap().trim();
    match parse_bool_arg(args) {
        BoolArg::Empty => {
            let msg = format!("yolo: {}", app.status.yolo);
            app.show_toast(Toast::info(&msg, std::time::Duration::from_secs(3)));
            true
        }
        BoolArg::On => {
            app.push_intent(AppIntent::set_yolo(true));
            true
        }
        BoolArg::Off => {
            app.push_intent(AppIntent::set_yolo(false));
            true
        }
        BoolArg::Other(_) => {
            app.show_toast(Toast::warning(
                "Usage: /yolo [on|off]",
                std::time::Duration::from_secs(3),
            ));
            true
        }
    }
}

/// `/compact [instruction]` — compact the context, with an optional focus.
fn compact_session(app: &mut App, text: &str) -> bool {
    // `/compact` 或 `/compact <侧重指令>` —— 指令附加到压缩 prompt。
    let args = text.strip_prefix("/compact").unwrap().trim();
    let instruction = if args.is_empty() {
        None
    } else {
        Some(args.to_string())
    };
    app.push_intent(AppIntent::CompactSession { instruction });
    true
}

/// `/reload` — reload agents / config from disk.
fn reload_system(app: &mut App, _text: &str) -> bool {
    app.push_intent(AppIntent::ReloadSystem);
    true
}

/// `/fork <uuid>` — fork the session at a branch target.
fn fork_session(app: &mut App, text: &str) -> bool {
    match parse_string_arg(text, "/fork") {
        Some(uuid) => {
            app.push_intent(AppIntent::ForkSession { target_uuid: uuid });
        }
        None => {
            app.show_toast(Toast::warning(
                "Usage: /fork <uuid>",
                std::time::Duration::from_secs(3),
            ));
        }
    }
    true
}

/// `/rewind <uuid>` — rewind the session to a branch target.
fn rewind_session(app: &mut App, text: &str) -> bool {
    match parse_string_arg(text, "/rewind") {
        Some(uuid) => {
            app.push_intent(AppIntent::RewindSession { target_uuid: uuid });
        }
        None => {
            app.show_toast(Toast::warning(
                "Usage: /rewind <uuid>",
                std::time::Duration::from_secs(3),
            ));
        }
    }
    true
}

/// `/session <id>` (alias `/ss`) — resume a session by id.
fn resume_session(app: &mut App, text: &str) -> bool {
    let id = parse_string_arg(text, "/session").or_else(|| parse_string_arg(text, "/ss"));
    match id {
        Some(id) => {
            app.push_intent(AppIntent::ResumeSession { session_id: id });
        }
        None => {
            app.show_toast(Toast::warning(
                "Usage: /session <id>",
                std::time::Duration::from_secs(3),
            ));
        }
    }
    true
}
