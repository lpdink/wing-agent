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
use crate::protocol::SessionStatus;
use crate::render::markdown::truncate_left_to_display_width;
use crate::render::markdown::truncate_to_display_width;

/// Maximum rows to show before scrolling (single-line popups).
const MAX_VISIBLE_ROWS: usize = 8;

/// Maximum rich (double-line) rows to show before scrolling (session popup).
const MAX_VISIBLE_RICH_ROWS: usize = 6;

/// Presentation of [`SessionStatus`] in the session picker (the vocabulary
/// itself lives in the protocol layer — one definition, shared with the event
/// projections).
impl SessionStatus {
    /// 状态图标：inactive/idle/working 为实心圆点，waiting 为非 emoji 的 `?`。
    pub fn icon(self) -> &'static str {
        match self {
            Self::Waiting => "?",
            _ => "●",
        }
    }

    /// 状态图标颜色（走调色板槽位，不引命名 ANSI 色：主题改一次，这里跟着变）。
    ///
    /// Working = `warning`（忙），Waiting = `accent`（轮到你了），Idle = `text`，
    /// Inactive = `dim`（被逐出内存的会话）。
    pub fn color(self, palette: &ThemePalette) -> Color {
        match self {
            Self::Working => palette.warning,
            Self::Waiting => palette.accent,
            Self::Idle => palette.text,
            Self::Inactive => palette.dim,
        }
    }
}

/// 双行 session 候选的富渲染数据。
#[derive(Debug, Clone)]
pub struct RichSessionRow {
    /// 运行时状态（决定第一行的图标与颜色）。
    pub status: SessionStatus,
    /// 第一行：最后活跃时间（人类可读）。
    pub last_active: String,
    /// 第二行：session 标题。
    pub title: String,
    /// 第一行：session 工作目录（**只展示**，不参与排序）。
    pub workspace: String,
    /// 是否被 pin —— 第一行末尾一个星标（`★`）；未 pin 不占位。
    pub pinned: bool,
    /// 树形渲染前缀（`shared::session_tree` 的连接符；根 / 搜索态 = 空串）。
    /// 行 1 从它右边开始画，行 2 的缩进按它的宽度对齐。
    pub tree_prefix: String,
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
    // 前面四个分隔符检查顺带保证了 5/8/11/14 都是字符边界，16 没有：一个横跨
    // 14..16 的非 ASCII 字符（手改 metadata / 第三方网关写脏的 last_interaction）
    // 会让字节切片 panic，而这里的契约是「看不懂就给空串」。
    let Some(minute) = s.get(14..16) else {
        return String::new();
    };
    // "MM-DD HH:MM"
    format!("{}-{} {}:{}", &s[5..7], &s[8..10], &s[11..13], minute)
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
    /// Scroll offset — the first row the list *wants* drawn. 版面给的矩形装不下
    /// `max_visible` 条时渲染侧会按矩形收缩窗口（`visible_window`），这里记的是
    /// 导航侧的意图。
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

