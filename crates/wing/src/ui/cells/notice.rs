//! NoticeRow — the one quiet form shared by system / warning / error messages.
//!
//! These three used to render as a two-line block: a coloured label row
//! (`⦁ system` / `⦁ warning` / `⦁ error`) followed by an italic body at column
//! zero. The label carried no information (the *form* is what says "this came
//! from the system"), cost a whole row, reused the message bullet's glyph —
//! which reads as "someone said this" — and the italics are the markdown
//! emphasis register, not a notice register.
//!
//! The form here is a **rail**: `│ ` in the cell-type marker column, one
//! register for the whole row:
//!
//! ```text
//! │ 已加载的 Skills:
//! │   pdf: 处理 PDF 文档
//! │
//! │ 已加载的 Rules 文件:
//! │   /Users/…/AGENTS.md
//! ```
//!
//! * `│` is already the vocabulary's annotation glyph (markdown blockquotes
//!   carry the same `Border` bar) — a notice then never reads as content;
//! * single-line facts stay one line; multi-line output becomes one block with
//!   a boundary you can see from either end (a long `/context` dump included);
//! * severity rides the whole row: info = `tool_result` data under a `dim`
//!   rail (the same two registers a tool result uses), warning = `warning`,
//!   error = `danger` — the same colour semantics the old label row had;
//! * no italics, no label, no English tag.
//!
//! Body lines are **pre-wrapped** (UAX #14, CJK-aware) to `width - 2` so every
//! emitted row already fits and carries the rail — nothing wraps behind the
//! rail at draw time, and the cell height stays the line count.

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::render::markdown::truncate_to_display_width;
use crate::render::markdown::wrap::wrap_plain_text;

/// The rail glyph — box drawing, same one markdown blockquotes use.
const RAIL: &str = "│";
/// Columns the rail prefix takes (`│ `), i.e. the body's left edge.
const RAIL_WIDTH: usize = 2;

/// Severity of a notice row — the whole row's register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// A system message: session facts, command output, info notices.
    Info,
    /// A notice that does not end the turn (e.g. "retrying in 6s").
    Warning,
    /// A real error — the turn is over.
    Error,
}

impl NoticeLevel {
    /// Style of the body text.
    fn body(self, palette: &ThemePalette) -> Style {
        match self {
            Self::Info => Style::default().fg(palette.tool_result),
            Self::Warning => Style::default().fg(palette.warning),
            Self::Error => Style::default().fg(palette.danger),
        }
    }

    /// Style of the rail. Info is chrome (`dim`, like every other gutter);
    /// warning / error carry their severity on the marker too, so the row is
    /// scannable by its left edge alone.
    fn rail(self, palette: &ThemePalette) -> Style {
        match self {
            Self::Info => Style::default().fg(palette.dim),
            Self::Warning | Self::Error => self.body(palette),
        }
    }
}

/// Render `text` as a notice row (or block) for `width` columns.
///
/// Always ends with one blank line — the cell rhythm of the chat view.
pub fn notice_lines(
    text: &str,
    level: NoticeLevel,
    width: u16,
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    let width = width as usize;
    let body = level.body(palette);
    let rail = level.rail(palette);

    // Zero columns: nothing can be painted — return the blank row alone so the
    // caller's height arithmetic still has a line to measure.
    if width == 0 {
        return vec![Line::from("")];
    }

    // Too narrow for the rail (`│ ` is 2 columns): degrade to plain rows rather
    // than push the body off the right edge.
    if width <= RAIL_WIDTH {
        let mut lines: Vec<Line<'static>> = wrap_plain_text(text, width)
            .into_iter()
            .map(|row| Line::from(Span::styled(clamp(&row, width), body)))
            .collect();
        lines.push(Line::from(""));
        return lines;
    }

    let body_width = width - RAIL_WIDTH;
    let mut rows = wrap_plain_text(text, body_width);
    if rows.is_empty() {
        // An empty message still shows its marker, not a hole in the flow.
        rows.push(String::new());
    }

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 1);
    for row in rows {
        let row = clamp(&row, body_width);
        // A blank body row keeps the rail and nothing after it — a trailing
        // space would be invisible ink the copy path would carry along.
        let spans = if row.is_empty() {
            vec![Span::styled(RAIL.to_string(), rail)]
        } else {
            vec![
                Span::styled(format!("{RAIL} "), rail),
                Span::styled(row, body),
            ]
        };
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines
}

