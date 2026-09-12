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
//! - Fenced code blocks get a line-level cache in both profiles: complete
//!   body lines render once (Content: stateful syntect `HighlightLines`;
//!   Thinking: plain single-color), and each sync only renders new lines.
//!   The composed top border + completed body lines are themselves stable,
//!   so a giant growing code block stays at O(new lines) per frame.
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

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::types::{
    MarkdownLine, MarkdownSegment, MarkdownTheme, SegmentKind, thinking_segment_style,
};
use super::{RenderOpts, render_markdown_lines_with};
use crate::config::ThemePalette;
use crate::render::syntax::{highlight_line_with, new_highlighter};

// ============================================================
// Profile
// ============================================================

/// Rendering profile of a streaming cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Reasoning — code blocks render plain (no syntect, no gutter),
    /// while streaming AND in the final reconcile render.
    Thinking,
    /// Assistant content — code blocks keep syntax highlighting.
    Content,
}

impl Profile {
    fn code_highlight(self) -> bool {
        matches!(self, Profile::Content)
    }
}

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
    List { blank_seen: bool },
    /// Open fenced code block. Blank lines are content; closes only on a
    /// matching (or longer) fence line.
    FencedCode { fence_char: u8, fence_len: usize },
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
    /// Cell-final lines: promoted closed blocks + separators, then the
    /// tail and one trailing cell blank (managed by `sync`).
    flat: Vec<Line<'static>>,
    /// Length of the promoted (immutable) prefix of `flat` — including the
    /// open fenced block's composed lines (top border + completed body
    /// lines), which are stable in exactly the same sense.
    stable_len: usize,
    /// Separator pending after the last promoted block — emitted when the
    /// next block starts (never dangles at stream end).
    pending_sep: bool,
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
            flat: Vec::new(),
            stable_len: 0,
            pending_sep: false,
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
        if self.finalized && self.width == Some(width) {
            return &self.flat;
        }
        if self.width != Some(width) {
            // First render at this width (or a width change): full
            // deterministic rebuild from the buffer.
            self.rebuild(width, palette);
            return &self.flat;
        }
        if !self.dirty {
            return &self.flat;
        }
        self.sync(width, palette);
        &self.flat
    }

    /// Height in terminal rows — valid after the latest `lines()` call.
    pub fn height(&self) -> usize {
        self.flat.len()
    }

    /// Terminal: replace the incremental state with the reference full
    /// render (Content keeps highlighting; Thinking stays plain).
    ///
    /// After finalize the cell must not receive further deltas.
    pub fn finalize(&mut self, width: u16, palette: &ThemePalette) {
        let lines = full_lines(&self.buf, width, self.profile, palette);
        self.flat = lines;
        self.stable_len = self.flat.len();
        self.pending_sep = false;
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
        *self = StreamingRender::new(profile);
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
            drop_code_flat(&mut self.flat, &mut self.stable_len, &mut self.code_flat);
        }

        // 1) Promote newly-closed blocks (in order).
        self.flat.truncate(self.stable_len);
        let closed = std::mem::take(&mut self.split.closed);
        for block in closed {
            let emitted_sep = if self.pending_sep {
                self.flat.push(sep_line(self.profile, palette));
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
                None => render_block(&slice, width, self.profile, palette),
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
            compose_into(&mut self.flat, md_lines, width, palette, self.profile);
            self.pending_sep = sep;
        }
        self.stable_len = self.flat.len();

        // 2) Render the active tail (doc-end semantics: trailing blanks
        // trimmed, matching the full render at the same text).
        let tail_start = self.split.tail_start;
        let mode = self.split.mode.clone();
        if matches!(mode, Mode::FencedCode { .. }) && self.code_tail.is_some() {
            // Cached (non-diff) fenced code: append only newly completed
            // body lines — the block's earlier lines stay in the prefix.
            self.sync_fenced_tail(tail_start, width, palette);
        } else if tail_start < self.buf.len() {
            // Everything else (paragraphs, lists, quotes, indented code,
            // diff fences): re-render the tail slice. Diff fences render
            // through the generic path because their output is whole-block
            // (file summaries, metadata stripping via `group_diff_by_file`).
            let md_lines = render_generic(&self.buf[tail_start..], width, self.profile, palette);
            // Emit the pending separator only when the tail actually
            // renders content (a blank-run tail keeps it pending — the
            // next block will trigger it). The separator is stable from
            // the moment it is emitted — fold it into the stable prefix
            // so the next sync's truncate keeps it.
            if !md_lines.is_empty() && self.pending_sep {
                self.flat.push(sep_line(self.profile, palette));
                self.pending_sep = false;
                self.stable_len += 1;
            }
            compose_into(&mut self.flat, md_lines, width, palette, self.profile);
        }

        // 3) Cell trailing blank (matches the non-streaming cell renders).
        self.flat.push(Line::from(""));
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
        let (partial, total_lines) = fill_code_cache(&mut cache, body_trimmed, &theme);

        let number_width = code_number_width(&cache, total_lines);
        if number_width != cache.gutter_width && cache.gutter_width > 0 {
            // A power of ten was crossed: every cached gutter changes, so
            // the composed lines are stale and are rebuilt in full (at
            // most log10(n) times per block).
            rewrite_gutters(&mut cache, number_width);
            cache.gutter_width = number_width;
            drop_code_flat(&mut self.flat, &mut self.stable_len, &mut self.code_flat);
        }

        if self.code_flat.is_none() {
            // First composition of this block at this width (or a rebuild
            // after promotion / gutter growth): the block's separator,
            // then the top border — both stable from here on.
            if self.pending_sep {
                self.flat.push(sep_line(profile, palette));
                self.pending_sep = false;
                self.stable_len += 1;
            }
            let start = self.flat.len();
            compose_into(
                &mut self.flat,
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
                &mut self.flat,
                cache.rendered[composed..].iter().cloned(),
                width,
                palette,
                profile,
            );
            self.code_flat.as_mut().expect("just ensured").body_lines = cache.rendered.len();
        }
        // The composed block prefix (border + completed lines) is stable.
        self.stable_len = self.flat.len();

        // Transient: the in-flight trailing line (rendered statelessly —
        // the stateful highlighter cannot be cloned, and advancing it on a
        // partial line would corrupt the sequence) and the bottom border.
        if let Some(partial) = partial {
            let number = cache.rendered.len() + 1;
            let md = render_code_line_stateless(partial, &cache, number, number_width, &theme);
            compose_into(&mut self.flat, std::iter::once(md), width, palette, profile);
        }
        compose_into(
            &mut self.flat,
            std::iter::once(code_bottom_border(&theme)),
            width,
            palette,
            profile,
        );

        self.code_tail = Some(cache);
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
    fn normalize_fences(&mut self) {
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
            let at_line_start = at == 0 || bytes[at - 1] == b'\n';
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
                    self.split.mode = Mode::FencedCode {
                        fence_char: fc,
                        fence_len: fl,
                    };
                } else if list_marker_len(line).is_some() {
                    self.split.list_item_has_content = false;
                    self.split.mode = Mode::List { blank_seen: false };
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
                    self.split.mode = Mode::FencedCode {
                        fence_char: fc,
                        fence_len: fl,
                    };
                    return true;
                }
                self.split.mode = Mode::Paragraph;
                true
            }
            Mode::List { blank_seen } => {
                if line_is_blank(line) {
                    self.split.mode = Mode::List { blank_seen: true };
                    return true;
                }
                if let Some((fc, fl, info)) = fence_open(line) {
                    // A column-0 fence ends the list.
                    self.close_slice(line_start);
                    self.open_code_cache(fc, fl, info);
                    self.split.mode = Mode::FencedCode {
                        fence_char: fc,
                        fence_len: fl,
                    };
                    return false;
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
                    self.split.mode = Mode::List { blank_seen: false };
                    true
                } else if indented || lazy || empty_item_continuation {
                    self.split.list_item_has_content = true;
                    self.split.mode = Mode::List { blank_seen: false };
                    true
                } else {
                    self.close_slice(line_start);
                    self.split.mode = Mode::Gap;
                    false
                }
            }
            Mode::FencedCode {
                fence_char,
                fence_len,
            } => {
                if is_fence_close(line, fence_char, fence_len) {
                    self.close_slice(line_start + line.len() + 1);
                    self.split.mode = Mode::Gap;
                    // The cache moved into the closed block.
                    return true;
                }
                self.split.mode = Mode::FencedCode {
                    fence_char,
                    fence_len,
                };
                true
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
        // Thinking profile never highlights (plain single-color code).
        let highlighter = if self.profile.code_highlight() && lang.is_some() {
            new_highlighter(lang.as_deref().unwrap_or(""))
        } else {
            None
        };
        let show_gutter = self.profile.code_highlight() && lang.is_some();
        self.code_tail = Some(CodeCache::new(
            lang,
            highlighter,
            if show_gutter { 3 } else { 0 },
        ));
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
    flat: &mut Vec<Line<'static>>,
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
    let flat_ends_blank = flat
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()));
    let mut out = Vec::new();
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
        for seg in md_line.segments {
            let style = match profile {
                Profile::Thinking => thinking_segment_style(seg.kind, seg.style, thinking_style),
                Profile::Content => seg.style,
            };
            spans.push(Span::styled(seg.text, style));
        }
        out.push(Line::from(spans));
    }
    let wrapped = hard_wrap_lines(out, limit);
    flat.extend(wrapped);
}

