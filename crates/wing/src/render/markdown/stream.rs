//! StreamingRender — block-level incremental markdown rendering.
//!
//! A streaming cell (Reasoning / assistant content) renders through a
//! **stable prefix + active tail** model instead of re-rendering the whole
//! accumulated text on every frame:
//!
//! - The splitter scans incoming bytes line-by-line and cuts the source
//!   into markdown **blocks**. A block whose closing line has arrived is
//!   rendered once (parse + compose) and promoted into an immutable prefix
//!   of `flat`; it is never re-rendered (until a width change forces a full
//!   rebuild).
//! - The active tail (the last, unclosed block) is re-rendered each sync,
//!   so per-frame cost is O(tail), not O(text).
//! - Fenced code blocks get a line-level cache: complete body lines render
//!   once (stateful syntect `HighlightLines`, identical in both profiles),
//!   and each sync only renders new lines. The composed top border +
//!   completed body lines are themselves stable, so a giant growing code
//!   block stays at O(new lines) per frame. A line that can still be
//!   retracted (the trailing blank run before a closing fence, the in-flight
//!   partial line) is held back and re-rendered each sync — see
//!   [`fill_code_cache`].
//!
//! Correctness contract: for the same final text and width, the
//! incremental result is span-identical to [`full_lines`] — the reference
//! full render with the same profile options. `finalize()` replaces the
//! incremental state with exactly that reference, so any transient drift
//! converges at turn end.
//!
//! Known limit — slice-isolated parsing: each block is parsed on its own,
//! so anything CommonMark resolves at **document** scope stops working
//! once the definition and its use land in different slices. The one seen
//! in practice is reference-style links (`[foo]: /url` in an earlier block,
//! `[foo]` in a later one): while streaming, the later block renders the
//! literal `[foo]` as plain text instead of a link. It converges at
//! `finalize()`. Sharing a reference map across slices would defeat the
//! stable-prefix model, so this is accepted rather than fixed (see the
//! `shapes()` note in `tests/stream_render_reconcile.rs`).
//!
//! Known limit — list interrupting a paragraph: a list marker on the line
//! right after paragraph text (`para` / `- item`) opens a list in pulldown,
//! but the splitter keeps the line inside the paragraph slice (it only looks
//! for fences and blank lines while a paragraph is open). The slice still
//! renders correctly — the marker is inside it — but content that belongs to
//! the item (an indented continuation paragraph) is then sliced as an
//! indented block, which renders it with paragraph blanks the doc-context
//! parse suppresses inside list items. Visible as extra blank lines while
//! streaming; `finalize()` converges.
//!
//! Known limit — lazy continuation after a promoted block: a line that the
//! doc-context parse reads as a list item's lazy continuation (indented, no
//! blank line before it) is its own slice here, so it parses as a plain
//! paragraph and loses the item's continuation prefix until `finalize()`.
//!
//! Known limit — nested fences: CommonMark has no nested code fences, so a
//! model that wraps a fenced draft in another fence (`Draft:` + ```` ```markdown ````
//! … ```bash … ``` … ```` ``` ````) gets a spec-mandated pairing: one bare
//! fence closes the outer block, the parity of everything after it flips, and
//! prose can end up inside a code block (or vice versa). No local rule
//! recovers the author's intent — that needs a global pairing optimization,
//! which the stable-prefix model cannot honor. Rendering follows CommonMark
//! exactly here (as any other markdown renderer does).
//!
//! Block-boundary rules (see the design doc): fences open/close code
//! blocks and interrupt paragraphs (code needs its own mode for the line
//! cache); lists swallow blank lines (loose lists) and close on
//! non-list content after a blank; everything else (paragraphs, headings,
//! tables, blockquotes, rules) lives in one "paragraph" slice whose
//! INTERNAL structure is decided by the markdown parser itself — the
//! splitter only decides when a slice is final. That keeps the splitter
//! conservative: a misjudged boundary can only delay promotion (perf), or
//! produce a transient visual difference corrected by `finalize` — with
//! the reference-link case above as the documented exception.
//!
//! Separator semantics replicate the full renderer exactly: a blank line
//! follows paragraph/heading/list/table blocks but NOT code blocks or
//! blockquotes-ending-in-code; the separator is emitted lazily when the
//! next block starts, so a trailing blank never dangles at the end of the
//! stream (matching the full render's trailing-blank trim).
//!
//! **Thinking vs Content.** The three rendering rules `Profile` owns are the
//! only place the profiles differ, plus one shared rule: inline ````
//! normalization (skipped for reasoning, which discusses fences in prose),
//! indented (4-space) blocks (prose for reasoning, code for assistant
//! content) — and math delimiter normalization (`\(…\)` / `\[…\]` / a bare
//! AMS environment → `$…$` / `$$…$$`, see `super::math`), which applies to
//! both profiles and is therefore also safe per slice: it only fires on a
//! complete, code-free span inside one blank-line-delimited block, and a
//! slice boundary IS a blank line or a fence. Fenced blocks render
//! identically: highlight, gutter, borders. The cell compose is what
//! recolors reasoning prose (`thinking_segment_style`); math keeps its own
//! color in both.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::images::{
    ImageAnchor, ImageOpts, ImageSpan, cover_span, image_side_channel, row_is_cover_row,
};
use super::links::{ComposedLines, LinkSpan, compose_lines, line_link_spans};
use super::profile::Profile;
use super::types::{
    MarkdownLine, MarkdownSegment, MarkdownTheme, SegmentKind, thinking_segment_style,
};
use super::{RenderOpts, render_markdown_lines_with};
use crate::config::ThemePalette;
use crate::render::syntax::{highlight_line_with, new_highlighter};

// ============================================================
// Splitter — line state machine over the source buffer
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Between blocks (inside a blank run). No open block.
    Gap,
    /// Open paragraph-ish slice — paragraphs, headings, tables,
    /// blockquotes, rules all live here; the parser handles internal
    /// structure. Closes on a blank line or a fence opener.
    Paragraph,
    /// Open list slice — blank lines stay (loose lists), lazy
    /// continuations stay; closes on non-list content after a blank.
    ///
    /// `fence` is the fence running *inside* the item (`- ~~~`, `  ``` `):
    /// while it is open nothing may be cut — the normalization scanner is
    /// inside that fence too, and a cut there would make the two disagree
    /// about which lines are code (review r3).
    List {
        blank_seen: bool,
        fence: Option<FenceTrack>,
    },
    /// Open fenced code block. Blank lines are content; closes only on a
    /// matching (or longer) fence line.
    ///
    /// The track is [`FenceTrack`] — the shared state machine; this mode only
    /// adds the slice bookkeeping around it.
    FencedCode(FenceTrack),
    /// A fence that carries a block prefix (`> ~~~`, `- ~~~`): the parser
    /// still reads it as a code block (prefixes are resolved first), so the
    /// slice must not cut at the blank lines inside it — a slice starting
    /// inside a fence body would render its content as prose.
    ///
    /// It deliberately does NOT use the fenced-code path: `FencedCode` drives
    /// the line-level code cache, which expects a bare fence (language on the
    /// opener line, no prefix in the body). A prefixed fence stays one
    /// paragraph-ish slice and renders through the generic path, which is
    /// exactly what the document-level render does with those lines.
    PrefixedFence {
        track: FenceTrack,
        /// Mode to return to when the fence closes: a list slice keeps its
        /// blank bookkeeping.
        resume_list: bool,
    },
    /// Open indented (4-space) code block. Closes on a non-blank line
    /// indented fewer than 4 spaces.
    IndentedCode,
    /// Terminal state after `finalize`.
    Finished,
}

/// A block closed by the splitter, awaiting promotion (render + compose)
/// at the next sync.
///
/// The post-block separator is NOT decided here — it is derived at
/// promotion time from the renderer's own behavior (see `sync`).
struct ClosedBlock {
    /// Byte range of the block's source slice in `buf`.
    start: usize,
    end: usize,
    /// Live code cache moved out of the tail when a fenced block closes —
    /// lets promotion compose cached lines without re-highlighting.
    /// None for diff blocks (whole-block renderer) and non-fence slices.
    code: Option<CodeCache>,
}

struct Splitter {
    mode: Mode,
    /// Byte offset where the current (unclosed) tail slice starts.
    tail_start: usize,
    /// Byte offset just past the last complete line handed to the state
    /// machine. Always at a line boundary (or buf end).
    scan: usize,
    /// Whether the current list item has any content beyond its marker.
    /// pulldown keeps post-blank column-0 text INSIDE an empty list item
    /// (lazy continuation) — matching that boundary keeps the slice
    /// structure in agreement with the parser.
    list_item_has_content: bool,
    /// Blocks closed since the last sync, pending promotion.
    closed: Vec<ClosedBlock>,
}

impl Splitter {
    fn reset(&mut self) {
        self.mode = Mode::Gap;
        self.tail_start = 0;
        self.scan = 0;
        self.list_item_has_content = false;
        self.closed.clear();
    }
}

// ============================================================
// Code block line cache
// ============================================================

/// Incremental cache for one fenced code block's body lines.
///
/// Width-independent: rendered body lines survive syncs and width
/// rebuilds; only promotion composes them into cell-final lines.
struct CodeCache {
    /// Language token from the opener line (None = plain block).
    lang: Option<String>,
    /// Rendered body lines (gutter/border/content, markdown-level).
    /// Never holds the in-flight trailing line.
    rendered: Vec<MarkdownLine>,
    /// Byte offset (in the body text, trailing-newline-trimmed) of the
    /// next line to render. Only moves forward: a line that stops being
    /// the trailing partial line is picked up from here exactly once, and
    /// the cursor stays put while the line is still incomplete. This is
    /// what keeps a growing block O(new lines) per sync instead of
    /// re-scanning the whole body (`lines().count()` / `nth()`).
    scan_off: usize,
    /// Stateful highlighter positioned after `rendered.len()` lines
    /// (Content profile with a known language; None otherwise).
    highlighter: Option<HighlightLines<'static>>,
    /// Current gutter number width (≥3 while the block has a language and
    /// highlighting is on). Grows at powers of ten; rewrite is O(lines)
    /// and happens at most log10(n) times per block.
    gutter_width: usize,
}

impl CodeCache {
    fn new(
        lang: Option<String>,
        highlighter: Option<HighlightLines<'static>>,
        gutter_width: usize,
    ) -> Self {
        Self {
            lang,
            rendered: Vec::new(),
            scan_off: 0,
            highlighter,
            gutter_width,
        }
    }
}

/// Bookkeeping for the OPEN fenced code block's already-composed lines.
///
/// The block's top border and its completed body lines live in `flat` as
/// part of the stable prefix — each sync appends only newly completed
/// lines. The trailing partial line and the bottom border are re-appended
/// (transient) and sit beyond `stable_len`.
///
/// Dropped (`None`) whenever the composed region becomes invalid: a width
/// rebuild, the block's promotion, or a gutter-width rewrite.
#[derive(Clone, Copy)]
struct CodeFlat {
    /// flat index of the block's top border.
    start: usize,
    /// Cached body lines already composed after the border.
    body_lines: usize,
}

// ============================================================
// FlatLines — composed lines and their side channels
// ============================================================

/// The composed lines of a streaming cell, with the side channels that must
/// stay index-parallel to them: the markdown link spans (OSC8 injection and
/// click hit-testing) and the image anchors (the drawing layer).
///
/// Every mutation goes through this type, so the three vectors cannot drift
/// apart — a `truncate` that missed one of them would silently misplace every
/// link and every picture below it.
#[derive(Default)]
struct FlatLines {
    lines: Vec<Line<'static>>,
    /// `links[i]` belongs to `lines[i]` (empty when the line has no link).
    links: Vec<Vec<LinkSpan>>,
    /// `images[i]` belongs to `lines[i]` (empty unless the line opens an
    /// anchor).
    images: Vec<Vec<ImageSpan>>,
}

impl FlatLines {
    fn len(&self) -> usize {
        self.lines.len()
    }

    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    fn truncate(&mut self, len: usize) {
        self.lines.truncate(len);
        self.links.truncate(len);
        self.images.truncate(len);
    }

    /// Append the cell's trailing blank line (no side channels).
    fn push_blank(&mut self) {
        self.lines.push(Line::from(""));
        self.links.push(Vec::new());
        self.images.push(Vec::new());
        self.assert_parallel();
    }

    /// Replace the whole buffer (used by `finalize`).
    fn set(
        &mut self,
        lines: Vec<Line<'static>>,
        links: Vec<Vec<LinkSpan>>,
        images: Vec<Vec<ImageSpan>>,
    ) {
        self.lines = lines;
        self.links = links;
        self.images = images;
        self.assert_parallel();
    }

