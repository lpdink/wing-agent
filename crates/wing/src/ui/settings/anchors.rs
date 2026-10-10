//! 左栏：业务分组锚点（v2 的导航列）。
//!
//! ```text
//! ❯ Providers      ← 左栏焦点下选中（accent + bold；焦点在右栏时是 `▏`）
//!   Agents
//!   Behavior      ● ← 组内有未保存的改动
//!   Images
//!   Sessions    2  ← 问题数（danger）
//!   Gateway
//!   Advanced      3 ← 搜索命中数（仅搜索态）
//!   Interface
//! ```
//!
//! 锚点的**内容**全部来自 07 的 [`AnchorView`]（后端 `groups[]` + Interface 的本地声明），
//! 这里只负责排版：一行一个锚点，光标在左栏时画 `❯`，右端画徽标。
//! 徽标的优先级：搜索命中数 > 问题数 > 脏标记（搜索态下命中数最有用；问题比脏更要紧）。

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::shared::panels::settings::AnchorView;
use crate::shared::panels::settings::Focus;
use crate::shared::panels::settings::SettingsPanel;
use crate::ui::panel::cursor_span;
use crate::ui::panel::label_style;

use super::text::clamp_line;
use super::text::display_width;

/// 左栏的行（窗口已按 [`SettingsPanel::anchors_visible_range`] 选好）。
pub(crate) fn anchor_lines(
    panel: &SettingsPanel,
    palette: &ThemePalette,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    if height == 0 || width == 0 {
        return Vec::new();
    }
    let views = panel.anchor_views();
    if views.is_empty() {
        return Vec::new();
    }
    let focused = panel.focus() == Focus::Groups;
    let range = panel.anchors_visible_range(height);
    range
        .filter_map(|index| views.get(index))
        .map(|view| anchor_line(view, focused, palette, width))
        .take(height)
        .collect()
}

/// 一个锚点行：`[光标 2][标题][填充][徽标]`。
fn anchor_line(
    view: &AnchorView,
    focused: bool,
    palette: &ThemePalette,
    width: usize,
) -> Line<'static> {
    let badge = badge_spans(view, palette);
    let badge_width: usize = badge
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();

    let mut spans = vec![rail(view.selected, focused, palette)];
    // 标题的可用宽度 = 整行 - 光标列 - 徽标 - 一个空格间隔。
    let budget = width.saturating_sub(2 + badge_width + usize::from(badge_width > 0));
    let title = view.title.as_str();
    // 三档寄存器：左栏焦点 + 选中 = accent bold；右栏焦点 + 选中 = 正文色（"你在这里"
    // 但不抢光标）；其余 = dim。
    let title_style = if view.selected {
        if focused {
            label_style(true, palette)
        } else {
            Style::default().fg(palette.text)
        }
    } else {
        Style::default().fg(palette.dim)
    };
    spans.push(Span::styled(shorten(title, budget), title_style));
    if badge_width > 0 {
        let used = 2 + display_width(&spans[1].content);
        if used < width.saturating_sub(badge_width) {
            spans.push(Span::raw(
                " ".repeat(width.saturating_sub(badge_width) - used),
            ));
        } else {
            spans.push(Span::raw(" "));
        }
        spans.extend(badge);
    }
    clamp_line(Line::from(spans), width)
}

/// 左端两格：左栏焦点 + 选中 = `❯`（与全应用同一个光标寄存器）；
/// 右栏焦点 + 选中 = `▏`（"你在这组里"，但不抢右栏的光标）；其余留白。
fn rail(selected: bool, focused: bool, palette: &ThemePalette) -> Span<'static> {
    if selected && focused {
        return cursor_span(true, palette);
    }
    if selected {
        return Span::styled("▏ ".to_string(), Style::default().fg(palette.dim));
    }
    Span::raw("  ")
}

