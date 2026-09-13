//! DiffView — IDE-style diff rendering for a single file.
//!
//! The look follows what GitHub / VS Code do, which is what the terminal
//! TUI needs to reach product level:
//!
//!   - changed lines are marked by a **tinted row background** (green for
//!     additions, red for deletions) with a stronger tint on the exact
//!     words that changed,
//!   - the **text keeps its syntax colors** (syntect) instead of every
//!     glyph being painted green/red,
//!   - rows carry an **old/new line-number gutter** and the collapsed
//!     regions become `@@` hunk headers, like `git diff` output.
//!
//! Rendering is two-staged: a **width-independent plan** (context
//! collapsing, syntax highlighting, inline emphasis — the expensive part)
//! is built once per diff and cached, while `to_lines` only re-tints and
//! re-pads it for the current width (a few microseconds, so window
//! resizes stay cheap).
//!
//! Syntax highlighting runs one stateful syntect pass per revision: lines
//! from the old file feed one highlighter and lines from the new file feed
//! another, in file order. Collapsed lines are still fed (their styled
//! output is dropped) so multi-line constructs — block comments, template
//! strings, brackets — stay correctly colored across hunks.

use std::cell::Ref;
use std::cell::RefCell;

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

/// Width of the row prefix after the line numbers: `" │ "` + marker + space.
const ROW_SUFFIX_WIDTH: usize = 5;

/// A diff view showing changes to a file.
#[derive(Debug, Clone)]
pub struct DiffView {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
    /// Width-independent render plan. Rebuilt when the requested context
    /// window differs from the one it was built for — the plan is only valid
    /// for its own `context_lines`, and a `OnceCell` could not re-key.
    ///
    /// The `RefCell` makes `DiffView` `!Sync` (it stays `Send`): cells are
    /// built and rendered on the UI thread only. Off-thread rendering
    /// (offscreen capture, parallel layout) would have to move this cache
    /// behind its own lock.
    plan: RefCell<Option<Plan>>,
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
    old_no: Option<usize>,
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
    /// `@@ -old,count +new,count @@` — a collapsed region boundary.
    Hunk(String),
    /// A code row.
    Code(Row),
}

/// Width-independent render plan.
#[derive(Debug, Clone)]
struct Plan {
    entries: Vec<Entry>,
    /// Context window the plan was collapsed for (cache key).
    context_lines: usize,
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
    pub fn new(path: String, old_text: Option<String>, new_text: String) -> Self {
        Self {
            path,
            old_text,
            new_text,
            plan: RefCell::new(None),
        }
    }

    /// Render the diff to lines with context-window collapsing.
    ///
    /// `width` is the render width: rows are padded to it so the tinted
    /// background covers the whole row (a `Line` background only covers the
    /// text extent — the same constraint the user-message card works around
    /// in `chat_view`).
    pub fn to_lines(
        &self,
        palette: &ThemePalette,
        context_lines: usize,
        width: u16,
    ) -> Vec<Line<'static>> {
        let dim = Style::default().fg(palette.dim);
        let plan = self.plan(context_lines);
        let width = width as usize;
        let mut lines = Vec::with_capacity(plan.entries.len() + 1);

        for entry in &plan.entries {
            match entry {
                Entry::Frame(path) => {
                    lines.push(Line::from(Span::styled(format!("  ┌─ {path}"), dim)));
                }
                Entry::Hunk(text) => {
                    lines.push(Line::from(Span::styled(
                        format!("  {text}"),
                        Style::default().fg(palette.accent),
                    )));
                }
                Entry::Code(row) => {
                    lines.push(render_row(&plan, row, palette, width));
                }
            }
        }