    /// Select a specific item (clamped, scroll adjusted to keep it visible).
    pub fn select(&mut self, index: usize) {
        self.selected = index.min(self.count.saturating_sub(1));
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

/// 一个高 `height` 的矩形里装得下多少条行（整行装不下就停在它前面）。
///
/// 与 [`popup_height`] 同一条口径（按行高累加、从第 0 条起算），所以「要了多高」
/// 与「装得下几条」永远是同一份数学。
fn rows_fitting(rows: &[SelectionRow], height: u16) -> usize {
    let mut used = 0usize;
    for (idx, row) in rows.iter().enumerate() {
        used += row.height();
        if used > height as usize {
            return idx;
        }
    }
    rows.len()
}

/// 这一帧真正画得出来的行窗口（`first..end`，单位是**条目**，不是行）。
///
/// [`SelectionState::scroll`] 只是导航侧的意图：它按 `max_visible` 条维护窗口，
/// 而版面给 popup 的矩形未必有 `max_visible` 条那么高（矮终端上被挤过的 popup 行、
/// 开屏里挂在输入卡下面的浮层，见 `app::splash_popup`）。窗口因此**以矩形为准**：
/// 取 `min(max_visible, 装得下的条数)`，再把窗口挪到包住 `selected` —— 选中的那一条
/// 一定在画出来的范围里，否则高亮会停在视口外。
///
/// 窗口语义沿用 [`SelectionState`]：先钉住 `scroll`，选中项越界才滑；到底时
/// 窗口贴住列表尾部（露出的是最后几条，不是被裁掉的那几条）。
///
/// 前提：全列表行高一致（[`rows_fitting`] 从第 0 条起累加，见 [`popup_height`]）。
fn visible_window(
    rows: &[SelectionRow],
    state: &SelectionState,
    height: u16,
) -> std::ops::Range<usize> {
    debug_assert!(
        rows.windows(2).all(|w| w[0].height() == w[1].height()),
        "popup rows must share one height"
    );
    let window = rows_fitting(rows, height).min(state.max_visible);
    if window == 0 {
        return 0..0;
    }
    let selected = state.selected.min(rows.len() - 1);
    // 窗口必须包住选中项：`first` 落在 `[selected + 1 - window, selected]` 里。
    // 导航侧本来就维持着这个区间（先钉住 `scroll`，越界才滑），只有窗口被矩形
    // 缩小之后两者才会分开 —— 夹取把「高亮永远在画出来的范围里」写成一句。
    let lo = (selected + 1).saturating_sub(window);
    let first = state.scroll.min(rows.len() - window).clamp(lo, selected);
    first..first + window
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

/// 一次行渲染的上下文：同一帧内跨行不变的色板（强调 / 暗淡）与逐行变化的选中态
/// （它同时决定底色）。打包成一个参数是为了不再给两个行渲染函数各挂九个位置参数。
#[derive(Clone, Copy)]
struct RowStyle {
    /// Background only — **never** a foreground: spans carry their own hues and
    /// are combined with `Style::patch`, whose `other.fg.or(self.fg)` would let
    /// a colour here flatten every span (a selected row's band is `surface`;
    /// the copy on it is handled by the per-span `fg` decisions instead).
    base: Style,
    accent: Color,
    dim: Color,
    /// Foreground for the row's quiet description on the selected band
    /// (`surface` under the terminal default would be a light/dark mismatch).
    fg: Color,
    selected: bool,
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
    fn render_plain_row(
        &self,
        row: &SelectionRow,
        y: u16,
        area: Rect,
        style: RowStyle,
        name_width: usize,
        buf: &mut Buffer,
    ) {
        let RowStyle {
            base: base_style,
            accent,
            dim: dim_color,
            fg: row_fg,
            selected: is_selected,
        } = style;
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
        let desc_fg = if is_selected { row_fg } else { dim_color };
        spans.push(Span::styled(
            desc,
            Style::default().fg(desc_fg).patch(base_style),
        ));

        let line = Line::from(spans);
        buf.set_line(area.x, y, &line, area.width);
    }

    /// Render a rich two-line session row:
    /// line 1 = `[<tree prefix>] <icon> <workspace>  [<last_active>]` + ` ★`（仅 pinned）,
    /// line 2 = `    <title>`（缩进与前缀宽度对齐）。
    ///
    /// workspace 只做展示（#145 删掉的是「按 workspace 排序」，不是这段显示）：
    /// 行 1 的容量先让给时间标签与星标，再让给树前缀（放不下时截前缀，保尾部
    /// 连接符），剩下的才画 workspace（超长截断）。
    fn render_rich_row(
        &self,
        rich: &RichSessionRow,
        y: u16,
        area: Rect,
        style: RowStyle,
        buf: &mut Buffer,
    ) {
        let RowStyle {
            base: base_style,
            accent,
            dim: dim_color,
            fg: _,
            selected: is_selected,
        } = style;
        let width = area.width as usize;

        // ── Line 1: [tree prefix] + status icon + workspace + [last_active] + [★] ──
        // 让位顺序：星标 / 时间标签 → 前缀 → workspace。深链前缀的头几十列是
        // 每行都一样的引导槽（信息量最低），但前缀自己也要让位——它不能反过来
        // 把图标甚至整行挤出面板（裁剪保留尾部：连接符与最近几层缩进）。
        //
        // 前缀的预算按**行的最宽形态**（星标 + 时间标签）统一扣除，不随本行
        // 实际有没有它们变化：同深度的行前缀一样长，裁剪宽度若各行不同，带星标
        // 的那行就会整体左移 2 列——树的引导线在同一屏里各画各的。空闲的列
        // 归 workspace（它按本行的实际占用算）。
        const STAR_W: usize = 2; // " ★"
        // 必须等于 `format_last_active` 的输出宽度（恒 14 列 = 11 字符 + 空格
        // 与方括号；空串时没有时间标签）：它是记账用的格式常量，不是实测值，
        // 改格式（例如加秒）必须同时改这里，否则行 1 会超宽。
        const TIME_TAG_W: usize = 14; // " [MM-DD HH:MM]"
        let time_tag = if rich.last_active.is_empty() {
            String::new()
        } else {
            format!(" [{}]", rich.last_active)
        };
        let star_w = if rich.pinned { STAR_W } else { 0 };
        let max_prefix_w = width.saturating_sub(3 + STAR_W + TIME_TAG_W).max(3);
        let prefix = if UnicodeWidthStr::width(rich.tree_prefix.as_str()) > max_prefix_w {
            truncate_left_to_display_width(&rich.tree_prefix, max_prefix_w)
        } else {
            rich.tree_prefix.clone()
        };
        let prefix_w = UnicodeWidthStr::width(prefix.as_str());

        Self::clear_line(buf, area, y, base_style);
        let mut line1: Vec<Span<'static>> = Vec::new();
        if prefix_w > 0 {
            // 连接线用暗淡色（选中行染 accent，与时间标签同款）——它是结构，
            // 不该和会话内容抢注意力。
            let prefix_fg = if is_selected { accent } else { dim_color };
            line1.push(Span::styled(
                prefix,
                Style::default().fg(prefix_fg).patch(base_style),
            ));
        }
        line1.push(Span::styled(" ", base_style));
        line1.push(Span::styled(
            rich.status.icon(),
            Style::default()
                .fg(rich.status.color(self.palette))
                .patch(base_style),
        ));
        line1.push(Span::styled(" ", base_style));

        // "<prefix> ● " prefix occupies prefix_w + 3 columns; the time tag
        // " [MM-DD HH:MM]" 14; the pin star " ★" 2. The workspace gets what is left.
        let available = width.saturating_sub(prefix_w + 3 + star_w);
        let ws_budget = available.saturating_sub(UnicodeWidthStr::width(time_tag.as_str()));

        let workspace = if rich.workspace.is_empty() {
            // 占位符同样走预算：它替的是 workspace 这一格，不该反过来把时间
            // 标签顶出行外。
            truncate_to_display_width("(no workspace)", ws_budget)
        } else if UnicodeWidthStr::width(rich.workspace.as_str()) > ws_budget {
            // 左截断：路径的区分度在尾段（`…/ws/wing-agent`）——右截断留下的
            // 是每个会话都一样的 `/Users/…` 前缀，等于没显示。
            truncate_left_to_display_width(&rich.workspace, ws_budget)
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
        if rich.pinned {
            // 星标只标"已 pin"（未 pin 不占位）：展示面在 pin 与否之间只差
            // 这一个字形，行宽不因 pin 变化而抖动。
            line1.push(Span::styled(
                " ★",
                Style::default().fg(self.palette.warning).patch(base_style),
            ));
        }

        buf.set_line(area.x, y, &Line::from(line1), area.width);

        // ── Line 2: indented title ──
        // 缩进与前缀宽度对齐（`prefix_w + 4`，根行照旧是 4 列）：行 2 的内容
        // 跟着行 1 挪，两行读起来是同一条目的上下两段，而不是错位的两块。
        let y2 = y + 1;
        Self::clear_line(buf, area, y2, base_style);
        let mut line2: Vec<Span<'static>> = Vec::new();
        line2.push(Span::styled(" ".repeat(prefix_w + 4), base_style));
        let title_w = width.saturating_sub(prefix_w + 5);
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

        // 画窗口里的行。`visible_window` 已经保证了每条都装得下（`rows_fitting`
        // 与 `popup_height` 同一份数学），两个 `bottom` 判断是**矩形栅栏**：
        // 万一行高不再整齐（混排），宁可少画一条也不许画到矩形外去覆盖邻座
        // 的控件（`buf.set_line` 只按缓冲区裁剪，不认这个矩形）。
        //
        // `selected` 与窗口用同一条夹取口径：过界的陈旧下标两边都按最后一条算，
        // 否则窗口正常却一条高亮都没有。
        let selected = self.state.selected.min(self.rows.len().saturating_sub(1));
        let bottom = area.y + area.height;
        let mut y = area.y;
        for idx in visible_window(self.rows, self.state, area.height) {
            if y >= bottom {
                break;
            }
            let row = &self.rows[idx];
            let is_selected = idx == selected;
            let (bg, fg) = if is_selected {
                (self.palette.selection_band(), self.palette.text)
            } else {
                (Color::Reset, Color::Reset)
            };
            let style = RowStyle {
                base: Style::default().bg(bg),
                accent,
                dim: dim_color,
                fg,
                selected: is_selected,
            };

            if let Some(rich) = &row.rich {
                if y + 1 >= bottom {
                    break;
                }
                self.render_rich_row(rich, y, area, style, buf);
                y += 2;
            } else {
                self.render_plain_row(row, y, area, style, name_width, buf);
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
                    last_active: "07-22 21:41".into(),
                    title: format!("title {i}"),
                    workspace: "/ws".into(),
                    pinned: false,
                    tree_prefix: String::new(),
                }),
            })
            .collect();
        // 3 rich rows × 2 lines = 6.
        assert_eq!(popup_height(&rows, MAX_VISIBLE_RICH_ROWS), 6);
        // Capped by max_visible: 2 rich rows × 2 = 4.
        assert_eq!(popup_height(&rows, 2), 4);
    }

    /// `count` 条双行 session 行（标题 `title-NN`）。
    fn rich_rows(count: usize) -> Vec<SelectionRow> {
        (0..count)
            .map(|i| session_row(SessionStatus::Idle, "07-22 21:41", &format!("title-{i:02}")))
            .collect()
    }

    #[test]
    fn test_rows_fitting_counts_whole_rows_only() {
        // 与 `popup_height` 同一份数学：装不下整条就停在它前面（rich 行两行一条，
        // 奇数高的矩形最后那条只露出半截 —— 那半截不画）。
        assert_eq!(rows_fitting(&[], 10), 0);
        let plain: Vec<SelectionRow> = (0..5).map(|i| plain_row(format!("r{i}"), "")).collect();
        assert_eq!(rows_fitting(&plain, 0), 0);
        assert_eq!(rows_fitting(&plain, 3), 3);
        assert_eq!(rows_fitting(&plain, 99), 5);

        let rich = rich_rows(5);
        assert_eq!(rows_fitting(&rich, 1), 0);
        assert_eq!(rows_fitting(&rich, 7), 3);
        assert_eq!(rows_fitting(&rich, 8), 4);
        assert_eq!(rows_fitting(&rich, 99), 5);
    }

    #[test]
    fn test_visible_window_is_the_state_scroll_when_the_rect_fits() {
        // 矩形装得下策略窗口时渲染侧不改任何东西（会话里的常规版式）。
        let rich = rich_rows(12);
        let mut state = SelectionState::with_max_visible(rich.len(), MAX_VISIBLE_RICH_ROWS);
        assert_eq!(visible_window(&rich, &state, 12), 0..6);
        for _ in 0..7 {
            state.move_down();
        }
        assert_eq!(state.scroll, 2);
        assert_eq!(visible_window(&rich, &state, 12), 2..8);
        assert_eq!(
            visible_window(&rich, &state, 40),
            2..8,
            "更高的矩形不改变窗口"
        );
    }

    #[test]
    fn test_rows_fitting_inverts_popup_height() {
        // 「要了多高」与「装得下几条」是同一份数学：反过来也成立（两份实现由
        // 这条性质钉在一起，改一份必须同时改另一份）。
        let plain: Vec<SelectionRow> = (0..6).map(|i| plain_row(format!("r{i}"), "")).collect();
        let rich = rich_rows(6);
        for n in 1..=8 {
            assert_eq!(
                rows_fitting(&plain, popup_height(&plain, n)),
                plain.len().min(n)
            );
            assert_eq!(
                rows_fitting(&rich, popup_height(&rich, n)),
                rich.len().min(n)
            );
        }
    }

    #[test]
    fn test_visible_window_shrinks_to_the_rect_and_keeps_the_selection_inside() {
        // 矩形只有 7 行 = 3 条 rich 行，而策略窗口是 6 条：滚到底时窗口贴住尾部，
        // 选中的最后一条落在窗口里。
        let rich = rich_rows(12);
        let mut state = SelectionState::with_max_visible(rich.len(), MAX_VISIBLE_RICH_ROWS);
        for _ in 0..rich.len() - 1 {
            state.move_down();
        }
        assert_eq!((state.selected, state.scroll), (11, 6));
        let window = visible_window(&rich, &state, 7);
        assert_eq!(window, 9..12, "窗口按矩形收缩并贴住尾部");
        assert!(
            window.contains(&state.selected),
            "选中的这条在窗口里：{window:?} vs {}",
            state.selected
        );

        // 选中项跑到窗口外（窗口比策略窗口小、scroll 还钉在上面）时窗口跟着它滑。
        let mut state = SelectionState::with_max_visible(rich.len(), MAX_VISIBLE_RICH_ROWS);
        state.select(9);
        assert_eq!(visible_window(&rich, &state, 7), 7..10);
        // 往上走不滑：只要选中项还在窗口里，窗口就钉住 `scroll`。
        state.select(7);
        assert_eq!(visible_window(&rich, &state, 7), 5..8);
        state.select(6);
        assert_eq!(visible_window(&rich, &state, 7), 4..7);

        // 连一条 rich 行都装不下 → 什么都不画（不是画半条出去）。
        assert_eq!(visible_window(&rich, &state, 1), 0..0);
    }

    #[test]
    fn test_visible_window_covers_the_last_item_of_a_short_plain_list() {
        // 单行窗口 8 条，矩形只有 7 行：最后一条不能掉在矩形外面。
        let plain: Vec<SelectionRow> = (0..10).map(|i| plain_row(format!("r{i:02}"), "")).collect();
        let mut state = SelectionState::new(plain.len());
        for _ in 0..plain.len() - 1 {
            state.move_down();
        }
        assert_eq!((state.selected, state.scroll), (9, 2));
        assert_eq!(visible_window(&plain, &state, 7), 3..10);
    }

    #[test]
    fn test_visible_window_clamps_a_stale_selection() {
        // rows 变短（过滤收窄）时 selected 可能还没被 `set_count` 夹住：窗口按
        // 列表尾部算，不越界。
        let plain: Vec<SelectionRow> = (0..3).map(|i| plain_row(format!("r{i}"), "")).collect();
        let mut state = SelectionState::with_max_visible(3, 8);
        state.selected = 9;
        assert_eq!(visible_window(&plain, &state, 8), 0..3);
        assert_eq!(visible_window(&plain, &state, 0), 0..0);
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
    fn test_row_height() {
        assert_eq!(plain_row("a", "b").height(), 1);
        let rich = SelectionRow {
            name: "id".into(),
            description: String::new(),
            rich: Some(RichSessionRow {
                status: SessionStatus::Working,
                last_active: String::new(),
                title: "t".into(),
                workspace: String::new(),
                pinned: false,
                tree_prefix: String::new(),
            }),
        };
        assert_eq!(rich.height(), 2);
    }

    /// Render a single rich row (selected, so the title path is exercised) and
    /// return its two lines as text.
    fn rich_lines(row: &SelectionRow, filter: &str, width: u16) -> (String, String) {
        let rows = std::slice::from_ref(row);
        let state = SelectionState::new(1);
        let area = Rect::new(0, 0, width, 2);
        let mut buf = Buffer::empty(area);
        SelectionPopup::new(rows, &state, filter, &ThemePalette::default()).render(area, &mut buf);
        let text = |y: u16| {
            (0..width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        (text(0), text(1))
    }

    /// Render several rich rows into one frame and return each one's line 1
    /// (跨行的列对齐要靠同一帧才看得见).
    fn line1s(rows: &[SelectionRow], width: u16) -> Vec<String> {
        let state = SelectionState::new(rows.len());
        let area = Rect::new(0, 0, width, (rows.len() * 2) as u16);
        let mut buf = Buffer::empty(area);
        SelectionPopup::new(rows, &state, "", &ThemePalette::default()).render(area, &mut buf);
        (0..rows.len())
            .map(|i| {
                let y = (i * 2) as u16;
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn session_row(status: SessionStatus, last_active: &str, title: &str) -> SelectionRow {
        session_row_at(status, last_active, title, "/ws/wing", false)
    }

    fn session_row_at(
        status: SessionStatus,
        last_active: &str,
        title: &str,
        workspace: &str,
        pinned: bool,
    ) -> SelectionRow {
        session_row_tree(status, last_active, title, workspace, pinned, "")
    }

    /// `tree_prefix` 非空 = 树里的子节点（行 1 前缀 + 行 2 缩进都跟着挪）。
    fn session_row_tree(
        status: SessionStatus,
        last_active: &str,
        title: &str,
        workspace: &str,
        pinned: bool,
        tree_prefix: &str,
    ) -> SelectionRow {
        SelectionRow {
            name: "sess-1".into(),
            description: String::new(),
            rich: Some(RichSessionRow {
                status,
                last_active: last_active.into(),
                title: title.into(),
                workspace: workspace.into(),
                pinned,
                tree_prefix: tree_prefix.into(),
            }),
        }
    }

    #[test]
    fn test_rich_row_line1_is_icon_workspace_and_time() {
        // line 1 = 图标 + workspace + 时间标签（workspace 的**展示**回归；
        // 它不参与排序，只和搜索有关）。根行（前缀为空）就是平滑退化面：
        // 无父子关系的列表渲染与树形之前**逐字符一致**。
        let row = session_row(SessionStatus::Working, "07-22 21:41", "my session");
        let (line1, line2) = rich_lines(&row, "", 40);
        assert_eq!(line1, " ● /ws/wing [07-22 21:41]");
        assert_eq!(line2.trim_start(), "my session");
        assert_eq!(
            line2.len() - line2.trim_start().len(),
            4,
            "根行缩进照旧 4 列"
        );
    }

    #[test]
    fn test_rich_row_pin_star_is_only_drawn_when_pinned() {
        let pinned = session_row_at(SessionStatus::Idle, "07-22 21:41", "t", "/ws", true);
        let (line1, _) = rich_lines(&pinned, "", 40);
        assert_eq!(line1, " ● /ws [07-22 21:41] ★");

        // 未 pin 不占位：同一行去掉星标后是另一条完整的行（宽度不抖动）。
        let plain = session_row_at(SessionStatus::Idle, "07-22 21:41", "t", "/ws", false);
        let (line1, _) = rich_lines(&plain, "", 40);
        assert_eq!(line1, " ● /ws [07-22 21:41]");
        assert!(!line1.contains('★'));
    }

    #[test]
    fn test_rich_row_without_timestamp_keeps_the_workspace() {
        // 时间不可解析（format_last_active 给空串）时不留悬空的空格/括号；
        // workspace 照常渲染。
        let row = session_row(SessionStatus::Inactive, "", "untitled work");
        let (line1, line2) = rich_lines(&row, "", 40);
        assert_eq!(line1, " ● /ws/wing");
        assert_eq!(line2.trim_start(), "untitled work");
    }

    #[test]
    fn test_rich_row_without_a_workspace_says_so() {
        // 老会话 / 无 workspace：显式占位，而不是一段空白（那看起来像渲染失败）。
        let row = session_row_at(SessionStatus::Idle, "07-22 21:41", "t", "", false);
        let (line1, _) = rich_lines(&row, "", 40);
        assert_eq!(line1, " ● (no workspace) [07-22 21:41]");

        // 放不下时占位符自己也走预算：它不该把时间标签顶出行外。
        let (line1, _) = rich_lines(&row, "", 30);
        assert_eq!(UnicodeWidthStr::width(line1.as_str()), 30, "{line1:?}");
        assert!(line1.contains("[07-22 21:41]"), "{line1:?}");
        assert!(line1.starts_with(" ● (no workspa"), "{line1:?}");
    }

    #[test]
    fn test_rich_row_truncates_the_workspace_from_the_left_before_time_and_star() {
        // 极窄终端：时间标签与星标先保，workspace 左截断（保留区分度最高的
        // 尾段——右截断留下的 `/a/very/…` 对每个会话都一样）；line 1 不得超宽。
        let row = session_row_at(
            SessionStatus::Idle,
            "07-22 21:41",
            "title",
            "/a/very/long/workspace/path",
            true,
        );
        let (line1, _) = rich_lines(&row, "", 24);
        assert_eq!(UnicodeWidthStr::width(line1.as_str()), 24, "{line1:?}");
        assert!(line1.ends_with("★"), "the pin star survives: {line1:?}");
        assert!(line1.contains("[07-22 21:41]"), "{line1:?}");
        assert!(
            line1.contains('…') && line1.contains("path"),
            "the workspace keeps its tail, got: {line1:?}"
        );
        assert!(
            !line1.contains("/a/very"),
            "the shared prefix is what gets dropped, got: {line1:?}"
        );
    }

    #[test]
    fn test_rich_row_draws_the_tree_prefix_before_the_icon_and_aligns_line2() {
        // 子节点：`├─ ` 在最左，图标随前缀右移；行 2 缩进 = 前缀宽 + 4。
        let row = session_row_tree(
            SessionStatus::Idle,
            "07-22 21:41",
            "a child session",
            "/ws",
            false,
            "├─ ",
        );
        let (line1, line2) = rich_lines(&row, "", 40);
        assert_eq!(line1, "├─  ● /ws [07-22 21:41]");
        assert_eq!(line2, "       a child session");

        // 孙节点：延续线 `│  ` 跟着前缀一起画（祖先是非末位兄弟）。
        let grand = session_row_tree(
            SessionStatus::Idle,
            "07-22 21:41",
            "grandchild",
            "/ws",
            false,
            "│  └─ ",
        );
        let (line1, line2) = rich_lines(&grand, "", 40);
        assert_eq!(line1, "│  └─  ● /ws [07-22 21:41]");
        assert_eq!(line2, "          grandchild");
    }

    #[test]
    fn test_rich_row_prefix_takes_its_columns_from_the_workspace_budget() {
        // 前缀参与行宽记账：窄终端上时间标签与星标照旧先保，workspace 让位；
        // 行 1 不超宽，行 2 的缩进不与行 1 错位。
        let row = session_row_tree(
            SessionStatus::Idle,
            "07-22 21:41",
            "title",
            "/a/very/long/workspace/path",
            true,
            "│  └─ ",
        );
        let (line1, line2) = rich_lines(&row, "", 30);
        assert_eq!(UnicodeWidthStr::width(line1.as_str()), 30, "{line1:?}");
        assert!(line1.starts_with("│  └─  ● "), "{line1:?}");
        assert!(line1.ends_with("★"), "the pin star survives: {line1:?}");
        assert!(line1.contains("[07-22 21:41]"), "{line1:?}");
        assert!(
            line1.contains('…') && line1.contains("path"),
            "the workspace keeps its tail, got: {line1:?}"
        );
        assert!(line2.starts_with("          title"), "{line2:?}");
    }

    #[test]
    fn test_rich_row_clips_a_deep_prefix_instead_of_losing_the_row() {
        // 深链（前缀吃到 16 层封顶 = 51 列）+ 窄面板：前缀自己是第一个让位的
        // 结构元素——它不能让图标 / 时间标签 / 星标跟着整行消失。
        let deep = format!("{}└─ ", "│  ".repeat(16));
        assert_eq!(UnicodeWidthStr::width(deep.as_str()), 51);
        let row = session_row_tree(
            SessionStatus::Idle,
            "07-22 21:41",
            "deep title",
            "/ws",
            true,
            &deep,
        );

        let (line1, line2) = rich_lines(&row, "", 40);
        assert_eq!(UnicodeWidthStr::width(line1.as_str()), 40, "{line1:?}");
        assert!(line1.starts_with('…'), "clipped head: {line1:?}");
        assert!(
            line1.contains("└─  ●"),
            "the branch marker survives: {line1:?}"
        );
        assert!(line1.contains("[07-22 21:41]"), "{line1:?}");
        assert!(line1.ends_with("★"), "the pin star survives: {line1:?}");
        // 行 2 跟着**裁剪后**的前缀走（缩进 = 21 + 4），标题仍有一席之地。
        assert_eq!(line2.trim_start(), "deep title", "{line2:?}");
        assert_eq!(line2.chars().take_while(|c| *c == ' ').count(), 25);
    }

    #[test]
    fn test_clipped_rows_keep_a_title_even_without_a_time_tag() {
        // 时间缺失（创建即打标窗口期的会话）时前缀与标题争的是同一段宽度：
        // 前缀的预算按行的最宽形态扣，标题因此始终留着（≥ 14 列）。
        let deep = format!("{}└─ ", "│  ".repeat(16));
        let row = session_row_tree(
            SessionStatus::Inactive,
            "",
            "deep title",
            "/ws",
            false,
            &deep,
        );

        let (line1, line2) = rich_lines(&row, "", 40);
        assert!(
            UnicodeWidthStr::width(line1.as_str()) <= 40,
            "line 1 must fit: {line1:?}"
        );
        assert!(line1.contains("└─  ●"), "{line1:?}");
        // 没有时间标签占位，省下的列归 workspace。
        assert!(line1.contains("/ws"), "{line1:?}");
        assert_eq!(line2.trim_start(), "deep title", "{line2:?}");
    }

    #[test]
    fn test_sibling_rows_clip_the_prefix_to_the_same_width() {
        // 同深度 = 同前缀长；裁剪宽度不随本行有没有星标变化，否则带星标的兄弟
        // 会整体左移，树的引导线在同一屏里对不上。
        let deep = format!("{}└─ ", "│  ".repeat(16));
        let rows = vec![
            session_row_tree(
                SessionStatus::Idle,
                "07-22 21:41",
                "pinned",
                "/ws",
                true,
                &deep,
            ),
            session_row_tree(
                SessionStatus::Idle,
                "07-22 21:41",
                "plain",
                "/ws",
                false,
                &deep,
            ),
        ];

        let lines = line1s(&rows, 40);
        let icon_col = |line: &str| line.chars().take_while(|c| *c != '●').count();
        assert!(lines.iter().all(|l| l.starts_with('…')), "{lines:?}");
        assert_eq!(
            icon_col(&lines[0]),
            icon_col(&lines[1]),
            "the branch column must not depend on the star: {lines:?}"
        );
    }

    /// 渲染一帧，返回 (缓冲区, 行文本)。
    fn render_frame_with(
        rows: &[SelectionRow],
        state: &SelectionState,
        area: Rect,
        palette: ThemePalette,
    ) -> (Buffer, Vec<String>, ThemePalette) {
        let mut buf = Buffer::empty(area);
        SelectionPopup::new(rows, state, "", &palette).render(area, &mut buf);
        let lines = (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (buf, lines, palette)
    }

    fn render_frame(
        rows: &[SelectionRow],
        state: &SelectionState,
        area: Rect,
    ) -> (Buffer, Vec<String>, ThemePalette) {
        render_frame_with(rows, state, area, ThemePalette::default())
    }

    /// 选中带只换背景：每行自己的前景色（命令名 accent、描述 dim/text）必须
    /// 原样保留。`Style::patch` 是 `other.fg.or(self.fg)`，把前景塞进 `base`
    /// 会在 `patch` 时盖掉 span 自己的颜色 —— 整个弹窗被压成单色（两套
    /// palette 下都发生，且截图里只表现为"变素了"，很难肉眼发现）。
    #[test]
    fn row_colours_survive_the_selected_band() {
        let rows = vec![plain_row("alpha", "first"), plain_row("beta", "second")];
        let state = SelectionState::new(rows.len()); // 行 0 选中
        let area = Rect::new(0, 0, 40, 2);

        let (buf, _, palette) = render_frame(&rows, &state, area);
        assert_eq!(buf[(1, 0)].fg, palette.accent, "选中行的命令名");
        assert_eq!(buf[(8, 0)].fg, palette.text, "选中行的描述");
        assert_eq!(buf[(1, 0)].bg, palette.surface, "选中行的底");
        assert_eq!(buf[(1, 1)].fg, palette.accent, "未选中行的命令名");
        assert_eq!(buf[(8, 1)].fg, palette.dim, "未选中行的描述");
        assert_eq!(buf[(1, 1)].bg, Color::Reset, "未选中行不刷底");
    }

    /// 选中带在另外两套调色板下的语义：`light` 用浅灰卡底（命令名 accent
    /// 不被抹平）；`terminal` 把背景交给终端（`surface == Reset`）时回退到
    /// `dim`，否则选中行与未选中行完全一样。
    #[test]
    fn the_selected_band_survives_light_and_terminal_palettes() {
        let rows = vec![plain_row("alpha", "first")];
        let state = SelectionState::new(rows.len());
        let area = Rect::new(0, 0, 40, 1);
        let palette_for = |preset| {
            ThemePalette::from_config_with_appearance(
                &crate::config::ColorsConfig {
                    preset,
                    ..Default::default()
                },
                None,
            )
        };

        let light = palette_for(crate::config::ColorPreset::Light);
        let (buf, _, _) = render_frame_with(&rows, &state, area, light);
        assert_eq!(buf[(1, 0)].fg, light.accent);
        assert_eq!(buf[(1, 0)].bg, light.surface);

        let terminal = palette_for(crate::config::ColorPreset::Terminal);
        let (buf, _, _) = render_frame_with(&rows, &state, area, terminal);
        assert_eq!(buf[(1, 0)].bg, terminal.dim, "terminal: 回退到 dim 带");
        assert_ne!(buf[(1, 0)].bg, Color::Reset);
    }

    /// 一帧里**被选中**（背景 = palette.dim）的那些行文本。
    fn highlighted_rows(rows: &[SelectionRow], state: &SelectionState, area: Rect) -> Vec<String> {
        let (buf, lines, palette) = render_frame(rows, state, area);
        (area.y..area.bottom())
            .filter(|&y| (area.x..area.right()).any(|x| buf[(x, y)].bg == palette.surface))
            .map(|y| lines[(y - area.y) as usize].clone())
            .collect()
    }

    #[test]
    fn test_a_rect_shorter_than_the_window_still_draws_the_selected_row() {
        // 12 条 session（策略窗口 6 条 = 12 行），矩形只给 7 行 = 3 条 —— 开屏里
        // 弹层挂在输入卡下面就是这个形状。滚到底时高亮的那一条必须在矩形里。
        let rows = rich_rows(12);
        let mut state = SelectionState::with_max_visible(rows.len(), MAX_VISIBLE_RICH_ROWS);
        for _ in 0..rows.len() - 1 {
            state.move_down();
        }
        let area = Rect::new(0, 0, 40, 7);
        let highlighted = highlighted_rows(&rows, &state, area);
        assert_eq!(highlighted.len(), 2, "选中项的两行都在：{highlighted:?}");
        assert!(
            highlighted[0].contains("/ws/wing") && highlighted[1].contains("title-11"),
            "高亮的是选中的最后一条：{highlighted:?}"
        );
        // 画出来的正好是尾部三条（不是开头三条）：滚到底看得见的是列表末尾。
        let (_, lines, _) = render_frame(&rows, &state, area);
        let text = lines.join("\n");
        assert!(
            text.contains("title-09") && text.contains("title-10"),
            "{text}"
        );
        assert!(!text.contains("title-00"), "开头的行已经不在了：{text}");
    }

    #[test]
    fn test_a_rect_one_row_short_still_draws_the_last_plain_item() {
        // 单行窗口 8 条、矩形 7 行（开屏里常见）：最后一条也要画得出来。
        let rows: Vec<SelectionRow> = (0..10)
            .map(|i| plain_row(format!("cmd-{i:02}"), "desc"))
            .collect();
        let mut state = SelectionState::new(rows.len());
        for _ in 0..rows.len() - 1 {
            state.move_down();
        }
        let area = Rect::new(0, 0, 40, 7);
        let highlighted = highlighted_rows(&rows, &state, area);
        assert_eq!(highlighted.len(), 1, "{highlighted:?}");
        assert!(
            highlighted[0].contains("cmd-09"),
            "最后一条被高亮：{highlighted:?}"
        );
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

    #[test]
    fn test_format_last_active_survives_non_ascii_input() {
        // `last_interaction` 是后端事实（可被手改 metadata / 第三方网关写脏），
        // 面板的每一行都要过这里：非 ASCII 字符横跨 14..16（结构检查唯一没盖住
        // 的边界）时必须降级为空串，而不是让面板把 TUI 带走。
        assert_eq!(format_last_active("2025-01-01T00:0中"), "");
        assert_eq!(format_last_active("2025-01-01T00:0🎉"), "");
        assert_eq!(format_last_active("2025-01-01T00:0中:00"), "");
        // 结构检查挡下的多字节（分隔符位置不是 ASCII）照旧是空串。
        assert_eq!(format_last_active("20中5-01-01T00:00:00"), "");
        // 边界外的正常值不受影响。
        assert_eq!(format_last_active("2025-01-01T00:00:00"), "01-01 00:00");
    }
}
