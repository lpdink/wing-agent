//! Shared fixtures for the app test modules.
//!
//! Moved out of `app/mod.rs` when the lanes moved out of the composition
//! root: the assertions are unchanged, only their file changed.

use crate::app::App;
use crate::config::AppConfig;
use crate::protocol::EventMeta;
use crate::protocol::SessionStatus;
use crate::protocol::WingEvent;
use crate::shared::panels::ask::AskPanel;
use crate::shared::panels::ask::AskPayload;
use crate::shared::panels::picker::ModelPanel;
use crate::ui::chat_view::ChatCell;

/// Create a minimal App for command dispatch testing.
pub(super) fn test_app() -> App {
    App::new("test-session".into(), AppConfig::default(), None)
}

/// Build a SyncSession event for the test session.
///
/// The `status` is derived the way a healthy backend derives it (the snapshot
/// never contradicts the projections: content ⇒ a turn is in flight) — fixture
/// convenience, not a product rule: the frontends read the status and never
/// infer it. Tests that care about a specific status call
/// [`sync_event_with_status`].
pub(super) fn sync_event(
    messages: Vec<serde_json::Value>,
    uncommitted: Option<serde_json::Value>,
    uncommitted_tools: Vec<serde_json::Value>,
    events: Vec<serde_json::Value>,
    turn_started_at: Option<String>,
) -> WingEvent {
    let status = if uncommitted.is_some() || !uncommitted_tools.is_empty() {
        SessionStatus::Working
    } else {
        SessionStatus::Idle
    };
    sync_event_with_status(
        status,
        messages,
        uncommitted,
        uncommitted_tools,
        events,
        turn_started_at,
    )
}

/// Build a SyncSession event with an explicit `status`.
pub(super) fn sync_event_with_status(
    status: SessionStatus,
    messages: Vec<serde_json::Value>,
    uncommitted: Option<serde_json::Value>,
    uncommitted_tools: Vec<serde_json::Value>,
    events: Vec<serde_json::Value>,
    turn_started_at: Option<String>,
) -> WingEvent {
    WingEvent::SyncSession {
        session_id: "test-session".into(),
        messages,
        uncommitted,
        uncommitted_tools,
        events,
        status,
        turn_started_at,
        agent: None,
        name: None,
        draft: None,
        usage: None,
        meta: EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: Some("test-session".into()),
            request_id: "r".into(),
        },
    }
}

/// Provider group whose models have `id == name` (the存量配置 shape).
pub(super) fn model_group(
    provider: &str,
    models: &[&str],
) -> wing_api_client::models::ProviderModels {
    use wing_api_client::models::ModelDetail;
    wing_api_client::models::ProviderModels {
        provider: provider.into(),
        models: models
            .iter()
            .map(|m| ModelDetail {
                id: (*m).to_string(),
                name: (*m).to_string(),
                display_name: None,
                description: None,
                context_window_tokens: None,
                keep_recent_tokens: None,
                capabilities: Default::default(),
            })
            .collect(),
    }
}

/// Provider group carrying `display_name` declarations for the given model ids
/// — the display layer's input (the picker renders its rows from it, and the
/// label fallbacks resolve against it).
pub(super) fn model_group_with_labels(
    provider: &str,
    models: &[&str],
    labels: &[(&str, &str)],
) -> wing_api_client::models::ProviderModels {
    use wing_api_client::models::ModelDetail;
    wing_api_client::models::ProviderModels {
        provider: provider.into(),
        models: models
            .iter()
            .map(|id| {
                let label = labels
                    .iter()
                    .find(|(model, _)| model == id)
                    .map(|(_, label)| (*label).to_string());
                ModelDetail {
                    id: (*id).to_string(),
                    name: (*id).to_string(),
                    display_name: label,
                    description: None,
                    context_window_tokens: None,
                    keep_recent_tokens: None,
                    capabilities: Default::default(),
                }
            })
            .collect(),
    }
}

pub(super) fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

/// The rendered picker cell's panel snapshot, if the cell is present.
pub(super) fn picker_cell(app: &App) -> Option<&ModelPanel> {
    app.chat.cells.iter().find_map(|c| match c.cell() {
        ChatCell::ModelPicker(panel) => Some(panel),
        _ => None,
    })
}

/// A normalized required-choice ask panel — the shape the retired Bash
/// confirmation normalizes into. Built through the same entry the app uses, so
/// the tests exercise the real model.
pub(super) fn required_choice_panel(tool_call_id: &str, choices: &[&str]) -> AskPanel {
    let choices: Vec<String> = choices.iter().map(|c| (*c).to_string()).collect();
    AskPanel::from_ask(AskPayload {
        tool_call_id,
        questions: &[],
        question: "Proceed?",
        choices: &choices,
        required: true,
    })
}

/// The Bash dangerous-command confirmation's option set.
pub(super) fn yes_no_yolo() -> [&'static str; 3] {
    ["y", "n", "yolo"]
}

pub(super) fn wheel(kind: crossterm::event::MouseEventKind) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column: 12,
        row: 4,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }
}

pub(super) fn wheel_up() -> crossterm::event::MouseEvent {
    wheel(crossterm::event::MouseEventKind::ScrollUp)
}

pub(super) fn wheel_down() -> crossterm::event::MouseEvent {
    wheel(crossterm::event::MouseEventKind::ScrollDown)
}

