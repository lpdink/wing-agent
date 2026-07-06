//! Cursor movement operations for InputArea.

use super::InputArea;
use super::helpers::PREFIX_WIDTH;
use super::helpers::is_placeholder_line;
use super::wrap;

/// Direction for placeholder skip search.
enum Direction {
    Up,
    Down,
}

/// Starting from `target_vis`, search in `direction` for the nearest visual row
/// whose logical line is NOT a placeholder.  Returns `None` if all rows in that
/// direction are placeholders.
fn find_non_placeholder_vis(
    vis_rows: &[wrap::VisualRow],
    lines: &[String],
    target_vis: usize,
    dir: Direction,
) -> Option<usize> {
    match dir {
        Direction::Up => {
            let mut vi = target_vis;
            loop {
                if vi == 0 {
                    return None;
                }
                vi -= 1;
                if !is_placeholder_line(&lines[vis_rows[vi].logical_line]) {
                    return Some(vi);
                }
            }
        }
        Direction::Down => {
            let mut vi = target_vis;
            while vi + 1 < vis_rows.len() {
                vi += 1;
                if !is_placeholder_line(&lines[vis_rows[vi].logical_line]) {
                    return Some(vi);
                }
            }
            None
        }
    }
}

impl InputArea {
    pub(crate) fn move_left(&mut self) {
        self.clear_desired_col();
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            let orig_row = self.cursor_row;
            self.cursor_row -= 1;
            // Skip over all consecutive placeholder lines.
            while self.cursor_row > 0 && is_placeholder_line(&self.lines[self.cursor_row]) {
                self.cursor_row -= 1;
            }
            if is_placeholder_line(&self.lines[self.cursor_row]) {
                // All lines above are placeholders — can't move.
                self.cursor_row = orig_row;
                self.cursor_col = 0;
            } else {
                self.cursor_col = self.current_line_len();
            }
        }
    }

    pub(crate) fn move_right(&mut self) {
        self.clear_desired_col();
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_row < self.last_row() {
            let orig_row = self.cursor_row;
            self.cursor_row += 1;
            // Skip over all consecutive placeholder lines.
            while self.cursor_row < self.last_row()
                && is_placeholder_line(&self.lines[self.cursor_row])
            {
                self.cursor_row += 1;
            }
            if is_placeholder_line(&self.lines[self.cursor_row]) {
                // All lines below are placeholders — can't move.
                self.cursor_row = orig_row;
                self.cursor_col = self.current_line_len();
            } else {
                self.cursor_col = 0;
            }
        }
    }

    /// Build visual rows for movement computation.
    fn visual_rows(&self, available_width: u16) -> Vec<wrap::VisualRow> {
        let text_width = available_width.saturating_sub(PREFIX_WIDTH) as usize;
        wrap::build_visual_rows(&self.lines, text_width.max(1))
    }

    /// Navigate to a target visual row, resolving the desired visual column
    /// back to a logical position.  Handles placeholder skipping.
    fn move_to_visual_row(
        &mut self,
        vis_rows: &[wrap::VisualRow],
        target_vis: usize,
        desired: usize,
        dir: Direction,
    ) {
        // If the target visual row belongs to a placeholder line, search further.
        let resolved = if is_placeholder_line(&self.lines[vis_rows[target_vis].logical_line]) {
            find_non_placeholder_vis(vis_rows, &self.lines, target_vis, dir)
        } else {
            Some(target_vis)
        };

        if let Some(vi) = resolved {
            let (log_row, log_col) = wrap::visual_to_logical(vis_rows, vi, desired);
            self.cursor_row = log_row;
            self.cursor_col = log_col.min(self.lines[log_row].chars().count());
            self.desired_col = Some(desired);
        }
        // None → all rows in that direction are placeholders — no-op.
    }

    pub(crate) fn move_up(&mut self, available_width: u16) {
        let vis_rows = self.visual_rows(available_width);
        let (vis_row, vis_col) =
            wrap::logical_to_visual(&vis_rows, self.cursor_row, self.cursor_col);
        let desired = self.desired_col.unwrap_or(vis_col);

        if vis_row == 0 {
            return; // At top: no-op.
        }

        self.move_to_visual_row(&vis_rows, vis_row - 1, desired, Direction::Up);
    }

    pub(crate) fn move_down(&mut self, available_width: u16) {
        let vis_rows = self.visual_rows(available_width);
        let (vis_row, vis_col) =
            wrap::logical_to_visual(&vis_rows, self.cursor_row, self.cursor_col);
        let desired = self.desired_col.unwrap_or(vis_col);

        if vis_row + 1 >= vis_rows.len() {
            return; // At bottom: no-op.
        }

        self.move_to_visual_row(&vis_rows, vis_row + 1, desired, Direction::Down);
    }

    pub(crate) fn move_home(&mut self) {
        self.clear_desired_col();
        self.cursor_col = 0;
    }

    pub(crate) fn move_end(&mut self) {
        self.clear_desired_col();
        self.cursor_col = self.current_line_len();
    }
}
