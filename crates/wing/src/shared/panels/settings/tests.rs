//! 面板级测试：键位、模态性、Esc 阶梯、动作产出、回流（`apply_save` / `apply_snapshot`）。
//!
//! 每一条都用「手工目录 + 手工文档」构造状态机，断言的是**状态与产出的动作**，
//! 不看渲染（那是 08 的 `TestBackend` 帧测试）。

use std::collections::HashMap;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use serde_json::Value;
use serde_json::json;
use wing_api_client::models::ApplyScope;
use wing_api_client::models::SecretPresence;
use wing_api_client::models::SecretState;
use wing_api_client::models::SettingProblem;
use wing_api_client::models::SettingsGetResponse;
use wing_api_client::models::SettingsSchemaResponse;
use wing_api_client::models::SettingsSetResponse;

use super::ScalarKind;
use super::SettingsAction;
use super::SettingsPanel;
use super::View;
use super::test_support as fx;
use crate::shared::panels::PageKind;
use crate::shared::panels::SelectionPanel;
use crate::shared::panels::settings::InterfaceSource;
use crate::shared::panels::settings::PromptKind;
use crate::shared::panels::settings::Row;
use crate::shared::panels::settings::RowAction;
use crate::shared::panels::settings::ValueText;

// ── 夹具与工具 ───────────────────────────────────────────────

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn schema() -> SettingsSchemaResponse {
    SettingsSchemaResponse {
        version: "0.0.0".into(),
        root: fx::sample_catalog(),
        config_path: "/tmp/wing/core/config.yaml".into(),
    }
}

fn state_with(values: Value, problems: Vec<SettingProblem>) -> SettingsGetResponse {
    SettingsGetResponse {
        values,
        secrets: HashMap::new(),
        fingerprint: "fp-1".into(),
        problems,
        setup_mode: false,
        config_path: "/tmp/wing/core/config.yaml".into(),
    }
}

fn state() -> SettingsGetResponse {
    state_with(fx::sample_gateway_doc(), Vec::new())
}

fn panel() -> SettingsPanel {
    SettingsPanel::new(&schema(), state(), None, View::Tree)
}

fn interface_catalog() -> wing_api_client::models::SettingNode {
    fx::root(vec![fx::object(
        "colors",
        vec![fx::str_field("accent"), fx::bool_field("bold", false)],
    )])
}

fn interface_source() -> InterfaceSource {
    InterfaceSource {
        catalog: interface_catalog(),
        doc: json!({"colors": {"accent": "cyan"}}),
    }
}

fn panel_with_interface() -> SettingsPanel {
    SettingsPanel::new(&schema(), state(), Some(interface_source()), View::Tree)
}

fn press(panel: &mut SettingsPanel, key: KeyEvent) -> SettingsAction {
    panel.handle_key(key)
}

fn press_code(panel: &mut SettingsPanel, code: KeyCode) -> SettingsAction {
    press(panel, key(code))
}

fn type_text(panel: &mut SettingsPanel, text: &str) {
    for c in text.chars() {
        press(panel, ch(c));
    }
}

fn row_index(panel: &SettingsPanel, path: &str) -> usize {
    panel
        .rows()
        .iter()
        .position(|row| row.path == path)
        .unwrap_or_else(|| {
            panic!(
                "row {path} not found; rows = {:?}",
                panel.rows().iter().map(|row| &row.path).collect::<Vec<_>>()
            )
        })
}

/// 把光标挪到某一行（测试里代替鼠标/多次 `↓`）。
fn goto(panel: &mut SettingsPanel, path: &str) {
    let index = row_index(panel, path);
    panel.set_cursor_at(0, index);
}

/// 把光标挪到某个根的根头行。
fn goto_root(panel: &mut SettingsPanel, root: super::Root) {
    let index = panel
        .rows()
        .iter()
        .position(|row| row.root == root && row.path.is_empty())
        .expect("root row");
    panel.set_cursor_at(0, index);
}

fn row(panel: &SettingsPanel, path: &str) -> Row {
    panel.rows()[row_index(panel, path)].clone()
}

fn cursor_path(panel: &SettingsPanel) -> String {
    panel.rows()[panel.cursor()].path.clone()
}

/// 三个 provider 的文档（索引重排用）。
fn three_providers() -> Value {
    json!({
        "providers": [
            {"name": "a", "base_url": "https://a", "models": ["m-a"]},
            {"name": "b", "base_url": "https://b", "models": ["m-b"]},
            {"name": "c", "base_url": "https://c", "models": ["m-c"]}
        ],
        "gateway": {"port": 1},
        "tools": ["Bash"]
    })
}

fn panel_with(values: Value) -> SettingsPanel {
    SettingsPanel::new(&schema(), state_with(values, Vec::new()), None, View::Tree)
}

/// 列表项行（不含 `(+ 新增一项)`）的 label，按行序。
fn provider_labels(panel: &SettingsPanel) -> Vec<String> {
    panel
        .rows()
        .iter()
        .filter(|row| row.path.starts_with("providers[") && !row.path.ends_with("[]"))
        .filter(|row| {
            let rest = row.path.trim_start_matches("providers[");
            !rest.contains("].")
        })
        .map(|row| row.label.clone())
        .collect()
}

fn ok_response(problems: Vec<SettingProblem>) -> SettingsSetResponse {
    SettingsSetResponse {
        ok: problems.is_empty(),
        fingerprint: "fp-2".into(),
        problems,
        changed: vec![],
        restart_required: vec![],
        reload: None,
        setup_mode_exited: false,
        backup_path: None,
    }
}

// ── 扁平化与导航（A / B） ────────────────────────────────────

#[test]
fn initial_rows_show_both_roots_with_collapsed_top_level() {
    let panel = panel_with_interface();
    let paths: Vec<&str> = panel.rows().iter().map(|row| row.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "",
            "providers",
            "gateway",
            "tools",
            "extra_body",
            "",
            "colors"
        ],
        "两个根头行 + 各自的顶层字段（全折叠）"
    );
    assert_eq!(panel.cursor(), 0);
    assert_eq!(panel.rows()[0].root, super::Root::Gateway);
    assert_eq!(panel.rows()[5].root, super::Root::Interface);
    assert_eq!(panel.view(), View::Tree);
}

#[test]
fn enter_expands_and_collapses_structure_rows() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    assert_eq!(press_code(&mut panel, KeyCode::Enter), SettingsAction::None);
    assert!(panel.rows().iter().any(|row| row.path == "gateway.port"));
    assert_eq!(cursor_path(&panel), "gateway", "展开不移动光标");
    press_code(&mut panel, KeyCode::Enter);
    assert!(!panel.rows().iter().any(|row| row.path == "gateway.port"));
}

#[test]
fn arrow_right_expands_and_left_folds_then_jumps_to_the_parent() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    assert!(panel.rows().iter().any(|row| row.path == "gateway.port"));
    press_code(&mut panel, KeyCode::Left);
    assert!(!panel.rows().iter().any(|row| row.path == "gateway.port"));
    // 已折叠 → ← 跳到父行（根头行）。
    assert_eq!(cursor_path(&panel), "gateway");
    // 展开 → 进到子行，再按 ← = 跳回父行。
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Left);
    assert_eq!(cursor_path(&panel), "gateway");
}

#[test]
fn cursor_clamps_at_both_ends_without_wrapping() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Up);
    press_code(&mut panel, KeyCode::Up);
    assert_eq!(panel.cursor(), 0);
    let last = panel.rows().len() - 1;
    press_code(&mut panel, KeyCode::End);
    assert_eq!(panel.cursor(), last);
    press_code(&mut panel, KeyCode::Down);
    assert_eq!(panel.cursor(), last, "不绕回");
}

#[test]
fn visible_range_matches_the_kernel_window_math() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right); // 展开 tools（2 项 + 新增行）
    let len = panel.rows().len();
    assert_eq!(
        panel.visible_range(5),
        crate::shared::panels::window_range(panel.cursor(), len, 5)
    );
}

#[test]
fn page_keys_move_by_the_reported_viewport_rows() {
    let mut panel = panel();
    panel.set_viewport_rows(2);
    let before = panel.cursor();
    press_code(&mut panel, KeyCode::PageDown);
    assert_eq!(panel.cursor(), before + 2);
    press_code(&mut panel, KeyCode::PageUp);
    assert_eq!(panel.cursor(), before);
    press_code(&mut panel, KeyCode::Home);
    assert_eq!(panel.cursor(), 0);
}

#[test]
fn long_lists_keep_the_cursor_inside_a_centered_window() {
    let tools: Vec<String> = (0..30).map(|i| format!("tool-{i}")).collect();
    let mut panel = panel_with(json!({"tools": tools}));
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    // 走到第 20 项。
    for _ in 0..20 {
        press_code(&mut panel, KeyCode::Down);
    }
    let cursor = panel.cursor();
    let range = panel.visible_range(5);
    assert!(range.contains(&cursor));
    assert_eq!(range.len(), 5, "长列表用满窗口");
}

#[test]
fn selection_panel_contract_is_a_single_options_page() {
    let panel = panel();
    assert_eq!(panel.page_count(), 1);
    assert_eq!(
        panel.page_kind(0),
        PageKind::Options {
            rows: panel.rows().len()
        }
    );
    assert_eq!(panel.current_page(), 0);
    assert_eq!(panel.committed_at(0), None);
}

// ── 行标记（C） ─────────────────────────────────────────────

#[test]
fn row_markers_follow_dirty_and_problems() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "1234");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.edit_state().is_none(), "提交应当成功");
    assert!(row(&panel, "gateway.port").markers.dirty);
    assert!(row(&panel, "gateway").markers.dirty, "结构行是汇总");
    assert!(row(&panel, "").markers.dirty);
    assert!(!row(&panel, "tools").markers.dirty);
}