/// A `TestBackend` terminal: assertions read the very frame the app drew,
/// which is what the highlight and the copy source are made of.
pub(super) fn test_terminal(
    width: u16,
    height: u16,
) -> ratatui::Terminal<ratatui::backend::TestBackend> {
    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("test terminal")
}

pub(super) fn draw(app: &mut App, terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>) {
    app.draw(terminal).expect("draw");
}

pub(super) fn mouse_at(
    kind: crossterm::event::MouseEventKind,
    (column, row): (u16, u16),
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    }
}

pub(super) fn press(at: (u16, u16)) -> crossterm::event::MouseEvent {
    mouse_at(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        at,
    )
}

pub(super) fn drag(at: (u16, u16)) -> crossterm::event::MouseEvent {
    mouse_at(
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        at,
    )
}

pub(super) fn release(at: (u16, u16)) -> crossterm::event::MouseEvent {
    mouse_at(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        at,
    )
}

pub(super) fn hover(at: (u16, u16)) -> crossterm::event::MouseEvent {
    mouse_at(crossterm::event::MouseEventKind::Moved, at)
}

/// Declare the last frame's chat band **height**.
///
/// Surrogate for the removed `visible_height` field: the tests below drive the
/// wheel and the page keys without rendering a frame, and only the band's
/// height takes part in those paths. The band is recorded zero-wide, so no
/// pointer position can be claimed by a frame that was never drawn.
pub(super) fn set_chat_height(app: &mut App, height: u16) {
    app.geometry
        .record_chat_band(ratatui::layout::Rect::new(0, 0, 0, height));
}

/// App whose chat holds one user message and no header, so the rendered
/// rows are known: row 1 of the chat band holds "hello world" at column 2
/// (the user cell insets its text by two columns, one padding row on top).
pub(super) fn app_with_message() -> App {
    let mut app = test_app();
    app.clear_welcome();
    app.chat.push(ChatCell::UserMessage("hello world".into()));
    app
}

pub(super) fn reversed_cells(
    terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
) -> Vec<(u16, u16)> {
    let buf = terminal.backend().buffer();
    let mut cells = Vec::new();
    for y in buf.area.y..buf.area.bottom() {
        for x in buf.area.x..buf.area.right() {
            if buf[(x, y)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
            {
                cells.push((x, y));
            }
        }
    }
    cells
}

/// 打开 `/ss` 的会话候选面板，灌入 `count` 条双行（rich）候选 —— 策略窗口
/// 6 条 = 12 行，比任何「输入卡下面只剩几行」的版式都高。
pub(super) fn open_session_popup(app: &mut App, count: usize) {
    use crate::ui::popup::command::SessionCandidate;
    app.popup.cache.sessions = (0..count)
        .map(|i| SessionCandidate {
            id: format!("sess-{i:02}"),
            title: format!("session-{i:02}"),
            workspace: "/ws/wing".into(),
            status: "idle".into(),
            last_interaction: "2026-01-01T00:00:00".into(),
            pinned: false,
            tree_prefix: String::new(),
        })
        .collect();
    app.popup.cache.sessions_fetched = true;
    app.input.set_text("/ss");
    app.update_popup_from_input();
}

/// 把弹层的选中位走到列表末尾（`Down` 在末条会绕回第一条，所以走 `count - 1` 步）。
pub(super) fn walk_popup_to_the_end(app: &mut App, steps: usize) {
    for _ in 0..steps {
        app.popup.active.move_down();
    }
}

/// 弹层当前选中项应当出现在帧上的那段文本（rich 行是标题，普通行是名字）。
pub(super) fn popup_selection_label(app: &App) -> String {
    let (rows, state, _) = app.popup.active.render_data().expect("popup");
    let row = &rows[state.selected];
    row.rich
        .as_ref()
        .map_or_else(|| row.name.clone(), |rich| rich.title.clone())
}

/// App with a draft in the composer, no chat header: the draft is the only
/// content the chat lacks, so a frame assertion reads the card alone.
/// [`composer_text`] returns the cell the draft starts at (`❯ ` is left of
/// it, the card's border left of that).
pub(super) fn app_with_draft(draft: &str) -> App {
    let mut app = test_app();
    app.clear_welcome();
    app.input.set_text(draft);
    app
}

/// App with content taller than the band (30 lines), so scrolling and
/// auto-scroll have somewhere to go.
pub(super) fn app_with_tall_message() -> App {
    let mut app = test_app();
    app.clear_welcome();
    let text = (0..30)
        .map(|i| format!("line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.chat.push(ChatCell::UserMessage(text.into()));
    app
}

/// The composer's text area in the last drawn frame: `(x, y, rows)`.
///
/// The card's frame is not text: a pointer on it is inert and the draft never
/// lives there, so tests that click or type into the composer address this —
/// the card's own rect stays `InputArea::rendered_area`.
pub(super) fn composer_text(app: &App) -> (u16, u16, u16) {
    let chrome = crate::ui::input_area::chrome::Chrome::of(app.input.rendered_area());
    let (x, y) = app.input.text_origin();
    (x, y, chrome.text_rows)
}

/// The whole frame as text, one numbered row per line (assertion messages).
pub(super) fn frame_text(buf: &ratatui::buffer::Buffer) -> String {
    (buf.area.y..buf.area.bottom())
        .map(|row| {
            let line: String = (buf.area.x..buf.area.right())
                .map(|x| buf[(x, row)].symbol())
                .collect();
            format!("{row:>2} |{line}|\n")
        })
        .collect()
}