    fn slices(&self) -> (&[Line<'static>], &[Vec<LinkSpan>], &[Vec<ImageSpan>]) {
        (&self.lines, &self.links, &self.images)
    }

    fn assert_parallel(&self) {
        debug_assert_eq!(self.links.len(), self.lines.len());
        debug_assert_eq!(self.images.len(), self.lines.len());
    }
}

// ============================================================
// StreamingRender
// ============================================================

/// Incremental renderer for one streaming cell.
pub struct StreamingRender {
    profile: Profile,
    /// Source buffer (fence-normalized on append — identical to what the
    /// full path's `ensure_fences_on_own_line` would produce).
    buf: String,
    /// Fence-normalization cursor: lowest byte offset not yet checked for
    /// a fence glued to text. Kept at `buf.len() - 2` (2 bytes of overlap
    /// so a ``` split across two deltas is still seen) — restarting at the
    /// splitter's line cursor instead would make `push` O(unscanned tail)
    /// for a stream that goes a long way without a newline.
    norm_cursor: usize,
    split: Splitter,
    /// Cell-final lines plus their side channels: promoted closed blocks +
    /// separators, then the tail and one trailing cell blank (managed by
    /// `sync`).
    flat_lines: FlatLines,
    /// Image options the cell is rendering with — owned, because the engine
    /// is long-lived and has to notice a metadata change (a late probe
    /// changes the row count, which forces a rebuild).
    image_opts: ImageOpts,
    /// Length of the promoted (immutable) prefix of `flat` — including the
    /// open fenced block's composed lines (top border + completed body
    /// lines), which are stable in exactly the same sense.
    stable_len: usize,
    /// Separator pending after the last promoted block — emitted when the
    /// next block starts (never dangles at stream end).
    pending_sep: bool,
    /// Whether the last composed line is a TAIL separator — a separator the
    /// current frame emitted before the active tail. Unlike a promoted
    /// block's separator the tail can collapse to nothing on the next sync
    /// (an indented block that resolves to an empty list item, a list item
    /// still being typed), so the line is provisional: it is dropped and the
    /// pending flag restored before the tail is re-rendered.
    tail_sep: bool,
    /// Rendering width; None until the first `lines()` call.
    width: Option<u16>,
    /// Live code cache for the tail when it is an unclosed fenced block.
    code_tail: Option<CodeCache>,
    /// Composed-line bookkeeping for that same open block.
    code_flat: Option<CodeFlat>,
    finalized: bool,
    dirty: bool,
}

impl StreamingRender {
    pub fn new(profile: Profile) -> Self {
        Self::with_images(profile, ImageOpts::default())
    }

    /// [`new`](Self::new) with image anchors configured (see [`ImageOpts`]).
    pub fn with_images(profile: Profile, images: ImageOpts) -> Self {
        Self {
            profile,
            buf: String::new(),
            norm_cursor: 0,
            split: Splitter {
                mode: Mode::Gap,
                tail_start: 0,
                scan: 0,
                list_item_has_content: false,
                closed: Vec::new(),
            },
            flat_lines: FlatLines::default(),
            image_opts: images,
            stable_len: 0,
            pending_sep: false,
            tail_sep: false,
            width: None,
            code_tail: None,
            code_flat: None,
            finalized: false,
            dirty: true,
        }
    }

    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The image options this engine renders with.
    pub fn image_opts(&self) -> &ImageOpts {
        &self.image_opts
    }

    /// Adopt new image options.
    ///
    /// Metadata arrives asynchronously (a header probe finishing), and it
    /// changes how many rows an anchor reserves — so a change invalidates
    /// every composed line and forces a full rebuild at the next render, the
    /// same mechanism a width change uses.
    pub fn set_image_opts(&mut self, images: ImageOpts) {
        if self.image_opts == images {
            return;
        }
        self.image_opts = images;
        self.width = None;
    }

    pub fn is_finalized(&self) -> bool {
        self.finalized
    }

    /// Append a streaming delta.
    ///
    /// Fence-normalizes the new bytes (matching the full path's
    /// preprocessor), then advances the splitter over complete lines.
    pub fn push(&mut self, delta: &str) {
        debug_assert!(
            !self.finalized,
            "StreamingRender::push after finalize is a caller bug"
        );
        if delta.is_empty() || self.finalized {
            return;
        }
        self.buf.push_str(delta);
        self.normalize_fences();
        self.scan();
        self.dirty = true;
    }

    /// Current cell-final lines at `width`. Syncs the tail if dirty.
    ///
    /// The returned slice ends with the cell's trailing blank line, and
    /// every line is guaranteed ≤ `width` display columns (over-wide
    /// lines are hard-wrapped) — ready for direct blitting.
    pub fn lines(&mut self, width: u16, palette: &ThemePalette) -> &[Line<'static>] {
        self.composed(width, palette).lines
    }

    /// [`lines`](Self::lines) plus the link spans of every returned line.
    ///
    /// Both slices are index-aligned (`links[i]` belongs to `lines[i]`).
    pub fn lines_and_links(
        &mut self,
        width: u16,
        palette: &ThemePalette,
    ) -> (&[Line<'static>], &[Vec<LinkSpan>]) {
        let rendered = self.composed(width, palette);
        (rendered.lines, rendered.links)
    }

    /// The cell's composed lines with **both** side channels: link spans and
    /// image anchors, both index-aligned with `lines`.
    ///
    /// This is the accessor the ui layer wants: [`lines`](Self::lines) and
    /// [`lines_and_links`](Self::lines_and_links) are narrow views of it.
    pub fn composed(&mut self, width: u16, palette: &ThemePalette) -> StreamLines<'_> {
        if self.finalized && self.width == Some(width) {
            return self.stream_lines();
        }
        if self.width != Some(width) {
            // First render at this width (or a width change): full
            // deterministic rebuild from the buffer.
            self.rebuild(width, palette);
            return self.stream_lines();
        }
        if self.dirty {
            self.sync(width, palette);
        }
        self.stream_lines()
    }

    /// The current buffer's slices, without syncing.
    fn stream_lines(&self) -> StreamLines<'_> {
        let (lines, links, images) = self.flat_lines.slices();
        StreamLines {
            lines,
            links,
            images,
        }
    }

    /// Height in terminal rows — valid after the latest `lines()` call.
    pub fn height(&self) -> usize {
        self.flat_lines.len()
    }

    /// Terminal: replace the incremental state with the reference full
    /// render for this profile.
    ///
    /// After finalize the cell must not receive further deltas.
    pub fn finalize(&mut self, width: u16, palette: &ThemePalette) {
        let composed = full_render(&self.buf, width, self.profile, palette, &self.image_opts);
        let (lines, links, images) = composed.into_parts();
        self.flat_lines.set(lines, links, images);
        self.stable_len = self.flat_lines.len();
        self.pending_sep = false;
        self.tail_sep = false;
        self.code_tail = None;
        self.code_flat = None;
        self.split.reset();
        self.split.mode = Mode::Finished;
        self.width = Some(width);
        self.finalized = true;
        self.dirty = false;
    }

    // ------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------

    /// Full deterministic rebuild (first render at a width, or width
    /// change): reset the splitter, re-scan the buffer, re-promote
    /// everything, re-render the tail.
    fn rebuild(&mut self, width: u16, palette: &ThemePalette) {
        let buf = std::mem::take(&mut self.buf);
        let profile = self.profile;
        let images = std::mem::take(&mut self.image_opts);
        *self = StreamingRender::with_images(profile, images);
        self.buf = buf;
        self.scan();
        // The buffer was already fence-normalized as it was appended.
        self.norm_cursor = self.buf.len().saturating_sub(2);
        self.dirty = true;
        self.finalized = false;
        self.width = Some(width);
        self.sync(width, palette);
    }

    /// Incremental sync: promote newly-closed blocks, re-render the tail,
    /// append the cell trailing blank. Requires `self.width == Some(width)`.
    ///
    /// Separator semantics are DERIVED from the renderer itself: a
    /// promoted block renders with `trim_trailing_blank: false`, and the
    /// presence of the renderer's own trailing blank line is exactly what
    /// the doc-context full render would emit at that boundary
    /// (paragraph/heading/list/table ends push one via `push_blank_line`;
    /// code blocks, HTML blocks and quotes ending in code do not). The
    /// promotion pops that blank and re-emits it lazily before the NEXT
    /// block, so it never dangles at stream end.
    fn sync(&mut self, width: u16, palette: &ThemePalette) {
        self.dirty = false;

        // 0) A closed block invalidates the open fenced block's composed
        //    region (below it is promoted, which re-renders it whole).
        if !self.split.closed.is_empty() {
            drop_code_flat(
                &mut self.flat_lines,
                &mut self.stable_len,
                &mut self.code_flat,
            );
        }

        // 1) Promote newly-closed blocks (in order).
        self.flat_lines.truncate(self.stable_len);
        if std::mem::take(&mut self.tail_sep) {
            // The tail is about to be re-rendered and may have collapsed
            // since — re-derive the separator instead of keeping the last
            // frame's.
            self.pending_sep = true;
        }
        let closed = std::mem::take(&mut self.split.closed);
        for block in closed {
            let emitted_sep = if self.pending_sep {
                push_separator(&mut self.flat_lines, width, palette, self.profile);
                self.pending_sep = false;
                true
            } else {
                false
            };
            let slice = self.buf[block.start..block.end].to_string();
            let mut md_lines = match block.code {
                Some(mut cache) => code_block_markdown_lines(
                    &mut cache,
                    &slice,
                    &MarkdownTheme::from_palette(palette),
                ),
                None => render_block(&slice, width, self.profile, palette, &self.image_opts),
            };
            // The renderer's trailing blank IS the separator signal.
            let sep = md_lines.last().is_some_and(|l| l.segments.is_empty());
            if sep {
                md_lines.pop();
            }
            if md_lines.is_empty() {
                // The block contributes no lines (e.g. an empty
                // blockquote renders to just a blank). In doc context its
                // blank dedups against an already-present separator blank —
                // replicate that: keep the pending state only when no
                // separator was emitted for this block.
                self.pending_sep = sep && !emitted_sep;
                continue;
            }
            compose_into(&mut self.flat_lines, md_lines, width, palette, self.profile);
            self.pending_sep = sep;
        }
        self.stable_len = self.flat_lines.len();
        self.flat_lines.assert_parallel();

        // 2) Render the active tail (doc-end semantics: trailing blanks
        // trimmed, matching the full render at the same text).
        let tail_start = self.split.tail_start;
        let mode = self.split.mode.clone();
        if matches!(mode, Mode::FencedCode(_)) && self.code_tail.is_some() {
            // Cached (non-diff) fenced code: append only newly completed
            // body lines — the block's earlier lines stay in the prefix.
            self.sync_fenced_tail(tail_start, width, palette);
        } else if tail_start < self.buf.len() {
            // Everything else (paragraphs, lists, quotes, indented code,
            // diff fences): re-render the tail slice. Diff fences render
            // through the generic path because their output is whole-block
            // (file summaries, metadata stripping via `group_diff_by_file`).
            let md_lines = render_generic(
                &self.buf[tail_start..],
                width,
                self.profile,
                palette,
                &self.image_opts,
            );
            // Emit the pending separator only when the tail actually
            // renders content (a blank-run tail keeps it pending — the
            // next block will trigger it). The separator is stable from
            // the moment it is emitted — fold it into the stable prefix
            // so the next sync's truncate keeps it.
            if !md_lines.is_empty() && self.pending_sep {
                push_separator(&mut self.flat_lines, width, palette, self.profile);
                self.pending_sep = false;
                self.tail_sep = true;
            }
            compose_into(&mut self.flat_lines, md_lines, width, palette, self.profile);
        }

        // 3) Cell trailing blank (matches the non-streaming cell renders).
        self.flat_lines.push_blank();
    }

