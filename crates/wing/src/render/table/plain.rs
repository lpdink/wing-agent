//! Plain-text table renderer for the CLI (`wing ps` / `wing tools`).
//!
//! Same skin and width engine as the TUI markdown tables ([`super`]), so the
//! two frontends read as one product. Two deliberate differences:
//!
//! - cells **truncate** here (a terminal row is a hard constraint, not a
//!   wrapping surface; markdown tables word-wrap instead), and
//! - every measurement is display columns via `unicode-width`, because a CJK
//!   cell must not shift its column (the reason the old hand-padded tables
//!   went ragged).
//!
//! Output is a `Vec<String>` of equal-width lines; ANSI colour is emitted only
//! when the caller asks for it (the caller decides TTY / `NO_COLOR`), and the
//! escapes never count towards the measured width.

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

use super::ColumnKind;
use super::ColumnMetrics;
use super::MIN_COLUMN_WIDTH;
use super::RuleGlyphs;
use super::TableSkin;
use super::compute_column_widths;
use super::frame_overhead;
use super::longest_token_width;
use super::split_str_by_width;

/// One column of a plain table: header text plus its width policy.
pub struct PlainColumn {
    pub header: String,
    /// Shrink priority / floor class (see [`super::compute_column_widths`]).
    pub kind: ColumnKind,
    /// Natural width ceiling — keeps a wide terminal from stretching one
    /// column across the whole screen.
    pub max_width: Option<usize>,
    /// Never shrink below the natural width: for values whose full text *is*
    /// the point (a session id that gets copied and matched exactly). A
    /// too-narrow terminal makes the table overflow rather than truncate such
    /// a column (see [`super::ColumnMetrics::min_width`]).
    pub keep_natural: bool,
}

impl PlainColumn {
    pub fn new(header: impl Into<String>, kind: ColumnKind) -> Self {
        Self {
            header: header.into(),
            kind,
            max_width: None,
            keep_natural: false,
        }
    }

    /// A column with a natural-width ceiling.
    pub fn capped(header: impl Into<String>, kind: ColumnKind, max_width: usize) -> Self {
        Self {
            max_width: Some(max_width),
            ..Self::new(header, kind)
        }
    }

    /// A column that keeps its full natural width (no truncation ever).
    pub fn keep_natural(header: impl Into<String>, kind: ColumnKind) -> Self {
        Self {
            keep_natural: true,
            ..Self::new(header, kind)
        }
    }
}

/// One cell: text plus optional ink (`None` = the terminal default ink).
pub struct PlainCell {
    pub text: String,
    pub style: Option<Style>,
}

impl PlainCell {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: None,
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style: Some(style),
        }
    }
}

/// A whole table to render.
pub struct PlainTable {
    pub columns: Vec<PlainColumn>,
    pub rows: Vec<Vec<PlainCell>>,
}

/// Render options: the available width, whether ANSI colour may be emitted,
/// and the two chrome inks (the frame/grid and the header text).
pub struct PlainOpts {
    pub width: usize,
    pub color: bool,
    pub frame: Style,
    pub header: Style,
}

