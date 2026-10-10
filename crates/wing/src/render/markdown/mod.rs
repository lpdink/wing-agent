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
//!
//! ## Debugging
//!
//! `cargo run -p wing --example render_probe -- <FILE>` renders arbitrary
//! text through this pipeline (and through `stream::StreamingRender` with
//! `--chunk`, reconciling the two with `--check`). The debugging workflow and
//! the list of accepted boundaries live in `docs/dev/tui-rendering.md`.

pub(crate) mod code_blocks;
pub mod images;
pub(crate) mod links;
pub(crate) mod math;
pub(crate) mod parsing;
pub mod profile;
pub mod stream;
pub(crate) mod tables;
pub mod types;
pub(crate) mod wrap;

// Re-export the public API.
pub use profile::PROSE_DEPTH_LIMIT;
pub use profile::Profile;
pub use types::MarkdownLine;
pub use types::MarkdownSegment;
pub use types::MarkdownTheme;
pub use types::SegmentKind;
pub use types::thinking_segment_style;

// Image anchors (see `images`): the mode, the caller-supplied metadata and
// the side channel the ui layer reads.
pub use images::IMAGE_EXTENSIONS;
pub use images::ImageAnchor;
pub use images::ImageEntry;
pub use images::ImageMode;
pub use images::ImageOpts;
pub use images::ImageShape;
pub use images::ImageSpan;
pub use images::MAX_ANCHOR_ROWS;
pub use images::MAX_IMAGE_PATH_CHARS;
pub use images::MIN_ANCHOR_ROWS;
pub use images::PathReject;
pub use images::anchor_caption;
pub use images::anchor_rows;
pub use images::resolve_image_path;

// The pixel-to-cell fit the row count is computed with (see `render::fit`) —
// re-exported here because an `ImageOpts` cannot be built without a cell.
pub use crate::render::fit::CellPixels;

// Link side channel (see `links`): rendered lines + their link spans.
pub use links::ComposedLines;
pub use links::LinkSpan;
pub use links::compose_lines;
pub use links::line_link_spans;
pub use links::links_for_lines;
pub use links::osc8_close;
pub use links::osc8_open;
pub use links::sanitize_osc8_target;
pub use links::strip_osc8;
pub use links::symbol_width;

// Re-export utilities used by other modules.
pub use types::truncate_left_to_display_width;
pub use types::truncate_to_display_width;

use std::borrow::Cow;

use code_blocks::{CodeBlockRenderEnv, finalize_unclosed_code_block, handle_code_block_event};
use parsing::{
    LinkState, ListState, MarkdownContext, PendingImage, append_text, finish_pending_image,
    handle_end_tag, handle_start_tag, inline_code_style, push_blank_line,
    trim_trailing_blank_lines,
};
use pulldown_cmark::{Event, Options, Parser};
use ratatui::style::Style;
use ratatui::text::Line;
use tables::TableBuffer;

use crate::config::ThemePalette;
use crate::config::rendering::MathMode;

/// Code-block rendering options.
///
/// [`Profile`] carries the reasoning-vs-content rendering rules (see its
/// module docs); code blocks themselves render identically in both.
///
/// The `'a` lifetime is the image metadata's: [`RenderOpts::new`] borrows a
/// shared "images off" value, so every pre-existing call site keeps compiling
/// (and keeps the pre-image behaviour) unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderOpts<'a> {
    /// Which cell the text belongs to.
    pub profile: Profile,
    /// Remaining nesting budget for indented-as-prose blocks — see
    /// [`PROSE_DEPTH_LIMIT`]. Only the renderer decrements it.
    pub prose_depth: u8,
    /// Trim trailing blank lines (doc-end semantics). The streaming
    /// renderer disables this when rendering a PROMOTED block: the
    /// presence of the renderer's own trailing blank line is exactly the
    /// separator the doc-context full render would emit at that boundary
    /// (paragraph/heading/list/table ends push one; code/HTML do not) —
    /// the block promotion pops it and re-emits it lazily instead.
    pub trim_trailing_blank: bool,
    /// Whether formulas are rendered (see [`MathMode`]).
    ///
    /// `Text` — the default — enables pulldown's math parsing *and* the
    /// delimiter normalization that feeds it; `Off` leaves the source
    /// untouched, which is exactly the pre-math rendering. Callers that have
    /// a palette derive this from it ([`RenderOpts::with_math`]); the
    /// streaming engine and the reference render both do, so the two agree.
    pub math: MathMode,
    /// Image rendering (mode, workspace root, metadata table) — see
    /// [`images`](self::images).
    pub images: &'a ImageOpts,
}

impl Default for RenderOpts<'_> {
    fn default() -> Self {
        Self::new(Profile::Content, true)
    }
}

impl<'a> RenderOpts<'a> {
    /// Options for `profile`, with doc-end trimming per `trim_trailing_blank`
    /// and a fresh prose-nesting budget. Images stay off.
    pub fn new(profile: Profile, trim_trailing_blank: bool) -> Self {
        Self {
            profile,
            prose_depth: PROSE_DEPTH_LIMIT,
            trim_trailing_blank,
            math: MathMode::Text,
            images: ImageOpts::off(),
        }
    }

    /// Set the math mode (usually `palette.math_mode`).
    pub fn with_math(mut self, math: MathMode) -> Self {
        self.math = math;
        self
    }

    /// The same options with `images` in place of the shared "off" value.
    pub fn with_images<'b>(self, images: &'b ImageOpts) -> RenderOpts<'b> {
        RenderOpts { images, ..self }
    }
}

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
///
/// The math mode comes from `palette` (the palette is the render layer's
/// config carrier, see [`RenderOpts::math`]), so every one of these
/// convenience callers honours `rendering.math`.
pub fn render_markdown_lines(
    text: &str,
    width: Option<u16>,
    palette: &ThemePalette,
) -> Vec<MarkdownLine> {
    render_markdown_lines_with(
        text,
        width,
        palette,
        RenderOpts::default().with_math(palette.math_mode),
    )
}

