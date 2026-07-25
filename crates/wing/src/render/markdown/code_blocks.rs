//! Code block handling — state management, syntax highlighting, and rendering.
//!
//! Adapted from VTCode (MIT license). Simplified diff rendering;
//! full syntect highlighting is in `super::syntax`.

use ratatui::style::Style;

use super::parsing::{flush_current_line, push_blank_line};
use super::types::{MarkdownLine, MarkdownSegment, MarkdownTheme, SegmentKind};
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
    let show_line_numbers = has_language && !is_diff;

    let number_width = if show_line_numbers {
        line_count.max(1).to_string().len().max(3)
    } else {
        0
    };

    // Try syntax highlighting if we have a language.
    let highlighted = if has_language && !is_diff {
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
        // Diff coloring with file summary detection.
        let diff_blocks = group_diff_by_file(&source_lines);

        for block in &diff_blocks {
            // Emit file summary line if we have a path.
            if let Some(ref path) = block.path {
                let mut line = build_prefix(env);
                line.push_segment(SegmentKind::Border, border_style, "│ ");
                let summary = format!("▸ Edit {path} (+{} -{})", block.additions, block.deletions);
                line.push_segment(SegmentKind::CodeBlock, env.theme.diff_hunk, &summary);
                lines.push(line);
            }

            // Emit diff lines.
            for src_line in &block.lines {
                let mut line = build_prefix(env);
                line.push_segment(SegmentKind::Border, border_style, "│ ");
                let trimmed = src_line.trim_start();
                let style = if trimmed.is_empty() {
                    env.theme.code_block
                } else if trimmed.starts_with('+') {
                    env.theme.diff_add
                } else if trimmed.starts_with('-') {
                    env.theme.diff_del
                } else if trimmed.starts_with("@@") {
                    env.theme.diff_hunk
                } else {
                    env.theme.dimmed
                };
                line.push_segment(SegmentKind::CodeBlock, style, src_line);
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
        if trimmed.starts_with("index ")
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

    #[test]
    fn group_diff_empty() {
        let lines: Vec<&str> = vec![];
        let blocks = group_diff_by_file(&lines);
        assert!(blocks.is_empty());
    }
}
