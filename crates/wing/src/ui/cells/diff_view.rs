//! DiffView — IDE-style diff rendering for one diff payload.
//!
//! The look follows what GitHub / VS Code do, which is what the terminal
//! TUI needs to reach product level:
//!
//!   - changed lines are marked by a **tinted row background** (green for
//!     additions, red for deletions) with a stronger tint on the exact
//!     words that changed,
//!   - the **text keeps its syntax colors** (syntect) instead of every
//!     glyph being painted green/red,
//!   - rows carry an **old/new line-number gutter** and one `@@` hunk
//!     header, like `git diff` output.
//!
//! **The payload is already a window, not a file** (`diff-payload-window`):
//! the backend sends the changed region ± context lines plus the absolute
//! line number of the window's first line. This renderer shows exactly the
//! rows it was given — no context collapsing, no windowing policy of its
//! own. `old_start_line` / `new_start_line` seed the row counters, so the
//! gutter and the `@@` header carry real file line numbers.
//!
//! Rendering is two-staged: a **width-independent plan** (syntax
//! highlighting, inline emphasis — the expensive part) is built once per
//! diff and cached, while `to_lines` only re-tints, re-wraps and re-pads it
//! for the current width (a few microseconds, so window resizes stay cheap).
//!
//! **Wrapping is the view's job, with a hanging indent**: a row wider than
//! the render width is cut here (word wrap, column-exact fallback) and every
//! continuation row repeats the row's own indent — the gutter's columns,
//! blank — so the code column holds and the tint band of an add/delete row
//! runs on unbroken. Handing an over-wide row to the layout's `Paragraph`
//! would wrap it flush at column 0, sliding the code out from under the
//! line numbers.
//!
//! Syntax highlighting runs one stateful syntect pass per revision: lines
//! from the old revision feed one highlighter, lines from the new revision
//! feed another, in file order, so multi-line constructs — block comments,
//! template strings, brackets — stay correctly colored.

use std::cell::OnceCell;

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use similar::ChangeTag;
use similar::InlineChange;
use similar::TextDiff;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::render::diff_highlight::DiffHighlighters;
use crate::render::diff_highlight::DiffSide;
use crate::render::table::split_str_by_width;

/// Width of the row prefix after the line numbers: `" │ "` + marker + space.
const ROW_SUFFIX_WIDTH: usize = 5;

/// Fixed prefix of the frame line (`  ┌─ `) — a wrapped path continues in the
/// column it starts in.
const FRAME_PREFIX: &str = "  ┌─ ";

/// Fixed prefix of a hunk header line (`  `).
const HUNK_PREFIX: &str = "  ";

/// A diff view showing the window of changes to a file.
#[derive(Debug, Clone)]
pub struct DiffView {
    pub path: String,
    /// The window's old text; `None` for a new file (every row is an add).
    pub old_text: Option<String>,
    pub new_text: String,
    /// 1-based absolute line number of `old_text`'s first line in the old
    /// revision (gutter + `@@` header). Defaults to 1 for payloads that
    /// predate windowing (whole file, starting at line 1).
    pub old_start_line: usize,
    /// 1-based absolute line number of `new_text`'s first line.
    pub new_start_line: usize,
    /// Width-independent render plan. The window is immutable, so the plan
    /// is built once and reused for the lifetime of the view.
    ///
    /// The `OnceCell` makes `DiffView` `!Sync` (it stays `Send`): cells are
    /// built and rendered on the UI thread only. Off-thread rendering
    /// (offscreen capture, parallel layout) would have to move this cache
    /// behind its own lock.
    plan: OnceCell<Plan>,
}

/// One styled piece of a row (a syntax span, possibly split by emphasis).
#[derive(Debug, Clone)]
struct Run {
    text: String,
    /// Syntax style: foreground + font modifiers, never a background. `None`
    /// when nothing could be highlighted — the renderer then uses the theme's
    /// text color, so the plan stays palette-independent.
    style: Option<Style>,
    /// Word-level emphasis — render with the stronger background tint.
    strong: bool,
}

/// One code row of the plan.
#[derive(Debug, Clone)]
struct Row {
    kind: DiffSide,
    /// Absolute line number in the old revision (blank when absent there).
    old_no: Option<usize>,
    /// Absolute line number in the new revision.
    new_no: Option<usize>,
    runs: Vec<Run>,
    /// Display width of `runs`.
    width: usize,
}

/// One entry of the render plan, in display order.
#[derive(Debug, Clone)]
enum Entry {
    /// `  ┌─ path` frame line.
    Frame(String),
    /// `@@ -old,count +new,count @@` — absolute line numbers of the window.
    Hunk(String),
    /// A code row.
    Code(Row),
}

/// Width-independent render plan.
#[derive(Debug, Clone)]
struct Plan {
    entries: Vec<Entry>,
    /// Width of a single line-number column.
    number_width: usize,
    /// Whether the old revision gets its own number column.
    two_columns: bool,
}

impl Plan {
    /// Display width of the row prefix: indent + numbers + separator + marker.
    fn gutter_width(&self) -> usize {
        let numbers = if self.two_columns {
            2 * self.number_width + 1
        } else {
            self.number_width
        };
        2 + numbers + ROW_SUFFIX_WIDTH
    }
}

impl DiffView {
    pub fn new(
        path: String,
        old_text: Option<String>,
        new_text: String,
        old_start_line: usize,
        new_start_line: usize,
    ) -> Self {
        Self {
            path,
            old_text,
            new_text,
            old_start_line,
            new_start_line,
            plan: OnceCell::new(),
        }
    }

    /// Render the window to lines, verbatim: every line of `old_text` /
    /// `new_text` shows up, with its absolute line number in the gutter.
    ///
    /// `width` is the render width: rows are padded to it so the tinted
    /// background covers the whole row (a `Line` background only covers the
    /// text extent — the same constraint the user-message card works around
    /// in `chat_view`). Rows wider than `width` are wrapped **here** rather
    /// than by the layout's `Paragraph`: every visual row of a wrapped row
    /// keeps the row's own indent (the gutter columns stay blank, the code
    /// column holds, the tint band runs on unbroken) — see [`row_line`].
    pub fn to_lines(&self, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
        let dim = Style::default().fg(palette.dim);
        let plan = self.plan();
        let width = width as usize;
        let mut lines = Vec::with_capacity(plan.entries.len() + 1);

        for entry in &plan.entries {
            match entry {
                Entry::Frame(path) => {
                    push_chrome_line(&mut lines, FRAME_PREFIX, path, dim, width);
                }
                Entry::Hunk(text) => {
                    push_chrome_line(
                        &mut lines,
                        HUNK_PREFIX,
                        text,
                        Style::default().fg(palette.accent),
                        width,
                    );
                }
                Entry::Code(row) => {
                    lines.extend(render_row(plan, row, palette, width));
                }
            }
        }

        // Footer.
        lines.push(Line::from(Span::styled("  └────", dim)));
        lines.push(Line::from(""));
        lines
    }