/// Render `table` as framed plain-text lines (no trailing newline).
///
/// Every returned line has the same display width (ANSI escapes excluded), so
/// a caller can print them back to back.
pub fn render(table: &PlainTable, opts: &PlainOpts) -> Vec<String> {
    let col_count = table.columns.len();
    if col_count == 0 {
        return Vec::new();
    }

    // Natural widths: the widest of header and cells, capped per column.
    let metrics: Vec<ColumnMetrics> = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let mut max_width = UnicodeWidthStr::width(column.header.as_str());
            let mut body_token_width = 0usize;
            for row in &table.rows {
                let text = row.get(i).map(|c| c.text.as_str()).unwrap_or("");
                max_width = max_width.max(UnicodeWidthStr::width(text));
                body_token_width = body_token_width.max(longest_token_width(text));
            }
            if let Some(cap) = column.max_width {
                max_width = max_width.min(cap);
            }
            ColumnMetrics {
                max_width,
                header_token_width: longest_token_width(&column.header),
                body_token_width,
                min_width: if column.keep_natural {
                    max_width
                } else {
                    MIN_COLUMN_WIDTH
                },
                kind: column.kind,
            }
        })
        .collect();

    let content_budget = Some(opts.width.saturating_sub(frame_overhead(col_count)));
    let widths = compute_column_widths(&metrics, content_budget);

    let skin = TableSkin::framed();
    let mut lines = Vec::with_capacity(table.rows.len() * 2 + 3);
    lines.push(rule_line(&skin.top, &widths, opts));
    lines.push(header_line(table, &widths, opts, &skin));
    lines.push(rule_line(&skin.header_sep, &widths, opts));
    for (row_idx, row) in table.rows.iter().enumerate() {
        lines.push(body_line(row, col_count, &widths, opts, &skin));
        if row_idx + 1 < table.rows.len() {
            lines.push(rule_line(&skin.body_sep, &widths, opts));
        }
    }
    lines.push(rule_line(&skin.bottom, &widths, opts));
    lines
}

/// One horizontal rule spanning every column.
fn rule_line(glyphs: &RuleGlyphs, widths: &[usize], opts: &PlainOpts) -> String {
    let mut out = String::new();
    out.push_str(&paint(opts.frame, &glyphs.left.to_string(), opts.color));
    let fill = glyphs.fill.to_string();
    for (i, &w) in widths.iter().enumerate() {
        out.push_str(&paint(opts.frame, &fill.repeat(w + 2), opts.color));
        if i + 1 < widths.len() {
            out.push_str(&paint(opts.frame, &glyphs.junction.to_string(), opts.color));
        }
    }
    out.push_str(&paint(opts.frame, &glyphs.right.to_string(), opts.color));
    out
}

/// The header row: every cell in the header ink.
fn header_line(table: &PlainTable, widths: &[usize], opts: &PlainOpts, skin: &TableSkin) -> String {
    let mut out = String::new();
    out.push_str(&paint(opts.frame, &skin.outer_v.to_string(), opts.color));
    for (i, column) in table.columns.iter().enumerate() {
        let text = fit_truncate(&column.header, widths[i]);
        out.push_str(&cell_text(&text, widths[i], opts.header, opts.color));
        if i + 1 < table.columns.len() {
            out.push_str(&paint(opts.frame, &skin.header_v.to_string(), opts.color));
        }
    }
    out.push_str(&paint(opts.frame, &skin.outer_v.to_string(), opts.color));
    out
}

/// A body row: per-cell ink, cells truncated to their column width.
fn body_line(
    row: &[PlainCell],
    col_count: usize,
    widths: &[usize],
    opts: &PlainOpts,
    skin: &TableSkin,
) -> String {
    let mut out = String::new();
    out.push_str(&paint(opts.frame, &skin.outer_v.to_string(), opts.color));
    for (i, &width) in widths.iter().enumerate().take(col_count) {
        match row.get(i) {
            Some(cell) => {
                let text = fit_truncate(&cell.text, width);
                out.push_str(&cell_text(
                    &text,
                    width,
                    cell.style.unwrap_or_default(),
                    opts.color,
                ));
            }
            None => out.push_str(&cell_text("", width, Style::new(), opts.color)),
        }
        if i + 1 < col_count {
            out.push_str(&paint(opts.frame, &skin.inner_v.to_string(), opts.color));
        }
    }
    out.push_str(&paint(opts.frame, &skin.outer_v.to_string(), opts.color));
    out
}

/// One cell: one padding column, the (already truncated) text painted with
/// `style`, trailing padding up to the column width, one padding column.
/// Padding sits outside the ANSI span — the terminal shows the same grid
/// either way.
fn cell_text(text: &str, width: usize, style: Style, color: bool) -> String {
    let pad = width.saturating_sub(UnicodeWidthStr::width(text));
    let mut out = String::with_capacity(width + 2);
    out.push(' ');
    out.push_str(&paint(style, text, color));
    out.push_str(&" ".repeat(pad));
    out.push(' ');
    out
}

