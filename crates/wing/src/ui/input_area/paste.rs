//! Paste operations for InputArea.

use super::InputArea;
use super::model;

impl InputArea {
    /// Insert a string at cursor position (used for paste).
    ///
    /// Large pastes (>2 lines or >200 chars) leave a *chip* standing for them
    /// — inserted at the cursor like any other text, so the draft keeps its
    /// line structure and the user keeps typing where they were.
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

        // Trigger a chip for large pastes.
        if line_count > 2 || char_count > 200 {
            self.insert_paste(cleaned.to_string());
            return;
        }

        let mut parts: Vec<&str> = cleaned.split('\n').collect();

        // Truncate multi-line paste to respect MAX_INPUT_LINES.
        let available_new_lines = self.max_lines.saturating_sub(self.lines.len());
        if parts.len() > 1 && parts.len() - 1 > available_new_lines {
            parts.truncate(available_new_lines + 1);
        }

        if parts.len() == 1 {
            // Single-line paste: insert into the current line.
            model::insert_text(
                &mut self.lines[self.cursor_row],
                &self.pastes,
                self.cursor_col,
                parts[0],
            );
            self.cursor_col += parts[0].chars().count();
        } else {
            // Multi-line paste: split the current line and splice the parts
            // in; the text that followed the cursor closes the last one.
            let tail = model::split_off(
                &mut self.lines[self.cursor_row],
                &self.pastes,
                self.cursor_col,
            );
            model::insert_text(
                &mut self.lines[self.cursor_row],
                &self.pastes,
                self.cursor_col,
                parts[0],
            );

            let mut new_lines: Vec<model::Line> = parts[1..]
                .iter()
                .map(|part| model::text_line(part))
                .collect();
            if let Some(last) = new_lines.last_mut() {
                last.extend(tail);
                model::coalesce(last);
            }
            for (i, line) in new_lines.into_iter().enumerate() {
                self.lines.insert(self.cursor_row + 1 + i, line);
            }

            // Move cursor to end of last inserted line (before the tail text).
            self.cursor_row += parts.len() - 1;
            self.cursor_col = parts.last().unwrap().chars().count();
        }

        self.clear_desired_col();
    }

    /// Leave a chip in place of `text` (a large paste), at the cursor.
    ///
    /// The chip is *inline*: whatever follows the cursor stays on the same
    /// line and the cursor lands right after the chip, exactly as if the text
    /// had been inserted — the paste never restructures the draft. The payload
    /// is expanded again on submit ([`Self::expand_and_get_text`]); until then
    /// the chip is one atomic unit (see [`model`]).
    pub(crate) fn insert_paste(&mut self, text: String) {
        let number = self.pastes.add(text);
        let width = self.pastes.chip(number).chars().count();
        model::insert_chip(
            &mut self.lines[self.cursor_row],
            &self.pastes,
            self.cursor_col,
            number,
        );
        self.cursor_col += width;
        self.clear_desired_col();
    }

    /// Return full text with chips expanded to their original paste content.
    pub(crate) fn expand_and_get_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| model::expanded(line, &self.pastes))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
