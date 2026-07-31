//! CJK-aware prose line wrapping (UAX #14).
//!
//! ratatui's `Paragraph` word-wrap only breaks at whitespace and never splits
//! a "word". CJK text has no spaces, so a long CJK run becomes one giant word
//! that gets pushed whole to the next line, leaving the current line short with
//! a trail of buffer-padding spaces. That is the bug this module fixes.
//!
//! We pre-wrap prose ourselves using Unicode line-break opportunities
//! (UAX #14, via the `unicode-linebreak` crate) so CJK runs break at the margin
//! while Latin words stay intact and CJK punctuation honors kinsoku rules.
//!
//! The mechanism matches codex-rs (Apache-2.0), which enables textwrap's
//! `unicode-linebreak` feature for exactly this reason; here we use the
//! focused `unicode-linebreak` crate directly as the break-opportunity oracle
//! and do a thin greedy fill over our `MarkdownLine` IR so each segment's
//! kind/style/link is preserved through the wrap.

use ratatui::style::Style;
use unicode_linebreak::{BreakOpportunity, linebreaks};
use unicode_width::UnicodeWidthStr;

use super::tables::split_str_by_width;
use super::types::{MarkdownLine, SegmentKind};

/// A flattened segment: its byte range within the flattened line text plus the
/// styling needed to reconstruct it after wrapping.
struct FlatSeg {
    start: usize,
    end: usize,
    kind: SegmentKind,
    style: Style,
    link: Option<String>,
}

/// Whether a line is prose that should be width-wrapped.
///
/// Code blocks (`CodeBlock`/`Gutter`) and decorative chrome (`Border`) are laid
/// out by their own renderers and must not be re-wrapped; everything else
/// (`Text`/`Heading`/`InlineCode`/`Link`/`Marker`) is prose.
pub(crate) fn is_prose_line(line: &MarkdownLine) -> bool {
    !line.segments.is_empty()
        && line.segments.iter().all(|s| {
            !matches!(
                s.kind,
                SegmentKind::CodeBlock | SegmentKind::Gutter | SegmentKind::Border
            )
        })
}

/// Wrap prose lines to `width` display columns.
///
/// Non-prose lines (code, borders, gutters) and lines that already fit are
/// passed through untouched, so this only ever changes over-wide prose.
pub(crate) fn wrap_prose_lines(lines: Vec<MarkdownLine>, width: usize) -> Vec<MarkdownLine> {
    if width == 0 {
        return lines;
    }
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if is_prose_line(&line) && line.width() > width {
            out.extend(wrap_prose_line(&line, width));
        } else {
            out.push(line);
        }
    }
    out
}

/// Wrap a single prose line into one or more lines, each ≤ `width` display
/// columns, breaking at UAX #14 opportunities and preserving segment styling.
fn wrap_prose_line(line: &MarkdownLine, width: usize) -> Vec<MarkdownLine> {
    // Flatten segment texts into one string, recording byte ranges + styling.
    let mut flat = String::new();
    let mut segs: Vec<FlatSeg> = Vec::new();
    for seg in &line.segments {
        let start = flat.len();
        flat.push_str(&seg.text);
        segs.push(FlatSeg {
            start,
            end: flat.len(),
            kind: seg.kind,
            style: seg.style,
            link: seg.link_target.clone(),
        });
    }

    let ranges = wrap_ranges(&flat, width);

    let mut out = Vec::with_capacity(ranges.len());
    for (i, range) in ranges.iter().enumerate() {
        let (mut a, mut b) = *range;
        // Trim leading spaces on continuation lines and trailing spaces on
        // every line so wrapped prose looks clean.
        if i > 0 {
            while a < b && flat[a..].starts_with(' ') {
                a += 1;
            }
        }
        while b > a && flat[..b].ends_with(' ') {
            b -= 1;
        }
        if a >= b {
            // Whitespace-only remnant — keep a blank line for structure.
            out.push(MarkdownLine::default());
            continue;
        }
        out.push(slice_segments(&segs, &flat, a, b));
    }

    if out.is_empty() {
        out.push(line.clone());
    }
    out
}

/// Reconstruct a line from the segments overlapping byte range `[a, b)` of the
/// flattened buffer, splitting a segment if a boundary falls inside it.
/// Kind/style/link are preserved so downstream element-aware rendering (e.g.
/// thinking recolor) keeps working.
fn slice_segments(segs: &[FlatSeg], flat: &str, a: usize, b: usize) -> MarkdownLine {
    let mut line = MarkdownLine::default();
    for seg in segs {
        let s = seg.start.max(a);
        let e = seg.end.min(b);
        if s >= e {
            continue;
        }
        // `flat` is the contiguous concatenation of all segment texts, so the
        // byte slice `[s, e)` is exactly this segment's substring.
        let text = &flat[s..e];
        line.push_segment_with_link(seg.kind, seg.style, text, seg.link.clone());
    }
    line
}

