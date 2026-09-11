//! SelectionPanel — the shared kernel behind interactive selection panels.
//!
//! A panel is a sequence of pages (tabs). Each page is either a list of rows
//! the kernel navigates, or a *custom* page the adapter fully owns (AskPanel's
//! confirm page: the kernel keeps its tab slot but no cursor). The kernel
//! provides, once for every adapter:
//!
//! - page switching (`←`/`→`, wraps) and page-local cursor movement
//!   (`↑`/`↓`, wraps) with per-page cursor memory,
//! - single-select commit capture (`commit_current`), decoupled from later
//!   cursor movement,
//! - visible-window math ([`window_range`], default [`PANEL_WINDOW`]) for the
//!   tab bar and the option rows,
//! - the refresh fallback policy ([`SelectionPanel::clamp_after_refresh`]).
//!
//! **Storage belongs to the adapter.** AskPanel keeps one cursor per question
//! in its `QuestionState`; ModelPanel keeps one per provider. Adapters
//! implement [`SelectionPanel`] over their own state — the default methods are
//! the entire navigation/commit/refresh contract. `Enter` semantics, confirm
//! pages, multi-select state and inline editors are adapter concerns; the
//! kernel MUST NOT contain any of them.

/// Default number of visibly rendered tabs / option rows.
pub const PANEL_WINDOW: usize = 5;

/// Shape of one page (tab).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// Kernel-navigated rows (may be zero, e.g. an empty model list).
    Options { rows: usize },
    /// Adapter-owned page: no kernel cursor; still participates in tab
    /// navigation and window computation.
    Custom,
}

/// Navigation kernel contract, implemented by each panel adapter over its own
/// storage. See the module docs for the division of responsibilities.
pub trait SelectionPanel {
    /// Number of pages (tabs), custom pages included.
    fn page_count(&self) -> usize;

    /// Shape of `page` (0-based).
    fn page_kind(&self, page: usize) -> PageKind;

    /// Index of the active page.
    fn current_page(&self) -> usize;

    /// Move the active page to `page`; callers pass a valid in-range index.
    fn set_current_page(&mut self, page: usize);

    /// Cursor row of `page` (0 for pages without cursor rows).
    fn cursor_at(&self, page: usize) -> usize;

    /// Set the cursor row of `page`; callers pass a valid in-range index.
    fn set_cursor_at(&mut self, page: usize, row: usize);

    /// The captured single-select row of `page`, if any.
    fn committed_at(&self, page: usize) -> Option<usize>;

    /// Set (or clear) the captured row of `page`.
    fn set_committed_at(&mut self, page: usize, row: Option<usize>);

    /// Move the cursor on the active page (wraps). No-op on custom pages and
    /// on pages without rows.
    fn move_cursor(&mut self, delta: isize) {
        let page = self.current_page();
        let PageKind::Options { rows } = self.page_kind(page) else {
            return;
        };
        if rows == 0 {
            return;
        }
        self.set_cursor_at(page, wrap_index(self.cursor_at(page), delta, rows));
    }

    /// Switch the active page (wraps; custom pages participate).
    fn move_page(&mut self, delta: isize) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        self.set_current_page(wrap_index(self.current_page(), delta, count));
    }

    /// Capture the option under the cursor as the active page's committed
    /// selection. The capture is a snapshot, not a derivation: later cursor
    /// movement cannot change it, and committing again overrides it. Returns
    /// the captured row (`None` on custom pages and pages without rows).
    ///
    /// *When* a commit happens is an adapter decision (Ask commits on Enter
    /// over an option row; ModelPanel seeds the currently active model).
    fn commit_current(&mut self) -> Option<usize> {
        let page = self.current_page();
        let PageKind::Options { rows } = self.page_kind(page) else {
            return None;
        };
        if rows == 0 {
            return None;
        }
        let row = self.cursor_at(page);
        self.set_committed_at(page, Some(row));
        Some(row)
    }

    /// Refresh fallback: keep the active page when it still exists, otherwise
    /// fall back to the first page; clamp cursors and captured rows into the
    /// valid range of the (possibly changed) page data.
    fn clamp_after_refresh(&mut self) {
        let count = self.page_count();
        if count == 0 {
            self.set_current_page(0);
            return;
        }
        if self.current_page() >= count {
            self.set_current_page(0);
        }
        for page in 0..count {
            if let PageKind::Options { rows } = self.page_kind(page) {
                let cursor = self.cursor_at(page).min(rows.saturating_sub(1));
                self.set_cursor_at(page, cursor);
                if self.committed_at(page).is_some_and(|row| row >= rows) {
                    self.set_committed_at(page, None);
                }
            }
        }
    }
}