    /// Sync the tail when it is an unclosed fenced code block backed by a
    /// line cache: append the block's newly completed body lines to its
    /// composed prefix (O(new lines) per sync), then re-append the
    /// transient trailing line and bottom border.
    fn sync_fenced_tail(&mut self, tail_start: usize, width: u16, palette: &ThemePalette) {
        let theme = MarkdownTheme::from_palette(palette);
        let profile = self.profile;
        let mut cache = self.code_tail.take().expect("caller checked is_some");
        let slice = &self.buf[tail_start..];

        let (body_trimmed, has_language) = code_slice_parts(slice, &mut cache);
        let (pending, total_lines) = fill_code_cache(&mut cache, body_trimmed, &theme);

        let number_width = code_number_width(&cache, total_lines);
        if number_width != cache.gutter_width && cache.gutter_width > 0 {
            // A power of ten was crossed: every cached gutter changes, so
            // the composed lines are stale and are rebuilt in full (at
            // most log10(n) times per block).
            rewrite_gutters(&mut cache, number_width);
            cache.gutter_width = number_width;
            drop_code_flat(
                &mut self.flat_lines,
                &mut self.stable_len,
                &mut self.code_flat,
            );
        }

        if self.code_flat.is_none() {
            // First composition of this block at this width (or a rebuild
            // after promotion / gutter growth): the block's separator,
            // then the top border — both stable from here on.
            if self.pending_sep {
                push_separator(&mut self.flat_lines, width, palette, profile);
                self.pending_sep = false;
                self.stable_len += 1;
            }
            let start = self.flat_lines.len();
            compose_into(
                &mut self.flat_lines,
                std::iter::once(code_top_border(has_language, cache.lang.as_deref(), &theme)),
                width,
                palette,
                profile,
            );
            self.code_flat = Some(CodeFlat {
                start,
                body_lines: 0,
            });
        }

        // Append only the body lines completed since the last sync.
        let composed = self.code_flat.as_ref().expect("just ensured").body_lines;
        if composed < cache.rendered.len() {
            compose_into(
                &mut self.flat_lines,
                cache.rendered[composed..].iter().cloned(),
                width,
                palette,
                profile,
            );
            self.code_flat.as_mut().expect("just ensured").body_lines = cache.rendered.len();
        }
        // The composed block prefix (border + completed lines) is stable.
        self.stable_len = self.flat_lines.len();

        // Transient: the provisional tail (a trailing blank run and/or the
        // in-flight partial line — see `fill_code_cache`), rendered through a
        // highlighter owned by this sync (the committed one must not advance
        // on lines that may still be retracted; one instance for the whole
        // run also keeps multi-line constructs closer to their committed
        // colors, and avoids re-scanning the syntax set per line), then the
        // bottom border.
        let mut pending_hl = cache.lang.as_deref().and_then(new_highlighter);
        for (i, line) in pending.iter().enumerate() {
            let number = cache.rendered.len() + i + 1;
            let md =
                render_code_line_stateless(line, number, number_width, &theme, pending_hl.as_mut());
            compose_into(
                &mut self.flat_lines,
                std::iter::once(md),
                width,
                palette,
                profile,
            );
        }
        compose_into(
            &mut self.flat_lines,
            std::iter::once(code_bottom_border(&theme)),
            width,
            palette,
            profile,
        );

        self.code_tail = Some(cache);
        self.flat_lines.assert_parallel();
    }

    /// Apply the fence-on-own-line normalization to unchecked bytes.
    ///
    /// Insertions can only occur inside the current (incomplete) line —
    /// i.e. at offsets ≥ the splitter's line cursor — so splitter offsets
    /// stay valid. The normalization cursor (`norm_cursor`, kept 2 bytes
    /// behind the buffer end) upholds that invariant: a ``` found here can
    /// only start at or after the splitter's last line boundary, because
    /// the bytes before it are either line-terminated content or too short
    /// to hold a fence.
    ///
    /// **Thinking profile**: reasoning text often contains inline ``` references
    /// to discuss code fences (e.g. `（```rust）`). Normalizing these into
    /// line-level fences would create spurious code blocks with wrong language
    /// tags, swallowing the rest of the reasoning inside a code block border.
    /// Skip normalization entirely — inline ``` stays as literal text. Genuine
    /// line-start code blocks in reasoning are still detected by the splitter
    /// via [`fence_open`].
    fn normalize_fences(&mut self) {
        if !self.profile.normalizes_inline_fences() {
            self.norm_cursor = self.buf.len().saturating_sub(2);
            return;
        }
        let mut i = self.norm_cursor;
        let bytes = self.buf.as_bytes();
        let mut insertions: Vec<usize> = Vec::new();
        let mut deferred: Option<usize> = None;
        while let Some(rel) = find_sub(&bytes[i..], b"```") {
            let at = i + rel;
            if at + 3 >= bytes.len() {
                // The fence run touches the buffer end, so the byte that
                // decides whether this is a fence (or a fence at EOF) has
                // not arrived: stop here and re-examine it on the next
                // push. Deciding now would insert a newline the full path
                // (which sees the whole text) never would — e.g. an
                // indented closing fence `  ```  ` split right after its
                // backticks would be torn out of the code block.
                deferred = Some(at);
                break;
            }
            debug_assert!(
                at >= self.split.scan,
                "fence insertion below the splitter cursor would shift its offsets"
            );
            let at_line_start =
                at == 0 || bytes[at - 1] == b'\n' || super::fence_prefix_is_blank(bytes, at);
            let after = &bytes[(at + 3).min(bytes.len())..];
            let looks_like_fence =
                after.is_empty() || after[0] == b'\n' || after[0].is_ascii_alphanumeric();
            if !at_line_start && looks_like_fence {
                insertions.push(at);
            }
            i = at + 3;
        }
        for &at in insertions.iter().rev() {
            self.buf.insert(at, '\n');
        }
        self.norm_cursor = match deferred {
            // Insertions below the sighting shift it right by one each.
            Some(at) => at + insertions.iter().filter(|&&p| p < at).count(),
            // 2 bytes of overlap so a fence split across two deltas is
            // seen by the next pass.
            None => self.buf.len().saturating_sub(2),
        };
    }

    /// Advance the splitter over all complete lines in unscanned bytes.
    ///
    /// The line is copied out of the buffer so the state machine can mutate
    /// splitter state without fighting the buffer borrow (line-sized
    /// allocation, negligible next to rendering).
    fn scan(&mut self) {
        loop {
            let (line_start, line_end, next) = {
                let scan = self.split.scan;
                match self.buf[scan..].find('\n') {
                    Some(nl) => (scan, scan + nl, scan + nl + 1),
                    None => break,
                }
            };
            let line = self.buf[line_start..line_end].to_string();
            let consumed = self.handle_complete_line(&line, line_start, next);
            if consumed {
                self.split.scan = next;
            }
            // When a slice closed, the closing line is reprocessed in the
            // new mode (scan stays at line_start).
        }
    }

    /// Feed one complete line to the state machine.
    ///
    /// Returns `true` when the line was consumed; `false` means a slice
    /// closed at this line and the line must be reprocessed in the new
    /// mode.
    fn handle_complete_line(&mut self, line: &str, line_start: usize, _next: usize) -> bool {
        match std::mem::replace(&mut self.split.mode, Mode::Gap) {
            Mode::Finished => {
                self.split.mode = Mode::Finished;
                true // ignore (should not happen)
            }
            Mode::Gap => {
                if line_is_blank(line) {
                    self.split.mode = Mode::Gap;
                    return true;
                }
                // A real block starts here: move the tail start off the
                // leading blank run onto this line (the blank run was the
                // gap separator, not block content).
                self.split.tail_start = line_start;
                if let Some((fc, fl, info)) = fence_open(line) {
                    self.open_code_cache(fc, fl, info);
                    self.split.mode = Mode::FencedCode(FenceTrack::plain(fc, fl));
                } else if list_marker_len(line).is_some() {
                    // The line opens a list item — and possibly the item's own
                    // fence (`- ~~~`), which the List mode then has to keep
                    // track of (see `Mode::List`).
                    self.split.list_item_has_content = false;
                    self.split.mode = Mode::List {
                        blank_seen: false,
                        fence: fence_opener(line),
                    };
                } else if let Some(track) = prefixed_fence_open(line) {
                    self.split.mode = Mode::PrefixedFence {
                        track,
                        resume_list: false,
                    };
                } else if indent_of(line) >= 4 {
                    self.split.mode = Mode::IndentedCode;
                } else {
                    self.split.mode = Mode::Paragraph;
                }
                true
            }
            Mode::Paragraph => {
                if line_is_blank(line) {
                    self.close_slice(line_start);
                    self.split.mode = Mode::Gap;
                    return true;
                }
                if let Some((fc, fl, info)) = fence_open(line) {
                    // A fence interrupts the paragraph (and needs its own
                    // mode for the line-level code cache).
                    self.close_slice(line_start);
                    self.open_code_cache(fc, fl, info);
                    self.split.mode = Mode::FencedCode(FenceTrack::plain(fc, fl));
                    return true;
                }
                if let Some(track) = prefixed_fence_open(line) {
                    // A `> ~~~` / `- ~~~` fence: keep it (and the blank lines
                    // of its body) inside this slice — see `PrefixedFence`.
                    self.split.mode = Mode::PrefixedFence {
                        track,
                        resume_list: false,
                    };
                    return true;
                }
                self.split.mode = Mode::Paragraph;
                true
            }
            Mode::List { blank_seen, fence } => {
                if let Some(track) = fence {
                    // A fence opened inside the item is still running: the
                    // scanner is inside it too, so nothing may be cut here.
                    let (next, step) = track.step(line);
                    match step {
                        FenceStep::Body => {
                            self.split.mode = Mode::List {
                                blank_seen,
                                fence: Some(next),
                            };
                            return true;
                        }
                        FenceStep::Closes => {
                            // The closer is item content: consumed here, and
                            // the item goes on.
                            self.split.mode = Mode::List {
                                blank_seen,
                                fence: None,
                            };
                            return true;
                        }
                        FenceStep::OpensTopLevel => {
                            let (fc, fl, info) = fence_open(line)
                                .expect("FenceStep::OpensTopLevel implies a fence opener");
                            self.close_slice(line_start);
                            self.open_code_cache(fc, fl, info);
                            self.split.mode = Mode::FencedCode(FenceTrack::plain(fc, fl));
                            return true;
                        }
                    }
                }
                if line_is_blank(line) {
                    self.split.mode = Mode::List {
                        blank_seen: true,
                        fence: None,
                    };
                    return true;
                }
                if indent_of(line) == 0
                    && let Some((fc, fl, info)) = fence_open(line)
                {
                    // A column-0 fence ends the list (an indented fence stays
                    // inside the item — pulldown keeps it there, and a slice
                    // cut out of the item would lose the list continuation
                    // prefix). The line is the OPENER and is consumed here —
                    // returning it to the FencedCode mode would make the
                    // splitter read it as its own closer again (a bare fence
                    // has no info string, so `is_fence_close` matches it),
                    // which promotes an empty block and renders the whole
                    // body as prose.
                    self.close_slice(line_start);
                    self.open_code_cache(fc, fl, info);
                    self.split.mode = Mode::FencedCode(FenceTrack::plain(fc, fl));
                    return true;
                }
                if let Some(track) = prefixed_fence_open(line) {
                    // A fence inside the item: blank lines in its body are
                    // content, not a list separator.
                    self.split.mode = Mode::PrefixedFence {
                        track,
                        resume_list: true,
                    };
                    return true;
                }
                let marker = list_marker_len(line).is_some();
                let indented = indent_of(line) >= 2;

                let lazy = !blank_seen;
                // pulldown keeps post-blank column-0 text inside an EMPTY
                // list item (lazy continuation) — match it so the slice
                // boundary agrees with the parser's block structure.
                let empty_item_continuation = blank_seen && !self.split.list_item_has_content;
                if marker {
                    self.split.list_item_has_content = false;
                    self.split.mode = Mode::List {
                        blank_seen: false,
                        fence: fence_opener(line),
                    };
                    true
                } else if indented || lazy || empty_item_continuation {
                    self.split.list_item_has_content = true;
                    self.split.mode = Mode::List {
                        blank_seen: false,
                        fence: fence_opener(line),
                    };
                    true
                } else {
                    self.close_slice(line_start);
                    self.split.mode = Mode::Gap;
                    false
                }
            }
            Mode::FencedCode(track) => {
                let (track, step) = track.step(line);
                if step == FenceStep::Closes {
                    self.close_slice(line_start + line.len() + 1);
                    self.split.mode = Mode::Gap;
                    // The cache moved into the closed block.
                    return true;
                }
                self.split.mode = Mode::FencedCode(track);
                true
            }
            Mode::PrefixedFence { track, resume_list } => {
                let (next, step) = track.step(line);
                match step {
                    FenceStep::Closes => {
                        // The fence ends and the container it lives in keeps
                        // going.
                        self.split.mode = if resume_list {
                            Mode::List {
                                blank_seen: true,
                                fence: None,
                            }
                        } else {
                            Mode::Paragraph
                        };
                        true
                    }
                    FenceStep::OpensTopLevel => {
                        // The container ended here and this line starts a new
                        // top-level fence. Keep the parser's reading: the line
                        // is that fence's OPENER and is consumed here.
                        let (fc, fl, info) = fence_open(line)
                            .expect("FenceStep::OpensTopLevel implies a fence opener");
                        self.close_slice(line_start);
                        self.open_code_cache(fc, fl, info);
                        self.split.mode = Mode::FencedCode(FenceTrack::plain(fc, fl));
                        true
                    }
                    // Blank lines and prefixed body lines are fence body, not
                    // a block separator.
                    FenceStep::Body => {
                        self.split.mode = Mode::PrefixedFence {
                            track: next,
                            resume_list,
                        };
                        true
                    }
                }
            }
            Mode::IndentedCode => {
                if line_is_blank(line) {
                    self.split.mode = Mode::IndentedCode;
                    return true;
                }
                if indent_of(line) >= 4 {
                    self.split.mode = Mode::IndentedCode;
                    true
                } else {
                    self.close_slice(line_start);
                    self.split.mode = Mode::Gap;
                    false
                }
            }
        }
    }

    /// Close the current slice: record a pending block over
    /// `[tail_start, end)` and reset the tail to `end`.
    fn close_slice(&mut self, end: usize) {
        let start = self.split.tail_start;
        let code = self.code_tail.take();
        self.split.closed.push(ClosedBlock { start, end, code });
        self.split.tail_start = end;
    }

