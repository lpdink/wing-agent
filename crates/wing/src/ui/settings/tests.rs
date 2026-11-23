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
