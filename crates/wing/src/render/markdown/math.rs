//! Math (LaTeX) rendering: delimiter normalization + the two pulldown math events.
//!
//! `pulldown-cmark` hands us `Event::InlineMath` / `Event::DisplayMath` for `$…$` and `$$…$$`, but
//! it knows nothing about the delimiters the models actually emit most of the time:
//!
//! | source | pulldown sees | we do |
//! |---|---|---|
//! | `$x^2$` | `InlineMath("x^2")` | [`inline`] |
//! | `$$\na=b\n$$` | `DisplayMath("\na=b\n")` | [`display`] |
//! | `\(x\)` | `Text("(x")` + `Text(")")` | rewrite to `$x$` |
//! | `\[x\]` | `Text("[x")` + `Text("]")` | rewrite to `$$x$$` |
//! | `\begin{align}…\end{align}` | plain text + soft breaks | rewrite to `$$…$$` |
//!
//! The rewrites are a **source-text pre-pass** ([`normalize_delimiters`]) run before parsing, so
//! every render entry point that shares `render_markdown_to_lines` — the full reference render, a
//! promoted streaming block, a re-parsed indented prose block — sees the same text. The rule
//! itself lives in `super::Profile`.
//!
//! **Degradation: the source, never half a formula.** [`wing_math::render_inline`] /
//! [`wing_math::render_display`] return `None` for anything they will not render (too wide,
//! suspected truncation, unknown commands, a multiline block that cannot fit on one line — the
//! full list is in `wing_math`'s rustdoc). `None` is not an error: the segment then carries the
//! **complete source between its delimiters**, byte-identical to what the TUI showed before math
//! rendering existed. A formula is never truncated, never replaced by an empty string and never
//! silently dropped.
//!
//! Every math segment — rendered grid or literal fallback — is tagged [`SegmentKind::Math`] and
//! styled with [`MarkdownTheme::math`], so it stays a distinct element in both profiles (the
//! thinking recolor leaves it alone, like code).

use std::borrow::Cow;

use ratatui::style::Style;

use super::parsing::MarkdownContext;
use super::stream::{FenceStep, FenceTrack, content_start, fence_opener, indent_of, prefix};
use super::types::{MarkdownLine, SegmentKind};

/// Width assumed for display math when the caller has no content width
/// (IR-only callers: unit tests, `render_probe --kinds`). Real cells always
/// pass their content width, which is what the frame then wraps to.
pub(crate) const DEFAULT_MATH_WIDTH: usize = 120;

/// Environments worth wrapping in `$$…$$`.
///
/// Only what the engine can actually render (it strips a trailing `*`); an
/// environment outside this list is left exactly as the model wrote it,
/// which is what its unrenderable content would degrade to anyway.
const MATH_ENVS: &[&str] = &[
    // Multiline environments adapted by the engine.
    "align",
    "aligned",
    "alignat",
    "alignedat",
    "flalign",
    "split",
    "eqnarray",
    "gather",
    "multline",
    "center",
    "equation",
    "displaymath",
    "array",
    // Environments the vendored parser renders.
    "cases",
    "matrix",
    "pmatrix",
    "bmatrix",
    "Bmatrix",
    "vmatrix",
    "Vmatrix",
];

// ============================================================
// Event handlers
// ============================================================

/// `Event::InlineMath`: one segment, rendered when the engine can fit it on a
/// single row *and* that row fits the line (see the module docs for the
/// fallback).
pub(crate) fn inline(src: &str, ctx: &mut MarkdownContext<'_>) {
    // Inside a table cell the cell renderer owns the line; a block would
    // desynchronize the row accumulation.
    let text = match wing_math::render_inline(src) {
        Some(rendered) if fits_line(&rendered, ctx) => rendered,
        // Either the engine would not render it, or what it produced is wider
        // than the line can hold: show the source. Half a formula (what the
        // compose's hard wrap would make of an over-wide segment) is never an
        // option; the source flows through the ordinary prose wrap like any
        // other text.
        _ => format!("${src}$"),
    };
    if ctx.active_table.is_some() {
        let text = text.replace('\n', " ");
        ctx.ensure_prefix();
        ctx.current_line
            .push_segment(SegmentKind::Math, math_style(ctx), &text);
        return;
    }
    match text.split_once('\n') {
        // A formula that carries a newline (an unclosed `$…` span, or a
        // source fallback that wrapped) becomes a block of lines.
        None => {
            ctx.ensure_prefix();
            ctx.current_line
                .push_segment(SegmentKind::Math, math_style(ctx), &text);
        }
        Some(_) => push_math_block(ctx, text.split('\n')),
    }
}

/// `Event::DisplayMath`: a character-grid block of `N ≥ 1` lines, or the
/// literal `$$…$$` source when the engine declines (see the module docs).
pub(crate) fn display(src: &str, ctx: &mut MarkdownContext<'_>) {
    // A table cell cannot hold a block: try the single-row form, else keep
    // the literal source on the cell's own line.
    if ctx.active_table.is_some() {
        let text = match wing_math::render_inline(src) {
            Some(rendered) => rendered,
            None => format!("$${src}$$"),
        };
        let text = text.replace('\n', " ");
        ctx.ensure_prefix();
        ctx.current_line
            .push_segment(SegmentKind::Math, math_style(ctx), &text);
        return;
    }

    let max_width = ctx
        .available_width
        .map(usize::from)
        .unwrap_or(DEFAULT_MATH_WIDTH);
    let rendered: Vec<String> = match wing_math::render_display(src, max_width) {
        Some(math) => math.into_lines(),
        None => vec![format!("$${src}$$")],
    };
    push_math_block(ctx, rendered.iter().flat_map(|line| line.split('\n')));
}

/// Push `lines` as math block lines: the formula starts on a fresh line, and
/// every line carries the enclosing block's prefix (blockquote bars, list
/// continuation) — a display block inside `>` keeps its bar.
fn push_math_block<'a>(ctx: &mut MarkdownContext<'_>, lines: impl Iterator<Item = &'a str>) {
    ctx.flush_line();
    for line in lines {
        push_math_line(ctx, line);
    }
}

/// Append one math line: fresh line + block prefix + the text.
///
/// A grid row that renders to nothing keeps its (blank) row — except at the end of the document,
/// where the document-level trailing-blank trim (`trim_trailing_blank`) merges it with the block
/// separator as it does for any other trailing blank. For a formula whose last grid row is empty,
/// the engine's `height()` and the rendered row count can therefore differ by one; nothing visible
/// is lost.
fn push_math_line(ctx: &mut MarkdownContext<'_>, text: &str) {
    *ctx.current_line = MarkdownLine::default();
    ctx.ensure_prefix();
    if !text.is_empty() {
        ctx.current_line
            .push_segment(SegmentKind::Math, math_style(ctx), text);
    }
    if ctx.current_line.segments.is_empty() {
        // A grid row that renders to nothing (an empty row of a sparse
        // layout) still occupies a row — keep it instead of dropping it.
        ctx.lines.push(MarkdownLine::default());
    } else {
        ctx.flush_line();
    }
}

fn math_style(ctx: &MarkdownContext<'_>) -> Style {
    ctx.base_style.patch(ctx.theme.math)
}

