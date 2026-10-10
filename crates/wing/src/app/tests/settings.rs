//! 设置面板的 App 接线测试（10 步骤）。
//!
//! 覆盖：模态优先级（AD2）· ask 排队 · overlay 的绘制落点与三者处置
//! （design §20 风险 13 的 `needs_full_redraw` / `images.invalidate()` 配对）·
//! 实时预览与快照 · 一键保存两边的四种组合 · 轮次中拒绝重启 ·
//! `settings_changed` 的指纹比对 · 命令表别名。
//!
//! 夹具是自带的（07 / 08 的 `test_support` 是各自包私有的）：一棵小目录 + 一份稀疏文档，
//! 够走完「搜索定位 → 编辑提交 → 预览 / 保存」的真实按键路径。

use std::collections::HashMap;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use serde_json::Value;
use serde_json::json;
use wing_api_client::models::ApplyScope;
use wing_api_client::models::SecretPresence;
use wing_api_client::models::SecretState;
use wing_api_client::models::SettingChoice;
use wing_api_client::models::SettingGroup;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;
use wing_api_client::models::SettingProblem;
use wing_api_client::models::SettingsGetResponse;
use wing_api_client::models::SettingsSchemaResponse;
use wing_api_client::models::SettingsSetResponse;

use super::images::TempDir;
use super::images::app_with_images;
use super::images::draw_until;
use super::images::has_placeholder;
use super::images::kitty;
use super::images::write_png;
use super::support::frame_text;
use super::support::hover;
use super::support::press as mouse_press;
use super::support::sync_event;
use super::support::test_app;
use super::support::test_terminal;
use crate::app::App;
use crate::app::AppIntent;
use crate::app::modal::ChatScrollAction;
use crate::app::modal::KeyRoute;
use crate::app::modal::ModalOwner;
use crate::app::settings::GatewaySaveReport;
use crate::app::settings::InterfaceSaveReport;
use crate::app::settings::changed_leaf_paths;
use crate::app::settings::save_notice_ok;
use crate::app::settings::save_notice_text;
use crate::config::rendering::ImagesMode;
use crate::shared::panels::settings::InterfaceSource;
use crate::shared::panels::settings::SaveOutcome;
use crate::shared::panels::settings::SettingsAction;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::View;
use crate::ui::chat_view::ChatCell;
use crate::ui::toast::ToastKind;

// ── 目录夹具 ───────────────────────────────────────────────

fn node(key: &str, kind: SettingKind) -> SettingNode {
    SettingNode {
        key: key.into(),
        path: key.into(),
        title: key.into(),
        doc: format!("{key} 的说明"),
        notes: vec![],
        example: None,
        order: 0,
        kind,
        required: false,
        nullable: false,
        default: None,
        has_default: false,
        min: None,
        max: None,
        exclusive_min: false,
        exclusive_max: false,
        min_length: None,
        pattern: None,
        choices: vec![],
        min_items: None,
        max_items: None,
        secret: false,
        apply: ApplyScope::Hot,
        editable: true,
        deprecated: None,
        section: None,
        section_doc: None,
        children: vec![],
        element: None,
        variants: None,
        summary_fields: vec![],
        value_hint: None,
    }
}

/// 递归重算全部 path（**与 07/08 的夹具同口径**，review N1）：
/// `key == "[]"`（元素模板）产出 `<prefix>[]` 而不是 `<prefix>.[]`，并且
/// `element` / `variants` 也要跟着重算 —— 漏掉任何一条，列表内字段（`providers[].api_key`）
/// 的 catalog 路径就与 `flatten` 产出的模板路径对不上，搜索类用例会**静默空转**。
fn with_paths(mut node: SettingNode, prefix: &str) -> SettingNode {
    node.path = if node.key == "[]" {
        format!("{prefix}[]")
    } else if prefix.is_empty() {
        node.key.clone()
    } else {
        format!("{prefix}.{}", node.key)
    };
    let path = node.path.clone();
    if let Some(element) = node.element.take() {
        node.element = Some(Box::new(with_paths(*element, &path)));
    }
    if let Some(variants) = node.variants.take() {
        node.variants = Some(
            variants
                .into_iter()
                .map(|variant| with_paths(variant, &path))
                .collect(),
        );
    }
    let children = std::mem::take(&mut node.children);
    node.children = children
        .into_iter()
        .map(|child| with_paths(child, &path))
        .collect();
    node
}

fn object(key: &str, children: Vec<SettingNode>) -> SettingNode {
    let mut node = node(key, SettingKind::Object);
    node.children = children
        .into_iter()
        .map(|child| with_paths(child, key))
        .collect();
    node
}

fn str_field(key: &str) -> SettingNode {
    node(key, SettingKind::Str)
}

fn int_field(key: &str, default: i64) -> SettingNode {
    let mut node = node(key, SettingKind::Int);
    node.has_default = true;
    node.default = Some(json!(default));
    node
}

fn secret_field(key: &str) -> SettingNode {
    let mut node = node(key, SettingKind::Secret);
    node.secret = true;
    node
}

fn element(mut inner: SettingNode) -> SettingNode {
    inner.key = "[]".into();
    inner
}

fn list(key: &str, element: SettingNode) -> SettingNode {
    let mut node = node(key, SettingKind::List);
    let element = element;
    node.element = Some(Box::new(element));
    node
}

/// Gateway 根：`providers[0]`（含密文）+ `gateway.port` + `tools`。
fn gateway_catalog() -> SettingNode {
    let provider = object("[]", vec![str_field("name"), secret_field("api_key")]);
    let providers = {
        let mut providers = list("providers", provider);
        providers.min_items = Some(1);
        providers.summary_fields = vec!["name".into()];
        providers
    };
    let auth = object("auth", vec![node("enabled", SettingKind::Bool)]);
    let gateway = object("gateway", vec![int_field("port", 32523), auth]);
    let mut root = node("config", SettingKind::Object);
    root.path = "config".into();
    root.children = vec![
        with_paths(providers, ""),
        with_paths(gateway, ""),
        with_paths(list("tools", element(str_field("tool"))), ""),
    ];
    root
}

/// Interface 根：`colors.{preset, accent}` + `layout.max_input_lines`。
fn interface_catalog() -> SettingNode {
    let mut preset = node("preset", SettingKind::Enum);
    preset.choices = vec![
        SettingChoice {
            value: "wing".into(),
            doc: Some("品牌配色".into()),
        },
        SettingChoice {
            value: "terminal".into(),
            doc: Some("跟随终端".into()),
        },
    ];
    preset.has_default = true;
    preset.default = Some(json!("wing"));
    let mut accent = str_field("accent");
    accent.value_hint = Some("color".into());
    accent.nullable = true;
    let colors = object("colors", vec![preset, accent]);
    let layout = object("layout", vec![int_field("max_input_lines", 10)]);
    let mut root = node("interface", SettingKind::Object);
    root.path = "interface".into();
    root.children = vec![with_paths(colors, ""), with_paths(layout, "")];
    root
}

