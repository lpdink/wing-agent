//! Code block handling — state management, syntax highlighting, and rendering.
//!
//! Adapted from VTCode (MIT license). Simplified diff rendering;
//! full syntect highlighting is in `super::syntax`.

use ratatui::style::Style;

use super::parsing::{flush_current_line, push_blank_line};
use super::types::{MarkdownLine, MarkdownSegment, MarkdownTheme, SegmentKind};
use crate::render::diff_highlight::DiffHighlighters;
use crate::render::diff_highlight::DiffSide;
use crate::render::syntax::highlight_code_lines;

/// Tracks the state of an in-progress fenced code block.
#[derive(Clone, Debug)]
pub(crate) struct CodeBlockState {
    pub(crate) language: Option<String>,
    pub(crate) buffer: String,
}

/// Environment for rendering code block content.
pub(crate) struct CodeBlockRenderEnv<'a> {
    pub(crate) lines: &'a mut Vec<MarkdownLine>,
    pub(crate) current_line: &'a mut MarkdownLine,
    pub(crate) blockquote_depth: usize,
    pub(crate) list_continuation_prefix: &'a str,
    pub(crate) pending_list_prefix: &'a mut Option<String>,
    pub(crate) base_style: Style,
    pub(crate) theme: &'a MarkdownTheme,
    /// Available render width — diff rows pad their tinted background to it.
    /// None = unknown (no padding).
    pub(crate) width: Option<u16>,
    /// Render code with syntect highlighting + gutters. False = plain
    /// single-color code (the Thinking profile).
    pub(crate) highlight: bool,
}

/// Handle an event while inside a code block.
///
/// Returns `true` if the event was consumed (caller should skip further processing).
pub(crate) fn handle_code_block_event(
    event: &pulldown_cmark::Event<'_>,
    code_block: &mut Option<CodeBlockState>,
    env: &mut CodeBlockRenderEnv<'_>,
) -> bool {
    if code_block.is_none() {
        return false;
    }

    match event {
        pulldown_cmark::Event::Text(text) => {
            if let Some(state) = code_block.as_mut() {
                state.buffer.push_str(text);
            }
            true
        }
        pulldown_cmark::Event::End(pulldown_cmark::TagEnd::CodeBlock) => {
            finalize_code_block(env, code_block, false);
            true
        }
        _ => false,
    }
}

/// Finalize an unclosed code block (e.g., at end of input).
pub(crate) fn finalize_unclosed_code_block(
    code_block: &mut Option<CodeBlockState>,
    env: &mut CodeBlockRenderEnv<'_>,
) {
    finalize_code_block(env, code_block, false);
}

fn finalize_code_block(
    env: &mut CodeBlockRenderEnv<'_>,
    code_block: &mut Option<CodeBlockState>,
    append_trailing_blank_line: bool,
) {
    flush_current_line(
        env.lines,
        env.current_line,
        env.blockquote_depth,
        env.list_continuation_prefix,
        env.pending_list_prefix,
        env.base_style,
    );
    if let Some(state) = code_block.take() {
        let rendered = render_code_block(&state, env);
        env.lines.extend(rendered);
        if append_trailing_blank_line {
            push_blank_line(env.lines);
        }
    }
}

