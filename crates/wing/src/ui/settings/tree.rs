//! Settings row rendering for the single-column list.

use std::ops::Range;

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::config::colors::parse_color;
use crate::shared::panels::settings::Row;
use crate::shared::panels::settings::RowAction;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::ValueText;
use crate::ui::panel::cursor_span;
use crate::ui::panel::label_style;

use super::text::clamp_line;

pub(crate) fn tree_lines(
    panel: &SettingsPanel,
    palette: &ThemePalette,
    width: usize,
    height: usize,
    description: Option<&str>,
) -> Vec<Line<'static>> {
    let rows = panel.rows();
    if rows.is_empty() {
        return vec![Line::from(Span::styled(
            if panel.search_query().is_some() {
                "没有匹配的设置"
            } else {
                "没有设置项"
            },
            Style::default().fg(palette.dim),
        ))];
    }
    let visible = height.saturating_sub(usize::from(description.is_some()));
    let mut lines = Vec::new();
    for index in panel.visible_range(visible) {
        let row = &rows[index];
        let selected = index == panel.cursor();
        let color = panel
            .row_node(row)
            .and_then(|node| node.value_hint.as_deref())
            == Some("color");
        lines.push(row_line(
            row,
            selected,
            is_expanded(rows, index),
            color,
            width,
            palette,
        ));
        if selected && let Some(doc) = description {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    super::text::single_row(doc, width.saturating_sub(2))
                ),
                Style::default().fg(palette.dim),
            )));
        }
    }
    lines
}

/// 结构行是否展开：`Row` 没有 `expanded` 字段，
/// 用「下一行的 depth 更大」推断 —— 展开的子树一定紧跟自己的孩子。
fn is_expanded(rows: &[Row], index: usize) -> bool {
    rows.get(index + 1)
        .is_some_and(|next| next.depth > rows[index].depth)
}

/// 行前缀：光标 / 缩进 / 脏标记 / 展开箭头（选择项行是内项光标）。
fn prefix(
    row: &Row,
    is_cursor: bool,
    expanded: bool,
    palette: &ThemePalette,
) -> Vec<Span<'static>> {
    let mut spans = vec![cursor_span(is_cursor, palette)];
    if row.depth > 0 {
        spans.push(Span::raw("  ".repeat(row.depth.min(4))));
    }
    if let RowAction::Choose { selected, .. } = &row.action {
        spans.push(choice_marker(*selected, palette));
        return spans;
    }
    spans.push(dirty_marker(row, palette));
    spans.push(arrow(row, expanded, palette));
    spans
}

/// 脏标记 `●`（success —— 与 `ui/cells/model_picker.rs` 的 `●` 同色）；不脏时留白对齐。
fn dirty_marker(row: &Row, palette: &ThemePalette) -> Span<'static> {
    if row.markers.dirty {
        Span::styled("● ".to_string(), Style::default().fg(palette.success))
    } else {
        Span::raw("  ")
    }
}

/// 展开箭头：结构行 `▾` / `▸`，其余留白（根头行不画箭头）。
fn arrow(row: &Row, expanded: bool, palette: &ThemePalette) -> Span<'static> {
    if row.action != RowAction::Expand || row.path.is_empty() {
        return Span::raw("  ");
    }
    let glyph = if expanded { "▾ " } else { "▸ " };
    Span::styled(glyph.to_string(), Style::default().fg(palette.dim))
}

