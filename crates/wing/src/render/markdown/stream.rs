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
//!   This keeps a giant growing code block at O(new lines) per frame.
//!
//! Correctness contract: for the same final text and width, the
//! incremental result is span-identical to [`full_lines`] — the reference
//! full render with the same profile options. `finalize()` replaces the
//! incremental state with exactly that reference, so any transient drift
//! converges at turn end.
//!
//! Block-boundary rules (see the design doc): fences open/close code
//! blocks and interrupt paragraphs (code needs its own mode for the line
//! cache); lists swallow blank lines (loose lists) and close on
//! non-list content after a blank; everything else (paragraphs, headings,
//! tables, blockquotes, rules) lives in one "paragraph" slice whose
//! INTERNAL structure is decided by the markdown parser itself — the
//! splitter only decides when a slice is final. That keeps the splitter
//! conservative: a misjudged boundary can only delay promotion (perf), or
//! produce a transient visual difference corrected by `finalize`.
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
    /// True for ```diff blocks — rendered via the generic path (no cache).
    is_diff: bool,
    /// Number of COMPLETE body lines rendered into `rendered`.
    rendered_lines: usize,
    /// Rendered body lines (gutter/border/content, markdown-level).
    rendered: Vec<MarkdownLine>,
    /// Stateful highlighter positioned after `rendered_lines` lines
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
        is_diff: bool,
        highlighter: Option<HighlightLines<'static>>,
        gutter_width: usize,
    ) -> Self {
        Self {
            lang,
            is_diff,
            rendered_lines: 0,
            rendered: Vec::new(),
            highlighter,
            gutter_width,
        }
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
    split: Splitter,
    /// Cell-final lines: promoted closed blocks + separators, then the
    /// tail and one trailing cell blank (managed by `sync`).
    flat: Vec<Line<'static>>,
    /// Length of the promoted (immutable) prefix of `flat`.
    stable_len: usize,
    /// Separator pending after the last promoted block — emitted when the
    /// next block starts (never dangles at stream end).
    pending_sep: bool,
    /// Rendering width; None until the first `lines()` call.
    width: Option<u16>,
    /// Live code cache for the tail when it is an unclosed fenced block.
    code_tail: Option<CodeCache>,
    finalized: bool,
    dirty: bool,
}

impl StreamingRender {
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            buf: String::new(),
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

        // 1) Promote newly-closed blocks (in order).
        self.flat.truncate(self.stable_len);
        let closed = std::mem::take(&mut self.split.closed);
        for block in closed {
            let emitted_sep = if self.pending_sep {
                self.flat.push(self.sep_line(palette));
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
                    self.profile,
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
            self.compose_extend(md_lines, width, palette);
            self.pending_sep = sep;
        }
        self.stable_len = self.flat.len();

        // 2) Render the active tail (doc-end semantics: trailing blanks
        // trimmed, matching the full render at the same text).
        let tail_start = self.split.tail_start;
        if tail_start < self.buf.len() || self.code_tail.is_some() {
            let profile = self.profile;
            let theme = MarkdownTheme::from_palette(palette);
            let mode = self.split.mode.clone();
            let mut cache = self.code_tail.take();
            let md_lines = match mode {
                // Cached (non-diff) fenced code: line-level incremental.
                Mode::FencedCode { .. } if cache.is_some() => code_block_markdown_lines(
                    cache.as_mut().expect("checked is_some"),
                    &self.buf[tail_start..],
                    profile,
                    &theme,
                ),
                // Diff fences (and any cache-less fence): the generic
                // path — exact reference semantics (file summaries,
                // metadata stripping via group_diff_by_file) at
                // O(block)/frame; diff blocks are bounded in practice.
                _ => render_generic(&self.buf[tail_start..], width, profile, palette),
            };
            // Emit the pending separator only when the tail actually
            // renders content (a blank-run tail keeps it pending — the
            // next block will trigger it). The separator is stable from
            // the moment it is emitted — fold it into the stable prefix
            // so the next sync's truncate keeps it.
            if !md_lines.is_empty() && self.pending_sep {
                self.flat.push(self.sep_line(palette));
                self.pending_sep = false;
                self.stable_len += 1;
            }
            self.code_tail = cache;
            self.compose_extend(md_lines, width, palette);
        }

        // 3) Cell trailing blank (matches the non-streaming cell renders).
        self.flat.push(Line::from(""));
    }

