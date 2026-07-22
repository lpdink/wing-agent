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

/// Maximum rows to show before scrolling (single-line popups).
const MAX_VISIBLE_ROWS: usize = 8;

/// Maximum rich (double-line) rows to show before scrolling (session popup).
const MAX_VISIBLE_RICH_ROWS: usize = 6;

/// Session 运行时状态（与后端 `SessionStatus` 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// 在磁盘、未被 resume。
    Inactive,
    /// 已 resume、空闲。
    Idle,
    /// agent 正在处理 turn。
    Working,
    /// agent 阻塞在 ask / need_feedback，等待用户反馈。
    Waiting,
}

impl SessionStatus {
    /// 从后端字符串解析；未知值（含空串，旧网关）降级为 Inactive。
    pub fn parse(s: &str) -> Self {
        match s {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "waiting" => Self::Waiting,
            _ => Self::Inactive,
        }
    }

    /// 状态图标：inactive/idle/working 为实心圆点，waiting 为非 emoji 的 `?`。
    pub fn icon(self) -> &'static str {
        match self {
            Self::Waiting => "?",
            _ => "●",
        }
    }

    /// 状态图标颜色。
    pub fn color(self) -> Color {
        match self {
            Self::Inactive => Color::DarkGray,
            Self::Idle => Color::White,
            Self::Working => Color::Yellow,
            Self::Waiting => Color::Magenta,
        }
    }

    /// 前端排序优先级（越小越靠前）：waiting > working > idle > inactive。
    pub fn rank(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Working => 1,
            Self::Idle => 2,
            Self::Inactive => 3,
        }
    }
}

/// 双行 session 候选的富渲染数据。
#[derive(Debug, Clone)]
pub struct RichSessionRow {
    /// 运行时状态（决定第一行的图标与颜色）。
    pub status: SessionStatus,
    /// 第一行：session 工作目录。
    pub workspace: String,
    /// 第一行右侧：最后活跃时间（人类可读）。
    pub last_active: String,
    /// 第二行：session 标题。
    pub title: String,
}

/// Format an ISO 8601 timestamp (e.g. `2025-07-22T21:41:28.123`) into a compact
/// display string `07-22 21:41`. Returns empty string for unparseable input.
pub fn format_last_active(iso: &str) -> String {
    // Expect "YYYY-MM-DDTHH:MM:SS..." — extract MM-DD HH:MM.
    let s = iso.trim();
    if s.len() < 16 {
        return String::new();
    }
    let bytes = s.as_bytes();
    // Validate minimal structure: digits and separators at expected positions.
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' || bytes[13] != b':' {
        return String::new();
    }
    // "MM-DD HH:MM"
    format!("{}-{} {}:{}", &s[5..7], &s[8..10], &s[11..13], &s[14..16])
}

/// A single row in a selection popup.
#[derive(Debug, Clone)]
pub struct SelectionRow {
    /// Display name (e.g. "/model", "gpt-4o"). For session rows this is the
    /// session id used as the completion value (not displayed verbatim).
    pub name: String,
    /// Description text (e.g. "Switch model"). Unused for rich session rows.
    pub description: String,
    /// Optional rich two-line layout (session popup). When `Some`, overrides
    /// the single-line render.
    pub rich: Option<RichSessionRow>,
}

impl SelectionRow {
    /// Display height in terminal lines (rich rows occupy two lines).
    pub fn height(&self) -> usize {
        if self.rich.is_some() { 2 } else { 1 }
    }
}

/// Convenience constructor for a plain single-line row.
pub fn plain_row(name: impl Into<String>, description: impl Into<String>) -> SelectionRow {
    SelectionRow {
        name: name.into(),
        description: description.into(),
        rich: None,
    }
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
    /// Maximum number of items visible at once (scroll window size, in items).
    /// Single-line popups use [`MAX_VISIBLE_ROWS`]; double-line session popups
    /// use a smaller value so the rendered line count stays bounded.
    pub max_visible: usize,
}

impl SelectionState {
    pub fn new(count: usize) -> Self {
        Self::with_max_visible(count, MAX_VISIBLE_ROWS)
    }

    /// Create state with an explicit visible-item window (e.g. rich session popup).
    pub fn with_max_visible(count: usize, max_visible: usize) -> Self {
        Self {
            selected: 0,
            count,
            scroll: 0,
            max_visible: max_visible.max(1),
        }
    }

    /// Create state with the last item selected (scroll adjusted).
    pub fn new_selecting_last(count: usize) -> Self {
        Self::new_selecting_last_with_max_visible(count, MAX_VISIBLE_ROWS)
    }