#[test]
fn structural_rows_report_their_apply_scope_and_required_leaves() {
    let mut panel = panel_with(json!({"providers": [{"name": "p", "base_url": "u"}]}));
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    assert!(row(&panel, "providers[0].name").markers.required);
    assert!(row(&panel, "providers[0].api_key").markers.secret);
    assert_eq!(
        row(&panel, "providers[0].name").markers.apply,
        ApplyScope::Hot
    );
}

// ── 编辑器（D / E） ─────────────────────────────────────────

#[test]
fn editing_a_string_field_commits_and_marks_dirty() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "tools[0]");
    assert_eq!(press_code(&mut panel, KeyCode::Enter), SettingsAction::None);
    let edit = panel.edit_state().expect("editor open");
    assert_eq!(edit.path(), "tools[0]");
    assert_eq!(edit.visible_buffer(), "Bash");
    // Ctrl+U 清空后重新输入。
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "Bash2");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.edit_state().is_none());
    assert_eq!(panel.rows()[row_index(&panel, "tools[0]")].label, "Bash2");
    assert!(panel.is_dirty(super::Root::Gateway, "tools[0]"));
}

#[test]
fn editing_an_int_field_rejects_garbage_and_stays_open() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "abc");
    press_code(&mut panel, KeyCode::Enter);
    let edit = panel.edit_state().expect("校验失败必须留在编辑器里");
    assert_eq!(edit.error(), Some("需要一个整数"));
    // 改对再提交。
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "8080");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.edit_state().is_none());
    assert_eq!(
        row(&panel, "gateway.port").value,
        ValueText::Text("8080".into())
    );
}

#[test]
fn editing_an_out_of_range_int_shows_the_bracket_label() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "70000");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        panel.edit_state().unwrap().error(),
        Some("取值范围 [1, 65535]")
    );
}

#[test]
fn editing_a_map_field_requires_a_json_object() {
    let mut panel = panel();
    goto(&mut panel, "extra_body");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        panel.edit_state().unwrap().visible_buffer(),
        "{\"thinking\":{\"type\":\"enabled\"}}"
    );
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "[1,2]");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        panel.edit_state().unwrap().error(),
        Some("需要一个 JSON 对象")
    );
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "{\"a\":1}");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.edit_state().is_none());
    assert!(panel.is_dirty(super::Root::Gateway, "extra_body"));
}

#[test]
fn editing_a_secret_never_exposes_the_buffer_and_an_empty_submit_keeps_the_value() {
    let mut panel = panel();
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].api_key");
    press_code(&mut panel, KeyCode::Enter);
    let edit = panel.edit_state().unwrap();
    assert!(edit.is_secret());
    assert_eq!(edit.visible_buffer(), "", "缓冲从空开始");
    type_text(&mut panel, "sk-secret-value");
    assert_eq!(
        panel.edit_state().unwrap().visible_buffer(),
        "•••••••••••••••"
    );
    assert_eq!(panel.edit_state().unwrap().buffer_len(), 15);
    // 空提交 = 取消（不改文档）。
    press(&mut panel, ctrl('u'));
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.edit_state().is_none());
    assert!(!panel.is_dirty(super::Root::Gateway, "providers[0].api_key"));
    assert_eq!(
        row(&panel, "providers[0].api_key").value,
        ValueText::Inherited,
        "文档里的 null 原样保留"
    );
    // 非空提交 = 设值。
    press_code(&mut panel, KeyCode::Enter);
    type_text(&mut panel, "sk-new-value");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.is_dirty(super::Root::Gateway, "providers[0].api_key"));
    assert_eq!(
        row(&panel, "providers[0].api_key").value,
        ValueText::Masked {
            hint: Some("alue".into())
        },
        "本地就能算出 hint"
    );
}

#[test]
fn editor_ignores_navigation_keys_and_escape_cancels() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "tools[0]");
    press_code(&mut panel, KeyCode::Enter);
    for code in [KeyCode::Up, KeyCode::Down, KeyCode::Tab] {
        assert_eq!(press_code(&mut panel, code), SettingsAction::None);
    }
    assert_eq!(panel.edit_state().unwrap().visible_buffer(), "Bash");
    assert_eq!(cursor_path(&panel), "tools[0]", "模态性：光标不动");
    press_code(&mut panel, KeyCode::Esc);
    assert!(panel.edit_state().is_none());
    assert_eq!(row(&panel, "tools[0]").label, "Bash", "取消不改值");
    assert!(!panel.is_dirty(super::Root::Gateway, "tools[0]"));
}

#[test]
fn editor_initial_buffer_falls_back_to_the_declared_default() {
    let mut panel = panel_with(json!({}));
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        panel.edit_state().unwrap().visible_buffer(),
        "0",
        "缺席 → 默认值"
    );
}

#[test]
fn readonly_rows_ignore_edit_keys() {
    let mut catalog = fx::sample_catalog();
    // tools 只读 + readonly apply scope。
    catalog.children[2].editable = false;
    catalog.children[3].apply = ApplyScope::Readonly;
    let schema = SettingsSchemaResponse {
        version: "0".into(),
        root: catalog,
        config_path: "p".into(),
    };
    let mut panel = SettingsPanel::new(&schema, state(), None, View::Tree);
    goto(&mut panel, "extra_body");
    assert_eq!(row(&panel, "extra_body").action, RowAction::ReadOnly);
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.edit_state().is_none());
    press_code(&mut panel, KeyCode::Char('r'));
    assert!(!panel.is_dirty(super::Root::Gateway, "extra_body"));
    press_code(&mut panel, KeyCode::Char('d'));
    assert!(panel.prompt().is_none());
}

// ── Esc 阶梯（F） ───────────────────────────────────────────

#[test]
fn esc_ladder_level_1_cancels_the_editor() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "tools[0]");
    press_code(&mut panel, KeyCode::Enter);
    press_code(&mut panel, KeyCode::Esc);
    assert!(panel.edit_state().is_none());
}

#[test]
fn esc_ladder_level_2_cancels_the_pending_prompt() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "tools[0]");
    press_code(&mut panel, KeyCode::Char('d'));
    assert!(panel.prompt().is_some());
    press_code(&mut panel, KeyCode::Esc);
    assert!(panel.prompt().is_none());
    assert_eq!(row(&panel, "tools[0]").label, "Bash", "取消不删");
}

#[test]
fn esc_ladder_level_3_exits_search_and_restores_expansion() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "port");
    assert!(panel.rows().iter().any(|row| row.path == "gateway.port"));
    press_code(&mut panel, KeyCode::Esc);
    assert_eq!(panel.search_query(), None);
    assert!(
        !panel.rows().iter().any(|row| row.path == "gateway.port"),
        "快照恢复"
    );
}

#[test]
fn esc_ladder_level_4_closes_help() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Char('?'));
    assert_eq!(panel.view(), View::Help);
    press_code(&mut panel, KeyCode::Esc);
    assert_eq!(panel.view(), View::Tree);
}

#[test]
fn esc_ladder_level_5_folds_the_choice_block() {
    let mut panel = panel();
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].protocol");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.choices().is_some());
    press_code(&mut panel, KeyCode::Esc);
    assert!(panel.choices().is_none());
    assert_eq!(
        cursor_path(&panel),
        "providers[0].protocol",
        "光标留在 enum 行"
    );
}

#[test]
fn esc_ladder_level_6_asks_before_discarding_dirty_changes() {
    let mut panel = panel();
    edit_port(&mut panel, "9");
    press_code(&mut panel, KeyCode::Esc);
    assert_eq!(panel.prompt().unwrap().kind, PromptKind::Confirm);
    assert_eq!(panel.prompt().unwrap().title, "放弃 1 项未保存的改动？");
    // Enter 确认 → Close{discard:true}。
    assert_eq!(
        press_code(&mut panel, KeyCode::Enter),
        SettingsAction::Close { discard: true }
    );
}

#[test]
fn esc_ladder_level_7_closes_a_clean_panel() {
    let mut panel = panel();
    assert_eq!(
        press_code(&mut panel, KeyCode::Esc),
        SettingsAction::Close { discard: false }
    );
}

fn edit_port(panel: &mut SettingsPanel, value: &str) {
    goto(panel, "gateway");
    press_code(panel, KeyCode::Right);
    goto(panel, "gateway.port");
    press_code(panel, KeyCode::Enter);
    press(panel, ctrl('u'));
    type_text(panel, value);
    press_code(panel, KeyCode::Enter);
    assert!(panel.edit_state().is_none(), "提交应当成功");
}

// ── bool / enum（G） ────────────────────────────────────────

#[test]
fn bool_toggles_with_enter_and_space() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.auth");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.auth.enabled");
    assert_eq!(
        row(&panel, "gateway.auth.enabled").value,
        ValueText::Text("enabled".into())
    );
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        row(&panel, "gateway.auth.enabled").value,
        ValueText::Text("disabled".into())
    );
    press_code(&mut panel, KeyCode::Char(' '));
    assert_eq!(
        row(&panel, "gateway.auth.enabled").value,
        ValueText::Text("enabled".into())
    );
    assert!(panel.is_dirty(super::Root::Gateway, "gateway.auth.enabled"));
}