    /// Apply the fence-on-own-line normalization to unscanned bytes.
    ///
    /// Insertions can only occur inside the current (incomplete) line —
    /// always at offsets ≥ `scan` — so splitter offsets stay valid.
    fn normalize_fences(&mut self) {
        let scan = self.split.scan;
        let bytes = self.buf.as_bytes();
        let mut insertions: Vec<usize> = Vec::new();
        let mut i = scan;
        while let Some(rel) = find_sub(&bytes[i..], b"```") {
            let at = i + rel;
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
        let is_diff = lang
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("diff"));
        // Thinking profile never highlights; diff never caches (its
        // renderer is whole-block).
        let highlighter = if self.profile.code_highlight() && lang.is_some() && !is_diff {
            new_highlighter(lang.as_deref().unwrap_or(""))
        } else {
            None
        };
        let show_gutter = self.profile.code_highlight() && lang.is_some() && !is_diff;
        let cache = CodeCache::new(lang, is_diff, highlighter, if show_gutter { 3 } else { 0 });
        self.code_tail = if is_diff { None } else { Some(cache) };
    }

    /// Compose markdown-level lines into cell-final lines (prefix +
    /// thinking recolor + hard wrap) and extend `flat`.
    ///
    /// Replicates the full renderer's GLOBAL blank-line dedup at the batch
    /// boundary: a standalone slice render can carry leading blank lines
    /// (e.g. a table's `flush_paragraph`) that the doc-context render
    /// would have deduplicated against the preceding block's blank — skip
    /// them when `flat` already ends with a blank line.
    fn compose_extend(&mut self, md_lines: Vec<MarkdownLine>, width: u16, palette: &ThemePalette) {
        let thinking_style = Style::default().fg(palette.thinking);
        let bullet_style = Style::default().fg(palette.text);
        let limit = width as usize;
        // The `⦁ ` prefix belongs to the FIRST line ever composed into an
        // empty flat buffer — the tail is re-composed every frame, so
        // "first" must be positional (flat empty), not a sticky flag.
        let cell_first_pending = self.flat.is_empty();
        let flat_ends_blank = self
            .flat
            .last()
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()));
        let mut out = Vec::with_capacity(md_lines.len());
        for (i, md_line) in md_lines.into_iter().enumerate() {
            if flat_ends_blank && i == 0 && md_line.segments.is_empty() && !self.flat.is_empty() {
                // Dedup: the previous content already ended this blank run.
                continue;
            }
            // First content line of the whole cell: while nothing has been
            // composed yet (flat empty, nothing in this batch so far).
            let first = cell_first_pending && out.is_empty();
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(md_line.segments.len() + 1);
            let prefix = match (self.profile, first) {
                (Profile::Thinking, true) => Span::styled("⦁ ".to_string(), thinking_style),
                (Profile::Thinking, false) => Span::styled("  ".to_string(), thinking_style),
                (Profile::Content, true) => Span::styled("⦁ ".to_string(), bullet_style),
                (Profile::Content, false) => Span::raw("  "),
            };
            spans.push(prefix);
            for seg in md_line.segments {
                let style = match self.profile {
                    Profile::Thinking => {
                        thinking_segment_style(seg.kind, seg.style, thinking_style)
                    }
                    Profile::Content => seg.style,
                };
                spans.push(Span::styled(seg.text, style));
            }
            out.push(Line::from(spans));
        }
        let wrapped = hard_wrap_lines(out, limit);
        self.flat.extend(wrapped);
    }

    /// The separator blank line between blocks — the full renderer emits
    /// an empty MarkdownLine, which the cell compose prefixes with two
    /// spaces.
    fn sep_line(&self, palette: &ThemePalette) -> Line<'static> {
        match self.profile {
            Profile::Thinking => {
                Line::from(Span::styled("  ", Style::default().fg(palette.thinking)))
            }
            Profile::Content => Line::from(Span::raw("  ")),
        }
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

