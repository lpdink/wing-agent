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
//! [`super::modal`] for the `/model` picker.

use super::App;
use super::AppIntent;
use super::title;
use crate::shared::constants::CLEAR_COMMAND;
use crate::shared::constants::COPY_COMMAND;
use crate::shared::constants::NEW_COMMAND;
use crate::shared::constants::TIPS_COMMAND;
use crate::shared::panels::picker::ModelPanel;
use crate::shared::tips::full_text as tips_full_text;
use crate::ui::cells::tool_call::truncate_by_chars;
use crate::ui::chat_view::ChatCell;
use crate::ui::chat_view::Submission;
use crate::ui::input_area::ImagePayload;
use crate::ui::input_area::SubmitPayload;
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

/// The command table row a line routes to, if any.
///
/// The one predicate behind [`App::try_frontend_command`] — callers that only
/// need the *name* (the submit path's dropped-picture notice) ask it instead of
/// re-deriving the match, so "matched" and "named" cannot disagree.
pub(super) fn frontend_command(text: &str) -> Option<&'static CommandRoute> {
    COMMANDS.iter().find(|route| route.matches(text))
}

/// 图片随一次提交被丢掉时的提示文案（命令不是消息，图片没有落点）。
///
/// 两个丢图点必须说同一句话：`submit_message`（命令真的开始跑）与
/// `handle_composer_key` 的 must-select 重写路径（草稿被换回纯文本、提交被拦下
/// 去重开候选 popup）。静默丢一张图是最坏的结果，而文案分叉会让"丢的是哪条命令"
/// 在两个入口上对不上。
pub(super) fn image_dropped_toast(command: &str) -> Toast {
    Toast::info(
        format!("Image dropped: {command} takes no image"),
        std::time::Duration::from_secs(3),
    )
}

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
        name: TIPS_COMMAND,
        aliases: &[],
        takes_args: false,
        handler: show_tips,
    },
    CommandRoute {
        name: COPY_COMMAND,
        aliases: &[],
        takes_args: true,
        handler: copy_assistant_message,
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
    // ---- 设置面板（TUI 写配置的唯一入口；design §12.4） ----
    CommandRoute {
        name: "/settings",
        aliases: &["/config", "/set"],
        takes_args: false,
        handler: open_settings_command,
    },
];

impl App {
    /// Submit the composer's product: try the command table, else send as
    /// message.
    ///
    /// The payload travels whole, and the two halves part ways here: a
    /// frontend command reads the text and **never touches the pictures** — a
    /// command runs locally, it is not a message, so there is nowhere for a
    /// picture to ride. A paste chip's payload survives that path (its text
    /// form is already inside `text`), a picture's does not, so the drop is
    /// announced (see below).
    ///
    /// Returns `true` if it was consumed (either as a command or message).
    pub(super) fn submit_message(&mut self, payload: SubmitPayload) -> bool {
        // 同一个谓词：`Some` ⟺ `try_frontend_command` 命中（下面那句提示读它取命令名）。
        let command = frontend_command(&payload.text).map(|route| route.name);
        if self.try_frontend_command(&payload.text) {
            if let Some(command) = command
                && !payload.images.is_empty()
            {
                // 命令不是消息，图片没有落点。罕见，但静默丢一张图是最坏的结果，
                // 所以这里说一句（命令自己的 toast 若也有，会被它顶掉——命令的效果
                // 在屏幕上看得见，而图片的消失看不见）。
                self.show_toast(image_dropped_toast(command));
            }
            return true;
        }
        // The message only moves up into chat history when the
        // model actually receives it (user_message_accepted). Until then it
        // queues in the pending area below in-flight streaming output — and
        // carries the submission itself, so a failure can hand it back to the
        // composer (see `restore_failed_send`).
        let request_id = crate::protocol::generate_request_id();
        let images: Vec<_> = payload
            .images
            .into_iter()
            .map(ImagePayload::into_image)
            .collect();
        // 提交产物随 pending 条目一起留下（正文 + 图片 + 预测引用）：气泡用它拼内容
        // （图块 / 标签），失败回填用它把原件还给草稿。图片字节因此多存一份（≤1 MiB/张），
        // 换来「被拒 / 失败之后内容还在用户手里」。
        let submission = Submission::new(payload.text.clone(), images.clone());
        // 本机就有字节：pending 气泡立刻能画（不必等上传往返，也不必再抓一次）。引用是
        // 按内容寻址预测的（`Submission::media`，与上传回执同 id）；这里把字节**再拷一份**
        // 进图片 lane 的缓存（提交产物那份随 pending 条目走，lane 这份随图片被淘汰）。
        for (image, picture) in submission.images.iter().zip(submission.media()) {
            self.images
                .supply_media(&picture.id, std::sync::Arc::new(image.bytes.clone()));
        }
        self.chat.push_pending(request_id.clone(), submission);
        self.push_intent(AppIntent::SendMessage {
            content: payload.text,
            tool_call_id: None,
            request_id,
            // 上传只要图片本身（标签是纯展示）。
            images,
        });
        self.turn.usage = TurnUsage::default();
        true
    }

