use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use super::test_support as fx;
use crate::shared::panels::settings::{InterfaceSource, SettingsPanel, View};

fn frame(panel: &mut SettingsPanel, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| super::draw(frame, panel, frame.area(), &fx::palette()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn lines(buf: &Buffer) -> Vec<String> {
    (buf.area.y..buf.area.bottom())
        .map(|y| {
            (buf.area.x..buf.area.right())
                .map(|x| buf[(x, y)].symbol())
                .collect()
        })
        .collect()
}

fn cursor(buf: &Buffer) -> (u16, u16) {
    (buf.area.y..buf.area.bottom())
        .find_map(|y| {
            (buf.area.x..buf.area.right())
                .find(|&x| buf[(x, y)].modifier.contains(Modifier::REVERSED))
                .map(|x| (x, y))
        })
        .expect("visible input cursor")
}

#[test]
fn categories_and_settings_share_one_column_at_both_widths() {
    for width in [32, 120] {
        let mut panel = fx::panel();
        let categories = lines(&frame(&mut panel, width, 15)).join("\n");
        assert!(categories.contains("Settings"), "{categories}");
        assert!(categories.contains("❯ Providers"), "{categories}");
        assert!(!categories.contains("default · openai"), "{categories}");
        fx::press(&mut panel, KeyCode::Enter);
        assert_eq!(panel.view(), View::Tree);
        let selected = lines(&frame(&mut panel, width, 15)).join("\n");
        assert!(selected.contains("Providers"), "{selected}");
        assert!(selected.contains("default"), "{selected}");
        assert!(!selected.contains('│'), "{selected}");
        fx::press(&mut panel, KeyCode::Esc);
        assert_eq!(panel.view(), View::Categories);
    }
}

#[test]
fn the_editor_uses_the_full_line_and_follows_a_cjk_cursor() {
    for width in [32, 120] {
        let mut panel = fx::panel();
        fx::open_editor(&mut panel, "base_url");
        fx::press_with(&mut panel, KeyCode::Char('u'), KeyModifiers::CONTROL);
        fx::type_text(&mut panel, &format!("首端{}尾端", "中".repeat(100)));
        let end = frame(&mut panel, width, 10);
        let (x, y) = cursor(&end);
        let rendered = lines(&end);
        assert!(rendered[y as usize].contains("尾 端"), "{rendered:?}");
        assert!(x >= width - 2 && x < width);
        fx::press(&mut panel, KeyCode::Home);
        let start = frame(&mut panel, width, 10);
        let (x, y) = cursor(&start);
        assert_eq!(x, 0);
        assert_eq!(start[(x, y)].symbol(), "首");
        fx::press(&mut panel, KeyCode::Right);
        assert_eq!(cursor(&frame(&mut panel, width, 10)).0, 2);
    }
}

#[test]
fn the_editor_retains_its_value_and_visible_error_on_a_short_screen() {
    let mut panel = fx::panel();
    fx::open_editor(&mut panel, "gateway.port");
    fx::press_with(&mut panel, KeyCode::Char('u'), KeyModifiers::CONTROL);
    fx::type_text(&mut panel, "abc");
    fx::press(&mut panel, KeyCode::Enter);
    let buf = frame(&mut panel, 32, 3);
    cursor(&buf);
    let text = lines(&buf).join("\n");
    assert!(text.contains("abc"), "{text}");
    assert!(text.contains("需 要 一 个 整 数"), "{text}");
}

#[test]
fn the_secret_row_and_editor_never_render_plaintext() {
    let mut panel = fx::panel();
    fx::goto(&mut panel, "api_key");
    let before = lines(&frame(&mut panel, 60, 15)).join("\n");
    assert!(before.contains("••••••••"), "{before}");
    fx::press(&mut panel, KeyCode::Enter);
    let secret = "private-test-marker-".repeat(5);
    fx::type_text(&mut panel, &secret);
    let buf = frame(&mut panel, 32, 6);
    let shown = lines(&buf).join("\n");
    assert!(shown.contains("••••••••••••"), "{shown}");
    assert!(!shown.contains("private"), "{shown}");
    cursor(&buf);
}

#[test]
fn preview_swatch_and_selected_description_use_the_live_catalog() {
    let catalog = fx::sample_catalog();
    let mut panel = SettingsPanel::new(
        &fx::schema(&catalog),
        fx::state(fx::sample_values()),
        Some(InterfaceSource {
            catalog: crate::config::catalog::interface_catalog(),
            groups: crate::config::catalog::interface_groups(),
            doc: serde_json::json!({"colors": {"accent": "cyan"}}),
        }),
        View::Categories,
    );
    fx::goto(&mut panel, "colors.accent");
    let buf = frame(&mut panel, 120, 15);
    let rows = lines(&buf);
    let selected = rows
        .iter()
        .position(|line| line.contains("accent") && line.contains("cyan"))
        .unwrap();
    assert!(!rows[selected + 1].trim().is_empty());
    assert!(
        buf.content
            .iter()
            .any(|cell| cell.symbol() == "■" && cell.fg == Color::Cyan)
    );
}

#[test]
fn search_echo_wins_over_the_disk_change_notice() {
    let mut panel = fx::panel();
    panel.on_settings_changed("external-fingerprint");
    assert!(
        lines(&frame(&mut panel, 52, 8))
            .join("\n")
            .contains("磁 盘")
    );
    fx::press(&mut panel, KeyCode::Char('/'));
    fx::type_text(&mut panel, &format!("{}last", "long".repeat(40)));
    let buf = frame(&mut panel, 32, 2);
    let text = lines(&buf).join("\n");
    assert!(text.contains("last"), "{text}");
    assert!(!text.contains("磁 盘"), "{text}");
    cursor(&buf);
}

#[test]
fn setup_problem_rows_and_confirmation_remain_visible_on_short_frames() {
    let (_, mut setup) = fx::setup_panel();
    let text = lines(&frame(&mut setup, 32, 9)).join("\n");
    assert!(text.contains("待 修 复"), "{text}");
    assert!(text.contains("providers"), "{text}");
    let mut panel = fx::panel();
    fx::goto(&mut panel, "tools");
    fx::press(&mut panel, KeyCode::Right);
    fx::press(&mut panel, KeyCode::Down);
    fx::press(&mut panel, KeyCode::Char('d'));
    let text = lines(&frame(&mut panel, 32, 1)).join("\n");
    assert!(text.contains("Bash"), "{text}");
}

#[test]
fn a_long_list_keeps_its_selected_row_inside_the_viewport() {
    let (_, mut panel) = fx::big_list(35);
    fx::press(&mut panel, KeyCode::Enter);
    fx::press(&mut panel, KeyCode::End);
    fx::press(&mut panel, KeyCode::Up);
    let text = lines(&frame(&mut panel, 32, 8)).join("\n");
    assert!(
        text.lines()
            .any(|line| line.contains('❯') && line.contains("item34")),
        "{text}"
    );
    assert!(!text.contains("item00"));
}

#[test]
fn the_painter_only_touches_its_assigned_rectangle() {
    for (width, height) in [(0, 0), (1, 1), (32, 4), (120, 18)] {
        let mut panel = fx::panel();
        let rect = Rect::new(2, 2, width, height);
        let mut terminal = Terminal::new(TestBackend::new(width + 4, height + 4)).unwrap();
        terminal
            .draw(|frame| {
                for cell in frame.buffer_mut().content.iter_mut() {
                    cell.set_symbol(".");
                }
                super::draw(frame, &mut panel, rect, &fx::palette());
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if !rect.contains((x, y).into()) {
                    assert_eq!(buf[(x, y)].symbol(), ".", "at ({x}, {y})");
                }
            }
        }
    }
}

#[test]
fn both_model_shapes_keep_the_full_vision_label_and_state_at_32_columns() {
    use crate::shared::panels::settings::test_support as model_fixture;

    for width in [32, 120] {
        let catalog = model_fixture::mixed_model_catalog();
        let mut panel = SettingsPanel::new(
            &fx::schema(&catalog),
            fx::state(model_fixture::mixed_model_values()),
            None,
            View::Categories,
        );
        for (index, before, after) in [(0, "disabled", "enabled"), (1, "enabled", "disabled")] {
            let path = format!("providers[0].models[{index}].capabilities.vision");
            fx::press(&mut panel, KeyCode::Char('/'));
            fx::type_text(&mut panel, "Image input");
            let cursor = panel.cursor();
            let target = panel
                .rows()
                .iter()
                .position(|row| row.path == path)
                .unwrap();
            for _ in cursor..target {
                fx::press(&mut panel, KeyCode::Down);
            }
            assert_eq!(panel.current_row().unwrap().path, path);
            fx::press(&mut panel, KeyCode::Enter);
            for state in [before, after] {
                let text = lines(&frame(&mut panel, width, 4)).join("\n");
                assert!(
                    text.lines()
                        .any(|line| line.contains('❯')
                            && line.contains(&format!("Image input {state}"))),
                    "{width} columns, {path}:\n{text}"
                );
                if state == before {
                    fx::press(&mut panel, KeyCode::Tab);
                }
            }
        }
    }
}

#[test]
fn compact_variant_prompt_keeps_the_selected_option_and_cancel_preserves_the_list() {
    let mut panel = fx::panel();
    fx::goto(&mut panel, "providers[].models");
    fx::press(&mut panel, KeyCode::Char('a'));
    let prompt = panel.prompt().expect("generic union offers its variants");
    assert_eq!(prompt.options.len(), 2);
    let variant = |index: usize| prompt.options[index].split_whitespace().next().unwrap();

    fx::press(&mut panel, KeyCode::Down);
    let compact = lines(&frame(&mut panel, 32, 1));
    assert!(
        compact[0].starts_with(&format!("❯ {}", variant(1))),
        "{compact:?}"
    );
    let expanded = lines(&frame(&mut panel, 48, 6));
    assert!(expanded[0].starts_with("新 增"), "{expanded:?}");
    assert!(
        expanded
            .iter()
            .any(|line| line.starts_with(&format!("  {}", variant(0))))
    );
    assert!(
        expanded
            .iter()
            .any(|line| line.starts_with(&format!("❯ {}", variant(1))))
    );

    fx::press(&mut panel, KeyCode::Esc);
    assert!(panel.prompt().is_none());
    assert_eq!(panel.dirty_count(), 0);
    assert!(panel.current_row().unwrap().label.ends_with("(1)"));
}

#[test]
fn clear_confirmation_shows_its_consequence_without_changing_the_value_on_cancel() {
    let mut panel = fx::panel();
    fx::goto(&mut panel, "gateway.port");
    let original = panel.current_row().unwrap().value.clone();
    fx::press(&mut panel, KeyCode::Char('d'));
    let prompt = lines(&frame(&mut panel, 32, 6));
    assert!(prompt[0].contains("port"), "{prompt:?}");
    assert!(
        prompt[1..].iter().any(|line| line.contains("config.yaml")),
        "{prompt:?}"
    );
    fx::press(&mut panel, KeyCode::Esc);
    assert_eq!(panel.current_row().unwrap().value, original);
    assert_eq!(panel.dirty_count(), 0);
}

#[test]
fn a_category_and_its_long_label_keep_dirty_problem_and_value_markers() {
    let mut field = fx::int_field("timeout", Some(1.0), None);
    field.title = "Maximum buffering time for provider messages".into();
    let catalog = fx::root(vec![fx::object("gateway", vec![field])]);
    let mut get = fx::state(serde_json::json!({"gateway": {"timeout": 10}}));
    get.problems.push(wing_api_client::models::SettingProblem {
        path: Some("gateway.timeout".into()),
        kind: "invalid_value".into(),
        message: "Server rejected the timeout".into(),
        hint: None,
    });
    let mut panel = SettingsPanel::new(&fx::schema(&catalog), get, None, View::Categories);
    fx::open_editor(&mut panel, "timeout");
    fx::press_with(&mut panel, KeyCode::Char('u'), KeyModifiers::CONTROL);
    fx::type_text(&mut panel, "735");
    fx::press(&mut panel, KeyCode::Enter);
    let items = frame(&mut panel, 32, 7);
    let text = lines(&items).join("\n");
    assert!(
        text.lines()
            .any(|line| line.contains("Maximum") && line.contains("… 735 !")),
        "{text}"
    );
    assert!(
        items
            .content
            .iter()
            .any(|cell| cell.symbol() == "!" && cell.fg == fx::palette().danger)
    );

    fx::press(&mut panel, KeyCode::Esc);
    let groups = lines(&frame(&mut panel, 32, 7)).join("\n");
    assert!(groups.contains("❯ ● Net · 1 问 题"), "{groups}");
}

#[test]
fn an_empty_catalog_renders_both_views_and_handles_keys_without_a_row() {
    let schema = fx::schema(&fx::root(vec![]));
    let get = fx::state(serde_json::json!({}));
    let mut panel = SettingsPanel::new(&schema, get, None, View::Tree);
    let items = lines(&frame(&mut panel, 32, 4)).join("\n");
    assert!(items.contains("没 有 设 置 项"), "{items}");
    fx::press(&mut panel, KeyCode::Tab);
    assert_eq!(panel.view(), View::Tree);
    assert_eq!(panel.dirty_count(), 0);
    fx::press(&mut panel, KeyCode::Left);
    let categories = lines(&frame(&mut panel, 32, 4)).join("\n");
    assert_eq!(panel.view(), View::Categories);
    assert!(categories.contains("没 有 设 置 分 类"), "{categories}");
}

#[test]
fn a_one_row_frame_prioritizes_search_and_a_short_heading_still_flags_stale_config() {
    let mut panel = fx::panel();
    panel.on_settings_changed("another-editor");
    let warning = lines(&frame(&mut panel, 60, 3));
    assert!(warning[0].contains("R 读 取 磁 盘"), "{warning:?}");
    fx::press(&mut panel, KeyCode::Char('/'));
    fx::type_text(&mut panel, "gateway.port");
    let buf = frame(&mut panel, 32, 1);
    let query = lines(&buf);
    assert!(query[0].starts_with("/ gateway.port"), "{query:?}");
    assert_eq!(cursor(&buf), (14, 0));
    fx::press(&mut panel, KeyCode::Esc);
    assert_eq!(panel.view(), View::Categories);
    assert!(panel.is_stale());
}

#[test]
fn editor_displays_stored_controls_safely_and_keeps_a_cursor_at_one_column() {
    let catalog = fx::root(vec![fx::str_field("greeting")]);
    let original = "界\r\nnext\tend\u{1b}";
    let mut panel = SettingsPanel::new(
        &fx::schema(&catalog),
        fx::state(serde_json::json!({"greeting": original})),
        None,
        View::Categories,
    );
    fx::open_editor(&mut panel, "greeting");
    let buf = frame(&mut panel, 32, 5);
    let (_, y) = cursor(&buf);
    let value = &lines(&buf)[y as usize];
    assert!(value.contains("↵↵next⇥end�"), "{value}");
    assert!(!value.chars().any(char::is_control));
    fx::press(&mut panel, KeyCode::Home);
    let narrow = frame(&mut panel, 1, 1);
    assert_eq!(cursor(&narrow), (0, 0));
    assert_eq!(narrow[(0, 0)].symbol(), " ");
    assert_eq!(
        super::editor::search_line(original, 0, &fx::palette()).width(),
        0
    );
    assert_eq!(panel.edit_state().unwrap().visible_buffer(), original);
}

#[test]
fn the_enum_owner_scrolls_to_its_highlight_and_direct_choice_tab_does_not_commit() {
    let catalog = fx::root(vec![fx::enum_field(
        "protocol",
        &[("first", "legacy"), ("second", "current")],
    )]);
    let mut panel = SettingsPanel::new(
        &fx::schema(&catalog),
        fx::state(serde_json::json!({"protocol": "first"})),
        None,
        View::Categories,
    );
    fx::goto(&mut panel, "protocol");
    fx::press(&mut panel, KeyCode::Enter);
    fx::press(&mut panel, KeyCode::Down);
    let selected = lines(&frame(&mut panel, 32, 1));
    assert!(selected[0].contains("❯ second"), "{selected:?}");
    assert_eq!(panel.current_row().unwrap().path, "protocol");
    fx::press(&mut panel, KeyCode::End);
    assert!(panel.footer_hint().contains("Enter 选中"));
    fx::press(&mut panel, KeyCode::Tab);
    assert_eq!(panel.dirty_count(), 0);
    fx::press(&mut panel, KeyCode::Enter);
    assert_eq!(
        panel.current_row().unwrap().value,
        crate::shared::panels::settings::ValueText::Text("second".into())
    );
}

#[test]
fn narrow_help_wraps_descriptions_and_scrolling_can_return_to_the_first_line() {
    let mut panel = fx::panel();
    fx::press(&mut panel, KeyCode::Char('?'));
    let original = lines(&frame(&mut panel, 12, 8));
    fx::press(&mut panel, KeyCode::Down);
    let scrolled = lines(&frame(&mut panel, 12, 8));
    assert_ne!(original, scrolled);
    fx::press(&mut panel, KeyCode::Up);
    assert_eq!(lines(&frame(&mut panel, 12, 8)), original);
    let help = super::help::help_lines(&fx::palette(), 12, usize::MAX);
    let enter = help
        .iter()
        .position(|line| line.to_string() == "Enter")
        .unwrap();
    assert!(help[enter + 1].to_string().starts_with("进入分类"));
    fx::press(&mut panel, KeyCode::Esc);
    assert_eq!(panel.view(), View::Categories);
}