fn gateway_schema() -> SettingsSchemaResponse {
    let root = gateway_catalog();
    SettingsSchemaResponse {
        version: "0.0.0-test".into(),
        root,
        config_path: "/home/u/.wing/core/config.yaml".into(),
        groups: Vec::new(),
    }
}

fn gateway_state(values: Value, fingerprint: &str) -> SettingsGetResponse {
    let mut secrets = HashMap::new();
    secrets.insert(
        "providers[0].api_key".to_string(),
        SecretState {
            state: SecretPresence::Set,
            hint: Some("ab12".into()),
        },
    );
    SettingsGetResponse {
        values,
        secrets,
        fingerprint: fingerprint.into(),
        problems: vec![],
        setup_mode: false,
        config_path: "/home/u/.wing/core/config.yaml".into(),
    }
}

fn sample_values() -> Value {
    json!({
        "providers": [{"name": "default", "api_key": null}],
        "gateway": {"port": 32523, "auth": {"enabled": true}},
        "tools": ["Bash"],
    })
}

fn interface_source(accent: &str) -> InterfaceSource {
    InterfaceSource {
        groups: vec![SettingGroup {
            id: "interface".into(),
            title: "Interface".into(),
            doc: "TUI 自身".into(),
            members: vec!["colors".into(), "layout".into()],
        }],
        catalog: interface_catalog(),
        doc: json!({"colors": {"accent": accent}}),
    }
}

/// 打开状态的面板：schema / state 已注入，Interface 根以一个确定性的文档注入
/// （生产路径读磁盘，测试不依赖 `~/.wing`；欢迎屏保留，header 重建的测试要用它）。
fn app_with_settings_panel() -> App {
    let mut app = test_app();
    app.inject_settings_panel(
        &gateway_schema(),
        gateway_state(sample_values(), "fp-1"),
        Some(interface_source("magenta")),
    );
    app
}

fn open_with_cache(app: &mut App) {
    app.settings_cache = Some((gateway_schema(), gateway_state(sample_values(), "fp-1")));
    app.open_settings_panel();
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn press(app: &mut App, code: KeyCode) {
    app.handle_key(key(code));
}

fn press_event(app: &mut App, event: KeyEvent) {
    app.handle_key(event);
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch));
    }
}

/// 搜索定位到含 `needle` 的第一行（祖先自动展开，Enter 退出搜索并保留光标）。
fn goto(app: &mut App, needle: &str) {
    press(app, KeyCode::Char('/'));
    type_text(app, needle);
    press(app, KeyCode::Enter);
}

fn panel(app: &App) -> &SettingsPanel {
    app.settings_panel.as_ref().expect("面板开着")
}

/// header 行里出现过的前景色（去重排序）——调色板变了它就该变。
fn header_colors(app: &App) -> Vec<String> {
    let mut colors: Vec<String> = app
        .chat
        .header_lines()
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter_map(|span| span.style.fg)
        .map(|color| format!("{color:?}"))
        .collect();
    colors.sort();
    colors.dedup();
    colors
}

fn last_cell_text(app: &App) -> (bool, String) {
    let cell = app.chat.cells.last().expect("至少一条 cell").cell();
    match cell {
        ChatCell::SystemMessage(text) => (true, text.clone()),
        ChatCell::WarningMessage(text) => (false, text.clone()),
        ChatCell::ErrorMessage(text) => (false, text.clone()),
        other => panic!("最后一条不是 notice：{other:?}"),
    }
}

/// 把 Interface 半边记进"等 Gateway 回执"的槽里 —— 生产里是 runner 干的
/// （`execute_intent` 的 SaveSettings 臂），回执合并读的就是这一份。
fn record_pending_interface(app: &mut App, report: InterfaceSaveReport) {
    app.settings_save = Some(crate::app::settings::PendingSettingsSave {
        interface: report,
        document: json!({}),
    });
}

fn settings_response(
    ok: bool,
    fingerprint: &str,
    problems: Vec<SettingProblem>,
) -> SettingsSetResponse {
    SettingsSetResponse {
        warnings: vec![],
        ok,
        fingerprint: fingerprint.into(),
        problems,
        changed: vec!["gateway.port".into()],
        restart_required: vec![],
        reload: None,
        setup_mode_exited: false,
        backup_path: None,
    }
}

// ── 1. 模态优先级（AD2） ───────────────────────────────────

#[test]
fn settings_owns_the_app_reserved_keys_and_ctrl_o() {
    let app = app_with_settings_panel();
    assert_eq!(app.modal_owner(), Some(ModalOwner::Settings));
    for code in [
        KeyCode::Esc,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Char('o'),
    ] {
        let key = if code == KeyCode::Char('o') {
            ctrl(code)
        } else {
            key(code)
        };
        assert_eq!(
            app.route_key(&key),
            KeyRoute::Settings,
            "{code:?} 归面板（AD2）"
        );
    }
    // Ctrl+C 永远归应用（双击退出）。
    assert_eq!(app.route_key(&ctrl(KeyCode::Char('c'))), KeyRoute::Quit);
}

#[test]
fn without_the_panel_the_reserved_keys_route_exactly_as_before() {
    let app = test_app();
    assert_eq!(app.route_key(&key(KeyCode::Esc)), KeyRoute::EscLadder);
    assert_eq!(
        app.route_key(&key(KeyCode::PageUp)),
        KeyRoute::ChatScroll(ChatScrollAction::PageUp)
    );
    assert_eq!(
        app.route_key(&ctrl(KeyCode::Char('o'))),
        KeyRoute::ToggleReasoning
    );
}

#[test]
fn an_ask_panel_keeps_its_reserved_key_contract() {
    // 回归：ask 的既有保留键行为一字不改（只新增 Settings 这一层的例外）。
    let mut app = test_app();
    app.handle_event(sync_event(
        vec![],
        None,
        vec![],
        vec![json!({
            "type": "ask",
            "tool_call_id": "ask-1",
            "questions": [{"id": "q", "header": "h", "question": "q?", "options": [{"label": "a"}]}],
        })],
        None,
    ));
    assert_eq!(app.modal_owner(), Some(ModalOwner::AskPanel));
    assert_eq!(app.route_key(&key(KeyCode::Esc)), KeyRoute::EscLadder);
    assert_eq!(
        app.route_key(&key(KeyCode::PageUp)),
        KeyRoute::ChatScroll(ChatScrollAction::PageUp)
    );
}