/// Drop the open fenced block's composed lines (invalidated by a
/// promotion, a gutter-width rewrite, or a rebuild). Free function so the
/// caller can hold the buffer borrow that triggered the invalidation.
fn drop_code_flat(
    flat: &mut Vec<Line<'static>>,
    stable_len: &mut usize,
    code_flat: &mut Option<CodeFlat>,
) {
    if let Some(cf) = code_flat.take() {
        flat.truncate(cf.start);
        *stable_len = (*stable_len).min(cf.start);
    }
}

/// The separator blank line between blocks — the full renderer emits an
/// empty MarkdownLine, which the cell compose prefixes with two spaces.
fn sep_line(profile: Profile, palette: &ThemePalette) -> Line<'static> {
    match profile {
        Profile::Thinking => Line::from(Span::styled("  ", Style::default().fg(palette.thinking))),
        Profile::Content => Line::from(Span::raw("  ")),
    }
}

// ============================================================
// Reference full render (finalize + reconcile target)
// ============================================================

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
    let opts = RenderOpts {
        code_highlight: profile.code_highlight(),
        trim_trailing_blank: true,
    };
    let md = render_markdown_lines_with(text, Some(width.saturating_sub(2)), palette, opts);
    let thinking_style = Style::default().fg(palette.thinking);
    let bullet_style = Style::default().fg(palette.text);
    let limit = width as usize;

    let mut out = Vec::with_capacity(md.len() + 1);
    for (i, md_line) in md.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(md_line.segments.len() + 1);
        let prefix = match (profile, i == 0) {
            (Profile::Thinking, true) => Span::styled("⦁ ".to_string(), thinking_style),
            (Profile::Thinking, false) => Span::styled("  ".to_string(), thinking_style),
            (Profile::Content, true) => Span::styled("⦁ ".to_string(), bullet_style),
            (Profile::Content, false) => Span::raw("  "),
        };
        spans.push(prefix);
        for seg in md_line.segments {
            let style = match profile {
                Profile::Thinking => thinking_segment_style(seg.kind, seg.style, thinking_style),
                Profile::Content => seg.style,
            };
            spans.push(Span::styled(seg.text, style));
        }
        out.push(Line::from(spans));
    }
    let mut lines = hard_wrap_lines(out, limit);
    lines.push(Line::from(""));
    lines
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
) -> Vec<MarkdownLine> {
    let opts = RenderOpts {
        code_highlight: profile.code_highlight(),
        trim_trailing_blank: true,
    };
    render_markdown_lines_with(slice, Some(width.saturating_sub(2)), palette, opts)
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
) -> Vec<MarkdownLine> {
    let opts = RenderOpts {
        code_highlight: profile.code_highlight(),
        trim_trailing_blank: false,
    };
    render_markdown_lines_with(slice, Some(width.saturating_sub(2)), palette, opts)
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
    }
}

