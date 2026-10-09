//! 树行渲染：把 07 的 [`Row`] 画成一行。
//!
//! 行的形状（每段都可能缺席）：
//!
//! ```text
//! [cursor 2][indent 2*depth][dirty 2][arrow 2][■ 色块][label][pad → 值列][value][ !][ apply 短标记]
//! ```
//!
//! 三条来自 07 的约定（`shared/panels/settings/tree.rs` 的模块文档）在这里落地：
//!
//! 1. **列表项行的主文本在 `label`**（标量项 = 值本身；对象项 = `name · protocol · …`），
//!    `value` 恒为 `ValueText::None` —— 所以值列只在 `value` 非 `None` 时画；
//! 2. 合成行（`(+ 新增一项)` 的 `<列表>[]`、enum 选择项的 `<enum>=<value>`）走同一条渲染路径，
//!    选择项行的 `❯` 是**内项光标**（`RowAction::Choose { selected }`），不是主光标；
//! 3. 搜索命中在 `Row.match_spans`（label 的字节区间）—— 切成 underline + accent 的 span，
//!    与 `ui/popup/selection.rs` 的高亮寄存器一致。
//!
//! 值列对齐用**全部行**（不只可见行）算一次，于是滚动不会让值列抖动；
//! 上限是区域宽的 60%，深缩进的子树不会把值挤出屏幕。

use std::ops::Range;

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;
use wing_api_client::models::ApplyScope;

use crate::config::ThemePalette;
use crate::config::colors::parse_color;
use crate::shared::panels::settings::EditState;
use crate::shared::panels::settings::Row;
use crate::shared::panels::settings::RowAction;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::ValueText;
use crate::ui::panel::cursor_span;
use crate::ui::panel::label_style;

use super::SettingsCatalogs;
use super::editor;
use super::text::clamp_line;

/// 值列上限：区域宽的 60%（剩下的留给值本身）。
const VALUE_COLUMN_RATIO: usize = 6;

/// 整棵树的行列表（窗口已经按 07 的 `visible_range` 选好）。
///
/// `height` 是主体可用行数；编辑器带错误时少要一行给 `↳ <error>`。
pub(crate) fn tree_lines(
    panel: &SettingsPanel,
    catalogs: &SettingsCatalogs<'_>,
    palette: &ThemePalette,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let rows = panel.rows();
    if rows.is_empty() {
        // 空目录 / 空搜索：一行 dim 占位，而不是一片空白（那看起来像渲染失败）。
        return vec![Line::from(Span::styled(
            "(空目录)".to_string(),
            Style::default().fg(palette.dim),
        ))];
    }

    let value_col = value_column(rows, catalogs, width);
    let edit = panel.edit_state();
    // 错误行要占一行：窗口少要一行，于是「树行 + 错误行」正好填满主体。
    let reserve = usize::from(edit.is_some_and(|state| state.error().is_some()));
    let visible = height.saturating_sub(reserve).max(1);
    let range = panel.visible_range(visible);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut editor_drawn = false;
    for index in range {
        let Some(row) = rows.get(index) else {
            continue;
        };
        let is_cursor = index == panel.cursor();
        let editing = edit.filter(|state| state.root() == row.root && state.path() == row.path);
        if editing.is_some() {
            editor_drawn = true;
        }
        lines.push(row_line(
            row,
            is_cursor,
            is_expanded(rows, index),
            catalogs,
            editing,
            value_col,
            width,
            palette,
        ));
        if let Some(state) = editing
            && let Some(error) = state.error()
        {
            lines.push(editor::error_line(error, value_col, width, palette));
        }
    }
    // 防御：编辑器开着但它的行不在窗口里（07 保证不会，但渲染层不能因此丢掉编辑器）。
    if let Some(state) = edit
        && !editor_drawn
    {
        lines.push(orphan_editor_line(state, width, palette));
    }
    lines.truncate(height);
    lines
}

/// 结构行是否展开：07 的 `Row` 没有 `expanded` 字段（见 design.md A2），
/// 用「下一行的 depth 更大」推断 —— 展开的子树一定紧跟自己的孩子。
fn is_expanded(rows: &[Row], index: usize) -> bool {
    rows.get(index + 1)
        .is_some_and(|next| next.depth > rows[index].depth)
}

