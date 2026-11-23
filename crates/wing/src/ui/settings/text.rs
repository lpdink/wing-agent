//! 渲染原语：行裁剪、CJK 折行、带高度预算的行缓冲。
//!
//! 一条硬约束是「任何一行都不许溢出自己的区域」。`clamp_line` 是它的唯一实现：
//! 所有组合出来的 [`Line`] 在写进 buffer 之前都过一遍它（最外层还有 `Buffer::set_line`
//! 的 `max_width` 兜底），于是「多尺寸不溢出」不是纪律而是结构。

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::render::markdown::truncate_to_display_width;
use crate::render::markdown::wrap::wrap_plain_text;

/// 把一行裁剪到 `width` 列（CJK 安全）：超出的 span 被截断，再多的整段丢弃。
///
/// 无条件可调用（放得下就原样返回），因此每个部件的出口都可以机械地套一层。
pub(crate) fn clamp_line(line: Line<'static>, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        if used >= width {
            break;
        }
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if used + span_width <= width {
            used += span_width;
            spans.push(span);
        } else {
            let text = truncate_to_display_width(&span.content, width - used);
            used = width;
            spans.push(Span::styled(text, span.style));
        }
    }
    let mut out = Line::from(spans);
    out.style = line.style;
    out.alignment = line.alignment;
    out
}

/// 折行（UAX #14，CJK 安全）+ 行数上限：最多 `max_rows` 行；溢出的最后一行以 `…` 收尾。
///
/// `width == 0` 或空文本 → 空向量（调用方自己决定要不要画占位）。
pub(crate) fn wrap_rows(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }
    let rows = wrap_plain_text(text, width);
    if rows.len() <= max_rows {
        return rows;
    }
    let mut kept: Vec<String> = rows.into_iter().take(max_rows).collect();
    if let Some(last) = kept.last_mut() {
        *last = format!(
            "{}…",
            truncate_to_display_width(last, width.saturating_sub(1))
        );
    }
    kept
}

/// 键位栏专用折行：按 ` · ` 切段、整段装箱，不在词中间断行。
///
/// 只做「从左往右装箱」会在窄终端丢掉**尾巴** —— 而尾巴上恰恰是 `Esc 关闭` 这类
/// 「不知道就出不去」的键。所以装不完时，最后一行改成**从右往左**装箱并加 `…` 前缀：
/// 头部（移动 / 编辑）与尾部（关闭 / 帮助）同时活着，牺牲的是中间那几段。
pub(crate) fn pack_segments(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }
    let segments: Vec<&str> = text
        .split(" · ")
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return Vec::new();
    }

    let mut rows: Vec<String> = Vec::new();
    let mut front = 0usize;
    while front < segments.len() && rows.len() < max_rows {
        let mut row = String::new();
        while front < segments.len() && try_append(&mut row, segments[front], width) {
            front += 1;
        }
        if row.is_empty() {
            // 单段就超宽：自己占一行，交给调用方按宽度兜底裁剪。
            row.push_str(segments[front]);
            front += 1;
        }
        rows.push(row);
    }

    // 装不完 → 最后一行从右往左重装（保住尾巴）。
    if front < segments.len() && rows.len() >= 2 {
        let mut tail = String::new();
        let mut back = segments.len();
        while back > front {
            let candidate = if tail.is_empty() {
                segments[back - 1].to_string()
            } else {
                format!("{} · {}", segments[back - 1], tail)
            };
            if display_width(&candidate) + 2 > width && !tail.is_empty() {
                break;
            }
            tail = candidate;
            back -= 1;
        }
        if let Some(last) = rows.last_mut() {
            *last = format!("… {tail}");
        }
    }
    rows
}

/// 试着把一段追加进行（段之间用 ` · ` 连接）；放不下返回 `false`。
fn try_append(row: &mut String, segment: &str, width: usize) -> bool {
    let extra = if row.is_empty() {
        display_width(segment)
    } else {
        3 + display_width(segment)
    };
    if display_width(row) + extra > width {
        return false;
    }
    if !row.is_empty() {
        row.push_str(" · ");
    }
    row.push_str(segment);
    true
}

/// 文本的显示宽度（CJK 安全）。
pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// 单行文本的裁剪（放得下就原样返回）。
pub(crate) fn single_row(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    format!(
        "{}…",
        truncate_to_display_width(text, width.saturating_sub(1))
    )
}

