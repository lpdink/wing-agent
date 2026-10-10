//! Core markdown event handling — start/end tags, text, list, blockquote.
//!
//! Adapted from VTCode (MIT license). Uses `ratatui::style::Style` throughout.

use std::cmp::max;

use pulldown_cmark::{CodeBlockKind, HeadingLevel, Tag, TagEnd};

use super::code_blocks::CodeBlockState;
use super::images::{
    ImageAnchor, ImageOpts, ImageShape, anchor_caption, anchor_rows, resolve_image_path,
};
use super::links;
use super::tables::{TableBuffer, render_table};
use super::types::{MarkdownLine, MarkdownSegment, MarkdownTheme, SegmentKind};

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
    /// `![alt](path)` rather than `[text](url)` — only images can become
    /// anchors.
    pub(crate) is_image: bool,
    /// `lines.len()` when the label opened.
    ///
    /// A label that spans a line break (a soft break inside a multi-line alt,
    /// `![l1\nl2](a.png)`) flushes the line it started on, so the anchor would
    /// land on the label's *last* line with only the last line's text as alt —
    /// the earlier lines stay behind as ordinary link text. Anchoring is
    /// refused in that case (the mismatch is visible here): the image keeps
    /// the link path.
    pub(crate) lines_at_start: usize,
}

// ============================================================
// Image anchor state
// ============================================================

/// A completed image that may become an anchor block.
///
/// Recorded at `TagEnd::Image` (path, metadata and row count already checked)
/// and consumed when the line it sits on ends: an anchor is only valid if
/// nothing was appended to that line afterwards, which is what
/// [`PendingImage::line`] verifies. Anything else leaves the line on the
/// link path — see the module docs of [`super::images`].
#[derive(Clone, Debug)]
pub(crate) struct PendingImage {
    /// The anchor, with its path resolved, its shape known and its rows
    /// computed.
    pub(crate) anchor: ImageAnchor,
    /// The owning line's content when the image closed (see [`line_shape`]).
    pub(crate) line: (usize, usize),
}

/// A line's content identity: segment count and total text bytes.
///
/// Both are needed: two adjacent images on one line *merge* into a single
/// segment (same kind, style and link target), so a count alone would not
/// notice that a second image joined the line — and applying the first
/// image's anchor would silently drop the second one's text.
fn line_shape(line: &MarkdownLine) -> (usize, usize) {
    let bytes = line.segments.iter().map(|segment| segment.text.len()).sum();
    (line.segments.len(), bytes)
}

/// Convert a pending image into an anchor block on `line`.
///
/// The line's link rendering is replaced by the caption segment plus the
/// anchor payload. No-op when there is no pending image, or when the line
/// grew since the image closed (the image shares its line with other
/// content → the link path stays).
pub(crate) fn finish_pending_image(
    pending: &mut Option<PendingImage>,
    line: &mut MarkdownLine,
    images: &ImageOpts,
    theme: &MarkdownTheme,
) {
    let Some(pending) = pending.take() else {
        return;
    };
    if !images.is_enabled() || pending.line != line_shape(line) {
        return;
    }
    let anchor = pending.anchor;
    let caption = anchor_caption(&anchor.alt, &anchor.path, anchor.shape, anchor.cols);
    line.segments.clear();
    line.push_segment(SegmentKind::Image, theme.base.patch(theme.dimmed), &caption);
    line.image = Some(Box::new(anchor));
}

// ============================================================
// MarkdownContext — mutable state passed through event handlers
// ============================================================

pub(crate) struct MarkdownContext<'a> {
    pub(crate) style_stack: &'a mut Vec<Style>,
    /// Semantic element stack mirroring `style_stack` for element kinds.
    /// Only elements that change the segment *kind* (headings, links) push
    /// here; pure modifiers (bold/italic/strike) do not.
    pub(crate) kind_stack: &'a mut Vec<SegmentKind>,
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
    /// Render indented (4-space) blocks as prose — the Thinking profile,
    /// where indentation is nesting rather than code (see [`super::Profile`]).
    pub(crate) indented_prose: bool,
    pub(crate) active_table: &'a mut Option<TableBuffer>,
    pub(crate) link_state: &'a mut Option<LinkState>,
    /// Image anchors in flight (see [`PendingImage`]).
    pub(crate) pending_image: &'a mut Option<PendingImage>,
    /// Image mode, workspace root and metadata table — the caller's, never
    /// read/written here (no I/O on the render path).
    pub(crate) images: &'a ImageOpts,
}