/// Whether an inline formula fits the line it would be written into.
///
/// `available_width` is the same content width the prose wrap and the display
/// grid are fitted to; with no width known (the IR-only callers) anything
/// fits. Measured with `unicode-width`, so a CJK-wide glyph counts as the two
/// columns it will occupy.
fn fits_line(rendered: &str, ctx: &MarkdownContext<'_>) -> bool {
    let Some(max) = ctx.available_width else {
        return true;
    };
    unicode_width::UnicodeWidthStr::width(rendered) <= usize::from(max)
}

// ============================================================
// Delimiter normalization (source pre-pass)
// ============================================================

/// Rewrite the math delimiters pulldown does not understand into the `$` / `$$` form it does:
/// `\(…\)` → `$…$`, `\[…\]` → `$$…$$`, and a bare environment (`\begin{align}…\end{align}`) →
/// `$$…$$`.
///
/// Never touches a region pulldown does not parse as markdown text: fenced code blocks (``` / ~~~),
/// **including a fence behind a block prefix** (`> ~~~`, `- ``` ` — the parser resolves the prefix
/// before it decides what the line is, so a prefix must not hide the region); inline code spans;
/// existing `$…$` / `$$…$$` math (so a wrapped environment stays wrapped); indented (4-space) code
/// blocks, prefix-aware in the same way; HTML blocks (`<div>` … to the next blank line); an inline
/// HTML tag or autolink, a link / image destination (`](url "title")` — it is what a click and the
/// OSC8 hyperlink open) and a link reference definition line; anything whose closing delimiter is
/// missing, or that would cross a blank line or a fence (pulldown's own math pairing does not cross
/// a blank line either, so leaving the source alone keeps the output identical to the `$`-less
/// literal it is today); and a span the scan reaches *after* an unpaired `$$` in the same block
/// (inserting `$$` there would re-pair the stray delimiter and grow the text on every pass).
///
/// The rewrite is purely local (a span is rewritten iff its own text is well-formed), which is what
/// lets the streaming engine normalize a *slice* and still agree with the reference render of the
/// whole document: slice boundaries are exactly the blank lines / fences a span may not cross.
///
/// Two refusals keep the output a pure function of the input rather than of how often the pass ran
/// — inserting a delimiter must never re-pair a `$` that is already there: the delimiters a rewrite
/// would insert must not touch an existing `$` (`fuses_with_dollar`, or the fused `$$` would change
/// what the next pass sees), and the positional unpaired-`$$` rule above.
///
/// Two budgets bound the pass: a span longer than [`MAX_SPAN`] bytes is not rewritten (the engine's
/// own source budget is smaller), and the pass stops after [`MAX_SCAN_WORK`] bytes of
/// closer-searching. Past either, the remaining text is left exactly as it is.
///
/// A span's leading/trailing whitespace is trimmed, because `$` only pairs against non-whitespace:
/// `\(a \)` written out verbatim as `$a $` would not be math at all.
///
/// Returns [`Cow::Borrowed`] when there is nothing to rewrite.
pub(crate) fn normalize_delimiters(text: &str) -> Cow<'_, str> {
    // Fast path: most text has no LaTeX-delimited math at all.
    if !text.contains("\\(") && !text.contains("\\[") && !text.contains("\\begin{") {
        return Cow::Borrowed(text);
    }
    let mut scan = Scan::new(text);
    scan.run();
    scan.finish()
}

/// Work budget for one pass, in bytes walked while looking for a closing
/// delimiter. A well-formed formula closes within a few dozen bytes; the cap
/// only stops a pathological input (thousands of unterminated `\(`) from
/// turning the pass into a quadratic scan. Past it the remaining text is left
/// untouched — degrading to the literal source is always allowed here.
const MAX_SCAN_WORK: usize = 1 << 20;

/// Longest span searched for a closing delimiter (the engine's own source
/// budget is 8192 chars, so a longer "formula" could not render anyway).
const MAX_SPAN: usize = 8192;

/// One pending rewrite: byte range `[start, end)` replaced by `replacement`.
struct Edit {
    start: usize,
    end: usize,
    replacement: String,
}

struct Scan<'a> {
    text: &'a str,
    bytes: &'a [u8],
    edits: Vec<Edit>,
    /// Bytes walked by closer searches (see [`MAX_SCAN_WORK`]).
    work: usize,
    exhausted: bool,
    /// Open fenced code block, as the SHARED [`FenceTrack`] — never a local
    /// re-implementation of the fence rules.
    fence: Option<FenceTrack>,
    /// Inside an indented (4-space) code block.
    indented: bool,
    /// Whether the line being scanned starts a block (text start, or right
    /// after a blank line / a fenced block). Only a block STARTING with an
    /// indented line is an indented code block — an indented line below
    /// paragraph text is a lazy continuation and stays prose.
    at_block_start: bool,
    /// Lines still belonging to the reference definition that just started
    /// (`[label]:` with its destination and/or title on the following lines).
    /// 0 = none.
    ref_def_lines: u8,
    /// Whether the definition's destination is still missing (it may be on the
    /// next line, indented or not).
    ref_def_dest_missing: bool,
    /// The HTML block currently running, if any — while it runs markdown is
    /// not parsed, so nothing inside may be rewritten.
    ///
    /// The [`HtmlContainer`] is the container the block lives in: an HTML block
    /// cannot outlive its container (`> <div>` ends when the `>` chain stops),
    /// but a *top-level* block keeps its content verbatim — `- a` or `> q`
    /// inside it are block content, not containers.
    in_html_block: Option<(HtmlBlock, HtmlContainer)>,
    /// The scan has met an unpaired `$$` in the current block.
    ///
    /// A rewrite inserts `$$`, and a stray `$$` would then pair with the inserted one on the next
    /// pass — the text would keep growing. So from that point on the rest of the block is left
    /// alone (a formula there degrades to its source, which is always allowed); a span *before* the
    /// stray delimiter is unaffected. The rule is positional on purpose: decided by what the scan
    /// has already seen, never by a look-ahead. Single `$` deliberately does not set this: `$100` in
    /// prose is common and cannot pair with an inserted `$$`.
    stray_display_delim: bool,
}

