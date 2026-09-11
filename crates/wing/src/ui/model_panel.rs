//! ModelPanel widget — provider tab bar + model rows + hint footer.
//!
//! Renders above the input area while `/model` is open. Pure projection of
//! [`ModelPanel`] state through the shared panel renderer (`ui/panel.rs`):
//! windowed tabs and rows, `‹`/`›` markers on hidden sides, `●` on the
//! session's currently active model.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;

use crate::app::model_panel::ModelPanel;
use crate::app::selection_panel::SelectionPanel;
use crate::config::ThemePalette;
use crate::ui::panel::Tab;
use crate::ui::panel::TabState;
use crate::ui::panel::Window;
use crate::ui::panel::cursor_span;
use crate::ui::panel::footer;
use crate::ui::panel::label_span;
use crate::ui::panel::option_row;
use crate::ui::panel::tab_bar;

/// Key hints shown on the last line.
const HINT: &str = "↑↓ select · Enter apply · ←→ provider · Esc close";

/// Build the panel's lines: tab bar / visible model rows / hint footer.
pub fn panel_lines(panel: &ModelPanel, palette: &ThemePalette) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Provider tab bar (windowed; custom pages not used here).
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
    lines.push(tab_bar("⇅ ", &tabs, current_page, palette));

    // Model rows: the cursor row is always inside the window.
    let models = panel.models();
    let cursor = panel.cursor();
    let window = Window::new(cursor, models.len());
    for i in window.range.clone() {
        let is_cursor = cursor == i;
        let mut content = vec![label_span(&models[i], is_cursor, palette)];
        // ● — the model the session currently uses (kernel committed row).
        if panel.committed_at(current_page) == Some(i) {
            content.push(Span::styled(" ●", Style::default().fg(palette.success)));
        }
        lines.push(option_row(&window, i, is_cursor, palette, content));
    }
    if models.is_empty() {
        // Empty provider page: nothing to apply, the user can switch away.
        lines.push(Line::from(vec![
            cursor_span(false, palette),
            Span::styled("(no models)", dim),
        ]));
    }

    lines.push(footer(HINT, palette));
    lines
}

/// Widget wrapper for the fixed panel chunk above the input area.
pub struct ModelPanelWidget<'a> {
    panel: &'a ModelPanel,
    palette: &'a ThemePalette,
}

impl<'a> ModelPanelWidget<'a> {
    pub fn new(panel: &'a ModelPanel, palette: &'a ThemePalette) -> Self {
        Self { panel, palette }
    }
}

impl Widget for ModelPanelWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        for (i, line) in panel_lines(self.panel, self.palette).iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.bottom() {
                break;
            }
            buf.set_line(area.x, y, line, area.width);
        }
    }
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

    #[test]
    fn renders_tabs_rows_and_marker() {
        let panel = ModelPanel::new(
            vec![
                group("anthropic", &["a1", "a2"]),
                group("qoder", &["dfmodel"]),
            ],
            Some(("qoder", "dfmodel")),
        );
        let out = text(&panel_lines(&panel, &palette()));
        assert!(out.contains("⇅ anthropic > qoder"), "{out}");
        assert!(out.contains("❯ dfmodel ●"), "current model marked: {out}");
        assert!(out.contains("Esc close"), "{out}");
    }

    #[test]
    fn long_lists_render_window_markers() {
        let models: Vec<String> = (0..8).map(|i| format!("m{i}")).collect();
        let mut panel = ModelPanel::new(
            vec![ProviderModels {
                provider: "p".into(),
                models,
            }],
            None,
        );
        for _ in 0..5 {
            panel.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let out = text(&panel_lines(&panel, &palette()));
        assert!(out.contains('‹') && out.contains('›'), "{out}");
        assert!(!out.contains("m0"), "above the window: {out}");
        assert!(out.contains("❯ m5"), "{out}");
    }

    #[test]
    fn empty_page_renders_empty_state() {
        let panel = ModelPanel::new(vec![group("p", &[])], None);
        let out = text(&panel_lines(&panel, &palette()));
        assert!(out.contains("(no models)"), "{out}");
    }

    #[test]
    fn many_providers_window_the_tab_bar() {
        let sources: Vec<ProviderModels> =
            (0..7).map(|i| group(&format!("p{i}"), &["m"])).collect();
        let mut panel = ModelPanel::new(sources, None);
        for _ in 0..5 {
            panel.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        let out = text(&panel_lines(&panel, &palette()));
        assert!(out.contains("‹ p1"), "left marker on hidden tabs: {out}");
        assert!(out.contains("p5 ›"), "right marker on hidden tabs: {out}");
        assert!(!out.contains("p0"), "first provider is scrolled out: {out}");
    }
}