fn code_bottom_border(theme: &MarkdownTheme) -> MarkdownLine {
    MarkdownLine {
        segments: vec![MarkdownSegment::new(
            SegmentKind::Border,
            theme.border,
            "└────────",
        )],
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

/// Drop a trailing `\r` from a body line: pulldown normalizes CRLF line
/// endings out of code text, and the full renderer therefore never shows
/// one — a CRLF stream would otherwise diverge on every line (and on the
/// span comparison in the reconcile matrix).
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
    let (partial, total_lines) = fill_code_cache(cache, body_trimmed, theme);
    let number_width = code_number_width(cache, total_lines);
    if number_width != cache.gutter_width && cache.gutter_width > 0 {
        rewrite_gutters(cache, number_width);
        cache.gutter_width = number_width;
    }

    let mut lines = Vec::with_capacity(cache.rendered.len() + 2);
    lines.push(code_top_border(has_language, cache.lang.as_deref(), theme));
    lines.extend(cache.rendered.iter().cloned());
    // A closed block has no trailing partial line (its last body line is
    // newline-terminated); keep the branch for safety.
    if let Some(partial) = partial {
        lines.push(render_code_line_stateless(
            partial,
            cache,
            cache.rendered.len() + 1,
            number_width,
            theme,
        ));
    }
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

/// Fill the cache with the body lines that became COMPLETE since the last
/// call, returning the still-incomplete trailing line (if any) and the
/// body's total line count.
///
/// Cost is O(lines added): the byte cursor (`cache.scan_off`) and the
/// line counter only move forward, so a growing fence never re-scans or
/// re-clones its body — the module's O(new lines) contract.
fn fill_code_cache<'a>(
    cache: &mut CodeCache,
    body_trimmed: &'a str,
    theme: &MarkdownTheme,
) -> (Option<&'a str>, usize) {
    // `trimmed` drops the trailing newline run, so every line inside it is
    // complete — except its last one when the body is still mid-line
    // (that one is the in-flight partial line).
    let trimmed = body_trimmed.trim_end_matches('\n');
    let has_partial = !body_trimmed.is_empty() && !body_trimmed.ends_with('\n');
    let mut off = cache.scan_off;
    let mut partial = None;
    while off < trimmed.len() {
        match body_trimmed[off..].find('\n').map(|rel| off + rel) {
            // A terminator at or before the trimmed end closes a complete
            // line. (Past `trimmed.len()` there is only the newline run,
            // whose first '\n' sits exactly at it.)
            Some(end) => {
                debug_assert!(end <= trimmed.len());
                let line = strip_cr(&body_trimmed[off..end]);
                let number = cache.rendered.len() + 1;
                let rendered = render_code_line_stateful(cache, line, number, theme);
                cache.rendered.push(rendered);
                off = end + 1;
            }
            // No terminator left: the in-flight partial line. The cursor
            // stays on it, so it is rendered exactly once when it ends.
            None => {
                let tail = &body_trimmed[off..];
                // pulldown emits no text for an unterminated trailing line
                // of 1–3 spaces (EOF handling), so the reference render
                // has no such line — match it, or a stream paused on a
                // blank line would show one body line the reference lacks.
                // A ≥4-space run and anything containing other bytes
                // (tabs included) are kept.
                let spaces_only = tail.bytes().all(|b| b == b' ');
                if has_partial && !(spaces_only && tail.len() < 4) {
                    partial = Some(strip_cr(tail));
                }
                break;
            }
        }
    }
    cache.scan_off = off;
    let total = cache.rendered.len() + usize::from(partial.is_some());
    (partial, total)
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

/// Render one PARTIAL (incomplete) body line without touching the
/// highlighter state.
fn render_code_line_stateless(
    line: &str,
    cache: &CodeCache,
    number: usize,
    number_width: usize,
    theme: &MarkdownTheme,
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
    // Stateless approximation: fresh highlight context for the in-flight
    // line. Transient only (see doc comment above).
    if cache.highlighter.is_some()
        && let Some(mut hl) = cache.lang.as_deref().and_then(new_highlighter)
        && let Some(ops) = highlight_line_with(&mut hl, line)
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

fn indent_of(line: &str) -> usize {
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

/// If the line opens a fenced code block, return
/// `(fence_char, fence_len, info)`.
fn fence_open(line: &str) -> Option<(u8, usize, &str)> {
    let indent = indent_of(line);
    if indent >= 4 {
        return None;
    }
    let rest = &line[indent..];
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
    let indent = indent_of(line);
    if indent >= 4 {
        return None;
    }
    let rest = &line[indent..];
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

fn is_fence_close(line: &str, fence_char: u8, fence_len: usize) -> bool {
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
    let indent = indent_of(opener);
    let rest = &opener[indent..];
    let b = rest.as_bytes();
    if b.is_empty() {
        return ("", "");
    }
    let fc = b[0];
    let run = rest.bytes().take_while(|&c| c == fc).count();
    (&rest[run..], "")
}

/// Length of the list marker prefix (indent + marker + following space),
/// or None when the line doesn't start a list item.
fn list_marker_len(line: &str) -> Option<usize> {
    let indent = indent_of(line);
    if indent >= 4 {
        return None;
    }
    let rest = &line[indent..];
    let b = rest.as_bytes();
    if b.is_empty() {
        return None;
    }
    if matches!(b[0], b'-' | b'*' | b'+') {
        if b.len() == 1 {
            return Some(indent + 1);
        }
        if b[1] == b' ' {
            return Some(indent + 2);
        }
        return None;
    }
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits <= 9 && b.len() > digits && matches!(b[digits], b'.' | b')') {
        if b.len() == digits + 1 {
            return Some(indent + digits + 1);
        }
        if b[digits + 1] == b' ' {
            return Some(indent + digits + 2);
        }
    }
    None
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
fn hard_wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return lines;
    }
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line_width(&line) <= width {
            out.push(line);
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
        }
    }
    out
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
    use super::*;

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
        let cases: &[(&str, usize, Option<&str>)] = &[
            ("let x = 1;\n", 1, None),
            ("let x = 1;\n  ", 1, None),
            ("let x = 1;\n   ", 1, None),
            ("let x = 1;\n    ", 1, Some("    ")),
            ("let x = 1;\n\t", 1, Some("\t")),
            ("let x = 1;\n  `", 1, Some("  `")),
            ("let x = 1;\nlet y = 2;", 1, Some("let y = 2;")),
            ("let x = 1;\r\nlet y = 2;\r", 1, Some("let y = 2;")),
            ("let x = 1;\n\n", 1, None),
            ("let x = 1;\n\nlet y", 2, Some("let y")),
        ];
        for &(body, complete, partial) in cases {
            let mut cache = CodeCache::new(None, None, 0);
            let (got, total) = fill_code_cache(&mut cache, body, &theme);
            assert_eq!(got, partial, "partial for {body:?}");
            assert_eq!(cache.rendered.len(), complete, "lines for {body:?}");
            assert_eq!(total, complete + usize::from(partial.is_some()));
        }
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