#[test]
fn enum_enter_expands_choices_with_the_cursor_on_the_current_value() {
    let mut panel = provider_panel();
    goto(&mut panel, "providers[0].protocol");
    assert_eq!(press_code(&mut panel, KeyCode::Enter), SettingsAction::None);
    let choices = panel.choices().expect("内联选择项");
    assert_eq!(choices.path, "providers[0].protocol");
    assert_eq!(choices.cursor, 0, "当前值 = openai");
    let rows = panel.rows();
    let openai = rows
        .iter()
        .find(|row| row.path == "providers[0].protocol=openai")
        .unwrap();
    assert_eq!(
        openai.action,
        RowAction::Choose {
            value: Some("openai".into()),
            selected: true
        }
    );
    assert_eq!(openai.value, ValueText::Text("OpenAI 兼容协议".into()));
}

#[test]
fn choosing_a_value_writes_it_and_folds_the_block() {
    let mut panel = provider_panel();
    goto(&mut panel, "providers[0].protocol");
    press_code(&mut panel, KeyCode::Enter);
    press_code(&mut panel, KeyCode::Down); // 内项光标 → anthropic
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.choices().is_none());
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("anthropic".into())
    );
    assert!(panel.is_dirty(super::Root::Gateway, "providers[0].protocol"));
}

#[test]
fn enum_arrow_keys_cycle_the_value_and_close_the_block() {
    let mut panel = provider_panel();
    goto(&mut panel, "providers[0].protocol");
    press_code(&mut panel, KeyCode::Right);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("anthropic".into())
    );
    press_code(&mut panel, KeyCode::Right);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("openai".into()),
        "绕回"
    );
    press_code(&mut panel, KeyCode::Left);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("anthropic".into())
    );
    // 在 enum 行上展开选择项后，← 折叠；再 ← 继续切值。
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.choices().is_some());
    press_code(&mut panel, KeyCode::Left);
    assert!(panel.choices().is_none());
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("anthropic".into())
    );
    press_code(&mut panel, KeyCode::Left);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("openai".into())
    );
}

#[test]
fn left_and_right_from_an_absent_enum_pick_the_edges() {
    let mut panel = provider_panel();
    goto(&mut panel, "providers[0].protocol");
    // 先复位成缺席。
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::None,
        "enum 无默认 → 缺席时没有值可显示"
    );
    press_code(&mut panel, KeyCode::Right);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("openai".into())
    );
    press_code(&mut panel, KeyCode::Char('r'));
    press_code(&mut panel, KeyCode::Left);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("anthropic".into())
    );
}

#[test]
fn nullable_enum_offers_an_unset_row_that_writes_null() {
    let mut catalog = fx::sample_catalog();
    let protocol = catalog.children[0]
        .element
        .as_mut()
        .unwrap()
        .children
        .iter_mut()
        .find(|child| child.key == "protocol")
        .unwrap();
    protocol.nullable = true;
    protocol.has_default = true;
    protocol.default = Some(json!("openai"));
    let schema = SettingsSchemaResponse {
        version: "0".into(),
        root: catalog,
        config_path: "p".into(),
    };
    let mut panel = SettingsPanel::new(&schema, state(), None, View::Tree);
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].protocol");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.choices().unwrap().cursor, 0, "当前值 openai");
    press_code(&mut panel, KeyCode::Down);
    press_code(&mut panel, KeyCode::Down); // → (unset)
    let unset = panel
        .rows()
        .iter()
        .find(|row| row.path == "providers[0].protocol=(unset)")
        .cloned()
        .expect("(unset) 行");
    assert_eq!(
        unset.action,
        RowAction::Choose {
            value: None,
            selected: true
        }
    );
    assert_eq!(unset.value, ValueText::Text("跟随默认 openai".into()));
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("(unset)".into())
    );
    assert!(panel.is_dirty(super::Root::Gateway, "providers[0].protocol"));
    // 再循环：null → openai。
    press_code(&mut panel, KeyCode::Right);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("openai".into())
    );
}

#[test]
fn space_selects_a_choice_row_and_enter_on_a_choice_row_works_too() {
    let mut panel = provider_panel();
    goto(&mut panel, "providers[0].protocol");
    press_code(&mut panel, KeyCode::Enter);
    // 光标主动落到选择项行（Home/End 之外的路径：直接定位）。
    let choice_row = row_index(&panel, "providers[0].protocol=anthropic");
    panel.set_cursor_at(0, choice_row);
    assert_eq!(
        press_code(&mut panel, KeyCode::Char(' ')),
        SettingsAction::None
    );
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("anthropic".into())
    );
    assert!(panel.choices().is_none(), "选中即折叠");
    // Enter 也认。
    press_code(&mut panel, KeyCode::Enter);
    let choice_row = row_index(&panel, "providers[0].protocol=openai");
    panel.set_cursor_at(0, choice_row);
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        row(&panel, "providers[0].protocol").value,
        ValueText::Text("openai".into())
    );
}

#[test]
fn home_and_end_do_not_break_an_open_choice_block() {
    let mut panel = provider_panel();
    goto(&mut panel, "providers[0].protocol");
    press_code(&mut panel, KeyCode::Enter);
    press_code(&mut panel, KeyCode::Home);
    assert_eq!(panel.cursor(), 0);
    // ↑ 只在光标仍停在 enum 行时驱动内项光标。
    press_code(&mut panel, KeyCode::Up);
    assert_eq!(panel.choices().unwrap().cursor, 0, "内项光标没动");
}

/// provider 面板：展开到 protocol 行可见。
fn provider_panel() -> SettingsPanel {
    let mut panel = panel();
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    panel
}

// ── 列表（H） ───────────────────────────────────────────────

#[test]
fn adding_a_scalar_item_appends_and_opens_the_editor() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "tools[]");
    assert_eq!(
        row(&panel, "tools[]").action,
        RowAction::AddItem { variants: 1 }
    );
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.rows()[row_index(&panel, "tools[2]")].label, "");
    let edit = panel.edit_state().expect("标量项立即进入编辑器");
    assert_eq!(edit.path(), "tools[2]");
    type_text(&mut panel, "Grep");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.rows()[row_index(&panel, "tools[2]")].label, "Grep");
    assert!(panel.is_dirty(super::Root::Gateway, "tools"));
    // `a` 快捷方式也一样。
    press_code(&mut panel, KeyCode::Char('a'));
    assert_eq!(panel.edit_state().unwrap().path(), "tools[3]");
    press_code(&mut panel, KeyCode::Esc);
}

#[test]
fn adding_an_object_item_inserts_a_stub_expands_it_and_moves_the_cursor() {
    let mut panel = panel();
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Char('a'));
    assert!(panel.prompt().is_none(), "单一形态不需要选形态");
    assert!(
        panel.rows().iter().any(|row| row.path == "providers[1]"),
        "自动展开列表与新项"
    );
    assert_eq!(
        cursor_path(&panel),
        "providers[1].name",
        "光标落在第一个必填字段"
    );
    assert_eq!(
        panel.rows()[row_index(&panel, "providers[1]")].label,
        "",
        "stub 只写了必填字段（name 空串）；protocol 缺席 → 摘要行是空的"
    );
    assert!(
        panel
            .rows()
            .iter()
            .any(|row| row.path == "providers[1].name")
    );
    assert!(panel.is_dirty(super::Root::Gateway, "providers"));
}

#[test]
fn union_list_add_asks_for_a_shape_first() {
    let mut panel = provider_panel();
    goto(&mut panel, "providers[0].models");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].models[]");
    assert_eq!(
        row(&panel, "providers[0].models[]").action,
        RowAction::AddItem { variants: 2 }
    );
    press_code(&mut panel, KeyCode::Char('a'));
    let prompt = panel.prompt().expect("先选形态");
    assert_eq!(prompt.kind, PromptKind::Variants);
    assert_eq!(prompt.cursor, 0);
    assert_eq!(prompt.options.len(), 2);
    // ↓ 选完整形态 → Enter。
    press_code(&mut panel, KeyCode::Down);
    assert_eq!(panel.prompt().unwrap().cursor, 1);
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.prompt().is_none());
    assert_eq!(
        row(&panel, "providers[0].models[1]").action,
        RowAction::Expand,
        "对象形态是结构行"
    );
    assert!(
        panel
            .rows()
            .iter()
            .any(|row| row.path == "providers[0].models[1].id")
    );
    // 简单形态：a → Enter（光标默认在 0）。
    goto(&mut panel, "providers[0].models[]");
    press_code(&mut panel, KeyCode::Char('a'));
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        panel.edit_state().map(|edit| edit.path().to_string()),
        Some("providers[0].models[2]".into()),
        "简单形态立即编辑"
    );
}

#[test]
fn deleting_a_list_item_needs_confirmation_and_remaps_indices() {
    let mut panel = panel_with(three_providers());
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].name");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "a2");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.is_dirty(super::Root::Gateway, "providers[0].name"));
    // 展开 providers[2]，并在它上面改一个字段。
    goto(&mut panel, "providers[2]");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[2].name");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "c2");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.is_dirty(super::Root::Gateway, "providers[2].name"));
    assert!(
        panel
            .expanded
            .contains(&(super::Root::Gateway, "providers[2]".to_string())),
        "providers[2] 展开着"
    );
    // 删除 providers[0]。
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Char('d'));
    let prompt = panel.prompt().unwrap();
    assert_eq!(prompt.title, "删除 \"a2\"？");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.prompt().is_none());
    // 原来 [1] 补位成 [0]，原来 [2] 补位成 [1]。
    assert_eq!(provider_labels(&panel), vec!["b", "c2"]);
    assert!(
        !panel
            .expanded
            .contains(&(super::Root::Gateway, "providers[2]".to_string())),
        "旧 [2] 的展开标记必须被重排"
    );
    assert!(
        panel
            .expanded
            .contains(&(super::Root::Gateway, "providers[1]".to_string())),
        "展开标记跟到补位后的 [1]"
    );
    assert!(
        !panel.is_dirty(super::Root::Gateway, "providers[2].name"),
        "旧 [2] 的脏路径被重排"
    );
    assert!(
        panel.is_dirty(super::Root::Gateway, "providers[1].name"),
        "c2 的脏标记跟到 [1]"
    );
    assert!(
        !panel.is_dirty(super::Root::Gateway, "providers[0].name"),
        "被删项自己的脏标记丢弃；补位的 b 没改过"
    );
    assert_eq!(cursor_path(&panel), "providers[0]", "光标落到补位项");
}