/// Render a fenced code block slice (opener + body [+ closer]) through
/// the line cache, replicating `render_code_block` output exactly:
/// top border with language label, gutter (Content + language only),
/// per-line highlighting (Content) or plain color (Thinking), bottom
/// border — including for unclosed blocks (matches the full path's
/// `finalize_unclosed_code_block`).
///
/// `cache` is None when the block closed before any sync rendered it —
/// a fresh cache is created and filled (equivalent output, no reuse).
fn code_block_markdown_lines(
    cache: &mut CodeCache,
    slice: &str,
    _profile: Profile,
    theme: &MarkdownTheme,
) -> Vec<MarkdownLine> {
    let mut lines = Vec::new();
    let border_style = theme.border;

    // Opener line: fence run + info.
    let opener_end = slice.find('\n').unwrap_or(slice.len());
    let opener = &slice[..opener_end];
    let (fence_info, _rest) = split_fence_line(opener);
    let lang_owned = fence_info
        .split_whitespace()
        .next()
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    let has_language = lang_owned.as_deref().is_some_and(|l| !l.trim().is_empty());
    cache.lang = lang_owned;

    // Top border.
    let label = if has_language {
        format!("┌─ {} ─", cache.lang.as_deref().unwrap_or_default())
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

    // Body: between the opener line and (for closed blocks) the closing
    // fence line. Complete lines fill the cache; a trailing line without
    // a newline (unclosed stream) renders statelessly each call.
    let body = &slice[(opener_end + 1).min(slice.len())..];
    // The opener's fence signature — only a MATCHING fence closes it.
    let (fence_char, fence_len) = match fence_open(opener) {
        Some((fc, fl, _)) => (fc, fl),
        None => (b'`', 3),
    };
    // A closed slice ends with the closing fence line (plus its newline)
    // — exclude it from the body.
    let body_trimmed = {
        let without_final_nl = body.strip_suffix('\n').unwrap_or(body);
        let last_line_start = without_final_nl.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let last_line = &without_final_nl[last_line_start..];
        if is_fence_close(last_line, fence_char, fence_len) {
            &body[..last_line_start]
        } else {
            body
        }
    };
    let code = body_trimmed.trim_end_matches('\n');

    // The full renderer's line count (includes the trailing partial line
    // when the block is unclosed).
    let total_lines = code.lines().count();
    // Lines that are complete (terminated by \n) — the cache only ever
    // holds these, so a growing partial line never gets frozen in it.
    let has_partial = !body_trimmed.is_empty() && !body_trimmed.ends_with('\n');
    let complete_count = total_lines - usize::from(has_partial);

    // Ensure the cache covers `complete_count` complete lines.
    fill_code_cache(cache, code, complete_count, theme);

    let number_width = if cache.gutter_width > 0 {
        total_lines.max(1).to_string().len().max(3)
    } else {
        0
    };
    // Gutter digit growth: rewrite cached gutters when the width changed.
    if number_width != cache.gutter_width && cache.gutter_width > 0 {
        rewrite_gutters(cache, number_width);
        cache.gutter_width = number_width;
    }

    lines.extend(cache.rendered.iter().cloned());

    // Trailing partial line (unclosed stream, no newline yet).
    if has_partial && let Some(last_line) = code.lines().nth(complete_count) {
        // Rendered statelessly: the stateful highlighter cannot be cloned
        // (syntect), and advancing it on a partial line would corrupt the
        // sequence. Transient visual only — the completed line is
        // re-rendered with correct state.
        lines.push(render_code_line_stateless(
            last_line,
            cache,
            complete_count + 1,
            number_width,
            theme,
        ));
    }

    // Bottom border (present even while unclosed — matches
    // finalize_unclosed_code_block).
    lines.push(MarkdownLine {
        segments: vec![MarkdownSegment::new(
            SegmentKind::Border,
            border_style,
            "└────────",
        )],
    });

    lines
}

/// Render complete body lines into the cache up to `complete_count`.
fn fill_code_cache(
    cache: &mut CodeCache,
    code: &str,
    complete_count: usize,
    theme: &MarkdownTheme,
) {
    let mut idx = cache.rendered_lines;
    while idx < complete_count {
        let line = code.lines().nth(idx).expect("idx < complete_count");
        let number = idx + 1;
        let rendered = render_code_line_stateful(cache, line, number, theme);
        cache.rendered.push(rendered);
        idx += 1;
    }
    cache.rendered_lines = complete_count;
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
    // Plain (Thinking profile, diff, or unknown language).
    let style = if cache.is_diff {
        diff_line_style(line, theme)
    } else {
        theme.code_block
    };
    md.push_segment(SegmentKind::CodeBlock, style, line);
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
    let style = if cache.is_diff {
        diff_line_style(line, theme)
    } else {
        theme.code_block
    };
    md.push_segment(SegmentKind::CodeBlock, style, line);
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

/// Diff line coloring (subset of `render_code_block`'s diff path: body
/// lines; file summaries are a whole-block concern and diff blocks never
/// use the cache).
fn diff_line_style(line: &str, theme: &MarkdownTheme) -> Style {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        theme.code_block
    } else if trimmed.starts_with('+') {
        theme.diff_add
    } else if trimmed.starts_with('-') {
        theme.diff_del
    } else if trimmed.starts_with("@@") {
        theme.diff_hunk
    } else {
        theme.dimmed
    }
}

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
    match fence_close_len(line) {
        Some(run) => line.starts_with(fence_char as char) && run >= fence_len,
        None => false,
    }
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
    fn streaming_paragraphs_match_reference() {
        let palette = ThemePalette::default();
        let text = "First para.\n\nSecond para with **bold** and `code`.\n\nThird.";
        for profile in [Profile::Thinking, Profile::Content] {
            let mut sr = StreamingRender::new(profile);
            // Chunked at odd offsets.
            for chunk in text.as_bytes().chunks(7) {
                let s = std::str::from_utf8(chunk).unwrap_or_default();
                let mut owned = s.to_string();
                // cut at char boundary
                while !owned.is_empty() && !text.contains(&owned) {
                    owned.pop();
                }
                sr.push(&owned);
                let _ = sr.lines(80, &palette);
            }
            sr.finalize(80, &palette);
            let reference = full_lines(text, 80, profile, &palette);
            assert_eq!(
                span_texts(sr.lines(80, &palette)),
                span_texts(&reference),
                "profile {profile:?}"
            );
        }
    }
}