/// 右端徽标（优先级见模块文档）。
fn badge_spans(view: &AnchorView, palette: &ThemePalette) -> Vec<Span<'static>> {
    if let Some(hits) = view.hits {
        let style = if hits > 0 {
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.dim)
        };
        return vec![Span::styled(hits.to_string(), style)];
    }
    if view.problems > 0 {
        return vec![Span::styled(
            view.problems.to_string(),
            Style::default()
                .fg(palette.danger)
                .add_modifier(Modifier::BOLD),
        )];
    }
    if view.dirty {
        return vec![Span::styled(
            "●".to_string(),
            Style::default().fg(palette.success),
        )];
    }
    Vec::new()
}

/// 标题裁剪（放得下就原样；放不下按显示宽度截断 + `…`）。
fn shorten(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if display_width(text) <= budget {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::Root;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    fn view(title: &str, selected: bool) -> AnchorView {
        AnchorView {
            title: title.to_string(),
            root: Root::Gateway,
            doc: String::new(),
            selected,
            dirty: false,
            problems: 0,
            hits: None,
        }
    }

    #[test]
    fn the_selected_anchor_gets_the_cursor_glyph() {
        let line = anchor_line(&view("Providers", true), true, &palette(), 20);
        assert_eq!(line.to_string(), "❯ Providers");
        let plain = anchor_line(&view("Agents", false), true, &palette(), 20);
        assert_eq!(plain.to_string(), "  Agents");
    }

    #[test]
    fn the_rail_tells_the_two_columns_apart() {
        // 左栏焦点：`❯`（应用统一的光标寄存器）。
        let focused = anchor_line(&view("Providers", true), true, &palette(), 20);
        assert_eq!(focused.to_string(), "❯ Providers");
        // 右栏焦点：`▏`（还在这组里，但不抢右栏的光标）。
        let unfocused = anchor_line(&view("Providers", true), false, &palette(), 20);
        assert_eq!(unfocused.to_string(), "▏ Providers");
        // 未选中：留白（对齐不变）。
        assert_eq!(
            anchor_line(&view("Agents", false), false, &palette(), 20).to_string(),
            "  Agents"
        );
    }

    #[test]
    fn badges_are_right_aligned_and_prioritised() {
        let mut dirty = view("Behavior", false);
        dirty.dirty = true;
        assert_eq!(
            anchor_line(&dirty, true, &palette(), 16).to_string(),
            "  Behavior     ●"
        );
        let mut problems = dirty.clone();
        problems.problems = 2;
        assert_eq!(
            anchor_line(&problems, true, &palette(), 16).to_string(),
            "  Behavior     2",
            "问题压过脏标记"
        );
        let mut hits = problems.clone();
        hits.hits = Some(3);
        assert_eq!(
            anchor_line(&hits, true, &palette(), 16).to_string(),
            "  Behavior     3",
            "搜索态只显示命中数"
        );
        let mut zero = view("Images", false);
        zero.hits = Some(0);
        assert_eq!(
            anchor_line(&zero, true, &palette(), 16).to_string(),
            "  Images       0",
            "零命中也要看得见（否则像是没搜）"
        );
    }

    #[test]
    fn long_titles_are_truncated_not_wrapped() {
        let line = anchor_line(&view("一个很长很长的分组名字", false), true, &palette(), 10);
        let text = line.to_string();
        assert!(text.ends_with('…'), "{text:?}");
        assert!(display_width(&text) <= 10, "不溢出：{text:?}");
        assert!(text.contains("一个很"), "头部保留（CJK 不撕半格）");
    }

    #[test]
    fn a_narrow_column_never_overflows() {
        let mut dirty = view("Providers", true);
        dirty.problems = 12;
        for width in 0..12usize {
            let line = anchor_line(&dirty, true, &palette(), width);
            assert!(
                display_width(&line.to_string()) <= width,
                "width {width}: {line:?}"
            );
        }
    }

    #[test]
    fn shorten_handles_cjk_and_empty_budgets() {
        assert_eq!(shorten("abc", 0), "");
        assert_eq!(shorten("abc", 3), "abc");
        assert_eq!(shorten("abcd", 3), "ab…");
        assert_eq!(shorten("中文名字", 4), "中…");
    }
}