#[test]
fn the_open_panel_blocks_the_composer_pointer_and_typing() {
    let mut app = app_with_settings_panel();
    assert!(app.composer_pointer_blocked());
    assert!(app.composer_typing_blocked());
    // 关掉之后回到"没有模态"的既有状态。
    app.close_settings_panel(false);
    assert!(!app.composer_pointer_blocked());
    assert!(!app.composer_typing_blocked());
}

#[test]
fn paste_while_the_panel_is_open_does_not_reach_the_draft() {
    let mut app = app_with_settings_panel();
    app.handle_paste("hello");
    assert_eq!(app.input.text(), "", "面板开着时粘贴不进草稿");
    app.close_settings_panel(false);
    app.handle_paste("hello");
    assert_eq!(app.input.text(), "hello");
}

/// N4（review_r1）：设置面板开着时，粘贴**先**被面板丢掉 —— 哪怕队列里正躺着一条
/// ask（AD2：面板开着时到达的 ask 入队但不弹）。顺序反过来的话，`Ctrl+V` 会写进
/// 那条**看不见**的 ask 内联编辑器里。
#[test]
fn paste_is_dropped_before_it_can_reach_a_queued_ask() {
    let mut app = app_with_settings_panel();
    app.handle_event(sync_event(
        vec![],
        None,
        vec![],
        vec![json!({
            "type": "ask",
            "tool_call_id": "ask-paste",
            "questions": [{
                "id": "q1",
                "header": "标题",
                "question": "问题？",
                "options": [{"label": "甲"}, {"label": "乙"}],
            }],
        })],
        None,
    ));
    assert_eq!(app.ask_panels.len(), 1, "ask 入队（不弹）");

    // 把那条 ask 摆成"正在编辑自由输入行"——只有这个状态才可能被粘贴写脏。
    {
        let panel = app.ask_panels.front_mut().expect("队列前端");
        let free_form = panel.questions[0].options.len();
        panel.states[0].cursor = free_form;
        panel.states[0].editing = true;
    }

    app.handle_paste("leaked");
    assert_eq!(
        app.ask_panels.front().expect("队列前端").states[0].draft,
        "",
        "面板开着时粘贴不落到看不见的 ask 编辑器"
    );
    assert_eq!(app.input.text(), "", "也不落到 composer");

    // 关面板后同一条 ask 照旧吃粘贴（挡板只属于面板）。
    app.close_settings_panel(false);
    app.handle_paste("ok");
    assert_eq!(
        app.ask_panels.front().expect("队列前端").states[0].draft,
        "ok",
        "面板关掉后 ask 的粘贴路径不变"
    );
}

// ── 2. ask 排队 ────────────────────────────────────────────

#[test]
fn an_ask_arriving_while_the_panel_is_open_waits_in_the_queue() {
    let mut app = app_with_settings_panel();
    app.handle_event(sync_event(
        vec![],
        None,
        vec![],
        vec![json!({
            "type": "ask",
            "tool_call_id": "ask-9",
            "choices": ["y", "n"],
            "required": true,
        })],
        None,
    ));
    assert_eq!(app.ask_panels.len(), 1, "ask 入队");
    assert_eq!(
        app.modal_owner(),
        Some(ModalOwner::Settings),
        "面板仍持有键盘（ask 不抢）"
    );

    // 面板开着时按键不进 ask：Enter 是面板的（无选择项时无事发生），
    // 也不会把它答掉。
    press(&mut app, KeyCode::Enter);
    let sent = app
        .drain_intents()
        .into_iter()
        .filter(|intent| matches!(intent, AppIntent::SendMessage { .. }))
        .count();
    assert_eq!(sent, 0, "面板开着时不该答 ask");

    // 关面板 → 队列前端接管（照旧的行为）。
    app.close_settings_panel(false);
    assert_eq!(app.modal_owner(), Some(ModalOwner::AskPanel));
    press(&mut app, KeyCode::Enter);
    let sent = app
        .drain_intents()
        .into_iter()
        .find_map(|intent| match intent {
            AppIntent::SendMessage {
                content,
                tool_call_id,
                ..
            } => Some((content, tool_call_id)),
            _ => None,
        });
    assert_eq!(sent, Some(("y".to_string(), Some("ask-9".into()))));
}

// ── 3. overlay 的绘制落点 + 三者处置（§20 风险 13） ────────

#[test]
fn the_open_panel_floats_over_the_chat_and_leaves_the_background_visible() {
    let mut app = test_app();
    app.chat.push(ChatCell::UserMessage("hello world".into()));
    app.input.set_text("draft text");
    let mut terminal = test_terminal(100, 30);

    app.draw(&mut terminal).expect("draw");
    let before = frame_text(terminal.backend().buffer());
    assert!(before.contains("hello world"));
    assert!(before.contains("draft text"));

    app.inject_settings_panel(
        &gateway_schema(),
        gateway_state(sample_values(), "fp-1"),
        Some(interface_source("magenta")),
    );
    // 打开面板要求整屏重画一次：卡片底下的图片像素要擦掉（与关闭时对称）。
    assert!(app.needs_full_redraw, "打开面板 = 一次整屏重画");
    app.draw(&mut terminal).expect("draw");
    let buf = terminal.backend().buffer().clone();
    let after = frame_text(&buf);
    assert!(after.contains("Settings"), "卡片的标题在场：\n{after}");

    // 卡片几何：min(终端宽-4, 110) × min(终端高-4, 32)，居中。
    let card = crate::ui::settings::card_area(ratatui::layout::Rect::new(0, 0, 100, 30));
    assert_eq!(card, ratatui::layout::Rect::new(2, 2, 96, 26));
    assert_eq!(buf[(card.x, card.y)].symbol(), "┌", "卡片左上角");
    assert_eq!(
        buf[(card.right() - 1, card.bottom() - 1)].symbol(),
        "┘",
        "卡片右下角"
    );

    // 四周露出背景：状态栏与 composer 都还在（v1 的全屏 overlay 会把它们抹掉）。
    assert!(after.contains("test-session"), "状态栏可见：\n{after}");
    assert!(after.contains("draft text"), "composer 可见：\n{after}");
    // 卡片内部不透底：被它盖住的那条消息不见了。
    assert!(!after.contains("hello world"), "卡片内部不透底：\n{after}");
}