/// [`render_markdown_lines`] with explicit rendering options (code blocks and
/// image anchors).
///
/// Image anchors are produced only when `width` is `Some` (the row count is a
/// function of the render width) and `opts.images` is in `Anchor` mode with a
/// metadata table that knows the image — see [`images`](self::images).
pub fn render_markdown_lines_with(
    text: &str,
    width: Option<u16>,
    palette: &ThemePalette,
    opts: RenderOpts<'_>,
) -> Vec<MarkdownLine> {
    let theme = MarkdownTheme::from_palette(palette);
    let base_style = theme.base;

    // Pre-process: ensure code fences are on their own line. Reasoning text
    // often contains inline ``` references (e.g. `（```rust）`); normalizing
    // them would create spurious code blocks (see `Profile`).
    let text = if opts.profile.normalizes_inline_fences() {
        ensure_fences_on_own_line(text)
    } else {
        text.into()
    };

    let lines = render_markdown_to_lines(&text, base_style, &theme, width, opts);

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
/// - It's not already at line start, and everything before it on the line is
///   not blank either (a blank prefix means the fence IS line-start — up to
///   3 spaces of indent are legal — or an indented code block; inserting a
///   newline there would tear the indentation off the fence and its body)
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
        // Already at line start (blank prefix included) — nothing to do.
        if i == 0 || bytes[i - 1] == b'\n' || fence_prefix_is_blank(bytes, i) {
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

/// Whether everything before byte offset `at` on that line is blank — the
/// fence there is already a line-start fence (≤3 spaces of indent are legal)
/// or an indented code block, so it must not be normalized.
pub(crate) fn fence_prefix_is_blank(bytes: &[u8], at: usize) -> bool {
    let line_start = bytes[..at]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    bytes[line_start..at]
        .iter()
        .all(|&b| b == b' ' || b == b'\t')
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
    opts: RenderOpts<'_>,
) -> Vec<MarkdownLine> {
    // Math delimiters pulldown does not know (`\(…\)`, `\[…\]`, a bare
    // `\begin{align}…\end{align}`) are rewritten into `$…$` / `$$…$$` before
    // parsing. Pure text rewrite, so every entry point that shares this
    // function — the full render, a streaming slice, a re-parsed prose
    // block — normalizes identically (see `math`). The rule itself belongs
    // to `Profile` (like the fence and indentation rules); `rendering.math
    // = off` is the config switch that turns the whole math path off.
    let math_on = opts.math == MathMode::Text && opts.profile.normalizes_math_delimiters();
    let normalized = if math_on {
        math::normalize_delimiters(source)
    } else {
        Cow::Borrowed(source)
    };
    let source: &str = &normalized;

    let mut parser_options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    if math_on {
        parser_options |= Options::ENABLE_MATH;
    }
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
    // A completed image that may still become an anchor: it is validated (and
    // dropped) when the line it sits on is flushed — see `parsing`.
    let mut pending_image: Option<PendingImage> = None;

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
            width: available_width,
            opts,
            render_markdown: render_markdown_to_lines,
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
            indented_prose: opts.profile.indented_blocks_are_prose(),
            active_table: &mut active_table,
            link_state: &mut link_state,
            pending_image: &mut pending_image,
            images: opts.images,
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
            // Math. Both arms must exist BEFORE `ENABLE_MATH` is turned on:
            // the catch-all below is `_ => {}`, so an unhandled math event
            // would silently drop the formula.
            Event::InlineMath(src) => math::inline(&src, &mut ctx),
            Event::DisplayMath(src) => math::display(&src, &mut ctx),
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
        width: available_width,
        opts,
        render_markdown: render_markdown_to_lines,
    };
    finalize_unclosed_code_block(&mut code_block, &mut code_block_env);

    // The final line (no trailing newline in the source) can hold the last
    // image of the document: validate its anchor before pushing.
    finish_pending_image(&mut pending_image, &mut current_line, opts.images, theme);
    if !current_line.segments.is_empty() {
        lines.push(current_line);
    }

    if opts.trim_trailing_blank {
        trim_trailing_blank_lines(&mut lines);
    }
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

    // ============================================================
    // Profile semantics (reasoning vs assistant content)
    // ============================================================

    /// Render `md` under `profile`, flattening to (kind, text) pairs.
    fn profile_pairs(md: &str, profile: Profile) -> Vec<(SegmentKind, String)> {
        render_markdown_lines_with(md, None, &dp(), RenderOpts::new(profile, true))
            .into_iter()
            .flat_map(|line| line.segments)
            .map(|seg| (seg.kind, seg.text))
            .collect()
    }

    fn joined_plain(md: &str, profile: Profile) -> String {
        render_markdown_lines_with(md, None, &dp(), RenderOpts::new(profile, true))
            .iter()
            .map(|line| line.to_plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn profile_content_keeps_indented_blocks_as_code() {
        // CommonMark: a 4-space indented block IS code. Assistant content is
        // a plain document and keeps that reading.
        let pairs = profile_pairs("before\n\n    indented line\n\nafter", Profile::Content);
        assert_eq!(
            find_segment(&pairs, "indented line"),
            SegmentKind::CodeBlock
        );
        assert_eq!(find_segment(&pairs, "┌"), SegmentKind::Border);
    }

    #[test]
    fn profile_thinking_renders_indented_blocks_as_prose() {
        // Reasoning indents to nest sub-thoughts, not to write code: the
        // block renders as prose, de-indented and markdown-parsed like the
        // surrounding text.
        let text = "before\n\n    indented **prose** with `code`\n    second line\n\nafter";
        let pairs = profile_pairs(text, Profile::Thinking);
        assert!(
            pairs.iter().all(|(kind, _)| !matches!(
                kind,
                SegmentKind::CodeBlock | SegmentKind::Gutter | SegmentKind::Border
            )),
            "thinking rendered code chrome for an indented block: {pairs:?}"
        );
        assert_eq!(find_segment(&pairs, "indented "), SegmentKind::Text);
        assert_eq!(find_segment(&pairs, "code"), SegmentKind::InlineCode);
        assert_eq!(find_segment(&pairs, "second line"), SegmentKind::Text);
        assert_eq!(find_segment(&pairs, "after"), SegmentKind::Text);
        // De-indented: the nesting offset is dropped, and the paragraphs keep
        // their blank-line separation.
        let plain = joined_plain(text, Profile::Thinking);
        assert!(
            plain.contains("\nindented ") && plain.contains("\nsecond line"),
            "indentation not dropped: {plain:?}"
        );
        assert_eq!(
            joined_plain("    one\n\n    two\n", Profile::Thinking),
            "one\n\ntwo",
            "interior blank lines separate paragraphs"
        );
    }

    #[test]
    fn profile_thinking_indented_list_stays_a_list() {
        // A list inside an indented region is prose structure, not code.
        let pairs = profile_pairs("note:\n\n    - first\n    - second\n", Profile::Thinking);
        assert_eq!(find_segment(&pairs, "first"), SegmentKind::Text);
        assert!(
            pairs
                .iter()
                .any(|(kind, text)| *kind == SegmentKind::Marker && text.contains('•')),
            "list marker lost: {pairs:?}"
        );
        assert!(
            pairs
                .iter()
                .all(|(kind, _)| *kind != SegmentKind::CodeBlock)
        );
    }

    #[test]
    fn profile_thinking_indented_prose_recursion_is_bounded() {
        // Every nesting level re-parses the block, so the budget caps what a
        // degenerate stream can cost: past it the block renders as a code
        // block again (one parser per level would be O(depth × text) per
        // frame while streaming, and a stack overflow the TUI cannot catch).
        let depth = usize::from(PROSE_DEPTH_LIMIT) + 4;
        let text = format!("before\n\n{}deep\n", "    ".repeat(depth));
        let pairs = profile_pairs(&text, Profile::Thinking);
        assert!(
            pairs
                .iter()
                .any(|(kind, _)| *kind == SegmentKind::CodeBlock),
            "expected the code-block fallback past the budget: {pairs:?}"
        );

        // Within the budget the same shape is prose — no code chrome at all.
        let text = format!("before\n\n{}deep\n", "    ".repeat(2));
        let pairs = profile_pairs(&text, Profile::Thinking);
        assert!(
            pairs.iter().all(|(kind, _)| !matches!(
                kind,
                SegmentKind::CodeBlock | SegmentKind::Border | SegmentKind::Gutter
            )),
            "within the budget the block is prose: {pairs:?}"
        );

        // Absurd depth stays cheap and does not recurse per level.
        let text = format!("{}deep\n", "    ".repeat(2000));
        let lines = profile_pairs(&text, Profile::Thinking);
        assert!(!lines.is_empty());
    }

    #[test]
    fn profile_normalizes_inline_fences_only_for_content() {
        // `text:```lang` on one line is normalized into a fence for content;
        // reasoning discusses fences in prose, so the backticks stay literal.
        let md = "run this:```rust\nlet x = 1;";
        let content = profile_pairs(md, Profile::Content);
        assert!(
            content
                .iter()
                .any(|(kind, text)| *kind == SegmentKind::CodeBlock && text.contains("let")),
            "content did not normalize the inline fence: {content:?}"
        );
        let thinking = profile_pairs(md, Profile::Thinking);
        assert!(
            thinking
                .iter()
                .all(|(kind, _)| *kind != SegmentKind::CodeBlock),
            "reasoning normalized an inline fence: {thinking:?}"
        );
    }

    #[test]
    fn profile_fenced_blocks_render_the_same_in_both() {
        // The alignment contract: a fenced block (highlighting, gutter,
        // borders) is profile-independent — only prose is recolored, and that
        // happens later, in the cell compose.
        let md = "text\n\n```rust\nlet x = 1;\n```\n";
        let pairs = |profile| profile_pairs(md, profile);
        assert_eq!(
            pairs(Profile::Thinking),
            pairs(Profile::Content),
            "a fenced code block must render identically in both profiles"
        );
        // Both sides must really be highlighted, or the equality is trivial:
        // syntect splits the line into per-token segments.
        let code = pairs(Profile::Thinking)
            .into_iter()
            .filter(|(kind, _)| *kind == SegmentKind::CodeBlock)
            .map(|(_, text)| text)
            .collect::<Vec<_>>();
        assert!(code.len() >= 2, "line was not token-split: {code:?}");
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
    fn table_framed_grid() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let lines = render_text(md);
        let text = join_lines(&lines);
        // 重框三档：外框与表头带重、表体网格轻 —— 每档有自己的结点字形。
        assert!(
            text.contains('┏') && text.contains('┓'),
            "frame corners missing: {text}"
        );
        assert!(text.contains('╇'), "heavy header separator: {text}");
        assert!(text.contains('│'), "light body divider: {text}");
        assert!(text.contains('┷'), "bottom junctions: {text}");
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

    /// 引用 / 列表里的表格：外层前缀（引用栏 `│ `、列表续行 / marker）不许
    /// 进单元格——每个格子糊一道引用栏（`┃ │ A ┃`）比没有前缀更糟。表格
    /// 整体也不带外层前缀；列表 item 的 marker 单独一行（表格前的归属注记）。
    #[test]
    fn table_in_blockquote_keeps_cells_clean() {
        let md = "> | A | B |\n> |---|---|\n> | 1 | 2 |";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("┃ A"), "clean header cell: {text}");
        assert!(
            !text.contains("┃ │"),
            "quote rail leaked into a cell: {text}"
        );
    }

    #[test]
    fn table_in_list_item_keeps_cells_clean() {
        let md = "- | A | B |\n  |---|---|\n  | 1 | 2 |";
        let lines = render_text(md);
        let text = join_lines(&lines);
        assert!(text.contains("┃ A"), "clean header cell: {text}");
        assert!(
            !text.contains("┃ •"),
            "list marker leaked into a cell: {text}"
        );
        // marker 单独成行（表格仍在，结构没丢）。
        assert!(
            lines.iter().any(|l| l.trim() == "•"),
            "marker line missing: {lines:?}"
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
        // Framed grid at a constrained width: heavy frame + heavy header rule.
        assert!(text.contains("━"), "got: {text}");
        assert!(text.contains('┃'), "frame verticals missing: {text}");
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
        // Layout: top frame, header, heavy rule, row0, light rule, row1, bottom.
        assert!(
            non_blank.len() >= 7,
            "expected frame + header + rule + row + rule + row + frame, got: {non_blank:?}"
        );
        assert!(non_blank[0].contains('┏'), "top frame: {}", non_blank[0]);
        assert!(non_blank[1].contains("File") && non_blank[1].contains("Function"));
        assert!(non_blank[2].contains("━"), "header rule: {}", non_blank[2]);
        assert!(non_blank[3].contains("src/main.rs"));
        assert!(non_blank[4].contains("─"), "body rule: {}", non_blank[4]);
        assert!(non_blank[5].contains("src/lib.rs"));
        assert!(non_blank[6].contains('┗'), "bottom frame: {}", non_blank[6]);
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

    /// Fenced diff blocks tint add/delete rows (code keeps syntax colors)
    /// and pad the band to the available width.
    #[test]
    fn diff_code_block_rows_are_tinted() {
        let md = "```diff\ndiff --git a/file.rs b/file.rs\n@@ -1,3 +1,3 @@\n-old();\n+new();\n context\n```";
        let palette = dp();
        let width = 60u16;
        let lines = render_markdown_with_width(md, Some(width), &palette);

        let add = lines
            .iter()
            .find(|l| l.to_string().contains("new()"))
            .expect("add row");
        let del = lines
            .iter()
            .find(|l| l.to_string().contains("old()"))
            .expect("delete row");

        let add_bgs: Vec<_> = add.spans.iter().map(|s| s.style.bg).collect();
        let del_bgs: Vec<_> = del.spans.iter().map(|s| s.style.bg).collect();
        assert!(
            add_bgs.contains(&Some(palette.diff_add_bg)),
            "add row not tinted: {add_bgs:?}"
        );
        assert!(
            del_bgs.contains(&Some(palette.diff_del_bg)),
            "del row not tinted: {del_bgs:?}"
        );
        // Syntax colors survive on the tinted row.
        let fgs: std::collections::BTreeSet<_> = add
            .spans
            .iter()
            .map(|s| format!("{:?}", s.style.fg))
            .collect();
        assert!(fgs.len() >= 2, "no syntax colors on the add row: {fgs:?}");
        // The tinted band is padded to the available width.
        let used: usize = add
            .spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        assert_eq!(used, width as usize, "tinted band not padded to width");
    }

    fn fenced_diff_text(md: &str) -> String {
        render_markdown_with_width(md, Some(70), &dp())
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Context rows of a fenced diff are syntax-highlighted (not plain) and
    /// never tinted.
    #[test]
    fn diff_code_block_context_rows_are_highlighted() {
        let md = "```diff\ndiff --git a/m.rs b/m.rs\n@@ -1,3 +1,3 @@\n-use std::io;\n+use std::fmt;\n pub fn render() {}\n```";
        let palette = dp();
        let lines = render_markdown_with_width(md, Some(70), &palette);
        let ctx = lines
            .iter()
            .find(|l| l.to_string().contains("pub fn render"))
            .expect("context row");
        let fgs: std::collections::BTreeSet<_> = ctx
            .spans
            .iter()
            .map(|s| format!("{:?}", s.style.fg))
            .collect();
        assert!(fgs.len() >= 2, "context row not highlighted: {fgs:?}");
        assert!(
            ctx.spans.iter().all(|s| s.style.bg.is_none()),
            "context row must not be tinted"
        );
    }

    /// A context line that starts with a single `@` (decorators, annotations)
    /// is code — only `@@` opens a hunk header.
    #[test]
    fn diff_code_block_decorator_is_not_a_hunk_header() {
        let md = "```diff\ndiff --git a/t.py b/t.py\n@@ -1,3 +1,3 @@\n import pytest\n @pytest.fixture\n-def test_x():\n+def test_y():\n     pass\n```";
        let palette = dp();
        let lines = render_markdown_with_width(md, Some(70), &palette);

        let decorator = lines
            .iter()
            .find(|l| l.to_string().contains("@pytest.fixture"))
            .expect("decorator row");
        assert!(
            !decorator
                .spans
                .iter()
                .any(|s| s.style == palette_hunk(&palette)),
            "decorator line rendered as a hunk header: {decorator:?}"
        );
        // …while the real hunk header keeps its style.
        assert!(
            lines
                .iter()
                .any(|l| l.to_string().contains("@@ -1,3 +1,3 @@")
                    && l.spans.iter().any(|s| s.style == palette_hunk(&palette))),
            "hunk header lost its style"
        );
    }

    fn palette_hunk(palette: &ThemePalette) -> Style {
        MarkdownTheme::from_palette(palette).diff_hunk
    }

    /// `\ No newline at end of file` is diff metadata, not code.
    #[test]
    fn diff_code_block_skips_no_newline_marker() {
        let md = "```diff\ndiff --git a/f.txt b/f.txt\n@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n```";
        let text = fenced_diff_text(md);
        assert!(!text.contains("No newline"), "metadata leaked: {text}");
    }

    /// A construct opened in a context line must still color the deleted line
    /// that continues it (same rule as the diff cell — shared helper).
    #[test]
    fn diff_code_block_keeps_old_revision_state() {
        let md = "```diff\ndiff --git a/m.rs b/m.rs\n@@ -1,4 +1,3 @@\n fn main() {\n     /* note\n-    let removed = 1;\n     */\n }\n```";
        let lines = render_markdown_with_width(md, Some(70), &dp());
        let fg_of = |needle: &str| {
            lines
                .iter()
                .find(|l| l.to_string().contains(needle))
                .and_then(|l| l.spans.iter().find(|s| s.content.contains(needle)))
                .and_then(|s| s.style.fg)
        };
        let comment = fg_of("note").expect("comment row");
        let removed = fg_of("removed").expect("deleted row");
        assert_eq!(
            removed, comment,
            "deleted line lost the old revision's comment state"
        );
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

    // ============================================================
    // Image anchors (see `images`)
    // ============================================================

    /// The terminal cell the anchor fixtures are laid out for (10×20 px).
    const CELL: CellPixels = CellPixels::new(10, 20);

    /// A metadata table rooted at `/ws`, keyed by the resolved path.
    fn image_opts(entries: &[(&str, u32, u32)]) -> ImageOpts {
        ImageOpts::anchor(
            Some(std::path::PathBuf::from("/ws")),
            entries
                .iter()
                .map(|(path, px_w, px_h)| {
                    ImageEntry::new(
                        std::path::PathBuf::from("/ws").join(path),
                        ImageShape::new(*px_w, *px_h),
                    )
                })
                .collect(),
            CELL,
        )
    }

    /// The plot.png table used by most tests (800×600 → 30 rows at width 80).
    fn plot_opts() -> ImageOpts {
        image_opts(&[("plot.png", 800, 600)])
    }

    /// IR fingerprint: everything a caller can observe on a rendered line,
    /// plus whether it carries an anchor payload.
    type Fingerprint = Vec<(SegmentKind, String, Option<String>, Style, bool)>;

    fn fingerprint(lines: &[MarkdownLine]) -> Fingerprint {
        lines
            .iter()
            .flat_map(|line| {
                let anchored = line.image.is_some();
                line.segments
                    .iter()
                    .map(move |seg| {
                        (
                            seg.kind,
                            seg.text.clone(),
                            seg.link_target.clone(),
                            seg.style,
                            anchored,
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn render_with(md: &str, width: Option<u16>, opts: RenderOpts<'_>) -> Vec<MarkdownLine> {
        render_markdown_lines_with(md, width, &dp(), opts)
    }

    fn content_opts(images: &ImageOpts) -> RenderOpts<'_> {
        RenderOpts::new(Profile::Content, true).with_images(images)
    }

    // ── The two-tier rule: Off (or no metadata) == today ─────────────

    /// Every shape of image usage, so the "off" comparison is not vacuous.
    const IMAGE_SHAPES: &[&str] = &[
        "![alt](./plot.png)",
        "![alt](plot.png)",
        "text ![alt](plot.png) more",
        "![](./plot.png)",
        "![a](./plot.png)![b](./plot.png)",
        "![remote](https://example.com/x.png)",
        "![txt](notes.txt)",
        "![escape](../outside.png)",
        "- ![alt](./plot.png)\n- second",
        "> ![alt](./plot.png)",
        "# ![alt](./plot.png)",
        "```markdown\n![alt](./plot.png)\n```",
        "<img src=\"./plot.png\">",
        "![nested ![inner](./plot.png)](./plot.png)",
        "![alt](./plot.png)\n\n![b](./plot.png)",
        "![alt](./plot.png) tail",
    ];

    #[test]
    fn off_mode_is_identical_to_anchor_mode_without_metadata() {
        // D2's rendering-layer landing point: with images off — or on but
        // without a metadata entry — every image renders through the link
        // path, span for span.
        for md in IMAGE_SHAPES {
            let off = render_with(md, Some(80), RenderOpts::new(Profile::Content, true));
            let anchor = render_with(md, Some(80), content_opts(ImageOpts::off()));
            let anchor_with_table = render_with(md, Some(80), content_opts(&image_opts(&[])));
            assert_eq!(
                fingerprint(&off),
                fingerprint(&anchor),
                "off vs anchor(off) for {md:?}"
            );
            assert_eq!(
                fingerprint(&off),
                fingerprint(&anchor_with_table),
                "off vs anchor(no metadata) for {md:?}"
            );
            assert!(off.iter().all(|l| l.image.is_none()));
        }
    }

    #[test]
    fn off_mode_renders_an_image_as_a_link() {
        // The concrete pre-image behaviour, pinned: the alt text is the link
        // label, a bare relative destination is shown in parentheses, and a
        // local path is hidden.
        let pairs = render_with(
            "![alt](plot.png)",
            Some(80),
            RenderOpts::new(Profile::Content, true),
        );
        let pairs = pairs
            .into_iter()
            .flat_map(|l| l.segments)
            .map(|s| (s.text, s.link_target, s.kind))
            .collect::<Vec<_>>();
        // Adjacent same-style link segments merge, so the label and the
        // shown destination arrive as one segment.
        assert_eq!(
            pairs,
            vec![(
                "alt (plot.png)".to_string(),
                Some("plot.png".to_string()),
                SegmentKind::Link
            )]
        );
        // A `./` destination is a local path: the URL is hidden.
        let local = render_with(
            "![alt](./plot.png)",
            Some(80),
            RenderOpts::new(Profile::Content, true),
        )
        .into_iter()
        .flat_map(|l| l.segments)
        .map(|s| s.text)
        .collect::<Vec<_>>();
        assert_eq!(local, vec!["alt".to_string()]);
    }

    // ── Anchors ───────────────────────────────────────────────────

    #[test]
    fn a_standalone_image_becomes_an_anchor_line() {
        let lines = render_with(
            "before\n\n![销售趋势](./plot.png)\n\nafter",
            Some(80),
            content_opts(&plot_opts()),
        );
        let anchored = lines
            .iter()
            .find(|line| line.image.is_some())
            .expect("no anchor produced");
        // One IR line: the caption. The cover rows are expanded at compose
        // time, so nothing here changes the markdown layer's line count.
        assert_eq!(anchored.segments.len(), 1);
        assert_eq!(anchored.segments[0].kind, SegmentKind::Image);
        assert_eq!(anchored.segments[0].text, "▢ 销售趋势 · 800×600");
        let anchor = anchored.image.as_deref().expect("payload");
        assert_eq!(anchor.path, std::path::PathBuf::from("/ws/plot.png"));
        assert_eq!(anchor.alt, "销售趋势");
        assert_eq!(anchor.shape, ImageShape::new(800, 600));
        assert_eq!(anchor.cols, 80);
        assert_eq!(anchor.rows, 30);
        // Exactly one anchored line in the document.
        assert_eq!(lines.iter().filter(|l| l.image.is_some()).count(), 1);
    }

    #[test]
    fn anchor_does_not_add_ir_lines() {
        // The IR stays one line per markdown line: the cover rows exist only
        // after the compose (which is what keeps every line-count-sensitive
        // rule of this layer — trailing-blank trimming, blank dedup, the
        // streaming block separators — untouched).
        let md = "before\n\n![alt](./plot.png)\n\nafter";
        let off = render_with(md, Some(80), RenderOpts::new(Profile::Content, true));
        let on = render_with(md, Some(80), content_opts(&plot_opts()));
        // Same number of IR lines with and without the anchor — the only
        // difference is that one line is a caption with a payload.
        assert_eq!(off.len(), on.len());
        assert_eq!(
            off.len(),
            on.iter().filter(|line| line.image.is_none()).count() + 1
        );
    }

    #[test]
    fn anchor_rows_are_the_shared_fit_of_width_shape_and_cell() {
        for (px_w, px_h) in [
            (800, 600),
            (1920, 1080),
            (400, 400),
            (800, 6000),
            (512, 512),
        ] {
            let opts = image_opts(&[("plot.png", px_w, px_h)]);
            let shape = ImageShape::new(px_w, px_h);
            for width in [20u16, 40, 80, 118] {
                let lines = render_with("![alt](./plot.png)", Some(width), content_opts(&opts));
                let anchor = lines
                    .iter()
                    .find_map(|line| line.image.as_deref())
                    .expect("anchor");
                assert_eq!(
                    anchor.rows,
                    crate::render::markdown::anchor_rows(width, shape, CELL),
                    "{px_w}x{px_h} at width {width}"
                );
                assert_eq!(anchor.cols, width);
            }
        }
    }

    #[test]
    fn anchor_rows_do_not_depend_on_the_profile() {
        // The row count is a layout fact, not a rendering rule: both profiles
        // must agree, or a cell's height would change with the view mode.
        let md = "![alt](./plot.png)";
        let content = render_with(md, Some(80), content_opts(&plot_opts()))
            .into_iter()
            .find_map(|line| line.image.map(|a| a.rows));
        let thinking = render_with(
            md,
            Some(80),
            RenderOpts::new(Profile::Thinking, true).with_images(&plot_opts()),
        )
        .into_iter()
        .find_map(|line| line.image.map(|a| a.rows));
        assert_eq!(content, Some(30));
        assert_eq!(thinking, Some(30));
    }

    #[test]
    fn anchor_uses_the_alt_text_or_the_file_name_in_the_caption() {
        let lines = render_with("![](./plot.png)", Some(80), content_opts(&plot_opts()));
        let caption = lines
            .iter()
            .find(|line| line.image.is_some())
            .expect("anchor")
            .to_plain();
        assert_eq!(caption, "▢ plot.png · 800×600");
    }

    #[test]
    fn anchor_caption_fits_even_a_narrow_render_width() {
        for width in [4u16, 8, 12, 20] {
            let lines = render_with(
                "![一个相当长的中文替代文本](./plot.png)",
                Some(width),
                content_opts(&plot_opts()),
            );
            let line = lines.iter().find(|l| l.image.is_some()).expect("anchor");
            assert!(
                line.width() <= usize::from(width),
                "caption wider than the render width at {width}: {:?}",
                line.to_plain()
            );
        }
    }

    #[test]
    fn anchors_need_a_render_width() {
        // Without a width there is no row count — the width-less API keeps
        // the link path (documented in `render_markdown_lines_with`).
        let lines = render_with("![alt](./plot.png)", None, content_opts(&plot_opts()));
        assert!(lines.iter().all(|line| line.image.is_none()));
    }

    #[test]
    fn anchors_need_metadata_and_a_usable_shape() {
        for opts in [
            image_opts(&[]),                        // no entry
            image_opts(&[("other.png", 800, 600)]), // another path
            image_opts(&[("plot.png", 0, 600)]),    // degenerate
            image_opts(&[("plot.png", 800, 0)]),    // degenerate
        ] {
            let lines = render_with("![alt](./plot.png)", Some(80), content_opts(&opts));
            assert!(
                lines.iter().all(|line| line.image.is_none()),
                "anchor produced without usable metadata: {opts:?}"
            );
        }
    }

    #[test]
    fn a_multi_line_alt_degrades_to_the_link_path() {
        // A soft break inside the label flushes the line the image started on,
        // so an anchor would land on the label's last line with only that
        // line's text as alt (and the earlier lines would stay behind as link
        // text). The image keeps the link path instead.
        for md in [
            "![l1\nl2](./plot.png)",
            "![l1  \nl2](./plot.png)",
            "text ![l1\nl2](./plot.png)",
        ] {
            let lines = render_with(md, Some(80), content_opts(&plot_opts()));
            assert!(
                lines.iter().all(|line| line.image.is_none()),
                "multi-line alt anchored: {md:?}"
            );
            let off = render_with(md, Some(80), RenderOpts::new(Profile::Content, true));
            assert_eq!(fingerprint(&off), fingerprint(&lines), "{md:?}");
        }
        // A single-line label with the same text still anchors.
        let single = render_with("![l1 l2](./plot.png)", Some(80), content_opts(&plot_opts()));
        assert_eq!(single.iter().filter(|l| l.image.is_some()).count(), 1);
    }

    #[test]
    fn a_duplicate_metadata_entry_takes_the_first_row() {
        // The table is keyed by resolved path; duplicates are the caller's bug
        // but the outcome is deterministic (first row wins) — see
        // `ImageOpts::shape_for`.
        let opts = image_opts(&[("plot.png", 800, 600), ("plot.png", 1600, 900)]);
        let lines = render_with("![alt](./plot.png)", Some(80), content_opts(&opts));
        let anchor = lines
            .iter()
            .find_map(|line| line.image.as_deref())
            .expect("anchor");
        assert_eq!(anchor.shape, ImageShape::new(800, 600));
        assert_eq!(anchor.rows, 30);
    }

    #[test]
    fn anchor_paths_must_pass_the_policy() {
        // Every rejected destination degrades to the link path, and the
        // rendering is then exactly the "off" rendering.
        for md in [
            "![alt](https://example.com/plot.png)",
            "![alt](data:image/png;base64,AAAA)",
            "![alt](notes.txt)",
            "![alt](../outside.png)",
            "![alt](/etc/../etc/plot.png\u{0})",
            "![alt](~/plot.png)",
            "![alt](PLOT.TXT)",
        ] {
            let off = render_with(md, Some(80), RenderOpts::new(Profile::Content, true));
            let on = render_with(md, Some(80), content_opts(&plot_opts()));
            assert_eq!(fingerprint(&off), fingerprint(&on), "{md:?}");
            assert!(on.iter().all(|line| line.image.is_none()), "{md:?}");
        }
        // …while a bare relative path that *does* resolve is anchored.
        let bare = render_with("![alt](plot.png)", Some(80), content_opts(&plot_opts()));
        assert_eq!(
            bare.iter().filter(|line| line.image.is_some()).count(),
            1,
            "a workspace-relative path with metadata must anchor"
        );
    }

    #[test]
    fn anchors_are_standalone_top_level_images_only() {
        // Anything else keeps the link rendering: the anchor box's geometry
        // is only valid at column 0 of its own line.
        let cases: &[(&str, &str)] = &[
            ("leading text", "text ![alt](./plot.png)"),
            ("trailing text", "![alt](./plot.png) trailing"),
            ("two on a line", "![a](./plot.png)![b](./plot.png)"),
            ("list item", "- ![alt](./plot.png)"),
            ("ordered item", "1. ![alt](./plot.png)"),
            ("nested list item", "- parent\n  - ![alt](./plot.png)"),
            ("blockquote", "> ![alt](./plot.png)"),
            ("nested blockquote", "> > ![alt](./plot.png)"),
            ("heading", "# ![alt](./plot.png)"),
            ("inside a link", "[![alt](./plot.png)](./other.png)"),
            ("table cell", "| h |\n|---|\n| ![alt](./plot.png) |"),
            ("escaped", "\\![alt](./plot.png)"),
        ];
        for (name, md) in cases {
            let lines = render_with(md, Some(80), content_opts(&plot_opts()));
            assert!(
                lines.iter().all(|line| line.image.is_none()),
                "{name} was anchored: {md:?}"
            );
            // …and the rendering is the link path, i.e. today's.
            let off = render_with(md, Some(80), RenderOpts::new(Profile::Content, true));
            assert_eq!(fingerprint(&off), fingerprint(&lines), "{name}");
        }
    }

    #[test]
    fn anchors_never_come_from_code_blocks_html_or_nested_prose() {
        // A fenced block's content is text, an HTML `<img>` is text, and a
        // nested (indented) prose render has a shifted column space.
        for md in [
            "```markdown\n![alt](./plot.png)\n```",
            "<img src=\"./plot.png\">",
        ] {
            let lines = render_with(md, Some(80), content_opts(&plot_opts()));
            assert!(lines.iter().all(|line| line.image.is_none()), "{md:?}");
        }
        // Thinking profile: an indented block is re-parsed as prose and then
        // prefixed — its images stay links.
        let thinking = render_with(
            "note:\n\n    ![alt](./plot.png)\n",
            Some(80),
            RenderOpts::new(Profile::Thinking, true).with_images(&plot_opts()),
        );
        assert!(
            thinking.iter().all(|line| line.image.is_none()),
            "anchored inside a nested prose block: {thinking:?}"
        );
    }

    #[test]
    fn a_second_image_on_the_same_line_drops_the_first_anchor() {
        // The first image is alone when it closes, but by the time its line is
        // flushed the second one has joined it — the segment-count check drops
        // the pending anchor.
        let lines = render_with(
            "![a](./plot.png) ![b](./plot.png)",
            Some(80),
            content_opts(&plot_opts()),
        );
        assert!(lines.iter().all(|line| line.image.is_none()));
    }

    #[test]
    fn anchor_boundaries_stay_honest() {
        // Empty alt, empty path, absurd alt/path, control characters and a
        // zero-width render: none of them may panic, and each lands on the
        // side of the rule its metadata allows.
        let long_alt = "长".repeat(400);
        let long_path = format!("{}.png", "p".repeat(600));
        let cases = [
            // (markdown, should anchor)
            ("![](./plot.png)", true),
            ("![alt]()", false),
            ("![alt](./plot.txt)", false),
            ("![alt](./plot.png\u{7})", false),
            (&format!("![{long_alt}](./plot.png)"), true),
            (&format!("![alt]({long_path})"), false),
        ];
        for (md, should_anchor) in cases {
            let lines = render_with(md, Some(40), content_opts(&plot_opts()));
            assert_eq!(
                lines.iter().any(|line| line.image.is_some()),
                should_anchor,
                "{md:?}"
            );
            // Whatever the verdict, the caption/alt text never overflows.
            for line in &lines {
                assert!(line.width() <= 40, "over-wide line for {md:?}");
            }
        }
        // A zero-width render produces no anchor (there is nowhere to draw).
        let zero = render_with("![alt](./plot.png)", Some(0), content_opts(&plot_opts()));
        assert!(zero.iter().all(|line| line.image.is_none()));
    }

    #[test]
    fn anchors_are_per_document_not_per_cell_prefix() {
        // Two anchors in one document both carry their own geometry.
        let lines = render_with(
            "![a](./plot.png)\n\ntext\n\n![b](./plot.png)",
            Some(80),
            content_opts(&plot_opts()),
        );
        let anchors = lines
            .iter()
            .filter_map(|line| line.image.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(anchors.len(), 2);
        assert!(anchors.iter().all(|a| a.rows == 30 && a.cols == 80));
    }

    // ── Multi-line alt ─────────────────
    //
    // A soft break inside the label flushes the line the label started on
    // (`Event::SoftBreak` → `flush_line`), so the `label_start_segment_idx`
    // recorded at `Tag::Image` can point past the end of the *new* line. That
    // used to be an unchecked slice: `foo \`bar\` ![l1\nl2](a.png)` panicked
    // with `range start index 3 out of range for slice of length 1`. Every
    // shape below is a spelling of the same state, and none of them may panic
    // — or anchor, since the image is no longer alone on its line.

    /// Cross-line alts with the number of in-line segments *before* the label
    /// (the crash needed ≥ 2: one segment survives the flush, so `[2..]` and
    /// `[3..]` are the out-of-range ones) and in every enclosing block.
    const CROSS_LINE_ALT_SHAPES: &[&str] = &[
        // Minimal input: two segments (`foo ` + code `bar`).
        "foo `bar` ![l1\nl2](a.png)",
        // Two adjacent inline-code segments, and a longer prefix.
        "`a` `b` ![l1\nl2](a.png)",
        "*em* and `code` both ![l1\nl2](a.png) then",
        // Exactly one segment before the label (the boundary that stayed in
        // range by luck: `[1..]` of a one-segment line is empty, not a panic).
        "x ![l1\nl2](a.png)",
        // No segment before the label at all.
        "![l1\nl2](a.png)",
        // The same state inside every block that can hold a paragraph.
        "- item ![l1\nl2](plot.png)",
        "- parent\n  - nested ![l1\nl2](plot.png)",
        "> quoted ![l1\nl2](plot.png)",
        "# heading ![l1\nl2](plot.png)",
        "| a | b |\n|---|---|\n| ![l1\nl2](plot.png) | x |",
        "```markdown\n![l1\nl2](plot.png)\n```",
        // A hard break is the same event class (two trailing spaces).
        "foo `bar` ![l1  \nl2](a.png)",
    ];

    /// The metadata table for the cross-line shapes: both spellings the corpus
    /// uses, with a shape that *would* anchor if the image were alone.
    fn cross_line_opts() -> ImageOpts {
        image_opts(&[("plot.png", 800, 600), ("a.png", 800, 600)])
    }

    #[test]
    fn cross_line_alt_never_panics_and_stays_on_the_link_path() {
        for md in CROSS_LINE_ALT_SHAPES {
            let on = render_with(md, Some(80), content_opts(&cross_line_opts()));
            let off = render_with(md, Some(80), RenderOpts::new(Profile::Content, true));
            assert!(
                on.iter().all(|line| line.image.is_none()),
                "a cross-line alt was anchored: {md:?}"
            );
            // …and the rendering is the link path, span for span: the guard
            // only stops the panic, it does not change what is drawn.
            assert_eq!(fingerprint(&off), fingerprint(&on), "{md:?}");
        }
    }

    #[test]
    fn cross_line_alt_keeps_the_baseline_rendering() {
        // Pinned segment by segment (this is what
        // `origin/develop` renders for it — verified against the baseline
        // binary, see the step's design.md):
        //   line 1: `foo ` · inline code `bar` · ` ` · link label `l1`
        //   line 2: the label's last line, `l2`, with its (shown) destination
        let lines = render_with(
            "foo `bar` ![l1\nl2](a.png)",
            Some(80),
            content_opts(&cross_line_opts()),
        );
        let flat = lines
            .iter()
            .flat_map(|line| line.segments.iter())
            .map(|seg| (seg.kind, seg.text.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            flat,
            vec![
                (SegmentKind::Text, "foo "),
                (SegmentKind::InlineCode, "bar"),
                (SegmentKind::Text, " "),
                (SegmentKind::Link, "l1"),
                (SegmentKind::Link, "l2 (a.png)"),
            ]
        );
    }

    #[test]
    fn cross_line_alt_survives_math_and_images_off() {
        // Both switches are unable to dodge the panic (it is in
        // the parser, before either option is consulted) — pin that both are
        // clean now, and that the output does not depend on the math mode.
        let math_off = ThemePalette {
            math_mode: MathMode::Off,
            ..ThemePalette::default()
        };
        for md in CROSS_LINE_ALT_SHAPES {
            // `images: off` (the default tier) + `math: off`.
            let off = render_markdown_lines_with(
                md,
                Some(80),
                &math_off,
                RenderOpts::new(Profile::Content, true),
            );
            assert!(off.iter().all(|line| line.image.is_none()), "{md:?}");
            // `images: anchor` + `math: off`: still no anchor, still no panic.
            let anchored = render_markdown_lines_with(
                md,
                Some(80),
                &math_off,
                content_opts(&cross_line_opts()),
            );
            assert_eq!(
                fingerprint(&off),
                fingerprint(&anchored),
                "images=anchor math=off for {md:?}"
            );
            // `math: off` renders the same text as `math: text` (no `$` in the
            // corpus) — the option is not what decides the outcome here.
            let text =
                render_markdown_lines_with(md, Some(80), &dp(), content_opts(&cross_line_opts()));
            assert_eq!(fingerprint(&anchored), fingerprint(&text), "{md:?}");
        }
    }
}