    /// The plan, built on first use and reused for the view's lifetime.
    fn plan(&self) -> &Plan {
        self.plan.get_or_init(|| self.build_plan())
    }

    /// Build the width-independent plan: highlight syntax, mark word-level
    /// emphasis, number every row from the window's absolute start lines.
    fn build_plan(&self) -> Plan {
        let mut plan = Plan {
            entries: Vec::new(),
            number_width: 3,
            two_columns: self.old_text.is_some(),
        };
        plan.entries.push(Entry::Frame(self.path.clone()));

        // One old/new highlighter pair for the window; the first line lets
        // shebang scripts (extension-less) resolve their language.
        let first_line = self.new_text.lines().next().or_else(|| {
            self.old_text
                .as_deref()
                .and_then(|text| text.lines().next())
        });
        let mut hl = DiffHighlighters::for_file(&self.path, first_line);

        match &self.old_text {
            None => {
                // New file — every line is an addition.
                let rows: Vec<Row> = self
                    .new_text
                    .lines()
                    .enumerate()
                    .map(|(i, line)| {
                        let runs = split_runs(hl.line(DiffSide::Insert, line), &[], line);
                        Row {
                            kind: DiffSide::Insert,
                            old_no: None,
                            new_no: Some(self.new_start_line + i),
                            width: runs_width(&runs),
                            runs,
                        }
                    })
                    .collect();
                if !rows.is_empty() {
                    let last_line = self.new_start_line + rows.len() - 1;
                    plan.number_width = number_width(last_line);
                    plan.entries.push(Entry::Hunk(format!(
                        "@@ -0,0 +{},{} @@",
                        self.new_start_line,
                        rows.len()
                    )));
                }
                plan.entries.extend(rows.into_iter().map(Entry::Code));
            }
            Some(old) => {
                let diff = TextDiff::from_lines(old.as_str(), self.new_text.as_str());

                // Absolute line numbers: both counters start one line before
                // the window, so every row — and the `@@` header `flush_block`
                // derives from that starting pair — carries the real file line
                // instead of a window-relative index.
                let mut old_consumed = self.old_start_line.saturating_sub(1);
                let mut new_consumed = self.new_start_line.saturating_sub(1);
                let block_before = (old_consumed, new_consumed);
                let mut block: Vec<Row> = Vec::new();

                for change in diff.iter_all_inline_changes() {
                    let (kind, old_no, new_no) = match change.tag() {
                        ChangeTag::Equal => {
                            old_consumed += 1;
                            new_consumed += 1;
                            (DiffSide::Context, Some(old_consumed), Some(new_consumed))
                        }
                        ChangeTag::Delete => {
                            old_consumed += 1;
                            (DiffSide::Delete, Some(old_consumed), None)
                        }
                        ChangeTag::Insert => {
                            new_consumed += 1;
                            (DiffSide::Insert, None, Some(new_consumed))
                        }
                    };

                    let (text, emphasis) = inline_text(&change);
                    let runs = split_runs(hl.line(kind, &text), &emphasis, &text);
                    block.push(Row {
                        kind,
                        old_no,
                        new_no,
                        width: runs_width(&runs),
                        runs,
                    });
                }
                flush_block(&mut plan.entries, &mut block, block_before);

                plan.number_width = number_width(old_consumed.max(new_consumed));
            }
        }

        plan
    }
}

/// Emit the window's rows: its `@@` hunk header followed by its code rows.
///
/// `before` is the counter pair **before** the first row (the absolute start
/// lines minus one), so the header carries absolute line numbers; git's
/// convention is kept for a side that contributes no line (that side shows
/// the preceding line number instead of a start).
fn flush_block(entries: &mut Vec<Entry>, block: &mut Vec<Row>, before: (usize, usize)) {
    if block.is_empty() {
        return;
    }
    let old_count = block.iter().filter(|r| r.old_no.is_some()).count();
    let new_count = block.iter().filter(|r| r.new_no.is_some()).count();
    let (old_before, new_before) = before;
    let old_start = if old_count == 0 {
        old_before
    } else {
        old_before + 1
    };
    let new_start = if new_count == 0 {
        new_before
    } else {
        new_before + 1
    };
    entries.push(Entry::Hunk(format!(
        "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
    )));
    entries.extend(block.drain(..).map(Entry::Code));
}

/// Render one code row as one or more visual rows: gutter (numbers + marker),
/// content, full-row padding.
///
/// A row wider than the render width is wrapped **here**, not left to the
/// layout: the continuation rows repeat the gutter's columns as blank indent
/// ([`row_line`]), so the code column holds across the wrap and the tint band
/// stays continuous. `Paragraph`'s own wrap would break them back to column 0,
/// sliding the code out from under the numbers and cutting the band short.
fn render_row(plan: &Plan, row: &Row, palette: &ThemePalette, width: usize) -> Vec<Line<'static>> {
    // The hanging indent only pays while the code column gets at least as
    // much width as the gutter itself; below that (or at the gutter's own
    // width, where nothing is left over) the row renders as a single
    // over-wide line and the layout's full-width wrap is the better deal —
    // it keeps more columns of code per row than a wrap glued to the gutter.
    let gutter = plan.gutter_width();
    let wrap_limit = width
        .checked_sub(gutter)
        .filter(|limit| *limit >= gutter && row.width > *limit);
    match wrap_limit {
        Some(limit) => wrap_runs(&row.runs, limit)
            .iter()
            .enumerate()
            .map(|(i, runs)| row_line(plan, row, palette, runs, i == 0, width))
            .collect(),
        None => vec![row_line(plan, row, palette, &row.runs, true, width)],
    }
}

