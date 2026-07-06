//! Core markdown event handling — start/end tags, text, list, blockquote.
//!
//! Adapted from VTCode (MIT license). Uses `ratatui::style::Style` throughout.

use std::cmp::max;

use pulldown_cmark::{CodeBlockKind, HeadingLevel, Tag, TagEnd};

use super::code_blocks::CodeBlockState;
use super::links;
use super::tables::{TableBuffer, render_table};
use super::types::{MarkdownLine, MarkdownTheme};

use ratatui::style::Style;

pub(crate) const LIST_INDENT_WIDTH: usize = 2;

// ============================================================
// List state
// ============================================================

#[derive(Clone, Debug)]
pub(crate) struct ListState {
    pub(crate) kind: ListKind,
    pub(crate) depth: usize,
    pub(crate) continuation: String,
}

#[derive(Clone, Debug)]
pub(crate) enum ListKind {
    Unordered,
    Ordered { next: usize },
}

// ============================================================
// Link state
// ============================================================

#[derive(Clone, Debug)]
pub(crate) struct LinkState {
    pub(crate) destination: String,
    pub(crate) show_destination: bool,
    pub(crate) hidden_location_suffix: Option<String>,
    pub(crate) label_start_segment_idx: usize,
}

// ============================================================
// MarkdownContext — mutable state passed through event handlers
// ============================================================

pub(crate) struct MarkdownContext<'a> {
    pub(crate) style_stack: &'a mut Vec<Style>,
    pub(crate) blockquote_depth: &'a mut usize,
    pub(crate) list_stack: &'a mut Vec<ListState>,
    pub(crate) list_continuation_prefix: &'a mut String,
    pub(crate) pending_list_prefix: &'a mut Option<String>,
    pub(crate) lines: &'a mut Vec<MarkdownLine>,
    pub(crate) current_line: &'a mut MarkdownLine,
    pub(crate) theme: &'a MarkdownTheme,
    pub(crate) base_style: Style,
    pub(crate) available_width: Option<u16>,
    pub(crate) code_block: &'a mut Option<CodeBlockState>,
    pub(crate) active_table: &'a mut Option<TableBuffer>,
    pub(crate) link_state: &'a mut Option<LinkState>,
}

impl MarkdownContext<'_> {
    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or(self.base_style)
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    pub(crate) fn flush_line(&mut self) {
        flush_current_line(
            self.lines,
            self.current_line,
            *self.blockquote_depth,
            self.list_continuation_prefix,
            self.pending_list_prefix,
            self.base_style,
        );
    }

    fn flush_paragraph(&mut self) {
        self.flush_line();
        // Don't push blank line inside list items — nested lists and items
        // handle their own spacing via flush_line + set_pending_list_continuation.
        if self.list_stack.is_empty() {
            push_blank_line(self.lines);
        }
    }

    pub(crate) fn ensure_prefix(&mut self) {
        ensure_prefix(
            self.current_line,
            *self.blockquote_depth,
            self.list_continuation_prefix,
            self.pending_list_prefix,
            self.base_style,
        );
    }

    fn refresh_list_continuation_prefix(&mut self) {
        rebuild_list_continuation_prefix(self.list_stack, self.list_continuation_prefix);
    }

    fn set_pending_list_continuation(&mut self) {
        if let Some(state) = self.list_stack.last() {
            *self.pending_list_prefix = Some(state.continuation.clone());
        }
    }

    pub(crate) fn active_link_target(&self) -> Option<String> {
        self.link_state
            .as_ref()
            .map(|link| link.destination.clone())
    }
}

// ============================================================
// Start tag handler
// ============================================================

