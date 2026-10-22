//! Command routing lane tests — the table, its handlers, fetch results.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use super::support::*;
use crate::app::commands;
use crate::app::intent::SessionUpdate;
use crate::app::*;
use crate::shared::panels::SelectionPanel;
use crate::ui::chat_view::ChatCell;
use crate::ui::input_area::SubmitPayload;

/// Assert that `text` is consumed as a command (not sent to LLM).
fn assert_consumed(text: &str) {
    let mut app = test_app();
    assert!(
        app.try_frontend_command(text),
        "expected '{text}' to be consumed as a command"
    );
}

/// Assert that `text` is NOT consumed (falls through to LLM).
fn assert_not_consumed(text: &str) {
    let mut app = test_app();
    assert!(
        !app.try_frontend_command(text),
        "expected '{text}' to fall through to LLM"
    );
}

// ── Dispatch completeness: all known commands are consumed ──

#[test]
fn test_frontend_commands_consumed() {
    for cmd in ["/clear", "/new", "/copy", "/copy 1", "/tips"] {
        assert_consumed(cmd);
    }
}

#[test]
fn test_http_commands_consumed() {
    for cmd in [
        "/context",
        "/skills",
        "/model",
        "/agents",
        "/model gpt-4o",
        "/agents coder",
        "/title",
        "/title my session",
        "/workdir",
        "/workdir /tmp",
        "/think",
        "/think on",
        "/think off",
        "/think high",
        "/yolo",
        "/yolo on",
        "/yolo off",
        "/compact",
        "/compact keep architecture decisions",
        "/reload",
        "/fork abc-123",
        "/rewind def-456",
        "/session sess-789",
        "/ss sess-789",
    ] {
        assert_consumed(cmd);
    }
}

// ── /compact instruction parsing ────────────────────

#[test]
fn test_compact_command_parses_instruction() {
    // `/compact <侧重>` → instruction rides on the intent.
    let mut app = test_app();
    assert!(app.try_frontend_command("/compact keep architecture decisions and pending TODOs"));
    let intents = app.drain_intents();
    assert!(
        matches!(
            &intents[..],
            [AppIntent::CompactSession {
                instruction: Some(i)
            }] if i == "keep architecture decisions and pending TODOs"
        ),
        "expected CompactSession with instruction, got {intents:?}"
    );

    // Bare `/compact` → default strategy (no instruction).
    let mut app = test_app();
    assert!(app.try_frontend_command("/compact"));
    let intents = app.drain_intents();
    assert!(
        matches!(
            &intents[..],
            [AppIntent::CompactSession { instruction: None }]
        ),
        "expected CompactSession without instruction, got {intents:?}"
    );
}

// ── Bare commands with no args are consumed (usage toast) ──

#[test]
fn test_bare_commands_consumed() {
    for cmd in ["/fork", "/rewind", "/session", "/ss"] {
        assert_consumed(cmd);
    }
}

// ── Trailing space with empty args is consumed (not sent to LLM) ──

#[test]
fn test_trailing_space_consumed() {
    for cmd in [
        "/fork ",
        "/rewind ",
        "/session ",
        "/ss ",
        "/model ",
        "/agents ",
    ] {
        assert_consumed(cmd);
    }
}

// ── Unknown commands fall through to LLM ──

#[test]
fn test_unknown_commands_fall_through() {
    for cmd in ["hello world", "/unknown", "/forkabc", "fork abc", "/titlex"] {
        assert_not_consumed(cmd);
    }
}

// ── Intent correctness for key commands ──

#[test]
fn test_fork_produces_intent() {
    let mut app = test_app();
    app.try_frontend_command("/fork uuid-123");
    let intents = app.drain_intents();
    assert!(
        intents.iter().any(
            |i| matches!(i, AppIntent::ForkSession { target_uuid } if target_uuid == "uuid-123")
        )
    );
}

#[test]
fn test_rewind_produces_intent() {
    let mut app = test_app();
    app.try_frontend_command("/rewind uuid-456");
    let intents = app.drain_intents();
    assert!(intents.iter().any(
        |i| matches!(i, AppIntent::RewindSession { target_uuid } if target_uuid == "uuid-456")
    ));
}

#[test]
fn test_session_produces_intent() {
    let mut app = test_app();
    app.try_frontend_command("/session sess-abc");
    let intents = app.drain_intents();
    assert!(
        intents.iter().any(
            |i| matches!(i, AppIntent::ResumeSession { session_id } if session_id == "sess-abc")
        )
    );
}

#[test]
fn test_ss_alias_produces_intent() {
    let mut app = test_app();
    app.try_frontend_command("/ss sess-def");
    let intents = app.drain_intents();
    assert!(
        intents.iter().any(
            |i| matches!(i, AppIntent::ResumeSession { session_id } if session_id == "sess-def")
        )
    );
}

#[test]
fn test_model_command_opens_panel_instead_of_direct_switch() {
    // 4.6/4.7: `/model <name>` no longer switches directly — it opens the
    // panel exactly like `/model` (BREAKING).
    let mut app = test_app();
    app.model_sources = vec![model_group("p", &["gpt-4o"])];
    app.try_frontend_command("/model gpt-4o");
    assert!(app.model_panel.is_some(), "panel must open");
    assert_eq!(app.status.model, "unknown", "no direct model change");
    let intents = app.drain_intents();
    assert!(
        !intents
            .iter()
            .any(|i| matches!(i, AppIntent::UpdateSession(_))),
        "no direct switch request: {intents:?}"
    );
    assert!(intents.iter().any(|i| matches!(i, AppIntent::FetchModels)));
}

// ── /model panel (4.7) ──────────────────────────────────────