    /// Open the code cache for a fence opener at `line_start`.
    fn open_code_cache(&mut self, _fence_char: u8, _fence_len: usize, info: &str) {
        let lang = info
            .split_whitespace()
            .next()
            .filter(|l| !l.is_empty())
            .map(str::to_string);
        // Diff blocks never cache: their renderer is whole-block (file
        // summaries, metadata stripping) and runs through the generic path.
        let is_diff = lang
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("diff"));
        if is_diff {
            self.code_tail = None;
            return;
        }
        // Both profiles highlight: a code block renders the same in
        // reasoning and assistant content. Only the language-less block
        // (and a language syntect does not know) falls back to plain.
        let highlighter = lang.as_deref().and_then(new_highlighter);
        let gutter_width = if lang.is_some() { 3 } else { 0 };
        self.code_tail = Some(CodeCache::new(lang, highlighter, gutter_width));
    }
}

/// Compose markdown-level lines into cell-final lines (prefix + thinking
/// recolor + hard wrap) and extend `flat`.
///
/// Replicates the full renderer's GLOBAL blank-line dedup at the batch
/// boundary: a standalone slice render can carry leading blank lines
/// (e.g. a table's `flush_paragraph`) that the doc-context render
/// would have deduplicated against the preceding block's blank — skip
/// them when `flat` already ends with a blank line.
///
/// Free function (not a method) so callers can hold a `&self.buf` slice
/// and a `&mut self.flat` at the same time — the incremental fenced-code
/// tail must read the buffer while appending lines.
fn compose_into<I>(
    flat: &mut FlatLines,
    md_lines: I,
    width: u16,
    palette: &ThemePalette,
    profile: Profile,
) where
    I: IntoIterator<Item = MarkdownLine>,
{
    let thinking_style = Style::default().fg(palette.thinking);
    let bullet_style = Style::default().fg(palette.text);
    let limit = width as usize;
    // The `⦁ ` prefix belongs to the FIRST line ever composed into an
    // empty flat buffer — the tail is re-composed every frame, so
    // "first" must be positional (flat empty), not a sticky flag.
    let cell_first_pending = flat.is_empty();
    // "Did the previous content already end this blank run?" — a markdown
    // blank line, not an anchor cover row (which is blank-looking but is part
    // of the anchor block above it). The cover-row exception is structural
    // (the row is inside an anchor's row range), never a content test: at
    // narrow widths a hard-wrapped prefix row is a lone space too.
    let flat_ends_blank = match flat.lines.len().checked_sub(1) {
        Some(last) => {
            !row_is_cover_row(&flat.images, last)
                && flat.lines[last]
                    .spans
                    .iter()
                    .all(|span| span.content.trim().is_empty())
        }
        None => false,
    };
    let mut out = Vec::new();
    let mut out_links = Vec::new();
    // Image anchors, riding on their caption row so the hard wrap below can
    // move them with the line they belong to (see `hard_wrap_lines_with_links`).
    let mut out_images: Vec<Option<ImageAnchor>> = Vec::new();
    for (i, md_line) in md_lines.into_iter().enumerate() {
        if flat_ends_blank && i == 0 && md_line.segments.is_empty() && !flat.is_empty() {
            // Dedup: the previous content already ended this blank run.
            continue;
        }
        // First content line of the whole cell: while nothing has been
        // composed yet (flat empty, nothing in this batch so far).
        let first = cell_first_pending && out.is_empty();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(md_line.segments.len() + 1);
        let prefix = match (profile, first) {
            (Profile::Thinking, true) => Span::styled("⦁ ".to_string(), thinking_style),
            (Profile::Thinking, false) => Span::styled("  ".to_string(), thinking_style),
            (Profile::Content, true) => Span::styled("⦁ ".to_string(), bullet_style),
            (Profile::Content, false) => Span::raw("  "),
        };
        spans.push(prefix);
        // Same 2-column prefix on every line — the link columns shift by it.
        out_links.push(shift_spans(line_link_spans(&md_line), PREFIX_WIDTH));
        let anchor = md_line.image.map(|boxed| *boxed);
        let anchor_rows = anchor.as_ref().map_or(1, |anchor| anchor.rows);
        for seg in md_line.segments {
            let style = match profile {
                Profile::Thinking => thinking_segment_style(seg.kind, seg.style, thinking_style),
                Profile::Content => seg.style,
            };
            spans.push(Span::styled(seg.text, style));
        }
        out.push(Line::from(spans));
        out_images.push(anchor);
        // An anchor's cover rows: the caption row is already pushed, these
        // are the blank rows the picture is painted over. Never the cell's
        // first line, so they take the continuation prefix.
        if anchor_rows > 1 {
            for _ in 1..anchor_rows {
                let prefix = match profile {
                    Profile::Thinking => Span::styled("  ".to_string(), thinking_style),
                    Profile::Content => Span::raw("  "),
                };
                out.push(Line::from(vec![prefix, cover_span()]));
                out_links.push(Vec::new());
                out_images.push(None);
            }
        }
    }
    let (wrapped, wrapped_links, wrapped_images) =
        hard_wrap_lines_with_links(out, out_links, out_images, limit);
    let base = flat.len();
    let wrapped_len = wrapped.len();
    // The side channel describes the whole buffer (rows = everything up to and
    // including this batch), with `base` where this batch lands.
    let side_channel = image_side_channel(&wrapped_images, base + wrapped_len, base, PREFIX_WIDTH);
    flat.lines.extend(wrapped);
    flat.links.extend(wrapped_links);
    // The side channel covers this batch's rows only; everything above `base`
    // is untouched (and unchanged).
    flat.images.truncate(base);
    flat.images.extend(side_channel);
    flat.assert_parallel();
}

/// Cell line prefix width (`⦁ ` / `  `) — links shift by this many columns.
const PREFIX_WIDTH: u16 = super::links::CELL_PREFIX_WIDTH;

/// Shift link columns right by `by` (the cell prefix).
fn shift_spans(spans: Vec<LinkSpan>, by: u16) -> Vec<LinkSpan> {
    spans
        .into_iter()
        .map(|span| LinkSpan {
            start: span.start.saturating_add(by),
            end: span.end.saturating_add(by),
            target: span.target,
        })
        .collect()
}

/// Drop the open fenced block's composed lines (invalidated by a
/// promotion, a gutter-width rewrite, or a rebuild). Free function so the
/// caller can hold the buffer borrow that triggered the invalidation.
fn drop_code_flat(flat: &mut FlatLines, stable_len: &mut usize, code_flat: &mut Option<CodeFlat>) {
    if let Some(cf) = code_flat.take() {
        flat.truncate(cf.start);
        *stable_len = (*stable_len).min(cf.start);
    }
}

/// Emit the separator blank line between blocks.
///
/// The full renderer emits an empty `MarkdownLine` and lets the cell compose
/// prefix it, so the separator is composed the same way instead of being
/// hand-built: at the top of a cell it takes the first-line prefix (`⦁ `)
/// exactly like the reference, a block that collapsed into its own blank
/// line (an indented block resolving to an empty list item) reproduces the
/// reference's line, and every other position stays the usual two-space
/// indent. Free function so callers can hold a buffer borrow (the fenced
/// tail) while composing.
fn push_separator(flat: &mut FlatLines, width: u16, palette: &ThemePalette, profile: Profile) {
    compose_into(
        flat,
        std::iter::once(MarkdownLine::default()),
        width,
        palette,
        profile,
    );
}

// ============================================================
// Reference full render (finalize + reconcile target)
// ============================================================

/// Borrowed view of a composition: the lines plus both side channels.
///
/// Returned by [`StreamingRender::composed`]; the owned counterpart is
/// [`ComposedLines`] (the cached, non-streaming form).
#[derive(Clone, Copy, Debug)]
pub struct StreamLines<'a> {
    /// The composed lines.
    pub lines: &'a [Line<'static>],
    /// Link spans, index-aligned with `lines`.
    pub links: &'a [Vec<LinkSpan>],
    /// Image anchors, index-aligned with `lines`.
    pub images: &'a [Vec<ImageSpan>],
}

/// The reference full-render pipeline for a streaming cell: markdown
/// render with profile options → cell compose (prefix + thinking recolor)
/// → hard wrap at `width` → one trailing blank line.
///
/// `finalize()` installs exactly this output; the reconcile tests assert
/// the incremental engine converges to it span-for-span.
pub fn full_lines(
    text: &str,
    width: u16,
    profile: Profile,
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    full_render(text, width, profile, palette, ImageOpts::off())
        .into_parts()
        .0
}

/// [`full_lines`] with explicit image options — the reference render the
/// streaming engine reconciles against when anchors are enabled.
pub fn full_render(
    text: &str,
    width: u16,
    profile: Profile,
    palette: &ThemePalette,
    images: &ImageOpts,
) -> ComposedLines {
    // The math mode travels with the palette, so the reference render and
    // the incremental engine always agree on it (see `RenderOpts::math`);
    // the image options travel explicitly for the same reason.
    let opts = RenderOpts::new(profile, true)
        .with_math(palette.math_mode)
        .with_images(images);
    let md = render_markdown_lines_with(text, Some(width.saturating_sub(2)), palette, opts);
    let thinking_style = Style::default().fg(palette.thinking);
    let bullet_style = Style::default().fg(palette.text);
    let limit = width as usize;

    let composed = compose_lines(
        &md,
        PREFIX_WIDTH,
        |i| match (profile, i == 0) {
            (Profile::Thinking, true) => Span::styled("\u{2981} ".to_string(), thinking_style),
            (Profile::Thinking, false) => Span::styled("  ".to_string(), thinking_style),
            (Profile::Content, true) => Span::styled("\u{2981} ".to_string(), bullet_style),
            (Profile::Content, false) => Span::raw("  "),
        },
        |kind, style| match profile {
            Profile::Thinking => thinking_segment_style(kind, style, thinking_style),
            Profile::Content => style,
        },
    );
    let (lines, links, tags) = hard_wrap_lines_with_links(
        composed.lines().to_vec(),
        composed.links().to_vec(),
        composed.image_tags(),
        limit,
    );
    let images = image_side_channel(&tags, lines.len(), 0, PREFIX_WIDTH);
    let mut composed = ComposedLines::with_images(lines, links, images);
    // The cell's trailing blank line (matches the non-streaming renders).
    composed.push_blank();
    composed
}

/// [`full_render`] with image anchors off, plus the link spans of every line
/// (see [`StreamingRender::lines_and_links`]).
pub fn full_lines_with_links(
    text: &str,
    width: u16,
    profile: Profile,
    palette: &ThemePalette,
) -> (Vec<Line<'static>>, Vec<Vec<LinkSpan>>) {
    let (lines, links, _images) =
        full_render(text, width, profile, palette, ImageOpts::off()).into_parts();
    (lines, links)
}

// ============================================================
// Rendering helpers
// ============================================================

/// Render a generic slice with doc-end semantics (trailing blanks
/// trimmed) — used for the ACTIVE TAIL, where the full render at the same
/// text would also trim.
fn render_generic(
    slice: &str,
    width: u16,
    profile: Profile,
    palette: &ThemePalette,
    images: &ImageOpts,
) -> Vec<MarkdownLine> {
    render_markdown_lines_with(
        slice,
        Some(width.saturating_sub(2)),
        palette,
        RenderOpts::new(profile, true)
            .with_math(palette.math_mode)
            .with_images(images),
    )
}

/// Render a PROMOTED block with `trim_trailing_blank: false`: the
/// renderer's own trailing blank line (pushed by paragraph/heading/list/
/// table end tags, absent after code/HTML blocks) is exactly the
/// separator the doc-context full render emits at this boundary — the
/// promotion pops it and re-emits it lazily.
fn render_block(
    slice: &str,
    width: u16,
    profile: Profile,
    palette: &ThemePalette,
    images: &ImageOpts,
) -> Vec<MarkdownLine> {
    render_markdown_lines_with(
        slice,
        Some(width.saturating_sub(2)),
        palette,
        RenderOpts::new(profile, false)
            .with_math(palette.math_mode)
            .with_images(images),
    )
}

/// Fenced-code borders (top carries the language label, bottom is fixed).
fn code_top_border(has_language: bool, lang: Option<&str>, theme: &MarkdownTheme) -> MarkdownLine {
    let label = if has_language {
        format!("┌─ {} ─", lang.unwrap_or_default())
    } else {
        "┌────────".to_string()
    };
    MarkdownLine {
        segments: vec![MarkdownSegment::new(
            SegmentKind::Border,
            theme.border,
            label,
        )],
        ..Default::default()
    }
}

fn code_bottom_border(theme: &MarkdownTheme) -> MarkdownLine {
    MarkdownLine {
        segments: vec![MarkdownSegment::new(
            SegmentKind::Border,
            theme.border,
            "└────────",
        )],
        ..Default::default()
    }
}