pub(crate) fn handle_start_tag(tag: &Tag<'_>, ctx: &mut MarkdownContext<'_>) {
    match tag {
        Tag::Paragraph => {}
        Tag::Heading { level, .. } => {
            let style = heading_style(*level, ctx.theme);
            ctx.push_style(style);
            ctx.ensure_prefix();
        }
        Tag::BlockQuote(_) => {
            *ctx.blockquote_depth += 1;
        }
        Tag::List(start) => {
            // Flush accumulated text before entering nested list.
            if !ctx.current_line.segments.is_empty() {
                ctx.flush_line();
            }
            let depth = ctx.list_stack.len();
            let kind = start
                .map(|v| ListKind::Ordered {
                    next: max(1, v as usize),
                })
                .unwrap_or(ListKind::Unordered);
            ctx.list_stack.push(ListState {
                kind,
                depth,
                continuation: String::new(),
            });
            ctx.refresh_list_continuation_prefix();
        }
        Tag::Item => {
            if let Some(state) = ctx.list_stack.last_mut() {
                let indent = " ".repeat(state.depth * LIST_INDENT_WIDTH);
                match &mut state.kind {
                    ListKind::Unordered => {
                        let bullet = match state.depth % 3 {
                            0 => "•",
                            1 => "◦",
                            _ => "▪",
                        };
                        let marker = format!("{indent}{bullet} ");
                        state.continuation = format!("{indent}  ");
                        *ctx.pending_list_prefix = Some(marker);
                    }
                    ListKind::Ordered { next } => {
                        let marker = format!("{indent}{}. ", *next);
                        let width = marker.len().saturating_sub(indent.len());
                        state.continuation = format!("{indent}{}", " ".repeat(width));
                        *ctx.pending_list_prefix = Some(marker);
                        *next += 1;
                    }
                }
                ctx.refresh_list_continuation_prefix();
            }
        }
        Tag::Emphasis => ctx.push_style(ctx.current_style().italic()),
        Tag::Strong => ctx.push_style(ctx.current_style().bold()),
        Tag::Strikethrough => ctx.push_style(ctx.current_style().crossed_out()),
        Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
            let show_destination = links::should_render_link_destination(dest_url);
            let label_start_segment_idx = ctx.current_line.segments.len();
            *ctx.link_state = Some(LinkState {
                destination: dest_url.to_string(),
                show_destination,
                hidden_location_suffix: links::extract_hidden_location_suffix(dest_url),
                label_start_segment_idx,
            });
            ctx.push_style(ctx.theme.link);
        }
        Tag::CodeBlock(kind) => {
            let language = match kind {
                CodeBlockKind::Fenced(info) => info
                    .split_whitespace()
                    .next()
                    .filter(|lang| !lang.is_empty())
                    .map(|lang| lang.to_string()),
                CodeBlockKind::Indented => None,
            };
            *ctx.code_block = Some(CodeBlockState {
                language,
                buffer: String::new(),
            });
        }
        Tag::Table(_) => {
            ctx.flush_paragraph();
            *ctx.active_table = Some(TableBuffer::default());
        }
        Tag::TableRow => {
            if let Some(table) = ctx.active_table.as_mut() {
                table.current_row.clear();
            } else {
                ctx.flush_line();
            }
        }
        Tag::TableHead => {
            if let Some(table) = ctx.active_table.as_mut() {
                table.in_head = true;
                table.current_row.clear();
            }
        }
        Tag::TableCell => {
            if ctx.active_table.is_none() {
                ctx.ensure_prefix();
            } else {
                ctx.current_line.segments.clear();
            }
        }
        _ => {}
    }
}

// ============================================================
// End tag handler
// ============================================================

pub(crate) fn handle_end_tag(tag: TagEnd, ctx: &mut MarkdownContext<'_>) {
    match tag {
        TagEnd::Paragraph => ctx.flush_paragraph(),
        TagEnd::Heading(_) => {
            ctx.flush_line();
            ctx.pop_style();
            push_blank_line(ctx.lines);
        }
        TagEnd::BlockQuote(_) => {
            ctx.flush_line();
            *ctx.blockquote_depth = ctx.blockquote_depth.saturating_sub(1);
        }
        TagEnd::List(_) => {
            // Only flush if there's actual content; otherwise flush_line creates
            // a line with just the list continuation prefix (spaces).
            if !ctx.current_line.segments.is_empty() {
                ctx.flush_line();
            }
            if ctx.list_stack.pop().is_some() {
                ctx.refresh_list_continuation_prefix();
                if ctx.list_stack.is_empty() {
                    ctx.pending_list_prefix.take();
                    push_blank_line(ctx.lines);
                } else {
                    ctx.set_pending_list_continuation();
                }
            }
        }
        TagEnd::Item => {
            if !ctx.current_line.segments.is_empty() {
                ctx.flush_line();
            }
            ctx.set_pending_list_continuation();
        }
        TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
            ctx.pop_style();
        }
        TagEnd::Link | TagEnd::Image => {
            if let Some(link) = ctx.link_state.take() {
                if link.show_destination {
                    let style = ctx.current_style();
                    ctx.current_line.push_segment_with_link(
                        style,
                        " (",
                        Some(link.destination.clone()),
                    );
                    ctx.current_line.push_segment_with_link(
                        style,
                        &link.destination,
                        Some(link.destination.clone()),
                    );
                    ctx.current_line
                        .push_segment_with_link(style, ")", Some(link.destination));
                } else if let Some(suffix) = link.hidden_location_suffix.as_deref() {
                    let label_segments = ctx
                        .current_line
                        .segments
                        .get(link.label_start_segment_idx..)
                        .unwrap_or(&[]);
                    if !links::label_segments_have_location_suffix(label_segments) {
                        ctx.current_line
                            .push_segment_with_link(ctx.current_style(), suffix, None);
                    }
                }
            }
            ctx.pop_style();
        }
        TagEnd::CodeBlock => {} // Handled by code_block module.
        TagEnd::Table => {
            if let Some(mut table) = ctx.active_table.take() {
                if !table.current_row.is_empty() {
                    table.rows.push(std::mem::take(&mut table.current_row));
                }
                let rendered = render_table(&table, ctx.base_style, ctx.available_width);
                ctx.lines.extend(rendered);
            }
            push_blank_line(ctx.lines);
        }
        TagEnd::TableRow => {
            if let Some(table) = ctx.active_table.as_mut() {
                if table.in_head {
                    table.headers = std::mem::take(&mut table.current_row);
                } else {
                    let row = std::mem::take(&mut table.current_row);
                    table.rows.push(row);
                }
            } else {
                ctx.flush_line();
            }
        }
        TagEnd::TableCell => {
            if let Some(table) = ctx.active_table.as_mut() {
                let cell = std::mem::take(ctx.current_line);
                table.current_row.push(cell);
            }
        }
        TagEnd::TableHead => {
            if let Some(table) = ctx.active_table.as_mut() {
                if !table.current_row.is_empty() {
                    table.headers = std::mem::take(&mut table.current_row);
                }
                table.in_head = false;
            }
        }
        _ => {}
    }
}