fn feed_models(app: &mut App, providers: Vec<wing_api_client::models::ProviderModels>) {
    let session_id = app.session_id.clone();
    app.handle_fetch_result(crate::app::intent::FetchResult {
        session_id,
        payload: crate::app::intent::FetchPayload::Models(
            wing_api_client::models::ModelsResponse { providers },
        ),
    });
}

#[test]
fn test_model_opens_panel_instantly_from_cache_and_preselects() {
    let mut app = test_app();
    app.model_sources = vec![model_group("p", &["m1", "m2"])];
    app.status.provider = Some("p".into());
    app.status.model = "m2".into();
    app.status.model_id = Some("m2".into());
    assert!(app.try_frontend_command("/model"));
    let panel = app.model_panel.as_ref().expect("panel opens from cache");
    assert_eq!(panel.current_page(), 0);
    assert_eq!(panel.cursor(), 1, "preselected the current model");
    assert_eq!(panel.committed_at(0), Some(1), "● on the current model");
    // Rendered in the transcript (like the Ask panel), not near the input.
    let rendered = picker_cell(&app).expect("picker cell shown in the chat");
    assert_eq!(rendered.cursor(), 1);
    let intents = app.drain_intents();
    assert!(
        intents.iter().any(|i| matches!(i, AppIntent::FetchModels)),
        "background refresh requested"
    );
}

#[test]
fn test_model_refused_while_working() {
    let mut app = test_app();
    app.model_sources = vec![model_group("p", &["m1"])];
    app.turn.working = true;
    assert!(app.try_frontend_command("/model"));
    assert!(app.model_panel.is_none(), "no panel while working");
    assert!(
        !app.drain_intents()
            .iter()
            .any(|i| matches!(i, AppIntent::FetchModels))
    );
    assert!(app.toast.is_some(), "refusal toast shown");
}

#[test]
fn test_model_panel_applies_the_id_of_the_selected_page() {
    // 同名模型跨 provider：id 各自独立，选中的是**当前 tab** 那一行的 id
    // （旧世界靠随行携带 provider 消歧；现在 id 本身就是消歧结果）。
    let mut app = test_app();
    app.model_sources = vec![
        model_group("dashscope", &["shared-a"]),
        model_group("dashscope-openai", &["shared-b"]),
    ];
    app.try_frontend_command("/model");
    app.drain_intents();
    app.handle_key(key(crossterm::event::KeyCode::Right)); // → second provider
    app.handle_key(key(crossterm::event::KeyCode::Enter)); // apply
    assert!(app.model_panel.is_none(), "panel closes on apply");
    assert!(
        picker_cell(&app).is_none(),
        "the transient picker cell disappears on apply"
    );
    let intents = app.drain_intents();
    assert!(
        intents.iter().any(|i| matches!(
            i,
            AppIntent::UpdateSession(SessionUpdate { model_id: Some(m), .. }) if m == "shared-b"
        )),
        "the selected row's id is required, got {intents:?}"
    );
}

#[test]
fn test_model_fetch_opens_panel_and_refreshes_in_place() {
    let mut app = test_app();
    assert!(app.try_frontend_command("/model"));
    assert!(app.model_panel.is_none(), "no cache → wait for the fetch");
    feed_models(&mut app, vec![model_group("p", &["m1", "m2"])]);
    assert_eq!(app.model_panel.as_ref().unwrap().page_count(), 1);
    assert!(
        picker_cell(&app).is_some(),
        "the fetched result shows the picker cell"
    );
    // Move the cursor, then refresh: page and cursor stay put.
    app.handle_key(key(crossterm::event::KeyCode::Down));
    assert_eq!(app.model_panel.as_ref().unwrap().cursor(), 1);
    assert_eq!(
        picker_cell(&app).unwrap().cursor(),
        1,
        "the cell mirrors the navigation"
    );
    feed_models(&mut app, vec![model_group("p", &["m1", "m2", "m3"])]);
    assert_eq!(
        app.model_panel.as_ref().unwrap().cursor(),
        1,
        "in-place refresh keeps the cursor"
    );
    assert_eq!(
        picker_cell(&app).unwrap().models().len(),
        3,
        "the open cell refreshes in place"
    );
}

#[test]
fn test_model_fetch_empty_shows_toast_and_no_panel() {
    let mut app = test_app();
    assert!(app.try_frontend_command("/model"));
    feed_models(&mut app, vec![]);
    assert!(app.model_panel.is_none());
    assert!(app.toast.is_some(), "empty result gives a hint");
}

#[test]
fn test_title_set_produces_intent() {
    let mut app = test_app();
    app.try_frontend_command("/title my project");
    let intents = app.drain_intents();
    assert!(intents.iter().any(
        |i| matches!(i, AppIntent::UpdateSession(SessionUpdate { title: Some(t), .. }) if t == "my project")
    ));
}

#[test]
fn test_bare_title_no_intent() {
    let mut app = test_app();
    app.try_frontend_command("/title");
    let intents = app.drain_intents();
    assert!(
        intents.is_empty(),
        "bare /title should only show toast, no intent"
    );
}

#[test]
fn test_bare_fork_no_intent() {
    let mut app = test_app();
    app.try_frontend_command("/fork");
    let intents = app.drain_intents();
    assert!(
        intents.is_empty(),
        "bare /fork should only show usage toast, no intent"
    );
}

// ── Composer pinning & scroll routing ────────────────────

