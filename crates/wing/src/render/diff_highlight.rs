//! Two-revision syntax highlighting for diffs.
//!
//! A diff interleaves lines from two revisions, but a syntect highlighter is
//! a state machine that must be fed each revision's lines **in file order** —
//! otherwise multi-line constructs (block comments, template strings,
//! heredocs, brackets) lose their context and whole hunks get the wrong
//! colors.
//!
//! That means:
//!
//!   - `Delete` lines feed the old revision's highlighter,
//!   - `Insert` lines feed the new revision's highlighter,
//!   - context (unchanged) lines belong to **both** revisions, so they advance
//!     the old revision's state as well — even though the colors rendered are
//!     the new revision's.
//!
//! Every row of a diff window is rendered (the payload IS the window — the
//! frontend never collapses it), so feeding a line and rendering it are the
//! same operation.
//!
//! Both diff renderers ([`crate::ui::cells::diff_view`] and the fenced
//! ```diff``` block renderer) share this type so the rules cannot drift apart
//! between them.

use ratatui::style::Style;
use syntect::easy::HighlightLines;

use super::syntax::advance_line;
use super::syntax::highlight_line_with;
use super::syntax::new_highlighter_for_file;

/// Which revision(s) a diff line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffSide {
    /// Unchanged line — present in both revisions.
    Context,
    /// Added line — exists only in the new revision.
    Insert,
    /// Deleted line — exists only in the old revision.
    Delete,
}

/// The old/new highlighter pair of one diffed file.
pub(crate) struct DiffHighlighters {
    old: Option<HighlightLines<'static>>,
    new: Option<HighlightLines<'static>>,
}

impl DiffHighlighters {
    /// Highlighters for `path`'s language, or a pair of `None`s when the
    /// language is unknown (callers then fall back to plain text).
    pub(crate) fn for_file(path: &str, first_line: Option<&str>) -> Self {
        Self {
            old: new_highlighter_for_file(path, first_line),
            new: new_highlighter_for_file(path, first_line),
        }
    }

    /// Feed one line of `side` to the matching highlighter(s) and return its
    /// styled spans (`None` for an unknown language — callers then render
    /// plain text). Every revision the line belongs to advances by exactly
    /// one line.
    pub(crate) fn line(&mut self, side: DiffSide, text: &str) -> Option<Vec<(Style, String)>> {
        // Context lines exist in both revisions: the old state has to advance
        // too, or every later `Delete` line is highlighted from a stale state.
        if side == DiffSide::Context
            && let Some(old) = self.old.as_mut()
        {
            advance_line(old, text);
        }

        let highlighter = match side {
            DiffSide::Delete => self.old.as_mut(),
            DiffSide::Context | DiffSide::Insert => self.new.as_mut(),
        }?;

        highlight_line_with(highlighter, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A construct opened in an unchanged line must still color the deleted
    /// line that continues it (the old revision's state has to advance over
    /// context lines).
    #[test]
    fn context_lines_advance_the_old_revision() {
        let mut hl = DiffHighlighters::for_file("main.rs", None);
        let context = hl
            .line(DiffSide::Context, "/* note")
            .expect("context highlight");
        let deleted = hl
            .line(DiffSide::Delete, "   removed();")
            .expect("delete highlight");

        let fg = |spans: &[(Style, String)]| spans[0].0.fg;
        assert_eq!(
            fg(&deleted),
            fg(&context),
            "deleted line not in the comment's state: {deleted:?}"
        );
    }

    /// A deleted line that opens a construct must not leak its state into
    /// later deletes through a closed context line.
    #[test]
    fn delete_state_stays_inside_the_old_revision() {
        let mut hl = DiffHighlighters::for_file("main.rs", None);
        hl.line(DiffSide::Delete, "let a = \"unterminated");
        hl.line(DiffSide::Context, "let b = 1;");
        let after = hl
            .line(DiffSide::Delete, "let c = 2;")
            .expect("delete highlight");
        let plain = hl
            .line(DiffSide::Insert, "let d = 3;")
            .expect("insert highlight");
        // The new revision never saw the old line's string opener.
        assert_ne!(
            after[0].0.fg, plain[0].0.fg,
            "old revision state leaked: {after:?} / {plain:?}"
        );
    }

    /// Unknown language → no highlighting at all, and advancing is a no-op.
    #[test]
    fn unknown_language_is_inert() {
        let mut hl = DiffHighlighters::for_file("data.zzzq", None);
        assert!(hl.line(DiffSide::Context, "whatever").is_none());
        assert!(hl.line(DiffSide::Delete, "whatever").is_none());
    }
}