/// 值列（0 基列号）：全部行的 `[前缀 + label]` 最大宽度 + 2，上限区域宽的 60%。
fn value_column(rows: &[Row], catalogs: &SettingsCatalogs<'_>, width: usize) -> usize {
    let widest = rows
        .iter()
        .map(|row| prefix_width(row, row_hint(catalogs, row)) + label_width(row))
        .max()
        .unwrap_or(0);
    let cap = (width * VALUE_COLUMN_RATIO / 10).max(6);
    (widest + 2).min(cap).min(width)
}

/// 这一行的 label 显示宽度。
fn label_width(row: &Row) -> usize {
    UnicodeWidthStr::width(row.label.as_str())
}

/// 这一行有没有前置于 label 的色块（enum 选择项行）。
fn label_swatch(row: &Row, hit: bool) -> bool {
    hit && matches!(&row.action, RowAction::Choose { value: Some(_), .. })
}

/// 这一行是否命中 `value_hint == "color"`（07 的字段在目录节点上，不在 `Row` 上）。
fn row_hint(catalogs: &SettingsCatalogs<'_>, row: &Row) -> bool {
    !row.path.is_empty() && catalogs.hint_for(row.root, &row.path)
}

/// 前缀列数（与 [`prefix`] 同源，值列对齐要靠它）。
fn prefix_width(row: &Row, hit: bool) -> usize {
    2 + row.depth * 2
        + if matches!(row.action, RowAction::Choose { .. }) {
            2
        } else {
            4
        }
        + if label_swatch(row, hit) { 2 } else { 0 }
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
        spans.push(Span::raw("  ".repeat(row.depth)));
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

/// 一行：前缀 + label（搜索高亮）+ 值列（编辑器 / 值 / 标记）。
#[allow(clippy::too_many_arguments)]
fn row_line(
    row: &Row,
    is_cursor: bool,
    expanded: bool,
    catalogs: &SettingsCatalogs<'_>,
    editing: Option<&EditState>,
    value_col: usize,
    width: usize,
    palette: &ThemePalette,
) -> Line<'static> {
    let hit = row_hint(catalogs, row);
    let mut spans = prefix(row, is_cursor, expanded, palette);
    let mut prefix_w = prefix_width(row, hit);
    if label_swatch(row, hit)
        && let RowAction::Choose {
            value: Some(value), ..
        } = &row.action
    {
        let swatch = color_swatch(value, hit);
        prefix_w += swatch.iter().map(span_width).sum::<usize>();
        spans.extend(swatch);
    }
    let base_style = row_label_style(row, is_cursor, palette);
    let hit_style = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    spans.extend(label_spans(
        &row.label,
        &row.match_spans,
        base_style,
        hit_style,
    ));
    let label_end = prefix_w + label_width(row);

    match editing {
        Some(state) => {
            pad_to(&mut spans, label_end, value_col);
            spans.extend(editor::value_spans(state, palette));
        }
        None if !matches!(row.value, ValueText::None) => {
            pad_to(&mut spans, label_end, value_col);
            spans.extend(value_spans(row, hit, palette));
        }
        None => {}
    }
    spans.extend(marker_spans(row, palette));
    clamp_line(Line::from(spans), width)
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

/// 色块预览（design §20 D25 / protocol_addendum P1）：值可解析成颜色时画一个该色的 `■`。
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

/// 行尾标记：problem `!`（danger）、apply 短标记（dim）、只读提示（dim）。
fn marker_spans(row: &Row, palette: &ThemePalette) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if row.markers.problem {
        spans.push(Span::styled(
            " !".to_string(),
            Style::default()
                .fg(palette.danger)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(tag) = apply_tag(&row.markers.apply) {
        spans.push(Span::styled(
            format!(" {tag}"),
            Style::default().fg(palette.dim),
        ));
    }
    if row.action == RowAction::ReadOnly {
        spans.push(Span::styled(
            " (只读)".to_string(),
            Style::default().fg(palette.dim),
        ));
    }
    spans
}

/// 生效域短标记：`hot` 不显示（最常见的值不该占列），其余取短词。
pub(crate) fn apply_tag(apply: &ApplyScope) -> Option<String> {
    match apply {
        ApplyScope::Hot => None,
        ApplyScope::NextSession => Some("session".to_string()),
        ApplyScope::Restart => Some("restart".to_string()),
        ApplyScope::Readonly => Some("readonly".to_string()),
        ApplyScope::Unknown(raw) => Some(raw.clone()),
    }
}

/// 一个 span 的显示宽度。
fn span_width(span: &Span<'_>) -> usize {
    UnicodeWidthStr::width(span.content.as_ref())
}

/// 补空格到 `target` 列；已经越过就只留一个空格（值紧跟其后，不硬挤）。
fn pad_to(spans: &mut Vec<Span<'static>>, used: usize, target: usize) {
    if used >= target {
        spans.push(Span::raw(" "));
        return;
    }
    spans.push(Span::raw(" ".repeat(target - used)));
}

/// 编辑器行不在窗口里时的兜底行（路径当 label，后跟 `[缓冲▏]`）。
fn orphan_editor_line(state: &EditState, width: usize, palette: &ThemePalette) -> Line<'static> {
    let mut spans = vec![
        cursor_span(false, palette),
        Span::styled(state.path().to_string(), Style::default().fg(palette.text)),
        Span::raw("  "),
    ];
    spans.extend(editor::value_spans(state, palette));
    clamp_line(Line::from(spans), width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::panels::settings::Root;
    use crate::shared::panels::settings::ScalarKind;
    use crate::ui::settings::test_support as fx;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    /// 只画一行（值 / 标记类断言不必拖上整棵树）。
    fn one(row: &Row, width: usize) -> Line<'static> {
        let catalog = fx::sample_catalog();
        let catalogs = SettingsCatalogs::new(&catalog, None);
        let col = value_column(std::slice::from_ref(row), &catalogs, width);
        row_line(row, false, false, &catalogs, None, col, width, &palette())
    }

    fn one_text(row: &Row, width: usize) -> String {
        one(row, width).to_string()
    }

    /// 一个「像树里那种」的行：depth 1、str 编辑器。
    fn sample_row(path: &str, label: &str, value: ValueText) -> Row {
        Row {
            root: Root::Gateway,
            path: path.to_string(),
            depth: 1,
            label: label.to_string(),
            value,
            markers: fx::markers(),
            action: RowAction::Edit(ScalarKind::Str),
            match_spans: Vec::new(),
            tpath: path.to_string(),
        }
    }

    #[test]
    fn value_column_aligns_across_rows_without_scrolling_jitter() {
        let rows = vec![
            sample_row("a", "protocol", ValueText::Text("openai".into())),
            sample_row("b", "timeout_first_chunk", ValueText::Text("300.0".into())),
        ];
        let catalog = fx::sample_catalog();
        let catalogs = SettingsCatalogs::new(&catalog, None);
        let col = value_column(&rows, &catalogs, 80);
        // 同一个值列渲染两行（`one()` 只画一行，会各自算列宽）。
        let first =
            row_line(&rows[0], false, false, &catalogs, None, col, 80, &palette()).to_string();
        let second =
            row_line(&rows[1], false, false, &catalogs, None, col, 80, &palette()).to_string();
        assert_eq!(
            first.find("openai").unwrap(),
            second.find("300.0").unwrap(),
            "值列对齐：{first:?} / {second:?}"
        );
        assert_eq!(first.find("openai").unwrap(), col);
    }

    #[test]
    fn value_column_is_capped_at_sixty_percent() {
        let rows: Vec<Row> = (0..3)
            .map(|i| {
                sample_row(
                    &format!("deep{i}"),
                    &"很长很长很长很长的标签".repeat(3),
                    ValueText::Text("v".into()),
                )
            })
            .collect();
        let catalog = fx::sample_catalog();
        let catalogs = SettingsCatalogs::new(&catalog, None);
        let width = 100usize;
        let col = value_column(&rows, &catalogs, width);
        assert!(col <= width * 6 / 10, "cap: {col}");
    }

    #[test]
    fn value_texts_use_their_own_registers() {
        let masked = sample_row(
            "k",
            "api_key",
            ValueText::Masked {
                hint: Some("ab12".into()),
            },
        );
        let out = one_text(&masked, 60);
        assert!(out.contains("•••••••• ab12"), "{out:?}");
        assert!(
            !out.contains("sk-") && !out.contains("secret"),
            "密文值只有掩码：{out:?}"
        );

        let default = sample_row("p", "port", ValueText::Default("32523".into()));
        let out = one_text(&default, 60);
        assert!(out.ends_with("32523 (default)"), "{out:?}");

        let inherited = sample_row("k", "api_key", ValueText::Inherited);
        assert!(one_text(&inherited, 60).ends_with("(unchanged)"));

        let none = sample_row("s", "providers (1)", ValueText::None);
        let out = one_text(&none, 60);
        assert!(
            out.trim_end().ends_with("providers (1)"),
            "值列不画：{out:?}"
        );
    }

    #[test]
    fn required_missing_value_is_danger_and_problem_marker_is_drawn() {
        let mut row = sample_row("api_key", "api_key", ValueText::Text("(required)".into()));
        row.markers.required = true;
        row.markers.problem = true;
        let line = one(&row, 80);
        let text = line.to_string();
        assert!(text.contains("(required)"), "{text:?}");
        assert!(
            text.trim_end().ends_with('!'),
            "problem 标记在行尾：{text:?}"
        );
        let required_span = line
            .spans
            .iter()
            .find(|span| span.content.contains("(required)"))
            .expect("(required) span");
        assert_eq!(required_span.style.fg, Some(palette().danger));
    }

    #[test]
    fn apply_tags_hide_hot_and_use_short_words() {
        assert_eq!(apply_tag(&ApplyScope::Hot), None);
        assert_eq!(
            apply_tag(&ApplyScope::NextSession).as_deref(),
            Some("session")
        );
        assert_eq!(apply_tag(&ApplyScope::Restart).as_deref(), Some("restart"));
        assert_eq!(
            apply_tag(&ApplyScope::Unknown("weird".into())).as_deref(),
            Some("weird")
        );
    }

    #[test]
    fn search_hits_are_accent_underlined_and_never_break_cjk() {
        let mut row = sample_row("gateway.port", "gateway.port 端口", ValueText::None);
        let start = row.label.find("port").unwrap();
        row.match_spans = std::iter::once(start..start + 4).collect();
        let line = one(&row, 80);
        let hit = line
            .spans
            .iter()
            .find(|span| span.content == "port")
            .expect("命中片段切成独立 span");
        assert!(hit.style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(line.to_string(), "        gateway.port 端口");
    }

    #[test]
    fn invalid_match_spans_are_skipped_without_panicking() {
        let mut row = sample_row("p", "端口名", ValueText::None);
        row.match_spans = vec![0..1, 3..3, 9..99, 2..4];
        let out = one_text(&row, 40);
        assert!(out.contains("端口名"), "{out:?}");
    }

    #[test]
    fn choice_rows_mark_the_inner_cursor() {
        let mut selected = sample_row("p=openai", "openai", ValueText::Text("OpenAI 兼容".into()));
        selected.action = RowAction::Choose {
            value: Some("openai".into()),
            selected: true,
        };
        let out = one_text(&selected, 60);
        assert!(out.trim_start().starts_with("❯ openai"), "{out:?}");

        let mut plain = selected.clone();
        plain.action = RowAction::Choose {
            value: Some("anthropic".into()),
            selected: false,
        };
        let out = one_text(&plain, 60);
        assert!(!out.contains('❯'), "未选中的选择项不画光标：{out:?}");
        assert!(out.contains("openai"), "{out:?}");
    }

    #[test]
    fn color_swatch_marks_a_color_value_and_falls_back_silently() {
        // 值行：`colors.accent = cyan`（Interface 根的目录节点带 value_hint=color）。
        let gateway = fx::sample_catalog();
        let catalog = fx::interface_catalog();
        let catalogs = SettingsCatalogs::new(&gateway, Some(&catalog));
        let row = Row {
            root: Root::Interface,
            path: "colors.accent".into(),
            depth: 2,
            label: "accent".into(),
            value: ValueText::Text("cyan".into()),
            markers: fx::markers(),
            action: RowAction::Edit(ScalarKind::Str),
            match_spans: Vec::new(),
            tpath: "colors.accent".into(),
        };
        let line = row_line(&row, false, false, &catalogs, None, 20, 80, &palette());
        let cell = line.spans.iter().find(|span| span.content == "■ ");
        assert!(
            cell.is_some_and(|span| span.style.fg == Some(ratatui::style::Color::Cyan)),
            "色块用该颜色着色：{line:?}"
        );

        // 解析失败 / 无 hint → 不画色块（也不 panic）。
        let mut broken = row.clone();
        broken.value = ValueText::Text("neon_pink".into());
        let out = row_line(&broken, false, false, &catalogs, None, 20, 80, &palette()).to_string();
        assert!(!out.contains('■'), "{out:?}");

        let no_hint = row_line(
            &Row {
                root: Root::Gateway,
                ..row.clone()
            },
            false,
            false,
            &catalogs,
            None,
            20,
            80,
            &palette(),
        )
        .to_string();
        assert!(!no_hint.contains('■'), "{no_hint:?}");

        // `reset` 等于没画（否则屏幕上是一块透明）。
        let mut reset = row.clone();
        reset.value = ValueText::Text("reset".into());
        let out = row_line(&reset, false, false, &catalogs, None, 20, 80, &palette()).to_string();
        assert!(!out.contains('■'), "{out:?}");
    }

    #[test]
    fn color_swatch_also_marks_enum_choice_rows() {
        let gateway = fx::sample_catalog();
        let catalog = fx::interface_catalog();
        let catalogs = SettingsCatalogs::new(&gateway, Some(&catalog));
        let row = Row {
            root: Root::Interface,
            path: "colors.accent=cyan".into(),
            depth: 3,
            label: "cyan".into(),
            value: ValueText::Text("命名色".into()),
            markers: fx::markers(),
            action: RowAction::Choose {
                value: Some("cyan".into()),
                selected: true,
            },
            match_spans: Vec::new(),
            tpath: "colors.accent".into(),
        };
        let line = row_line(&row, false, false, &catalogs, None, 20, 80, &palette());
        assert!(
            line.spans
                .iter()
                .any(|span| span.content == "■ "
                    && span.style.fg == Some(ratatui::style::Color::Cyan)),
            "{line:?}"
        );
    }

    #[test]
    fn expansion_arrows_follow_the_next_row_depth() {
        let rows = vec![
            Row {
                depth: 1,
                ..sample_row("providers", "Providers (2)", ValueText::None)
            },
            Row {
                depth: 2,
                ..sample_row("providers[0]", "default", ValueText::None)
            },
            Row {
                depth: 1,
                ..sample_row("tools", "tools (0)", ValueText::None)
            },
        ];
        assert!(is_expanded(&rows, 0), "下一行更深 = 展开");
        assert!(!is_expanded(&rows, 1));
        assert!(!is_expanded(&rows, 2), "末行按折叠处理");
    }

    #[test]
    fn read_only_rows_are_dim_with_a_note() {
        let mut row = sample_row("gateway.host", "host", ValueText::Text("127.0.0.1".into()));
        row.action = RowAction::ReadOnly;
        let line = one(&row, 80);
        assert!(line.to_string().ends_with("(只读)"), "{line:?}");
        let label = line
            .spans
            .iter()
            .find(|span| span.content == "host")
            .expect("label span");
        assert_eq!(label.style.fg, Some(palette().dim));
    }

    #[test]
    fn add_item_rows_are_dim_but_the_cursor_still_wins() {
        let mut row = sample_row("providers[]", "(+ 新增一项)", ValueText::None);
        row.action = RowAction::AddItem { variants: 1 };
        let plain = one(&row, 60);
        let label = plain
            .spans
            .iter()
            .find(|span| span.content == "(+ 新增一项)")
            .expect("label");
        assert_eq!(label.style.fg, Some(palette().dim));

        let catalog = fx::sample_catalog();
        let catalogs = SettingsCatalogs::new(&catalog, None);
        let cursor = row_line(&row, true, false, &catalogs, None, 20, 60, &palette());
        let label = cursor
            .spans
            .iter()
            .find(|span| span.content == "(+ 新增一项)")
            .expect("label");
        assert_eq!(label.style.fg, Some(palette().accent));
    }

    #[test]
    fn rows_never_exceed_the_width() {
        let rows = vec![
            sample_row(
                "a",
                "一个很长很长很长的中文标签",
                ValueText::Text("值也长".repeat(20)),
            ),
            sample_row(
                "b",
                "x",
                ValueText::Masked {
                    hint: Some("ab12".into()),
                },
            ),
        ];
        for width in [10usize, 20, 40, 80] {
            for row in &rows {
                let out = one(row, width).to_string();
                assert!(
                    UnicodeWidthStr::width(out.as_str()) <= width,
                    "width {width}: {out:?}"
                );
            }
        }
    }

    #[test]
    fn a_row_is_rendered_even_without_a_matching_node() {
        // 合成行 / 文档对不上目录时，渲染路径依然成立（hint 取不到 → 没有色块）。
        let mut row = sample_row("mystery", "mystery", ValueText::Text("{\"a\":1}".into()));
        row.action = RowAction::ReadOnly;
        row.markers.apply = ApplyScope::Readonly;
        let out = one_text(&row, 60);
        assert!(out.contains("{\"a\":1}"), "{out:?}");
        assert!(out.contains("readonly"), "{out:?}");
    }
}