#[test]
fn test_submit_jumps_to_bottom_and_queues_pending() {
    let mut app = test_app();
    app.chat.scroll_up(5); // reading history: auto_scroll=false
    app.input.set_text("hi");
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(
        app.chat.is_at_bottom(),
        "submit must pin the view to the bottom"
    );
    // Not committed to history yet — queued until the model accepts it.
    assert!(
        app.chat
            .cells
            .iter()
            .all(|c| !matches!(c.cell(), ChatCell::UserMessage(_))),
        "submitted message must not enter history before acceptance"
    );
    assert_eq!(app.chat.pending.len(), 1);
    assert!(matches!(
        app.chat.pending[0].cell.cell(),
        ChatCell::PendingUserMessage(s) if s.text == "hi"
    ));
}

#[test]
fn test_submit_intent_carries_pending_request_id() {
    let mut app = test_app();
    assert!(app.submit_message(SubmitPayload::text("hello")));

    assert_eq!(app.chat.pending.len(), 1);
    let pending_id = app.chat.pending[0].request_id.clone();

    let intents = app.drain_intents();
    let Some(AppIntent::SendMessage {
        request_id,
        tool_call_id,
        ..
    }) = intents.first()
    else {
        panic!("expected SendMessage intent");
    };
    assert_eq!(request_id, &pending_id);
    assert!(tool_call_id.is_none());
}

// ── The command table itself ────────────────────────────────────────────

/// Every command the router owns must be completable.
///
/// `/tips` 上线时漏掉的正是这条：路由表（`COMMANDS`）和补全兜底表
/// （`TUI_ONLY_COMMANDS`）是两份手写清单，新增一条命令只改前者，用户就永远
/// 看不到它 —— 表单靠人来同步迟早会漏，所以钉成断言。
#[test]
fn test_every_routed_command_is_offered_by_completion() {
    let offered = crate::ui::popup::command::tui_only_names();
    for route in commands::COMMANDS {
        let bare = route.name.trim_start_matches('/');
        assert!(
            offered.contains(&bare),
            "`{}` 能执行却不在补全表里（TUI_ONLY_COMMANDS 少了 `{bare}`）",
            route.name
        );
    }
}

#[test]
fn test_command_table_names_and_aliases_are_unique() {
    let mut seen = std::collections::BTreeSet::new();
    for route in commands::COMMANDS {
        assert!(seen.insert(route.name), "duplicate spelling {}", route.name);
        for alias in route.aliases {
            assert!(seen.insert(*alias), "duplicate spelling {alias}");
        }
    }
}

#[test]
fn test_command_table_rows_do_not_shadow_each_other() {
    // The table is matched in order, so a row that claims another row's
    // spelling would silently make that command unreachable.
    for (index, route) in commands::COMMANDS.iter().enumerate() {
        assert_eq!(
            commands::COMMANDS
                .iter()
                .position(|r| r.matches(route.name)),
            Some(index),
            "{} must resolve to its own row",
            route.name
        );
        for alias in route.aliases {
            assert_eq!(
                commands::COMMANDS.iter().position(|r| r.matches(alias)),
                Some(index),
                "{alias} must resolve to the row of {}",
                route.name
            );
        }
    }
}

#[test]
fn test_command_table_keeps_exact_and_argument_spellings_apart() {
    let find = |name: &str| {
        commands::COMMANDS
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} is in the table"))
    };

    // A command without arguments claims its exact spelling only: `/clear `
    // (or `/clear now`) is a plain message.
    let clear = find("/clear");
    assert!(clear.matches("/clear"));
    assert!(!clear.matches("/clear "));
    assert!(!clear.matches("/clear now"));

    // An argument-taking command claims both spellings, aliases included.
    let session = find("/session");
    assert!(session.matches("/session"));
    assert!(session.matches("/session sess-1"));
    assert!(session.matches("/ss sess-1"));
    assert!(!session.matches("/sessions sess-1"));
}

#[test]
fn test_every_table_row_is_reachable_through_the_router() {
    // The table is not a parallel list: each row is what the router actually
    // dispatches on, argument-taking rows included.
    for route in commands::COMMANDS {
        let mut app = test_app();
        let line = if route.takes_args {
            format!("{} ", route.name)
        } else {
            route.name.to_string()
        };
        assert!(
            app.try_frontend_command(&line),
            "'{line}' must be consumed by the router"
        );
    }
}

#[test]
fn test_stale_fetch_results_are_discarded() {
    use crate::app::intent::FetchPayload;
    use crate::app::intent::FetchResult;

    let mut app = test_app();
    app.handle_fetch_result(FetchResult {
        session_id: "some-other-session".into(),
        payload: FetchPayload::Toast {
            message: "boom".into(),
            is_error: true,
        },
    });
    assert!(
        app.toast.is_none(),
        "a result addressed to another session must not touch the UI"
    );

    app.handle_fetch_result(FetchResult {
        session_id: app.session_id.clone(),
        payload: FetchPayload::Toast {
            message: "hello".into(),
            is_error: false,
        },
    });
    assert!(app.toast.is_some(), "the current session's result lands");
}

/// Feed a `FetchPayload::Info` result into the app (the `/api/session/info`
/// projection path).
fn feed_info(app: &mut App, model: &str, display_name: Option<&str>) {
    feed_info_full(app, model, None, None, display_name, vec![]);
}

/// 同 [`feed_info`]，但可以带上会话标签（pin 星标的真值来源）。
fn feed_info_with_tags(app: &mut App, model: &str, display_name: Option<&str>, tags: Vec<String>) {
    feed_info_full(app, model, None, None, display_name, tags);
}

