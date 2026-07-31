//! Markdown to ratatui Lines renderer.
//!
//! This module renders markdown text into `Vec<Line<'static>>` for display
//! in the terminal UI. The architecture is adapted from VTCode (MIT license).
//!
//! ## Architecture
//!
//! 1. `pulldown-cmark` parses markdown into events
//! 2. `parsing.rs` handles start/end tags, building `MarkdownLine` segments
//! 3. `tables.rs` accumulates and renders table structures
//! 4. `code_blocks.rs` handles fenced code blocks with syntax highlighting
//! 5. `links.rs` provides link URL display logic
//! 6. `types.rs` defines the intermediate `MarkdownLine`/`MarkdownSegment` types
//! 7. This module orchestrates the event loop and provides the public API

pub(crate) mod code_blocks;
pub(crate) mod links;
pub(crate) mod parsing;
pub(crate) mod tables;
pub mod types;
pub(crate) mod wrap;

// Re-export the public API.
pub use types::MarkdownLine;
pub use types::MarkdownSegment;
pub use types::MarkdownTheme;
pub use types::SegmentKind;

// Re-export utilities used by other modules.
pub use types::truncate_to_display_width;

use std::borrow::Cow;

use code_blocks::{CodeBlockRenderEnv, finalize_unclosed_code_block, handle_code_block_event};
use parsing::{
    LinkState, ListState, MarkdownContext, append_text, handle_end_tag, handle_start_tag,
    inline_code_style, push_blank_line, trim_trailing_blank_lines,
};
use pulldown_cmark::{Event, Options, Parser};
use ratatui::style::Style;
use ratatui::text::Line;
use tables::TableBuffer;

use crate::config::ThemePalette;

/// Render markdown text to ratatui Lines using the given theme palette.
pub fn render_markdown(text: &str, palette: &ThemePalette) -> Vec<Line<'static>> {
    render_markdown_with_width(text, None, palette)
}

/// Render markdown text with an optional terminal width for table column balancing.
///
/// When `width` is `Some`, tables will balance their column widths to fit within
/// the available space, wrapping cell content instead of truncating.
pub fn render_markdown_with_width(
    text: &str,
    width: Option<u16>,
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    render_markdown_lines(text, width, palette)
        .into_iter()
        .map(Line::from)
        .collect()
}

/// Render markdown text to intermediate [`MarkdownLine`]s, preserving each
/// segment's [`SegmentKind`].
///
/// Used by callers that need element-aware post-processing — e.g. the
/// thinking block recolors prose segments while preserving code colors.
pub fn render_markdown_lines(
    text: &str,
    width: Option<u16>,
    palette: &ThemePalette,
) -> Vec<MarkdownLine> {
    let theme = MarkdownTheme::from_palette(palette);
    let base_style = theme.base;

    // Pre-process: ensure code fences are on their own line.
    let text = ensure_fences_on_own_line(text);

    let lines = render_markdown_to_lines(&text, base_style, &theme, width);

    // Pre-wrap prose to the available width using UAX #14 line breaking so CJK
    // runs break at the margin instead of being shoved whole to the next line
    // by ratatui's whitespace-only word wrap. Code/border lines and lines that
    // already fit pass through untouched.
    match width {
        Some(w) => wrap::wrap_prose_lines(lines, w as usize),
        None => lines,
    }
}

/// Ensure fenced code block delimiters (```) are on their own line.
///
/// pulldown-cmark requires ``` to be at line start (up to 3 spaces indent).
/// LLMs sometimes output `text:```python` without a preceding newline,
/// causing the fence to be treated as literal text.
///
/// This preprocessor inserts a newline before ``` if:
/// - It's not already at line start
/// - It looks like a fence (followed by \n, EOF, or alphanumeric lang tag)
fn ensure_fences_on_own_line(text: &str) -> Cow<'_, str> {
    // Fast path: no ``` in text.
    if !text.contains("```") {
        return Cow::Borrowed(text);
    }

    let bytes = text.as_bytes();
    let mut result = String::new();
    let mut last_end = 0;
    let mut needs_alloc = false;

    for (i, _) in text.match_indices("```") {
        // Already at line start — nothing to do.
        if i == 0 || bytes[i - 1] == b'\n' {
            continue;
        }

        // Heuristic: only treat as fence if followed by \n, EOF, or alphanumeric (lang tag).
        let after = &bytes[i + 3..];
        let looks_like_fence =
            after.is_empty() || after[0] == b'\n' || after[0].is_ascii_alphanumeric();

        if !looks_like_fence {
            continue;
        }

        if !needs_alloc {
            result.reserve(text.len() + 16);
            needs_alloc = true;
        }
        result.push_str(&text[last_end..i]);
        result.push('\n');
        last_end = i;
    }

    if needs_alloc {
        result.push_str(&text[last_end..]);
        Cow::Owned(result)
    } else {
        Cow::Borrowed(text)
    }
}