/// 带高度预算的行缓冲：满了之后的 `push` 静默丢弃（调用方不需要自己数行）。
pub(crate) struct LineBuf {
    width: usize,
    height: usize,
    lines: Vec<Line<'static>>,
}

impl LineBuf {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            lines: Vec::new(),
        }
    }

    /// 剩余可写行数。
    pub(crate) fn remaining(&self) -> usize {
        self.height.saturating_sub(self.lines.len())
    }

    /// 写一行（自动裁到宽度）；缓冲满了返回 `false`。
    pub(crate) fn push(&mut self, line: Line<'static>) -> bool {
        if self.remaining() == 0 {
            return false;
        }
        self.lines.push(clamp_line(line, self.width));
        true
    }

    /// 写一行纯文本。
    pub(crate) fn text(&mut self, text: &str, style: Style) -> bool {
        self.push(Line::from(Span::styled(text.to_string(), style)))
    }

    pub(crate) fn blank(&mut self) {
        self.push(Line::from(""));
    }

    pub(crate) fn take(self) -> Vec<Line<'static>> {
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn text(line: &Line<'static>) -> String {
        line.to_string()
    }

    #[test]
    fn clamp_line_truncates_cjk_safely_and_keeps_what_fits() {
        let line = Line::from(vec![
            Span::styled("abc", Style::default().fg(Color::Red)),
            Span::styled("中文标签", Style::default().fg(Color::Blue)),
        ]);
        assert_eq!(text(&clamp_line(line.clone(), 10)), "abc中文标");
        assert_eq!(text(&clamp_line(line.clone(), 3)), "abc");
        assert_eq!(text(&clamp_line(line, 0)), "");
    }

    #[test]
    fn clamp_line_never_exceeds_the_budget() {
        let line = Line::from(vec![Span::raw("一二三四五"), Span::raw("abc")]);
        for width in 0..12 {
            let out = text(&clamp_line(line.clone(), width));
            assert!(
                UnicodeWidthStr::width(out.as_str()) <= width,
                "width {width}: {out:?}"
            );
        }
    }

    #[test]
    fn wrap_rows_caps_the_rows_and_marks_the_overflow() {
        let long = "这是一段很长的中文说明文字".repeat(4);
        let rows = wrap_rows(&long, 10, 2);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].ends_with('…'), "{rows:?}");
        for row in &rows {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 10, "{row:?}");
        }
        // 放得下 → 原样。
        assert_eq!(wrap_rows("短", 10, 3), vec!["短".to_string()]);
        assert!(wrap_rows("", 10, 3).is_empty());
        assert!(wrap_rows("abc", 0, 3).is_empty());
    }

    #[test]
    fn pack_segments_keeps_whole_segments_and_never_drops_the_tail_alone() {
        let hint = "↑↓ 移动 · Enter 编辑 · s 保存 · p 问题 · / 搜索 · Tab 切根 · ? 帮助 · Esc 关闭";
        // 宽得下 → 两行装完，尾巴在第二行。
        let rows = pack_segments(hint, 60, 2);
        assert!(rows.len() <= 2, "{rows:?}");
        assert!(rows[0].starts_with("↑↓ 移动"), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("Esc 关闭")), "{rows:?}");
        for row in &rows {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 60, "{row:?}");
        }
        // 窄得装不完 → 头尾都在，中间省略（`…` 前缀）。
        let rows = pack_segments(hint, 34, 2);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(rows[0].starts_with("↑↓ 移动"), "{rows:?}");
        assert!(rows[1].starts_with('…'), "省略号在最后一行开头：{rows:?}");
        assert!(rows[1].ends_with("Esc 关闭"), "尾巴活着：{rows:?}");
        for row in &rows {
            assert!(UnicodeWidthStr::width(row.as_str()) <= 34, "{row:?}");
        }
        // 单段超宽 / 退化输入。
        let rows = pack_segments("a · bbbbbbbbbbbbbbbbbbbbbb", 6, 4);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(pack_segments("", 10, 2).is_empty());
        assert!(pack_segments("x", 0, 2).is_empty());
        assert!(pack_segments("x", 10, 0).is_empty());
    }

    #[test]
    fn line_buf_respects_its_height_budget() {
        let mut buf = LineBuf::new(6, 2);
        assert!(buf.push(Line::from("一")));
        assert!(buf.push(Line::from("二")));
        assert!(!buf.push(Line::from("三")), "满了之后静默丢弃");
        assert_eq!(buf.remaining(), 0);
        let out = buf.take();
        assert_eq!(out.len(), 2);
    }
}