/// 完整的 info 夹具：调用名 + 引用词 + provider 事实 + 展示名 + 标签。
fn feed_info_full(
    app: &mut App,
    model: &str,
    model_id: Option<&str>,
    provider_name: Option<&str>,
    display_name: Option<&str>,
    tags: Vec<String>,
) {
    use crate::app::intent::FetchPayload;
    use crate::app::intent::FetchResult;
    use wing_api_client::models::ContextStatsInfo;
    use wing_api_client::models::SessionInfoResponse;

    let session_id = app.session_id.clone();
    app.handle_fetch_result(FetchResult {
        session_id,
        payload: FetchPayload::Info(Box::new(SessionInfoResponse {
            model: model.into(),
            model_id: model_id.map(str::to_string),
            provider_name: provider_name.map(str::to_string),
            model_display_name: display_name.map(str::to_string),
            api_url: "http://x".into(),
            tools: vec![],
            total_tokens: 0,
            context_window_tokens: 0,
            thinking: false,
            reasoning_effort: None,
            yolo: false,
            session_name: None,
            workdir: None,
            parent_session_id: None,
            status: "idle".into(),
            context_stats: ContextStatsInfo {
                message_count: 0,
                total_tokens: 0,
            },
            skills_info: String::new(),
            system_prompt: String::new(),
            tags,
            tag_meta: Default::default(),
            usage: None,
        })),
    });
}

#[test]
fn test_info_carries_the_pin_state_of_the_current_session() {
    // 星标的真值只从这里来（`/api/session/info` 的 tags）：赋值错了方向就反，
    // 点一下想 unpin 结果又 add（幂等自愈，但用户看到的是"没反应"）。
    let mut app = test_app();
    assert!(!app.status.pinned);

    feed_info_with_tags(&mut app, "m", None, vec!["pin".into()]);
    assert!(app.status.pinned, "pin 标签 → 星标点亮");

    // 别的标签不算 pin；unpin 后（标签消失）星标熄灭。
    feed_info_with_tags(&mut app, "m", None, vec!["favorite".into()]);
    assert!(!app.status.pinned, "只有裸 `pin` 才算置顶");
    feed_info_with_tags(&mut app, "m", None, vec!["pin".into()]);
    feed_info_with_tags(&mut app, "m", None, vec![]);
    assert!(!app.status.pinned, "标签被移除 → 星标熄灭");
}

#[test]
fn test_info_display_name_prefers_the_gateway_and_falls_back_locally() {
    let mut app = test_app();
    app.status.provider = Some("qoder".into());
    app.status.model = "dfmodel".into();
    app.model_sources = vec![model_group_with_labels(
        "qoder",
        &["dfmodel"],
        &[("dfmodel", "DeepSeek-Flash")],
    )];

    // Old gateway (no label field): the local snapshot keeps the label on
    // reconnect — resolved by the reference word when one is present.
    feed_info_full(&mut app, "dfmodel", Some("dfmodel"), None, None, vec![]);
    assert_eq!(
        app.status.model_display_name.as_deref(),
        Some("DeepSeek-Flash")
    );

    // Current gateway: the shipped value wins over the local snapshot, and the
    // provider fact comes straight from the response (no clearing hack).
    feed_info_full(
        &mut app,
        "dfmodel",
        Some("dfmodel"),
        Some("qoder"),
        Some("Gateway Label"),
        vec![],
    );
    assert_eq!(
        app.status.model_display_name.as_deref(),
        Some("Gateway Label")
    );
    assert_eq!(app.status.provider.as_deref(), Some("qoder"));

    // Undeclared model: no label is invented, the raw name is the fallback.
    feed_info(&mut app, "mystery", None);
    assert_eq!(app.status.model, "mystery");
    assert_eq!(app.status.model_display_name, None);
}

#[test]
fn test_info_carries_model_id_and_provider_without_a_clearing_hack() {
    // 模型变了 + info 带 provider_name → provider 直接进 status（旧实现在模型变化时
    // 清空 provider，因为 info 那时不带它；现在权威值随 info 一起到）。
    let mut app = test_app();
    app.status.model = "old-model".into();
    app.status.provider = Some("old-provider".into());
    app.status.model_id = Some("old-id".into());

    feed_info_full(
        &mut app,
        "dfmodel-2026",
        Some("ds-flash"),
        Some("qoder"),
        None,
        vec![],
    );
    assert_eq!(app.status.model, "dfmodel-2026");
    assert_eq!(app.status.model_id.as_deref(), Some("ds-flash"));
    assert_eq!(
        app.status.provider.as_deref(),
        Some("qoder"),
        "provider 是权威字段，不是被清空的旧缓存"
    );
}

// ── Session list: the tree order + the two-layer pin ──

/// `/session`（`/ss`）的候选缓存按**树序**排列（唯一实现
/// `shared::session_tree::build_forest`，与 `wing ps --tree` 共用）：子会话
/// 跟在父下面并带连接前缀，pin 的根连同子树整块上浮，子节点的 pin 只在兄弟
/// 组内说话。行序由每行自足的排序键（pin 时间 / 子树活跃 / id）决定。
///
/// 三条旧重排键仍逐一覆盖（它们都不再参与定序）：
/// - **workspace 匹配**：inactive 行的 workspace 正是启动目录，active 行的不是；
/// - **状态优先级**：payload 里更旧的 `waiting`（旧 rank 0）排在更新的 `idle`
///   （旧 rank 2）之后——只看 rank 会把它提到最前；
/// - **后端基准序**（active 优先）也不再是行序的来源。
mod session_list {
    use crate::app::App;
    use crate::app::intent::FetchPayload;
    use crate::app::intent::FetchResult;
    use crate::config::AppConfig;
    use crate::shared::pinning::PIN_TAG;
    use crate::ui::popup::selection::SelectionRow;
    use wing_api_client::models::SessionInfo;
    use wing_api_client::models::SessionListResponse;
    use wing_api_client::models::TagMeta;