/// Render a complete code block with border, optional syntax highlighting,
/// and optional line numbers.
fn render_code_block(state: &CodeBlockState, env: &CodeBlockRenderEnv<'_>) -> Vec<MarkdownLine> {
    let mut lines = Vec::new();
    let border_style = env.theme.border;
    let has_language = state
        .language
        .as_ref()
        .is_some_and(|lang| !lang.trim().is_empty());

    // Top border.
    let label = if let Some(ref lang) = state.language
        && has_language
    {
        format!("┌─ {lang} ─")
    } else {
        "┌────────".to_string()
    };
    lines.push(MarkdownLine {
        segments: vec![MarkdownSegment::new(
            SegmentKind::Border,
            border_style,
            label,
        )],
    });

    // Determine if this is a diff block.
    let is_diff = state
        .language
        .as_ref()
        .is_some_and(|lang| lang.eq_ignore_ascii_case("diff"));

    // Render code lines.
    let code = state.buffer.trim_end_matches('\n');
    let source_lines: Vec<&str> = code.lines().collect();
    let line_count = source_lines.len();
    let show_line_numbers = has_language && !is_diff && env.highlight;

    let number_width = if show_line_numbers {
        line_count.max(1).to_string().len().max(3)
    } else {
        0
    };

    // Try syntax highlighting if we have a language.
    let highlighted = if has_language && !is_diff && env.highlight {
        let lang = state.language.as_deref().unwrap_or("");
        let ext = lang.split_whitespace().next().unwrap_or(lang);
        highlight_code_lines(code, Some(ext), env.theme)
    } else {
        None
    };

    if let Some(highlighted_lines) = highlighted {
        for (i, hl_line) in highlighted_lines.into_iter().enumerate() {
            let mut line = build_prefix(env);
            if show_line_numbers {
                let num = format!("{:>width$}  ", i + 1, width = number_width);
                line.push_segment(SegmentKind::Gutter, env.theme.code_block_gutter, &num);
            }
            line.push_segment(SegmentKind::Border, border_style, "│ ");
            for seg in hl_line.segments {
                line.segments.push(seg);
            }
            lines.push(line);
        }
    } else if is_diff {
        // Diff coloring: file summaries + one pass per side so the code keeps
        // its syntax colors under a tinted add/delete row background.
        let diff_blocks = group_diff_by_file(&source_lines);
        let highlight = env.highlight;

        for block in &diff_blocks {
            // Emit file summary line if we have a path.
            if let Some(ref path) = block.path {
                let mut line = build_prefix(env);
                line.push_segment(SegmentKind::Border, border_style, "│ ");
                let summary = format!("▸ Edit {path} (+{} -{})", block.additions, block.deletions);
                line.push_segment(SegmentKind::CodeBlock, env.theme.diff_hunk, &summary);
                lines.push(line);
            }

            // One old/new highlighter pair for this file: a hunk can sit
            // inside a construct that only exists in one revision, and
            // context lines belong to both (so they advance both).
            let mut highlighters = highlight.then(|| {
                DiffHighlighters::for_file(block.path.as_deref().unwrap_or_default(), None)
            });

            // Emit diff lines.
            for src_line in &block.lines {
                let mut line = build_prefix(env);
                let row = split_diff_row(src_line);

                // Hunk header keeps its own (uncolored, unfit for tint) look.
                if row.marker == Some('@') {
                    line.push_segment(SegmentKind::Border, border_style, "│ ");
                    line.push_segment(SegmentKind::CodeBlock, env.theme.diff_hunk, row.content);
                    lines.push(line);
                    continue;
                }

                let (tint, marker_style) = match row.side {
                    Some(DiffSide::Insert) => (env.theme.diff_add_bg, env.theme.diff_add),
                    Some(DiffSide::Delete) => (env.theme.diff_del_bg, env.theme.diff_del),
                    _ => (Style::default(), env.theme.code_block),
                };
                let tinted = matches!(row.side, Some(DiffSide::Insert | DiffSide::Delete));

                line.push_segment(SegmentKind::Border, border_style.patch(tint), "│ ");
                if let Some(marker) = row.marker {
                    line.push_segment(
                        SegmentKind::CodeBlock,
                        marker_style.patch(tint),
                        &format!("{marker} "),
                    );
                }
                let styled = highlighters.as_mut().and_then(|hl| {
                    hl.line(row.side.unwrap_or(DiffSide::Context), row.content, true)
                });
                match styled {
                    Some(spans) => {
                        for (style, text) in spans {
                            line.push_segment(SegmentKind::CodeBlock, style.patch(tint), &text);
                        }
                    }
                    None => {
                        line.push_segment(
                            SegmentKind::CodeBlock,
                            env.theme.code_block.patch(tint),
                            row.content,
                        );
                    }
                }

                // Rows tinted as a band: pad so the background spans the block.
                if tinted && let Some(width) = env.width {
                    pad_to_width(&mut line, width as usize, tint);
                }
                lines.push(line);
            }
        }
    } else {
        // Plain code block.
        for (i, src_line) in source_lines.iter().enumerate() {
            let mut line = build_prefix(env);
            if show_line_numbers {
                let num = format!("{:>width$}  ", i + 1, width = number_width);
                line.push_segment(SegmentKind::Gutter, env.theme.code_block_gutter, &num);
            }
            line.push_segment(SegmentKind::Border, border_style, "│ ");
            line.push_segment(SegmentKind::CodeBlock, env.theme.code_block, src_line);
            lines.push(line);
        }
    }

    // Bottom border.
    lines.push(MarkdownLine {
        segments: vec![MarkdownSegment::new(
            SegmentKind::Border,
            border_style,
            "└────────",
        )],
    });

    lines
}

/// Group diff lines by file, counting additions/deletions per file.
struct DiffFileBlock<'a> {
    path: Option<String>,
    lines: Vec<&'a str>,
    additions: usize,
    deletions: usize,
}

