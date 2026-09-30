//! Cursor movement operations for InputArea.

use super::InputArea;
use super::chrome::Chrome;
use super::model;
use super::wrap;

impl InputArea {
    /// Step left: one character, or over a whole chip (a chip is one unit —
    /// the cursor never lands inside one).
    pub(crate) fn move_left(&mut self) {
        self.clear_desired_col();
        if self.cursor_col > 0 {
            self.cursor_col = model::unit_start_before(
                &self.lines[self.cursor_row],
                &self.pastes,
                self.cursor_col,
            );
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
        }
    }

    /// Step right: one character, or over a whole chip.
    pub(crate) fn move_right(&mut self) {
        self.clear_desired_col();
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            self.cursor_col =
                model::unit_end_at(&self.lines[self.cursor_row], &self.pastes, self.cursor_col);
        } else if self.cursor_row < self.last_row() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    /// Build visual rows for movement computation (the rendered chrome).
    fn visual_rows(&self, chrome: Chrome) -> Vec<wrap::VisualRow> {
        let text_width = chrome.text_width as usize;
        wrap::build_visual_rows(&self.lines, &self.pastes, text_width.max(1))
    }

    /// The column ↑/↓ should aim at: the sticky one if the previous move set
    /// it, otherwise the cursor's own *display* column inside its visual row
    /// (a char index is not a cell — wide characters are two cells wide).
    fn desired_column(&self, vis_rows: &[wrap::VisualRow]) -> usize {
        if let Some(desired) = self.desired_col {
            return desired;
        }
        let (vis_row, _) = wrap::logical_to_visual(vis_rows, self.cursor_row, self.cursor_col);
        let flat = model::flat(&self.lines[self.cursor_row], &self.pastes);
        wrap::char_display_offset(
            &flat,
            &vis_rows[vis_row.min(vis_rows.len() - 1)],
            self.cursor_col,
        )
    }

    /// Navigate to a target visual row, resolving the desired column back to a
    /// logical position. A column that lands on a chip resolves to the chip's
    /// leading edge, like the pointer's hit test.
    fn move_to_visual_row(&mut self, vis_rows: &[wrap::VisualRow], target: usize, desired: usize) {
        let row = vis_rows[target];
        let point = model::column_point(&self.lines[row.logical_line], &self.pastes, &row, desired);
        self.cursor_row = row.logical_line;
        self.cursor_col = point.point.min(self.current_line_len());
        self.desired_col = Some(desired);
    }

    pub(crate) fn move_up(&mut self, chrome: Chrome) {
        let vis_rows = self.visual_rows(chrome);
        let (vis_row, _) = wrap::logical_to_visual(&vis_rows, self.cursor_row, self.cursor_col);
        let desired = self.desired_column(&vis_rows);

        if vis_row == 0 {
            return; // At top: no-op.
        }

        self.move_to_visual_row(&vis_rows, vis_row - 1, desired);
    }

    pub(crate) fn move_down(&mut self, chrome: Chrome) {
        let vis_rows = self.visual_rows(chrome);
        let (vis_row, _) = wrap::logical_to_visual(&vis_rows, self.cursor_row, self.cursor_col);
        let desired = self.desired_column(&vis_rows);

        if vis_row + 1 >= vis_rows.len() {
            return; // At bottom: no-op.
        }

        self.move_to_visual_row(&vis_rows, vis_row + 1, desired);
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