    /// `pinned_at` 给出时带 `pin` 标签与对应记录。
    fn session(
        id: &str,
        name: &str,
        ws: &str,
        status: &str,
        at: &str,
        pinned_at: Option<&str>,
    ) -> SessionInfo {
        let tags = if pinned_at.is_some() {
            vec![PIN_TAG.to_string()]
        } else {
            vec![]
        };
        let tag_meta = pinned_at
            .map(|stamp| {
                [(
                    PIN_TAG.to_string(),
                    TagMeta {
                        added_at: Some(stamp.to_string()),
                    },
                )]
                .into_iter()
                .collect()
            })
            .unwrap_or_default();
        SessionInfo {
            id: id.into(),
            name: Some(name.into()),
            created_at: None,
            template_name: None,
            workspace: Some(ws.into()),
            parent_session_id: None,
            last_interaction: Some(at.into()),
            status: status.into(),
            tags,
            tag_meta,
            model_id: None,
            model_name: None,
            provider_name: None,
            model_display_name: None,
        }
    }

    fn app() -> App {
        App::new(
            "test-session".into(),
            AppConfig::default(),
            Some("/launch/project".into()),
        )
    }

    fn feed(app: &mut App, sessions: Vec<SessionInfo>) {
        let session_id = app.session_id.clone();
        app.handle_fetch_result(FetchResult {
            session_id,
            payload: FetchPayload::SessionList(SessionListResponse { sessions }),
        });
    }

    fn ids(app: &App) -> Vec<&str> {
        app.popup
            .cache
            .sessions
            .iter()
            .map(|c| c.id.as_str())
            .collect()
    }

    fn prefixes(app: &App) -> Vec<&str> {
        app.popup
            .cache
            .sessions
            .iter()
            .map(|c| c.tree_prefix.as_str())
            .collect()
    }

    /// 把 `s` 挂到 `parent` 名下（血缘：谁创建了我）。
    fn under(mut s: SessionInfo, parent: &str) -> SessionInfo {
        s.parent_session_id = Some(parent.into());
        s
    }

    #[test]
    fn test_flat_list_orders_by_subtree_activity_not_workspace_or_status() {
        let mut app = app();
        feed(
            &mut app,
            vec![
                // active（在内存里）且时间最新，但 workspace 与启动目录不匹配。
                session(
                    "active-new",
                    "current work",
                    "/elsewhere/project",
                    "idle",
                    "2025-06-01T00:00:00",
                    None,
                ),
                // 还在 active 组里但时间更旧：waiting 不享有状态优先级。
                session(
                    "active-old-waiting",
                    "waiting for me",
                    "/elsewhere/project",
                    "waiting",
                    "2025-01-01T00:00:00",
                    None,
                ),
                // inactive 的时间居中，workspace 正是启动目录。
                session(
                    "inactive-mid",
                    "old work",
                    "/launch/project",
                    "inactive",
                    "2025-03-01T00:00:00",
                    None,
                ),
            ],
        );

        assert_eq!(
            ids(&app),
            ["active-new", "inactive-mid", "active-old-waiting"],
            "无父子关系的列表按「最后交互降序」；workspace / status 都不参与"
        );
        assert_eq!(app.popup.cache.sessions[0].status, "idle");
        assert_eq!(
            app.popup.cache.sessions[0].last_interaction,
            "2025-06-01T00:00:00"
        );
        // workspace 随候选带下来（展示 + 搜索），但不参与排序。
        assert_eq!(app.popup.cache.sessions[1].workspace, "/launch/project");
        // 全是根 → 全不带前缀（平滑退化：看起来和树形之前一样）。
        assert_eq!(prefixes(&app), ["", "", ""]);
    }

    #[test]
    fn test_children_follow_their_parent_with_drawing_prefixes() {
        // A → B → G，且 B 之后还有兄弟 C：树序 + 连接前缀，
        // G 的祖先前缀里要保留 B 那一层的 `│` 延续线。
        let mut app = app();
        feed(
            &mut app,
            vec![
                session(
                    "r2",
                    "other root",
                    "/ws",
                    "idle",
                    "2025-06-01T00:00:00",
                    None,
                ),
                session("A", "the team", "/ws", "idle", "2025-01-01T00:00:00", None),
                under(
                    session(
                        "B",
                        "worker one",
                        "/ws",
                        "idle",
                        "2025-02-01T00:00:00",
                        None,
                    ),
                    "A",
                ),
                under(
                    session(
                        "C",
                        "worker two",
                        "/ws",
                        "idle",
                        "2025-03-01T00:00:00",
                        None,
                    ),
                    "A",
                ),
                under(
                    session(
                        "G",
                        "sub-worker",
                        "/ws",
                        "idle",
                        "2025-04-01T00:00:00",
                        None,
                    ),
                    "B",
                ),
            ],
        );

        assert_eq!(ids(&app), ["r2", "A", "B", "G", "C"]);
        assert_eq!(prefixes(&app), ["", "", "├─ ", "│  └─ ", "└─ "]);
    }

    #[test]
    fn test_a_pinned_root_floats_with_its_whole_subtree() {
        let mut app = app();
        feed(
            &mut app,
            vec![
                session(
                    "busy",
                    "recent work",
                    "/ws",
                    "idle",
                    "2025-06-01T00:00:00",
                    None,
                ),
                session(
                    "pinned",
                    "the team",
                    "/ws",
                    "inactive",
                    "2025-01-01T00:00:00",
                    Some("2025-05-01T09:00:00"),
                ),
                under(
                    session(
                        "worker",
                        "worker",
                        "/ws",
                        "idle",
                        "2025-02-01T00:00:00",
                        None,
                    ),
                    "pinned",
                ),
            ],
        );

        assert_eq!(
            ids(&app),
            ["pinned", "worker", "busy"],
            "pin 的根连同子树整块上浮（置顶区只对根开放）"
        );
        assert_eq!(prefixes(&app), ["", "└─ ", ""]);
    }