#[test]
fn deleting_the_last_item_anchors_the_add_row() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "tools[1]");
    press_code(&mut panel, KeyCode::Char('d'));
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(row(&panel, "tools[0]").label, "Bash");
    assert_eq!(cursor_path(&panel), "tools[]", "删掉末项 → 光标到新增行");
    assert!(panel.is_dirty(super::Root::Gateway, "tools"));
}

#[test]
fn moving_items_with_shift_j_k_swaps_them_and_remaps_marks() {
    let mut panel = panel_with(three_providers());
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[1]");
    press_code(&mut panel, KeyCode::Char('J'));
    assert_eq!(provider_labels(&panel), vec!["a", "c", "b"]);
    assert_eq!(cursor_path(&panel), "providers[2]", "光标跟着被移动的项");
    press_code(&mut panel, KeyCode::Char('K'));
    assert_eq!(provider_labels(&panel), vec!["a", "b", "c"]);
    assert!(panel.is_dirty(super::Root::Gateway, "providers"));
}

#[test]
fn moving_at_the_boundary_is_a_no_op() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "tools[0]");
    press_code(&mut panel, KeyCode::Char('K'));
    assert_eq!(row(&panel, "tools[0]").label, "Bash", "到顶不动");
    goto(&mut panel, "tools[1]");
    press_code(&mut panel, KeyCode::Char('J'));
    assert_eq!(row(&panel, "tools[1]").label, "Read", "到底不动");
}

#[test]
fn removing_the_last_list_item_reports_min_items_and_shows_the_consequence() {
    let mut panel = panel_with(json!({
        "providers": [{"name": "solo", "base_url": "u", "models": ["m"]}]
    }));
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Char('d'));
    let prompt = panel.prompt().unwrap();
    assert!(
        prompt
            .lines
            .iter()
            .any(|line| line.contains("至少需要 1 项")),
        "{:?}",
        prompt.lines
    );
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(row(&panel, "providers").label, "providers (0)");
    assert!(row(&panel, "providers").markers.problem);
    assert!(panel.problems().iter().any(
        |problem| problem.path.as_deref() == Some("providers") && problem.kind == "empty_list"
    ));
}

#[test]
fn add_row_is_absent_at_max_items_so_add_does_nothing() {
    let mut catalog = fx::sample_catalog();
    catalog.children[2].max_items = Some(2);
    let schema = SettingsSchemaResponse {
        version: "0".into(),
        root: catalog,
        config_path: "p".into(),
    };
    let mut panel = SettingsPanel::new(&schema, state(), None, View::Tree);
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    assert!(!panel.rows().iter().any(|row| row.path == "tools[]"));
    goto(&mut panel, "tools[0]");
    assert_eq!(
        press_code(&mut panel, KeyCode::Char('a')),
        SettingsAction::None
    );
    assert!(panel.edit_state().is_none());
}

// ── 复位（I） ───────────────────────────────────────────────

#[test]
fn reset_removes_the_key_and_marks_dirty() {
    let mut panel = panel();
    goto(&mut panel, "tools");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "tools[0]");
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(
        press_code(&mut panel, KeyCode::Char('d')),
        SettingsAction::None,
        "标量列表项上的 d 是删除项（有确认），不是复位"
    );
    assert!(panel.prompt().is_some());
    press_code(&mut panel, KeyCode::Esc);
    // 真·复位：叶子字段。
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(
        row(&panel, "gateway.port").value,
        ValueText::Default("0".into())
    );
    assert!(panel.is_dirty(super::Root::Gateway, "gateway.port"));
    // 再按一次：已经没有显式值 → 无操作（脏路径数不涨）。
    let dirty = panel.dirty_count();
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(panel.dirty_count(), dirty);
}

#[test]
fn reset_on_an_absent_key_is_a_no_op() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Char('r'));
    assert!(panel.is_dirty(super::Root::Gateway, "gateway.port"));
    press_code(&mut panel, KeyCode::Char('r'));
    assert!(
        panel.is_dirty(super::Root::Gateway, "gateway.port"),
        "脏标记是累积的（改动即脏）"
    );
    let dirty_before = panel.dirty_count();
    // 再复位一次不会新增脏路径。
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(panel.dirty_count(), dirty_before);
}

#[test]
fn reset_on_a_list_item_or_structure_row_is_a_no_op() {
    let mut panel = panel();
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(panel.dirty_count(), 0, "结构行不复位");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(panel.dirty_count(), 0, "列表项行不复位");
}

#[test]
fn resetting_a_field_of_a_newly_added_item_falls_back_to_default() {
    let mut panel = panel();
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Char('a'));
    press_code(&mut panel, KeyCode::Enter); // 在第一个必填字段上开编辑器
    type_text(&mut panel, "newbie");
    press_code(&mut panel, KeyCode::Enter); // 提交 name
    assert_eq!(
        row(&panel, "providers[1].name").value,
        ValueText::Text("newbie".into())
    );
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(
        row(&panel, "providers[1].name").value,
        ValueText::Text("(required)".into()),
        "必填字段复位 → 立刻变成 required 问题"
    );
    assert!(
        panel
            .problems()
            .iter()
            .any(|problem| problem.path.as_deref() == Some("providers[1].name"))
    );
}

#[test]
fn reset_a_secret_back_to_not_set() {
    let mut panel = panel();
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].api_key");
    press_code(&mut panel, KeyCode::Char('r'));
    assert_eq!(
        row(&panel, "providers[0].api_key").value,
        ValueText::Text("(not set)".into())
    );
    assert!(panel.is_dirty(super::Root::Gateway, "providers[0].api_key"));
}

// ── 搜索（J） ───────────────────────────────────────────────

#[test]
fn search_filters_and_jumps_to_the_first_hit() {
    let mut panel = panel_with_interface();
    press_code(&mut panel, KeyCode::Char('/'));
    assert_eq!(panel.search_query(), Some(""));
    type_text(&mut panel, "port");
    assert_eq!(panel.search_query(), Some("port"));
    assert!(panel.search_hits() >= 1);
    assert_eq!(cursor_path(&panel), "gateway.port", "光标跳到第一个命中");
    assert!(
        panel.rows().iter().all(|row| {
            row.path.is_empty() || row.path == "gateway" || row.path == "gateway.port"
        })
    );
}

#[test]
fn search_expands_ancestors_and_hides_siblings() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "api_key");
    assert!(panel.rows().iter().any(|row| row.path == "providers"));
    assert!(panel.rows().iter().any(|row| row.path == "providers[0]"));
    assert!(
        panel
            .rows()
            .iter()
            .any(|row| row.path == "providers[0].api_key")
    );
    assert!(
        !panel.rows().iter().any(|row| row.path == "gateway"),
        "未命中的兄弟隐藏"
    );
}

#[test]
fn search_shows_direct_children_of_a_matching_structure_node() {
    // 只让列表节点自己命中（查询词不在任何子节点的元信息里）。
    let mut catalog = fx::sample_catalog();
    catalog.children[0].doc = "zzz-unique".into();
    let schema = SettingsSchemaResponse {
        version: "0".into(),
        root: catalog,
        config_path: "p".into(),
    };
    let mut panel = SettingsPanel::new(&schema, state(), None, View::Tree);
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "zzz-unique");
    assert!(panel.rows().iter().any(|row| row.path == "providers"));
    assert!(
        panel.rows().iter().any(|row| row.path == "providers[0]"),
        "命中结构节点要连带展示直接子节点"
    );
    assert!(
        !panel
            .rows()
            .iter()
            .any(|row| row.path == "providers[0].name"),
        "只钻一层"
    );
}

#[test]
fn enter_keeps_the_filtered_position_and_expands_its_ancestors() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "api_key");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.search_query(), None);
    assert_eq!(cursor_path(&panel), "providers[0].api_key");
    assert!(
        panel.rows().iter().any(|row| row.path == "gateway"),
        "退出搜索 = 完整树"
    );
    assert!(
        panel.rows().iter().any(|row| row.path == "providers[0]"),
        "祖先保持展开"
    );
}

#[test]
fn esc_restores_the_expansion_snapshot_taken_before_the_search() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "api_key");
    assert!(panel.rows().iter().any(|row| row.path == "providers[0]"));
    press_code(&mut panel, KeyCode::Esc);
    assert_eq!(panel.search_query(), None);
    assert!(
        !panel.rows().iter().any(|row| row.path == "providers[0]"),
        "展开状态恢复到搜索前的快照（一切折叠）"
    );
    assert_eq!(cursor_path(&panel), "", "光标回到搜索前那一行");
}

#[test]
fn search_spans_cross_roots_and_navigation_moves_inside_the_filter() {
    let mut panel = panel_with_interface();
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "bold");
    assert_eq!(cursor_path(&panel), "colors.bold");
    assert_eq!(panel.rows()[panel.cursor()].root, super::Root::Interface);
    // 在过滤结果里导航。
    press_code(&mut panel, KeyCode::Up);
    press_code(&mut panel, KeyCode::Down);
    assert_eq!(cursor_path(&panel), "colors.bold");
    press_code(&mut panel, KeyCode::Esc);
    assert!(!panel.rows().iter().any(|row| row.path == "colors.bold"));
}