/// Opener/body split of a fenced-code slice.
///
/// Returns the body region with the trailing newline run intact (the
/// closing fence line is excluded when the slice has one) and whether the
/// opener carries a language label. Refreshes `cache.lang` from the
/// opener — it is the single source for the top-border label.
fn code_slice_parts<'a>(slice: &'a str, cache: &mut CodeCache) -> (&'a str, bool) {
    let opener_end = slice.find('\n').unwrap_or(slice.len());
    let opener = &slice[..opener_end];
    let (fence_info, _rest) = split_fence_line(opener);
    cache.lang = fence_info
        .split_whitespace()
        .next()
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    let has_language = cache.lang.is_some();

    // Body: between the opener line and (for closed blocks) the closing
    // fence line.
    let body = &slice[(opener_end + 1).min(slice.len())..];
    // The opener's fence signature — only a MATCHING fence closes it.
    let (fence_char, fence_len) = match fence_open(opener) {
        Some((fc, fl, _)) => (fc, fl),
        None => (b'`', 3),
    };
    // A closed slice ends with the closing fence line (plus its newline)
    // — exclude it from the body.
    let without_final_nl = body.strip_suffix('\n').unwrap_or(body);
    let last_line_start = without_final_nl.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let last_line = &without_final_nl[last_line_start..];
    let body_trimmed = if is_fence_close(last_line, fence_char, fence_len) {
        &body[..last_line_start]
    } else {
        body
    };
    (body_trimmed, has_language)
}

/// Drop the trailing `\r` of a NEWLINE-TERMINATED body line: pulldown
/// normalizes CRLF endings out of the code text, so the reference never
/// shows that byte — a CRLF stream would otherwise diverge on every line
/// (and on the span comparison in the reconcile matrix). A bare `\r` with
/// no line feed after it is content and must NOT be stripped: `str::lines`
/// keeps it, so it survives into the reference render.
fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// Render a fenced code block slice (opener + body [+ closer]) through
/// the line cache, replicating `render_code_block` output exactly: top
/// border with language label, gutter (Content + language only), per-line
/// highlighting (Content) or plain color (Thinking), bottom border —
/// including for unclosed blocks (matches the full path's
/// `finalize_unclosed_code_block`).
///
/// Used for PROMOTION (a closed block, once): O(lines) by design.
fn code_block_markdown_lines(
    cache: &mut CodeCache,
    slice: &str,
    theme: &MarkdownTheme,
) -> Vec<MarkdownLine> {
    let (body_trimmed, has_language) = code_slice_parts(slice, cache);
    let (pending, total_lines) = fill_code_cache(cache, body_trimmed, theme);
    let number_width = code_number_width(cache, total_lines);
    if number_width != cache.gutter_width && cache.gutter_width > 0 {
        rewrite_gutters(cache, number_width);
        cache.gutter_width = number_width;
    }

    // A closed block has no provisional tail: its body is complete and the
    // trailing newline run after the last body line is not part of the code
    // text (the reference trims it), so every pending line is dropped.
    debug_assert!(
        pending.is_empty(),
        "a closed block must have no pending lines: {pending:?}"
    );

    let mut lines = Vec::with_capacity(cache.rendered.len() + 2);
    lines.push(code_top_border(has_language, cache.lang.as_deref(), theme));
    lines.extend(cache.rendered.iter().cloned());
    lines.push(code_bottom_border(theme));
    lines
}

/// Gutter number width for a body of `total_lines` lines (0 = no gutter).
fn code_number_width(cache: &CodeCache, total_lines: usize) -> usize {
    if cache.gutter_width > 0 {
        total_lines.max(1).to_string().len().max(3)
    } else {
        0
    }
}

/// Fill the cache with the body lines that are FINAL, returning the
/// provisional tail (lines that may still disappear) and the body's total
/// line count.
///
/// A body line is final when nothing can move it again:
///
/// - It is complete — newline-terminated inside the body (an unterminated
///   last line is the in-flight partial one).
/// - It is not an EMPTY line with nothing but the trailing newline run
///   after it. The reference renderer trims that run
///   (`trim_end_matches('\n')`, CRLF pairs first — see `strip_cr`), so a
///   blank line that is currently the last thing in the body disappears the
///   moment the closing fence — or the doc end — arrives. Committing it
///   early leaves one body line the reference does not have until
///   `finalize`: the "extra empty line inside a streaming code block" bug,
///   visible whenever a chunk boundary lands between the blank line and the
///   closing fence (the cache saw the blank line as an interior line before
///   the fence closed the block).
///
/// Everything after the last final line — the trailing blank run plus, when
/// the body is mid-line, the partial line — is returned as `pending` and
/// re-rendered by the caller every sync (statelessly, since a line that can
/// still be retracted must not advance the highlighter state). Cost stays
/// O(final lines added): the byte cursor only moves forward, while the
/// pending tail is re-rendered and bounded by the blank run the model has
/// emitted so far (normally empty or a line or two, and each line is a
/// fresh stateless highlight — see [`render_code_line_stateless`]).
fn fill_code_cache<'a>(
    cache: &mut CodeCache,
    body_trimmed: &'a str,
    theme: &MarkdownTheme,
) -> (Vec<&'a str>, usize) {
    // `trimmed` drops the trailing newline run — exactly the bytes the
    // reference renderer drops, except that the bytes here are the RAW body
    // (pulldown hands the reference an LF-normalized copy), so CRLF endings
    // are trimmed as units. A bare `\r` is content, not an ending: the
    // reference keeps it (`str::lines` leaves it alone, see `strip_cr`).
    let trimmed = trim_trailing_code_endings(body_trimmed);
    let complete_tail = body_trimmed.ends_with('\n');
    let final_end = final_line_end(body_trimmed, trimmed.len(), complete_tail);
    let mut off = cache.scan_off;
    while off < final_end {
        let end = body_trimmed[off..]
            .find('\n')
            .map(|rel| off + rel)
            .expect("a final line is newline-terminated");
        let line = strip_cr(&body_trimmed[off..end]);
        let number = cache.rendered.len() + 1;
        let rendered = render_code_line_stateful(cache, line, number, theme);
        cache.rendered.push(rendered);
        off = end + 1;
    }
    cache.scan_off = final_end;

    // `final_end` is a body offset and may point one past `trimmed` (the last
    // line's terminator is the body's trailing newline run) — clamp it into
    // `trimmed` for the pending slice.
    let pending_src = &trimmed[final_end.min(trimmed.len())..];
    let raw: Vec<&str> = if pending_src.is_empty() {
        Vec::new()
    } else {
        pending_src.split('\n').collect()
    };
    let mut pending: Vec<&str> = Vec::with_capacity(raw.len());
    for (i, line) in raw.iter().enumerate() {
        if i + 1 < raw.len() || complete_tail {
            // Newline-terminated: the `\r` of a CRLF ending is not content.
            pending.push(strip_cr(line));
            continue;
        }
        // The in-flight partial line — pulldown emits no text for an
        // unterminated trailing line of 1–3 spaces at EOF, so the reference
        // has no such line either; anything else (a ≥4-space run, tabs, a
        // lone `\r`) is kept verbatim, un-stripped.
        if !(line.bytes().all(|b| b == b' ') && line.len() < 4) {
            pending.push(line);
        }
    }
    let total = cache.rendered.len() + pending.len();
    (pending, total)
}

/// The body without its trailing newline run — the bytes the reference
/// renderer drops (`trim_end_matches('\n')` on the LF-normalized code text).
///
/// Scanned from the END so mixed endings work out: a `\n` is consumed, and a
/// `\r` right in front of a consumed `\n` is part of that CRLF ending and
/// goes with it (`\r\n\n` = one CRLF line ending plus an empty line). A
/// trailing `\r` with no line feed after it is content and stops the scan.
fn trim_trailing_code_endings(body: &str) -> &str {
    let bytes = body.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &body[..end]
}

/// Whether a body line is blank for the reference renderer: empty, or
/// nothing but the single `\r` of a CRLF ending that pulldown normalizes
/// away.
///
/// Exactly one, not "any run of them": pulldown strips only the `\r` that
/// sits right before the `\n` (`append_code_text` appends the preceding
/// `\r`s as content), so `"\r\r\n"` is a body line `"\r"` in the
/// reference and must not be treated as a droppable blank line.
fn code_line_is_blank(line: &str) -> bool {
    line.is_empty() || line == "\r"
}

/// Byte offset in the BODY just past the last line that can never move
/// again: the last COMPLETE, non-blank line's terminator. 0 when there is
/// none (every line is still provisional). The offset may land past
/// `trimmed_len` — the last line's terminator then belongs to the body's
/// trailing newline run — so callers clamp before slicing `trimmed`.
///
/// Offsets are body coordinates on purpose: CRLF endings make the body
/// longer than `trimmed` while its terminators stay `\n` bytes (the `\r`
/// in front of one is content to `code_line_is_blank`), and the cache's
/// byte cursor lives in body coordinates.
///
/// `body_ends_with_newline` says whether the last line of `trimmed` is
/// terminated by the newline that starts the body's trailing run (the
/// reference keeps it), or is the in-flight partial line (it does not).
fn final_line_end(body: &str, trimmed_len: usize, body_ends_with_newline: bool) -> usize {
    // Walk the lines backwards: `line_end` is the current line's exclusive
    // end (a terminator index), `terminated` whether that terminator exists
    // in the body.
    let mut line_end = if body_ends_with_newline {
        // The first newline at or after the trimmed text is this line's
        // terminator (CRLF or LF alike).
        body[trimmed_len..]
            .find('\n')
            .map(|rel| trimmed_len + rel)
            .expect("a terminated last line has a newline")
    } else {
        body.len()
    };
    let mut terminated = body_ends_with_newline;
    loop {
        let start = body[..line_end].rfind('\n').map(|p| p + 1).unwrap_or(0);
        if terminated && !code_line_is_blank(&body[start..line_end]) {
            return line_end + 1;
        }
        if start == 0 {
            return 0;
        }
        line_end = start - 1;
        terminated = true;
    }
}

/// Render one COMPLETE body line, advancing the highlighter state.
fn render_code_line_stateful(
    cache: &mut CodeCache,
    line: &str,
    number: usize,
    theme: &MarkdownTheme,
) -> MarkdownLine {
    let mut md = MarkdownLine::default();
    if cache.gutter_width > 0 {
        md.push_segment(
            SegmentKind::Gutter,
            theme.code_block_gutter,
            &format!("{:>width$}  ", number, width = cache.gutter_width),
        );
    }
    md.push_segment(SegmentKind::Border, theme.border, "│ ");
    if let Some(hl) = cache.highlighter.as_mut()
        && let Some(ops) = highlight_line_with(hl, line)
    {
        for (style, text) in ops {
            md.push_segment(SegmentKind::CodeBlock, style, &text);
        }
        return md;
    }
    // Plain (Thinking profile, or a language syntect does not know).
    md.push_segment(SegmentKind::CodeBlock, theme.code_block, line);
    md
}

/// Render one PROVISIONAL body line with a caller-owned highlighter (see
/// `sync_fenced_tail`): the committed highlighter's state must not advance on
/// a line that can still be retracted, so the provisional run gets its own.
fn render_code_line_stateless(
    line: &str,
    number: usize,
    number_width: usize,
    theme: &MarkdownTheme,
    highlighter: Option<&mut HighlightLines<'static>>,
) -> MarkdownLine {
    let mut md = MarkdownLine::default();
    if number_width > 0 {
        md.push_segment(
            SegmentKind::Gutter,
            theme.code_block_gutter,
            &format!("{:>width$}  ", number, width = number_width),
        );
    }
    md.push_segment(SegmentKind::Border, theme.border, "│ ");
    if let Some(hl) = highlighter
        && let Some(ops) = highlight_line_with(hl, line)
    {
        for (style, text) in ops {
            md.push_segment(SegmentKind::CodeBlock, style, &text);
        }
        return md;
    }
    md.push_segment(SegmentKind::CodeBlock, theme.code_block, line);
    md
}

/// Rewrite the gutter segments of all cached lines at a new width
/// (happens at each power-of-ten crossing).
fn rewrite_gutters(cache: &mut CodeCache, number_width: usize) {
    for (i, line) in cache.rendered.iter_mut().enumerate() {
        if let Some(first) = line.segments.first_mut()
            && first.kind == SegmentKind::Gutter
        {
            first.text = format!("{:>width$}  ", i + 1, width = number_width);
        }
    }
}

// Diff blocks render whole-block through the generic path
// (`render_code_block` / `group_diff_by_file`), so no line-level diff
// coloring lives here any more.

// ============================================================
// Line classification helpers
// ============================================================

fn line_is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

pub(crate) fn indent_of(line: &str) -> usize {
    let mut n = 0;
    for b in line.bytes() {
        match b {
            b' ' => n += 1,
            b'\t' => return 4,
            _ => break,
        }
    }
    n
}