    #[test]
    fn test_a_pinned_child_leads_its_siblings_without_moving_the_parent() {
        // 子节点的 pin 只在兄弟组内说话：它排到兄弟组第一，但既不提升自己
        // （仍然在父下面），也不影响父节点在根组里的位置（父的子树活跃时间
        // 比根兄弟旧，pin 时间不算进父的排序键）。
        let mut app = app();
        feed(
            &mut app,
            vec![
                session("busy", "recent", "/ws", "idle", "2025-06-01T00:00:00", None),
                session("parent", "team", "/ws", "idle", "2025-01-01T00:00:00", None),
                under(
                    session(
                        "kid-pinned",
                        "kid",
                        "/ws",
                        "idle",
                        "2025-02-01T00:00:00",
                        Some("2025-05-01T09:00:00"),
                    ),
                    "parent",
                ),
                under(
                    session(
                        "kid-newer",
                        "newer kid",
                        "/ws",
                        "idle",
                        "2025-03-01T00:00:00",
                        None,
                    ),
                    "parent",
                ),
            ],
        );

        assert_eq!(ids(&app), ["busy", "parent", "kid-pinned", "kid-newer"]);
        assert_eq!(prefixes(&app), ["", "", "├─ ", "└─ "]);
    }

    #[test]
    fn test_a_ghost_parent_degrades_to_a_root() {
        // 父不在列表里（被删 / memory 后端重启 / 弱引用的野生值）→ 升为根，
        // 不挂死、不污染健康行。
        let mut app = app();
        feed(
            &mut app,
            vec![
                under(
                    session(
                        "orphan",
                        "lost child",
                        "/ws",
                        "idle",
                        "2025-06-01T00:00:00",
                        None,
                    ),
                    "ghost",
                ),
                session(
                    "healthy",
                    "fine",
                    "/ws",
                    "idle",
                    "2025-01-01T00:00:00",
                    None,
                ),
            ],
        );

        assert_eq!(ids(&app), ["orphan", "healthy"]);
        assert_eq!(prefixes(&app), ["", ""]);
    }

    #[test]
    fn test_the_search_state_flattens_the_rendered_rows() {
        // 全量（无过滤词）带树前缀；一旦输入过滤词，命中行平铺（前缀清空）——
        // 命中集合与树不同构，悬空的连接线只会误导。
        let mut app = app();
        feed(
            &mut app,
            vec![
                session("root", "root", "/ws", "idle", "2025-02-01T00:00:00", None),
                under(
                    session(
                        "kid",
                        "kid worker",
                        "/ws",
                        "idle",
                        "2025-01-01T00:00:00",
                        None,
                    ),
                    "root",
                ),
            ],
        );

        let panel_prefixes = |app: &mut App| -> Vec<String> {
            app.update_popup_from_input();
            let (rows, ..) = app.popup.active.render_data().expect("panel is open");
            rich_prefixes(rows)
        };
        app.input.set_text("/ss");
        assert_eq!(panel_prefixes(&mut app), ["", "└─ "]);

        app.input.set_text("/ss worker");
        assert_eq!(panel_prefixes(&mut app), [""]);
    }