#[test]
fn search_keys_go_to_the_query_not_to_actions() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "sa"); // s / a 都不能触发保存或新增
    assert_eq!(panel.search_query(), Some("sa"));
    assert_eq!(panel.dirty_count(), 0);
    assert!(panel.edit_state().is_none());
    press(&mut panel, ctrl('u'));
    assert_eq!(panel.search_query(), Some(""));
    press_code(&mut panel, KeyCode::Esc);
}

#[test]
fn search_match_spans_highlight_the_label() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "PORT");
    let row = panel
        .rows()
        .iter()
        .find(|row| row.path == "gateway.port")
        .cloned()
        .unwrap();
    assert_eq!(
        row.match_spans
            .iter()
            .map(|span| &row.label[span.clone()])
            .collect::<Vec<_>>(),
        vec!["port"],
        "大小写不敏感的高亮片段"
    );
}

// ── 问题清单（K） ───────────────────────────────────────────

#[test]
fn problems_view_lists_backend_and_local_problems_sorted_and_deduplicated() {
    let backend = vec![SettingProblem {
        path: Some("providers".into()),
        kind: "empty_list".into(),
        message: "providers 不得为空".into(),
        hint: None,
    }];
    let panel = SettingsPanel::new(
        &schema(),
        state_with(json!({"providers": [{"protocol": "openai"}]}), backend),
        None,
        View::Problems,
    );
    assert_eq!(panel.view(), View::Problems);
    let paths: Vec<_> = panel
        .problems()
        .iter()
        .map(|problem| (problem.path.as_deref(), problem.kind.as_str()))
        .collect();
    assert_eq!(
        paths,
        vec![
            (Some("providers[0].base_url"), "missing_required"),
            (Some("providers[0].name"), "missing_required"),
            (Some("providers"), "empty_list"),
            (Some("providers[0].models"), "empty_list"),
        ],
        "严重度优先（missing_required 在前），同档按路径"
    );
}

#[test]
fn enter_on_a_problem_jumps_to_the_row_and_expands_ancestors() {
    let backend = vec![SettingProblem {
        path: Some("providers[0].api_key".into()),
        kind: "missing_required".into(),
        message: "必填项未设置".into(),
        hint: None,
    }];
    let mut panel = SettingsPanel::new(
        &schema(),
        state_with(fx::sample_gateway_doc(), backend),
        None,
        View::Problems,
    );
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.view(), View::Tree);
    assert_eq!(cursor_path(&panel), "providers[0].api_key");
    assert!(
        panel
            .rows()
            .iter()
            .any(|row| row.path == "providers[0].api_key")
    );
}

#[test]
fn enter_on_a_document_level_problem_does_nothing() {
    let backend = vec![SettingProblem {
        path: None,
        kind: "invalid_value".into(),
        message: "YAML 语法错".into(),
        hint: None,
    }];
    let mut panel = SettingsPanel::new(
        &schema(),
        state_with(fx::sample_gateway_doc(), backend),
        None,
        View::Problems,
    );
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.view(), View::Problems, "无法定位的问题不跳");
}

#[test]
fn problems_view_escape_and_p_return_to_the_tree_and_slash_starts_search() {
    let mut panel = SettingsPanel::new(&schema(), state(), None, View::Problems);
    press_code(&mut panel, KeyCode::Esc);
    assert_eq!(panel.view(), View::Tree);
    press_code(&mut panel, KeyCode::Char('p'));
    assert_eq!(panel.view(), View::Problems);
    press_code(&mut panel, KeyCode::Char('p'));
    assert_eq!(panel.view(), View::Tree);
    press_code(&mut panel, KeyCode::Char('p'));
    press_code(&mut panel, KeyCode::Char('/'));
    assert_eq!(panel.view(), View::Tree);
    assert_eq!(panel.search_query(), Some(""));
    press_code(&mut panel, KeyCode::Esc);
}

#[test]
fn problem_cursor_moves_and_clamps() {
    let backend = vec![
        SettingProblem {
            path: Some("providers".into()),
            kind: "empty_list".into(),
            message: "a".into(),
            hint: None,
        },
        SettingProblem {
            path: Some("tools".into()),
            kind: "empty_list".into(),
            message: "b".into(),
            hint: None,
        },
    ];
    let mut panel = SettingsPanel::new(
        &schema(),
        state_with(json!({"providers": [], "tools": []}), backend),
        None,
        View::Problems,
    );
    assert!(panel.problems().len() >= 2);
    press_code(&mut panel, KeyCode::Up);
    assert_eq!(panel.problem_cursor(), 0);
    press_code(&mut panel, KeyCode::Down);
    assert_eq!(panel.problem_cursor(), 1);
    press_code(&mut panel, KeyCode::End);
    assert_eq!(panel.problem_cursor(), panel.problems().len() - 1);
    press_code(&mut panel, KeyCode::Down);
    assert_eq!(panel.problem_cursor(), panel.problems().len() - 1, "不绕");
}

// ── 脏标记 / 保存 / 重载（L） ────────────────────────────────

#[test]
fn save_without_changes_emits_nothing() {
    let mut panel = panel();
    assert_eq!(
        press_code(&mut panel, KeyCode::Char('s')),
        SettingsAction::None
    );
}

#[test]
fn save_reports_which_side_changed() {
    // 只改 Gateway。
    let mut panel = panel_with_interface();
    edit_port(&mut panel, "8080");
    let action = press_code(&mut panel, KeyCode::Char('s'));
    match action {
        SettingsAction::Save {
            gateway_dirty,
            interface_dirty,
            base,
            gateway,
            ..
        } => {
            assert!(gateway_dirty);
            assert!(!interface_dirty);
            assert_eq!(base, "fp-1");
            assert_eq!(gateway["gateway"]["port"], json!(8080));
        }
        other => panic!("expected Save, got {other:?}"),
    }
    // 只改 Interface。
    let mut panel = panel_with_interface();
    goto_root(&mut panel, super::Root::Interface);
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "colors");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "colors.accent");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "magenta");
    press_code(&mut panel, KeyCode::Enter);
    match press_code(&mut panel, KeyCode::Char('s')) {
        SettingsAction::Save {
            gateway_dirty,
            interface_dirty,
            interface,
            ..
        } => {
            assert!(!gateway_dirty);
            assert!(interface_dirty);
            assert_eq!(interface["colors"]["accent"], json!("magenta"));
        }
        other => panic!("expected Save, got {other:?}"),
    }
}

#[test]
fn every_interface_commit_emits_a_preview_with_the_whole_document() {
    let mut panel = panel_with_interface();
    goto_root(&mut panel, super::Root::Interface);
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "colors");
    press_code(&mut panel, KeyCode::Right);
    // 字符串编辑。
    goto(&mut panel, "colors.accent");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "magenta");
    match press_code(&mut panel, KeyCode::Enter) {
        SettingsAction::PreviewInterface(doc) => {
            assert_eq!(doc["colors"]["accent"], json!("magenta"));
            assert!(
                doc["colors"].get("bold").is_none(),
                "整份**稀疏**文档：没碰过的键缺席（10 用 appconfig_from_doc 合并默认）"
            );
        }
        other => panic!("expected preview, got {other:?}"),
    }
    // bool 切换。
    goto(&mut panel, "colors.bold");
    match press_code(&mut panel, KeyCode::Char(' ')) {
        SettingsAction::PreviewInterface(doc) => assert_eq!(doc["colors"]["bold"], json!(true)),
        other => panic!("expected preview, got {other:?}"),
    }
    // 复位。
    goto(&mut panel, "colors.accent");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "x");
    press_code(&mut panel, KeyCode::Enter);
    match press_code(&mut panel, KeyCode::Char('r')) {
        SettingsAction::PreviewInterface(doc) => assert!(doc["colors"].get("accent").is_none()),
        other => panic!("expected preview, got {other:?}"),
    }
    assert!(panel.is_dirty(super::Root::Interface, "colors.accent"));
}

#[test]
fn gateway_edits_never_preview() {
    let mut panel = panel_with_interface();
    edit_port(&mut panel, "8080");
    assert_eq!(panel.dirty_count(), 1, "只脏 gateway");
    assert!(!panel.is_dirty(super::Root::Interface, "colors.accent"));
}

#[test]
fn successful_save_clears_dirty_and_updates_the_fingerprint() {
    let mut panel = panel();
    edit_port(&mut panel, "8080");
    assert_eq!(panel.dirty_count(), 1);
    panel.apply_save(super::SaveOutcome {
        gateway: Some(ok_response(Vec::new())),
        interface_ok: None,
    });
    assert_eq!(panel.dirty_count(), 0);
    assert_eq!(panel.fingerprint(), "fp-2");
    assert_eq!(
        row(&panel, "gateway.port").value,
        ValueText::Text("8080".into()),
        "值保留"
    );
}

#[test]
fn failed_save_keeps_dirty_and_jumps_to_the_problems_view() {
    let mut panel = panel();
    edit_port(&mut panel, "8080");
    assert!(panel.edit_state().is_none(), "本地校验通过");
    let problems = vec![SettingProblem {
        path: Some("gateway.port".into()),
        kind: "invalid_value".into(),
        message: "端口被占用".into(),
        hint: None,
    }];
    panel.apply_save(super::SaveOutcome {
        gateway: Some(SettingsSetResponse {
            ok: false,
            fingerprint: "fp-1".into(),
            problems: problems.clone(),
            changed: vec![],
            restart_required: vec![],
            reload: None,
            setup_mode_exited: false,
            backup_path: None,
        }),
        interface_ok: None,
    });
    assert_eq!(panel.view(), View::Problems);
    assert_eq!(panel.dirty_count(), 1, "失败保留脏标记");
    assert_eq!(panel.problems().len(), 1);
    // 修好再保存成功 → 回树视图之外，脏标记清空（视图由 10 决定重开）。
    panel.apply_save(super::SaveOutcome {
        gateway: Some(ok_response(Vec::new())),
        interface_ok: None,
    });
    assert_eq!(panel.dirty_count(), 0);
    assert!(panel.problems().is_empty());
}