impl MarkdownContext<'_> {
    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or(self.base_style)
    }

    fn current_kind(&self) -> SegmentKind {
        self.kind_stack
            .last()
            .copied()
            .expect("kind stack must never be empty")
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    fn push_kind(&mut self, kind: SegmentKind) {
        self.kind_stack.push(kind);
    }

    fn pop_kind(&mut self) {
        self.kind_stack.pop();
    }

    pub(crate) fn flush_line(&mut self) {
        // A line end is where an in-flight image either becomes an anchor or
        // falls back to the link rendering.
        finish_pending_image(
            self.pending_image,
            self.current_line,
            self.images,
            self.theme,
        );
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
        // 表格单元格自成一格：外层前缀（引用栏 `│ `、列表续行缩进、列表
        // marker）不进单元格——进去了就是每个格子糊一道假框线（`┃ │ A ┃`），
        // 比没有前缀更糟。表格整体因此不带外层前缀（登记在
        // docs/dev/tui-rendering.md 第四节）。
        if self.active_table.is_some() {
            return;
        }
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

    /// Whether the enclosing block contributes segments to the start of the
    /// line (blockquote bars, a list marker or a list continuation indent).
    ///
    /// An anchor's geometry assumes it starts at column 0 of the markdown
    /// line; anything with a block prefix degrades to the link path.
    fn has_block_prefix(&self) -> bool {
        *self.blockquote_depth > 0
            || self.pending_list_prefix.is_some()
            || !self.list_continuation_prefix.is_empty()
    }

    /// Forget an in-flight image (the line it belonged to was consumed by a
    /// path that does not finish anchors — a table cell).
    pub(crate) fn clear_pending_image(&mut self) {
        *self.pending_image = None;
    }

    /// Try to turn the image that just closed into an anchor.
    ///
    /// Every gate here is a *degradation*, not an error: the line keeps the
    /// link rendering it already has (see the design doc, "standalone images
    /// only"). Called after the link state's `Link` kind was popped (so
    /// `current_kind()` is the enclosing element) and after the image's own
    /// destination segments were appended (so the recorded segment count is
    /// the line's final one for a standalone image).
    fn try_image_anchor(
        &mut self,
        destination: &str,
        label_start_segment_idx: usize,
        lines_at_start: usize,
        alt: String,
    ) {
        if !self.images.is_enabled() {
            return;
        }
        // Alone on its line: the label is the line's first segment (nothing
        // before it — no text, no block prefix) …
        if label_start_segment_idx != 0 {
            return;
        }
        // … and it is all on one line (a multi-line alt flushes the line it
        // started on; see `LinkState::lines_at_start`).
        if self.lines.len() != lines_at_start {
            return;
        }
        // … at the top level: not in a table cell or code block, no block
        // prefix, and not inside a heading or another link.
        if self.active_table.is_some() || self.code_block.is_some() || self.has_block_prefix() {
            return;
        }
        if self.current_kind() != SegmentKind::Text {
            return;
        }
        // The row count needs the render width (anchors are not produced by
        // the width-less API).
        let Some(width) = self.available_width.filter(|width| *width > 0) else {
            return;
        };
        let Ok(path) = resolve_image_path(self.images.workspace(), destination) else {
            return;
        };
        let Some(shape) = self.images.shape_for(&path).filter(ImageShape::is_usable) else {
            return;
        };
        *self.pending_image = Some(PendingImage {
            anchor: ImageAnchor {
                path,
                alt,
                shape,
                cols: width,
                rows: anchor_rows(width, shape, self.images.cell_pixels()),
            },
            line: line_shape(self.current_line),
        });
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
            ctx.push_kind(SegmentKind::Heading);
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
                is_image: matches!(tag, Tag::Image { .. }),
                lines_at_start: ctx.lines.len(),
            });
            ctx.push_style(ctx.theme.link);
            ctx.push_kind(SegmentKind::Link);
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
                prose: matches!(kind, CodeBlockKind::Indented) && ctx.indented_prose,
            });
        }
        Tag::Table(alignments) => {
            ctx.flush_paragraph();
            *ctx.active_table = Some(TableBuffer {
                alignments: alignments.clone(),
                ..Default::default()
            });
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
            // Table cells are not anchor positions (the cell's line is
            // re-rendered by the table layout): drop any in-flight image.
            ctx.clear_pending_image();
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
            ctx.pop_kind();
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
                let LinkState {
                    destination,
                    show_destination,
                    hidden_location_suffix,
                    label_start_segment_idx,
                    is_image,
                    lines_at_start,
                } = link;
                // The alt text as written, captured before the destination /
                // location-suffix segments are appended below.
                //
                // Guarded like the location-suffix branch below: a soft break
                // inside the label (`![l1\nl2](a.png)`) flushes the line the
                // label started on, so the saved index can point past the new
                // line's end. The slice is then empty — and `alt` is only ever
                // *used* when the label did not span a line break, which is
                // exactly what `try_image_anchor`'s `lines_at_start` gate
                // checks, so this cannot change what gets anchored.
                let alt = if is_image {
                    ctx.current_line
                        .segments
                        .get(label_start_segment_idx..)
                        .unwrap_or(&[])
                        .iter()
                        .map(|segment| segment.text.as_str())
                        .collect::<String>()
                } else {
                    String::new()
                };
                if show_destination {
                    let style = ctx.current_style();
                    ctx.current_line.push_segment_with_link(
                        SegmentKind::Link,
                        style,
                        " (",
                        Some(destination.clone()),
                    );
                    ctx.current_line.push_segment_with_link(
                        SegmentKind::Link,
                        style,
                        &destination,
                        Some(destination.clone()),
                    );
                    ctx.current_line.push_segment_with_link(
                        SegmentKind::Link,
                        style,
                        ")",
                        Some(destination.clone()),
                    );
                } else if let Some(suffix) = hidden_location_suffix.as_deref() {
                    let label_segments = ctx
                        .current_line
                        .segments
                        .get(label_start_segment_idx..)
                        .unwrap_or(&[]);
                    if !links::label_segments_have_location_suffix(label_segments) {
                        ctx.current_line.push_segment_with_link(
                            SegmentKind::Link,
                            ctx.current_style(),
                            suffix,
                            None,
                        );
                    }
                }
                ctx.pop_style();
                ctx.pop_kind();
                if is_image {
                    ctx.try_image_anchor(
                        &destination,
                        label_start_segment_idx,
                        lines_at_start,
                        alt,
                    );
                }
            } else {
                ctx.pop_style();
                ctx.pop_kind();
            }
        }
        TagEnd::CodeBlock => {} // Handled by code_block module.
        TagEnd::Table => {
            if let Some(mut table) = ctx.active_table.take() {
                if !table.current_row.is_empty() {
                    table.rows.push(std::mem::take(&mut table.current_row));
                }
                let rendered = render_table(
                    &table,
                    ctx.theme.border,
                    ctx.base_style,
                    ctx.available_width,
                );
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
            ctx.clear_pending_image();
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
    let kind = ctx.current_kind();
    let link_target = ctx.active_link_target();

    let mut start = 0usize;
    let mut chars = text.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch == '\n' {
            let segment = &text[start..idx];
            if !segment.is_empty() {
                ctx.ensure_prefix();
                ctx.current_line
                    .push_segment_with_link(kind, style, segment, link_target.clone());
            }
            // A raw newline ends the line without a flush — validate any
            // in-flight image anchor here too (content appended above already
            // invalidated it through the segment count).
            finish_pending_image(ctx.pending_image, ctx.current_line, ctx.images, ctx.theme);
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
                .push_segment_with_link(kind, style, remaining, link_target);
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
    current_line.segments = block_prefix_segments(
        blockquote_depth,
        list_continuation_prefix,
        pending_list_prefix,
        base_style,
    );
}

/// Segments the enclosing block contributes to a line's start: one
/// blockquote bar per depth, then the pending list marker (consumed once) or
/// the list continuation indent.
fn block_prefix_segments(
    blockquote_depth: usize,
    list_continuation_prefix: &str,
    pending_list_prefix: &mut Option<String>,
    base_style: Style,
) -> Vec<MarkdownSegment> {
    let mut segments = Vec::new();
    for _ in 0..blockquote_depth {
        segments.push(MarkdownSegment::new(
            SegmentKind::Border,
            base_style.dim().italic(),
            "│ ",
        ));
    }
    if let Some(prefix) = pending_list_prefix.take() {
        segments.push(MarkdownSegment::new(
            SegmentKind::Marker,
            base_style,
            &prefix,
        ));
    } else if !list_continuation_prefix.is_empty() {
        segments.push(MarkdownSegment::new(
            SegmentKind::Marker,
            base_style,
            list_continuation_prefix,
        ));
    }
    segments
}

/// Prefix already-rendered lines with the enclosing block's prefix.
///
/// Used for a nested render spliced into the current block (an indented
/// block rendered as prose, see `Profile`): blank lines stay blank — the
/// paragraph convention — while every content line gets the blockquote bars
/// and the list continuation indent.
pub(crate) fn prefix_prose_lines(
    lines: Vec<MarkdownLine>,
    blockquote_depth: usize,
    list_continuation_prefix: &str,
    pending_list_prefix: &mut Option<String>,
    base_style: Style,
) -> Vec<MarkdownLine> {
    lines
        .into_iter()
        .map(|mut line| {
            if !line.segments.is_empty() {
                let mut segments = block_prefix_segments(
                    blockquote_depth,
                    list_continuation_prefix,
                    pending_list_prefix,
                    base_style,
                );
                segments.append(&mut line.segments);
                line.segments = segments;
            }
            line
        })
        .collect()
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