    /// 面板行的树前缀（顺序 = 渲染顺序）。
    fn rich_prefixes(rows: &[SelectionRow]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                r.rich
                    .as_ref()
                    .expect("session rows are rich")
                    .tree_prefix
                    .clone()
            })
            .collect()
    }

    #[test]
    fn test_the_frame_draws_the_tree_block_with_aligned_second_lines() {
        // 端到端：把面板画进帧缓冲，读真正上了屏的行——树块成画（`├─` /
        // `│` / `└─`），行 2 的标题缩进与行 1 的图标列对齐。
        use super::{draw, test_terminal};

        let mut app = app();
        feed(
            &mut app,
            vec![
                session("A", "the team", "/ws", "idle", "2025-01-01T00:00:00", None),
                under(
                    session(
                        "B",
                        "worker one",
                        "/ws",
                        "idle",
                        "2025-02-01T00:00:00",
                        None,
                    ),
                    "A",
                ),
                under(
                    session(
                        "G",
                        "sub-worker",
                        "/ws",
                        "idle",
                        "2025-04-01T00:00:00",
                        None,
                    ),
                    "B",
                ),
                under(
                    session(
                        "C",
                        "worker two",
                        "/ws",
                        "idle",
                        "2025-03-01T00:00:00",
                        None,
                    ),
                    "A",
                ),
            ],
        );
        app.input.set_text("/ss");
        app.update_popup_from_input();

        // 32 行：开屏版式下牌子占 12 行、输入卡 3 行，剩下的空气要放得下整张
        // 会话列表（`MAX_VISIBLE_ROWS` = 8）。
        let mut term = test_terminal(60, 32);
        draw(&mut app, &mut term);
        let buf = term.backend().buffer();
        let rows: Vec<String> = (buf.area.y..buf.area.bottom())
            .map(|y| {
                (buf.area.x..buf.area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("no row contains {needle:?}:\n{}", frame_rows(&rows)))
        };

        // 树块：父在子上面，孙的祖先前缀里保留 `│` 延续线，C 是最后一个。
        // 时间标签唯一，用它定位行（前缀 `└─` 是 `│  └─` 的子串，不唯一）。
        let a = row_of("[01-01 00:00]");
        let b = row_of("[02-01 00:00]");
        let g = row_of("[04-01 00:00]");
        let c = row_of("[03-01 00:00]");
        assert!(
            a < b && b < g && g < c,
            "tree order:\n{}",
            frame_rows(&rows)
        );
        assert!(rows[a].contains("  ● /ws [01-01 00:00]"), "root: no prefix");
        assert!(rows[b].contains("├─  ● /ws [02-01 00:00]"));
        assert!(rows[g].contains("│  └─  ● /ws [04-01 00:00]"));
        assert!(rows[c].contains("└─  ● /ws [03-01 00:00]"));

        // 行 2 的对齐：标题列 = 图标列 + 前缀宽 + 1（根行照旧）。
        for (icon_row, title) in [(a, "the team"), (b, "worker one"), (g, "sub-worker")] {
            let icon_col = rows[icon_row].chars().take_while(|c| *c != '●').count();
            let title_row = row_of(title);
            let title_col = rows[title_row].chars().take_while(|c| *c == ' ').count();
            assert_eq!(
                title_col,
                icon_col + 3,
                "line 2 aligns under line 1:\n{}",
                frame_rows(&rows)
            );
        }
    }

    /// 行文本的调试视图（断言消息用）。
    fn frame_rows(rows: &[String]) -> String {
        rows.iter().map(|r| format!("|{r}|\n")).collect::<String>()
    }

    #[test]
    fn test_pinned_sessions_float_to_the_top_even_when_inactive() {
        let mut app = app();
        feed(
            &mut app,
            vec![
                session(
                    "active-new",
                    "current work",
                    "/ws",
                    "idle",
                    "2025-06-01T00:00:00",
                    None,
                ),
                session(
                    "inactive-pinned",
                    "pinned yesterday",
                    "/ws",
                    "inactive",
                    "2025-01-01T00:00:00",
                    Some("2025-05-01T09:00:00"),
                ),
            ],
        );

        assert_eq!(
            ids(&app),
            ["inactive-pinned", "active-new"],
            "pin 是前端的第一排序键：即使 inactive 也置顶"
        );
        assert!(app.popup.cache.sessions[0].pinned);
    }

    /// pin 标签在、但 `tag_meta` 里没有对应记录（时间未知的真样本）。
    fn pinned_without_record(mut s: SessionInfo) -> SessionInfo {
        s.tags = vec![PIN_TAG.to_string()];
        s.tag_meta.clear();
        s
    }

    #[test]
    fn test_later_pins_sort_earlier_and_unknown_times_sort_last() {
        let mut app = app();
        feed(
            &mut app,
            vec![
                session(
                    "old-pin",
                    "old",
                    "/ws",
                    "idle",
                    "2025-01-01T00:00:00",
                    Some("2025-01-02T00:00:00"),
                ),
                session(
                    "loose-pin",
                    "hand-tagged",
                    "/ws",
                    "idle",
                    "2025-01-01T00:00:00",
                    Some("nonsense"),
                ),
                session(
                    "new-pin",
                    "new",
                    "/ws",
                    "idle",
                    "2025-01-01T00:00:00",
                    Some("2025-06-02T00:00:00"),
                ),
                pinned_without_record(session(
                    "bare-pin",
                    "no record",
                    "/ws",
                    "idle",
                    "2025-01-01T00:00:00",
                    None,
                )),
            ],
        );

        // 有时间的按时间降序；时间未知 / 不可解析的 pin 排在其后，两者并列
        // （活跃度同分）时按 id 升序（`build_forest` 的确定性 tie-break）。
        assert_eq!(ids(&app), ["new-pin", "old-pin", "bare-pin", "loose-pin"]);
    }

    #[test]
    fn test_refresh_reselects_the_row_by_id() {
        let mut app = app();
        feed(
            &mut app,
            vec![
                session("first", "one", "/ws", "idle", "2025-01-02T00:00:00", None),
                session("second", "two", "/ws", "idle", "2025-01-01T00:00:00", None),
            ],
        );
        // 树序（时间降序）把更旧的 second 放在第二行。
        assert_eq!(ids(&app), ["first", "second"]);
        app.input.set_text("/ss");
        app.update_popup_from_input();
        app.popup.active.move_down(); // 用户把光标移到第二行
        assert_eq!(app.popup.active.selected_name(), Some("second"));

        // 刷新响应：行序翻转（second 被 pin）——光标跟着 id 走。
        feed(
            &mut app,
            vec![
                session("first", "one", "/ws", "idle", "2025-01-02T00:00:00", None),
                session(
                    "second",
                    "two",
                    "/ws",
                    "idle",
                    "2025-01-01T00:00:00",
                    Some("2025-07-01T00:00:00"),
                ),
            ],
        );
        assert_eq!(ids(&app), ["second", "first"]);
        assert_eq!(
            app.popup.active.selected_name(),
            Some("second"),
            "刷新后光标按 session id 重定位，不留在旧下标上"
        );
    }
}

// ── The `/ss` panel refreshes on open (#147) ──

#[test]
fn test_opening_the_session_panel_refreshes_the_cached_list_once() {
    use crate::app::intent::AppIntent;
    use crate::ui::popup::command::SessionCandidate;

    let mut app = test_app();
    app.popup.cache.sessions = vec![SessionCandidate {
        id: "s1".into(),
        title: "Test".into(),
        workspace: "/tmp".into(),
        status: "idle".into(),
        last_interaction: "2025-01-01T00:00:00Z".into(),
        pinned: false,
        tree_prefix: String::new(),
    }];
    app.popup.cache.sessions_fetched = true;

    // 进入面板：即使有缓存也刷新一次（缓存只负责"先渲染旧行"）。
    app.input.set_text("/ss");
    app.update_popup_from_input();
    assert!(
        matches!(
            app.drain_intents().as_slice(),
            [AppIntent::FetchSessionList]
        ),
        "opening the panel must ask for a fresh list"
    );

    // 面板已开：继续打字（过滤词）不再触发请求。
    app.input.set_text("/ss work");
    app.update_popup_from_input();
    assert!(
        app.drain_intents().is_empty(),
        "a keystroke inside the open panel must not refetch"
    );

    // 关闭再打开：新的一次打开 = 新的一次刷新。
    app.input.clear();
    app.update_popup_from_input();
    app.input.set_text("/ss");
    app.update_popup_from_input();
    assert!(
        matches!(
            app.drain_intents().as_slice(),
            [AppIntent::FetchSessionList]
        ),
        "reopening the panel refreshes again"
    );
}