#[test]
fn a_small_terminal_degrades_the_panel_to_full_screen() {
    // < 80×24：浮层比全屏更难读，直接铺满。
    for (width, height) in [(79u16, 30u16), (80, 23), (60, 48), (40, 10)] {
        let card = crate::ui::settings::card_area(ratatui::layout::Rect::new(0, 0, width, height));
        assert_eq!(
            card,
            ratatui::layout::Rect::new(0, 0, width, height),
            "{width}x{height} 应当铺满"
        );
    }
    // ≥ 80×24：浮层，且不超过 110×32。
    let card = crate::ui::settings::card_area(ratatui::layout::Rect::new(0, 0, 80, 24));
    assert_eq!(card, ratatui::layout::Rect::new(2, 2, 76, 20));
    let card = crate::ui::settings::card_area(ratatui::layout::Rect::new(0, 0, 200, 60));
    assert_eq!(card, ratatui::layout::Rect::new(45, 14, 110, 32));
}

#[test]
fn the_open_panel_suppresses_pictures_and_closing_invalidates_them() {
    let dir = TempDir::new("settings-overlay");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut terminal = test_terminal(60, 48);
    draw_until(&mut app, &mut terminal, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    assert_eq!(app.images.stats().expect("lane").cached, 1);

    // 面板开着：这一帧不画图片（overlay 是最后写入者）。
    app.inject_settings_panel(
        &gateway_schema(),
        gateway_state(sample_values(), "fp-1"),
        Some(interface_source("magenta")),
    );
    let buf = super::images::frame(&mut app, &mut terminal);
    assert!(
        !has_placeholder(&buf),
        "卡片挡住的图片这一帧不画（打开面板时还整屏擦过一次）"
    );

    // 关闭：整屏重画 + 图片句柄失效（design §20 风险 13 的配对）。
    app.close_settings_panel(false);
    assert!(app.needs_full_redraw, "关闭后面板要求整屏重画");
    // N2：`invalidate` 在**关闭那一刻**就已经生效（不是靠下一次 draw 的兜底）——
    // 下一帧的 `needs_full_redraw` 分支会把它再做一遍，只有在这里读才分得清。
    assert_eq!(
        app.images.stats().expect("lane").cached,
        0,
        "关闭动作自己调了 images.invalidate()（不等 draw）"
    );
    let buf = super::images::frame(&mut app, &mut terminal);
    assert!(!has_placeholder(&buf), "失效的编码不会再画");
    assert_eq!(
        app.images.stats().expect("lane").cached,
        0,
        "关闭面板时 images.invalidate() 被调用（编码被丢掉）"
    );
}

#[test]
fn opening_the_panel_cancels_an_in_flight_selection() {
    let mut app = test_app();
    app.chat.push(ChatCell::UserMessage("hello world".into()));
    let mut terminal = test_terminal(80, 30);
    app.draw(&mut terminal).expect("draw");
    // 起一个选区（按下不松开），随后打开面板。
    let band = app.geometry.chat_band();
    app.handle_mouse(mouse_press((band.x + 2, band.y + 1)));
    assert!(app.selection.is_press_active(), "选区在飞");
    app.inject_settings_panel(
        &gateway_schema(),
        gateway_state(sample_values(), "fp-1"),
        Some(interface_source("magenta")),
    );
    assert!(!app.selection.is_press_active(), "打开面板取消选区");
}

// ── 3b. 滚动条在 overlay 期间的交接（§20 风险 13 的第三件） ────

/// 滚动条独有的字形：强调态（悬停 / 拖拽）的 thumb 是 `█`，其余是 `┃`；
/// `│` 与所有边框共享，不能当判据。面板自己只画 `│` / `─` / `❯` / `■`（U+25A0，
/// 不是 `█` U+2588），所以整帧扫这两个码位就是"滚动条画没画"。
fn has_bar_glyph(buf: &ratatui::buffer::Buffer) -> bool {
    for y in buf.area.y..buf.area.bottom() {
        for x in buf.area.x..buf.area.right() {
            if matches!(buf[(x, y)].symbol(), "┃" | "█") {
                return true;
            }
        }
    }
    false
}

/// S2（review_r1）：overlay 期间**不绘制**滚动条，且 hover / drag 态在打开那一刻
/// 就交出去 —— 不交出去的话关掉面板后拖拽态会复活（强调字形重新出现）。
#[test]
fn the_open_panel_hands_the_scrollbar_over_and_gives_it_back_on_close() {
    let mut app = test_app();
    for i in 0..40 {
        app.chat
            .push(ChatCell::AssistantMessage(format!("msg {i}")));
    }
    let mut terminal = test_terminal(80, 24);
    app.draw(&mut terminal).expect("draw");

    // 悬停让滚动条进入强调态：这是"有东西要交接"的前提。
    let geom = app.scrollbar_geometry().expect("内容溢出，滚动条在场");
    app.handle_mouse(hover((geom.column, geom.thumb_top)));
    assert!(app.scrollbar.is_active(), "悬停亮起");
    app.draw(&mut terminal).expect("draw");
    assert!(
        has_bar_glyph(terminal.backend().buffer()),
        "强调态画的是 `█`：\n{}",
        frame_text(terminal.backend().buffer())
    );

    // 打开面板：overlay 的第一帧就把状态交出去，且一个滚动条字形都不画。
    app.inject_settings_panel(
        &gateway_schema(),
        gateway_state(sample_values(), "fp-1"),
        Some(interface_source("magenta")),
    );
    app.draw(&mut terminal).expect("draw");
    let buf = terminal.backend().buffer();
    assert!(frame_text(buf).contains("Settings"), "面板在场");
    assert!(!app.scrollbar.is_active(), "overlay 帧清掉 hover / drag 态");
    assert!(
        !has_bar_glyph(buf),
        "overlay 期间不画滚动条：\n{}",
        frame_text(buf)
    );

    // 关面板：滚动条回来（悬停态不复活 —— 那要用户下一次真的悬停）。
    app.close_settings_panel(false);
    app.draw(&mut terminal).expect("draw");
    assert!(!app.scrollbar.is_active());
    assert!(
        has_bar_glyph(terminal.backend().buffer()),
        "关掉面板后滚动条回来：\n{}",
        frame_text(terminal.backend().buffer())
    );
}

// ── 4. 实时预览（§15.3） ───────────────────────────────────

#[test]
fn editing_an_interface_color_through_the_real_key_path_previews_it() {
    let mut app = app_with_settings_panel();
    // Tab 到 Interface 根，搜索定位到 accent，改色提交。
    press(&mut app, KeyCode::Tab);
    goto(&mut app, "accent");
    press(&mut app, KeyCode::Enter); // 打开编辑器（预填 magenta）
    press_event(&mut app, ctrl(KeyCode::Char('u'))); // 清空缓冲
    type_text(&mut app, "cyan");
    // S1：先把 `App::with_images` 的初始值消费掉 —— 否则下面那条断言恒真
    //（构造即 `true`，而这条测试从不 draw）。提交之后的 `true` 必须由这次预览给出。
    app.needs_full_redraw = false;
    press(&mut app, KeyCode::Enter); // 提交

    assert_eq!(
        app.config.colors.accent.as_deref(),
        Some("cyan"),
        "实时预览写进了 self.config"
    );
    assert_eq!(app.palette.accent, ratatui::style::Color::Cyan);
    assert!(app.needs_full_redraw, "调色板变了要整屏重画");
}

/// S1 的专门守卫（review_r1 的 M-E 从这里穿过去）：`apply_palette` 必须**主动**
/// 置 `needs_full_redraw` —— 调色板不在 ratatui 的 diff 键里，不整屏重画就会留残影。
#[test]
fn a_preview_asks_for_a_full_redraw() {
    let mut app = app_with_settings_panel();
    app.needs_full_redraw = false;
    app.apply_settings_preview(&json!({"colors": {"accent": "cyan"}}));
    assert!(app.needs_full_redraw, "改调色板 → 整屏重画");
}

/// 同一条守卫的另一半：回退预览（`Close{discard:true}`）也必须整屏重画。
#[test]
fn a_discard_close_asks_for_a_full_redraw() {
    let mut app = app_with_settings_panel();
    app.apply_settings_preview(&json!({"colors": {"accent": "cyan"}}));
    app.needs_full_redraw = false;
    app.close_settings_panel(true);
    assert!(app.needs_full_redraw, "关掉面板（含回退）→ 整屏重画");
}

#[test]
fn previewing_rebuilds_the_welcome_header_with_the_new_palette() {
    let mut app = app_with_settings_panel();
    // 定格时钟（跳过开屏扫光）：两次构建之间只有调色板不同 —— 颜色集合的差异
    // 因此只可能来自主题（姿态只换字形，不换每个字母的取色）。
    let settled = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let palette = app.palette();
    app.sync_welcome(&palette, 120, settled);
    let before = header_colors(&app);
    assert!(!before.is_empty(), "header 有色可用");

    app.apply_settings_preview(&json!({"colors": {"accent": "cyan"}}));
    assert!(app.welcome_theme_dirty, "调色板变了，header 要重建一次");
    let palette = app.palette();
    app.sync_welcome(&palette, 120, settled);
    assert!(!app.welcome_theme_dirty, "重建一次即清");
    let after = header_colors(&app);
    assert_ne!(
        before, after,
        "header 的取色跟着调色板换了：{before:?} → {after:?}"
    );
}

#[test]
fn closing_with_discard_restores_the_config_snapshot() {
    let mut app = app_with_settings_panel();
    let before = app.config.colors.accent.clone();
    app.apply_settings_preview(&json!({"colors": {"accent": "cyan"}}));
    assert_eq!(app.config.colors.accent.as_deref(), Some("cyan"));
    // S1：消费初始值，让下面那条断言由"这次关闭"负责（不是构造时的 true）。
    app.needs_full_redraw = false;

    app.close_settings_panel(true);
    assert_eq!(app.config.colors.accent, before, "预览被回退");
    assert!(app.needs_full_redraw, "关闭要整屏重画");
    assert!(app.settings_panel.is_none());
}

#[test]
fn a_successful_interface_save_updates_the_snapshot() {
    let mut app = app_with_settings_panel();
    let saved = json!({"colors": {"accent": "cyan"}});
    app.apply_settings_preview(&saved);
    let report = InterfaceSaveReport::Ok {
        path: "/home/u/.wing/tui/config.yaml".into(),
        changed: 1,
    };
    app.settle_interface_save(&report, &saved);
    assert_eq!(
        app.config_snapshot
            .as_ref()
            .and_then(|c| c.colors.accent.clone()),
        Some("cyan".to_string()),
        "保存成功后快照跟到已保存的值"
    );

    // 再按 Esc（discard）不会把已保存的改动撤销掉。
    app.close_settings_panel(true);
    assert_eq!(app.config.colors.accent.as_deref(), Some("cyan"));
}

// ── 5. 一键保存两边（§15.4）＋ 四种组合的 notice ────────────

#[test]
fn a_full_save_reports_both_sides_in_one_notice() {
    let mut app = app_with_settings_panel();
    let mut response = settings_response(true, "fp-2", vec![]);
    response.changed = vec!["gateway.port".into(), "gateway.auth.enabled".into()];
    response.restart_required = vec!["gateway.port".into()];
    response.reload = Some(wing_api_client::models::ReloadResponse {
        ok: true,
        results: vec![wing_api_client::models::ReloadResultItem {
            name: "provider".into(),
            ok: true,
            detail: Some("rebuilt 2 provider(s)".into()),
        }],
    });
    let interface = InterfaceSaveReport::Ok {
        path: "/home/u/.wing/tui/config.yaml".into(),
        changed: 3,
    };
    // Interface 半边也真的落了地（面板清脏 + 快照更新），并记进等回执的槽。
    let saved = json!({"colors": {"accent": "cyan"}});
    app.settle_interface_save(&interface, &saved);
    record_pending_interface(&mut app, interface);
    app.settle_gateway_save(GatewaySaveReport::Saved(response));

    let (info, text) = last_cell_text(&app);
    assert!(info, "全部成功 = info rail");
    assert!(text.starts_with("设置已保存"), "{text}");
    assert!(
        text.contains("✓ Interface   /home/u/.wing/tui/config.yaml（3 项）"),
        "{text}"
    );
    assert!(
        text.contains("✓ Gateway     2 项变更 · provider rebuilt 2 provider(s)"),
        "{text}"
    );
    assert!(
        text.contains("⚠ gateway.port 需重启网关才生效 — Ctrl+R 立即重启"),
        "AD1 的文案：{text}"
    );
    assert!(
        !text.contains("按 r "),
        "不许出现 design §12.4 的旧文案：{text}"
    );
    assert_eq!(app.toast.as_ref().map(|t| t.kind), Some(ToastKind::Info));
}

#[test]
fn gateway_problems_switch_the_panel_to_the_problems_view() {
    let mut app = app_with_settings_panel();
    let problems = vec![
        SettingProblem {
            path: Some("providers".into()),
            kind: "empty_list".into(),
            message: "providers 不得为空".into(),
            hint: None,
        },
        SettingProblem {
            path: Some("gateway.port".into()),
            kind: "invalid_value".into(),
            message: "端口越界".into(),
            hint: None,
        },
    ];
    let response = settings_response(false, "fp-1", problems);
    app.settle_gateway_save(GatewaySaveReport::Saved(response));

    let (info, text) = last_cell_text(&app);
    assert!(!info, "失败 = warning rail");
    assert!(
        text.contains("✗ Gateway     2 个问题，未保存（已切到问题清单）"),
        "{text}"
    );
    assert_eq!(panel(&app).view(), View::Problems, "面板自动切到问题清单");
    assert_eq!(panel(&app).problems().len(), 2);
}

#[test]
fn an_interface_write_failure_leaves_the_gateway_half_visible() {
    let mut app = app_with_settings_panel();
    let interface = InterfaceSaveReport::Err {
        message: "cannot write /home/u/.wing/tui/config.yaml: permission denied".into(),
    };
    app.settle_interface_save(&interface, &json!({}));
    let response = settings_response(true, "fp-2", vec![]);
    record_pending_interface(&mut app, interface.clone());
    app.settle_gateway_save(GatewaySaveReport::Saved(response));

    let (info, text) = last_cell_text(&app);
    assert!(!info);
    assert!(text.starts_with("设置部分保存"), "{text}");
    assert!(
        text.contains("✗ Interface   写入失败：cannot write"),
        "{text}"
    );
    assert!(text.contains("✓ Gateway     1 项变更"), "{text}");
    assert!(!save_notice_ok(&interface, &GatewaySaveReport::Skipped));
}

#[test]
fn both_halves_failing_reports_unsaved() {
    let mut app = app_with_settings_panel();
    let interface = InterfaceSaveReport::Err {
        message: "disk on fire".into(),
    };
    app.settle_interface_save(&interface, &json!({}));
    record_pending_interface(&mut app, interface);
    let response = settings_response(
        false,
        "fp-1",
        vec![SettingProblem {
            path: None,
            kind: "invalid_value".into(),
            message: "文档级问题".into(),
            hint: None,
        }],
    );
    app.settle_gateway_save(GatewaySaveReport::Saved(response));

    let (info, text) = last_cell_text(&app);
    assert!(!info);
    assert!(text.starts_with("设置未保存"), "{text}");
    assert!(text.contains("✗ Interface"), "{text}");
    assert!(text.contains("✗ Gateway     1 个问题"), "{text}");
    assert_eq!(app.toast.as_ref().map(|t| t.kind), Some(ToastKind::Warning));
}

#[test]
fn a_gateway_transport_failure_is_reported_without_touching_the_interface_half() {
    let mut app = app_with_settings_panel();
    let interface = InterfaceSaveReport::Ok {
        path: "/home/u/.wing/tui/config.yaml".into(),
        changed: 1,
    };
    app.settle_interface_save(&interface, &json!({"colors": {"accent": "cyan"}}));
    record_pending_interface(&mut app, interface);
    app.settle_gateway_save(GatewaySaveReport::Failed {
        message: "connection refused".into(),
        conflict: false,
    });

    let (_, text) = last_cell_text(&app);
    assert!(
        text.contains("✗ Gateway     保存失败：connection refused"),
        "{text}"
    );
    assert!(text.contains("✓ Interface"), "{text}");
    assert!(!panel(&app).is_stale(), "传输失败不是指纹冲突");
}

#[test]
fn a_conflict_marks_the_panel_stale_and_points_at_reload() {
    let mut app = app_with_settings_panel();
    app.settle_gateway_save(GatewaySaveReport::Failed {
        message: "409 conflict".into(),
        conflict: true,
    });
    let (_, text) = last_cell_text(&app);
    assert!(
        text.contains("配置已被其它客户端修改，按 R 重新载入"),
        "{text}"
    );
    assert!(panel(&app).is_stale(), "409 让面板亮起横幅");
}

#[test]
fn an_interface_only_save_closes_the_receipt_without_a_gateway_line() {
    // 只有 Interface 半边：Gateway 那条不该出现（`Skipped`）。
    let interface = InterfaceSaveReport::Ok {
        path: "/home/u/.wing/tui/config.yaml".into(),
        changed: 2,
    };
    let text = save_notice_text(&interface, &GatewaySaveReport::Skipped);
    assert!(text.starts_with("设置已保存"), "{text}");
    assert!(text.contains("✓ Interface"), "{text}");
    assert!(!text.contains("Gateway"), "{text}");
    assert!(save_notice_ok(&interface, &GatewaySaveReport::Skipped));
}

/// AD13：后端带回 `warnings`（坏文件被修复、密文无从回填）时，回执**逐条**显示 ——
/// 它是"用户必须知道"的话，不是日志；且不改变成败等级（保存是成功的）。
#[test]
fn a_save_that_lost_secrets_reports_the_backend_warnings() {
    let mut app = app_with_settings_panel();
    let mut response = settings_response(true, "fp-2", vec![]);
    response.changed = vec!["providers[0].api_key".into()];
    response.setup_mode_exited = true;
    response.warnings = vec!["原配置文件无法解析，其中的密钥无法保留，请重新填写".into()];
    app.settle_gateway_save(GatewaySaveReport::Saved(response));

    let (info, text) = last_cell_text(&app);
    assert!(info, "有 warning 也还是成功（info rail）：{text}");
    assert!(text.starts_with("设置已保存"), "{text}");
    assert!(
        text.contains("⚠ 原配置文件无法解析，其中的密钥无法保留，请重新填写"),
        "AD13 的说明要在回执里：{text}"
    );
    assert_eq!(
        app.toast.as_ref().map(|t| t.message.as_str()),
        Some("设置已保存 · 1 条说明（见回执）"),
        "回执压在面板下面：toast 要指出还有说明可看"
    );

    // 没有 warnings 的响应不产生这一行（老网关 / 一切正常）。
    let mut app = app_with_settings_panel();
    app.settle_gateway_save(GatewaySaveReport::Saved(settings_response(
        true,
        "fp-3",
        vec![],
    )));
    let (_, text) = last_cell_text(&app);
    assert!(
        !text.contains("⚠ 原配置文件"),
        "没有 warning 就没有这一行：{text}"
    );
}

// ── 6. 轮次中拒绝重启（§12.5） ─────────────────────────────

#[test]
fn restart_is_refused_while_a_turn_is_running() {
    let mut app = app_with_settings_panel();
    app.turn.working = true;
    app.handle_settings_action(SettingsAction::RestartGateway);
    assert!(
        !app.drain_intents()
            .iter()
            .any(|intent| matches!(intent, AppIntent::RestartGateway)),
        "轮次进行中不产出重启意图"
    );
    assert_eq!(app.toast.as_ref().map(|t| t.kind), Some(ToastKind::Warning));

    app.turn.working = false;
    app.handle_settings_action(SettingsAction::RestartGateway);
    assert!(
        app.drain_intents()
            .iter()
            .any(|intent| matches!(intent, AppIntent::RestartGateway)),
        "空闲时产出重启意图"
    );
}

// ── 7. settings_changed 的指纹比对（§7.6） ─────────────────

#[test]
fn our_own_save_does_not_raise_the_stale_banner() {
    let mut app = app_with_settings_panel();
    app.settle_gateway_save(GatewaySaveReport::Saved(settings_response(
        true,
        "fp-2",
        vec![],
    )));
    assert_eq!(panel(&app).fingerprint(), "fp-2");

    // 自己的保存也会广播 settings_changed：指纹相同 → 不警告。
    app.handle_event(crate::protocol::WingEvent::SettingsChanged {
        changed: vec![],
        restart_required: vec![],
        setup_mode_exited: false,
        fingerprint: "fp-2".into(),
        meta: crate::protocol::EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: None,
            request_id: "r".into(),
        },
    });
    assert!(!panel(&app).is_stale(), "自己的保存不触发横幅");
}