/// Compute the byte ranges of each wrapped line of `flat`.
///
/// Splits `flat` into atomic chunks at every UAX #14 break opportunity, then
/// greedily fills lines to `width`. A chunk wider than the whole line (e.g. a
/// long unbreakable URL) is hard-broken by display width.
fn wrap_ranges(flat: &str, width: usize) -> Vec<(usize, usize)> {
    // Build atomic chunks: [prev, offset) for each break opportunity.
    let mut chunks: Vec<(usize, usize, bool)> = Vec::new();
    let mut prev = 0usize;
    for (offset, opp) in linebreaks(flat) {
        let end = offset.min(flat.len());
        if end > prev {
            chunks.push((prev, end, matches!(opp, BreakOpportunity::Mandatory)));
        }
        prev = prev.max(end);
    }
    if prev < flat.len() {
        chunks.push((prev, flat.len(), true));
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut line_start: Option<usize> = None;
    let mut line_end = 0usize;
    let mut cur_w = 0usize;

    for &(cs, ce, mandatory) in &chunks {
        let is_ws_only = flat[cs..ce].trim().is_empty();

        // Never begin a fresh line with a whitespace-only chunk.
        if line_start.is_none() && is_ws_only {
            continue;
        }

        let chunk_w = UnicodeWidthStr::width(&flat[cs..ce]);

        if line_start.is_none() {
            // Start a new line with this chunk.
            if chunk_w > width {
                hard_break(flat, cs, ce, width, &mut ranges);
            } else {
                line_start = Some(cs);
                line_end = ce;
                cur_w = chunk_w;
                if mandatory {
                    ranges.push((cs, ce));
                    line_start = None;
                    cur_w = 0;
                }
            }
        } else if cur_w + chunk_w <= width {
            // Fits on the current line.
            line_end = ce;
            cur_w += chunk_w;
            if mandatory {
                ranges.push((line_start.unwrap(), line_end));
                line_start = None;
                cur_w = 0;
            }
        } else {
            // Doesn't fit: finish the current line, then place this chunk on a
            // fresh one.
            ranges.push((line_start.unwrap(), line_end));
            line_start = None;
            cur_w = 0;
            if is_ws_only {
                continue; // drop leading whitespace on the new line
            }
            if chunk_w > width {
                hard_break(flat, cs, ce, width, &mut ranges);
            } else {
                line_start = Some(cs);
                line_end = ce;
                cur_w = chunk_w;
                if mandatory {
                    ranges.push((cs, ce));
                    line_start = None;
                    cur_w = 0;
                }
            }
        }
    }

    if let Some(s) = line_start {
        ranges.push((s, line_end));
    }
    ranges
}

/// Break a span `[cs, ce)` that is wider than `width` into full-width pieces.
fn hard_break(flat: &str, cs: usize, ce: usize, width: usize, ranges: &mut Vec<(usize, usize)>) {
    let mut s = cs;
    while s < ce {
        let (head, _) = split_str_by_width(&flat[s..ce], width);
        let hl = if head.is_empty() {
            // Safety valve: advance one char so we always make progress.
            flat[s..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(ce - s)
        } else {
            head.len()
        };
        ranges.push((s, s + hl));
        s += hl;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    fn text_line(s: &str) -> MarkdownLine {
        let mut l = MarkdownLine::default();
        l.push_segment(SegmentKind::Text, Style::new(), s);
        l
    }

    fn widths(lines: &[MarkdownLine]) -> Vec<usize> {
        lines.iter().map(|l| l.width()).collect()
    }

    fn joined(lines: &[MarkdownLine]) -> String {
        lines
            .iter()
            .map(|l| l.to_plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn cjk_long_line_wraps_at_margin() {
        // 20 CJK chars = 40 columns; wrap at 10 cols → 5 chars (10 cols) each.
        let line = text_line("一二三四五六七八九十一二三四五六七八九十");
        let out = wrap_prose_line(&line, 10);
        assert!(
            out.len() >= 4,
            "expected several lines, got {:?}",
            widths(&out)
        );
        for w in widths(&out) {
            assert!(w <= 10, "line exceeds width: {w}");
        }
        // No content lost.
        let flat: String = out.iter().map(|l| l.to_plain()).collect();
        assert_eq!(flat, "一二三四五六七八九十一二三四五六七八九十");
    }

    #[test]
    fn latin_words_stay_intact() {
        let line = text_line("alpha beta gamma delta");
        let out = wrap_prose_line(&line, 11);
        let text = joined(&out);
        // Words must not be split mid-word.
        for word in ["alpha", "beta", "gamma", "delta"] {
            assert!(text.contains(word), "word broken: {word} in {text}");
        }
        for w in widths(&out) {
            assert!(w <= 11, "line exceeds width: {w}");
        }
    }

    #[test]
    fn mixed_cjk_and_latin_wraps() {
        let line = text_line("Chat 是只读的——它告诉你东西，什么都没发生，零风险。Agent 是要动手的");
        let out = wrap_prose_line(&line, 20);
        assert!(out.len() >= 2, "should wrap: {:?}", widths(&out));
        for w in widths(&out) {
            assert!(w <= 20, "line exceeds width: {w}");
        }
        let flat: String = out.iter().map(|l| l.to_plain()).collect();
        // Spaces may be trimmed at break points; compare without spaces.
        let no_space: String = flat.chars().filter(|c| *c != ' ').collect();
        let orig_no_space: String =
            "Chat 是只读的——它告诉你东西，什么都没发生，零风险。Agent 是要动手的"
                .chars()
                .filter(|c| *c != ' ')
                .collect();
        assert_eq!(no_space, orig_no_space);
    }

    #[test]
    fn segment_style_preserved_across_wrap() {
        // A bold run that gets split across the wrap boundary must keep its
        // style on both fragments.
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Text, Style::new(), "前缀");
        line.push_segment(
            SegmentKind::Text,
            Style::new().bold(),
            "粗体内容很长需要被折断",
        );
        let out = wrap_prose_line(&line, 8);
        assert!(out.len() >= 2);
        // Every non-empty segment carrying bold content keeps the bold modifier.
        let bold_frags: Vec<&str> = out
            .iter()
            .flat_map(|l| l.segments.iter())
            .filter(|s| {
                s.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD)
            })
            .map(|s| s.text.as_str())
            .collect();
        let bold_text: String = bold_frags.concat();
        assert_eq!(
            bold_text, "粗体内容很长需要被折断",
            "bold fragments: {bold_frags:?}"
        );
    }

    #[test]
    fn wrap_prose_lines_skips_code_and_fitting() {
        // A code-block line wider than width must NOT be wrapped.
        let mut code = MarkdownLine::default();
        code.push_segment(
            SegmentKind::CodeBlock,
            Style::new(),
            "a_very_long_code_line_that_exceeds",
        );
        // A short prose line must pass through unchanged.
        let short = text_line("hi");
        // An over-wide prose line must be wrapped.
        let wide = text_line("一二三四五六七八九十一二三四五六");

        let out = wrap_prose_lines(vec![code.clone(), short.clone(), wide], 10);
        // code line preserved verbatim
        assert_eq!(out[0].to_plain(), code.to_plain());
        assert_eq!(out[0].segments.len(), 1);
        // short line preserved
        assert_eq!(out[1].to_plain(), "hi");
        // wide prose expanded into multiple lines, all ≤ width
        let prose: Vec<_> = out[2..].to_vec();
        assert!(
            prose.len() >= 2,
            "wide prose should wrap: {:?}",
            widths(&prose)
        );
        for w in widths(&prose) {
            assert!(w <= 10);
        }
    }

    #[test]
    fn is_prose_line_classification() {
        assert!(is_prose_line(&text_line("hello")));

        let mut border = MarkdownLine::default();
        border.push_segment(SegmentKind::Border, Style::new(), "┌──┐");
        assert!(!is_prose_line(&border));

        let mut gutter = MarkdownLine::default();
        gutter.push_segment(SegmentKind::Gutter, Style::new(), "1");
        gutter.push_segment(SegmentKind::CodeBlock, Style::new(), "let x = 1;");
        assert!(!is_prose_line(&gutter));

        // Prose with inline code is still prose.
        let mut mixed = MarkdownLine::default();
        mixed.push_segment(SegmentKind::Text, Style::new(), "run ");
        mixed.push_segment(SegmentKind::InlineCode, Style::new(), "cargo build");
        assert!(is_prose_line(&mixed));
    }

    #[test]
    fn long_unbreakable_token_hard_breaks() {
        // A token wider than the line with no break opportunity still gets
        // broken so no line exceeds the width.
        let line = text_line("abcdefghijklmnopqrstuvwxyz");
        let out = wrap_prose_line(&line, 10);
        for w in widths(&out) {
            assert!(w <= 10, "line exceeds width: {w}");
        }
        let flat: String = out.iter().map(|l| l.to_plain()).collect();
        assert_eq!(flat, "abcdefghijklmnopqrstuvwxyz");
    }
}
