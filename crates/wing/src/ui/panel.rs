//! Shared rendering for interactive selection panels (Ask, model picker).
//!
//! The window math lives in the kernel ([`crate::app::selection_panel`]):
//! both panels render at most [`PANEL_WINDOW`] tabs / option rows, the
//! cursor's slot stays centered while the window scrolls, the window is
//! pinned at the ends, and there are deliberately no overflow indicator
//! glyphs — the rows stay column-aligned instead.
//!
//! Adapter-specific row decorations (Ask's ordinals/checkboxes/editor, the
//! model picker's `●` mark) stay with the adapters; the shared helpers here
//! cover the cursor marker, label styling and footer hint.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::app::selection_panel::PANEL_WINDOW;
use crate::app::selection_panel::window_range;
use crate::config::ThemePalette;

/// Visual state of one tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabState {
    /// The active tab (accent + bold).
    Active,
    /// Adapter-defined secondary state, e.g. Ask's "answered" (success).
    Marked,
    /// Default (dim).
    Normal,
}

/// One tab bar entry.
pub struct Tab<'a> {
    pub label: &'a str,
    pub state: TabState,
}

/// Render a windowed tab bar: `prefix` (omitted when empty) + the visible
/// slice of tabs. `current` is the active tab index — it drives the centered
/// window but can also be passed explicitly by adapters (Ask marks the
/// confirm page as active while the cursor index stays on the last question).
pub fn tab_bar(
    prefix: &str,
    tabs: &[Tab<'_>],
    current: usize,
    palette: &ThemePalette,
) -> Line<'static> {
    let range = window_range(current, tabs.len(), PANEL_WINDOW);
    let dim = Style::default().fg(palette.dim);

    let mut spans: Vec<Span<'static>> = Vec::new();
    if !prefix.is_empty() {
        spans.push(Span::styled(
            prefix.to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ));
    }
    for (i, tab) in tabs.iter().enumerate().take(range.end).skip(range.start) {
        if i > range.start {
            spans.push(Span::styled(" > ", dim));
        }
        spans.push(Span::styled(
            tab.label.to_string(),
            tab_style(tab.state, palette),
        ));
    }
    Line::from(spans)
}

/// Style of a tab label for its state.
fn tab_style(state: TabState, palette: &ThemePalette) -> Style {
    match state {
        TabState::Active => Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
        TabState::Marked => Style::default().fg(palette.success),
        TabState::Normal => Style::default().fg(palette.dim),
    }
}

/// Cursor marker of an option row: `❯ ` (accent + bold) or blank padding.
pub fn cursor_span(is_cursor: bool, palette: &ThemePalette) -> Span<'static> {
    if is_cursor {
        Span::styled(
            "❯ ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    }
}

/// Label style of an option row: accent + bold under the cursor, plain text
/// otherwise.
pub fn label_style(is_cursor: bool, palette: &ThemePalette) -> Style {
    if is_cursor {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text)
    }
}

/// Label span of an option row in the shared cursor style.
pub fn label_span(label: &str, is_cursor: bool, palette: &ThemePalette) -> Span<'static> {
    Span::styled(label.to_string(), label_style(is_cursor, palette))
}

/// Dim footer hint line.
pub fn footer(hint: &str, palette: &ThemePalette) -> Line<'static> {
    Line::from(Span::styled(
        hint.to_string(),
        Style::default().fg(palette.dim),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    fn tabs<'a>(labels: &[&'a str]) -> Vec<Tab<'a>> {
        labels
            .iter()
            .map(|l| Tab {
                label: l,
                state: TabState::Normal,
            })
            .collect()
    }

    fn text(line: &Line<'static>) -> String {
        line.to_string()
    }

    #[test]
    fn tab_bar_renders_all_tabs_when_short() {
        let tabs = vec![
            Tab {
                label: "配色",
                state: TabState::Active,
            },
            Tab {
                label: "测试项",
                state: TabState::Marked,
            },
            Tab {
                label: "Submit",
                state: TabState::Normal,
            },
        ];
        let out = text(&tab_bar("? ", &tabs, 0, &palette()));
        assert_eq!(out, "? 配色 > 测试项 > Submit");
    }

    #[test]
    fn tab_bar_scrolls_a_centered_window_without_markers() {
        let labels: Vec<String> = (0..7).map(|i| format!("Q{i}")).collect();
        let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        let tabs = tabs(&refs);
        // Cursor near the top: window pinned at the start.
        assert_eq!(
            text(&tab_bar("? ", &tabs, 0, &palette())),
            "? Q0 > Q1 > Q2 > Q3 > Q4"
        );
        // Middle: the active tab sits in the center slot.
        assert_eq!(
            text(&tab_bar("? ", &tabs, 3, &palette())),
            "? Q1 > Q2 > Q3 > Q4 > Q5"
        );
        // End: window pinned at the end.
        assert_eq!(
            text(&tab_bar("? ", &tabs, 6, &palette())),
            "? Q2 > Q3 > Q4 > Q5 > Q6"
        );
        // No overflow glyphs anywhere.
        for current in 0..7 {
            let out = text(&tab_bar("? ", &tabs, current, &palette()));
            assert!(!out.contains('‹') && !out.contains('›'), "{out}");
        }
    }

    #[test]
    fn tab_bar_prefix_is_optional() {
        let tabs = tabs(&["alpha", "beta"]);
        assert_eq!(text(&tab_bar("", &tabs, 0, &palette())), "alpha > beta");
    }

    #[test]
    fn option_row_helpers_are_aligned() {
        // Cursor marker + label: the cursor row and a plain row occupy the
        // same columns (no marker glyphs shifting the text).
        let cursor = Line::from(vec![
            cursor_span(true, &palette()),
            label_span("model-a", true, &palette()),
        ]);
        let plain = Line::from(vec![
            cursor_span(false, &palette()),
            label_span("model-b", false, &palette()),
        ]);
        assert_eq!(text(&cursor), "❯ model-a");
        assert_eq!(text(&plain), "  model-b");
    }
}