    /// 一条发不出去的用户消息：从 pending 队列取回产物、还给草稿、提示原因。
    ///
    /// 三条发送失败路径（上传 / WS / 断连）与服务端拒绝共用这里——共同点是草稿在提交
    /// 时就清了，内容只在 pending 里。**纯数据恢复**：只写 composer 与 toast，不 push
    /// intent、不碰 chat（不重发、不重复上传）。
    ///
    /// 条目已经不在队列里（被打断丢弃 / 已提升）⇒ 只提示原因：内容要么已经在历史里
    /// 看得见，要么由那条路径自己解释过了。
    pub(super) fn restore_failed_send(&mut self, request_id: &str, reason: &str) -> bool {
        let Some(submission) = self.chat.take_pending(request_id) else {
            self.show_toast(Toast::warning(
                reason.to_string(),
                std::time::Duration::from_secs(3),
            ));
            return false;
        };
        self.restore_draft(submission);
        self.show_toast(Toast::warning(
            format!("{reason}; draft restored"),
            std::time::Duration::from_secs(3),
        ));
        true
    }

    /// 把一份提交产物放回草稿：文本 + 图片 chip。
    ///
    /// 空草稿走 `set_text`（提交时的原文精确还原）；草稿里已经有内容（消息在飞的那几
    /// 秒里用户又打了字 / 粘了图）时**追加在末尾**——`set_text` 的既定语义是替换整份
    /// 草稿、连同它的 chip 一起丢，那会吃掉用户刚写的东西。
    pub(super) fn restore_draft(&mut self, submission: Submission) {
        if self.input.is_empty() {
            self.input.set_text(&submission.content);
        } else {
            self.input.move_draft_end();
            if !submission.content.is_empty() {
                self.input.insert_str(&format!("\n{}", submission.content));
            }
        }
        for image in submission.images {
            self.input.insert_image(image);
        }
    }

    /// Route `text` through the command table.
    ///
    /// Returns `true` if the command was recognized and handled locally;
    /// `false` if it should be sent to the gateway as usual.
    pub(super) fn try_frontend_command(&mut self, text: &str) -> bool {
        let Some(route) = frontend_command(text) else {
            return false;
        };
        (route.handler)(self, text)
    }

    /// Update popup state based on current input text.
    ///
    /// Skips requests while the agent is streaming to avoid interference.
    /// HTTP calls are idempotent — no dedup needed.
    ///
    /// 响应路径（fetch 结果、快照）走这里：只原地换行，不产生新请求。
    pub(super) fn update_popup(&mut self) {
        self.update_popup_inner(false);
    }

    /// 输入路径的 popup 更新（按键 / 粘贴改变了 draft）：额外承担
    /// **「面板打开即刷新一次」**（#147）。
    ///
    /// 面板是快照：缓存只负责"先渲染旧行"，不再是"有缓存就不取数"的判据。
    /// 边沿只在输入路径判定（`was_session_panel → is_session_panel`），刷新
    /// 响应走响应路径原地换行——**一次打开恰好一次请求**；流式（agent 忙）
    /// 时沿用既有守卫，不发请求。
    pub(super) fn update_popup_from_input(&mut self) {
        self.update_popup_inner(true);
    }

    fn update_popup_inner(&mut self, from_input: bool) {
        // The modal model panel owns keyboard input — no popup while open.
        if self.model_panel.is_some() {
            return;
        }

        // Streaming guard: defer *refreshes* while the agent is busy.
        let streaming = self.turn.working;
        // 面板身份必须在更新**之前**读：进入（上一次不在面板里）才是边沿。
        let was_session_panel = self.popup.active.session_panel_command().is_some();
        let text = self.input.text().to_string();
        if let Some(action) = self.popup.update_from_input(&text) {
            // 流式守卫**不拦**这一支：`update_from_input` 只在候选缺失时返回
            // action，所以这条 fetch 就是**面板的首帧**（各命令的等待态画不画
            // 得出东西都不一样——`/ss` 的占位列表无行、`/session` 与 `/fork`
            // 会显示一条命令占位行——所以判据不能看画没画出来，只能看是不是
            // 首帧）。丢掉它面板永远停在占位上：没有任何路径在 turn 结束时补发
            // （响应路径挂在 fetch 结果上，不挂在 turn 状态上）。它也不打扰
            // 正在跑的一轮：intent 是 spawn 出去的只读 GET；"打开即刷新"的
            // 请求另有下文的 `!streaming` 守卫。
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
            return;
        }
        // 刚打开面板（且这次是输入路径）→ 刷新一次。等待首帧列表的路径已经
        // 由上面的 action 分支发过请求，不会走到这里。
        if from_input
            && !streaming
            && !was_session_panel
            && self.popup.active.session_panel_command().is_some()
        {
            self.push_intent(AppIntent::FetchSessionList);
        }
    }

