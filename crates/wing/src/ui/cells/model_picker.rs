//! Model picker cell — the `/model` panel rendered inside the chat view.
//!
//! The picker lives in the transcript like the Ask panel, but leaves no
//! record: the cell is removed the moment the user applies a model or closes
//! the panel. Provider tabs and model rows are windowed by the shared kernel
//! (at most 5 visible, cursor / active tab kept centered while scrolling,
//! pinned at the ends, no overflow indicator glyphs). `●` marks the model
//! the session currently uses.

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::app::model_panel::ModelPanel;
use crate::app::selection_panel::PANEL_WINDOW;
use crate::app::selection_panel::SelectionPanel;
use crate::app::selection_panel::window_range;
use crate::config::ThemePalette;
use crate::ui::panel::Tab;
use crate::ui::panel::TabState;
use crate::ui::panel::cursor_span;
use crate::ui::panel::footer;
use crate::ui::panel::label_span;
use crate::ui::panel::tab_bar;

/// Key hints shown on the last line.
const HINT: &str = "↑↓ select · Enter apply · ←→ provider · Esc close";

/// Render the picker: provider tab bar / visible model rows / hint footer.
pub fn model_picker_lines(panel: &ModelPanel, palette: &ThemePalette) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Provider tabs — the active one is centered while the strip scrolls.
    let current_page = panel.current_page();
    let tabs: Vec<Tab> = panel
        .sources()
        .iter()
        .enumerate()
        .map(|(i, group)| Tab {
            label: group.provider.as_str(),
            state: if i == current_page {
                TabState::Active
            } else {
                TabState::Normal
            },
        })
        .collect();
    lines.push(tab_bar("", &tabs, current_page, palette));

    // Model rows — the cursor row stays centered while the list scrolls; the
    // session's current model carries the `●` mark.
    let models = panel.models();
    let cursor = panel.cursor();
    let range = window_range(cursor, models.len(), PANEL_WINDOW);
    for i in range {
        let is_cursor = cursor == i;
        let mut spans = vec![
            cursor_span(is_cursor, palette),
            label_span(&models[i], is_cursor, palette),
        ];
        if panel.committed_at(current_page) == Some(i) {
            spans.push(Span::styled(" ●", Style::default().fg(palette.success)));
        }
        lines.push(Line::from(spans));
    }
    if models.is_empty() {
        // Empty provider page: nothing to apply, the user can switch away.
        lines.push(Line::from(vec![
            cursor_span(false, palette),
            Span::styled("(no models)", dim),
        ]));
    }

    lines.push(footer(HINT, palette));
    lines.push(Line::from(""));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::model_panel::ModelPanel;
    use wing_api_client::models::ProviderModels;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    fn group(provider: &str, models: &[&str]) -> ProviderModels {
        ProviderModels {
            provider: provider.into(),
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn down(panel: &mut ModelPanel) {
        panel.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        ));
    }

    #[test]
    fn renders_tabs_rows_and_marker() {
        let panel = ModelPanel::new(
            vec![
                group("anthropic", &["a1", "a2"]),
                group("qoder", &["dfmodel"]),
            ],
            Some(("qoder", "dfmodel")),
        );
        let out = text(&model_picker_lines(&panel, &palette()));
        assert!(out.contains("anthropic > qoder"), "{out}");
        assert!(out.contains("❯ dfmodel ●"), "current model marked: {out}");
        assert!(out.contains("Esc close"), "{out}");
        assert!(
            !out.contains('‹') && !out.contains('›'),
            "no markers: {out}"
        );
    }

    #[test]
    fn long_lists_scroll_centered_without_markers() {
        let models: Vec<String> = (0..8).map(|i| format!("m{i}")).collect();
        let mut panel = ModelPanel::new(
            vec![ProviderModels {
                provider: "p".into(),
                models,
            }],
            None,
        );
        for _ in 0..5 {
            down(&mut panel); // cursor → m5
        }
        let out = text(&model_picker_lines(&panel, &palette()));
        for hidden in ["m0", "m1", "m2"] {
            assert!(!out.contains(hidden), "{hidden} above the window: {out}");
        }
        assert!(out.contains("❯ m5"), "cursor centered: {out}");
        assert!(out.contains("m7"), "last row visible: {out}");
        assert!(
            !out.contains('‹') && !out.contains('›'),
            "no markers: {out}"
        );
    }

    #[test]
    fn rows_are_column_aligned() {
        let panel = ModelPanel::new(
            vec![group("p", &["model-a", "model-b"])],
            Some(("p", "model-b")),
        );
        let out = text(&model_picker_lines(&panel, &palette()));
        // Cursor and non-cursor rows start at the same column; the `●` mark
        // is a suffix so it never shifts the labels.
        assert!(out.contains("  model-a"), "{out}");
        assert!(out.contains("❯ model-b ●"), "{out}");
        let lines: Vec<&str> = out.lines().collect();
        let a = lines.iter().find(|l| l.contains("model-a")).unwrap();
        let b = lines.iter().find(|l| l.contains("model-b")).unwrap();
        let col = |line: &str, needle: &str| line[..line.find(needle).unwrap()].chars().count();
        assert_eq!(
            col(a, "model-a"),
            col(b, "model-b"),
            "labels aligned: {out}"
        );
    }

    #[test]
    fn empty_page_renders_empty_state() {
        let panel = ModelPanel::new(vec![group("p", &[])], None);
        let out = text(&model_picker_lines(&panel, &palette()));
        assert!(out.contains("(no models)"), "{out}");
    }

    #[test]
    fn many_providers_center_the_active_tab() {
        let sources: Vec<ProviderModels> =
            (0..7).map(|i| group(&format!("p{i}"), &["m"])).collect();
        let mut panel = ModelPanel::new(sources, None);
        for _ in 0..5 {
            panel.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let out = text(&model_picker_lines(&panel, &palette()));
        assert!(
            out.contains("p2 > p3 > p4 > p5 > p6"),
            "active tab centered: {out}"
        );
        assert!(!out.contains("p0") && !out.contains("p1"), "{out}");
    }
}