#[test]
fn test_the_refresh_response_does_not_trigger_another_request() {
    use crate::ui::popup::command::SessionCandidate;
    use wing_api_client::models::SessionListResponse;

    let mut app = test_app();
    app.popup.cache.sessions = vec![SessionCandidate {
        id: "s1".into(),
        title: "Test".into(),
        workspace: "/tmp".into(),
        status: "idle".into(),
        last_interaction: "2025-01-01T00:00:00Z".into(),
        pinned: false,
        tree_prefix: String::new(),
    }];
    app.popup.cache.sessions_fetched = true;
    app.input.set_text("/ss");
    app.update_popup_from_input();
    assert_eq!(app.drain_intents().len(), 1);

    // 响应路径（update_popup，非输入路径）原地换行：不得再发一次请求。
    let session_id = app.session_id.clone();
    app.handle_fetch_result(crate::app::intent::FetchResult {
        session_id,
        payload: crate::app::intent::FetchPayload::SessionList(SessionListResponse {
            sessions: vec![],
        }),
    });
    assert!(
        app.drain_intents().is_empty(),
        "the refresh response must land in place, not spawn another fetch"
    );
}

#[test]
fn test_opening_the_panel_with_an_empty_cache_requests_the_list_once() {
    use crate::app::intent::AppIntent;
    use wing_api_client::models::SessionListResponse;

    let mut app = test_app();
    assert!(!app.popup.cache.has_sessions());

    // 等待首帧列表的路径自己发请求（一次）。
    app.input.set_text("/ss");
    app.update_popup_from_input();
    assert!(matches!(
        app.drain_intents().as_slice(),
        [AppIntent::FetchSessionList]
    ));

    // 列表到达 → 原地建面板；响应路径不得再次请求。
    let session_id = app.session_id.clone();
    app.handle_fetch_result(crate::app::intent::FetchResult {
        session_id,
        payload: crate::app::intent::FetchPayload::SessionList(SessionListResponse {
            sessions: vec![],
        }),
    });
    let intents = app.drain_intents();
    assert!(intents.is_empty(), "unexpected intents: {intents:?}");
}

// ── The streaming guard vs. a panel's first frame ──

#[test]
fn test_a_panels_first_fetch_survives_a_running_turn() {
    // 回归（集成测试发现 + bot 复核补上 `/session` 全拼写）：流式守卫曾把
    // "等待首帧候选"的那条 fetch 一并丢掉——那条请求是**打开面板的前提**，
    // 而没有任何路径会在 turn 结束时补发（响应路径挂在 fetch 结果上，不挂在
    // turn 状态上），于是"agent 干活时敲 /ss（缓存空）"面板永远停在占位上，
    // 直到用户再敲一个键。
    //
    // 四个入口逐一锚定：等待态的可见性各不相同（`/ss` 的占位列表无行，
    // `/session`、`/fork`、`/agents` 画得出一条命令占位行），这正是"按有没有
    // 画出来做判据"会在全拼写上失手的原因。
    use crate::app::intent::AppIntent;

    type Matcher = fn(&AppIntent) -> bool;
    let cases: [(&str, Matcher); 4] = [
        ("/ss", |intent| {
            matches!(intent, AppIntent::FetchSessionList)
        }),
        ("/session", |intent| {
            matches!(intent, AppIntent::FetchSessionList)
        }),
        ("/fork ", |intent| {
            matches!(intent, AppIntent::FetchBranches)
        }),
        ("/agents ", |intent| {
            matches!(intent, AppIntent::FetchAgents)
        }),
    ];
    for (input, expected) in cases {
        let mut app = test_app();
        app.turn.working = true;
        assert!(!app.popup.cache.sessions_fetched, "前提：缓存是冷的");

        app.input.set_text(input);
        app.update_popup_from_input();
        let intents = app.drain_intents();
        assert_eq!(
            intents.len(),
            1,
            "{input}: 首帧请求必须发出去（流式不是把它丢掉的理由），got {intents:?}"
        );
        assert!(
            expected(&intents[0]),
            "{input}: 请求了错误的资源 {intents:?}"
        );
    }
}

#[test]
fn test_a_panel_refresh_still_waits_for_an_idle_agent() {
    // 反向锚定：有缓存时面板照常渲染旧行，但"打开即刷新"（#147）不在
    // turn 进行中发请求——刷新**不排队**（不补发），下次打开面板自然会再刷。
    use crate::ui::popup::ActivePopup;
    use crate::ui::popup::command::SessionCandidate;

    let mut app = test_app();
    app.popup.cache.sessions = vec![SessionCandidate {
        id: "s1".into(),
        title: "Test".into(),
        workspace: "/tmp".into(),
        status: "idle".into(),
        last_interaction: "2025-01-01T00:00:00Z".into(),
        pinned: false,
        tree_prefix: String::new(),
    }];
    app.popup.cache.sessions_fetched = true;
    app.turn.working = true;

    app.input.set_text("/ss");
    app.update_popup_from_input();
    assert!(
        matches!(app.popup.active, ActivePopup::SubCommand { .. }),
        "有缓存的面板照常弹出（用旧行）"
    );
    assert!(
        app.drain_intents().is_empty(),
        "agent 忙碌时不刷新——刷新留到下一次打开"
    );
}