/// Leading whitespace measured in **columns**, with tabs advanced to the next
/// multiple of 4 — CommonMark's tab handling (§2.2). Use this whenever an
/// indentation is compared against something, and [`indent_bytes`] whenever a
/// line is sliced: the two are different coordinate systems and mixing them is
/// the defect class of review r4/r5.
pub(crate) fn indent_columns(line: &str) -> usize {
    let mut col = 0usize;
    for c in line.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col = (col / 4 + 1) * 4,
            _ => break,
        }
    }
    col
}

/// Number of leading whitespace **bytes** (spaces and tabs) — i.e. the byte
/// offset of the first non-whitespace character.
///
/// Distinct from [`indent_of`], which returns **columns** (a tab counts as
/// four): the two agree only up to three columns of spaces — and those are
/// exactly the cases in which the shape helpers below are allowed to look past
/// the indentation. Anything that *slices* a line must use this one; anything
/// that compares indentation against CommonMark's limits (≤3 for a fence or a
/// list marker, ≥4 for an indented block) must use `indent_of`.
fn indent_bytes(line: &str) -> usize {
    line.bytes()
        .take_while(|&b| b == b' ' || b == b'\t')
        .count()
}

/// The fenced-code state of a line sequence.
///
/// **Single source of truth** for "which lines are code": the streaming
/// splitter (slice boundaries) and the math normalization scanner (rewrite
/// suppression) both drive this type. They used to carry two hand-written
/// copies of the same rules, and the copies drifted — the scanner kept
/// treating a prefix-less fence line as a closer while the splitter already
/// knew it opens a new top-level fence (review r3), which rewrote code-block
/// content in the final render.
///
/// The rules below are the only place that decides; add a caller, not a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FenceTrack {
    char: u8,
    len: usize,
    /// The fence was opened behind a block prefix (`> ~~~`, `- ``` `). The
    /// parser resolves the prefix first, so such a fence ends when its
    /// container ends — not when some unrelated fence line shows up.
    prefixed: bool,
    /// The innermost list item's `(marker indent, content column)` — both in
    /// COLUMNS (tabs advanced to the next multiple of 4), so they can be
    /// compared with `indent_columns`. `None` = no list item in the chain.
    item_col: Option<(usize, usize)>,
    /// The tracked item has seen a non-blank content line since its marker.
    ///
    /// An EMPTY item ends at a blank line, so an indented, prefix-less fence
    /// line after blanks is a NEW top-level fence, not the item's content —
    /// `  - ~~~~/ - / ␣␣ / ␣␣ / ␤ / "  ~~~~"` swallows what follows, while the
    /// same shape without the blanks (`- ~~~/ - / "  ~~~"`) closes the fence
    /// and leaves the rest prose (pulldown, review r4 / S1).
    item_has_content: bool,
    /// A blank line has been seen since the last marker / content line.
    seen_blank: bool,
    /// A line that cannot be item content was seen (less indented than
    /// `content_col`): the item's content was interrupted, so an indented
    /// prefix-less fence line can no longer be read as its closer — the
    /// parser opens a new top-level fence there instead.
    container_gone: bool,
}

/// What a line does to a running fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FenceStep {
    /// Body of the running fence (blank lines included).
    Body,
    /// The line closes the running fence.
    Closes,
    /// The container the fence lives in ends here, and the line opens a NEW
    /// top-level fence which swallows what follows (CommonMark).
    OpensTopLevel,
}

impl FenceTrack {
    /// A fence opened by the line itself (`~~~`, `  ``` `).
    pub(crate) fn plain(char: u8, len: usize) -> Self {
        Self {
            char,
            len,
            prefixed: false,
            item_col: None,
            item_has_content: false,
            seen_blank: false,
            container_gone: false,
        }
    }

    /// A fence opened behind a block prefix (`content_col` = where the
    /// container's content starts).
    fn behind_prefix(char: u8, len: usize, p: Prefix) -> Self {
        Self {
            char,
            len,
            prefixed: true,
            item_col: p.item,
            // The fence line itself is the item's content.
            item_has_content: true,
            seen_blank: false,
            container_gone: false,
        }
    }

    /// What `line` does to this fence, and the track for the next line.
    pub(crate) fn step(mut self, line: &str) -> (Self, FenceStep) {
        let blank = line.trim().is_empty();
        // What the line means is decided by the state BEFORE it; the updates
        // below are for the next line.
        let had_content = self.item_has_content;
        let blank_before = self.seen_blank;
        if let Some((item_indent, item_col)) = self.item_col
            && !blank
            && indent_columns(line) < item_col
        {
            // A non-blank line that is not the item's content. Two cases:
            //
            // * a LIST ITEM MARKER (`- a`, `1. b`, `  - c`, `1. `): this item
            //   ended — a nested list would have to be indented at least at
            //   this item's content column, and the line is less indented than
            //   that. The marker opens a NEW item, whose own content column
            //   governs what follows (`1. ~~~` + `  - c` + `   ~~~`: the last
            //   line at column 3 is below the new item's column 4, so it is a
            //   top-level fence, exactly as the parser reads it). Adopt it;
            // * anything else (a paragraph line, a quote, …): the content was
            //   interrupted, so an indented fence line can no longer be the
            //   item's closer (see `container_gone`).
            //
            // Blank lines are fence body, not content that interrupts the item.
            if let Some(len) = list_marker_len(line) {
                let p = prefix(line);
                self.item_col = p.item.or(Some((item_indent, item_col)));
                // Whether the new item carries content on its marker line.
                self.item_has_content = line.get(len..).is_some_and(|rest| !rest.trim().is_empty());
                self.container_gone = false;
            } else {
                self.container_gone = true;
                self.item_has_content = true;
            }
            self.seen_blank = false;
        } else if blank {
            self.seen_blank = true;
        } else {
            self.item_has_content = true;
            self.seen_blank = false;
        }
        if !self.prefixed {
            // A plain fence: only a matching fence line (indent ≤3) ends it.
            let step = if is_fence_close(line, self.char, self.len) {
                FenceStep::Closes
            } else {
                FenceStep::Body
            };
            return (self, step);
        }
        let start = content_start(line);
        if start > 0 {
            // Still carrying a block prefix: only such a line can live in the
            // fence's container, so only such a line can close it.
            let step = if is_fence_close(&line[start..], self.char, self.len) {
                FenceStep::Closes
            } else {
                FenceStep::Body
            };
            return (self, step);
        }
        // No prefix left. Inside an intact list item an indented line is still
        // item content, so there an indented fence line is the item's OWN
        // closer (`- ~~~ … \n  ~~~`).
        if let Some((_, item_col)) = self.item_col
            && !self.container_gone
            && indent_columns(line) >= item_col
            // An empty item ended at the blank line: the fence line that
            // follows is a new top-level fence (see `item_has_content`).
            && (had_content || !blank_before)
        {
            let step = if is_fence_close(line, self.char, self.len) {
                FenceStep::Closes
            } else {
                FenceStep::Body
            };
            return (self, step);
        }
        // The container ended here. A fence opener starts a NEW top-level
        // fence which swallows what follows (CommonMark); anything else is
        // (conservatively) still body, so nothing inside gets rewritten.
        if fence_open(line).is_some() {
            return (self, FenceStep::OpensTopLevel);
        }
        (self, FenceStep::Body)
    }
}

/// The fence `line` opens, if any — a plain opener or one behind a block
/// prefix (`> ~~~`, `- ``` `, indented or not).
pub(crate) fn fence_opener(line: &str) -> Option<FenceTrack> {
    if let Some((fc, fl, _)) = fence_open(line) {
        return Some(FenceTrack::plain(fc, fl));
    }
    prefixed_fence_open(line)
}

/// If the line opens a fenced code block, return
/// `(fence_char, fence_len, info)`.
pub(crate) fn fence_open(line: &str) -> Option<(u8, usize, &str)> {
    if indent_of(line) >= 4 {
        return None;
    }
    let rest = &line[indent_bytes(line)..];
    let b = rest.as_bytes();
    if b.is_empty() {
        return None;
    }
    let fc = b[0];
    if fc != b'`' && fc != b'~' {
        return None;
    }
    let run = rest.bytes().take_while(|&c| c == fc).count();
    if run < 3 {
        return None;
    }
    let info = &rest[run..];
    // ``` followed by space + more backticks is inline code, not a fence —
    // pulldown treats it as a fence only as a block construct; keep the
    // simple rule: any fence run at line start opens a block.
    Some((fc, run, info))
}

/// Fence-close run length if the line consists only of fence chars
/// (≥1 run), else None.
fn fence_close_len(line: &str) -> Option<usize> {
    if indent_of(line) >= 4 {
        return None;
    }
    let rest = &line[indent_bytes(line)..];
    let b = rest.as_bytes();
    if b.is_empty() {
        return None;
    }
    let fc = b[0];
    if fc != b'`' && fc != b'~' {
        return None;
    }
    let run = rest.bytes().take_while(|&c| c == fc).count();
    if run >= 3 && rest[run..].trim().is_empty() {
        Some(run)
    } else {
        None
    }
}

/// A fence opener that carries a block prefix (`> ~~~`, `- ``` `): the
/// parser resolves the prefix first, so this is a fence in the document even
/// though the line does not start with the fence run. Returns
/// `(fence_char, run length)`.
pub(crate) fn prefixed_fence_open(line: &str) -> Option<FenceTrack> {
    let p = prefix(line);
    let content = &line[p.bytes..];
    if content.len() == line.len() {
        return None; // a bare fence — the caller handles those
    }
    let (fc, fl, _) = fence_open(content)?;
    Some(FenceTrack::behind_prefix(fc, fl, p))
}

pub(crate) fn is_fence_close(line: &str, fence_char: u8, fence_len: usize) -> bool {
    let Some(run) = fence_close_len(line) else {
        return false;
    };
    // `fence_close_len` tolerates up to 3 spaces of indent (CommonMark /
    // pulldown do the same) — the fence char must be compared AFTER that
    // indent, not at column 0, or an indented closer never closes.
    line.as_bytes()[indent_of(line)..].first() == Some(&fence_char) && run >= fence_len
}

/// Split a fence opener line into (info, rest-after-info) — used for
/// language extraction.
fn split_fence_line(opener: &str) -> (&str, &str) {
    // Callers only pass lines `fence_open` accepted (≤3 columns), where the
    // two indentation measures coincide; using the byte offset keeps the
    // slice safe by construction.
    let rest = &opener[indent_bytes(opener)..];
    let b = rest.as_bytes();
    if b.is_empty() {
        return ("", "");
    }
    let fc = b[0];
    let run = rest.bytes().take_while(|&c| c == fc).count();
    (&rest[run..], "")
}

/// The block prefix in front of a line's content, in **both** coordinate
/// systems — one walk, so nothing can disagree about where the content starts:
///
/// ```text
/// Prefix { bytes, columns, quotes, item: Option<(marker columns, content columns)> }
/// ```
///
/// * `bytes` — where to slice (the callers that need a `&str`);
/// * `columns` — where the content *is*, with tabs advanced to the next
///   multiple of 4 (CommonMark §2.2): indentation comparisons must use this,
///   never `bytes` (mixing them was the defect class of review r4/r5);
/// * `quotes` — how many `>` markers the chain has (an HTML block ends when its
///   own chain does, so it needs this);
/// * `item` — the innermost list item's content columns, from which the
///   fence tracker derives the item's content column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct Prefix {
    pub(crate) bytes: usize,
    pub(crate) columns: usize,
    pub(crate) quotes: usize,
    pub(crate) item: Option<(usize, usize)>,
}

/// Alias for callers that replace the pre-`Prefix` helper.
pub(crate) fn content_start(line: &str) -> usize {
    prefix(line).bytes
}

pub(crate) fn prefix(line: &str) -> Prefix {
    let mut out = Prefix::default();
    let mut off = 0usize;
    let mut col = 0usize;
    loop {
        let rest = &line[off..];
        // ≥4 columns of indentation is an indented block: what follows is code
        // content, never a `>` or a list marker. This is also the only case in
        // which the two coordinate systems differ by more than nothing at all,
        // so returning here keeps every slice below on a character boundary.
        if indent_columns(rest) >= 4 {
            out.bytes = off;
            out.columns = col;
            return out;
        }
        let rest_col = col + indent_columns(rest);
        let after = &rest[indent_bytes(rest)..];
        if after.starts_with('>') {
            out.quotes += 1;
            off += indent_bytes(rest) + 1;
            col = rest_col + 1;
            if line[off..].starts_with(' ') {
                off += 1;
                col += 1;
            }
            continue;
        }
        if let Some((content_bytes, content_col)) = list_marker_bounds(after, rest_col) {
            // The offset right after the marker's padding is exactly where this
            // item's content starts (in bytes *and* in columns).
            out.item = Some((rest_col, content_col));
            off += indent_bytes(rest) + content_bytes;
            col = content_col;
            continue;
        }
        out.bytes = off;
        out.columns = col;
        return out;
    }
}