    /// Refresh `/copy` candidate cache from current chat state.
    pub(super) fn refresh_copy_candidates(&mut self) {
        self.popup.cache.copies = self.chat.collect_assistant_messages();
    }

    /// Invalidate the cached session list so the next `/session` popup re-fetches.
    ///
    /// Called after session-mutating operations (resume, create, fork, title
    /// update) succeed, ensuring the popup always shows fresh data. 复位的是
    /// "抓过没有"而不是"有没有内容"：空列表也是抓过的结果，切换会话后必须
    /// 重新抓（见 `CandidateCache::sessions_fetched`）。
    pub(super) fn invalidate_session_cache(&mut self) {
        self.popup.cache.invalidate_sessions();
    }

    /// Apply a background fetch result to app state.
    ///
    /// Called from the main event loop when a spawned fetch task completes.
    /// Discards stale results whose `session_id` doesn't match the current session.
    pub(super) fn handle_fetch_result(&mut self, result: crate::app::intent::FetchResult) {
        use crate::app::intent::FetchPayload;

        let crate::app::intent::FetchResult {
            session_id,
            payload,
        } = result;

        // 粘贴图片的压缩回执属于 **composer**，不属于会话：草稿跨会话还在，而守卫一旦
        // 丢掉它，流水线的「在飞」标记就永远为真（之后所有粘贴都不落地）。所以它在
        // 守卫**之前**分派。
        if let FetchPayload::ClipboardImage { target, result } = payload {
            self.on_clipboard_compressed(target, result);
            return;
        }

        // 消息的后台发送失败回执：属于那条 pending 条目（按 `request_id` 认领），
        // 同样在守卫之前分派——换过会话后条目多半已被 `chat.clear()` 清掉，此时
        // `restore_failed_send` 只 toast 原因（不碰草稿、不假装回填）。
        if let FetchPayload::SendFailed { request_id, reason } = payload {
            self.restore_failed_send(&request_id, &reason);
            return;
        }

        // 本地剪贴板读取（L1）的回执同样是 composer 的：换过会话草稿还在，粘贴照落
        // （文本进草稿、图片进流水线）。
        if let FetchPayload::ClipboardPaste(result) = payload {
            self.on_clipboard_paste(result);
            return;
        }

        // Session guard: discard results from a previous session.
        if session_id != self.session_id {
            tracing::debug!(
                stale = %session_id,
                current = %self.session_id,
                "discarding stale fetch result"
            );
            return;
        }

        match payload {
            FetchPayload::Info(info) => {
                // The model identity group travels whole: `model_id` is the
                // reference word, `model` the call name, `provider_name` the
                // runtime fact (info ships it now — no more "clear the provider
                // when the model changes" guesswork). The display label falls
                // back to the local `/api/models` snapshot only when the
                // gateway did not ship one, so a reconnect does not drop a
                // label that is already known: by id when there is one, by
                // call name for id-less sessions (display only).
                let model_display_name = info.model_display_name.clone().or_else(|| {
                    match info.model_id.as_deref().filter(|id| !id.trim().is_empty()) {
                        Some(id) => self.model_display_label(id),
                        None => self.model_display_label_by_name(&info.model),
                    }
                });
                self.status.set_model(crate::ui::status_bar::ModelUpdate {
                    id: info.model_id.clone(),
                    name: info.model.clone(),
                    provider: info.provider_name.clone(),
                    display_name: model_display_name,
                });
                self.status.total_tokens = info.total_tokens;
                self.status.context_window_tokens = info.context_window_tokens;
                self.status.thinking = info.thinking;
                self.status.reasoning_effort = info.reasoning_effort;
                self.status.yolo = info.yolo;
                self.status.session_name = info.session_name;
                self.status.workdir = info.workdir;
                // pin 星标的唯一数据来源（快照不带标签，见 `apply_sync_session`
                // 的补取）——以服务端读回的 `pin` 标签为准。
                self.status.pinned = crate::shared::pinning::is_pinned(&info.tags);
                // Refresh title so the workdir suffix appears once known.
                let dir = self.dir_label();
                let title = if self.turn.working {
                    title::title_working(self.turn.spinner.frame_str(), dir.as_deref())
                } else {
                    title::title_idle(dir.as_deref())
                };
                self.turn.last_title = Some(title.clone());
                self.push_intent(AppIntent::SetTitle(title));
                tracing::info!(model = %self.status.model, model_id = ?self.status.model_id, "session info received");
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
                            ModelPanel::new(self.model_sources.clone(), self.current_model_id());
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
                use crate::shared::pinning::{is_pinned, pin_added_at};
                use crate::shared::session_tree::{TreeRow, build_forest};
                use crate::ui::popup::command::SessionCandidate;

                // 面板的展示序 = **树序**（`shared::session_tree`，与 `wing ps
                // --tree` 共用同一份实现，见 docs/dev/session-lineage.md §4）：
                // 有父的会话跟在父下面并带连接前缀，pin 的根连同子树整块上浮，
                // 子节点的 pin 只在兄弟组内说话。后端下发的基准序不再是面板的
                // 定序输入——树序的每个排序键都是自足的（pin 时间 / 子树活跃 /
                // id），workspace 与 status 只做展示与搜索。
                let anchor = self.popup.active.selected_name().map(str::to_string);
                // pin 事实（标签 + 时间）算一次喂两处：树的排序键与候选行的星标
                // 谓词——两者共用同一份投影，不会各算各的。
                let pins: Vec<(bool, Option<&str>)> = resp
                    .sessions
                    .iter()
                    .map(|s| (is_pinned(&s.tags), pin_added_at(&s.tag_meta)))
                    .collect();
                let rows: Vec<TreeRow<'_>> = resp
                    .sessions
                    .iter()
                    .zip(&pins)
                    .map(|(s, &(pinned, pin_at))| TreeRow {
                        id: &s.id,
                        parent: s.parent_session_id.as_deref(),
                        pinned,
                        pin_added_at: pin_at,
                        last_interaction: s.last_interaction.as_deref(),
                    })
                    .collect();
                let candidates: Vec<SessionCandidate> = build_forest(&rows)
                    .into_iter()
                    .map(|entry| {
                        let s = &resp.sessions[entry.index];
                        SessionCandidate {
                            id: s.id.clone(),
                            title: s.name.clone().unwrap_or_default(),
                            workspace: s.workspace.clone().unwrap_or_default(),
                            status: s.status.clone(),
                            last_interaction: s.last_interaction.clone().unwrap_or_default(),
                            pinned: pins[entry.index].0,
                            tree_prefix: entry.prefix,
                        }
                    })
                    .collect();

                self.popup.cache.sessions = candidates;
                self.popup.cache.sessions_fetched = true;
                self.update_popup();
                // 刷新后光标按 session id 回到原选中行（行序可能因 pin 变化，
                // 选区不该跟着跳走）。
                if let Some(anchor) = anchor {
                    self.popup.active.reselect(&anchor);
                }
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
            FetchPayload::Settings { schema, state } => {
                self.handle_settings_fetch(*schema, *state);
            }
            FetchPayload::SettingsSaved(response) => {
                self.settle_gateway_save(crate::app::settings::GatewaySaveReport::Saved(*response));
            }
            FetchPayload::MediaBytes { media_id, result } => {
                // 字节到手 → 下一帧编码并画出来；抓不到 → 记下来（离开视口再重试）。
                match result {
                    Ok(bytes) => self.images.supply_media(&media_id, bytes),
                    Err(_) => self.images.media_failed(&media_id),
                }
            }
            FetchPayload::SettingsSaveError { message, conflict } => {
                self.settle_gateway_save(crate::app::settings::GatewaySaveReport::Failed {
                    message,
                    conflict,
                });
            }
            // 上面已经分派过（这些回执不属于会话：composer 的压缩 / 本地读取 / pending 的发送失败）。
            FetchPayload::ClipboardImage { .. }
            | FetchPayload::ClipboardPaste(_)
            | FetchPayload::SendFailed { .. } => {}
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

/// `/tips` — list every startup tip in the chat.
///
/// 欢迎屏只轮换显示一条；想看全量就这条命令 —— 两个出口读的是同一个池子。
fn show_tips(app: &mut App, _text: &str) -> bool {
    app.chat.push(ChatCell::SystemMessage(tips_full_text()));
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
/// applied value is the model id the gateway takes.
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

/// `/settings` (aliases `/config`, `/set`) — open the settings panel.
///
/// 与 `/model` 不同：设置面板**允许在轮次进行中打开**（provider 重建是 reload-safe 的，
/// design §12.5），cache-first 的打开与后台刷新都在 [`App::open_settings_panel`] 里。
fn open_settings_command(app: &mut App, _text: &str) -> bool {
    app.open_settings_panel();
    true
}