        // Footer.
        lines.push(Line::from(Span::styled("  └────", dim)));
        lines.push(Line::from(""));
        lines
    }

    /// The plan for `context_lines`, built on first use and rebuilt whenever
    /// a caller asks for a different context window.
    fn plan(&self, context_lines: usize) -> Ref<'_, Plan> {
        let stale = self
            .plan
            .borrow()
            .as_ref()
            .is_none_or(|plan| plan.context_lines != context_lines);
        if stale {
            *self.plan.borrow_mut() = Some(self.build_plan(context_lines));
        }
        Ref::map(self.plan.borrow(), |slot| {
            slot.as_ref().expect("plan built above")
        })
    }

    /// Build the width-independent plan: collapse context, highlight syntax,
    /// mark word-level emphasis.
    fn build_plan(&self, context_lines: usize) -> Plan {
        let mut plan = Plan {
            entries: Vec::new(),
            context_lines,
            number_width: 3,
            two_columns: self.old_text.is_some(),
        };
        plan.entries.push(Entry::Frame(self.path.clone()));

        // One old/new highlighter pair for the file; the first line lets
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
                        let runs = split_runs(hl.line(DiffSide::Insert, line, true), &[], line);
                        Row {
                            kind: DiffSide::Insert,
                            old_no: None,
                            new_no: Some(i + 1),
                            width: runs_width(&runs),
                            runs,
                        }
                    })
                    .collect();
                plan.number_width = number_width(rows.len());
                if !rows.is_empty() {
                    plan.entries
                        .push(Entry::Hunk(format!("@@ -0,0 +1,{} @@", rows.len())));
                }
                plan.entries.extend(rows.into_iter().map(Entry::Code));
            }
            Some(old) => {
                let diff = TextDiff::from_lines(old.as_str(), self.new_text.as_str());
                let visible = visible_rows(diff.ops(), context_lines);

                let mut old_consumed = 0usize;
                let mut new_consumed = 0usize;
                let mut block: Vec<Row> = Vec::new();
                let mut block_before = (0usize, 0usize);

                for (i, change) in diff.iter_all_inline_changes().enumerate() {
                    let (before_old, before_new) = (old_consumed, new_consumed);
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
                    // Every row advances the state of each revision it
                    // belongs to, visible or not (`DiffHighlighters`), but
                    // collapsed rows skip the styled output entirely.
                    let visible = is_visible(&visible, i);
                    let styled = hl.line(kind, &text, visible);
                    if !visible {
                        flush_block(&mut plan.entries, &mut block, block_before);
                        continue;
                    }

                    let runs = split_runs(styled, &emphasis, &text);
                    let row = Row {
                        kind,
                        old_no,
                        new_no,
                        width: runs_width(&runs),
                        runs,
                    };
                    if block.is_empty() {
                        block_before = (before_old, before_new);
                    }
                    block.push(row);
                }
                flush_block(&mut plan.entries, &mut block, block_before);

                plan.number_width = number_width(old_consumed.max(new_consumed));
            }
        }

        plan
    }
}

/// Whether the flat row `index` is inside a visible context window.
///
/// Fail-open when the mask is shorter than the walk (both come from the same
/// ops, so this only guards against a future `similar` behaviour change): a
/// row we cannot classify is shown rather than silently dropped.
fn is_visible(mask: &[bool], index: usize) -> bool {
    mask.get(index).copied().unwrap_or(true)
}

/// Emit one visible block: its `@@` hunk header followed by its rows.
fn flush_block(entries: &mut Vec<Entry>, block: &mut Vec<Row>, before: (usize, usize)) {
    if block.is_empty() {
        return;
    }
    let old_count = block.iter().filter(|r| r.old_no.is_some()).count();
    let new_count = block.iter().filter(|r| r.new_no.is_some()).count();
    let (old_before, new_before) = before;
    // git shows the preceding line number when a side contributes no line.
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

/// Render one code row: gutter (numbers + marker), content, full-row padding.
fn render_row(plan: &Plan, row: &Row, palette: &ThemePalette, width: usize) -> Line<'static> {
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

    let mut spans = Vec::with_capacity(row.runs.len() + 4);
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

    for run in &row.runs {
        let bg = if run.strong { tint_strong } else { tint };
        // No syntax style (unknown language) → the theme's text color, so an
        // unknown-language diff stays readable on light terminals too.
        let style = run
            .style
            .unwrap_or_else(|| Style::default().fg(palette.text));
        spans.push(Span::styled(run.text.clone(), tint_style(bg, Some(style))));
    }

    // Pad so the tint spans the whole row. Content wider than the view is
    // left alone (the layout wraps it).
    let used = plan.gutter_width() + row.width;
    if tint.is_some() && used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            tint_style(tint, None),
        ));
    }

    Line::from(spans)
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