/// One visual line of a code row.
///
/// `first` picks the gutter (numbers + `│` + marker) over the continuation
/// indent: the same columns, left blank — the code keeps its column and the
/// line-number band reads as one uninterrupted block instead of the
/// continuation hugging the left edge.
fn row_line(
    plan: &Plan,
    row: &Row,
    palette: &ThemePalette,
    runs: &[Run],
    first: bool,
    width: usize,
) -> Line<'static> {
    let (tint, tint_strong, marker_style) = match row.kind {
        DiffSide::Insert => (
            Some(palette.diff_add_bg),
            Some(palette.diff_add_bg_strong),
            Style::default().fg(palette.success).bold(),
        ),
        DiffSide::Delete => (
            Some(palette.diff_del_bg),
            Some(palette.diff_del_bg_strong),
            Style::default().fg(palette.danger).bold(),
        ),
        DiffSide::Context => (None, None, Style::default().fg(palette.dim)),
    };
    let marker = match row.kind {
        DiffSide::Insert => '+',
        DiffSide::Delete => '-',
        DiffSide::Context => ' ',
    };

    let mut spans = Vec::with_capacity(runs.len() + 4);
    if first {
        spans.push(Span::styled("  ", tint_style(tint, None)));
        let number = Style::default().fg(palette.dim);
        if plan.two_columns {
            spans.push(Span::styled(
                number_text(row.old_no, plan.number_width),
                tint_style(tint, Some(number)),
            ));
            spans.push(Span::styled(" ", tint_style(tint, None)));
        }
        spans.push(Span::styled(
            number_text(row.new_no, plan.number_width),
            tint_style(tint, Some(number)),
        ));
        spans.push(Span::styled(" │ ", tint_style(tint, None)));
        spans.push(Span::styled(
            format!("{marker} "),
            tint_style(tint, Some(marker_style)),
        ));
    } else {
        spans.push(Span::styled(
            " ".repeat(plan.gutter_width()),
            tint_style(tint, None),
        ));
    }

    for run in runs {
        let bg = if run.strong { tint_strong } else { tint };
        // No syntax style (unknown language) → the theme's text color, so an
        // unknown-language diff stays readable on light terminals too.
        let style = run
            .style
            .unwrap_or_else(|| Style::default().fg(palette.text));
        spans.push(Span::styled(run.text.clone(), tint_style(bg, Some(style))));
    }

    // Pad so the tint spans the whole visual row — including the wrapped
    // continuation rows, so the band does not break at the wrap.
    let used = plan.gutter_width() + runs_width(runs);
    if tint.is_some() && used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            tint_style(tint, None),
        ));
    }

    Line::from(spans)
}

/// Push one chrome line (`prefix` + `body`) wrapped to `width`, continuation
/// rows holding `body`'s column: a long path or `@@` header continues under
/// itself instead of snapping back to the left edge.
///
/// Same threshold as a code row: the indent only pays while the continuation
/// still gets at least as much width as it spends, so a degenerate window
/// (a handful of columns) keeps the one over-wide line.
fn push_chrome_line(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    body: &str,
    style: Style,
    width: usize,
) {
    let indent = UnicodeWidthStr::width(prefix);
    let flat = format!("{prefix}{body}");
    if width < 2 * indent || UnicodeWidthStr::width(flat.as_str()) <= width {
        lines.push(Line::from(Span::styled(flat, style)));
        return;
    }
    let pad = " ".repeat(indent);
    for (i, row) in wrap_plain(body, width - indent).into_iter().enumerate() {
        let head = if i == 0 { prefix } else { &pad };
        lines.push(Line::from(Span::styled(format!("{head}{row}"), style)));
    }
}

/// Split a row's runs into segments of at most `limit` display columns each.
///
/// The row text is treated as one string so a break can fall inside a run;
/// every segment keeps the styles (and word-level emphasis) of the runs it
/// spans. Breaks land on the last space that fits (word wrap) with a
/// column-exact cut as the fallback, and the spaces at a break are dropped —
/// the caller re-adds the indent.
fn wrap_runs(runs: &[Run], limit: usize) -> Vec<Vec<Run>> {
    let mut flat = String::new();
    let mut spans: Vec<(usize, usize, Option<Style>, bool)> = Vec::with_capacity(runs.len());
    for run in runs {
        let start = flat.len();
        flat.push_str(&run.text);
        spans.push((start, flat.len(), run.style, run.strong));
    }

    let mut segments = Vec::new();
    let mut start = 0usize;
    while start < flat.len() {
        let rest = &flat[start..];
        if UnicodeWidthStr::width(rest) <= limit {
            // A whitespace tail is a wrap artefact, not content: dropping it
            // keeps trailing spaces from costing a blank visual row.
            let rest = rest.trim_end();
            if !rest.is_empty() || segments.is_empty() {
                segments.push(slice_runs(&spans, &flat, start, start + rest.len()));
            }
            break;
        }
        let cut = break_point(rest, limit);
        let head = rest[..cut].trim_end();
        if !head.is_empty() {
            segments.push(slice_runs(&spans, &flat, start, start + head.len()));
        }
        // Skip the spaces at the break: a wrapped row never starts with the
        // boundary whitespace it was cut at.
        start += cut + (rest[cut..].len() - rest[cut..].trim_start_matches(' ').len());
    }
    if segments.is_empty() {
        segments.push(Vec::new());
    }
    segments
}

/// Rebuild the runs covering `[a, b)` of the flattened row text, cutting a run
/// in half when a wrap boundary lands inside it.
fn slice_runs(
    spans: &[(usize, usize, Option<Style>, bool)],
    flat: &str,
    a: usize,
    b: usize,
) -> Vec<Run> {
    let mut runs = Vec::new();
    for &(start, end, style, strong) in spans {
        let start = start.max(a);
        let end = end.min(b);
        if start >= end {
            continue;
        }
        runs.push(Run {
            text: flat[start..end].to_string(),
            style,
            strong,
        });
    }
    runs
}

/// The rows of `text` wrapped to `limit` display columns (at least one row,
/// empty only for empty input).
fn wrap_plain(text: &str, limit: usize) -> Vec<&str> {
    let mut rows = Vec::new();
    let mut rest = text;
    loop {
        if UnicodeWidthStr::width(rest) <= limit {
            // Trailing spaces of the last row are a wrap artefact, not
            // content: dropping them keeps a whitespace tail from costing a
            // visual row (an empty band row on a tinted code row).
            let rest = rest.trim_end();
            if !rest.is_empty() || rows.is_empty() {
                rows.push(rest);
            }
            return rows;
        }
        let cut = break_point(rest, limit);
        let head = rest[..cut].trim_end();
        if !head.is_empty() {
            rows.push(head);
        }
        rest = rest[cut..].trim_start_matches(' ');
    }
}

/// Byte index to cut `text` at so that the head fits `limit` display columns:
/// the last space that fits (word wrap), or the column itself when the window
/// holds no space (a long token, CJK runs).
///
/// Always in `1..text.len()` for a non-empty `text` that is over-wide, so the
/// callers always make progress: a window whose only space is the leading one
/// is cut by column (cutting at 0 would push nothing and stall the loop), and
/// a single grapheme wider than the window is cut alone.
///
/// A space cut is only taken when it leaves something in the head — a row of
/// pure indentation would otherwise swallow the leading whitespace of an
/// over-wide line (an indented statement would lose its indentation), so that
/// case is cut by column instead.
fn break_point(text: &str, limit: usize) -> usize {
    let (head, _) = split_str_by_width(text, limit);
    if head.is_empty() {
        return text
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(text.len());
    }
    match head.rfind(' ') {
        Some(i) if !head[..i].trim().is_empty() => i,
        _ => head.len(),
    }
}