/// Length of the list marker prefix (indent + marker + following space),
/// or None when the line doesn't start a list item.
fn list_marker_len(line: &str) -> Option<usize> {
    if indent_columns(line) >= 4 {
        return None;
    }
    let rest = &line[indent_bytes(line)..];
    let (content, _) = list_marker_bounds(rest, indent_columns(line))?;
    Some(indent_bytes(line) + content)
}

/// `(end of the marker, start of the item's content)` for the list marker at
/// the very start of `s` — CommonMark §5.2: 1–4 spaces of padding after the
/// marker make the content start after them, otherwise (no space, or 5+) after
/// the first space.
fn list_marker_bounds(s: &str, col0: usize) -> Option<(usize, usize)> {
    let b = s.as_bytes();
    let marker = match *b.first()? {
        b'-' | b'*' | b'+' => 1,
        c if c.is_ascii_digit() => {
            let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
            if !(1..=9).contains(&digits) || !matches!(b.get(digits), Some(b'.') | Some(b')')) {
                return None;
            }
            digits + 1
        }
        _ => return None,
    };
    // Padding is measured in COLUMNS with tabs advanced to the next multiple of
    // 4 (CommonMark §5.2 + tab handling): `-\titem` has 3 columns of padding,
    // `  - \titem` has 5 and therefore starts its content after one space — a
    // tab counted as a single byte would put the item's content column in the
    // wrong place and turn an indented code block into a paragraph
    // (review r5 / S3).
    let marker_end_col = col0 + marker;
    let mut at = marker;
    let mut col = marker_end_col;
    let mut pad_cols = 0usize;
    while let Some(&c) = b.get(at) {
        match c {
            b' ' => {
                col += 1;
                pad_cols += 1;
                at += 1;
            }
            b'\t' => {
                let next = (col / 4 + 1) * 4;
                pad_cols += next - col;
                col = next;
                at += 1;
            }
            _ => break,
        }
    }
    if pad_cols == 0 {
        // A marker must be followed by whitespace (or end the line): `-item`
        // is a paragraph, not a list item.
        return (marker == b.len()).then_some((marker, marker_end_col));
    }
    if pad_cols <= 4 || at == b.len() {
        Some((at, col))
    } else {
        // 5+ columns of padding: the content begins after the first whitespace
        // character (the rest is content — usually an indented code block).
        Some((marker + 1, marker_end_col + 1))
    }
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ============================================================
// Hard wrap — over-wide lines split at display width
// ============================================================

/// Split lines wider than `width` display columns, preserving span
/// styles. Prose is already pre-wrapped by the markdown pipeline; this
/// catches code/border/table lines (and any pathological over-wide
/// content) so every line in the flat buffer can be blitted directly.
///
/// Continuation lines drop leading spaces (break-at-space semantics) and
/// never re-emit the cell prefix.
#[cfg(test)]
fn hard_wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    hard_wrap_lines_with_links(lines, Vec::new(), Vec::new(), width).0
}