#[test]
fn another_clients_change_raises_the_banner() {
    let mut app = app_with_settings_panel();
    app.handle_event(crate::protocol::WingEvent::SettingsChanged {
        changed: vec!["gateway.port".into()],
        restart_required: vec![],
        setup_mode_exited: false,
        fingerprint: "fp-someone-else".into(),
        meta: crate::protocol::EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: None,
            request_id: "r".into(),
        },
    });
    assert!(panel(&app).is_stale(), "别人的改动亮横幅");
    // 不自动覆盖本地：面板的文档 / 指纹一个都没变。
    assert_eq!(panel(&app).fingerprint(), "fp-1");
}

#[test]
fn a_settings_changed_with_the_panel_closed_drops_the_cache() {
    let mut app = app_with_settings_panel();
    app.close_settings_panel(false);
    assert!(app.settings_cache.is_some());
    app.handle_event(crate::protocol::WingEvent::SettingsChanged {
        changed: vec![],
        restart_required: vec![],
        setup_mode_exited: false,
        fingerprint: "fp-x".into(),
        meta: crate::protocol::EventMeta {
            created_at: "2026-01-01T00:00:00+00:00".into(),
            session_id: None,
            request_id: "r".into(),
        },
    });
    assert!(
        app.settings_cache.is_none(),
        "关着时丢缓存：旧文档 + 新指纹的 base 会造成丢失更新"
    );
}