    /// Create state with the last item selected and an explicit visible-item window.
    pub fn new_selecting_last_with_max_visible(count: usize, max_visible: usize) -> Self {
        let max_visible = max_visible.max(1);
        let selected = count.saturating_sub(1);
        Self {
            selected,
            count,
            scroll: selected.saturating_sub(max_visible - 1),
            max_visible,
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
        } else if self.selected >= self.scroll + self.max_visible {
            self.scroll = self.selected.saturating_sub(self.max_visible - 1);
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
        } else if self.selected >= self.scroll + self.max_visible {
            self.scroll = self.selected.saturating_sub(self.max_visible - 1);
        }
    }
}

/// Calculate the popup height (in terminal lines) for the given rows.
///
/// Rich (session) rows occupy two lines each. The height is the sum of the
/// visible window's row heights, capped by the rows' own window size.
///
/// NOTE: assumes uniform row height within a popup (all rich or all plain),
/// so summing from index 0 is equivalent to summing from any scroll offset.
pub fn popup_height(rows: &[SelectionRow], max_visible: usize) -> u16 {
    let visible = rows.len().min(max_visible);
    rows.iter().take(visible).map(|r| r.height()).sum::<usize>() as u16
}

/// Default visible-item window for rich (double-line) session popups.
pub fn rich_max_visible() -> usize {
    MAX_VISIBLE_RICH_ROWS
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

    /// Clear a single terminal line with the given background style.
    fn clear_line(buf: &mut Buffer, area: Rect, y: u16, style: Style) {
        for x in area.x..area.right() {
            if buf.area().contains(ratatui::layout::Position::new(x, y)) {
                buf[(x, y)].set_style(style);
            }
        }
    }

    /// Render a plain single-line row: `<name>  <description>`.
    #[allow(clippy::too_many_arguments)]
    fn render_plain_row(
        &self,
        row: &SelectionRow,
        y: u16,
        area: Rect,
        base_style: Style,
        accent: Color,
        dim_color: Color,
        is_selected: bool,
        name_width: usize,
        buf: &mut Buffer,
    ) {
        Self::clear_line(buf, area, y, base_style);

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

    /// Render a rich two-line session row:
    /// line 1 = ` <icon> <workspace>  <last_active>`, line 2 = `    <title>`.
    #[allow(clippy::too_many_arguments)]
    fn render_rich_row(
        &self,
        rich: &RichSessionRow,
        y: u16,
        area: Rect,
        base_style: Style,
        accent: Color,
        dim_color: Color,
        is_selected: bool,
        buf: &mut Buffer,
    ) {
        let width = area.width as usize;

        // ── Line 1: status icon + workspace + [last_active] ──
        Self::clear_line(buf, area, y, base_style);
        let mut line1: Vec<Span<'static>> = Vec::new();
        line1.push(Span::styled(" ", base_style));
        line1.push(Span::styled(
            rich.status.icon(),
            Style::default().fg(rich.status.color()).patch(base_style),
        ));
        line1.push(Span::styled(" ", base_style));

        // "● " prefix occupies 3 columns. Time tag " [MM-DD HH:MM]" = 14 cols.
        let time_tag = if rich.last_active.is_empty() {
            String::new()
        } else {
            format!(" [{}]", rich.last_active)
        };
        let time_tag_w = UnicodeWidthStr::width(time_tag.as_str());
        let available = width.saturating_sub(3);
        let ws_budget = available.saturating_sub(time_tag_w);

        let workspace = if rich.workspace.is_empty() {
            "(no workspace)".to_string()
        } else if UnicodeWidthStr::width(rich.workspace.as_str()) > ws_budget {
            truncate_to_display_width(&rich.workspace, ws_budget)
        } else {
            rich.workspace.clone()
        };
        line1.push(Span::styled(
            workspace,
            Style::default().fg(accent).patch(base_style),
        ));

        if !time_tag.is_empty() {
            let time_fg = if is_selected { accent } else { dim_color };
            line1.push(Span::styled(
                time_tag,
                Style::default().fg(time_fg).patch(base_style),
            ));
        }

        buf.set_line(area.x, y, &Line::from(line1), area.width);

        // ── Line 2: indented title ──
        let y2 = y + 1;
        Self::clear_line(buf, area, y2, base_style);
        let mut line2: Vec<Span<'static>> = Vec::new();
        line2.push(Span::styled("    ", base_style));
        let title_w = width.saturating_sub(5);
        let title = if rich.title.is_empty() {
            "(untitled)".to_string()
        } else if UnicodeWidthStr::width(rich.title.as_str()) > title_w {
            truncate_to_display_width(&rich.title, title_w)
        } else {
            rich.title.clone()
        };
        if is_selected {
            // Highlight filter matches in the title when selected.
            for span in highlight_matches(&title, self.filter, accent) {
                line2.push(Span::styled(span.content, span.style.patch(base_style)));
            }
        } else {
            line2.push(Span::styled(
                title,
                Style::default().fg(dim_color).patch(base_style),
            ));
        }
        buf.set_line(area.x, y2, &Line::from(line2), area.width);
    }
}

impl Widget for SelectionPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let accent = self.palette.accent;
        let dim_color = self.palette.dim;

        // Column width for plain (single-line) rows.
        let name_width = self
            .rows
            .iter()
            .map(|r| UnicodeWidthStr::width(r.name.as_str()))
            .max()
            .unwrap_or(10)
            .min(area.width as usize / 2);

        // Render visible rows, advancing y by each row's height.
        let bottom = area.y + area.height;
        let mut y = area.y;
        for idx in self.state.scroll..self.rows.len() {
            if y >= bottom {
                break;
            }
            let row = &self.rows[idx];
            let is_selected = idx == self.state.selected;
            let bg = if is_selected { dim_color } else { Color::Reset };
            let base_style = Style::default().bg(bg);

            if let Some(rich) = &row.rich {
                if y + 1 >= bottom {
                    break; // not enough room for both lines
                }
                self.render_rich_row(
                    rich,
                    y,
                    area,
                    base_style,
                    accent,
                    dim_color,
                    is_selected,
                    buf,
                );
                y += 2;
            } else {
                self.render_plain_row(
                    row,
                    y,
                    area,
                    base_style,
                    accent,
                    dim_color,
                    is_selected,
                    name_width,
                    buf,
                );
                y += 1;
            }
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
        let plain = |n: String| plain_row(n, "");
        let rows: Vec<SelectionRow> = (0..3).map(|i| plain(format!("r{i}"))).collect();
        assert_eq!(popup_height(&[], MAX_VISIBLE_ROWS), 0);
        assert_eq!(popup_height(&rows, MAX_VISIBLE_ROWS), 3);
        let many: Vec<SelectionRow> = (0..100).map(|i| plain(format!("r{i}"))).collect();
        assert_eq!(
            popup_height(&many, MAX_VISIBLE_ROWS),
            MAX_VISIBLE_ROWS as u16
        );
    }

    #[test]
    fn test_popup_height_rich_rows_are_two_lines() {
        let rows: Vec<SelectionRow> = (0..3)
            .map(|i| SelectionRow {
                name: format!("s{i}"),
                description: String::new(),
                rich: Some(RichSessionRow {
                    status: SessionStatus::Idle,
                    workspace: "/tmp".into(),
                    last_active: "07-22 21:41".into(),
                    title: format!("title {i}"),
                }),
            })
            .collect();
        // 3 rich rows × 2 lines = 6.
        assert_eq!(popup_height(&rows, MAX_VISIBLE_RICH_ROWS), 6);
        // Capped by max_visible: 2 rich rows × 2 = 4.
        assert_eq!(popup_height(&rows, 2), 4);
    }

    #[test]
    fn test_session_status_from_str() {
        assert_eq!(SessionStatus::parse("idle"), SessionStatus::Idle);
        assert_eq!(SessionStatus::parse("working"), SessionStatus::Working);
        assert_eq!(SessionStatus::parse("waiting"), SessionStatus::Waiting);
        assert_eq!(SessionStatus::parse("inactive"), SessionStatus::Inactive);
        // Unknown / empty (old gateway) degrades to Inactive.
        assert_eq!(SessionStatus::parse(""), SessionStatus::Inactive);
        assert_eq!(SessionStatus::parse("bogus"), SessionStatus::Inactive);
    }

    #[test]
    fn test_session_status_icons() {
        assert_eq!(SessionStatus::Inactive.icon(), "●");
        assert_eq!(SessionStatus::Idle.icon(), "●");
        assert_eq!(SessionStatus::Working.icon(), "●");
        // waiting uses a non-emoji glyph, distinct from the dots.
        assert_eq!(SessionStatus::Waiting.icon(), "?");
    }

    #[test]
    fn test_session_status_rank_order() {
        // waiting > working > idle > inactive
        assert!(SessionStatus::Waiting.rank() < SessionStatus::Working.rank());
        assert!(SessionStatus::Working.rank() < SessionStatus::Idle.rank());
        assert!(SessionStatus::Idle.rank() < SessionStatus::Inactive.rank());
    }

    #[test]
    fn test_row_height() {
        assert_eq!(plain_row("a", "b").height(), 1);
        let rich = SelectionRow {
            name: "id".into(),
            description: String::new(),
            rich: Some(RichSessionRow {
                status: SessionStatus::Working,
                workspace: "/w".into(),
                last_active: String::new(),
                title: "t".into(),
            }),
        };
        assert_eq!(rich.height(), 2);
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

    #[test]
    fn test_format_last_active() {
        assert_eq!(
            format_last_active("2025-07-22T21:41:28.123456"),
            "07-22 21:41"
        );
        assert_eq!(format_last_active("2025-01-05T08:03:00"), "01-05 08:03");
        // Too short or malformed → empty.
        assert_eq!(format_last_active(""), "");
        assert_eq!(format_last_active("short"), "");
        assert_eq!(format_last_active("not-a-timestamp!!"), "");
    }
}