/// [`hard_wrap_lines`] keeping the side channels aligned with the output rows.
///
/// A line that fits keeps its spans unchanged (the common case — prose is
/// pre-wrapped upstream, so links are not split here). A line that has to be
/// split loses its spans: the split re-flows the text at arbitrary character
/// boundaries, and guessing where a link's text landed would risk pointing a
/// click at the wrong target — the link simply stays inactive on those rows.
///
/// `images` carries an image anchor **on its caption row** so that the anchor
/// follows its line through the split: the returned vector is index-aligned
/// with the output rows, and a split row drops its anchor (a re-flowed anchor
/// box has no meaningful geometry — it cannot happen in practice, the anchor's
/// own rows always fit the width).
fn hard_wrap_lines_with_links(
    lines: Vec<Line<'static>>,
    links: Vec<Vec<LinkSpan>>,
    images: Vec<Option<ImageAnchor>>,
    width: usize,
) -> (
    Vec<Line<'static>>,
    Vec<Vec<LinkSpan>>,
    Vec<Option<ImageAnchor>>,
) {
    let mut links = links;
    links.resize(lines.len(), Vec::new());
    let mut images = images;
    images.resize(lines.len(), None);
    if width == 0 {
        return (lines, links, images);
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut out_links = Vec::with_capacity(lines.len());
    let mut out_images = Vec::with_capacity(lines.len());
    for ((line, line_links), line_image) in lines.into_iter().zip(links).zip(images) {
        if line_width(&line) <= width {
            out.push(line);
            out_links.push(line_links);
            out_images.push(line_image);
            continue;
        }
        let mut cur: Vec<Span<'static>> = Vec::new();
        let mut cur_w = 0usize;
        for span in line.spans {
            let mut chunk = String::new();
            // After a break, consume the run of spaces that caused it
            // (break-at-space semantics) — but never strip a line's own
            // leading indent, which is meaningful in code blocks.
            let mut skipping_spaces = false;
            for ch in span.content.chars() {
                let cw = ch.width().unwrap_or(0);
                if cur_w + cw > width && cur_w > 0 {
                    if !chunk.is_empty() {
                        cur.push(Span::styled(std::mem::take(&mut chunk), span.style));
                    }
                    out.push(Line::from(std::mem::take(&mut cur)));
                    out_links.push(Vec::new());
                    out_images.push(None);
                    cur_w = 0;
                    skipping_spaces = true;
                }
                if skipping_spaces {
                    if ch == ' ' {
                        continue;
                    }
                    skipping_spaces = false;
                }
                chunk.push(ch);
                cur_w += cw;
            }
            if !chunk.is_empty() {
                cur.push(Span::styled(chunk, span.style));
            }
        }
        if !cur.is_empty() {
            out.push(Line::from(cur));
            out_links.push(Vec::new());
            out_images.push(None);
        }
    }
    debug_assert_eq!(out.len(), out_links.len());
    debug_assert_eq!(out.len(), out_images.len());
    (out, out_links, out_images)
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::super::images::ImageEntry;
    use super::*;

    /// Byte chunks on char boundaries (the probe's splitter).
    fn chunk_stream(text: &str, size: usize) -> Vec<&str> {
        let size = size.max(1);
        let mut chunks = Vec::new();
        let mut start = 0usize;
        while start < text.len() {
            let mut end = (start + size).min(text.len());
            while end < text.len() && !text.is_char_boundary(end) {
                end += 1;
            }
            chunks.push(&text[start..end]);
            start = end;
        }
        chunks
    }

    #[test]
    fn helpers_fence_detection() {
        assert!(fence_open("```rust").is_some());
        assert!(fence_open("  ~~~python").is_some());
        assert!(fence_open("    ```").is_none()); // indented ≥4 → not a fence opener here
        assert!(fence_open("``").is_none());
        assert!(fence_open("text").is_none());
        let (fc, len, info) = fence_open("```rust extra").unwrap();
        assert_eq!((fc, len), (b'`', 3));
        assert_eq!(info, "rust extra");

        assert_eq!(fence_close_len("```"), Some(3));
        assert_eq!(fence_close_len("`````"), Some(5));
        assert_eq!(fence_close_len("``` tail"), None);
        assert_eq!(fence_close_len("text"), None);
    }

    /// The shared fence rules (review r3): both the splitter and the math
    /// scanner drive `FenceTrack::step`, so these assertions pin the single
    /// source of truth. A prefix-less fence line after a PREFIXED fence is a
    /// new top-level fence, not that fence's closer — the rule the math
    /// scanner used to get wrong (it rewrote code-block content).
    #[test]
    fn fence_track_step_rules() {
        let step = |track: FenceTrack, line: &str| track.step(line).1;

        let plain = FenceTrack::plain(b'~', 3);
        // A plain fence closes on a matching fence line (indent ≤3), also when
        // it is longer; a prefixed fence line is body.
        assert_eq!(step(plain, "~~~"), FenceStep::Closes);
        assert_eq!(step(plain, "   ~~~~"), FenceStep::Closes);
        assert_eq!(step(plain, "~~~ tail"), FenceStep::Body);
        assert_eq!(step(plain, "> ~~~"), FenceStep::Body);
        assert_eq!(step(plain, "    ~~~"), FenceStep::Body);
        assert_eq!(step(plain, ""), FenceStep::Body);

        let quoted = prefixed_fence_open("> ~~~").unwrap();
        // Only a line that still carries the container's prefix can close it.
        assert_eq!(step(quoted, "> ~~~"), FenceStep::Closes);
        assert_eq!(step(quoted, "> text"), FenceStep::Body);
        assert_eq!(step(quoted, ">     x"), FenceStep::Body);
        // A prefix-less fence line ends the container: it opens a NEW
        // top-level fence (it does not close this one).
        assert_eq!(step(quoted, "~~~"), FenceStep::OpensTopLevel);
        assert_eq!(step(quoted, "~~~python"), FenceStep::OpensTopLevel);
        assert_eq!(step(quoted, "  ```"), FenceStep::OpensTopLevel); // wrong char, still a fence line
        assert_eq!(step(quoted, "```"), FenceStep::OpensTopLevel);

        let listed = prefixed_fence_open("- ~~~").unwrap();
        // Inside the item an indented line is item content: there the item's
        // own fence closer is just that.
        assert_eq!(step(listed, "  ~~~"), FenceStep::Closes);
        assert_eq!(step(listed, "  text"), FenceStep::Body);
        // At column 0 the item ended: a fence line starts a new top-level
        // fence.
        assert_eq!(step(listed, "~~~"), FenceStep::OpensTopLevel);

        // Blank lines are fence body: they neither close the fence nor end
        // the item (review r3: a fence body with blanks must keep its closer).
        let (listed, s) = listed.step("");
        assert_eq!(s, FenceStep::Body);
        assert_eq!(step(listed, "  ~~~"), FenceStep::Closes);

        // …but once a non-blank line that cannot be item content showed up, the
        // item's fence is over as well: an indented fence line is then a NEW
        // top-level fence, exactly as the parser reads it (review r3: the
        // corpus case `- ~~~\n> \n  ~~~\n\n\(x\) T`).
        let (listed, s) = listed.step("> ");
        assert_eq!(s, FenceStep::Body);
        assert_eq!(step(listed, "  ~~~"), FenceStep::OpensTopLevel);
    }

    /// `content_start` must never slice at a **column** count: `indent_of`
    /// reports a tab as four columns, and `&line[4..]` is not a character
    /// boundary in general (review r2 / B1: this panicked the TUI on any
    /// tab-indented line that reached the streaming splitter).
    #[test]
    fn content_start_is_tab_safe() {
        // Plain prefixes still resolve.
        assert_eq!(content_start("- item"), 2);
        assert_eq!(content_start("> quoted"), 2);
        assert_eq!(content_start("> > nested"), 4);
        assert_eq!(content_start("1. ordered"), 3);
        assert_eq!(content_start("text"), 0);

        // A tab is four COLUMNS: the line is an indented block, so no prefix
        // is stripped — and, crucially, nothing is sliced at column 4.
        for line in [
            "\tx",
            "\t- 中文项目",
            "\t- ",
            "  \t中文注释",
            "\t🙂x",
            "\t中文",
            "\t- item",
            "    x",
            "    - x",
        ] {
            assert_eq!(content_start(line), 0, "{line:?}");
        }
        // A tab INSIDE a container: the prefix is resolved, then the tab makes
        // what follows an indented block (no further stripping) — and the
        // offset stays a byte offset.
        assert_eq!(content_start("> \t"), 2);
        assert_eq!(indent_of(&"> \t"[content_start("> \t")..]), 4);
        // …and the same lines still go through the shape helpers unharmed.
        for line in ["\tx", "\t- 中文项目", "  \t中文注释", "\t🙂x"] {
            assert!(fence_open(&line[content_start(line)..]).is_none());
            assert!(fence_open(line).is_none());
            assert!(prefixed_fence_open(line).is_none());
            assert_eq!(indent_of(&line[content_start(line)..]) >= 4, true);
        }
        // A prefixed fence behind a prefix is still found (that is the r1 S2
        // fix, which must survive the tab-safety change).
        assert_eq!(
            prefixed_fence_open("> ~~~").map(|t| (t.char, t.len, t.prefixed, t.item_col)),
            Some((b'~', 3, true, None))
        );
        assert_eq!(
            prefixed_fence_open("- ```").map(|t| (t.char, t.len, t.prefixed, t.item_col)),
            Some((b'`', 3, true, Some((0, 2))))
        );
        assert_eq!(
            prefixed_fence_open("> - ~~~").map(|t| (t.char, t.len, t.item_col)),
            Some((b'~', 3, Some((2, 4)))),
            "the list item's own (marker indent, content column), not the fence's offset"
        );
        // Columns, not bytes: a tab advances to the next multiple of 4, so the
        // item's content column is 8 — an indented code block, as the parser
        // reads it (review r5 / S3).
        assert_eq!(
            prefixed_fence_open("  - \titem").map(|t| t.item_col),
            None,
            "`item` is the item's content, not a fence — but the walk still sees the item"
        );
        // Columns, not bytes: `" \t"` is 5 COLUMNS of padding, so the content
        // starts after the first whitespace character (the tab, byte 4) — its
        // own 4 columns of indent make it an indented code block, exactly as
        // the parser reads it (review r5 / S3).
        assert_eq!(
            prefix("  - \titem"),
            Prefix {
                bytes: 4,
                columns: 4,
                quotes: 0,
                item: Some((2, 4)),
            }
        );
        assert_eq!(indent_columns("\titem"), 4);
        assert_eq!(
            prefix("  -     x").bytes,
            4,
            "5 columns of padding → one space"
        );
        assert_eq!(prefix("  -     x").columns, 4);
        // 3 columns of padding (`-\t` from column 1 → 4) keep the content at
        // its real position.
        assert_eq!(prefix("-\titem").item, Some((0, 4)));
        assert_eq!(prefixed_fence_open("~~~"), None);
    }

    #[test]
    fn helpers_list_markers() {
        assert_eq!(list_marker_len("- item"), Some(2));
        assert_eq!(list_marker_len("  * item"), Some(4));
        assert_eq!(list_marker_len("1. ordered"), Some(3));
        assert_eq!(list_marker_len("23) parens"), Some(4));
        assert_eq!(list_marker_len("-item"), None);
        assert_eq!(list_marker_len("1.item"), None);
        assert_eq!(list_marker_len("    - indented"), None);
    }

    #[test]
    fn hard_wrap_splits_overwide() {
        let line = Line::from(vec![
            Span::raw("  "),
            Span::styled("abcdefghij".repeat(10), Style::default()),
        ]);
        let out = hard_wrap_lines(vec![line], 40);
        // 2 cols prefix + 100 cols content = 102 → fragments of ≤40.
        assert_eq!(out.len(), 3);
        for l in &out {
            assert!(line_width(l) <= 40);
        }
        // Continuations drop leading spaces and the prefix only appears on
        // the first fragment.
        assert!(out[0].to_string().starts_with("  "));
        assert!(!out[1].to_string().starts_with("  "));
    }

    #[test]
    fn hard_wrap_preserves_fit() {
        let line = Line::from("short");
        let out = hard_wrap_lines(vec![line], 40);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_string(), "short");
    }

    #[test]
    fn streaming_empty_matches_reference() {
        let palette = ThemePalette::default();
        for profile in [Profile::Thinking, Profile::Content] {
            let mut sr = StreamingRender::new(profile);
            let lines = sr.lines(80, &palette);
            let reference = full_lines("", 80, profile, &palette);
            assert_eq!(span_texts(lines), span_texts(&reference));
        }
    }

    fn span_texts(lines: &[Line<'static>]) -> Vec<(String, Style)> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    #[test]
    fn fill_code_cache_eof_trailing_line_rule() {
        // The cache must reproduce pulldown's line splitting for an
        // unterminated trailing line: 1-3 spaces emit nothing (EOF), a
        // ≥4-space run and any other content are kept as the partial line.
        // Probed against `full_lines` for each case.
        let theme = MarkdownTheme::default();
        let cases: &[(&str, usize, &[&str])] = &[
            ("let x = 1;\n", 1, &[]),
            ("let x = 1;\n  ", 1, &[]),
            ("let x = 1;\n   ", 1, &[]),
            ("let x = 1;\n    ", 1, &["    "]),
            ("let x = 1;\n\t", 1, &["\t"]),
            ("let x = 1;\n  `", 1, &["  `"]),
            ("let x = 1;\nlet y = 2;", 1, &["let y = 2;"]),
            ("let x = 1;\n\n", 1, &[]),
            ("let x = 1;\n\nlet y", 1, &["", "let y"]),
            // CRLF: `\r\n` endings are trimmed as units, and only
            // newline-terminated lines lose their `\r` — an unterminated
            // trailing `\r` is content (the reference renders it, probed
            // against `full_lines`).
            ("let x = 1;\r\n\r\n", 1, &[]),
            ("let x = 1;\r\n\r\nlet y\r\n", 3, &[]),
            ("let x = 1;\r\nlet y = 2;\r", 1, &["let y = 2;\r"]),
            ("let x = 1;\r\n\r\n`", 1, &["", "`"]),
            // A run of bare `\r`s is content: pulldown normalizes only the
            // `\r` adjacent to the `\n`, so this is one body line `"\r"`.
            ("\r\r\n", 1, &[]),
            ("let x = 1;\r\r\n", 1, &[]),
            // The trailing blank run is provisional: it is not final until a
            // non-empty line follows, because the reference renderer trims
            // it the moment the closing fence arrives (the mid-line "```"
            // here is what makes the blank line still interior).
            ("let x = 1;\n\n`", 1, &["", "`"]),
            ("let x = 1;\n\n\n```", 1, &["", "", "```"]),
        ];
        for &(body, complete, pending) in cases {
            let mut cache = CodeCache::new(None, None, 0);
            let (got, total) = fill_code_cache(&mut cache, body, &theme);
            assert_eq!(got.as_slice(), pending, "pending for {body:?}");
            assert_eq!(cache.rendered.len(), complete, "lines for {body:?}");
            assert_eq!(total, complete + pending.len());
        }
    }

    // ── Image anchors through the incremental engine ─────────────

    fn image_opts() -> ImageOpts {
        ImageOpts::anchor(
            Some(std::path::PathBuf::from("/ws")),
            vec![ImageEntry::new(
                std::path::PathBuf::from("/ws/plot.png"),
                super::super::images::ImageShape::new(800, 600),
            )],
        )
    }

    /// Every chunk split of the same text must land on the reference render —
    /// lines, links **and** image anchors.
    #[test]
    fn streaming_anchors_match_the_reference_render() {
        let palette = ThemePalette::default();
        let text =
            "before\n\n![销售趋势](./plot.png)\n\nafter the image\n\n![b](./plot.png)\n\nend";
        let images = image_opts();
        for profile in [Profile::Thinking, Profile::Content] {
            for chunk in [1usize, 3, 16, 256] {
                // Narrow widths are where the caption truncation and the cover
                // rows have to fit exactly — the anchor's geometry must still
                // agree with the reference.
                for width in [4u16, 5, 20, 80] {
                    let mut sr = StreamingRender::with_images(profile, images.clone());
                    for piece in chunk_stream(text, chunk) {
                        sr.push(piece);
                        let _ = sr.lines(width, &palette);
                    }
                    let rendered = sr.composed(width, &palette);
                    let got = (
                        rendered.lines.to_vec(),
                        rendered.links.to_vec(),
                        rendered.images.to_vec(),
                    );
                    let reference = full_render(text, width, profile, &palette, &images);
                    let want = reference.into_parts();
                    assert_eq!(
                        got.0.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
                        want.0.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
                        "profile={profile:?} chunk={chunk} width={width}"
                    );
                    assert_eq!(
                        got.1, want.1,
                        "links profile={profile:?} chunk={chunk} width={width}"
                    );
                    assert_eq!(
                        got.2, want.2,
                        "anchors profile={profile:?} chunk={chunk} width={width}"
                    );
                    assert_eq!(got.2.iter().flatten().count(), 2);
                    for line in rendered.lines {
                        assert!(
                            line.width() <= usize::from(width),
                            "over-wide line at width {width}: {line:?}"
                        );
                    }
                }
            }
        }
    }

    /// An over-wide line *above* an anchor is hard-wrapped into extra rows:
    /// the anchor's row index must move with it (the tag rides the line).
    #[test]
    fn streaming_anchor_rows_survive_a_wrap_above_them() {
        let palette = ThemePalette::default();
        let long = "a".repeat(80);
        let wrapped = format!("```\n{long}\n```\n\n![b](./plot.png)");
        let fitting = "```\nshort\n```\n\n![b](./plot.png)";
        let images = image_opts();
        let anchor_line = |text: &str, chunk: usize| {
            let mut sr = StreamingRender::with_images(Profile::Content, images.clone());
            for piece in chunk_stream(text, chunk) {
                sr.push(piece);
                let _ = sr.lines(30, &palette);
            }
            let rendered = sr.composed(30, &palette);
            let got = rendered.images.to_vec();
            let want = full_render(text, 30, Profile::Content, &palette, &images)
                .into_parts()
                .2;
            assert_eq!(got, want, "chunk={chunk}");
            let anchor = got.iter().flatten().next().expect("anchor");
            let caption = &rendered.lines[anchor.line].to_string();
            assert!(caption.contains('▢'), "caption row: {caption:?}");
            anchor.line
        };
        for chunk in [1usize, 8, 64] {
            let wrapped_line = anchor_line(&wrapped, chunk);
            let fitting_line = anchor_line(fitting, chunk);
            assert!(
                wrapped_line > fitting_line,
                "the widened code line must have moved the anchor down \
                 (wrapped={wrapped_line}, fitting={fitting_line})"
            );
        }
    }

    /// The engine notices a metadata change and rebuilds: a path that was a
    /// link becomes an anchor (and the row count changes with it).
    #[test]
    fn set_image_opts_rebuilds_the_composed_buffer() {
        let palette = ThemePalette::default();
        let text = "para\n\n![b](./plot.png)";
        let mut sr = StreamingRender::new(Profile::Content);
        for piece in chunk_stream(text, 8) {
            sr.push(piece);
        }
        let before = sr.composed(80, &palette);
        assert_eq!(before.images.iter().flatten().count(), 0);
        let link_rows = before.lines.len();

        sr.set_image_opts(image_opts());
        let after = sr.composed(80, &palette);
        let anchors = after.images.iter().flatten().collect::<Vec<_>>();
        assert_eq!(anchors.len(), 1);
        // The row count is computed from the *markdown* width (cell width
        // minus the 2-column prefix), which is where the anchor can live.
        assert_eq!(anchors[0].column, 2);
        assert_eq!(anchors[0].cols, 78);
        assert_eq!(
            anchors[0].rows,
            super::super::images::anchor_rows(78, super::super::images::ImageShape::new(800, 600))
        );
        assert!(
            after.lines.len() > link_rows,
            "the anchor must reserve rows"
        );

        // Setting the same options again is a no-op (no rebuild, no drift).
        let lines_before: Vec<String> = after.lines.iter().map(|l| l.to_string()).collect();
        sr.set_image_opts(image_opts());
        let again = sr.composed(80, &palette);
        assert_eq!(
            again
                .lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>(),
            lines_before
        );
    }

    /// `Off` stays byte-identical to the pre-image streaming output.
    #[test]
    fn streaming_with_images_off_matches_the_plain_render() {
        let palette = ThemePalette::default();
        let text = "text\n\n![b](./plot.png)\n\nmore";
        let mut plain = StreamingRender::new(Profile::Content);
        let mut off = StreamingRender::with_images(Profile::Content, ImageOpts::default());
        for piece in chunk_stream(text, 5) {
            plain.push(piece);
            off.push(piece);
        }
        let a = plain.composed(80, &palette);
        let b = off.composed(80, &palette);
        assert_eq!(a.lines, b.lines);
        assert_eq!(a.links, b.links);
        assert_eq!(a.images, b.images);
        assert!(a.images.iter().all(Vec::is_empty));
    }

    /// `finalize` installs the reference render — anchors included.
    #[test]
    fn finalize_installs_the_reference_anchors() {
        let palette = ThemePalette::default();
        let text = "a\n\n![b](./plot.png)\n\nc";
        let images = image_opts();
        let mut sr = StreamingRender::with_images(Profile::Content, images.clone());
        for piece in chunk_stream(text, 4) {
            sr.push(piece);
            let _ = sr.lines(60, &palette);
        }
        sr.finalize(60, &palette);
        let got = sr.composed(60, &palette);
        let (lines, links, anchor_rows) =
            (got.lines.to_vec(), got.links.to_vec(), got.images.to_vec());
        let (want_lines, want_links, want_anchors) =
            full_render(text, 60, Profile::Content, &palette, &images).into_parts();
        assert_eq!(
            lines.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
            want_lines.iter().map(|l| l.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(links, want_links);
        assert_eq!(anchor_rows, want_anchors);
    }

    #[test]
    fn streaming_paragraphs_match_reference() {
        let palette = ThemePalette::default();
        let text = "First para.\n\nSecond para with **bold** and `code`.\n\nThird.";
        for profile in [Profile::Thinking, Profile::Content] {
            let mut sr = StreamingRender::new(profile);
            // Feed as odd-sized chunks at char boundaries, syncing after
            // each — the mid-stream output must match the reference full
            // render at every step (the reconcile matrix covers this more
            // thoroughly; this is a minimal unit-test guard).
            let mut pos = 0;
            while pos < text.len() {
                let chunk_end = (pos + 7).min(text.len());
                // Never split a UTF-8 char.
                let end = text[..chunk_end]
                    .char_indices()
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(chunk_end);
                let chunk = &text[pos..end];
                sr.push(chunk);
                let _ = sr.lines(80, &palette);
                pos = end;
            }
            let streamed = sr.lines(80, &palette).to_vec();
            let reference = full_lines(text, 80, profile, &palette);
            assert_eq!(
                span_texts(&streamed),
                span_texts(&reference),
                "mid-stream profile {profile:?}"
            );
        }
    }
}
