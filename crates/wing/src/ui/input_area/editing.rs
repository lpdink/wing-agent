//! Core editing operations for InputArea.

use super::InputArea;
use super::helpers::{char_to_byte, is_placeholder_line};

impl InputArea {
    /// Insert a character at the current cursor position.
    /// Ignored if current line is a placeholder.
    pub(crate) fn insert_char(&mut self, c: char) {
        if is_placeholder_line(&self.lines[self.cursor_row]) {
            return;
        }
        self.clear_desired_col();
        let byte_col = char_to_byte(&self.lines[self.cursor_row], self.cursor_col);
        self.lines[self.cursor_row].insert(byte_col, c);
        self.cursor_col += 1;
    }

    /// Insert a newline at the current cursor position, splitting the line.
    /// Does nothing if we're already at `MAX_INPUT_LINES`.
    /// On placeholder lines, inserts a new empty line after instead of splitting.
    pub(crate) fn insert_newline(&mut self) {
        if self.lines.len() >= self.max_lines {
            return;
        }
        self.clear_desired_col();
        if is_placeholder_line(&self.lines[self.cursor_row]) {
            // Don't split placeholder — insert empty line after it.
            self.lines.insert(self.cursor_row + 1, String::new());
            self.cursor_row += 1;
            self.cursor_col = 0;
            return;
        }
        let byte_col = char_to_byte(&self.lines[self.cursor_row], self.cursor_col);
        let rest = self.lines[self.cursor_row][byte_col..].to_string();
        self.lines[self.cursor_row].truncate(byte_col);
        self.lines.insert(self.cursor_row + 1, rest);
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    /// Delete one character before the cursor (backspace).
    /// At column 0, merges current line with the previous line.
    /// If the previous line is a placeholder, removes it entirely.
    pub(crate) fn backspace(&mut self) {
        self.clear_desired_col();
        if self.cursor_col > 0 {
            // Delete char within the line.
            let line = &mut self.lines[self.cursor_row];
            let byte_col = char_to_byte(line, self.cursor_col);
            let prev_byte = line[..byte_col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            line.drain(prev_byte..byte_col);
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            // If previous line is a placeholder, remove it entirely.
            if is_placeholder_line(&self.lines[self.cursor_row - 1]) {
                self.lines.remove(self.cursor_row - 1);
                self.cursor_row -= 1;
                self.cleanup_pending_pastes();
                return;
            }
            // Merge with previous line.
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current);
        }
    }

    /// Delete one character at the cursor (forward delete).
    /// At end of line, merges next line into current line.
    /// If the next line is a placeholder, removes it entirely.
    pub(crate) fn delete_forward(&mut self) {
        self.clear_desired_col();
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            // Delete char within the line.
            let line = &mut self.lines[self.cursor_row];
            let byte_col = char_to_byte(line, self.cursor_col);
            let next_byte = line[byte_col..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| byte_col + i)
                .unwrap_or(line.len());
            line.drain(byte_col..next_byte);
        } else if self.cursor_row < self.last_row() {
            // If next line is a placeholder, remove it entirely.
            if is_placeholder_line(&self.lines[self.cursor_row + 1]) {
                self.lines.remove(self.cursor_row + 1);
                self.cleanup_pending_pastes();
                return;
            }
            // Merge next line into current.
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
        }
    }
}
