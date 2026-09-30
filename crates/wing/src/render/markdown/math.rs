//! Math (LaTeX) rendering: delimiter normalization + the two pulldown math
//! events.
//!
//! `pulldown-cmark` hands us `Event::InlineMath` / `Event::DisplayMath` for
//! `$…$` and `$$…$$`, but it knows nothing about the delimiters the models
//! actually emit most of the time:
//!
//! | source | pulldown sees | we do |
//! |---|---|---|
//! | `$x^2$` | `InlineMath("x^2")` | [`inline`] |
//! | `$$\na=b\n$$` | `DisplayMath("\na=b\n")` | [`display`] |
//! | `\(x\)` | `Text("(x")` + `Text(")")` | rewrite to `$x$` |
//! | `\[x\]` | `Text("[x")` + `Text("]")` | rewrite to `$$x$$` |
//! | `\begin{align}…\end{align}` | plain text + soft breaks | rewrite to `$$…$$` |
//!
//! The rewrites are a **source-text pre-pass** ([`normalize_delimiters`]) run
//! before parsing, so every render entry point that shares
//! `render_markdown_to_lines` — the full reference render, a promoted
//! streaming block, a re-parsed indented prose block — sees the same text and
//! produces the same output. See the module-level docs of `super::Profile`
//! for where the rule lives.
//!
//! ## Degradation: the source, never half a formula
//!
//! [`wing_math::render_inline`] / [`wing_math::render_display`] return `None`
//! for anything they will not render (too wide, suspected truncation, unknown
//! commands, a multiline block that cannot fit on one line — the full list is
//! in `wing_math`'s rustdoc). `None` is not an error: the segment then carries
//! the **complete source between its delimiters** (`$…$` / `$$…$$`), which is
//! byte-identical to what the TUI showed before math rendering existed. A
//! formula is never truncated, never replaced by an empty string and never
//! silently dropped.
//!
//! Every math segment — rendered grid or literal fallback — is tagged
//! [`SegmentKind::Math`] and styled with [`MarkdownTheme::math`], so it stays
//! a distinct element in both profiles (the thinking recolor leaves it alone,
//! like code).

use std::borrow::Cow;

use ratatui::style::Style;

use super::parsing::MarkdownContext;
use super::stream::{fence_open, indent_of, is_fence_close};
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
/// single row (see the module docs for the fallback).
pub(crate) fn inline(src: &str, ctx: &mut MarkdownContext<'_>) {
    // Inside a table cell the cell renderer owns the line; a block would
    // desynchronize the row accumulation.
    let text = match wing_math::render_inline(src) {
        Some(rendered) => rendered,
        None => format!("${src}$"),
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

// ============================================================
// Delimiter normalization (source pre-pass)
// ============================================================

/// Rewrite the math delimiters pulldown does not understand into the `$` /
/// `$$` form it does: `\(…\)` → `$…$`, `\[…\]` → `$$…$$`, and a bare
/// environment (`\begin{align}…\end{align}`) → `$$…$$`.
///
/// Never touches an opaque region:
///
/// - fenced code blocks (``` / ~~~),
/// - inline code spans (backticks),
/// - existing `$…$` / `$$…$$` math (so a wrapped environment stays wrapped),
/// - indented (4-space) code blocks,
/// - anything whose closing delimiter is missing, or that would cross a blank
///   line or a fence — pulldown's own math pairing does not cross a blank
///   line either, so leaving the source alone keeps the renderer's output
///   identical to the `$`-less literal it is today,
/// - a block that already holds an unpaired `$$` (inserting `$$` there would
///   re-pair the stray delimiter and grow the text on every pass).
///
/// The rewrite is purely local (a span is rewritten iff its own text is
/// well-formed), which is what lets the streaming engine normalize a *slice*
/// and still agree with the reference render of the whole document: slice
/// boundaries are exactly the blank lines / fences a span may not cross.
///
/// Two more refusals keep the output a pure function of the input rather than
/// of how often the pass ran — inserting a delimiter must never re-pair a `$`
/// that is already there:
///
/// - the delimiters a rewrite would insert must not touch an existing `$`
///   (`fuses_with_dollar`), or the fused `$$` would change what the next pass
///   sees;
/// - a block that holds an unpaired `$$` is left alone entirely.
///
/// A span's leading/trailing whitespace is trimmed, because `$` only pairs
/// against non-whitespace: `\(a \)` written out verbatim as `$a $` would not
/// be math at all.
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
    /// Open fenced code block: (fence char, run length).
    fence: Option<(u8, usize)>,
    /// Inside an indented (4-space) code block.
    indented: bool,
    /// Whether the line being scanned starts a block (text start, or right
    /// after a blank line / a fenced block). Only a block STARTING with an
    /// indented line is an indented code block — an indented line below
    /// paragraph text is a lazy continuation and stays prose.
    at_block_start: bool,
    /// The current block contains an unpaired `$$`.
    ///
    /// A rewrite inserts `$$`, and a stray `$$` would then pair with the
    /// inserted one on the next pass — the text would keep growing. Rather
    /// than produce delimiters nobody asked for, a block that already holds an
    /// unpaired `$$` is left alone (the formula degrades to its source, which
    /// is always allowed). Single `$` deliberately does not set this: `$100`
    /// in prose is common and cannot pair with an inserted `$$`.
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
                // Line-level state: fences and indented blocks are opaque,
                // blank lines reset the per-block scan.
                if let Some((fc, flen)) = self.fence {
                    if is_fence_close(&self.text[i..line_end], fc, flen) {
                        self.fence = None;
                        self.at_block_start = true;
                        self.stray_display_delim = false;
                    }
                    i = next;
                    line_start = next;
                    continue;
                }
                let line = &self.text[i..line_end];
                if let Some((fc, flen, _)) = fence_open(line) {
                    self.fence = Some((fc, flen));
                    i = next;
                    line_start = next;
                    continue;
                }
                if line.trim().is_empty() {
                    self.indented = false;
                    self.at_block_start = true;
                    self.stray_display_delim = false;
                    i = next;
                    line_start = next;
                    continue;
                }
                if self.indented {
                    if indent_of(line) >= 4 {
                        i = next;
                        line_start = next;
                        continue;
                    }
                    self.indented = false;
                } else if self.at_block_start && indent_of(line) >= 4 {
                    // An indented code block (CommonMark): it can only start
                    // a block, so a lazily continued paragraph line below it
                    // stays prose.
                    self.indented = true;
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
    /// backtick run, `$`, `\(`, `\[`, `\begin{`.
    fn next_token(&self, from: usize, line_end: usize) -> Option<usize> {
        let mut i = from;
        while i < line_end {
            match self.bytes[i] {
                b'`' | b'$' => return Some(i),
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

    /// Whether the delimiters a rewrite would insert at `[start, end)` fuse
    /// with a `$` that is already there into a `$$` pair.
    ///
    /// `$$` is a different region kind (it hides a bare environment from the
    /// next pass), so fusing would change what a second pass sees and the
    /// rewrite would stop being idempotent — the text would grow a `$$` per
    /// pass. Sources that put a `$` right next to a delimiter are left alone
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
            if line.trim().is_empty() || fence_open(line).is_some() {
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
