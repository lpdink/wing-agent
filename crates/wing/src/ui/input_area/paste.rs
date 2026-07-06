//! Paste operations for InputArea.

use super::InputArea;
use super::helpers::{char_to_byte, is_placeholder_line};

impl InputArea {
    /// Insert a string at cursor position (used for paste).
    ///
    /// Large pastes (>2 lines or >200 chars) are replaced with a placeholder.
    /// Multi-line paste preserves line structure for small pastes.
    /// Pasted content is truncated to respect `MAX_INPUT_LINES`.
    pub fn insert_str(&mut self, s: &str) {
        // Sanitize: strip \r, skip pure whitespace.
        let cleaned: String = s.chars().filter(|&c| c != '\r').collect();
        let cleaned = cleaned.trim_end();
        if cleaned.trim().is_empty() {
            return;
        }

        let line_count = cleaned.split('\n').count();
        let char_count = cleaned.chars().count();

        // Trigger placeholder for large pastes.
        if line_count > 2 || char_count > 200 {
            self.insert_paste_placeholder(cleaned.to_string());
            return;
        }

        let mut parts: Vec<&str> = cleaned.split('\n').collect();

        // Truncate multi-line paste to respect MAX_INPUT_LINES.
        let available_new_lines = self.max_lines.saturating_sub(self.lines.len());
        if parts.len() > 1 && parts.len() - 1 > available_new_lines {
            parts.truncate(available_new_lines + 1);
        }

        if parts.len() == 1 {
            // Single-line paste: insert into current line.
            let byte_col = char_to_byte(&self.lines[self.cursor_row], self.cursor_col);
            self.lines[self.cursor_row].insert_str(byte_col, parts[0]);
            self.cursor_col += parts[0].chars().count();
        } else {
            // Multi-line paste: split current line and splice in the new lines.
            let byte_col = char_to_byte(&self.lines[self.cursor_row], self.cursor_col);
            let after = self.lines[self.cursor_row][byte_col..].to_string();
            self.lines[self.cursor_row].truncate(byte_col);

            // Append first part to current line.
            self.lines[self.cursor_row].push_str(parts[0]);

            // Insert middle lines as new lines.
            for (i, part) in parts[1..].iter().enumerate() {
                let insert_idx = self.cursor_row + 1 + i;
                if insert_idx == self.cursor_row + parts.len() - 1 {
                    // Last part: prepend the "after" text.
                    let mut new_line = String::from(*part);
                    new_line.push_str(&after);
                    self.lines.insert(insert_idx, new_line);
                } else {
                    self.lines.insert(insert_idx, String::from(*part));
                }
            }

            // Move cursor to end of last inserted line (before "after" text).
            let last_insert_row = self.cursor_row + parts.len() - 1;
            let last_part = parts.last().unwrap();
            self.cursor_row = last_insert_row;
            self.cursor_col = last_part.chars().count();
        }

        self.clear_desired_col();
    }

    /// Insert a paste placeholder for large pastes.
    pub(crate) fn insert_paste_placeholder(&mut self, text: String) {
        // Reject if input is full.
        if self.lines.len() >= self.max_lines {
            return;
        }

        self.paste_counter += 1;
        let line_count = text.split('\n').count();
        let extra_lines = line_count.saturating_sub(1);
        let placeholder = format!(
            "[Pasted text #{} +{} lines]",
            self.paste_counter, extra_lines
        );

        // If current line is non-empty, insert on a new line.
        if !self.lines[self.cursor_row].is_empty() {
            self.lines.insert(self.cursor_row + 1, placeholder.clone());
            self.cursor_row += 1;
        } else {
            self.lines[self.cursor_row] = placeholder.clone();
        }

        // Move cursor to an editable line after the placeholder.
        let next_row = self.cursor_row + 1;
        if next_row >= self.lines.len() && self.lines.len() < self.max_lines {
            self.lines.push(String::new());
        }
        if next_row < self.lines.len() {
            self.cursor_row = next_row;
        }
        self.cursor_col = 0;

        self.pending_pastes.push((placeholder, text));
        self.clear_desired_col();
    }

    /// Return full text with placeholders expanded to original paste content.
    pub(crate) fn expand_and_get_text(&self) -> String {
        if self.pending_pastes.is_empty() {
            return self.text();
        }
        let mut result = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                result.push('\n');
            }
            if let Some(actual) = self.find_pending_paste(line) {
                result.push_str(actual);
            } else {
                result.push_str(line);
            }
        }
        result
    }

    fn find_pending_paste<'a>(&'a self, line: &str) -> Option<&'a str> {
        if !is_placeholder_line(line) {
            return None;
        }
        self.pending_pastes
            .iter()
            .find(|(placeholder, _)| placeholder == line)
            .map(|(_, actual)| actual.as_str())
    }

    /// Remove pending_pastes entries whose placeholder is no longer in lines.
    pub(crate) fn cleanup_pending_pastes(&mut self) {
        self.pending_pastes
            .retain(|(placeholder, _)| self.lines.iter().any(|line| line == placeholder));
    }
}