#[test]
fn reload_when_clean_emits_reload_and_when_dirty_asks_first() {
    let mut panel = panel();
    assert_eq!(
        press_code(&mut panel, KeyCode::Char('R')),
        SettingsAction::Reload
    );
    edit_port(&mut panel, "8080");
    assert_eq!(
        press_code(&mut panel, KeyCode::Char('R')),
        SettingsAction::None
    );
    let prompt = panel.prompt().expect("有脏改动先确认");
    assert_eq!(prompt.title, "放弃 1 项未保存的改动并重新载入？");
    assert_eq!(
        press_code(&mut panel, KeyCode::Enter),
        SettingsAction::Reload
    );
}

#[test]
fn ctrl_r_is_accepted_only_when_a_restart_is_pending() {
    // AD1：没有待重启的变更 → 无操作（键位栏也不显示这个键）。
    let mut panel = panel();
    assert_eq!(press(&mut panel, ctrl('r')), SettingsAction::None);
    assert!(!panel.footer_hint().contains("Ctrl+R"));
    // 一次带 restart_required 的保存回执之后，键才生效、才出现在键位栏。
    mark_restart_pending(&mut panel);
    assert!(panel.footer_hint().contains("Ctrl+R 立即重启"));
    assert_eq!(press(&mut panel, ctrl('r')), SettingsAction::RestartGateway);
}

#[test]
fn ctrl_r_asks_before_discarding_dirty_changes() {
    let mut panel = panel();
    mark_restart_pending(&mut panel);
    edit_port(&mut panel, "8080");
    assert_eq!(press(&mut panel, ctrl('r')), SettingsAction::None);
    assert_eq!(
        panel.prompt().unwrap().title,
        "放弃 1 项未保存的改动并重启网关？"
    );
    assert_eq!(
        press_code(&mut panel, KeyCode::Enter),
        SettingsAction::RestartGateway
    );
}

/// 灌一次「带 restart_required」的保存回执（Ctrl+R 的前提）。
fn mark_restart_pending(panel: &mut SettingsPanel) {
    let mut response = ok_response(Vec::new());
    response.restart_required = vec!["gateway.port".into()];
    panel.apply_save(super::SaveOutcome {
        gateway: Some(response),
        interface_ok: None,
    });
}

#[test]
fn apply_snapshot_replaces_the_document_and_resets_the_baseline() {
    let mut panel = panel();
    edit_port(&mut panel, "8080");
    assert_eq!(panel.dirty_count(), 1);
    let schema = schema();
    let state = state_with(
        json!({"gateway": {"port": 1}, "tools": ["Bash"]}),
        Vec::new(),
    );
    panel.apply_snapshot(&schema, state);
    assert_eq!(panel.dirty_count(), 0, "重载丢弃本地改动");
    assert_eq!(panel.fingerprint(), "fp-1");
    assert_eq!(
        row(&panel, "gateway.port").value,
        ValueText::Text("1".into()),
        "展开集被剪过之后仍能看到旧行吗？——根仍展开，但要重新展开 gateway"
    );
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    assert_eq!(
        row(&panel, "gateway.port").value,
        ValueText::Text("1".into())
    );
}

#[test]
fn restart_required_is_tracked_from_the_last_save() {
    let mut panel = panel();
    edit_port(&mut panel, "8080");
    let mut response = ok_response(Vec::new());
    response.restart_required = vec!["gateway.port".into()];
    panel.apply_save(super::SaveOutcome {
        gateway: Some(response),
        interface_ok: None,
    });
    assert_eq!(panel.restart_required(), &["gateway.port".to_string()]);
    assert!(panel.footer_hint().contains("保存"));
}

#[test]
fn stale_fingerprint_marks_the_banner_and_a_matching_one_clears_it() {
    let mut panel = panel();
    assert!(!panel.is_stale());
    panel.on_settings_changed("fp-other");
    assert!(panel.is_stale(), "别的客户端改过 → 横幅");
    panel.on_settings_changed("fp-1");
    assert!(!panel.is_stale(), "指纹一致（可能就是自己刚触发的那次）");
    panel.on_settings_changed("fp-other");
    panel.apply_save(super::SaveOutcome {
        gateway: Some(ok_response(Vec::new())),
        interface_ok: None,
    });
    assert!(!panel.is_stale(), "保存成功把本地指纹推到最新");
}

// ── Interface 根（D2 / D4） ─────────────────────────────────

#[test]
fn tab_switches_between_roots_and_expands_the_target() {
    let mut panel = panel_with_interface();
    press_code(&mut panel, KeyCode::Tab);
    assert_eq!(panel.rows()[panel.cursor()].root, super::Root::Interface);
    assert_eq!(cursor_path(&panel), "");
    press_code(&mut panel, KeyCode::Right);
    assert!(panel.rows().iter().any(|row| row.path == "colors"));
    press_code(&mut panel, KeyCode::Tab);
    assert_eq!(panel.rows()[panel.cursor()].root, super::Root::Gateway);
}

#[test]
fn tab_without_interface_is_a_no_op() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Tab);
    assert_eq!(panel.rows()[panel.cursor()].root, super::Root::Gateway);
    assert!(!panel.has_interface());
}

#[test]
fn set_interface_injects_a_root_document_and_clears_its_dirty() {
    let mut panel = panel();
    assert!(!panel.has_interface());
    panel.set_interface(interface_source());
    assert!(panel.has_interface());
    assert_eq!(
        panel.rows()[panel.cursor()].path,
        "",
        "光标锚点保住（回到 Gateway 根）"
    );
    assert!(
        panel
            .rows()
            .iter()
            .any(|row| row.root == super::Root::Interface)
    );
    // 改一笔再注入新文档 → 该根脏标记清空、文档换新。
    goto_root(&mut panel, super::Root::Interface);
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "colors");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "colors.accent");
    press_code(&mut panel, KeyCode::Enter);
    press(&mut panel, ctrl('u'));
    type_text(&mut panel, "x");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.is_dirty(super::Root::Interface, "colors.accent"));
    panel.set_interface(InterfaceSource {
        catalog: interface_catalog(),
        doc: json!({"colors": {"accent": "blue"}}),
    });
    assert_eq!(panel.dirty_count(), 0);
    assert_eq!(
        row(&panel, "colors.accent").value,
        ValueText::Text("blue".into())
    );
}

// ── 模态提示（10.2） ────────────────────────────────────────

#[test]
fn delete_confirm_prompt_shows_consequences() {
    let mut catalog = fx::sample_catalog();
    catalog.children[0].apply = ApplyScope::NextSession;
    let schema = SettingsSchemaResponse {
        version: "0".into(),
        root: catalog,
        config_path: "p".into(),
    };
    let mut panel = SettingsPanel::new(&schema, state(), None, View::Tree);
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Char('d'));
    let prompt = panel.prompt().unwrap();
    assert_eq!(prompt.kind, PromptKind::Confirm);
    assert_eq!(prompt.title, "删除 \"default · openai\"？");
    assert!(
        prompt
            .lines
            .iter()
            .any(|line| line.contains("至少需要 1 项")),
        "打破 min_items 的后果"
    );
    assert!(
        prompt.lines.iter().any(|line| line.contains("新会话")),
        "生效域后果"
    );
    assert!(prompt.options.is_empty(), "纯确认");
}

#[test]
fn clear_scalar_confirm_removes_the_key() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Char('d'));
    let prompt = panel.prompt().unwrap();
    assert_eq!(prompt.title, "清空 \"port\"？");
    assert_eq!(press_code(&mut panel, KeyCode::Enter), SettingsAction::None);
    assert_eq!(
        row(&panel, "gateway.port").value,
        ValueText::Default("0".into())
    );
    assert!(panel.is_dirty(super::Root::Gateway, "gateway.port"));
}

#[test]
fn prompt_enter_and_escape_are_the_only_modal_keys() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Char('d'));
    let before = cursor_path(&panel);
    press_code(&mut panel, KeyCode::Char('x'));
    press_code(&mut panel, KeyCode::Char('r'));
    assert!(panel.prompt().is_some(), "提示开着时其它键无效");
    assert_eq!(cursor_path(&panel), before);
    press_code(&mut panel, KeyCode::Esc);
    assert!(panel.prompt().is_none());
}

// ── 动作穷尽性（12.4） ──────────────────────────────────────

fn action_name(action: &SettingsAction) -> &'static str {
    match action {
        SettingsAction::None => "None",
        SettingsAction::PreviewInterface(_) => "PreviewInterface",
        SettingsAction::Save { .. } => "Save",
        SettingsAction::RestartGateway => "RestartGateway",
        SettingsAction::Close { discard: false } => "Close{false}",
        SettingsAction::Close { discard: true } => "Close{true}",
        SettingsAction::Reload => "Reload",
    }
}