fn group_diff_by_file<'a>(source_lines: &[&'a str]) -> Vec<DiffFileBlock<'a>> {
    let mut blocks: Vec<DiffFileBlock<'a>> = Vec::new();
    let mut current: Option<DiffFileBlock<'a>> = None;

    for &src_line in source_lines {
        let trimmed = src_line.trim_start();

        // Detect `diff --git a/path b/path`
        if let Some(path) = trimmed.strip_prefix("diff --git ") {
            // Flush previous block.
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            // Extract the b/ path (second half after space).
            let file_path = path
                .rsplit_once(' ')
                .map(|(_, b)| b.strip_prefix("b/").unwrap_or(b).to_string())
                .unwrap_or_else(|| path.to_string());
            current = Some(DiffFileBlock {
                path: Some(file_path),
                lines: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            continue;
        }

        // Skip metadata lines (index, ---, +++, new file mode, etc.)
        if trimmed.starts_with('\\')
            || trimmed.starts_with("index ")
            || trimmed.starts_with("---")
            || trimmed.starts_with("+++")
            || trimmed.starts_with("new file")
            || trimmed.starts_with("deleted file")
            || trimmed.starts_with("rename ")
            || trimmed.starts_with("similarity ")
            || trimmed.starts_with("Binary files")
        {
            continue;
        }

        let block = current.get_or_insert(DiffFileBlock {
            path: None,
            lines: Vec::new(),
            additions: 0,
            deletions: 0,
        });

        block.lines.push(src_line);
        if trimmed.starts_with('+') {
            block.additions += 1;
        } else if trimmed.starts_with('-') {
            block.deletions += 1;
        }
    }

    if let Some(block) = current {
        blocks.push(block);
    }

    // If no file boundaries detected, return everything as one block.
    if blocks.is_empty() && !source_lines.is_empty() {
        let mut block = DiffFileBlock {
            path: None,
            lines: Vec::new(),
            additions: 0,
            deletions: 0,
        };
        for &src_line in source_lines {
            let trimmed = src_line.trim_start();
            block.lines.push(src_line);
            if trimmed.starts_with('+') {
                block.additions += 1;
            } else if trimmed.starts_with('-') {
                block.deletions += 1;
            }
        }
        blocks.push(block);
    }

    blocks
}

/// One unified-diff line, split into its gutter marker and the code it
/// carries.
struct DiffRow<'a> {
    /// Leading marker as written (`+`, `-`, ` `); `@` for a hunk header.
    marker: Option<char>,
    /// Revision(s) the line belongs to — `None` when the line is not code
    /// (hunk header, unrecognized line).
    side: Option<DiffSide>,
    /// Code without the marker, or the whole line for hunk headers.
    content: &'a str,
}

/// Split a unified-diff line into marker, revision, and content.
///
/// `@@ …` opens a hunk header (indented fences included) — but only when
/// doubled: a context line can start with a single `@` (decorators,
/// annotations), and that is ordinary code.
///
/// `git diff` puts the marker in column 0, and a leading **space is the
/// context marker**, so it wins over the indented-marker fallback. That
/// fallback exists for LLM-written fences that indent the whole block: there
/// a `+`/`-` follows the indentation. Anything else is rendered as-is.
fn split_diff_row(line: &str) -> DiffRow<'_> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    let marker_at = |at: usize| match line.as_bytes().get(at) {
        Some(&b'+') => Some('+'),
        Some(&b'-') => Some('-'),
        Some(&b' ') => Some(' '),
        _ => None,
    };

    if trimmed.starts_with("@@") {
        return DiffRow {
            marker: Some('@'),
            side: None,
            content: trimmed,
        };
    }

    let at = if marker_at(0).is_some_and(|marker| marker != ' ') {
        0
    } else if indent > 0 && matches!(trimmed.as_bytes().first(), Some(b'+' | b'-')) {
        indent
    } else if line.starts_with(' ') {
        0
    } else {
        return DiffRow {
            marker: None,
            side: None,
            content: line,
        };
    };

    let marker = line.as_bytes()[at] as char;
    DiffRow {
        marker: Some(marker),
        side: side_of(marker),
        content: &line[at + 1..],
    }
}

fn side_of(marker: char) -> Option<DiffSide> {
    match marker {
        '+' => Some(DiffSide::Insert),
        '-' => Some(DiffSide::Delete),
        ' ' => Some(DiffSide::Context),
        _ => None,
    }
}

/// Pad a diff row with background-tinted spaces so the band spans the block.
fn pad_to_width(line: &mut MarkdownLine, width: usize, tint: Style) {
    let used = line.width();
    if used < width {
        line.push_segment(SegmentKind::CodeBlock, tint, &" ".repeat(width - used));
    }
}