/// Truncate `text` to at most `width` display columns, marking the cut with an
/// ellipsis. A text that already fits is returned as-is.
fn fit_truncate(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let (head, _) = split_str_by_width(text, width - 1);
    format!("{head}…")
}

const RESET: &str = "\u{1b}[0m";

/// ANSI-wrapped `text` when colour is on; the bare text otherwise (or when the
/// style carries no attributes at all).
fn paint(style: Style, text: &str, color: bool) -> String {
    if !color || text.is_empty() {
        return text.to_string();
    }
    let prefix = sgr(style);
    if prefix.is_empty() {
        text.to_string()
    } else {
        format!("{prefix}{text}{RESET}")
    }
}

/// ratatui [`Style`] → SGR prefix for what plain tables use: foreground colour
/// plus the bold / dim modifiers. Empty when the style carries nothing.
fn sgr(style: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    if let Some(fg) = style.fg {
        codes.push(color_code(fg, false));
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        codes.push("1".to_string());
    }
    if style.add_modifier.contains(Modifier::DIM) {
        codes.push("2".to_string());
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\u{1b}[{}m", codes.join(";"))
    }
}

fn color_code(color: Color, background: bool) -> String {
    let base = if background { 40 } else { 30 };
    let bright = if background { 100 } else { 90 };
    match color {
        Color::Reset => "0".to_string(),
        Color::Black => base.to_string(),
        Color::Red => (base + 1).to_string(),
        Color::Green => (base + 2).to_string(),
        Color::Yellow => (base + 3).to_string(),
        Color::Blue => (base + 4).to_string(),
        Color::Magenta => (base + 5).to_string(),
        Color::Cyan => (base + 6).to_string(),
        Color::Gray => (base + 7).to_string(),
        Color::DarkGray => bright.to_string(),
        Color::LightRed => (bright + 1).to_string(),
        Color::LightGreen => (bright + 2).to_string(),
        Color::LightYellow => (bright + 3).to_string(),
        Color::LightBlue => (bright + 4).to_string(),
        Color::LightMagenta => (bright + 5).to_string(),
        Color::LightCyan => (bright + 6).to_string(),
        Color::White => (bright + 7).to_string(),
        Color::Indexed(i) => format!("{};5;{i}", if background { 48 } else { 38 }),
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", if background { 48 } else { 38 }),
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(width: usize, color: bool) -> PlainOpts {
        PlainOpts {
            width,
            color,
            frame: Style::new().fg(Color::Rgb(0x5b, 0x64, 0x78)),
            header: Style::new().fg(Color::White).bold(),
        }
    }

    fn table(columns: Vec<PlainColumn>, rows: Vec<Vec<&str>>) -> PlainTable {
        PlainTable {
            columns,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(PlainCell::plain).collect())
                .collect(),
        }
    }

    fn cols() -> Vec<PlainColumn> {
        vec![
            PlainColumn::new("A", ColumnKind::Compact),
            PlainColumn::new("B", ColumnKind::Compact),
        ]
    }

    #[test]
    fn renders_the_framed_grid() {
        let t = table(cols(), vec![vec!["1", "2"], vec!["3", "4"]]);
        let lines = render(&t, &opts(80, false));
        assert_eq!(
            lines,
            [
                "┏━━━━━┳━━━━━┓",
                "┃ A   ┃ B   ┃",
                "┣━━━━━╇━━━━━┫",
                "┃ 1   │ 2   ┃",
                "┠─────┼─────┨",
                "┃ 3   │ 4   ┃",
                "┗━━━━━┷━━━━━┛",
            ]
        );
    }

    #[test]
    fn truncates_by_display_width_not_chars() {
        // 6 CJK chars = 12 columns; a width-10 terminal leaves a 6-column
        // content budget → keep 5 display columns + `…` (3 CJK chars would
        // overflow: 2×3 = 6 > 5).
        let t = table(
            vec![PlainColumn::new("列", ColumnKind::Narrative)],
            vec![vec!["一二三四五六"]],
        );
        let lines = render(&t, &opts(10, false));
        assert!(lines[3].contains("一二…"), "{:?}", lines[3]);
        // Display width is the same as the ruler lines.
        assert_eq!(UnicodeWidthStr::width(lines[3].as_str()), 10);
        assert_eq!(UnicodeWidthStr::width(lines[0].as_str()), 10);
    }

    #[test]
    fn truncation_keeps_the_ellipsis_inside_the_width() {
        assert_eq!(fit_truncate("abcdef", 4), "abc…");
        assert_eq!(fit_truncate("abcdef", 6), "abcdef");
        assert_eq!(fit_truncate("中文字", 2), "…");
        assert_eq!(fit_truncate("hello", 0), "");
    }

    #[test]
    fn narrow_width_shrinks_the_narrative_column_first() {
        let t = table(
            vec![
                PlainColumn::new("ID", ColumnKind::Compact),
                PlainColumn::new("NAME", ColumnKind::Narrative),
            ],
            vec![vec!["abcdefgh", "a fairly long descriptive name goes here"]],
        );
        let lines = render(&t, &opts(30, false));
        // Still a closed grid at 30 columns.
        assert!(
            UnicodeWidthStr::width(lines[0].as_str()) <= 30,
            "{:?}",
            lines[0]
        );
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| UnicodeWidthStr::width(l.as_str()))
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn max_width_caps_the_natural_width() {
        let long = "x".repeat(100);
        let t = table(
            vec![PlainColumn::capped("NAME", ColumnKind::Narrative, 20)],
            vec![vec![long.as_str()]],
        );
        let lines = render(&t, &opts(300, false));
        // Cap 20 + frame 4 = 24, not 104 even on a very wide terminal.
        assert_eq!(UnicodeWidthStr::width(lines[0].as_str()), 24);
    }

    /// `keep_natural` 列（会话 id）在窄预算下保持完整：收缩只发生在其它列。
    #[test]
    fn keep_natural_column_survives_a_narrow_budget() {
        let t = PlainTable {
            columns: vec![
                PlainColumn::keep_natural("SESSION ID", ColumnKind::Compact),
                PlainColumn::capped("NAME", ColumnKind::Narrative, 80),
            ],
            rows: vec![vec![
                PlainCell::plain("20261009-210702-217d85f2"),
                PlainCell::plain("我们前端的表格渲染虽然不错吧 但是其实我更喜欢包裹起来的感觉"),
            ]],
        };
        let lines = render(&t, &opts(80, false));
        assert!(
            lines[3].contains("20261009-210702-217d85f2"),
            "id 不能被截断：{:?}",
            lines[3]
        );
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| UnicodeWidthStr::width(l.as_str()))
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert!(widths[0] <= 80, "{widths:?}");
    }

    #[test]
    fn color_wraps_cells_and_frame_in_ansi() {
        let t = table(cols(), vec![vec!["1", "2"]]);
        let plain = render(&t, &opts(80, false));
        let colored = render(&t, &opts(80, true));
        // Same visible width once the escapes are stripped.
        for (p, c) in plain.iter().zip(&colored) {
            assert_eq!(strip_ansi(c), *p);
        }
        assert!(colored[0].contains("\u{1b}[38;2;91;100;120m"), "frame ink");
        assert!(
            colored[1].contains("\u{1b}[97;1m"),
            "header carries fg + bold: {:?}",
            colored[1]
        );
    }

    #[test]
    fn header_only_table_still_closes() {
        let t = table(cols(), vec![]);
        let lines = render(&t, &opts(80, false));
        assert_eq!(lines.len(), 4);
        assert!(lines[3].starts_with('┗'));
    }

    #[test]
    fn no_columns_renders_nothing() {
        let t = table(vec![], vec![]);
        assert!(render(&t, &opts(80, false)).is_empty());
    }

    /// Strip ANSI SGR sequences (tests only).
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }
}