// ── 8. 打开 / 刷新 / 重载的三条落地口径 ─────────────────────

#[test]
fn the_open_command_is_cache_first_and_asks_for_a_refresh() {
    let mut app = test_app();
    open_with_cache(&mut app);
    assert!(app.settings_panel.is_some(), "有缓存：立刻打开");
    assert!(
        app.drain_intents()
            .iter()
            .any(|intent| matches!(intent, AppIntent::FetchSettings)),
        "同时推一次后台刷新"
    );
}

#[test]
fn without_a_cache_the_panel_opens_when_the_fetch_lands() {
    let mut app = test_app();
    app.open_settings_panel();
    assert!(app.settings_panel.is_none(), "没缓存：先等数据");
    assert!(app.settings_pending);
    app.handle_settings_fetch(gateway_schema(), gateway_state(sample_values(), "fp-1"));
    assert!(app.settings_panel.is_some(), "数据到达即打开");
    assert!(!app.settings_pending);
}

#[test]
fn a_background_refresh_does_not_clobber_local_edits() {
    let mut app = app_with_settings_panel();
    // 用户先改一个 gateway 字段（标脏）。
    goto(&mut app, "port");
    press(&mut app, KeyCode::Enter);
    press_event(&mut app, ctrl(KeyCode::Char('u')));
    type_text(&mut app, "9999");
    press(&mut app, KeyCode::Enter);
    assert!(panel(&app).dirty_count() > 0);

    // 后台刷新到达：面板脏 → 不覆盖（用户的值还在）。
    app.handle_settings_fetch(gateway_schema(), gateway_state(sample_values(), "fp-1"));
    assert!(panel(&app).dirty_count() > 0, "刷新没有把本地改动抹掉");
}