impl<'a> Scan<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            edits: Vec::new(),
            work: 0,
            exhausted: false,
            fence: None,
            indented: false,
            at_block_start: true,
            in_html_block: None,
            ref_def_lines: 0,
            ref_def_dest_missing: false,
            stray_display_delim: false,
        }
    }

    /// Apply the collected edits (or hand the text back untouched).
    fn finish(self) -> Cow<'a, str> {
        if self.edits.is_empty() {
            return Cow::Borrowed(self.text);
        }
        let mut out = String::with_capacity(self.text.len() + 4 * self.edits.len());
        let mut at = 0usize;
        for edit in &self.edits {
            out.push_str(&self.text[at..edit.start]);
            out.push_str(&edit.replacement);
            at = edit.end;
        }
        out.push_str(&self.text[at..]);
        Cow::Owned(out)
    }

    /// Whether `line` is absorbed by the running reference definition.
    fn ref_def_continuation(&mut self, line: &str) -> bool {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if fence_opener(line).is_some() {
            // A fence line interrupts the definition (it is a block start).
            return false;
        }
        if self.ref_def_dest_missing {
            // The destination: any non-blank line the block parser still reads
            // as part of the definition.
            if line.trim().is_empty() {
                return false;
            }
            self.ref_def_lines -= 1;
            self.ref_def_dest_missing = false;
            if !indented {
                self.ref_def_lines = 0;
            } else {
                // A title may still follow, but only on an indented line.
                self.ref_def_lines = self.ref_def_lines.min(1);
            }
            return true;
        }
        // Only a real title may continue the definition.
        if !indented || !looks_like_title(line.trim()) {
            return false;
        }
        self.ref_def_lines = 0;
        true
    }

    fn run(&mut self) {
        let len = self.bytes.len();
        let mut i = 0usize;
        let mut line_start = 0usize;
        while i < len {
            if self.exhausted {
                break;
            }
            let (line_end, next) = self.line_bounds(i);
            if i == line_start {
                // Line-level state: fences, indented blocks and HTML blocks
                // are opaque, blank lines reset the per-block scan.
                //
                // Every shape decision is made on the line's CONTENT — the
                // prefixes pulldown resolves first (`> ` chains, list markers)
                // do not change what the line is: `> ~~~` is a fence and
                // `>     x` is an indented block.
                let line = &self.text[i..line_end];
                let content = &line[content_start(line)..];
                if let Some(track) = self.fence {
                    // Which lines are code is decided by the SHARED fence
                    // state machine (`FenceTrack::step`) — the same one the
                    // streaming splitter drives, so the slice boundaries and
                    // this scanner cannot disagree about a fence again.
                    let (track_next, step) = track.step(line);
                    match step {
                        FenceStep::Closes => {
                            self.fence = None;
                            self.at_block_start = true;
                            self.stray_display_delim = false;
                        }
                        FenceStep::OpensTopLevel => {
                            // The container ended and this line opens a new
                            // top-level fence: everything from here on is code
                            // until THAT fence closes.
                            self.fence = fence_opener(line);
                        }
                        FenceStep::Body => self.fence = Some(track_next),
                    }
                    i = next;
                    line_start = next;
                    continue;
                }
                if let Some((block, container)) = self.in_html_block {
                    // Inside an HTML block markdown is not parsed. Types 1–5
                    // end on the line containing their marker, 6–7 at the next
                    // blank line — and the container ends the block too.
                    if html_block_ends(block, content) {
                        // The end line still belongs to the block.
                        self.in_html_block = None;
                        i = next;
                        line_start = next;
                        continue;
                    }
                    if !container.holds(&prefix(line)) {
                        // The container ended: the block is over and THIS line
                        // is markdown again (`> <div>` + `~~~` opens a fence).
                        // A line that merely *carries a prefix* (`- a` inside a
                        // top-level block) does not end anything — HTML block
                        // content is verbatim.
                        self.in_html_block = None;
                    } else {
                        i = next;
                        line_start = next;
                        continue;
                    }
                }
                if content.trim().is_empty() {
                    // A blank line — including a prefix-only one (`> ` inside
                    // a quote is a blank line *inside* that container).
                    self.indented = false;
                    self.ref_def_lines = 0;
                    self.at_block_start = true;
                    self.stray_display_delim = false;
                    i = next;
                    line_start = next;
                    continue;
                }
                if self.ref_def_lines > 0 {
                    // The destination and/or the title of the definition above: markdown is not
                    // parsed here either (the destination is a URL — rewriting it would change what
                    // a click opens).
                    //
                    // Only lines the definition can actually absorb count (CommonMark §4.7): a fence
                    // line interrupts the definition, and a title is `"…"` / `'…'` / `(…)` —
                    // otherwise the definition ended and the line is parsed (`[ref]: http://x` +
                    // `  ~~~` used to swallow the fence opener, so the scanner rewrote the code the
                    // fence opened).
                    if self.ref_def_continuation(line) {
                        i = next;
                        line_start = next;
                        continue;
                    }
                    self.ref_def_lines = 0;
                }
                if let Some(track) = fence_opener(line) {
                    self.fence = Some(track);
                    i = next;
                    line_start = next;
                    continue;
                }
                if self.indented {
                    if indent_of(content) >= 4 {
                        i = next;
                        line_start = next;
                        continue;
                    }
                    self.indented = false;
                } else if self.at_block_start && indent_of(content) >= 4 {
                    // An indented code block (CommonMark): it can only start
                    // a block, so a lazily continued paragraph line below it
                    // stays prose.
                    self.indented = true;
                    i = next;
                    line_start = next;
                    continue;
                }
                if let Some(block) = html_block_start(content, self.at_block_start) {
                    // `<div>`, `<!-- … -->`, `<?… ?>` …: markdown is not
                    // parsed inside the block (CommonMark §4.6, `HtmlBlock`).
                    // Types 2–5 can end on the very line that opens them
                    // (`<!-- c -->`), in which case the block is that line.
                    if !html_block_ends(block, content) {
                        self.in_html_block = Some((block, HtmlContainer::of(&prefix(line))));
                    }
                    i = next;
                    line_start = next;
                    continue;
                }
                if let Some(dest_missing) = reference_definition(content) {
                    // `[label]: url "title"` — the destination is a URL, not
                    // prose: rewriting it would change what a click opens. The
                    // destination and the title may continue on the next line
                    // (indented, or — for a missing destination — not).
                    self.ref_def_dest_missing = dest_missing;
                    self.ref_def_lines = if dest_missing { 2 } else { 1 };
                    i = next;
                    line_start = next;
                    continue;
                }
                self.at_block_start = false;
            }
            match self.next_token(i, line_end) {
                None => {
                    i = next;
                    line_start = next;
                }
                Some(at) => match self.resolve(at) {
                    Some(end) => i = end,
                    None => i = at + 1,
                },
            }
        }
    }

    /// Byte range of the line containing `at`, as `(end_without_newline,
    /// next_line_start)`.
    fn line_bounds(&self, at: usize) -> (usize, usize) {
        match self.text[at..].find('\n') {
            Some(nl) => (at + nl, at + nl + 1),
            None => (self.bytes.len(), self.bytes.len()),
        }
    }

    fn line_start_of(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map_or(0, |nl| nl + 1)
    }

    /// Byte offset just past the character at `at` (char-boundary safe).
    fn bump(&self, at: usize) -> usize {
        at + self.text[at..].chars().next().map_or(1, char::len_utf8)
    }

    /// First math-delimiter token at or after `from`, before `line_end`.
    ///
    /// Tokens are the openers of every opaque-or-rewritable region: a
    /// backtick run, `$`, `\(`, `\[`, `\begin{`, an inline HTML tag or
    /// autolink (`<…>`), and a link/image destination (`](…`).
    fn next_token(&self, from: usize, line_end: usize) -> Option<usize> {
        let mut i = from;
        while i < line_end {
            match self.bytes[i] {
                b'`' | b'$' => return Some(i),
                // `<div>`, `</p>`, `<!--`, `<http://x/…>`: markdown is not
                // parsed inside, so a rewrite there would leak `$`s into the
                // text the user sees.
                b'<' if self.bytes.get(i + 1).copied().is_some_and(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'/' | b'!' | b'?')
                }) =>
                {
                    return Some(i);
                }
                // A link / image destination: `](url "title")`. The
                // destination string is what a click and the OSC8 hyperlink
                // use, so it must stay exactly as the author wrote it.
                b']' if self.bytes.get(i + 1) == Some(&b'(') => return Some(i),
                // A backslash preceded by another backslash is an escaped
                // backslash, not a delimiter (`\\\\(x` is a line break, not math).
                b'\\' if i == 0 || self.bytes[i - 1] != b'\\' => {
                    let opener = match self.bytes.get(i + 1).copied() {
                        Some(b'(' | b'[') => true,
                        Some(b'b') => self.text[i..].starts_with("\\begin{"),
                        _ => false,
                    };
                    if opener {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// Resolve the region starting at `at`.
    ///
    /// Returns the byte offset just past the region when it was consumed
    /// (rewritten or skipped as opaque), or `None` when the token is not a
    /// region opener after all — the caller then resumes one byte later.
    fn resolve(&mut self, at: usize) -> Option<usize> {
        match self.bytes[at] {
            b'`' => self.skip_code_span(at),
            b'$' => self.skip_dollar_math(at),
            b'<' => self.skip_inline_html(at),
            b']' => self.skip_link_destination(at),
            b'\\' => match self.bytes.get(at + 1).copied() {
                Some(b'(') => self.rewrite(at, "\\(", "\\)", "$", "$"),
                Some(b'[') => self.rewrite(at, "\\[", "\\]", "$$", "$$"),
                Some(b'b') if self.text[at..].starts_with("\\begin{") => self.rewrite_env(at),
                _ => None,
            },
            _ => None,
        }
    }

    /// `\(…\)` → `$…$`, `\[…\]` → `$$…$$`.
    fn rewrite(
        &mut self,
        at: usize,
        open: &str,
        close: &str,
        open_rep: &str,
        close_rep: &str,
    ) -> Option<usize> {
        if self.stray_display_delim {
            return None;
        }
        let inner_start = at + open.len();
        let close_at = self.find_closer(inner_start, close, true)?;
        // Edge whitespace is trimmed because pulldown only pairs `$` against
        // non-whitespace: `\(a \)` rewritten verbatim (`$a $`) would not be
        // math at all — the delimiters would leak into the text. Math-wise the
        // spaces are meaningless (the engine trims too).
        let inner = self.text[inner_start..close_at].trim();
        if inner.is_empty() {
            // `\(\)` has nothing to render: leave the source alone.
            return None;
        }
        let end = close_at + close.len();
        if self.fuses_with_dollar(at, end) {
            return None;
        }
        self.edits.push(Edit {
            start: at,
            end,
            replacement: format!("{open_rep}{inner}{close_rep}"),
        });
        Some(end)
    }

    /// `\begin{ENV}…\end{ENV}` → `$$…$$` for the environments the engine
    /// knows; anything else is left as the model wrote it.
    fn rewrite_env(&mut self, at: usize) -> Option<usize> {
        if self.stray_display_delim {
            return None;
        }
        let name_start = at + "\\begin{".len();
        let name_end = self.text[name_start..].find('}')? + name_start;
        let name = &self.text[name_start..name_end];
        let base = name.trim_end_matches('*');
        if !MATH_ENVS.contains(&base) {
            return None;
        }
        let body_start = name_end + 1;
        let close_at = self.find_env_end(body_start, name)?;
        let end = close_at + format!("\\end{{{name}}}").len();
        if self.fuses_with_dollar(at, end) {
            return None;
        }
        self.edits.push(Edit {
            start: at,
            end,
            replacement: format!("$${}$$", &self.text[at..end]),
        });
        Some(end)
    }

    /// Whether the delimiters a rewrite would insert at `[start, end)` fuse with a `$` that is
    /// already there into a `$$` pair.
    ///
    /// `$$` is a different region kind (it hides a bare environment from the next pass), so fusing
    /// would change what a second pass sees and the rewrite would stop being idempotent — the text
    /// would grow a `$$` per pass. Sources that put a `$` right next to a delimiter are left alone
    /// instead; showing the source is always allowed.
    fn fuses_with_dollar(&self, start: usize, end: usize) -> bool {
        (start > 0 && self.bytes[start - 1] == b'$') || self.bytes.get(end) == Some(&b'$')
    }

    /// Byte offset of the `\end{name}` matching the `\begin{name}` that ended
    /// just before `body_start` (same-name nesting counted).
    fn find_env_end(&mut self, body_start: usize, name: &str) -> Option<usize> {
        let end = self.span_limit(body_start)?;
        if !self.charge(end - body_start) {
            return None;
        }
        let begin = format!("\\begin{{{name}}}");
        let close = format!("\\end{{{name}}}");
        let mut depth = 0usize;
        let mut i = body_start;
        while i < end {
            if self.text[i..].starts_with(&close) {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
                i += close.len();
                continue;
            }
            if self.text[i..].starts_with(&begin) {
                depth += 1;
                i += begin.len();
                continue;
            }
            match self.bytes[i] {
                b'`' | b'$' => return None,
                b'\\' => i = self.bump(i),
                // Char-boundary safe: a CJK character inside the body (or a
                // `\text{中文}`) must not walk into the middle of a byte.
                _ => i = self.bump(i),
            }
        }
        None
    }

    /// Skip an inline HTML tag / autolink: everything up to its `>`.
    ///
    /// An unclosed `<` (a comparison, an arrow — anything with no `>` in the
    /// block) is not a region and the scan resumes right after it.
    fn skip_inline_html(&mut self, at: usize) -> Option<usize> {
        let end = self.span_limit(at + 1)?;
        let rel = self.text[at + 1..end].find('>')?;
        Some(at + 1 + rel + 1)
    }

    /// Skip a link / image destination: `](url "title")`, honouring nested
    /// parentheses (a URL may contain them).
    fn skip_link_destination(&mut self, at: usize) -> Option<usize> {
        let end = self.span_limit(at + 2)?;
        if !self.charge(end - at) {
            return None;
        }
        let mut depth = 0usize;
        let mut i = at + 2;
        while i < end {
            match self.bytes[i] {
                b'(' => depth += 1,
                b')' if depth == 0 => return Some(i + 1),
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// Skip a backtick code span, returning the offset just past its closing
    /// run (the run must have the same length — longer runs do not close).
    fn skip_code_span(&mut self, at: usize) -> Option<usize> {
        let run = self.bytes[at..].iter().take_while(|&&b| b == b'`').count();
        let limit = self.span_limit(at + run)?;
        let needle = "`".repeat(run);
        let mut from = at + run;
        while self.charge(limit - from)
            && let Some(rel) = self.text[from..limit].find(&needle)
        {
            let close = from + rel;
            let close_len = self.bytes[close..]
                .iter()
                .take_while(|&&b| b == b'`')
                .count();
            if close_len == run {
                return Some(close + run);
            }
            from = close + close_len;
        }
        None
    }

    /// Skip a `$…$` / `$$…$$` region, returning the offset just past it.
    fn skip_dollar_math(&mut self, at: usize) -> Option<usize> {
        if self.bytes.get(at + 1) == Some(&b'$') {
            let Some(close) = self.find_closer(at + 2, "$$", false) else {
                // Unpaired display delimiter: the rest of the block is
                // off-limits for rewrites (see the field docs).
                self.stray_display_delim = true;
                return None;
            };
            return Some(close + 2);
        }
        // A single `$` opens only when a non-whitespace byte follows (the
        // rule pulldown uses, which is what keeps `$100` out of math).
        if self
            .bytes
            .get(at + 1)
            .copied()
            .is_none_or(|b| b.is_ascii_whitespace())
        {
            return None;
        }
        let end = self.span_limit(at + 1)?;
        if !self.charge(end - at - 1) {
            return None;
        }
        let mut i = at + 1;
        while i < end {
            if self.bytes[i] == b'$' {
                // `$$` is a display delimiter, not the close of this span:
                // give up rather than pair across it.
                if self.bytes.get(i + 1) == Some(&b'$') || self.bytes[i - 1] == b'$' {
                    return None;
                }
                if !self.bytes[i - 1].is_ascii_whitespace() {
                    return Some(i + 1);
                }
            }
            i += 1;
        }
        None
    }

    /// Find `closer` in the current block (never past a blank line, a fence
    /// or [`MAX_SPAN`]), with an optional check that the span carries no
    /// code/math content of its own.
    fn find_closer(&mut self, from: usize, closer: &str, bail_on_nested: bool) -> Option<usize> {
        let end = self.span_limit(from)?;
        if !self.charge(end - from) {
            return None;
        }
        let hay = &self.text[from..end];
        let rel = hay.find(closer)?;
        if bail_on_nested {
            let inner = &hay[..rel];
            if inner.contains('`') || inner.contains('$') {
                return None;
            }
        }
        Some(from + rel)
    }

    /// End of the span a region may cover: the next blank line, the next
    /// fence opener, or [`MAX_SPAN`] — whichever comes first.
    ///
    /// Blank lines and fences are exactly where the streaming splitter cuts
    /// slices, which is why a purely local rule keeps the two paths in
    /// agreement (see [`normalize_delimiters`]).
    fn span_limit(&mut self, from: usize) -> Option<usize> {
        let cap = from.checked_add(MAX_SPAN)?;
        let hard = cap.min(self.bytes.len());
        let mut at = self.line_start_of(from);
        let mut walked = at.saturating_sub(from);
        while at < hard {
            let (line_end, next) = self.line_bounds(at);
            let line = &self.text[at..line_end];
            if line.trim().is_empty() || fence_opener(line).is_some() {
                return self.charge(walked).then(|| at.max(from));
            }
            if next <= at {
                break;
            }
            walked += next - at;
            at = next;
        }
        self.charge(walked).then_some(hard)
    }

    /// Pay for `bytes` of scanning; `false` once the pass is over budget (the
    /// remaining text is then left exactly as it is — degrading to the
    /// literal source is always allowed here).
    fn charge(&mut self, bytes: usize) -> bool {
        self.work += bytes;
        if self.work > MAX_SCAN_WORK {
            self.exhausted = true;
            return false;
        }
        true
    }
}

/// The HTML block a line opens — CommonMark 0.30 §4.6 “HTML blocks”.
///
/// Only the seven *start* conditions are modelled, plus the end condition that
/// matters for the scanner: while the block runs, markdown is not parsed, so
/// nothing inside may be rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HtmlBlock {
    /// Types 1–5 end on the first line **containing** this string (that line
    /// is part of the block): `</script>`/`</pre>`/`</style>`/`</textarea>`,
    /// `-->`, `?>`, `>`, `]]>`.
    Until(&'static str),
    /// Types 6–7 end at the next blank line.
    Blank,
}

/// The block-level tag names of CommonMark’s type-6 start condition.
const HTML_BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

/// Which HTML block `content` opens, if any — the *only* place that decides it.
///
/// Deliberately faithful to the seven start conditions instead of “any line
/// starting with `<…`”: treating a paragraph like `<3` or `<b>bold</b>` as a
/// block used to swallow a following fence line, and the scanner then rewrote
/// code-block content. `at_block_start` matters for type 7,
/// which cannot interrupt a paragraph.
fn html_block_start(content: &str, at_block_start: bool) -> Option<HtmlBlock> {
    let rest = content.strip_prefix('<')?;
    // Types 2–5: a same-line end marker.
    if rest.starts_with("!--") {
        return Some(HtmlBlock::Until("-->"));
    }
    if rest.starts_with('?') {
        return Some(HtmlBlock::Until("?>"));
    }
    if rest.starts_with("![CDATA[") {
        return Some(HtmlBlock::Until("]]>"));
    }
    if rest
        .strip_prefix('!')
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_alphabetic()))
    {
        return Some(HtmlBlock::Until(">"));
    }
    // Type 1: `<script`, `<pre`, `<style`, `<textarea` (case-insensitive)
    // followed by whitespace, `>` or EOL.
    for (tag, close) in [
        ("script", "</script>"),
        ("pre", "</pre>"),
        ("style", "</style>"),
        ("textarea", "</textarea>"),
    ] {
        if starts_with_tag(rest, tag) {
            return Some(HtmlBlock::Until(close));
        }
    }
    let (name, after) = html_tag_name(rest)?;
    // Type 6: a known block-level tag name, then whitespace, `>`, `/>` or EOL.
    if HTML_BLOCK_TAGS.contains(&name.to_ascii_lowercase().as_str())
        && (after.is_empty() || after.starts_with('>') || after.starts_with('/'))
    {
        return Some(HtmlBlock::Blank);
    }
    // Type 7: a complete tag and nothing but whitespace after it — and it
    // cannot interrupt a paragraph.
    if at_block_start && html_tag_only(content) {
        return Some(HtmlBlock::Blank);
    }
    None
}

/// The container an HTML block lives in (CommonMark's `containers` stack): the
/// block ends with its container, not with every line that happens to carry
/// some prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HtmlContainer {
    /// How many `>` markers the chain had when the block opened.
    quotes: usize,
    /// The list item's content column, when the block lives in one.
    item_col: Option<usize>,
}

impl HtmlContainer {
    fn of(prefix: &crate::render::markdown::stream::Prefix) -> Self {
        Self {
            quotes: prefix.quotes,
            item_col: prefix.item.map(|(_, col)| col),
        }
    }

    /// Whether `line` is still inside this container: the `>` chain and the
    /// item's content column must both still be satisfied (an HTML block inside
    /// a list item needs its content indented like any other item content).
    fn holds(&self, prefix: &crate::render::markdown::stream::Prefix) -> bool {
        prefix.quotes >= self.quotes && self.item_col.is_none_or(|col| prefix.columns >= col)
    }
}

/// Whether `line` ends the HTML block (its end condition is satisfied
/// *including* on the line itself — CommonMark includes that line in the
/// block).
fn html_block_ends(block: HtmlBlock, line: &str) -> bool {
    match block {
        HtmlBlock::Until(needle) => line.contains(needle),
        HtmlBlock::Blank => line.trim().is_empty(),
    }
}

/// `rest` (everything after `<`) starts with `tag` (ASCII case-insensitive)
/// followed by whitespace, `>` or end of line.
fn starts_with_tag(rest: &str, tag: &str) -> bool {
    let Some(head) = rest.get(..tag.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(tag) {
        return false;
    }
    match rest[tag.len()..].chars().next() {
        None => true,
        Some(c) => c.is_whitespace() || c == '>',
    }
}

/// The tag name at the start of `rest` (after `<`, possibly `/`), plus the
/// rest of the line after the name.
fn html_tag_name(rest: &str) -> Option<(&str, &str)> {
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let len = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '-')
        .count();
    if len == 0 {
        return None;
    }
    let (name, after) = rest.split_at(len);
    Some((name, after))
}

/// Whether the whole line is a single complete tag (`<b>`, `</div>`,
/// `<img src="x">`) followed by whitespace only.
fn html_tag_only(line: &str) -> bool {
    let line = line.trim();
    let start = if line.starts_with("</") { 2 } else { 1 };
    let Some(rest) = line.get(start..) else {
        return false;
    };
    let Some((_, mut after)) = html_tag_name(rest) else {
        return false;
    };
    // Walk attributes to the closing `>` (quoted values included).
    let bytes = after.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'>' => {
                after = &after[at + 1..];
                return after.trim().is_empty();
            }
            b'"' | b'\'' => {
                let quote = bytes[at];
                let Some(end) = bytes[at + 1..].iter().position(|&b| b == quote) else {
                    return false;
                };
                at += end + 2;
            }
            b'<' => return false,
            _ => at += 1,
        }
    }
    false
}

