//! 会话用量基线测试——SyncSession 快照是状态栏累计的唯一基线来源。
//!
//! 口径：基线来源只能是 `sync_session.usage`（与快照同一次取值、同一条有序
//! WS 通道）。HTTP `/api/session/info` 与 WS 帧无跨通道定序——快照可能已包含
//! 尚在途的事件，用它做基线就是重复计数。这里的断言把这两条钉住：
//! attach / 切换 / 恢复都以快照**赋值**（不是累加），其后的 live 事件加增量。

use super::support::test_app;
use crate::app::*;
use crate::protocol::EventMeta;
use crate::protocol::SessionStatus;
use crate::protocol::WingEvent;
use wing_api_client::models::SessionUsage;

fn meta(session_id: &str) -> EventMeta {
    EventMeta {
        created_at: "2026-01-01T00:00:00+00:00".into(),
        session_id: Some(session_id.into()),
        request_id: "r".into(),
    }
}

fn sync_with(session_id: &str, usage: Option<SessionUsage>) -> WingEvent {
    WingEvent::SyncSession {
        session_id: session_id.into(),
        messages: vec![],
        uncommitted: None,
        uncommitted_tools: vec![],
        events: vec![],
        status: SessionStatus::Idle,
        turn_started_at: None,
        agent: None,
        name: None,
        draft: None,
        usage,
        meta: meta(session_id),
    }
}

fn metrics(prompt: i64, completion: i64, cached: i64) -> WingEvent {
    WingEvent::LlmCallMetrics {
        model: "m".into(),
        prompt_tokens: prompt,
        completion_tokens: completion,
        cached_tokens: cached,
        first_chunk_rt_ms: 1.0,
        tokens_per_sec: 1.0,
        stop_reason: None,
        meta: meta("test-session"),
    }
}

fn usage(prompt: i64, completion: i64, cached: i64) -> SessionUsage {
    SessionUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        cached_tokens: cached,
        ..SessionUsage::default()
    }
}

fn counters(app: &App) -> (i64, i64, i64) {
    (
        app.status.session_prompt_tokens,
        app.status.session_completion_tokens,
        app.status.session_cached_tokens,
    )
}

/// 退出素材取的是**本地**事实：会话 id / 模型标签 / 墙钟 / 状态栏累计。
#[test]
fn tui_exit_collects_the_local_exit_material() {
    let mut app = test_app();
    app.status.model = "dfmodel-2026".into();
    app.status.model_display_name = Some("DeepSeek-Flash".into());
    app.status.session_prompt_tokens = 120_500;
    app.status.session_completion_tokens = 4_200;
    app.status.session_cached_tokens = 80_000;

    let exit = TuiExit::from_app(&app, std::time::Duration::from_secs(754));

    assert_eq!(exit.session_id, "test-session");
    assert_eq!(exit.model, "DeepSeek-Flash", "展示名优先于调用名");
    assert_eq!(exit.wall, std::time::Duration::from_secs(754));
    assert_eq!(exit.prompt_tokens, 120_500);
    assert_eq!(exit.completion_tokens, 4_200);
    assert_eq!(exit.cached_tokens, 80_000);
}

/// 恢复的会话不再从 0 累起：快照基线 + 其后的 live 增量。
#[test]
fn sync_baselines_the_counters_and_live_events_add_on_top() {
    let mut app = test_app();
    assert_eq!(counters(&app), (0, 0, 0));

    app.handle_event(sync_with("test-session", Some(usage(100, 20, 40))));
    assert_eq!(counters(&app), (100, 20, 40), "resume 以持久值为基线");

    app.handle_event(metrics(7, 3, 2));
    assert_eq!(counters(&app), (107, 23, 42), "live 事件在基线之上累加");
}

/// 重连 / 再次 resume：新快照**替换**基线，绝不与旧值相加。
#[test]
fn a_fresh_snapshot_replaces_the_baseline_instead_of_adding_to_it() {
    let mut app = test_app();
    app.handle_event(sync_with("test-session", Some(usage(100, 20, 40))));
    app.handle_event(metrics(7, 3, 2));
    assert_eq!(counters(&app), (107, 23, 42));

    // 服务端账本此刻已包含那 7/3/2（+ 新的一轮），快照整份替换。
    app.handle_event(sync_with("test-session", Some(usage(150, 30, 60))));
    assert_eq!(counters(&app), (150, 30, 60), "快照是权威值，不是增量");
}

/// 旧网关 / 无记录：快照不带 usage → 零基线，不是上一个会话的残留值。
#[test]
fn a_switch_without_usage_zeroes_the_counters() {
    let mut app = test_app();
    app.handle_event(sync_with("test-session", Some(usage(100, 20, 40))));
    assert_eq!(counters(&app), (100, 20, 40));

    app.handle_event(sync_with("other-session", None));
    assert_eq!(app.session_id, "other-session");
    assert_eq!(counters(&app), (0, 0, 0), "换会话不夹带上一个会话的累计");
}