// ============================================================
// Text handler
// ============================================================

pub(crate) fn append_text(text: &str, ctx: &mut MarkdownContext<'_>) {
    let style = ctx.current_style();
    let link_target = ctx.active_link_target();

    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch == '\n' {
            let segment = &text[start..idx];
            if !segment.is_empty() {
                ctx.ensure_prefix();
                ctx.current_line
                    .push_segment_with_link(style, segment, link_target.clone());
            }
            ctx.lines.push(std::mem::take(ctx.current_line));
            start = idx + 1;
            // Skip consecutive newlines.
            while chars.peek().is_some_and(|&(_, c)| c == '\n') {
                let Some((_, c)) = chars.next() else { break };
                start += c.len_utf8();
            }
        }
    }

    if start < text.len() {
        let remaining = &text[start..];
        if !remaining.is_empty() {
            ctx.ensure_prefix();
            ctx.current_line
                .push_segment_with_link(style, remaining, link_target);
        }
    }
}

// ============================================================
// Helper functions
// ============================================================

pub(crate) fn flush_current_line(
    lines: &mut Vec<MarkdownLine>,
    current_line: &mut MarkdownLine,
    blockquote_depth: usize,
    list_continuation_prefix: &str,
    pending_list_prefix: &mut Option<String>,
    base_style: Style,
) {
    if current_line.segments.is_empty() && pending_list_prefix.is_some() {
        ensure_prefix(
            current_line,
            blockquote_depth,
            list_continuation_prefix,
            pending_list_prefix,
            base_style,
        );
    }

    if !current_line.segments.is_empty() {
        lines.push(std::mem::take(current_line));
    }
}

pub(crate) fn push_blank_line(lines: &mut Vec<MarkdownLine>) {
    if lines
        .last()
        .map(|line| line.segments.is_empty())
        .unwrap_or(false)
    {
        return;
    }
    lines.push(MarkdownLine::default());
}

pub(crate) fn trim_trailing_blank_lines(lines: &mut Vec<MarkdownLine>) {
    while lines
        .last()
        .map(|line| line.segments.is_empty())
        .unwrap_or(false)
    {
        lines.pop();
    }
}

fn ensure_prefix(
    current_line: &mut MarkdownLine,
    blockquote_depth: usize,
    list_continuation_prefix: &str,
    pending_list_prefix: &mut Option<String>,
    base_style: Style,
) {
    if !current_line.segments.is_empty() {
        return;
    }

    for _ in 0..blockquote_depth {
        current_line.push_segment(base_style.dim().italic(), "│ ");
    }

    if let Some(prefix) = pending_list_prefix.take() {
        current_line.push_segment(base_style, &prefix);
    } else if !list_continuation_prefix.is_empty() {
        current_line.push_segment(base_style, list_continuation_prefix);
    }
}

fn heading_style(level: HeadingLevel, theme: &MarkdownTheme) -> Style {
    match level {
        HeadingLevel::H1 => theme.h1,
        HeadingLevel::H2 => theme.h2,
        HeadingLevel::H3 => theme.h3,
        HeadingLevel::H4 => theme.h4,
        HeadingLevel::H5 => theme.h5,
        HeadingLevel::H6 => theme.h6,
    }
}

pub(crate) fn inline_code_style(theme: &MarkdownTheme, base_style: Style) -> Style {
    base_style.patch(theme.code)
}

fn rebuild_list_continuation_prefix(
    list_stack: &[ListState],
    list_continuation_prefix: &mut String,
) {
    list_continuation_prefix.clear();
    for state in list_stack {
        list_continuation_prefix.push_str(&state.continuation);
    }
}