/// enum 选择项行的内项光标。
fn choice_marker(selected: bool, palette: &ThemePalette) -> Span<'static> {
    if selected {
        Span::styled(
            "❯ ".to_string(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    }
}

fn row_line(
    row: &Row,
    selected: bool,
    expanded: bool,
    color: bool,
    width: usize,
    palette: &ThemePalette,
) -> Line<'static> {
    let mut spans = prefix(row, selected, expanded, palette);
    if let RowAction::Choose {
        value: Some(value), ..
    } = &row.action
    {
        spans.extend(color_swatch(value, color));
    }
    let mut values = value_spans(row, color, palette);
    let mut label = label_spans(
        &row.label,
        &row.match_spans,
        row_label_style(row, selected, palette),
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    );
    let label_width = label.iter().map(span_width).sum::<usize>();
    let marker_width = usize::from(row.markers.problem) * 2;
    let available =
        width.saturating_sub(spans.iter().map(span_width).sum::<usize>() + marker_width);
    if !values.is_empty() && available >= 4 {
        let value_width = values.iter().map(span_width).sum::<usize>();
        let label_budget =
            label_width.min(available.saturating_sub(1 + value_width.min(available / 2)));
        if label_budget < label_width {
            label = clamp_line(Line::from(label), label_budget.saturating_sub(1)).spans;
            label.push(Span::styled("…", row_label_style(row, selected, palette)));
        }
        let used = label.iter().map(span_width).sum::<usize>();
        spans.extend(label);
        spans.push(Span::raw(" "));
        let budget = available.saturating_sub(used + 1);
        if budget < value_width && budget > 0 {
            values = clamp_line(Line::from(values), budget - 1).spans;
            values.push(Span::styled("…", Style::default().fg(palette.dim)));
        }
        spans.extend(values);
    } else {
        spans.extend(clamp_line(Line::from(label), available).spans);
    }
    if row.markers.problem {
        spans.push(Span::styled(" !", Style::default().fg(palette.danger)));
    }
    clamp_line(Line::from(spans), width)
}

fn span_width(span: &Span<'_>) -> usize {
    UnicodeWidthStr::width(span.content.as_ref())
}

/// label 的底色：光标行 accent + bold（借 `ui/panel.rs` 的寄存器）、只读 / 新增行 dim。
fn row_label_style(row: &Row, is_cursor: bool, palette: &ThemePalette) -> Style {
    if is_cursor {
        return label_style(true, palette);
    }
    match row.action {
        RowAction::ReadOnly | RowAction::AddItem { .. } => Style::default().fg(palette.dim),
        _ => Style::default().fg(palette.text),
    }
}

/// label 切成「普通 / 命中」两类 span（命中区间来自 07，按字节；越界或乱序一律跳过）。
fn label_spans(label: &str, spans: &[Range<usize>], base: Style, hit: Style) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    for range in spans {
        let start = range.start.min(label.len());
        let end = range.end.min(label.len());
        if start < cursor
            || end <= start
            || !label.is_char_boundary(start)
            || !label.is_char_boundary(end)
        {
            continue;
        }
        if cursor < start {
            out.push(Span::styled(label[cursor..start].to_string(), base));
        }
        out.push(Span::styled(label[start..end].to_string(), hit));
        cursor = end;
    }
    if cursor < label.len() {
        out.push(Span::styled(label[cursor..].to_string(), base));
    }
    out
}

/// 值列的渲染（`ValueText` 的六种形态 + 密文掩码 + 色块预览）。
fn value_spans(row: &Row, color_hint: bool, palette: &ThemePalette) -> Vec<Span<'static>> {
    let dim = Style::default().fg(palette.dim);
    match &row.value {
        ValueText::None => Vec::new(),
        ValueText::Masked { hint } => {
            let mut spans = vec![Span::styled("••••••••".to_string(), dim)];
            if let Some(hint) = hint {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(hint.clone(), dim));
            }
            spans
        }
        ValueText::Inherited => vec![Span::styled("(unchanged)".to_string(), dim)],
        ValueText::Default(text) => {
            let mut spans = color_swatch(text, color_hint);
            spans.push(Span::styled(text.clone(), dim));
            spans.push(Span::styled(" (default)".to_string(), dim));
            spans
        }
        ValueText::Text(text) => {
            let required_missing = row.markers.required && text == "(required)";
            let style = if required_missing {
                Style::default().fg(palette.danger)
            } else {
                Style::default().fg(palette.text)
            };
            let mut spans = color_swatch(text, color_hint);
            spans.push(Span::styled(text.clone(), style));
            spans
        }
    }
}

/// 色块预览：值可解析成颜色时画一个该色的 `■`。
///
/// 解析失败、`reset`（画出来是透明格，等于没画）一律不画 —— 宁可不画，不可画错。
fn color_swatch(text: &str, color_hint: bool) -> Vec<Span<'static>> {
    if !color_hint {
        return Vec::new();
    }
    match parse_color(text) {
        Some(color) if color != ratatui::style::Color::Reset => {
            vec![Span::styled("■ ".to_string(), Style::default().fg(color))]
        }
        _ => Vec::new(),
    }
}