/// Which flat row indices are visible: `context_lines` around each change.
fn visible_rows(ops: &[similar::DiffOp], context_lines: usize) -> Vec<bool> {
    use similar::DiffTag;
    let mut changed: Vec<bool> = Vec::new();
    for op in ops {
        let count = match op.tag() {
            DiffTag::Equal => op.old_range().len(),
            DiffTag::Delete => op.old_range().len(),
            DiffTag::Insert => op.new_range().len(),
            DiffTag::Replace => op.old_range().len() + op.new_range().len(),
        };
        changed.extend(std::iter::repeat_n(op.tag() != DiffTag::Equal, count));
    }

    let n = changed.len();
    let mut visible = vec![false; n];
    for (i, is_change) in changed.iter().enumerate() {
        if *is_change {
            let lo = i.saturating_sub(context_lines);
            let hi = (i + context_lines + 1).min(n);
            for slot in visible.iter_mut().skip(lo).take(hi - lo) {
                *slot = true;
            }
        }
    }
    visible
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

    fn find_line<'a>(lines: &'a [Line<'static>], needle: &str) -> Option<&'a Line<'static>> {
        lines.iter().find(|l| l.to_string().contains(needle))
    }

    fn rendered(diff: &DiffView, width: u16) -> String {
        diff.to_lines(&p(), 3, width)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_new_file_diff() {
        let diff = DiffView::new("main.rs".into(), None, "fn main() {}".into());
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
        let old = "fn old() {}\n".into();
        let new = "fn new() {}\n".into();
        let diff = DiffView::new("main.rs".into(), Some(old), new);
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
        let text = "fn same() {}\n".to_string();
        let diff = DiffView::new("main.rs".into(), Some(text.clone()), text);
        let lines = diff.to_lines(&p(), 3, WIDTH);
        let has_change = lines
            .iter()
            .any(|l| l.to_string().contains("│ +") || l.to_string().contains("│ -"));
        assert!(!has_change, "identical file should have no changes");
    }

    #[test]
    fn test_context_collapse_large_file() {
        let old_lines: Vec<String> = (1..=50).map(|i| format!("line {i}")).collect();
        let mut new_lines = old_lines.clone();
        new_lines[24] = "CHANGED".to_string();

        let old = old_lines.join("\n");
        let new = new_lines.join("\n");
        let diff = DiffView::new("big.rs".into(), Some(old), new);
        let lines = diff.to_lines(&p(), 3, WIDTH);

        assert!(
            lines.len() < 20,
            "expected context collapse, got {} lines",
            lines.len()
        );

        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("CHANGED"), "missing changed line");
        assert!(text.contains("@@"), "missing hunk header: {text}");
        assert!(!text.contains("⋮"), "gap marker replaced by hunk header");
    }

    /// Add/delete rows carry a tinted background while their text keeps
    /// distinct syntax colors.
    #[test]
    fn test_changed_rows_are_tinted_and_highlighted() {
        let old = "fn main() {\n    let name = \"a\";\n}\n";
        let new = "fn main() {\n    let name = \"b\";\n}\n";
        let diff = DiffView::new("main.rs".into(), Some(old.into()), new.into());
        let p = p();
        let lines = diff.to_lines(&p, 3, WIDTH);

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
        let diff = DiffView::new("main.rs".into(), Some(old.into()), new.into());
        let p = p();
        let lines = diff.to_lines(&p, 3, WIDTH);

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
        let diff = DiffView::new("main.rs".into(), Some(old.into()), new.into());
        let lines = diff.to_lines(&p(), 3, WIDTH);

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

    /// Extension-less files resolve their language from the file name (then
    /// the first line), so `Makefile` diffs are not forced into the plain
    /// fallback path.
    #[test]
    fn test_language_from_file_name() {
        let old = "all:\n\tcc -o app main.c\n";
        let new = "all:\n\tcc -O2 -o app main.c\n";
        let diff = DiffView::new("Makefile".into(), Some(old.into()), new.into());
        let lines = diff.to_lines(&p(), 3, WIDTH);
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
        let diff = DiffView::new("main.rs".into(), None, "fn main() {}\n".into());
        let lines = diff.to_lines(&p(), 3, WIDTH);
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
        let diff = DiffView::new("data.zzzq".into(), None, "fn main() {}\n".into());
        let p = p();
        let lines = diff.to_lines(&p, 3, WIDTH);
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

    /// Line numbers: old/new columns carry the right numbers.
    #[test]
    fn test_line_numbers() {
        let old = "a\nb\nc\n";
        let new = "a\nB\nc\n";
        let diff = DiffView::new("notes.txt".into(), Some(old.into()), new.into());
        let lines = diff.to_lines(&p(), 3, WIDTH);
        let row = |needle: &str| find_line(&lines, needle).unwrap().to_string();

        // "a" is line 1 on both sides; the change is line 2; "c" is line 3.
        assert!(row("│   a").starts_with("    1   1 │"), "{}", row("│   a"));
        assert!(row("│ - b").starts_with("    2     │"), "{}", row("│ - b"));
        assert!(row("│ + B").starts_with("        2 │"), "{}", row("│ + B"));
        assert!(row("│   c").starts_with("    3   3 │"), "{}", row("│   c"));
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
        let diff = DiffView::new("main.rs".into(), Some(old.into()), new.into());
        let p = p();
        let lines = diff.to_lines(&p, 3, WIDTH);

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
        let diff = DiffView::new("main.rs".into(), Some(old.into()), new.into());
        let p = p();
        for width in [24u16, 40, 60, 100, 121] {
            let lines = diff.to_lines(&p, 3, width);
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

    /// The plan is keyed by the context window: asking for a wider (or
    /// narrower) window than the cached one must rebuild, not silently reuse
    /// the old collapse.
    #[test]
    fn test_context_change_rebuilds_plan() {
        let old: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        let mut new = old.clone();
        new[19] = "CHANGED".to_string();
        let diff = DiffView::new("big.rs".into(), Some(old.join("\n")), new.join("\n"));

        let narrow = diff.to_lines(&p(), 1, WIDTH).len();
        let wide = diff.to_lines(&p(), 10, WIDTH).len();
        assert!(wide > narrow, "context change ignored: {narrow} vs {wide}");
        // …and back to the narrow window still collapses.
        assert_eq!(diff.to_lines(&p(), 1, WIDTH).len(), narrow);
    }
}