/// Wrap `index + delta` into `0..len` (empty list → 0).
pub fn wrap_index(index: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (index as isize + delta).rem_euclid(len as isize) as usize
}

/// Visible range of a sliding window of `size` items that always contains
/// `cursor`:
///
/// - fewer items than the window → the whole list,
/// - cursor before the window's right edge → window pinned at the start,
/// - cursor past the right edge → window slides right (the cursor ends up on
///   the last visible slot).
///
/// The render layer compares the range against `0` / `len` to draw the
/// `‹` / `›` markers on the hidden sides.
pub fn window_range(cursor: usize, len: usize, size: usize) -> std::ops::Range<usize> {
    if len == 0 || size == 0 {
        return 0..0;
    }
    if len <= size {
        return 0..len;
    }
    let cursor = cursor.min(len - 1);
    let start = cursor.saturating_sub(size - 1).min(len - size);
    start..start + size
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal host: one cursor slot per page, kind list fixed at construction.
    struct Host {
        pages: Vec<PageKind>,
        current: usize,
        cursors: Vec<usize>,
        committed: Vec<Option<usize>>,
    }

    impl Host {
        fn new(pages: &[Option<usize>]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|rows| match rows {
                        Some(rows) => PageKind::Options { rows: *rows },
                        None => PageKind::Custom,
                    })
                    .collect(),
                current: 0,
                cursors: vec![0; pages.len()],
                committed: vec![None; pages.len()],
            }
        }
    }

    impl SelectionPanel for Host {
        fn page_count(&self) -> usize {
            self.pages.len()
        }
        fn page_kind(&self, page: usize) -> PageKind {
            self.pages[page]
        }
        fn current_page(&self) -> usize {
            self.current
        }
        fn set_current_page(&mut self, page: usize) {
            self.current = page;
        }
        fn cursor_at(&self, page: usize) -> usize {
            self.cursors[page]
        }
        fn set_cursor_at(&mut self, page: usize, row: usize) {
            self.cursors[page] = row;
        }
        fn committed_at(&self, page: usize) -> Option<usize> {
            self.committed[page]
        }
        fn set_committed_at(&mut self, page: usize, row: Option<usize>) {
            self.committed[page] = row;
        }
    }

    // ── Page / cursor navigation ────────────────────────────────

    #[test]
    fn page_switch_wraps() {
        let mut host = Host::new(&[Some(2), Some(2), Some(2)]);
        host.move_page(-1);
        assert_eq!(host.current_page(), 2, "← from the first page wraps");
        host.move_page(1);
        assert_eq!(host.current_page(), 0);
        host.move_page(1);
        assert_eq!(host.current_page(), 1);
    }

    #[test]
    fn cursor_memory_is_per_page() {
        let mut host = Host::new(&[Some(5), Some(3)]);
        host.move_cursor(1);
        host.move_cursor(1);
        assert_eq!(host.cursor_at(0), 2);
        host.move_page(1); // page B
        host.move_cursor(1);
        assert_eq!(host.cursor_at(1), 1);
        host.move_page(-1); // back to A
        assert_eq!(host.cursor_at(0), 2, "page A restores its own cursor");
        assert_eq!(host.cursor_at(1), 1, "page B keeps its cursor too");
    }

    #[test]
    fn cursor_wraps_both_ways() {
        let mut host = Host::new(&[Some(3)]);
        host.move_cursor(-1);
        assert_eq!(host.cursor_at(0), 2);
        host.move_cursor(1);
        assert_eq!(host.cursor_at(0), 0);
    }

    #[test]
    fn custom_page_participates_in_navigation_but_has_no_cursor() {
        let mut host = Host::new(&[Some(2), None]);
        assert_eq!(host.page_kind(1), PageKind::Custom);
        host.move_page(1); // lands on the custom page
        assert_eq!(host.current_page(), 1);
        host.move_cursor(1); // no-op: no cursor rows
        host.move_cursor(-1);
        assert_eq!(host.cursor_at(1), 0);
        assert_eq!(host.commit_current(), None, "custom pages never commit");
        host.move_page(1); // wraps back to the first page
        assert_eq!(host.current_page(), 0);
    }

    #[test]
    fn empty_options_page_is_navigable_but_has_no_cursor() {
        let mut host = Host::new(&[Some(0)]);
        host.move_cursor(1);
        assert_eq!(host.cursor_at(0), 0);
        assert_eq!(host.commit_current(), None);
    }

    // ── Commit capture ──────────────────────────────────────────

    #[test]
    fn commit_capture_is_a_snapshot_not_a_derivation() {
        let mut host = Host::new(&[Some(3)]);
        host.set_cursor_at(0, 1);
        assert_eq!(host.commit_current(), Some(1));
        host.set_cursor_at(0, 0);
        assert_eq!(
            host.committed_at(0),
            Some(1),
            "moving the cursor after commit must not change the captured row"
        );
    }

    #[test]
    fn recommit_overrides_previous_capture() {
        let mut host = Host::new(&[Some(3)]);
        host.set_cursor_at(0, 2);
        host.commit_current();
        host.set_cursor_at(0, 0);
        assert_eq!(host.commit_current(), Some(0));
        assert_eq!(host.committed_at(0), Some(0));
    }

    #[test]
    fn commits_are_per_page() {
        let mut host = Host::new(&[Some(2), Some(2)]);
        host.commit_current(); // page A row 0
        host.move_page(1);
        host.set_cursor_at(1, 1);
        host.commit_current(); // page B row 1
        host.move_page(-1);
        assert_eq!(host.committed_at(0), Some(0));
        assert_eq!(host.committed_at(1), Some(1));
    }

    // ── Refresh fallback ────────────────────────────────────────

    #[test]
    fn refresh_keeps_valid_page_and_cursor() {
        let mut host = Host::new(&[Some(4), Some(3)]);
        host.current = 1;
        host.set_cursor_at(1, 2);
        host.set_committed_at(1, Some(2));
        host.clamp_after_refresh();
        assert_eq!(host.current_page(), 1, "existing page is kept");
        assert_eq!(host.cursor_at(1), 2, "valid cursor is kept");
        assert_eq!(host.committed_at(1), Some(2), "valid capture is kept");
    }

    #[test]
    fn refresh_falls_back_to_first_page_when_current_vanishes() {
        let mut host = Host::new(&[Some(2), Some(2), Some(2)]);
        host.current = 2;
        host.pages.truncate(1);
        host.cursors.truncate(1);
        host.committed.truncate(1);
        host.clamp_after_refresh();
        assert_eq!(
            host.current_page(),
            0,
            "missing page falls back to the first"
        );
    }

    #[test]
    fn refresh_clamps_cursor_and_clears_out_of_range_capture() {
        let mut host = Host::new(&[Some(6)]);
        host.set_cursor_at(0, 5);
        host.set_committed_at(0, Some(5));
        host.pages[0] = PageKind::Options { rows: 3 };
        host.clamp_after_refresh();
        assert_eq!(host.cursor_at(0), 2, "cursor clamps to the last row");
        assert_eq!(host.committed_at(0), None, "vanished capture is cleared");
    }

    #[test]
    fn refresh_on_empty_page_resets_cursor() {
        let mut host = Host::new(&[Some(4)]);
        host.set_cursor_at(0, 3);
        host.pages[0] = PageKind::Options { rows: 0 };
        host.clamp_after_refresh();
        assert_eq!(host.cursor_at(0), 0);
    }

    // ── Window math ─────────────────────────────────────────────

    #[test]
    fn window_covers_short_lists_entirely() {
        assert_eq!(window_range(0, 0, 5), 0..0);
        assert_eq!(window_range(2, 5, 5), 0..5, "len == size → no sliding");
        assert_eq!(window_range(1, 3, 5), 0..3);
        assert_eq!(window_range(9, 3, 5), 0..3, "stale cursor is clamped");
    }

    #[test]
    fn window_slides_when_cursor_crosses_bottom_edge() {
        // 8 items, window 5: cursor on 5 (the 6th) → window 1..6, cursor on
        // the last visible slot; the left side is hidden.
        assert_eq!(window_range(4, 8, 5), 0..5);
        assert_eq!(window_range(5, 8, 5), 1..6);
        assert_eq!(window_range(7, 8, 5), 3..8, "pinned at the end");
    }

    #[test]
    fn window_is_recomputed_per_page() {
        // Switching to a page with a different cursor/length recomputes.
        assert_eq!(window_range(0, 3, 5), 0..3);
        assert_eq!(window_range(5, 8, 5), 1..6);
        assert_eq!(window_range(1, 2, 5), 0..2);
    }

    #[test]
    fn wrap_index_edge_cases() {
        assert_eq!(wrap_index(0, 1, 3), 1);
        assert_eq!(wrap_index(2, 1, 3), 0);
        assert_eq!(wrap_index(0, -1, 3), 2);
        assert_eq!(wrap_index(0, 0, 0), 0);
    }
}
