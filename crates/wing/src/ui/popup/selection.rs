//! Generic selectable list rendering — reused for command popup and sub-command popup.
//!
//! Renders a list of `(name, description)` pairs with:
//! - Highlighted selection row
//! - Filter text highlighting (matching chars bold)
//! - Scroll window for long lists

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::render::markdown::truncate_to_display_width;

/// Maximum rows to show before scrolling.
const MAX_VISIBLE_ROWS: usize = 8;

/// A single row in a selection popup.
#[derive(Debug, Clone)]
pub struct SelectionRow {
    /// Display name (e.g. "/model", "gpt-4o").
    pub name: String,
    /// Description text (e.g. "Switch model").
    pub description: String,
}

/// Navigation state for a selection list.
#[derive(Debug, Clone)]
pub struct SelectionState {
    /// Index of the currently selected item.
    pub selected: usize,
    /// Number of items in the list.
    pub count: usize,
    /// Scroll offset (first visible index).
    pub scroll: usize,
}

impl SelectionState {
    pub fn new(count: usize) -> Self {
        Self {
            selected: 0,
            count,
            scroll: 0,
        }
    }

    /// Update count and clamp selection.
    pub fn set_count(&mut self, count: usize) {
        self.count = count;
        if self.selected >= count {
            self.selected = count.saturating_sub(1);
        }
        // Adjust scroll to keep selected visible.
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_VISIBLE_ROWS {
            self.scroll = self.selected.saturating_sub(MAX_VISIBLE_ROWS - 1);
        }
    }

    /// Move selection up (wraps to last item at top).
    pub fn move_up(&mut self) {
        if self.count == 0 {
            return;
        }
        if self.selected > 0 {
            self.selected -= 1;
        } else {
            self.selected = self.count - 1;
        }
        self.adjust_scroll();
    }

    /// Move selection down (wraps to first item at bottom).
    pub fn move_down(&mut self) {
        if self.count == 0 {
            return;
        }
        if self.selected + 1 < self.count {
            self.selected += 1;
        } else {
            self.selected = 0;
        }
        self.adjust_scroll();
    }

    /// Keep the selected item visible within the scroll window.
    fn adjust_scroll(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_VISIBLE_ROWS {
            self.scroll = self.selected.saturating_sub(MAX_VISIBLE_ROWS - 1);
        }
    }
}

/// Calculate the popup height needed for the given number of rows.
pub fn popup_height(row_count: usize) -> u16 {
    row_count.min(MAX_VISIBLE_ROWS) as u16
}

/// Highlight matching characters in the name with bold+underline.
fn highlight_matches(name: &str, filter: &str, accent: Color) -> Vec<Span<'static>> {
    if filter.is_empty() {
        return vec![Span::styled(name.to_string(), Style::default().fg(accent))];
    }

    let filter_chars: Vec<char> = filter.to_lowercase().chars().collect();

    let mut spans = Vec::new();
    let mut fi = 0;

    for ch in name.chars() {
        if fi < filter_chars.len() && ch.to_lowercase().next() == Some(filter_chars[fi]) {
            spans.push(Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(accent)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
            fi += 1;
        } else {
            spans.push(Span::styled(ch.to_string(), Style::default().fg(accent)));
        }
    }

    spans
}

/// Render a selection popup widget.
pub struct SelectionPopup<'a> {
    rows: &'a [SelectionRow],
    state: &'a SelectionState,
    filter: &'a str,
    palette: &'a ThemePalette,
}

impl<'a> SelectionPopup<'a> {
    pub fn new(
        rows: &'a [SelectionRow],
        state: &'a SelectionState,
        filter: &'a str,
        palette: &'a ThemePalette,
    ) -> Self {
        Self {
            rows,
            state,
            filter,
            palette,
        }
    }
}

impl Widget for SelectionPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let accent = self.palette.accent;
        let dim_color = self.palette.dim;

        // Calculate column widths.
        let name_width = self
            .rows
            .iter()
            .map(|r| UnicodeWidthStr::width(r.name.as_str()))
            .max()
            .unwrap_or(10)
            .min(area.width as usize / 2);

        // Render visible rows.
        let visible = area.height as usize;
        let start = self.state.scroll;
        let end = (start + visible).min(self.rows.len());

        for (i, idx) in (start..end).enumerate() {
            let row = &self.rows[idx];
            let y = area.y + i as u16;
            let is_selected = idx == self.state.selected;

            let bg = if is_selected { dim_color } else { Color::Reset };
            let base_style = Style::default().bg(bg);

            // Clear the row first.
            for x in area.x..area.right() {
                if buf.area().contains(ratatui::layout::Position::new(x, y)) {
                    buf[(x, y)].set_style(base_style);
                }
            }

            // Build the line.
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled(" ", base_style));

            // Name with highlighting.
            let name_spans = highlight_matches(&row.name, self.filter, accent);
            for span in name_spans {
                spans.push(Span::styled(span.content, span.style.patch(base_style)));
            }

            // Pad name column.
            let name_display_w = UnicodeWidthStr::width(row.name.as_str());
            let pad = name_width.saturating_sub(name_display_w);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), base_style));
            }

            // Separator + description.
            spans.push(Span::styled("  ", base_style));
            let used = name_width + 3;
            let desc_w = area.width as usize - used;
            let desc = if UnicodeWidthStr::width(row.description.as_str()) > desc_w {
                truncate_to_display_width(&row.description, desc_w)
            } else {
                row.description.clone()
            };
            let desc_fg = if is_selected { Color::Gray } else { dim_color };
            spans.push(Span::styled(
                desc,
                Style::default().fg(desc_fg).patch(base_style),
            ));

            let line = Line::from(spans);
            buf.set_line(area.x, y, &line, area.width);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selection_state_new() {
        let state = SelectionState::new(5);
        assert_eq!(state.selected, 0);
        assert_eq!(state.count, 5);
    }

    #[test]
    fn test_selection_state_navigation() {
        let mut state = SelectionState::new(3);
        state.move_down();
        assert_eq!(state.selected, 1);
        state.move_down();
        assert_eq!(state.selected, 2);
        state.move_down();
        assert_eq!(state.selected, 0); // wraps to first
        state.move_up();
        assert_eq!(state.selected, 2); // wraps to last
    }

    #[test]
    fn test_selection_state_count_change_clamps() {
        let mut state = SelectionState::new(5);
        state.selected = 4;
        state.set_count(3);
        assert_eq!(state.selected, 2); // clamped to new max
    }

    #[test]
    fn test_selection_state_scroll() {
        let mut state = SelectionState::new(20);
        // Move down past MAX_VISIBLE_ROWS.
        for _ in 0..10 {
            state.move_down();
        }
        assert_eq!(state.selected, 10);
        // Scroll should keep selected visible.
        assert!(state.selected >= state.scroll);
        assert!(state.selected < state.scroll + MAX_VISIBLE_ROWS);
    }

    #[test]
    fn test_popup_height() {
        assert_eq!(popup_height(0), 0);
        assert_eq!(popup_height(3), 3);
        assert_eq!(popup_height(100), MAX_VISIBLE_ROWS as u16);
    }

    #[test]
    fn test_highlight_matches() {
        let spans = highlight_matches("/model", "mo", Color::Cyan);
        // First two chars should be highlighted (bold).
        assert!(spans.len() >= 2);
    }

    #[test]
    fn test_highlight_empty_filter() {
        let spans = highlight_matches("/help", "", Color::Cyan);
        assert_eq!(spans.len(), 1);
    }
}
