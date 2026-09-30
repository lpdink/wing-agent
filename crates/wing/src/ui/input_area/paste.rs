//! Paste operations for InputArea.

use super::InputArea;
use super::model;

impl InputArea {
    /// Insert a string at cursor position (used for paste).
    ///
    /// Large pastes (>2 lines or >200 chars) leave a *chip* standing for them
    /// — inserted at the cursor like any other text, so the draft keeps its
    /// line structure and the user keeps typing where they were.
    /// A paste that does not fit the draft's line budget is chipped too: a
    /// chip costs no line, so a paste that cannot be spliced in whole is kept
    /// whole instead of being cut off.
    /// Multi-line paste preserves line structure for small pastes.
    pub fn insert_str(&mut self, s: &str) {
        // Sanitize: strip \r, skip pure whitespace.
        let cleaned: String = s.chars().filter(|&c| c != '\r').collect();
        let cleaned = cleaned.trim_end();
        if cleaned.trim().is_empty() {
            return;
        }

        let parts: Vec<&str> = cleaned.split('\n').collect();
        let char_count = cleaned.chars().count();

        // Leave a chip for large pastes — and for ones the draft has no room
        // for.
        let fits = parts.len() - 1 <= self.max_lines.saturating_sub(self.lines.len());
        if parts.len() > 2 || char_count > 200 || !fits {
            self.insert_paste(cleaned.to_string());
            return;
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