/// Clamp a wrapped row to `width` columns.
///
/// `wrap_plain_text` breaks at UAX #14 opportunities, but a **double-width**
/// glyph can never be split: at a 1-column budget (a pathological terminal, or
/// `width - 2 == 1`) a CJK row would still be one column too wide. Drop what
/// cannot fit instead of painting outside the cell — the renderer must obey
/// the same "no row exceeds the width" invariant every other cell holds.
fn clamp(row: &str, width: usize) -> String {
    if UnicodeWidthStr::width(row) > width {
        truncate_to_display_width(row, width)
    } else {
        row.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;
    use unicode_width::UnicodeWidthStr;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_width(line: &Line<'_>) -> usize {
        line.spans.iter().map(|s| s.content.width()).sum()
    }

    // ── 形态 ────────────────────────────────────────────────

    #[test]
    fn single_line_notice_is_one_row_plus_the_trailing_blank() {
        let lines = notice_lines(
            "模型生成 调用失败 (1/11)，3s 后重试",
            NoticeLevel::Warning,
            80,
            &palette(),
        );
        assert_eq!(lines.len(), 2, "单行注记 = 一行正文 + 一个空行");
        assert_eq!(
            lines[0].spans[0].content.as_ref(),
            "│ ",
            "首 span 是栏杆前缀"
        );
        assert!(
            !text_of(&lines).contains("⦁"),
            "注记行不占用消息子弹：{}",
            text_of(&lines)
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC)),
            "不该有斜体（斜体是 markdown 强调的寄存器）"
        );
    }

    #[test]
    fn multi_line_body_keeps_the_rail_on_every_row() {
        let lines = notice_lines(
            "已加载的 Skills:\n  pdf: 处理 PDF\n\n已加载的 Rules 文件:\n  AGENTS.md",
            NoticeLevel::Info,
            80,
            &palette(),
        );
        // 5 行正文 + 末尾空行。
        assert_eq!(lines.len(), 6);
        for line in &lines[..5] {
            assert!(
                line.spans[0].content.starts_with('│'),
                "每一行都要带栏杆：{line:?}"
            );
        }
        // 空行：只有栏杆，没有尾随空格。
        let blank = lines[2].spans[0].content.to_string();
        assert_eq!(blank, "│");
        assert!(
            lines.iter().all(|l| !l.to_string().ends_with(' ')),
            "不该有尾随空格"
        );
    }

    #[test]
    fn body_starts_in_the_second_column() {
        let lines = notice_lines("x", NoticeLevel::Info, 80, &palette());
        let body = &lines[0].spans[1];
        assert_eq!(body.content.as_ref(), "x");
        assert_eq!(
            lines[0].spans[0].content.width() + body.content.width(),
            line_width(&lines[0])
        );
        assert_eq!(lines[0].spans[0].content.as_ref(), "│ ");
    }

    #[test]
    fn empty_message_still_shows_its_marker() {
        let lines = notice_lines("", NoticeLevel::Error, 80, &palette());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content.as_ref(), "│");
    }

    // ── 寄存器（颜色）──────────────────────────────────────

    #[test]
    fn severity_rides_the_whole_row() {
        let p = palette();
        // info：dim 栏杆 + tool_result 正文（与工具结果同一套寄存器）。
        let info = notice_lines("facts", NoticeLevel::Info, 80, &p);
        assert_eq!(info[0].spans[0].style.fg, Some(p.dim));
        assert_eq!(info[0].spans[1].style.fg, Some(p.tool_result));
        // warning / error：整行一个语义色。
        for (level, color) in [
            (NoticeLevel::Warning, p.warning),
            (NoticeLevel::Error, p.danger),
        ] {
            let lines = notice_lines("x", level, 80, &p);
            assert_eq!(lines[0].spans[0].style.fg, Some(color), "{level:?} 栏杆");
            assert_eq!(lines[0].spans[1].style.fg, Some(color), "{level:?} 正文");
        }
        // 品牌色（accent）不参与注记行：它留给身份与交互。
        for level in [NoticeLevel::Info, NoticeLevel::Warning, NoticeLevel::Error] {
            for span in &notice_lines("a", level, 80, &p)[0].spans {
                assert_ne!(
                    span.style.fg,
                    Some(p.accent),
                    "{level:?} 不该用品牌色：{span:?}"
                );
            }
        }
    }

    // ── 宽度与折行 ──────────────────────────────────────────

    #[test]
    fn every_row_fits_every_width() {
        // 核心不变量：任何宽度下都不能有行超出（Paragraph 不换行，超了就是被裁）。
        let long = "模型生成 调用失败 (1/11)：TimeoutError: stalled for 60s without any byte \
                    from the upstream provider, 3s 后重试";
        for width in 0u16..160 {
            for level in [NoticeLevel::Info, NoticeLevel::Warning, NoticeLevel::Error] {
                let lines = notice_lines(long, level, width, &palette());
                for line in &lines {
                    assert!(
                        line_width(line) <= width as usize,
                        "width={width} level={level:?} 行超宽：{line:?}"
                    );
                }
            }
        }
        // 中文长行同样不溢出（UAX #14 断点，不是整段推到下一行）。
        let cjk = "这是一条很长的中文系统注记，用来验证换行发生在这个宽度之内而不是溢出";
        for width in 0u16..80 {
            for line in &notice_lines(cjk, NoticeLevel::Info, width, &palette()) {
                assert!(
                    line_width(line) <= width as usize,
                    "width={width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn wrapped_continuation_keeps_the_rail() {
        let lines = notice_lines(
            "aaaa bbbb cccc dddd eeee",
            NoticeLevel::Info,
            12,
            &palette(),
        );
        assert!(lines.len() > 2, "应折行：{lines:?}");
        for line in &lines[..lines.len() - 1] {
            assert!(
                line.spans[0].content.starts_with('│'),
                "续行也要带栏杆：{line:?}"
            );
        }
    }

    #[test]
    fn narrow_width_drops_the_rail_instead_of_the_body() {
        // 宽度不足两列：退化为无栏杆的普通行，不溢出。
        for width in 0u16..=2 {
            let lines = notice_lines("abc", NoticeLevel::Warning, width, &palette());
            assert_eq!(lines.last().map(|l| l.to_string()), Some(String::new()));
            assert!(
                lines.iter().all(|l| !l.to_string().starts_with('│')),
                "width={width} 时不该画栏杆：{lines:?}"
            );
        }
    }
}