/// Render plain text (no markdown parsing) to lines.
pub fn render_plain(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| Line::from(ratatui::text::Span::from(line.to_string())))
        .collect()
}

/// Internal: render markdown to `Vec<MarkdownLine>`.
fn render_markdown_to_lines(
    source: &str,
    base_style: Style,
    theme: &MarkdownTheme,
    available_width: Option<u16>,
) -> Vec<MarkdownLine> {
    let parser_options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(source, parser_options);

    let mut lines = Vec::new();
    let mut current_line = MarkdownLine::default();
    let mut style_stack = vec![base_style];
    let mut kind_stack = vec![SegmentKind::Text];
    let mut blockquote_depth = 0usize;
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut list_continuation_prefix = String::new();
    let mut pending_list_prefix: Option<String> = None;
    let mut code_block: Option<code_blocks::CodeBlockState> = None;
    let mut active_table: Option<TableBuffer> = None;
    let mut link_state: Option<LinkState> = None;

    for event in parser {
        // Code block events are handled separately.
        let mut code_block_env = CodeBlockRenderEnv {
            lines: &mut lines,
            current_line: &mut current_line,
            blockquote_depth,
            list_continuation_prefix: &list_continuation_prefix,
            pending_list_prefix: &mut pending_list_prefix,
            base_style,
            theme,
        };
        if handle_code_block_event(&event, &mut code_block, &mut code_block_env) {
            blockquote_depth = code_block_env.blockquote_depth;
            continue;
        }

        let mut ctx = MarkdownContext {
            style_stack: &mut style_stack,
            kind_stack: &mut kind_stack,
            blockquote_depth: &mut blockquote_depth,
            list_stack: &mut list_stack,
            list_continuation_prefix: &mut list_continuation_prefix,
            pending_list_prefix: &mut pending_list_prefix,
            lines: &mut lines,
            current_line: &mut current_line,
            theme,
            base_style,
            available_width,
            code_block: &mut code_block,
            active_table: &mut active_table,
            link_state: &mut link_state,
        };

        match event {
            Event::Start(ref tag) => handle_start_tag(tag, &mut ctx),
            Event::End(tag) => handle_end_tag(tag, &mut ctx),
            Event::Text(text) => append_text(&text, &mut ctx),
            Event::Code(code) => {
                ctx.ensure_prefix();
                ctx.current_line.push_segment_with_link(
                    SegmentKind::InlineCode,
                    inline_code_style(theme, base_style),
                    &code,
                    ctx.active_link_target(),
                );
            }
            Event::SoftBreak | Event::HardBreak => ctx.flush_line(),
            Event::Rule => {
                ctx.flush_line();
                let mut line = MarkdownLine::default();
                line.push_segment(SegmentKind::Border, base_style.dim(), &"―".repeat(32));
                ctx.lines.push(line);
                push_blank_line(ctx.lines);
            }
            Event::TaskListMarker(checked) => {
                ctx.ensure_prefix();
                ctx.current_line.push_segment(
                    SegmentKind::Marker,
                    base_style,
                    if checked { "[x] " } else { "[ ] " },
                );
            }
            Event::Html(html) | Event::InlineHtml(html) => append_text(&html, &mut ctx),
            _ => {}
        }
    }

    // Finalize any unclosed code block.
    let mut code_block_env = CodeBlockRenderEnv {
        lines: &mut lines,
        current_line: &mut current_line,
        blockquote_depth,
        list_continuation_prefix: &list_continuation_prefix,
        pending_list_prefix: &mut pending_list_prefix,
        base_style,
        theme,
    };
    finalize_unclosed_code_block(&mut code_block, &mut code_block_env);

    if !current_line.segments.is_empty() {
        lines.push(current_line);
    }

    trim_trailing_blank_lines(&mut lines);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dp() -> ThemePalette {
        ThemePalette::default()
    }

    fn render_text(md: &str) -> Vec<String> {
        render_markdown(md, &dp())
            .into_iter()
            .map(|l| l.to_string())
            .collect()
    }

    fn join_lines(lines: &[String]) -> String {
        lines.join("\n")
    }

    /// Flatten rendered markdown into (kind, text) segment pairs.
    fn segment_pairs(md: &str) -> Vec<(SegmentKind, String)> {
        render_markdown_lines(md, None, &dp())
            .into_iter()
            .flat_map(|line| line.segments)
            .map(|seg| (seg.kind, seg.text))
            .collect()
    }

    fn find_segment(pairs: &[(SegmentKind, String)], needle: &str) -> SegmentKind {
        pairs
            .iter()
            .find(|(_, text)| text.contains(needle))
            .unwrap_or_else(|| panic!("segment containing {needle:?} not found: {pairs:?}"))
            .0
    }

    // ============================================================
    // Segment kind tagging
    // ============================================================

    #[test]
    fn segment_kinds_inline_elements() {
        let pairs = segment_pairs("use `cargo build` and **bold** text");
        assert_eq!(find_segment(&pairs, "cargo build"), SegmentKind::InlineCode);
        assert_eq!(find_segment(&pairs, "bold"), SegmentKind::Text);
        assert_eq!(find_segment(&pairs, "use "), SegmentKind::Text);
    }

    #[test]
    fn segment_kinds_heading() {
        let pairs = segment_pairs("# Title here");
        assert_eq!(find_segment(&pairs, "Title here"), SegmentKind::Heading);
    }

    #[test]
    fn segment_kinds_link() {
        let pairs = segment_pairs("see [docs](https://example.com) now");
        assert_eq!(find_segment(&pairs, "docs"), SegmentKind::Link);
        assert_eq!(
            find_segment(&pairs, "https://example.com"),
            SegmentKind::Link
        );
        assert_eq!(find_segment(&pairs, "see "), SegmentKind::Text);
    }

    #[test]
    fn segment_kinds_code_block() {
        let pairs = segment_pairs("```\nlet x = 1;\n```");
        assert_eq!(find_segment(&pairs, "let x = 1;"), SegmentKind::CodeBlock);
        assert_eq!(find_segment(&pairs, "┌"), SegmentKind::Border);
        assert_eq!(find_segment(&pairs, "└"), SegmentKind::Border);
    }

    #[test]
    fn segment_kinds_list_and_rule() {
        let pairs = segment_pairs("- item one\n\n---");
        assert_eq!(find_segment(&pairs, "•"), SegmentKind::Marker);
        assert_eq!(find_segment(&pairs, "item one"), SegmentKind::Text);
        assert_eq!(find_segment(&pairs, "―"), SegmentKind::Border);
    }

    #[test]
    fn segment_kinds_blockquote_bar() {
        let pairs = segment_pairs("> quoted");
        assert_eq!(find_segment(&pairs, "│"), SegmentKind::Border);
        assert_eq!(find_segment(&pairs, "quoted"), SegmentKind::Text);
    }

    #[test]
    fn segment_kinds_code_inside_link_is_code() {
        // Innermost element wins: code inside a link keeps InlineCode kind.
        let pairs = segment_pairs("[`readme`](https://example.com)");
        assert_eq!(find_segment(&pairs, "readme"), SegmentKind::InlineCode);
    }

    #[test]
    fn segment_kinds_table_preserves_kinds_through_wrap() {
        // Narrow width exercises wrap_cell's word-wrap and hard-break paths;
        // kinds must survive both, plus the render_row passthrough.
        let md = "| Code | Desc |\n|---|---|\n| `snip` then `averylongcodetokenwhichcannotfit` | some long prose content that will wrap over lines |";
        let pairs: Vec<(SegmentKind, String)> = render_markdown_lines(md, Some(40), &dp())
            .into_iter()
            .flat_map(|line| line.segments)
            .map(|seg| (seg.kind, seg.text))
            .collect();

        // Short inline code survives the cell wrap.
        assert_eq!(find_segment(&pairs, "snip"), SegmentKind::InlineCode);

        // The over-wide code token is hard-broken; every fragment must keep
        // the InlineCode kind so the token reconstructs from code segments.
        let code_text: String = pairs
            .iter()
            .filter(|(kind, _)| *kind == SegmentKind::InlineCode)
            .map(|(_, text)| text.as_str())
            .collect();
        assert!(
            code_text.contains("averylongcodetokenwhichcannotfit"),
            "code fragments lost kind during hard break: {pairs:?}"
        );

        // Word-wrapped prose stays Text.
        assert_eq!(find_segment(&pairs, "prose"), SegmentKind::Text);

        // Table rules are Border.
        assert_eq!(find_segment(&pairs, "━"), SegmentKind::Border);
    }

    #[test]
    fn segment_kinds_code_block_gutter() {
        // A language-tagged block gets line numbers (Gutter) next to
        // highlighted code (CodeBlock).
        let pairs = segment_pairs("```rust\nlet x = 1;\n```");
        assert!(
            pairs
                .iter()
                .any(|(kind, text)| *kind == SegmentKind::Gutter && text.contains('1')),
            "line number gutter missing: {pairs:?}"
        );
        assert!(
            pairs
                .iter()
                .any(|(kind, _)| *kind == SegmentKind::CodeBlock),
            "code content missing: {pairs:?}"
        );
    }

    #[test]
    fn heading_renders_text() {
        let lines = render_text("# Title");
        let text = join_lines(&lines);
        assert!(text.contains("Title"), "got: {text}");
    }

    #[test]
    fn paragraph_renders() {
        let lines = render_text("Hello world");
        let text = join_lines(&lines);
        assert!(text.contains("Hello world"), "got: {text}");
    }

    #[test]
    fn bold_renders() {
        let lines = render_text("**bold text**");
        let text = join_lines(&lines);
        assert!(text.contains("bold text"), "got: {text}");
    }

    #[test]
    fn italic_renders() {
        let lines = render_text("*italic text*");
        let text = join_lines(&lines);
        assert!(text.contains("italic text"), "got: {text}");
    }

    #[test]
    fn inline_code_renders() {
        let lines = render_text("Use `cargo build` here");
        let text = join_lines(&lines);
        assert!(text.contains("cargo build"), "got: {text}");
    }

    #[test]
    fn unordered_list_bullets() {
        let lines = render_text("- item1\n- item2");
        let text = join_lines(&lines);
        assert!(text.contains("•"), "should use bullet char, got: {text}");
        assert!(text.contains("item1"), "got: {text}");
        assert!(text.contains("item2"), "got: {text}");
    }

    #[test]
    fn ordered_list_numbers() {
        let lines = render_text("1. first\n2. second");
        let text = join_lines(&lines);
        assert!(text.contains("1. first"), "got: {text}");
        assert!(text.contains("2. second"), "got: {text}");
    }

    #[test]
    fn nested_list_indent() {
        let md = "- parent\n  - child";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("• parent"), "got: {text}");
        // Child should have deeper indent.
        assert!(text.contains("child"), "got: {text}");
    }

    #[test]
    fn blockquote_prefix() {
        let lines = render_text("> quoted text");
        let text = join_lines(&lines);
        assert!(
            text.contains("│ "),
            "blockquote should have │ prefix, got: {text}"
        );
        assert!(text.contains("quoted text"), "got: {text}");
    }

    #[test]
    fn link_shows_url() {
        let lines = render_text("[click](https://example.com)");
        let text = join_lines(&lines);
        assert!(text.contains("click"), "got: {text}");
        assert!(
            text.contains("https://example.com"),
            "URL should be shown, got: {text}"
        );
    }

    #[test]
    fn horizontal_rule() {
        let lines = render_text("---");
        let text = join_lines(&lines);
        assert!(text.contains("―"), "should have rule chars, got: {text}");
    }

    #[test]
    fn code_block_borders() {
        let lines = render_text("```rust\nfn main() {}\n```");
        let text = join_lines(&lines);
        assert!(text.contains("┌"), "should have top border, got: {text}");
        assert!(text.contains("└"), "should have bottom border, got: {text}");
        assert!(text.contains("fn main()"), "got: {text}");
    }

    #[test]
    fn table_borderless() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let lines = render_text(md);
        let text = join_lines(&lines);
        // Borderless style uses a heavy header rule, not vertical bars.
        assert!(
            text.contains("━"),
            "should use heavy header rule, got: {text}"
        );
        assert!(!text.contains("│"), "should be borderless, got: {text}");
        assert!(text.contains("A"), "got: {text}");
        assert!(text.contains("1"), "got: {text}");
    }

    #[test]
    fn table_header_separator() {
        let md = "| H1 | H2 |\n|----|----|\n| a  | b  |";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(
            text.contains("━"),
            "should have heavy header separator, got: {text}"
        );
    }

    #[test]
    fn strikethrough_renders() {
        let lines = render_text("~~deleted~~");
        let text = join_lines(&lines);
        assert!(text.contains("deleted"), "got: {text}");
    }

    #[test]
    fn task_list_markers() {
        let lines = render_text("- [x] done\n- [ ] todo");
        let text = join_lines(&lines);
        assert!(text.contains("[x]"), "got: {text}");
        assert!(text.contains("[ ]"), "got: {text}");
    }

    #[test]
    fn empty_input() {
        let lines = render_text("");
        assert!(lines.is_empty() || lines.iter().all(|l| l.trim().is_empty()));
    }

    #[test]
    fn plain_text_renders() {
        let lines = render_plain("hello\nworld");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "hello");
        assert_eq!(lines[1].to_string(), "world");
    }

    #[test]
    fn diff_code_block_coloring() {
        let md = "```diff\n+ added line\n- removed line\n context line\n```";
        let lines = render_markdown(md, &dp());
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("added line"), "got: {text}");
        assert!(text.contains("removed line"), "got: {text}");
        assert!(text.contains("context line"), "got: {text}");
    }

    #[test]
    fn code_block_with_syntax_has_line_numbers() {
        let md = "```rust\nlet x = 1;\nlet y = 2;\n```";
        let lines = render_markdown(md, &dp());
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Should contain line numbers (1 and 2).
        assert!(text.contains("1"), "should have line number 1: {text}");
        assert!(text.contains("2"), "should have line number 2: {text}");
    }

    #[test]
    fn table_with_width_renders() {
        let md = "| A | B | C |\n|---|---|---|\n| 1 | 2 | 3 |";
        let lines = render_markdown_with_width(md, Some(80), &dp());
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Borderless style: heavy header rule, no vertical bars.
        assert!(text.contains("━"), "got: {text}");
        assert!(!text.contains("│"), "should be borderless, got: {text}");
        assert!(text.contains("A"), "got: {text}");
    }

    // ============================================================
    // Tests ported from VTCode (MIT license) — precise behavioral checks
    // ============================================================

    #[test]
    fn table_header_separator_and_rows() {
        let md = "| File | Line | Function |\n|------|------|----------|\n| src/main.rs | 10 | main |\n| src/lib.rs | 20 | init |\n";
        let lines = render_text(md);
        let non_blank: Vec<&str> = lines
            .iter()
            .map(|s| s.as_str())
            .filter(|l| !l.is_empty())
            .collect();
        // Layout: header, heavy rule, row0, light rule, row1.
        assert!(
            non_blank.len() >= 5,
            "expected header + rule + row + rule + row, got: {non_blank:?}"
        );
        assert!(non_blank[0].contains("File") && non_blank[0].contains("Function"));
        assert!(non_blank[1].contains("━"), "header rule: {}", non_blank[1]);
        assert!(non_blank[2].contains("src/main.rs"));
        assert!(non_blank[3].contains("─"), "body rule: {}", non_blank[3]);
        assert!(non_blank[4].contains("src/lib.rs"));
    }

    #[test]
    fn unordered_list_uses_unicode_bullets() {
        let md = "- Item 1\n- Item 2\n  - Nested 1\n  - Nested 2\n- Item 3\n";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(
            text.contains('•') || text.contains('◦') || text.contains('▪'),
            "should use Unicode bullet characters, got: {text}"
        );
    }

    #[test]
    fn nested_list_no_extra_blank_lines() {
        let md = "1. **Header**:\n   - Sub item 1\n   - Sub item 2\n\n2. **Another**:\n   - Sub item 3\n";
        let lines = render_text(md);
        // Count blank lines (empty segments)
        let blank_count = lines
            .iter()
            .filter(|l| l.to_string().trim().is_empty())
            .count();
        // Should have at most 1 blank line (between the two top-level items)
        assert!(
            blank_count <= 2,
            "too many blank lines ({blank_count}), output:\n{}",
            lines
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{i}: |{l}|"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn soft_break_renders_line_break() {
        let md = "first line\nsecond line";
        let lines = render_text(md);
        let non_blank: Vec<String> = lines.into_iter().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(non_blank.len(), 2, "got: {non_blank:?}");
        assert!(non_blank[0].contains("first line"));
        assert!(non_blank[1].contains("second line"));
    }

    #[test]
    fn inline_code_strips_backticks() {
        let md = "Use `code` here.";
        let lines = render_text(md);
        let text = join_lines(&lines);
        // The text "code" should appear without backticks.
        assert!(text.contains("code"), "got: {text}");
        assert!(
            !text.contains("`code`"),
            "backticks should be stripped, got: {text}"
        );
    }

    #[test]
    fn nested_list_different_bullet_depth() {
        // Use 4-space indent to ensure all parsers recognize nesting.
        let md = "- depth0\n    - depth1\n";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains('•'), "depth0 bullet missing: {text}");
        assert!(text.contains('◦'), "depth1 bullet missing: {text}");
    }

    #[test]
    fn ordered_list_sequential_numbers() {
        let md = "1. first\n2. second\n3. third\n";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("1."), "got: {text}");
        assert!(text.contains("2."), "got: {text}");
        assert!(text.contains("3."), "got: {text}");
    }

    #[test]
    fn blockquote_nested_depth() {
        let md = "> outer\n>> inner\n";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("│ "), "outer quote prefix missing: {text}");
        assert!(text.contains("inner"), "inner text missing: {text}");
    }

    #[test]
    fn link_local_path_hides_url() {
        let md = "[file](./src/main.rs)";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("file"), "link text missing: {text}");
        assert!(
            !text.contains("./src/main.rs"),
            "local path should be hidden: {text}"
        );
    }

    #[test]
    fn link_remote_shows_url() {
        let md = "[click](https://example.com)";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("click"), "got: {text}");
        assert!(
            text.contains("https://example.com"),
            "URL should be shown: {text}"
        );
    }

    #[test]
    fn diff_code_block_has_summary() {
        let md =
            "```diff\ndiff --git a/file.rs b/file.rs\n@@ -1,3 +1,3 @@\n-old\n+new\n context\n```";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("▸ Edit"), "diff summary missing: {text}");
        assert!(text.contains("file.rs"), "file path missing: {text}");
        assert!(text.contains("+1"), "additions missing: {text}");
        assert!(text.contains("-1"), "deletions missing: {text}");
    }

    #[test]
    fn table_width_balances_columns() {
        let md = "| Short | A very long description column that would normally overflow |\n|---|---|\n| x | Some long content here too that wraps |";
        let lines = render_markdown_with_width(md, Some(50), &dp());
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // All content must be present (no truncation).
        assert!(text.contains("Short"), "got: {text}");
        assert!(text.contains("overflow"), "long content must wrap: {text}");
        assert!(text.contains("wraps"), "cell content must wrap: {text}");
    }

    #[test]
    fn multiple_paragraphs_separated() {
        let md = "First paragraph.\n\nSecond paragraph.";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("First paragraph."), "got: {text}");
        assert!(text.contains("Second paragraph."), "got: {text}");
    }

    // ============================================================
    // Fence preprocessor tests
    // ============================================================

    #[test]
    fn fence_no_newline_gets_fixed() {
        let input = "text:```python\ncode\n```";
        let output = ensure_fences_on_own_line(input);
        assert_eq!(output.as_ref(), "text:\n```python\ncode\n```");
    }

    #[test]
    fn fence_already_correct_no_alloc() {
        let input = "text:\n```python\ncode\n```";
        let output = ensure_fences_on_own_line(input);
        assert!(matches!(output, Cow::Borrowed(_)));
    }

    #[test]
    fn fence_closing_stuck_to_text() {
        let input = "code here```\nnext paragraph";
        let output = ensure_fences_on_own_line(input);
        assert_eq!(output.as_ref(), "code here\n```\nnext paragraph");
    }

    #[test]
    fn backticks_not_a_fence() {
        // ``` followed by space — not a fence, should NOT insert newline.
        let input = "use ``` for code blocks";
        let output = ensure_fences_on_own_line(input);
        assert!(matches!(output, Cow::Borrowed(_)));
    }

    #[test]
    fn no_backticks_fast_path() {
        let input = "just plain text without any code";
        let output = ensure_fences_on_own_line(input);
        assert!(matches!(output, Cow::Borrowed(_)));
    }

    #[test]
    fn fence_at_start_of_text() {
        let input = "```python\ncode\n```";
        let output = ensure_fences_on_own_line(input);
        assert!(matches!(output, Cow::Borrowed(_)));
    }

    #[test]
    fn render_with_fence_fix() {
        // End-to-end: fence without newline should render as code block.
        let md = "text:```python\nlet x = 1;\n```";
        let lines = render_markdown(md, &dp());
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Should NOT contain ``` as visible text.
        assert!(!text.contains("```python"), "fence leaked as text: {text}");
        // Should contain code block border.
        assert!(text.contains("┌"), "missing code block border: {text}");
        assert!(text.contains("let x = 1"), "code content missing: {text}");
    }
}
