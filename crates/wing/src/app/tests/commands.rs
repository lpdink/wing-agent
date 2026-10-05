//! Command routing lane tests — the table, its handlers, fetch results.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use super::support::*;
use crate::app::commands;
use crate::app::*;
use crate::shared::panels::SelectionPanel;
use crate::ui::chat_view::ChatCell;

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
            .any(|i| matches!(i, AppIntent::UpdateSession { .. })),
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
fn test_model_panel_applies_explicit_provider_for_same_name_model() {
    // Regression: two providers expose the same model name — applying the
    // second one must carry the SECOND provider explicitly.
    let mut app = test_app();
    app.model_sources = vec![
        model_group("dashscope", &["shared"]),
        model_group("dashscope-openai", &["shared"]),
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
            AppIntent::UpdateSession { model: Some(m), provider: Some(p), .. }
                if m == "shared" && p == "dashscope-openai"
        )),
        "explicit provider required, got {intents:?}"
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
        |i| matches!(i, AppIntent::UpdateSession { title: Some(t), .. } if t == "my project")
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
        ChatCell::PendingUserMessage(s) if s == "hi"
    ));
}

#[test]
fn test_submit_intent_carries_pending_request_id() {
    let mut app = test_app();
    assert!(app.submit_message("hello"));

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
    use crate::app::intent::FetchPayload;
    use crate::app::intent::FetchResult;
    use wing_api_client::models::ContextStatsInfo;
    use wing_api_client::models::SessionInfoResponse;

    let session_id = app.session_id.clone();
    app.handle_fetch_result(FetchResult {
        session_id,
        payload: FetchPayload::Info(Box::new(SessionInfoResponse {
            model: model.into(),
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
            status: "idle".into(),
            context_stats: ContextStatsInfo {
                message_count: 0,
                total_tokens: 0,
            },
            skills_info: String::new(),
            system_prompt: String::new(),
        })),
    });
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

    // Old gateway (no field): the local snapshot keeps the label on reconnect.
    feed_info(&mut app, "dfmodel", None);
    assert_eq!(
        app.status.model_display_name.as_deref(),
        Some("DeepSeek-Flash")
    );

    // Current gateway: the shipped value wins over the local snapshot.
    feed_info(&mut app, "dfmodel", Some("Gateway Label"));
    assert_eq!(
        app.status.model_display_name.as_deref(),
        Some("Gateway Label")
    );

    // Undeclared model: no label is invented, the raw name is the fallback.
    feed_info(&mut app, "mystery", None);
    assert_eq!(app.status.model, "mystery");
    assert_eq!(app.status.model_display_name, None);
}

// ── Session list: the backend owns the order ──

/// `/session`（`/ss`）的候选缓存只做投影：顺序 = 后端 `/api/session/list` 的顺序。
///
/// 断言用的是「旧代码会重排」的形状，三条重排键逐一覆盖：
/// - **workspace 匹配**：inactive 行的 workspace 正是启动目录，active 行的不是；
/// - **状态优先级**：payload 里更旧的 `waiting`（旧 rank 0）排在更新的 `idle`
///   （旧 rank 2）之后——只看 rank 会把它提到最前；
/// - **组合**：旧的全键 `(ws_mismatch, rank)` 给出的顺序与 payload 完全相反。
#[test]
fn test_session_list_keeps_the_backend_order() {
    use crate::app::intent::FetchPayload;
    use crate::app::intent::FetchResult;
    use wing_api_client::models::SessionInfo;
    use wing_api_client::models::SessionListResponse;

    let session = |id: &str, name: &str, ws: &str, status: &str, at: &str| SessionInfo {
        id: id.into(),
        name: Some(name.into()),
        created_at: None,
        template_name: None,
        workspace: Some(ws.into()),
        last_interaction: Some(at.into()),
        status: status.into(),
    };

    let payload = SessionListResponse {
        sessions: vec![
            // active（在内存里）且时间最新，但 workspace 与启动目录不匹配。
            session(
                "active-new",
                "current work",
                "/elsewhere/project",
                "idle",
                "2025-06-01T00:00:00",
            ),
            // 还在 active 组里但时间更旧：waiting 不享有状态优先级。
            session(
                "active-old-waiting",
                "waiting for me",
                "/elsewhere/project",
                "waiting",
                "2025-01-01T00:00:00",
            ),
            // inactive 的时间居中，workspace 正是启动目录。
            session(
                "inactive-mid",
                "old work",
                "/launch/project",
                "inactive",
                "2025-03-01T00:00:00",
            ),
        ],
    };

    let mut app = App::new(
        "test-session".into(),
        AppConfig::default(),
        Some("/launch/project".into()),
    );
    let session_id = app.session_id.clone();
    app.handle_fetch_result(FetchResult {
        session_id,
        payload: FetchPayload::SessionList(payload),
    });

    let ids: Vec<&str> = app
        .popup
        .cache
        .sessions
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["active-new", "active-old-waiting", "inactive-mid"],
        "the popup renders the payload verbatim — no workspace/status re-ranking"
    );
    assert_eq!(app.popup.cache.sessions[0].status, "idle");
    assert_eq!(
        app.popup.cache.sessions[0].last_interaction,
        "2025-06-01T00:00:00"
    );
    // workspace 仍然随候选带下来（搜索用），只是不再参与渲染与排序。
    assert_eq!(app.popup.cache.sessions[2].workspace, "/launch/project");
}