/// Line-number cell: right-aligned, blanks when the row is absent there.
fn number_text(no: Option<usize>, width: usize) -> String {
    match no {
        Some(no) => format!("{no:>width$}"),
        None => " ".repeat(width),
    }
}

/// Overlay a background tint onto a style (foreground preserved).
fn tint_style(bg: Option<Color>, style: Option<Style>) -> Style {
    let style = style.unwrap_or_default();
    match bg {
        Some(bg) => style.bg(bg),
        None => style,
    }
}

/// Build a row's runs from a highlighter result, splitting the spans at the
/// emphasized byte ranges (both are byte offsets into the same line text).
///
/// `None` means nothing could be highlighted (unknown language, or a parse
/// failure): the row renders as plain text, the tint still carrying the
/// add/delete signal.
fn split_runs(
    spans: Option<Vec<(Style, String)>>,
    emphasis: &[(usize, usize)],
    text: &str,
) -> Vec<Run> {
    let Some(spans) = spans else {
        return vec![Run {
            text: text.to_string(),
            style: None,
            strong: false,
        }];
    };

    if emphasis.is_empty() {
        return spans
            .into_iter()
            .map(|(style, text)| Run {
                text,
                style: Some(style),
                strong: false,
            })
            .collect();
    }

    let len = text.len();
    let mut runs = Vec::with_capacity(spans.len());
    let mut pos = 0usize;
    for (style, text) in spans {
        let end = (pos + text.len()).min(len);
        let mut cursor = pos;
        for &(start, stop) in emphasis {
            let (start, stop) = (start.max(pos), stop.min(end));
            if start >= stop {
                continue;
            }
            if start > cursor {
                runs.push(Run {
                    text: text[cursor - pos..start - pos].to_string(),
                    style: Some(style),
                    strong: false,
                });
            }
            runs.push(Run {
                text: text[start - pos..stop - pos].to_string(),
                style: Some(style),
                strong: true,
            });
            cursor = stop;
        }
        if cursor < end {
            runs.push(Run {
                text: text[cursor - pos..end - pos].to_string(),
                style: Some(style),
                strong: false,
            });
        }
        pos += text.len();
    }
    runs
}

/// Line text + emphasized byte ranges of one inline change.
fn inline_text(change: &InlineChange<'_, str>) -> (String, Vec<(usize, usize)>) {
    let mut text = String::new();
    let mut emphasis = Vec::new();
    for (emphasized, value) in change.iter_strings_lossy() {
        if emphasized && !value.is_empty() {
            emphasis.push((text.len(), text.len() + value.len()));
        }
        text.push_str(&value);
    }
    // Rows render without the trailing newline; emphasis ranges stay valid
    // because they are byte offsets into the same text.
    if text.ends_with('\n') {
        text.pop();
        if let Some((_, stop)) = emphasis.last_mut() {
            *stop = (*stop).min(text.len());
        }
        emphasis.retain(|(start, stop)| start < stop);
    }
    (text, emphasis)
}

/// Line-number column width for a file of `lines` lines (min 3, matching the
/// code-block gutter).
fn number_width(lines: usize) -> usize {
    lines.max(1).to_string().len().max(3)
}