/// Build the prefix segments for a code block line (blockquote + list prefix).
fn build_prefix(env: &CodeBlockRenderEnv<'_>) -> MarkdownLine {
    let mut line = MarkdownLine::default();
    for _ in 0..env.blockquote_depth {
        line.push_segment(SegmentKind::Border, env.base_style.dim().italic(), "│ ");
    }
    if !env.list_continuation_prefix.is_empty() {
        line.push_segment(
            SegmentKind::Marker,
            env.base_style,
            env.list_continuation_prefix,
        );
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_diff_single_file() {
        let lines = vec![
            "diff --git a/main.rs b/main.rs",
            "@@ -1,3 +1,3 @@",
            "-old",
            "+new",
            " context",
        ];
        let blocks = group_diff_by_file(&lines);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].path.as_deref(), Some("main.rs"));
        assert_eq!(blocks[0].additions, 1);
        assert_eq!(blocks[0].deletions, 1);
        assert_eq!(blocks[0].lines.len(), 4); // hunk header + -old + +new + context
    }

    #[test]
    fn group_diff_multiple_files() {
        let lines = vec![
            "diff --git a/a.rs b/a.rs",
            "+add_a",
            "diff --git a/b.rs b/b.rs",
            "-del_b",
        ];
        let blocks = group_diff_by_file(&lines);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].path.as_deref(), Some("a.rs"));
        assert_eq!(blocks[0].additions, 1);
        assert_eq!(blocks[0].deletions, 0);
        assert_eq!(blocks[1].path.as_deref(), Some("b.rs"));
        assert_eq!(blocks[1].additions, 0);
        assert_eq!(blocks[1].deletions, 1);
    }

    #[test]
    fn group_diff_no_file_boundary() {
        let lines = vec!["+added", "-removed", " context"];
        let blocks = group_diff_by_file(&lines);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].path.is_none());
        assert_eq!(blocks[0].additions, 1);
        assert_eq!(blocks[0].deletions, 1);
    }

    #[test]
    fn group_diff_skips_metadata() {
        let lines = vec![
            "diff --git a/f.rs b/f.rs",
            "index abc123..def456",
            "--- a/f.rs",
            "+++ b/f.rs",
            "@@ -1 +1 @@",
            "-old",
            "+new",
        ];
        let blocks = group_diff_by_file(&lines);
        assert_eq!(blocks.len(), 1);
        // Only hunk header + change lines, metadata stripped.
        assert_eq!(blocks[0].lines.len(), 3);
    }

    fn classify(line: &str) -> (Option<char>, Option<DiffSide>, &str) {
        let row = split_diff_row(line);
        (row.marker, row.side, row.content)
    }

    #[test]
    fn split_diff_row_markers() {
        // git layout: the marker sits in column 0.
        assert_eq!(
            classify("+added"),
            (Some('+'), Some(DiffSide::Insert), "added")
        );
        assert_eq!(
            classify("-gone"),
            (Some('-'), Some(DiffSide::Delete), "gone")
        );
        assert_eq!(
            classify(" ctx"),
            (Some(' '), Some(DiffSide::Context), "ctx")
        );
        // A hunk header needs `@@`; hunk headers are not code rows.
        assert_eq!(
            classify("@@ -1,3 +1,3 @@"),
            (Some('@'), None, "@@ -1,3 +1,3 @@")
        );
        // Decorators / annotations are ordinary context lines.
        assert_eq!(
            classify(" @pytest.fixture"),
            (Some(' '), Some(DiffSide::Context), "@pytest.fixture")
        );
        assert_eq!(classify("@pytest.fixture"), (None, None, "@pytest.fixture"));
        // Indented fence: the marker sits after the indentation.
        assert_eq!(
            classify("    +  added"),
            (Some('+'), Some(DiffSide::Insert), "  added")
        );
        assert_eq!(
            classify("    -gone"),
            (Some('-'), Some(DiffSide::Delete), "gone")
        );
        assert_eq!(
            classify("    @@ -1 +1 @@"),
            (Some('@'), None, "@@ -1 +1 @@")
        );
        // …while a leading space stays the context marker of git's layout,
        // keeping the code's own indentation.
        assert_eq!(
            classify("     let x = 1;"),
            (Some(' '), Some(DiffSide::Context), "    let x = 1;")
        );
        // Unmarked line — kept verbatim (rendered as code without a marker).
        assert_eq!(
            classify("\\ No newline at end of file"),
            (None, None, "\\ No newline at end of file")
        );
    }

    #[test]
    fn group_diff_empty() {
        let lines: Vec<&str> = vec![];
        let blocks = group_diff_by_file(&lines);
        assert!(blocks.is_empty());
    }
}