#[test]
fn a_reload_discards_local_edits_unconditionally() {
    let mut app = app_with_settings_panel();
    goto(&mut app, "port");
    press(&mut app, KeyCode::Enter);
    press_event(&mut app, ctrl(KeyCode::Char('u')));
    type_text(&mut app, "9999");
    press(&mut app, KeyCode::Enter);
    assert!(panel(&app).dirty_count() > 0);

    // `R` → 面板产出 Reload（脏则先确认；这里先确认一次）。
    press(&mut app, KeyCode::Char('R'));
    press(&mut app, KeyCode::Enter); // 确认放弃
    assert!(
        app.drain_intents()
            .iter()
            .any(|intent| matches!(intent, AppIntent::ReloadSettings)),
        "R 推重载意图"
    );
    assert!(app.settings_reload_pending);

    // 重拉回来的快照无条件落地（丢弃本地改动是重载的语义）。
    app.handle_settings_fetch(gateway_schema(), gateway_state(sample_values(), "fp-1"));
    assert_eq!(panel(&app).dirty_count(), 0, "重载丢弃本地改动");
    assert!(!app.settings_reload_pending);
}

#[test]
fn closing_the_panel_drops_the_open_and_reload_flags() {
    let mut app = app_with_settings_panel();
    app.settings_pending = true;
    app.settings_reload_pending = true;
    app.close_settings_panel(false);
    assert!(!app.settings_pending && !app.settings_reload_pending);
    assert!(app.interface_catalog.is_none());
}

// ── 9. 保存载荷：密文红线（接线级） ────────────────────────

#[test]
fn the_app_passes_an_untouched_secret_null_straight_through_to_save() {
    let mut app = app_with_settings_panel();
    // 改一个别的字段（gateway.port），让面板产出 Save。
    goto(&mut app, "port");
    press(&mut app, KeyCode::Enter);
    press_event(&mut app, ctrl(KeyCode::Char('u')));
    type_text(&mut app, "9999");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('s'));

    let save = app
        .drain_intents()
        .into_iter()
        .find_map(|intent| match intent {
            AppIntent::SaveSettings { gateway, .. } => Some(gateway),
            _ => None,
        })
        .expect("s 产出 SaveSettings");
    assert_eq!(
        save["providers"][0]["api_key"],
        Value::Null,
        "没动过的密文必须原样回传 null（红线）"
    );
    assert_eq!(save["gateway"]["port"], json!(9999));
    assert_eq!(save["providers"][0]["name"], json!("default"));
}