#[test]
fn every_settings_action_variant_is_produced_by_keys() {
    let mut seen = Vec::new();

    // None + Close{discard:false}：干净面板直接 Esc。
    let mut plain = panel();
    seen.push(press_code(&mut plain, KeyCode::Esc));
    seen.push(press_code(&mut plain, KeyCode::Down));

    // PreviewInterface + Save：Interface 根上改一笔再保存。
    let mut preview = panel_with_interface();
    goto_root(&mut preview, super::Root::Interface);
    press_code(&mut preview, KeyCode::Right);
    goto(&mut preview, "colors");
    press_code(&mut preview, KeyCode::Right);
    goto(&mut preview, "colors.accent");
    press_code(&mut preview, KeyCode::Enter);
    press(&mut preview, ctrl('u'));
    type_text(&mut preview, "magenta");
    seen.push(press_code(&mut preview, KeyCode::Enter));
    seen.push(press_code(&mut preview, KeyCode::Char('s')));

    // Reload：干净面板按 R。
    let mut reload = panel();
    seen.push(press_code(&mut reload, KeyCode::Char('R')));

    // RestartGateway：有待重启的变更时按 Ctrl+R。
    let mut restart = panel();
    mark_restart_pending(&mut restart);
    seen.push(press(&mut restart, ctrl('r')));

    // Close{discard:true}：脏 + Esc + Enter。
    let mut dirty = panel();
    edit_port(&mut dirty, "8080");
    press_code(&mut dirty, KeyCode::Esc);
    seen.push(press_code(&mut dirty, KeyCode::Enter));

    let mut names: Vec<&str> = seen.iter().map(action_name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(
        names,
        vec![
            "Close{false}",
            "Close{true}",
            "None",
            "PreviewInterface",
            "Reload",
            "RestartGateway",
            "Save",
        ],
        "七个动作类别都要被按键真实产出过"
    );
}

#[test]
fn every_row_action_variant_is_reachable_from_the_tree() {
    // 带只读字段的目录 + 展开的列表 + 展开的 enum，一次收齐所有 RowAction。
    let mut catalog = fx::sample_catalog();
    let mut readonly_field = fx::str_field("readonly_note");
    readonly_field.editable = false;
    catalog.children[1].children.push(readonly_field); // gateway.readonly_note → ReadOnly
    let schema = SettingsSchemaResponse {
        version: "0".into(),
        root: catalog,
        config_path: "p".into(),
    };
    let mut panel = SettingsPanel::new(&schema, state(), None, View::Tree);
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].models");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].protocol");
    press_code(&mut panel, KeyCode::Enter); // 展开选择项
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.auth");
    press_code(&mut panel, KeyCode::Right);

    let mut kinds = Vec::new();
    for row in panel.rows() {
        let name = match &row.action {
            RowAction::Expand => "Expand",
            RowAction::Toggle => "Toggle",
            RowAction::OpenChoices => "OpenChoices",
            RowAction::Edit(ScalarKind::Str) => "Edit(Str)",
            RowAction::Edit(ScalarKind::Int) => "Edit(Int)",
            RowAction::Edit(ScalarKind::Float) => "Edit(Float)",
            RowAction::Edit(ScalarKind::Secret) => "Edit(Secret)",
            RowAction::Edit(ScalarKind::Json) => "Edit(Json)",
            RowAction::AddItem { .. } => "AddItem",
            RowAction::Choose { .. } => "Choose",
            RowAction::ReadOnly => "ReadOnly",
        };
        kinds.push(name);
    }
    for expected in [
        "Expand",
        "Toggle",
        "OpenChoices",
        "Edit(Str)",
        "Edit(Int)",
        "Edit(Float)",
        "Edit(Secret)",
        "Edit(Json)",
        "AddItem",
        "Choose",
        "ReadOnly",
    ] {
        assert!(kinds.contains(&expected), "缺 {expected}：{kinds:?}");
    }
}

// ── 零碎行为 ────────────────────────────────────────────────

#[test]
fn add_from_a_deep_field_targets_the_nearest_list() {
    let mut panel = provider_panel();
    goto(&mut panel, "providers[0].name");
    press_code(&mut panel, KeyCode::Char('a'));
    // 最近的列表是 providers → 新增 providers[1]。
    assert!(panel.rows().iter().any(|row| row.path == "providers[1]"));
    assert_eq!(cursor_path(&panel), "providers[1].name");
    // 在 models 的项上按 a → 最近的列表是 models，两个形态 → 先弹形态选择。
    goto(&mut panel, "providers[0].models");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0].models[0]");
    press_code(&mut panel, KeyCode::Char('a'));
    assert_eq!(panel.prompt().unwrap().kind, PromptKind::Variants);
    press_code(&mut panel, KeyCode::Enter); // 简单形态
    assert_eq!(
        panel.edit_state().map(|edit| edit.path().to_string()),
        Some("providers[0].models[1]".into()),
        "选完形态立刻编辑新项"
    );
}

#[test]
fn arrow_keys_on_a_leaf_row_jump_to_the_parent() {
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Left);
    assert_eq!(cursor_path(&panel), "gateway");
    press_code(&mut panel, KeyCode::Right);
    assert_eq!(cursor_path(&panel), "gateway", "→ 在标量行无操作");
}

#[test]
fn footer_hint_reflects_the_active_mode() {
    let mut panel = panel();
    assert!(panel.footer_hint().contains("保存"));
    assert!(panel.footer_hint().contains("Esc 关闭"));
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Char('?'));
    assert!(panel.footer_hint().contains("关闭帮助"));
    press_code(&mut panel, KeyCode::Esc);
    press_code(&mut panel, KeyCode::Char('/'));
    assert!(panel.footer_hint().contains("命中"));
    press_code(&mut panel, KeyCode::Esc);
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.footer_hint(), "Enter 提交 · Esc 取消 · Ctrl+U 清空");
    press_code(&mut panel, KeyCode::Esc);
    press_code(&mut panel, KeyCode::Char('d'));
    assert_eq!(panel.footer_hint(), "Enter 确认 · Esc 取消");
    press_code(&mut panel, KeyCode::Esc);
    press_code(&mut panel, KeyCode::Char('p'));
    assert!(panel.footer_hint().contains("返回树"));
}

#[test]
fn setup_mode_starts_in_the_problems_view_with_the_flag_visible() {
    let mut state = state();
    state.setup_mode = true;
    state.problems = vec![SettingProblem {
        path: Some("providers".into()),
        kind: "empty_list".into(),
        message: "providers 不得为空".into(),
        hint: None,
    }];
    let panel = SettingsPanel::new(&schema(), state, None, View::Problems);
    assert_eq!(panel.view(), View::Problems);
    assert!(panel.setup_mode());
    assert_eq!(panel.config_path(), "/tmp/wing/core/config.yaml");
    assert!(!panel.problems().is_empty());
}

#[test]
fn fingerprint_and_config_path_are_exposed_for_the_title_bar() {
    let panel = panel();
    assert_eq!(panel.fingerprint(), "fp-1");
    assert_eq!(panel.config_path(), "/tmp/wing/core/config.yaml");
}

#[test]
fn cursor_search_helpers_do_not_panic_on_empty_catalogs() {
    let schema = SettingsSchemaResponse {
        version: "0".into(),
        root: fx::root(vec![]),
        config_path: "p".into(),
    };
    let mut panel =
        SettingsPanel::new(&schema, state_with(json!({}), Vec::new()), None, View::Tree);
    assert_eq!(panel.rows().len(), 1, "只有根头行");
    press_code(&mut panel, KeyCode::Down);
    press_code(&mut panel, KeyCode::Home);
    press_code(&mut panel, KeyCode::Char('a'));
    press_code(&mut panel, KeyCode::Char('d'));
    press_code(&mut panel, KeyCode::Char('r'));
    press_code(&mut panel, KeyCode::Char('J'));
    press_code(&mut panel, KeyCode::Char('K'));
    press_code(&mut panel, KeyCode::Char('s'));
    press_code(&mut panel, KeyCode::Tab);
    assert_eq!(panel.dirty_count(), 0);
}

#[test]
fn interface_secret_editing_masks_the_buffer_and_previews() {
    let catalog = fx::root(vec![fx::secret_field("api_key")]);
    let mut panel = SettingsPanel::new(
        &schema(),
        state(),
        Some(InterfaceSource {
            catalog,
            doc: json!({"api_key": "sk-0123456789"}),
        }),
        View::Tree,
    );
    goto_root(&mut panel, super::Root::Interface);
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "api_key");
    assert_eq!(
        row(&panel, "api_key").value,
        ValueText::Masked {
            hint: Some("6789".into())
        },
        "本地文档里的密文也只回显末 4 位"
    );
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        panel.edit_state().unwrap().visible_buffer(),
        "",
        "缓冲恒从空开始"
    );
    type_text(&mut panel, "sk-new-secret");
    assert_eq!(
        panel.edit_state().unwrap().visible_buffer(),
        "•••••••••••••"
    );
    match press_code(&mut panel, KeyCode::Enter) {
        SettingsAction::PreviewInterface(doc) => {
            assert_eq!(
                doc["api_key"],
                json!("sk-new-secret"),
                "预览携带完整新值（App 写盘用）"
            )
        }
        other => panic!("expected preview, got {other:?}"),
    }
    // 空提交保留原值。
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.edit_state().unwrap().visible_buffer(), "");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.edit_state().is_none());
    assert_eq!(
        row(&panel, "api_key").value,
        ValueText::Masked {
            hint: Some("cret".into())
        },
        "空提交 = 取消：保留上一次提交的新值（sk-new-secret），不清空"
    );
}

#[test]
fn search_with_no_hits_keeps_the_tree_empty_and_esc_restores_it() {
    let mut panel = panel();
    press_code(&mut panel, KeyCode::Char('/'));
    type_text(&mut panel, "zzz-no-such-thing");
    assert_eq!(panel.search_hits(), 0);
    assert_eq!(panel.rows().len(), 1, "只剩根头行");
    press_code(&mut panel, KeyCode::Esc);
    assert!(panel.rows().len() > 1, "恢复完整树");
    assert!(panel.rows().iter().any(|row| row.path == "gateway"));
}