/// Whether `s` is a CommonMark title (`"…"`, `'…'` or `(…)`).
fn looks_like_title(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    matches!(
        (bytes[0], bytes[bytes.len() - 1]),
        (b'"', b'"') | (b'\'', b'\'') | (b'(', b')')
    )
}

/// Whether the line opens a link reference definition (`[label]: url`), and
/// whether the destination is still missing (it may then be on the next line).
///
/// The label may contain spaces — CommonMark allows it. A nested `]` inside the
/// label is not handled: such a line is simply not recognized, i.e. it is
/// scanned as prose (the conservative direction for a *miss* is the opposite,
/// so this stays a documented gap rather than a silent one).
fn reference_definition(content: &str) -> Option<bool> {
    let rest = content.strip_prefix('[')?;
    let close = rest.find(']')?;
    let after = rest[close + 1..].strip_prefix(':')?;
    Some(after.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(text: &str) -> String {
        normalize_delimiters(text).into_owned()
    }

    // ------------------------------------------------------------
    // The three rewrites
    // ------------------------------------------------------------

    #[test]
    fn rewrites_paren_delimiters_to_inline_math() {
        assert_eq!(
            norm(r"before \(x^2 + y^2\) after"),
            "before $x^2 + y^2$ after"
        );
        // Multi-line within one paragraph is fine (pulldown's inline math
        // spans soft breaks too).
        assert_eq!(norm("a \\(x\n+ y\\) b"), "a $x\n+ y$ b");
    }

    #[test]
    fn rewrites_bracket_delimiters_to_display_math() {
        assert_eq!(norm(r"\[E = mc^2\]"), "$$E = mc^2$$");
        // Edge whitespace is trimmed (one rule for every rewrite, see
        // `rewrite`): the delimiters end up hugging the source, which is also
        // what pulldown requires for a `$…$` pair to be math at all.
        assert_eq!(
            norm("before\n\\[\na = b\n\\]\nafter"),
            "before\n$$a = b$$\nafter"
        );
        assert_eq!(norm(r"\(  a + b  \)"), "$a + b$");
        assert_eq!(norm("\\[\n\\frac{a}{b}\n\\]"), "$$\\frac{a}{b}$$");
        // Nothing left to render after trimming: the source is left alone.
        assert_eq!(norm(r"\(   \)"), r"\(   \)");
    }

    #[test]
    fn wraps_a_bare_ams_environment_in_display_delimiters() {
        let src = "\\begin{align}\nf(x) &= x^2 \\\\\n&= (x+1)^2\n\\end{align}";
        assert_eq!(norm(src), format!("$${src}$$"));
        // Starred variants and the matrix family are engines-renderable too.
        assert_eq!(
            norm(r"\begin{align*}a\end{align*}"),
            r"$$\begin{align*}a\end{align*}$$"
        );
        assert_eq!(
            norm(r"\begin{pmatrix}a\end{pmatrix}"),
            r"$$\begin{pmatrix}a\end{pmatrix}$$"
        );
    }

    #[test]
    fn bare_environment_keeps_its_surrounding_text() {
        assert_eq!(
            norm("f(x) = \\begin{cases}1 & x > 0\\end{cases} done"),
            "f(x) = $$\\begin{cases}1 & x > 0\\end{cases}$$ done"
        );
    }

    #[test]
    fn same_name_nesting_pairs_with_the_matching_end() {
        let src = "\\begin{array}{l}\\begin{array}{l}a\\end{array}\\end{array}";
        assert_eq!(norm(src), format!("$${src}$$"));
    }

    /// Non-ASCII inside (or around) a span must not walk into the middle of a
    /// UTF-8 character: `\text{中文}` and CJK neighbours are ordinary input.
    #[test]
    fn multibyte_content_is_handled() {
        assert_eq!(
            norm("\\begin{align}\\text{中文} &= 1 \\end{align} 后面"),
            "$$\\begin{align}\\text{中文} &= 1 \\end{align}$$ 后面"
        );
        assert_eq!(norm("中文 \\(x_1 + 中文\\) 结尾"), "中文 $x_1 + 中文$ 结尾");
        assert_eq!(norm("`中文 \\(x\\)` 和 \\(y\\)"), "`中文 \\(x\\)` 和 $y$");
    }

    #[test]
    fn unknown_environment_is_left_alone() {
        let src = "\\begin{tikzcd} a \\arrow[r] & b \\end{tikzcd}";
        assert_eq!(norm(src), src);
        // …but a known one nested inside an unknown one still gets wrapped
        // only if it is complete, which here it is not.
        let src = "\\begin{tikzcd}\\begin{align}a\\end{align}\\end{tikzcd}";
        assert_eq!(
            norm(src),
            "\\begin{tikzcd}$$\\begin{align}a\\end{align}$$\\end{tikzcd}"
        );
    }

    // ------------------------------------------------------------
    // Opaque regions
    // ------------------------------------------------------------

    #[test]
    fn fenced_code_is_never_rewritten() {
        let src = "```latex\n\\(x\\)\n\\begin{align}a\\end{align}\n```\n";
        assert_eq!(norm(src), src);
        let src = "~~~\n\\(x\\)\n~~~\ntext \\(y\\)";
        assert_eq!(norm(src), "~~~\n\\(x\\)\n~~~\ntext $y$");
    }

    #[test]
    fn inline_code_is_never_rewritten() {
        let src = "use `\\(x\\)` here but \\(y\\) there";
        assert_eq!(norm(src), "use `\\(x\\)` here but $y$ there");
        // An unterminated backtick run is NOT a code span (CommonMark):
        // the text after it stays ordinary prose.
        let src = "a ` b \\(x\\)";
        assert_eq!(norm(src), "a ` b $x$");
    }

    #[test]
    fn existing_dollar_math_is_left_alone() {
        let src = "$$\n\\begin{align}\na &= b\n\\end{align}\n$$";
        assert_eq!(norm(src), src);
        let src = "$x$ and \\(y\\)";
        assert_eq!(norm(src), "$x$ and $y$");
        // Display math on one line.
        let src = "$$\\begin{align}a\\end{align}$$";
        assert_eq!(norm(src), src);
    }

    // ------------------------------------------------------------
    // Regions pulldown does not parse as text
    // ------------------------------------------------------------

    /// Inline HTML tags and autolinks: markdown is not parsed inside `<…>`, so
    /// a rewrite there would show up as literal `$`s (and would change an
    /// autolink's URL).
    ///
    /// The prose *between* two tags is markdown, exactly as it is for the
    /// baseline (`<span>*em*</span>` renders emphasis): only the tag itself is
    /// opaque.
    #[test]
    fn inline_html_tags_and_autolinks_are_left_alone() {
        for src in [
            "<http://x/\\(y\\)>",
            "<code>$$\\begin{align}a\\end{align}$$</code>",
            "<a href=\"http://x/\\(y\\)\">label</a>",
        ] {
            assert_eq!(norm(src), src, "{src:?} must not be rewritten");
        }
        // Between the tags the text is prose.
        assert_eq!(
            norm("before <b>\\(x\\)</b> after"),
            "before <b>$x$</b> after"
        );
        // A stray `<` (comparison, arrow) is not a region: the text after it
        // still normalizes.
        assert_eq!(norm("a <- \\(x\\)"), "a <- $x$");
    }

    /// An HTML block (`<div>` … blank line) is opaque, including the lines
    /// between its tags.
    #[test]
    fn html_blocks_are_left_alone() {
        let src = "<div>\n\\begin{align}a\\end{align}\n\\(x\\)\n</div>\n";
        assert_eq!(norm(src), src);
        // The block ends at the blank line: text after it normalizes again.
        assert_eq!(
            norm("<div>\n\\(a\\)\n</div>\n\n\\(b\\)\n"),
            "<div>\n\\(a\\)\n</div>\n\n$b$\n"
        );
        // A tag-like line inside a paragraph starts a block too (pulldown's
        // type-6 HTML block interrupts a paragraph).
        assert_eq!(norm("text\n<div>\n\\(x\\)\n"), "text\n<div>\n\\(x\\)\n");
    }

    /// A link / image destination is a URL: rewriting it would change what the
    /// click opens and what the OSC8 hyperlink carries.
    #[test]
    fn link_destinations_are_left_alone() {
        assert_eq!(norm("[a](http://x/\\(y\\))"), "[a](http://x/\\(y\\))");
        assert_eq!(
            norm("![img](http://x/\\(y\\) \"t \\(z\\)\")"),
            "![img](http://x/\\(y\\) \"t \\(z\\)\")"
        );
        // Nested parens in the URL keep the destination balanced.
        assert_eq!(
            norm("[a](http://x/(y)/\\(z\\))"),
            "[a](http://x/(y)/\\(z\\))"
        );
        // …and the label around it still normalizes (it is prose).
        assert_eq!(
            norm("[\\(x\\)](http://e/\\(y\\))"),
            "[$x$](http://e/\\(y\\))"
        );
    }

    /// A link reference definition line carries a URL in its destination and
    /// its title.
    #[test]
    fn reference_definitions_are_left_alone() {
        let src = "[ref]: http://x/\\(y\\) \"title \\(z\\)\"";
        assert_eq!(norm(src), src);
        // A label may contain spaces (CommonMark), and the destination and/or
        // the title may sit on the following line(s).
        for src in [
            "[a b]: http://x/\\(y\\)",
            "[a b]:\n  http://x/\\(y\\) \"t \\(z\\)\"",
            "[ref]:\nhttp://x/\\(y\\)",
            "[ref]: http://x/\\(y\\)\n  \"t \\(z\\)\"",
            "[ref]: <http://x/\\(y\\)>",
        ] {
            assert_eq!(norm(src), src, "{src:?} must not be rewritten");
        }
        // The line after the definition is prose again.
        assert_eq!(
            norm("[ref]: http://x\n\\(y\\) here"),
            "[ref]: http://x\n$y$ here"
        );
        assert_eq!(
            norm("[ref]: http://x/\\(y\\)\n\nuse \\(a\\)"),
            "[ref]: http://x/\\(y\\)\n\nuse $a$"
        );
        // Not a definition (no colon) — ordinary text.
        assert_eq!(norm("[ref] \\(a\\)"), "[ref] $a$");
    }

    // ------------------------------------------------------------
    // Block prefixes: `>` and list markers
    // ------------------------------------------------------------

    /// A fence or an indented block after a `>` / a list marker is still a
    /// fence / indented block to the parser: its content must not be
    /// rewritten.
    #[test]
    fn prefixed_code_regions_are_left_alone() {
        // Tilde fence inside a blockquote.
        let src = "> ~~~\n> \\begin{align}a\\end{align}\n> ~~~\n";
        assert_eq!(norm(src), src);
        // Indented code inside a blockquote.
        assert_eq!(norm(">     \\(x\\)\n"), ">     \\(x\\)\n");
        // Backtick fence inside a list item, and indented code inside one.
        assert_eq!(
            norm("- ```\n  \\(x\\)\n  ```\n"),
            "- ```\n  \\(x\\)\n  ```\n"
        );
        assert_eq!(
            norm("- item\n\n-     \\(x\\)\n"),
            "- item\n\n-     \\(x\\)\n"
        );
        // …but a prefixed line that is NOT code still normalizes.
        assert_eq!(norm("> \\(x\\)"), "> $x$");
        assert_eq!(norm("- \\(x\\)"), "- $x$");
        assert_eq!(norm("> text\n>     \\(x\\)"), "> text\n>     $x$");
    }

    #[test]
    fn indented_code_blocks_are_left_alone() {
        let src = "before\n\n    \\(x\\)\n\nafter";
        assert_eq!(norm(src), src);
        // A lazily continued paragraph line is prose, not code.
        let src = "text\n    \\(x\\)";
        assert_eq!(norm(src), "text\n    $x$");
    }

    #[test]
    fn currency_and_stray_backslashes_are_not_math() {
        for src in [
            "costs $100 and $200",
            "a bare backslash \\ here",
            r"a double backslash \\(x\\) is a line break",
        ] {
            assert_eq!(norm(src), src, "{src:?} must not be rewritten");
        }
    }

    // ------------------------------------------------------------
    // Limits: unterminated, cross-block, budget
    // ------------------------------------------------------------

    #[test]
    fn unterminated_delimiters_are_left_alone() {
        for src in [
            r"starts \(x + y",
            r"starts \[x + y",
            "\\begin{align}\na &= b\n",
            r"\)closing only",
            r"\]",
        ] {
            assert_eq!(norm(src), src, "{src:?} must not be rewritten");
        }
    }

    #[test]
    fn a_span_may_not_cross_a_blank_line_or_a_fence() {
        assert_eq!(norm("\\(a\n\nb\\)"), "\\(a\n\nb\\)");
        assert_eq!(norm("\\(a\n```\nb\n```\nc\\)"), "\\(a\n```\nb\n```\nc\\)");
        assert_eq!(
            norm("\\begin{align}\na\n\nb\n\\end{align}"),
            "\\begin{align}\na\n\nb\n\\end{align}"
        );
    }

    #[test]
    fn a_rewritten_prefix_stays_stable_as_it_grows() {
        // The streaming engine renders the same text per prefix; the local
        // rule means an unterminated prefix is literal and the finished span
        // is rewritten — both agree with the reference at that prefix.
        let full = "a \\(x + y\\) b";
        for end in 1..=full.len() {
            let prefix = &full[..end];
            let expected = if prefix.contains("\\)") {
                prefix.replace("\\(", "$").replace("\\)", "$")
            } else {
                prefix.to_string()
            };
            assert_eq!(norm(prefix), expected, "prefix {prefix:?}");
        }
    }

    #[test]
    fn nested_code_and_dollar_inside_a_span_blocks_the_rewrite() {
        assert_eq!(norm(r"\(a `b` c\)"), r"\(a `b` c\)");
        assert_eq!(norm(r"\(a $b$ c\)"), r"\(a $b$ c\)");
        assert_eq!(
            norm("\\begin{align}a `b` b\\end{align}"),
            "\\begin{align}a `b` b\\end{align}"
        );
    }

    #[test]
    fn rewrite_is_idempotent() {
        let once = norm("a \\(x\\) b \\[y\\] \\begin{align}z\\end{align}");
        assert_eq!(norm(&once), once);
    }

    /// A degenerate stream (thousands of unterminated `\(`) must not turn the
    /// pass quadratic: the work budget ends it, and everything stays literal.
    #[test]
    fn unterminated_delimiters_stay_cheap_and_literal() {
        let src = "\\(a ".repeat(4000);
        let started = std::time::Instant::now();
        let out = normalize_delimiters(&src);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.len(), src.len());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "degenerate input took {:?}",
            started.elapsed()
        );
    }

    /// A span longer than the engine's own source budget is not rewritten —
    /// it could not render anyway, so wrapping it would only add `$`s.
    #[test]
    fn overlong_spans_are_left_alone() {
        let src = format!("\\({}\\)", "x".repeat(10_000));
        assert!(matches!(normalize_delimiters(&src), Cow::Borrowed(_)));
    }

    #[test]
    fn text_without_latex_delimiters_is_borrowed() {
        let src = "plain text with $100 and a `code` span";
        assert!(matches!(normalize_delimiters(src), Cow::Borrowed(_)));
    }

    // ------------------------------------------------------------
    // The two safety rules (see `normalize_delimiters`)
    // ------------------------------------------------------------

    /// Rule 1: a rewrite may not put a delimiter right next to an existing
    /// `$`.
    ///
    /// Without the rule the `\(x\)` rewrite fuses into `$x$$$…`, the stray
    /// `$$` re-pairs with the inserted one and the text grows on every pass
    /// (`\(x\)$$\\(y\\)` is the shape that exposed it — deleting
    /// `fuses_with_dollar` makes this test fail).
    #[test]
    fn a_rewrite_never_fuses_with_an_existing_dollar() {
        for src in [
            r"\(x\)$$\\(y\\)",
            r"$$\\(x\\)",
            r"\(a\)$$",
            r"\(a\)$b$",
            r"$$\begin{align}a\end{align}$$",
            r"\begin{align}a\end{align}$",
        ] {
            assert_eq!(norm(src), src, "{src:?} must not be rewritten");
        }
        // The rule is about *adjacency*: one column of separation is enough.
        assert_eq!(norm(r"\(x\) $$"), "$x$ $$");
        assert_eq!(
            norm(r"\begin{align}a\end{align} x"),
            "$$\\begin{align}a\\end{align}$$ x"
        );
    }

    /// Rule 2: once the scan meets an unpaired `$$`, the rest of the block is
    /// left alone — an inserted `$$` would pair with the stray one.
    ///
    /// Deleting `stray_display_delim` makes the first two assertions fail: the
    /// stray `$$` is *before* the span, so only this rule can stop the rewrite
    /// (the `\(` is not adjacent to any `$`, so rule 1 stays silent).
    #[test]
    fn an_unpaired_display_delimiter_disarms_the_rest_of_the_block() {
        assert_eq!(norm(r"stray $$ here and \(x\)"), r"stray $$ here and \(x\)");
        assert_eq!(
            norm(
                "stray $$
\\begin{align}a\\end{align}"
            ),
            "stray $$
\\begin{align}a\\end{align}"
        );
        // A rewritable span BEFORE the stray delimiter is unaffected (the rule
        // is positional — see the docs).
        assert_eq!(norm(r"\(x\) then stray $$ here"), "$x$ then stray $$ here");
        // A *single* `$` does not disarm it: `$100` in prose cannot pair with
        // an inserted `$$`, and disarming there would cost the common case.
        assert_eq!(norm(r"price $100 and \(x\)"), "price $100 and $x$");
    }

    /// Seeded fuzz over the delimiter alphabet: the pass must never panic,
    /// must never change non-delimiter content, and must converge — a second
    /// pass is a no-op in every case except one, where it can wrap a bare
    /// environment that the first pass had swallowed inside a rewritten
    /// `$…$` span; after that it is stable.
    #[test]
    fn seeded_soup_never_panics_and_converges() {
        const TOKENS: &[&str] = &[
            "\\(",
            "\\)",
            "\\[",
            "\\]",
            "\\begin{align}",
            "\\end{align}",
            "\\begin{",
            "}",
            "{",
            "$",
            "$$",
            "`",
            "``",
            "\\",
            "x",
            "中文",
            "\n",
            "\n\n",
            "\n    ",
            "\\text{中文}",
            " ",
            "\\end{",
            // The shapes the safety rules exist for.
            "$x$",
            "\\(x\\)",
            "$$x$$",
            "~~~",
            "> ",
            "- ",
            "<div>",
            "](u)",
            "[r]: u",
            "a &= b",
            "\\\\",
            "(",
            ")",
        ];
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        for _ in 0..20_000 {
            let len = (next() % 24) as usize;
            let mut src = String::new();
            for _ in 0..len {
                src.push_str(TOKENS[(next() % TOKENS.len() as u64) as usize]);
            }
            let once = normalize_delimiters(&src).into_owned();
            let twice = normalize_delimiters(&once).into_owned();
            let thrice = normalize_delimiters(&twice).into_owned();
            assert_eq!(twice, thrice, "not convergent for {src:?}");
            assert_eq!(
                squash(&once),
                squash(&src),
                "content changed for {src:?} → {once:?}"
            );
        }
    }

    /// Everything the pass is allowed to touch is a math delimiter: strip the
    /// delimiters (and the whitespace they may have absorbed at their edges)
    /// and the two sides must be identical — the "no content is ever lost"
    /// invariant, at the normalizer level.
    fn squash(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if matches!(chars.peek(), Some('(' | ')' | '[' | ']')) => {
                    chars.next();
                }
                '$' => {}
                c if !c.is_whitespace() => out.push(c),
                _ => {}
            }
        }
        out
    }

    #[test]
    fn multiple_rewrites_in_one_line() {
        assert_eq!(
            norm(r"\(a\) then \(b\) and \[c\]"),
            "$a$ then $b$ and $$c$$"
        );
    }
}
