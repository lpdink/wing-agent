//! Shared rendering for interactive selection panels (Ask, model picker).
//!
//! The window math lives in the kernel ([`crate::app::selection_panel`]); this
//! module only consumes it: tab bars with `‹`/`›` markers on the hidden
//! sides, windowed option rows, and the dim footer hint. Adapter-specific row
//! decorations (Ask checkboxes/ordinals/editor, the model `●` mark) stay with
//! the adapters — they are passed in as content spans.

use std::ops::Range;

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::app::selection_panel::PANEL_WINDOW;
use crate::app::selection_panel::window_range;
use crate::config::ThemePalette;

/// A window over `len` items that always contains `cursor`, plus the
/// hidden-side flags the renderer turns into `‹` / `›` markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Visible slice of the item list.
    pub range: Range<usize>,
    /// Items before the window are hidden (render `‹`).
    pub left_hidden: bool,
    /// Items after the window are hidden (render `›`).
    pub right_hidden: bool,
}

impl Window {
    /// Window of the default size following `cursor`.
    pub fn new(cursor: usize, len: usize) -> Self {
        Self::with_size(cursor, len, PANEL_WINDOW)
    }

    /// Window of an explicit size following `cursor`.
    pub fn with_size(cursor: usize, len: usize, size: usize) -> Self {
        let range = window_range(cursor, len, size);
        Self {
            left_hidden: range.start > 0,
            right_hidden: range.end < len,
            range,
        }
    }
}

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

/// Render a windowed tab bar: `prefix` + visible tabs + `‹`/`›` markers.
/// `current` is the active tab index (drives the window).
pub fn tab_bar(
    prefix: &str,
    tabs: &[Tab<'_>],
    current: usize,
    palette: &ThemePalette,
) -> Line<'static> {
    let window = Window::new(current, tabs.len());
    let dim = Style::default().fg(palette.dim);

    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        prefix.to_string(),
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD),
    )];
    if window.left_hidden {
        spans.push(Span::styled("‹ ", dim));
    }
    for (i, tab) in tabs
        .iter()
        .enumerate()
        .take(window.range.end)
        .skip(window.range.start)
    {
        if i > window.range.start {
            spans.push(Span::styled(" > ", dim));
        }
        spans.push(Span::styled(
            tab.label.to_string(),
            tab_style(tab.state, palette),
        ));
    }
    if window.right_hidden {
        spans.push(Span::styled(" ›", dim));
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

/// Assemble one windowed option row: hidden-side markers + cursor + the
/// adapter-supplied content spans.
///
/// The `‹` marker prefixes the first visible row and `›` suffixes the last
/// one — both only when that side has hidden items.
pub fn option_row(
    window: &Window,
    row: usize,
    is_cursor: bool,
    palette: &ThemePalette,
    content: Vec<Span<'static>>,
) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if window.left_hidden && row == window.range.start {
        spans.push(Span::styled("‹ ", dim));
    }
    spans.push(cursor_span(is_cursor, palette));
    spans.extend(content);
    if window.right_hidden && row + 1 == window.range.end {
        spans.push(Span::styled(" ›", dim));
    }
    Line::from(spans)
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

    fn text(line: &Line<'static>) -> String {
        line.to_string()
    }

    // ── Window flags ────────────────────────────────────────────

    #[test]
    fn short_lists_have_no_hidden_sides() {
        let w = Window::new(0, 0);
        assert!(w.range.is_empty() && !w.left_hidden && !w.right_hidden);
        let w = Window::new(2, 5);
        assert_eq!(w.range, 0..5);
        assert!(!w.left_hidden, "len == size → nothing hidden");
        assert!(!w.right_hidden);
    }

    #[test]
    fn sliding_window_flags_both_sides() {
        // 8 items, window 5, cursor on the 6th (index 5) → 1..6, left hidden.
        let w = Window::new(5, 8);
        assert_eq!(w.range, 1..6);
        assert!(w.left_hidden);
        assert!(w.right_hidden);
        // Cursor pinned at the end: only the left side stays hidden.
        let w = Window::new(7, 8);
        assert_eq!(w.range, 3..8);
        assert!(w.left_hidden && !w.right_hidden);
    }

    // ── Tab bar ─────────────────────────────────────────────────

    #[test]
    fn tab_bar_renders_all_tabs_without_markers_when_short() {
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
        assert!(!out.contains('‹') && !out.contains('›'));
    }

    #[test]
    fn tab_bar_marks_hidden_sides_when_windowed() {
        let labels: Vec<String> = (0..7).map(|i| format!("Q{i}")).collect();
        let tabs: Vec<Tab> = labels
            .iter()
            .map(|l| Tab {
                label: l,
                state: TabState::Normal,
            })
            .collect();
        // Current on the last tab: window slides to 2..7 → left marker only.
        let out = text(&tab_bar("? ", &tabs, 6, &palette()));
        assert_eq!(out, "? ‹ Q2 > Q3 > Q4 > Q5 > Q6");
        // Current on the first tab: window pinned at 0..5 → right marker only.
        let out = text(&tab_bar("? ", &tabs, 0, &palette()));
        assert_eq!(out, "? Q0 > Q1 > Q2 > Q3 > Q4 ›");
        // Middle position (window 1..6): both sides hidden.
        let out = text(&tab_bar("? ", &tabs, 5, &palette()));
        assert_eq!(out, "? ‹ Q1 > Q2 > Q3 > Q4 > Q5 ›");
    }

    // ── Option rows ─────────────────────────────────────────────

    #[test]
    fn option_rows_have_no_markers_when_all_visible() {
        let w = Window::new(0, 3);
        let line = option_row(
            &w,
            1,
            true,
            &palette(),
            vec![label_span("hello", true, &palette())],
        );
        assert_eq!(text(&line), "❯ hello");
    }

    #[test]
    fn option_rows_mark_the_window_edges() {
        let w = Window::new(5, 8);
        // First visible row (index 1): left marker.
        let line = option_row(
            &w,
            1,
            false,
            &palette(),
            vec![label_span("m1", false, &palette())],
        );
        assert_eq!(text(&line), "‹   m1");
        // Last visible row (index 5): right marker, cursor row included.
        let line = option_row(
            &w,
            5,
            true,
            &palette(),
            vec![label_span("m5", true, &palette())],
        );
        assert_eq!(text(&line), "❯ m5 ›");
    }
}
