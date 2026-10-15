//! Core editing operations for InputArea.

use super::InputArea;
use super::model;

impl InputArea {
    /// Insert a character at the current cursor position.
    pub(crate) fn insert_char(&mut self, c: char) {
        self.clear_desired_col();
        model::insert_text(
            &mut self.lines[self.cursor_row],
            &self.pastes,
            self.cursor_col,
            &c.to_string(),
        );
        self.cursor_col += 1;
    }

    /// Insert a newline at the current cursor position, splitting the line.
    /// Does nothing if we're already at `MAX_INPUT_LINES`.
    /// A chip is never split: a newline at either of its edges stays there.
    pub(crate) fn insert_newline(&mut self) {
        if self.lines.len() >= self.max_lines {
            return;
        }
        self.clear_desired_col();
        let tail = model::split_off(
            &mut self.lines[self.cursor_row],
            &self.pastes,
            self.cursor_col,
        );
        self.lines.insert(self.cursor_row + 1, tail);
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    /// Delete one unit before the cursor (backspace).
    ///
    /// A unit is one character — or a whole chip: the cursor never rests
    /// inside one, so Backspace at its trailing edge removes the paste, not a
    /// character of its label.
    /// At column 0, merges the current line with the previous one.
    pub(crate) fn backspace(&mut self) {
        self.clear_desired_col();
        if self.cursor_col > 0 {
            let removed = model::remove_unit_before(
                &mut self.lines[self.cursor_row],
                &mut self.pastes,
                self.cursor_col,
            );
            self.cursor_col -= removed.min(self.cursor_col);
        } else if self.cursor_row > 0 {
            // Merge with previous line.
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            let joined = model::flat_len(&self.lines[self.cursor_row], &self.pastes);
            self.lines[self.cursor_row].extend(current);
            model::coalesce(&mut self.lines[self.cursor_row]);
            self.cursor_col = joined;
        }
    }

    /// Delete one unit at the cursor (forward delete).
    ///
    /// A unit is one character — or a whole chip (see [`Self::backspace`]).
    /// At end of line, merges the next line into the current one.
    pub(crate) fn delete_forward(&mut self) {
        self.clear_desired_col();
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            model::remove_unit_at(
                &mut self.lines[self.cursor_row],
                &mut self.pastes,
                self.cursor_col,
            );
        } else if self.cursor_row < self.last_row() {
            // Merge next line into current.
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].extend(next);
            model::coalesce(&mut self.lines[self.cursor_row]);
        }
    }
}