#[test]
fn problems_and_add_are_consistent_for_a_zero_item_list() {
    let mut panel = panel_with(json!({"providers": []}));
    goto(&mut panel, "providers");
    assert!(row(&panel, "providers").markers.problem, "空列表自带问题");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[]");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(cursor_path(&panel), "providers[0].name");
    assert!(
        panel
            .rows()
            .iter()
            .any(|row| row.path == "providers[0].name")
    );
    // 列表不再空 → 问题消失。
    assert!(
        panel
            .problems()
            .iter()
            .all(|problem| problem.path.as_deref() != Some("providers"))
    );
}

// ── 返修 r1：B1 密文 null 红线 / S1 问题清单键位 / AD3 编辑器无操作 / N5 setup 标记 ──

/// `get` 快照：`api_key` 为 `null`（密文「保留」态）+ secrets 表说磁盘上它是 set。
fn state_with_secret_set() -> SettingsGetResponse {
    let mut state = state();
    state.secrets.insert(
        "providers[0].api_key".to_string(),
        SecretState {
            state: SecretPresence::Set,
            hint: Some("ab12".into()),
        },
    );
    state
}

#[test]
fn unedited_secret_null_is_echoed_back_in_the_save_document() {
    // design.md §7.5 的契约红线：`null` = 保留磁盘现值，保存时必须**原样回传**。
    // 丢掉这个键 = 清空密钥 = 用户下一次调用 401。
    let mut panel = SettingsPanel::new(&schema(), state_with_secret_set(), None, View::Tree);
    goto(&mut panel, "providers");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "providers[0]");
    press_code(&mut panel, KeyCode::Right);
    assert_eq!(
        row(&panel, "providers[0].api_key").value,
        ValueText::Masked {
            hint: Some("ab12".into())
        },
        "行上按 secrets 表显示掩码"
    );
    edit_port(&mut panel, "8080");
    let action = press_code(&mut panel, KeyCode::Char('s'));
    let SettingsAction::Save { gateway, .. } = action else {
        panic!("expected Save, got {action:?}")
    };
    let provider = gateway["providers"][0]
        .as_object()
        .expect("provider 是对象");
    assert!(
        provider.contains_key("api_key"),
        "契约红线：原样回传 null（丢键 = 清空密钥）"
    );
    assert_eq!(provider["api_key"], Value::Null);
    assert!(
        !panel.is_dirty(super::Root::Gateway, "providers[0].api_key"),
        "没动过的密文不算脏"
    );
    assert_eq!(
        gateway["gateway"]["port"],
        json!(8080),
        "同一次保存带上真正的改动"
    );
}

#[test]
fn an_absent_secret_stays_absent_in_the_save_document() {
    // 文档里根本没有 `api_key` → 保存载荷里也不许出现（不制造显式 null）。
    let gateway_doc = json!({
        "providers": [{"name": "p", "base_url": "u", "models": ["m"]}],
        "gateway": {"port": 1}
    });
    let mut panel = panel_with(gateway_doc);
    edit_port(&mut panel, "8080");
    let action = press_code(&mut panel, KeyCode::Char('s'));
    let SettingsAction::Save { gateway, .. } = action else {
        panic!("expected Save, got {action:?}")
    };
    let provider = gateway["providers"][0]
        .as_object()
        .expect("provider 是对象");
    assert!(
        !provider.contains_key("api_key"),
        "缺席保持缺席：不物化成显式 null（两种语义不同）"
    );
}

#[test]
fn problems_view_left_returns_to_the_tree() {
    let mut panel = SettingsPanel::new(&schema(), state(), None, View::Problems);
    assert_eq!(press_code(&mut panel, KeyCode::Left), SettingsAction::None);
    assert_eq!(panel.view(), View::Tree);
}

#[test]
fn problems_view_right_jumps_to_the_row_like_enter() {
    let backend = vec![SettingProblem {
        path: Some("providers[0].api_key".into()),
        kind: "missing_required".into(),
        message: "必填项未设置".into(),
        hint: None,
    }];
    let mut panel = SettingsPanel::new(
        &schema(),
        state_with(fx::sample_gateway_doc(), backend),
        None,
        View::Problems,
    );
    press_code(&mut panel, KeyCode::Right);
    assert_eq!(panel.view(), View::Tree);
    assert_eq!(cursor_path(&panel), "providers[0].api_key");
}

#[test]
fn problems_view_tab_switches_the_root() {
    let mut panel =
        SettingsPanel::new(&schema(), state(), Some(interface_source()), View::Problems);
    assert_eq!(panel.rows()[panel.cursor()].root, super::Root::Gateway);
    press_code(&mut panel, KeyCode::Tab);
    assert_eq!(panel.rows()[panel.cursor()].root, super::Root::Interface);
    press_code(&mut panel, KeyCode::Tab);
    assert_eq!(panel.rows()[panel.cursor()].root, super::Root::Gateway);
}

#[test]
fn problems_view_r_reloads_and_ctrl_r_restarts_like_the_tree() {
    let mut problems = SettingsPanel::new(&schema(), state(), None, View::Problems);
    assert_eq!(
        press_code(&mut problems, KeyCode::Char('R')),
        SettingsAction::Reload
    );
    assert_eq!(problems.view(), View::Problems, "重载不切视图");
    mark_restart_pending(&mut problems);
    assert_eq!(
        press(&mut problems, ctrl('r')),
        SettingsAction::RestartGateway
    );
    // 脏改动时与树视图同一条路径：先放弃确认。
    let mut dirty = panel();
    edit_port(&mut dirty, "8080");
    press_code(&mut dirty, KeyCode::Char('p'));
    assert_eq!(dirty.view(), View::Problems);
    assert_eq!(
        press_code(&mut dirty, KeyCode::Char('R')),
        SettingsAction::None
    );
    assert!(dirty.prompt().is_some(), "脏改动先确认");
    press_code(&mut dirty, KeyCode::Esc);
    assert!(dirty.prompt().is_none());
    assert_eq!(dirty.view(), View::Problems);
}

#[test]
fn submitting_the_untouched_buffer_is_a_no_op() {
    // AD3：打开 → 不改 → Enter 必须什么都不做（不写入、不标脏）。
    let mut panel = panel();
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(
        panel.edit_state().unwrap().visible_buffer(),
        "32523",
        "预填有效展示值"
    );
    assert_eq!(press_code(&mut panel, KeyCode::Enter), SettingsAction::None);
    assert!(panel.edit_state().is_none(), "编辑器照常关闭");
    assert_eq!(panel.dirty_count(), 0, "没改就不脏");
}

#[test]
fn submitting_the_default_prefill_does_not_pin_it() {
    // 缺席 + 有默认 → 预填默认值；直接 Enter 不得把它物化成显式覆盖。
    let mut panel = panel_with(json!({}));
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.edit_state().unwrap().visible_buffer(), "0");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.dirty_count(), 0);
    assert_eq!(
        row(&panel, "gateway.port").value,
        ValueText::Default("0".into()),
        "仍是缺席态（跟随默认），不是显式 0"
    );
}

#[test]
fn editing_the_buffer_really_does_write() {
    // AD3 的反面：真的改了就必须写（免得把 no-op 误扩成「永不写入」）。
    let mut panel = panel_with(json!({}));
    goto(&mut panel, "gateway");
    press_code(&mut panel, KeyCode::Right);
    goto(&mut panel, "gateway.port");
    press_code(&mut panel, KeyCode::Enter);
    type_text(&mut panel, "7");
    press_code(&mut panel, KeyCode::Enter);
    assert!(panel.is_dirty(super::Root::Gateway, "gateway.port"));
    assert_eq!(
        row(&panel, "gateway.port").value,
        ValueText::Text("7".into()),
        "缓冲 07 → int 7 → 写回并展示为 7"
    );
}

#[test]
fn unchanged_nullable_field_does_not_write_an_explicit_null() {
    let mut field = fx::str_field("theme");
    field.nullable = true;
    let schema = SettingsSchemaResponse {
        version: "0".into(),
        root: fx::root(vec![field]),
        config_path: "p".into(),
    };
    let mut panel =
        SettingsPanel::new(&schema, state_with(json!({}), Vec::new()), None, View::Tree);
    goto(&mut panel, "theme");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.edit_state().unwrap().visible_buffer(), "");
    press_code(&mut panel, KeyCode::Enter);
    assert_eq!(panel.dirty_count(), 0, "空提交 = 打开时的值 → 无操作");
    assert_eq!(row(&panel, "theme").value, ValueText::None, "字段仍然缺席");
}

#[test]
fn a_successful_save_that_exits_setup_mode_clears_the_flag() {
    let mut setup = state();
    setup.setup_mode = true;
    let mut panel = SettingsPanel::new(&schema(), setup, None, View::Tree);
    assert!(panel.setup_mode());
    let mut response = ok_response(Vec::new());
    response.setup_mode_exited = true;
    panel.apply_save(super::SaveOutcome {
        gateway: Some(response),
        interface_ok: None,
    });
    assert!(!panel.setup_mode(), "保存让网关转入正常模式，标记跟着熄掉");
    // 不含 exited 的回执不动这个标记。
    let mut setup = state();
    setup.setup_mode = true;
    let mut panel = SettingsPanel::new(&schema(), setup, None, View::Tree);
    panel.apply_save(super::SaveOutcome {
        gateway: Some(ok_response(Vec::new())),
        interface_ok: None,
    });
    assert!(panel.setup_mode());
}