// ── 10. 命令表 ────────────────────────────────────────────

#[test]
fn the_settings_command_and_its_aliases_route_to_the_panel() {
    let route = crate::app::commands::COMMANDS
        .iter()
        .find(|route| route.name == "/settings")
        .expect("/settings 在命令表里");
    assert_eq!(route.aliases, &["/config", "/set"]);
    assert!(route.matches("/settings"));
    assert!(route.matches("/config"));
    assert!(route.matches("/set"));
    assert!(
        !route.matches("/settings now"),
        "takes_args: false —— 带尾巴的拼写是普通消息（与 /clear 同口径）"
    );
    assert!(!route.matches("/setx"), "别名是精确匹配，不做前缀");

    for spelling in ["/settings", "/config", "/set"] {
        let mut app = test_app();
        open_with_cache(&mut app);
        app.close_settings_panel(false);
        assert!(
            app.try_frontend_command(spelling),
            "{spelling} 被命令表吃掉"
        );
        assert!(app.settings_panel.is_some(), "{spelling} 打开面板");
    }
}

// ── 11. 纯函数：叶子路径 diff ──────────────────────────────

#[test]
fn changed_leaf_paths_counts_added_removed_and_modified_leaves() {
    let before = json!({"colors": {"accent": "cyan", "preset": "wing"}, "api_key": "x"});
    let after = json!({"colors": {"accent": "magenta", "preset": "wing"}, "api_key": null});
    assert_eq!(
        changed_leaf_paths(Some(&before), &after),
        vec!["api_key".to_string(), "colors.accent".to_string()]
    );

    // 新增 / 删除**整棵子树**各算一条（不逐叶展开）；数组整体算一条。
    let before = json!({"tools": ["a"], "colors": {"accent": "cyan"}});
    let after = json!({"tools": ["a", "b"]});
    assert_eq!(
        changed_leaf_paths(Some(&before), &after),
        vec!["colors".to_string(), "tools".to_string()]
    );

    // 没有旧文档（文件不存在）≈ 空文档：文档里每个顶层键各算一条。
    assert_eq!(
        changed_leaf_paths(None, &json!({"colors": {"accent": "cyan"}})),
        vec!["colors".to_string()]
    );
    // 完全相同 → 空。
    assert!(changed_leaf_paths(Some(&after), &after).is_empty());
}

// ── 12. 缓存回写与面板基线 ────────────────────────────────

#[test]
fn a_successful_gateway_save_writes_the_document_back_to_the_cache() {
    let mut app = app_with_settings_panel();
    let document = json!({"gateway": {"port": 9999}});
    app.settings_save = Some(crate::app::settings::PendingSettingsSave {
        interface: InterfaceSaveReport::Skipped,
        document: document.clone(),
    });
    app.settle_gateway_save(GatewaySaveReport::Saved(settings_response(
        true,
        "fp-2",
        vec![],
    )));
    let (_, cached) = app.settings_cache.as_ref().expect("cache");
    assert_eq!(cached.values, document);
    assert_eq!(cached.fingerprint, "fp-2");
    assert!(app.settings_save.is_none());
}

#[test]
fn a_failed_gateway_save_keeps_the_cached_values() {
    let mut app = app_with_settings_panel();
    app.settings_save = Some(crate::app::settings::PendingSettingsSave {
        interface: InterfaceSaveReport::Skipped,
        document: json!({"gateway": {"port": 9999}}),
    });
    app.settle_gateway_save(GatewaySaveReport::Saved(settings_response(
        false,
        "fp-1",
        vec![SettingProblem {
            path: Some("gateway.port".into()),
            kind: "invalid_value".into(),
            message: "越界".into(),
            hint: None,
        }],
    )));
    let (_, cached) = app.settings_cache.as_ref().expect("cache");
    assert_eq!(
        cached.values,
        sample_values(),
        "ok=false 时文件没变，缓存的值照旧"
    );
    assert_eq!(cached.problems.len(), 1);
}

// ── 13. 接口签名探针（编译期契约，防止上游漂移） ─────────────

#[test]
fn the_panel_contract_used_by_the_app_is_stable() {
    // 这些调用就是 App 侧用到的全部面板 API（07 Rework r1 的清单，逐条）。
    let mut panel = SettingsPanel::new(
        &gateway_schema(),
        gateway_state(sample_values(), "fp-1"),
        Some(interface_source("cyan")),
        View::Tree,
    );
    assert!(panel.has_interface());
    assert!(!panel.is_stale());
    assert!(panel.restart_required().is_empty());
    assert_eq!(panel.dirty_count(), 0);
    assert_eq!(panel.view(), View::Tree);
    assert_eq!(panel.fingerprint(), "fp-1");
    panel.set_viewport_rows(10);
    assert_eq!(panel.visible_range(10).start, 0);
    panel.apply_save(SaveOutcome {
        gateway: None,
        interface_ok: Some(true),
    });
    panel.on_settings_changed("fp-2");
    assert!(panel.is_stale(), "指纹不同 → stale");
    panel.set_interface(interface_source("magenta"));
    assert_eq!(panel.dirty_count(), 0);
}

// ── 8b. 重启路径的早退分支（N6） ───────────────────────────

/// N6（review_r1）：`restart_gateway` 连"没有 transport"这条早退都没有测试。
/// 它不需要真网关：没有连接时直接 toast + return，**不产出任何副作用**。
#[tokio::test]
async fn restart_without_a_transport_warns_and_keeps_the_endpoint() {
    let mut app = app_with_settings_panel();
    let mut transport: Option<crate::app::Transport> = None;
    let mut endpoint = crate::app::GatewayEndpoint {
        ws_url: "ws://127.0.0.1:32523/ws".into(),
        http_base: "http://127.0.0.1:32523".into(),
        api_key: None,
    };

    app.set_connected(false);
    super::super::settings::restart_gateway(&mut app, &mut transport, &mut endpoint).await;

    assert_eq!(
        app.toast.as_ref().map(|t| t.kind),
        Some(ToastKind::Warning),
        "没有连接时给一条 warning toast"
    );
    assert_eq!(endpoint.http_base, "http://127.0.0.1:32523", "早退不碰端点");
    assert!(!app.status.connected, "早退不改连接态");
    assert!(app.drain_intents().is_empty(), "早退不产出任何副作用");
}