/// Display width of a run list.
fn runs_width(runs: &[Run]) -> usize {
    runs.iter()
        .map(|r| UnicodeWidthStr::width(r.text.as_str()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: u16 = 72;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    /// A view whose window starts at line 1 on both sides (the Write / legacy
    /// payload shape).
    fn view(old: Option<&str>, new: &str) -> DiffView {
        DiffView::new("main.rs".into(), old.map(str::to_string), new.into(), 1, 1)
    }

    /// A view for a window that starts at `start` in both revisions.
    fn windowed(old: &str, new: &str, start: usize) -> DiffView {
        DiffView::new(
            "main.rs".into(),
            Some(old.to_string()),
            new.to_string(),
            start,
            start,
        )
    }

    fn find_line<'a>(lines: &'a [Line<'static>], needle: &str) -> Option<&'a Line<'static>> {
        lines.iter().find(|l| l.to_string().contains(needle))
    }

    fn rendered(diff: &DiffView, width: u16) -> String {
        diff.to_lines(&p(), width)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The rendered code rows (everything carrying the ` │ ` gutter), without
    /// the frame / hunk header / footer.
    fn code_rows(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.to_string())
            .filter(|text| text.contains(" │ "))
            .collect()
    }

    fn numbered(total: usize) -> String {
        (1..=total)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_new_file_diff() {
        let diff = view(None, "fn main() {}");
        let text = rendered(&diff, WIDTH);
        assert!(text.contains("main.rs"), "missing path: {text}");
        assert!(text.contains('+'), "missing add marker: {text}");
        assert!(
            text.contains("@@ -0,0 +1,1 @@"),
            "missing hunk header: {text}"
        );
    }

    #[test]
    fn test_modified_file_diff() {
        let diff = view(Some("fn old() {}"), "fn new() {}");
        let text = rendered(&diff, WIDTH);
        assert!(text.contains('-'), "missing delete marker: {text}");
        assert!(text.contains('+'), "missing add marker: {text}");
        assert!(
            text.contains("@@ -1,1 +1,1 @@"),
            "missing hunk header: {text}"
        );
    }

    #[test]
    fn test_identical_file_diff() {
        let text = "fn same() {}";
        let diff = view(Some(text), text);
        let lines = diff.to_lines(&p(), WIDTH);
        let has_change = lines
            .iter()
            .any(|l| l.to_string().contains("│ +") || l.to_string().contains("│ -"));
        assert!(!has_change, "identical payload should have no changes");
    }

    /// The payload is rendered verbatim: every row of the window shows up,
    /// including context rows far from the change. (Collapsing the window
    /// again on the frontend is exactly what this change removed — an
    /// implementation that brought it back would render 4 rows here, not 10.)
    #[test]
    fn window_is_rendered_verbatim() {
        let old = numbered(10);
        // Change on the first row: with context collapsing only rows 1..4
        // would survive.
        let new = old.replacen("line 1", "CHANGED", 1);
        let diff = windowed(&old, &new, 1);

        let lines = diff.to_lines(&p(), WIDTH);
        let rows = code_rows(&lines);

        // One delete + one insert + the 9 remaining context rows: every row of
        // the window shows up, none is collapsed away. (An implementation that
        // collapsed context again would render 4 rows here, not 11.)
        assert_eq!(rows.len(), 11, "window rows must all render: {rows:?}");
        assert!(rows[0].contains("line 1"));
        assert!(rows[1].contains("CHANGED"));
        assert!(
            rows[10].contains("line 10"),
            "last window row: {:?}",
            rows[10]
        );
        // One hunk header, no per-hunk re-collapse.
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.to_string().contains("@@"))
                .count(),
            1
        );
    }

    /// A legacy full-file payload (no windowing, no start lines) still renders
    /// the whole payload — nothing is hidden, nothing is lost.
    #[test]
    fn legacy_full_file_payload_renders_whole_file() {
        let old = numbered(50);
        let new = old.replacen("line 25", "CHANGED", 1);
        let diff = view(Some(&old), &new);

        let lines = diff.to_lines(&p(), WIDTH);
        let rows = code_rows(&lines);
        // 50 old rows + the inserted replacement line (a Replace = delete+insert).
        assert_eq!(rows.len(), 51);
        assert!(rows.iter().any(|r| r.contains("line 50")));
        assert!(rows.iter().any(|r| r.contains("CHANGED")));
    }

    /// The `@@` header carries the window's absolute line numbers, not
    /// window-relative ones — and its counts are the window's row counts.
    #[test]
    fn hunk_header_uses_absolute_start_lines() {
        let old = "a\nb\nCHANGE\nc\nd"; // 5 rows
        let new = "a\nb\nX\nY\nZ\nc\nd"; // 7 rows
        let diff = windowed(old, new, 37);

        let text = rendered(&diff, WIDTH);
        assert!(text.contains("@@ -37,5 +37,7 @@"), "{text}");
    }

    /// Gutter numbers are absolute file lines: the first row of a window
    /// starting at 37 shows 37 (not 1), and they advance per revision.
    #[test]
    fn gutter_numbers_are_absolute() {
        let old = "a\nb\nCHANGE\nc";
        let new = "a\nb\nX\nY\nc";
        let diff = windowed(old, new, 37);

        let lines = diff.to_lines(&p(), WIDTH);
        let row = |needle: &str| find_line(&lines, needle).unwrap().to_string();

        assert!(row("│   a").starts_with("   37  37 │"), "{}", row("│   a"));
        assert!(
            row("│ - CHANGE").starts_with("   39     │"),
            "{}",
            row("│ - CHANGE")
        );
        assert!(row("│ + X").starts_with("       39 │"), "{}", row("│ + X"));
        assert!(row("│ + Y").starts_with("       40 │"), "{}", row("│ + Y"));
        assert!(row("│   c").starts_with("   40  41 │"), "{}", row("│   c"));
    }

    /// New-file windows honour `new_start_line` too (a Write payload starts at
    /// 1, but the field is what the gutter trusts).
    #[test]
    fn new_file_window_uses_start_line() {
        let diff = DiffView::new("new.rs".into(), None, "one\ntwo\nthree".into(), 1, 10);

        let text = rendered(&diff, WIDTH);
        assert!(text.contains("@@ -0,0 +10,3 @@"), "{text}");

        let lines = diff.to_lines(&p(), WIDTH);
        let rows = code_rows(&lines);
        assert!(rows[0].starts_with("   10 │ + one"), "{:?}", rows[0]);
        assert!(rows[2].starts_with("   12 │ + three"), "{:?}", rows[2]);
    }

    /// Rows whose line number column grew to 4 digits keep the gutter aligned
    /// (the width is computed from the window's last absolute line).
    #[test]
    fn gutter_width_follows_absolute_numbers() {
        let diff = windowed("a\nb\nc", "a\nB\nc", 1000);
        let lines = diff.to_lines(&p(), WIDTH);
        let rows = code_rows(&lines);
        assert!(rows[0].starts_with("  1000 1000 │"), "{:?}", rows[0]);
        assert!(rows[1].starts_with("  1001      │"), "{:?}", rows[1]);
        assert!(rows[2].starts_with("       1001 │"), "{:?}", rows[2]);
        assert!(rows[3].starts_with("  1002 1002 │"), "{:?}", rows[3]);
    }

    /// Add/delete rows carry a tinted background while their text keeps
    /// distinct syntax colors.
    #[test]
    fn test_changed_rows_are_tinted_and_highlighted() {
        let old = "fn main() {\n    let name = \"a\";\n}\n";
        let new = "fn main() {\n    let name = \"b\";\n}\n";
        let diff = view(Some(old), new);
        let p = p();
        let lines = diff.to_lines(&p, WIDTH);

        let add_line = find_line(&lines, "│ + ").expect("add row");
        let del_line = find_line(&lines, "│ - ").expect("delete row");

        for span in add_line.spans.iter() {
            assert!(
                matches!(span.style.bg, Some(bg) if bg == p.diff_add_bg || bg == p.diff_add_bg_strong),
                "add row not tinted: {:?}",
                span.style.bg
            );
        }
        for span in del_line.spans.iter() {
            assert!(
                matches!(span.style.bg, Some(bg) if bg == p.diff_del_bg || bg == p.diff_del_bg_strong),
                "del row not tinted: {:?}",
                span.style.bg
            );
        }

        let fgs: std::collections::BTreeSet<_> = add_line
            .spans
            .iter()
            .map(|s| format!("{:?}", s.style.fg))
            .collect();
        assert!(
            fgs.len() >= 3,
            "expected several syntax colors on the row, got {fgs:?}"
        );
    }

    /// Changed words use the stronger tint, unchanged words keep the row tint.
    #[test]
    fn test_word_level_emphasis() {
        let old = "let total = compute(alpha, beta_one);\n";
        let new = "let total = compute(alpha, beta_two);\n";
        let diff = view(Some(old), new);
        let p = p();
        let lines = diff.to_lines(&p, WIDTH);

        let add_line = find_line(&lines, "│ + ").expect("add row");
        let bgs: Vec<_> = add_line.spans.iter().map(|s| s.style.bg).collect();
        assert!(
            bgs.contains(&Some(p.diff_add_bg)),
            "missing row tint: {bgs:?}"
        );
        assert!(
            bgs.contains(&Some(p.diff_add_bg_strong)),
            "missing emphasis tint: {bgs:?}"
        );
    }

    /// A construct opened in an unchanged line must still color a deleted
    /// line that continues it: the old revision's highlighter has to advance
    /// over context lines, not just over the deleted ones (otherwise every
    /// later `-` line is highlighted from a stale state).
    #[test]
    fn test_deleted_line_keeps_old_revision_state() {
        let old = "fn main() {\n    /* note\n    let removed = 1;\n    */\n}\n";
        let new = "fn main() {\n    /* note\n    */\n}\n";
        let diff = view(Some(old), new);
        let lines = diff.to_lines(&p(), WIDTH);

        let fg_of = |needle: &str| {
            lines
                .iter()
                .find(|l| l.to_string().contains(needle))
                .and_then(|l| l.spans.iter().find(|s| s.content.contains(needle)))
                .and_then(|s| s.style.fg)
        };
        // syntect splits a comment line into several spans ("/*", " note"),
        // so the needle has to fit inside one span of the row.
        let comment = fg_of("note").expect("comment row");
        let removed = fg_of("removed").expect("deleted row");
        let code = fg_of("main").expect("code row");

        assert_ne!(comment, code, "fixture should color comments differently");
        assert_eq!(
            removed, comment,
            "deleted line lost the old revision's comment state"
        );
    }

    /// The reported bug, at the view level: a `.py` file whose first inline
    /// comment grayed out every row below it. Diff rows carry no line
    /// terminator, and the comment scope only pops on one.
    #[test]
    fn test_trailing_comment_does_not_gray_out_later_rows() {
        let new =
            "import pty\n\na[0] = 0  # iflag\na[6][termios.VMIN] = 1\na[6][termios.VTIME] = 0\n";
        let diff = DiffView::new("pty.py".into(), None, new.into(), 1, 1);
        let lines = diff.to_lines(&p(), WIDTH);

        // syntect splits both rows into several spans, so the needle has to
        // fit inside one of them.
        let fg_of = |needle: &str| {
            lines
                .iter()
                .find(|l| l.to_string().contains(needle))
                .and_then(|l| l.spans.iter().find(|s| s.content.contains(needle)))
                .and_then(|s| s.style.fg)
        };
        let comment = fg_of("iflag").expect("comment row");
        let below = fg_of("VMIN").expect("row below the comment");
        assert_ne!(below, comment, "row below the comment is comment-colored");
    }

    /// Extension-less files resolve their language from the file name (then
    /// the first line), so `Makefile` diffs are not forced into the plain
    /// fallback path.
    #[test]
    fn test_language_from_file_name() {
        let old = "all:\n\tcc -o app main.c\n";
        let new = "all:\n\tcc -O2 -o app main.c\n";
        let diff = DiffView::new("Makefile".into(), Some(old.into()), new.into(), 1, 1);
        let lines = diff.to_lines(&p(), WIDTH);
        let row = find_line(&lines, "cc -o app").expect("changed row");
        let fgs: std::collections::BTreeSet<_> = row
            .spans
            .iter()
            .map(|s| format!("{:?}", s.style.fg))
            .collect();
        assert!(
            fgs.len() >= 2,
            "Makefile row not syntax-highlighted: {fgs:?}"
        );
    }

    /// Rows are padded to the render width so the tint covers the row.
    #[test]
    fn test_rows_pad_to_width() {
        let diff = view(None, "fn main() {}\n");
        let lines = diff.to_lines(&p(), WIDTH);
        let add_line = find_line(&lines, "│ + ").expect("add row");
        assert_eq!(
            UnicodeWidthStr::width(add_line.to_string().as_str()),
            WIDTH as usize
        );
    }

    /// Unknown language → tint only, no syntax colors: the content renders in
    /// the theme's text color — a definite foreground, so the band stays
    /// readable on light terminals (where the terminal default would be dark).
    #[test]
    fn test_unknown_language_falls_back_to_tint() {
        let diff = DiffView::new("data.zzzq".into(), None, "fn main() {}\n".into(), 1, 1);
        let p = p();
        let lines = diff.to_lines(&p, WIDTH);
        let add_line = find_line(&lines, "│ + ").expect("add row");

        // The content span (found by text — a positional skip would silently
        // stop covering it if the gutter ever grows another column).
        let content = add_line
            .spans
            .iter()
            .find(|s| s.content.contains("fn main"))
            .expect("content span");
        assert_eq!(
            content.style.fg,
            Some(p.text),
            "fallback should use the theme's text color"
        );
        assert_eq!(content.style.bg, Some(p.diff_add_bg));

        // Code and padding (everything after the marker) carry no syntax
        // colors; the tint is uniform across the row.
        let body: Vec<_> = add_line
            .spans
            .iter()
            .skip_while(|s| s.content != "+ ")
            .skip(1)
            .collect();
        assert!(
            body.iter().any(|s| s.content.contains("fn main")),
            "content span missing"
        );
        assert!(
            body.iter()
                .all(|s| s.style.fg.is_none() || s.style.fg == Some(p.text)),
            "unexpected syntax color: {:?}",
            body.iter().map(|s| s.style.fg).collect::<Vec<_>>()
        );
        assert!(
            add_line
                .spans
                .iter()
                .all(|s| s.style.bg == Some(p.diff_add_bg)),
            "row not uniformly tinted"
        );
    }

    /// Line numbers: old/new columns carry the right numbers (window-relative
    /// arithmetic once the absolute start is 1).
    #[test]
    fn test_line_numbers() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let diff = view(Some(old), new);
        let lines = diff.to_lines(&p(), WIDTH);
        let row = |needle: &str| find_line(&lines, needle).unwrap().to_string();

        // "a" is line 1 on both sides; the change is line 2; "c" is line 3.
        assert!(row("│   a").starts_with("    1   1 │"), "{}", row("│   a"));
        assert!(row("│ - b").starts_with("    2     │"), "{}", row("│ - b"));
        assert!(row("│ + B").starts_with("        2 │"), "{}", row("│ + B"));
        assert!(row("│   c").starts_with("    3   3 │"), "{}", row("│   c"));
    }

    /// A window with an empty old revision (whole file replaced by "")
    /// renders every row as a deletion and keeps git's `-0,0`-style header
    /// for the absent side.
    #[test]
    fn whole_file_deletion_renders_all_rows() {
        let diff = DiffView::new("gone.rs".into(), Some("a\nb".into()), String::new(), 1, 1);

        let lines = diff.to_lines(&p(), WIDTH);
        let rows = code_rows(&lines);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.contains("│ -")));
        assert!(
            rendered(&diff, WIDTH).contains("@@ -1,2 +0,0 @@"),
            "{}",
            rendered(&diff, WIDTH)
        );
    }

    /// End-to-end through ratatui: an add row is tinted across the full width,
    /// and padded rows never wrap into a phantom row.
    #[test]
    fn test_buffer_row_fully_tinted() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::widgets::{Paragraph, Widget, Wrap};

        let old = "fn main() {\n    let a = 1;\n}\n";
        let new = "fn main() {\n    let a = 2;\n}\n";
        let diff = view(Some(old), new);
        let p = p();
        let lines = diff.to_lines(&p, WIDTH);

        let mut term = Terminal::new(TestBackend::new(WIDTH, lines.len() as u16)).unwrap();
        term.draw(|f| {
            let area = f.area();
            Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .render(area, f.buffer_mut());
        })
        .unwrap();

        let buf = term.backend().buffer();
        let mut tinted_rows = 0;
        for y in 0..buf.area.height {
            let row: Vec<_> = (0..buf.area.width).map(|x| buf[(x, y)].clone()).collect();
            let is_tint = |bg: Color| bg == p.diff_add_bg || bg == p.diff_add_bg_strong;
            if row.iter().any(|c| is_tint(c.bg)) {
                tinted_rows += 1;
                assert!(
                    row.iter().all(|c| is_tint(c.bg)),
                    "row {y} partially tinted: {:?}",
                    row.iter().map(|c| c.bg).collect::<Vec<_>>()
                );
            }
        }
        assert_eq!(tinted_rows, 1, "expected exactly one add row");
    }

    /// Padding a visible row to exactly the view width must not push it over
    /// it: an off-by-one would make the layout wrap every diff row into a
    /// phantom second row, and every cell height (and therefore the whole
    /// scroll geometry) would be wrong. Content here fits at every tested
    /// width, so every row must stay within the width and render as one row.
    #[test]
    fn padded_rows_do_not_wrap() {
        use ratatui::widgets::{Paragraph, Wrap};

        let old = "x = 1;\ny = 2;\n";
        let new = "x = 1;\ny = 3;\n";
        let diff = view(Some(old), new);
        let p = p();
        for width in [24u16, 40, 60, 100, 121] {
            let lines = diff.to_lines(&p, width);
            assert!(lines.len() >= 5, "expected a full diff at {width}");
            for line in &lines {
                let text = line.to_string();
                let used = UnicodeWidthStr::width(text.as_str());
                assert!(
                    used <= width as usize,
                    "row overflows {width}: {used} cols: {text:?}"
                );
                let rows = Paragraph::new(vec![line.clone()])
                    .wrap(Wrap { trim: false })
                    .line_count(width);
                assert_eq!(rows, 1, "row wrapped into {rows} at {width}: {text:?}");
            }
        }
    }

    /// The plan is built once (the window is immutable): rendering again —
    /// including at another width — reuses it, so the rows (and their line
    /// numbers) stay identical; only the row padding follows the width (and
    /// only rows over the new width wrap, which this fixture avoids).
    #[test]
    fn plan_is_reused_across_renders() {
        let diff = windowed("a\nb\nc", "a\nB\nc", 1);
        let first = diff.to_lines(&p(), WIDTH);
        let again = diff.to_lines(&p(), WIDTH);
        assert_eq!(first.len(), again.len());
        assert_eq!(
            rendered(&diff, WIDTH),
            rendered(&diff, WIDTH),
            "same width renders identically"
        );

        let narrow = diff.to_lines(&p(), 40);
        assert_eq!(
            narrow.len(),
            first.len(),
            "a window that fits at both widths renders the same rows"
        );
        for (a, b) in first.iter().zip(narrow.iter()) {
            assert_eq!(a.to_string().trim_end(), b.to_string().trim_end());
        }
    }

    /// The reported bug: an over-wide row reached `Paragraph`, which wrapped
    /// it flush at column 0 — the continuation slid out from under the gutter
    /// and the tint band broke. Rows must wrap inside the view instead, with
    /// every continuation row holding the row's own indent.
    #[test]
    fn wrapped_rows_keep_the_code_column() {
        use ratatui::widgets::{Paragraph, Wrap};

        let long = "let x = a_very_long_name + another_very_long_name + yet_another_name;";
        let diff = view(None, &format!("{long}\n"));
        let p = p();
        let lines = diff.to_lines(&p, 40);

        // Gutter "   1 │ + " is 10 columns wide, so the code wraps at 30 and
        // the break falls on the last space that fits: three rows, each one
        // padded to the full width so the tint covers it.
        let start = lines
            .iter()
            .position(|l| l.to_string().contains("│ +"))
            .expect("add row");
        let padded = |s: &str| format!("{s:<40}");
        // The code column: `  ` + 3-digit number + ` │ ` + marker + space.
        let code_column = " ".repeat(10);
        let rows: Vec<String> = lines[start..start + 3]
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(
            rows,
            vec![
                padded("    1 │ + let x = a_very_long_name +"),
                padded(&format!("{code_column}another_very_long_name +")),
                padded(&format!("{code_column}yet_another_name;")),
            ],
            "continuation rows must hold the code column (10) and the tint padding"
        );
        assert!(
            lines[start + 3].to_string().contains('└'),
            "the add row must be wrapped into exactly three rows: {:?}",
            lines[start + 3].to_string()
        );

        // One continuous tint band: every span of every wrapped row carries
        // the add tint, gutter indent included.
        for line in &lines[start..start + 3] {
            assert!(
                line.spans.iter().all(
                    |span| matches!(span.style.bg, Some(bg) if bg == p.diff_add_bg || bg == p.diff_add_bg_strong)
                ),
                "wrapped row not fully tinted: {:?}",
                line.spans.iter().map(|s| s.style.bg).collect::<Vec<_>>()
            );
        }

        // Rows are exact: `Paragraph` has nothing left to wrap.
        for line in &lines[start..start + 3] {
            assert_eq!(
                Paragraph::new(vec![line.clone()])
                    .wrap(Wrap { trim: false })
                    .line_count(40),
                1,
                "wrapped row re-wraps: {:?}",
                line.to_string()
            );
        }
    }

    /// Two-column windows hold their wider gutter across the wrap (old + new
    /// number columns are blank on the continuation rows), and an untinted
    /// context row stays unpadded — the indent is alignment, not paint.
    #[test]
    fn wrapped_two_column_rows_keep_the_gutter_indent() {
        let long = "let a = alpha + beta + gamma + delta;";
        let diff = windowed(&format!("{long}\nkeep"), &format!("{long}\nKEEP"), 1);
        let p = p();
        let lines = diff.to_lines(&p, 40);
        let rows: Vec<String> = lines.iter().map(|l| l.to_string()).collect();

        // Gutter "   1   1 │   " is 14 columns wide, so the code wraps at 26:
        // the break is the last space before "gamma".
        let start = rows
            .iter()
            .position(|r| r.contains("│   let a"))
            .expect("context row");
        assert_eq!(rows[start], "    1   1 │   let a = alpha + beta +");
        assert_eq!(rows[start + 1], format!("{}gamma + delta;", " ".repeat(14)));
    }

    /// The chrome lines wrap with their column held too: a long path
    /// continues under itself (5 columns of `  ┌─ `), a wrapped `@@` header
    /// continues two columns in.
    #[test]
    fn wrapped_chrome_lines_hold_their_column() {
        let path = "src/modules/alpha/beta/gamma/file.rs";
        let diff = DiffView::new(path.into(), None, "fn main() {}\n".into(), 1, 1);
        let rows: Vec<String> = diff
            .to_lines(&p(), 35)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(rows[0], "  ┌─ src/modules/alpha/beta/gamma/f");
        assert_eq!(rows[1], "     ile.rs");

        // Below twice the prefix's width (the frame's `  ┌─ ` is 5) the
        // hanging indent would leave the body ~2 columns: keep one over-wide
        // line instead.
        let narrow: Vec<String> = diff
            .to_lines(&p(), 8)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(narrow[0], "  ┌─ src/modules/alpha/beta/gamma/file.rs");

        // Window starting at line 100000: the `@@` header is wider than 24
        // columns, so it wraps after the old-side range.
        let diff = windowed("a\nb", "a\nB", 100000);
        let rows: Vec<String> = diff
            .to_lines(&p(), 24)
            .iter()
            .map(|l| l.to_string())
            .collect();
        let hunk = rows
            .iter()
            .position(|r| r.starts_with("  @@"))
            .expect("hunk header");
        assert_eq!(rows[hunk], "  @@ -100000,2");
        assert_eq!(rows[hunk + 1], "  +100000,2 @@");
    }

    /// The hanging indent only pays while the code column gets at least as
    /// much width as the gutter itself (10 columns here): at 20 the code row
    /// wraps into four visual rows of ten columns each, below it the row
    /// stays one over-wide line (the layout's full-width wrap keeps more
    /// code per row than a hanging indent into a one-column body would).
    #[test]
    fn too_narrow_for_the_code_column_stays_unwrapped() {
        let diff = view(None, &format!("{}\n", "x".repeat(32)));
        let add_row = |width: u16| {
            let lines = diff.to_lines(&p(), width);
            let start = lines
                .iter()
                .position(|l| l.to_string().contains("│ +"))
                .expect("add row");
            (lines, start)
        };
        for width in [10, 12, 19] {
            let (lines, start) = add_row(width);
            // The footer right after the row: it was not wrapped (a wrapped
            // row would continue with indent-only rows in between).
            assert!(
                lines[start + 1].to_string().starts_with("  └"),
                "unexpected wrap at {width}: {:?}",
                lines[start + 1].to_string()
            );
        }

        let (lines, start) = add_row(20);
        let rows: Vec<String> = lines[start..start + 4]
            .iter()
            .map(|l| l.to_string())
            .collect();
        let code_column = " ".repeat(10);
        assert_eq!(
            rows,
            vec![
                "    1 │ + xxxxxxxxxx".to_string(),
                format!("{code_column}xxxxxxxxxx"),
                format!("{code_column}xxxxxxxxxx"),
                format!("{code_column}xx        "),
            ]
        );
    }

    /// An over-wide line whose leading indent is followed by a token too long
    /// for the window keeps that indent on the first visual row: the row may
    /// not be cut inside the indentation (the statement would look unindented
    /// at the top level).
    #[test]
    fn wrapped_rows_keep_the_leading_indent() {
        // 8 columns of indent + a 24-column token: at 40 the code column is
        // 30, so the first row is a column-exact cut (the only space in the
        // window is inside the indentation, where a break would drop it).
        let line = "        some_function_name(arg1, arg2)";
        let diff = view(None, &format!("{line}\n"));
        let lines = diff.to_lines(&p(), 40);
        let start = lines
            .iter()
            .position(|l| l.to_string().contains("│ +"))
            .expect("add row");
        let rows: Vec<String> = lines[start..start + 2]
            .iter()
            .map(|l| l.to_string())
            .collect();
        let code_column = " ".repeat(10);
        assert_eq!(
            rows,
            vec![
                "    1 │ +         some_function_name(arg".to_string(),
                format!("{:<40}", format!("{code_column}1, arg2)")),
            ]
        );
    }

    /// A CJK line has no space to break at: the wrap cuts it by column —
    /// never inside a wide character — and the continuation holds the code
    /// column like any other row.
    #[test]
    fn wrapped_wide_characters_cut_at_the_column() {
        let diff = DiffView::new(
            "notes.txt".into(),
            None,
            "这是一条很长的中文注释内容\n".into(),
            1,
            1,
        );
        let lines = diff.to_lines(&p(), 32); // code column 10, content 22
        let start = lines
            .iter()
            .position(|l| l.to_string().contains("│ +"))
            .expect("add row");
        let rows: Vec<String> = lines[start..start + 2]
            .iter()
            .map(|l| l.to_string())
            .collect();
        let code_column = " ".repeat(10);
        assert_eq!(
            rows,
            vec![
                // 11 wide characters = 22 columns, cut on the character edge.
                "    1 │ + 这是一条很长的中文注释".to_string(),
                // 2 wide characters, then the tint padding (32 - 10 - 4).
                format!("{code_column}内容{}", " ".repeat(18)),
            ]
        );
    }

    /// A delete row wraps like an add row — the continuation keeps the number
    /// column's indent and the delete tint.
    #[test]
    fn wrapped_delete_rows_keep_the_delete_tint() {
        let long = "let x = a_very_long_name + another_very_long_name + yet_another_name;";
        let diff = windowed(&format!("{long}\n"), "let x = 1;\n", 1);
        let p = p();
        let lines = diff.to_lines(&p, 40); // two-column gutter 14, content 26
        let start = lines
            .iter()
            .position(|l| l.to_string().contains("│ -"))
            .expect("delete row");
        let rows: Vec<String> = lines[start..start + 3]
            .iter()
            .map(|l| l.to_string())
            .collect();
        let code_column = " ".repeat(14);
        assert_eq!(
            rows,
            vec![
                format!("{:<40}", "    1     │ - let x = a_very_long_name"),
                format!("{:<40}", format!("{code_column}+ another_very_long_name")),
                format!("{:<40}", format!("{code_column}+ yet_another_name;")),
            ]
        );
        for line in &lines[start..start + 3] {
            assert!(
                line.spans.iter().all(
                    |span| matches!(span.style.bg, Some(bg) if bg == p.diff_del_bg || bg == p.diff_del_bg_strong)
                ),
                "wrapped delete row not fully tinted: {:?}",
                line.spans.iter().map(|s| s.style.bg).collect::<Vec<_>>()
            );
        }
    }
}
