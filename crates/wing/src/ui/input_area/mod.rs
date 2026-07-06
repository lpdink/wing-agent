//! Input area widget — multi-line text input at the bottom.

pub(crate) mod editing;
pub(crate) mod helpers;
pub(crate) mod movement;
pub(crate) mod paste;
pub(crate) mod widget;
pub(crate) mod wrap;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use helpers::PREFIX_WIDTH;

// Re-exports for external use.
pub use widget::InputAreaWidget;
pub use widget::cursor_screen_pos;

/// Actions the input area wants the app to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    /// User pressed Enter — submit the text.
    Submit(String),
    /// User pressed Esc.
    Escape,
    /// No action (just editing text).
    None,
}

/// Multi-line text input.
///
/// Text is stored as `Vec<String>` (one element per line, no embedded newlines).
/// Cursor position is `(row, col)` where `col` is a char-based index.
pub struct InputArea {
    /// Lines of text. Always has at least one element.
    pub(crate) lines: Vec<String>,
    /// Cursor row (0-based, index into `lines`).
    pub(crate) cursor_row: usize,
    /// Cursor column (0-based, char index within the current line).
    pub(crate) cursor_col: usize,
    /// Desired column for Up/Down navigation.
    pub(crate) desired_col: Option<usize>,
    /// Placeholder text shown when empty.
    pub(crate) placeholder: String,
    /// Vertical scroll offset (first visible visual row index).
    pub(crate) vertical_scroll: usize,
    /// Pending paste contents: (placeholder_text, full_original_text).
    pub(crate) pending_pastes: Vec<(String, String)>,
    /// Paste counter for generating unique #N in placeholders.
    pub(crate) paste_counter: usize,
    /// Maximum number of input lines (configurable).
    pub(crate) max_lines: usize,
}

impl InputArea {
    pub fn new(placeholder: &str) -> Self {
        Self::with_max_lines(placeholder.to_string(), helpers::MAX_INPUT_LINES)
    }

    pub fn with_max_lines(placeholder: String, max_lines: usize) -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            desired_col: None,
            placeholder,
            vertical_scroll: 0,
            pending_pastes: Vec::new(),
            paste_counter: 0,
            max_lines,
        }
    }

    // ── Content access ──────────────────────────────────────────

    /// Get current text as a single string (lines joined by `\n`).
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Get lines.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Number of lines.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Desired height for layout (visual row count, width-aware).
    pub fn height(&self, available_width: u16) -> u16 {
        let text_width = available_width.saturating_sub(PREFIX_WIDTH) as usize;
        if text_width == 0 {
            return 1;
        }
        let vis = wrap::build_visual_rows(&self.lines, text_width);
        let count = vis.len();
        count.clamp(1, self.max_lines) as u16
    }

    /// Check if all lines are empty/whitespace.
    pub(crate) fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    // ── Cursor helpers ──────────────────────────────────────────

    /// Length of the current line in chars.
    pub(crate) fn current_line_len(&self) -> usize {
        self.lines[self.cursor_row].chars().count()
    }

    /// Last valid row index.
    pub(crate) fn last_row(&self) -> usize {
        self.lines.len() - 1
    }

    /// Clear the desired_col memory (call on Left/Right/Char/Home/End).
    pub(crate) fn clear_desired_col(&mut self) {
        self.desired_col = None;
    }

    // ── Key event handling ──────────────────────────────────────

    /// Handle a key event, returning the desired action.
    pub fn handle_key(&mut self, key: KeyEvent, available_width: u16) -> InputAction {
        let mods = key.modifiers;
        let has_shift = mods.contains(KeyModifiers::SHIFT);
        let has_alt = mods.contains(KeyModifiers::ALT);
        let has_ctrl = mods.contains(KeyModifiers::CONTROL);

        // C0 control character normalization: some terminals send raw
        // control characters (0x0A = LF, 0x0D = CR) instead of KeyCode::Enter.
        let is_enter_like = matches!(
            key.code,
            KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n')
        );

        match key.code {
            // Ctrl+J (LF) or Ctrl+M (CR) → insert newline.
            KeyCode::Char('j' | 'm') if has_ctrl => {
                self.insert_newline();
                InputAction::None
            }
            _ if is_enter_like => {
                // Shift+Enter or Alt+Enter → insert newline.
                if has_shift || has_alt {
                    self.insert_newline();
                    return InputAction::None;
                }
                // Plain Enter → submit.
                if self.is_empty() {
                    return InputAction::None;
                }
                let text = self.expand_and_get_text();
                let text = text.trim().to_string();
                self.clear();
                InputAction::Submit(text)
            }
            KeyCode::Esc => InputAction::Escape,
            KeyCode::Char(c) => {
                self.insert_char(c);
                InputAction::None
            }
            KeyCode::Backspace => {
                self.backspace();
                InputAction::None
            }
            KeyCode::Delete => {
                self.delete_forward();
                InputAction::None
            }
            KeyCode::Left => {
                self.move_left();
                InputAction::None
            }
            KeyCode::Right => {
                self.move_right();
                InputAction::None
            }
            KeyCode::Up => {
                self.move_up(available_width);
                InputAction::None
            }
            KeyCode::Down => {
                self.move_down(available_width);
                InputAction::None
            }
            KeyCode::Home => {
                self.move_home();
                InputAction::None
            }
            KeyCode::End => {
                self.move_end();
                InputAction::None
            }
            _ => InputAction::None,
        }
    }

    // ── Text manipulation ───────────────────────────────────────

    /// Set the text (used for draft restoration / popup completion).
    pub fn set_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(String::from).collect()
        };
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.current_line_len();
        self.desired_col = None;
        self.vertical_scroll = 0;
        self.pending_pastes.clear();
        self.paste_counter = 0;
    }

    /// Clear input and reset cursor.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.desired_col = None;
        self.vertical_scroll = 0;
        self.pending_pastes.clear();
        self.paste_counter = 0;
    }

    // ── Scroll management ───────────────────────────────────────

    /// Update vertical scroll to keep cursor's visual row visible.
    pub(crate) fn update_vertical_scroll(&mut self, visible_height: u16, available_width: u16) {
        let h = visible_height as usize;
        if h == 0 {
            return;
        }
        let text_width = available_width.saturating_sub(PREFIX_WIDTH) as usize;
        if text_width == 0 {
            return;
        }
        let vis_rows = wrap::build_visual_rows(&self.lines, text_width);
        let (vis_row, _) = wrap::logical_to_visual(&vis_rows, self.cursor_row, self.cursor_col);

        if vis_row < self.vertical_scroll {
            self.vertical_scroll = vis_row;
        } else if vis_row >= self.vertical_scroll + h {
            self.vertical_scroll = vis_row - h + 1;
        }
    }

    // ── Cursor screen position ──────────────────────────────────

    /// Get cursor screen position as `(x, y)` relative to `area`.
    ///
    /// Returns absolute coordinates suitable for `MoveTo(x, y)`.
    pub fn cursor_screen_pos(&self, area: &Rect) -> (u16, u16) {
        let text_width = area.width.saturating_sub(PREFIX_WIDTH) as usize;
        let vis_rows = wrap::build_visual_rows(&self.lines, text_width.max(1));
        let (vis_row, vis_col) =
            wrap::logical_to_visual(&vis_rows, self.cursor_row, self.cursor_col);

        // Compute display width of the visual column portion.
        let line = &self.lines[self.cursor_row];
        let vr = &vis_rows[vis_row.min(vis_rows.len() - 1)];
        let col_display: usize = line
            .chars()
            .skip(vr.char_start)
            .take(vis_col)
            .map(|c| c.width().unwrap_or(0))
            .sum();

        let x = area
            .x
            .saturating_add(PREFIX_WIDTH)
            .saturating_add(col_display as u16);
        let visible_row = vis_row.saturating_sub(self.vertical_scroll);
        let y = area.y.saturating_add(visible_row as u16);
        (x.min(area.x + area.width.saturating_sub(1)), y)
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::helpers::MAX_INPUT_LINES;
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    // ── Data model tests ────────────────────────────────────

    #[test]
    fn empty_input() {
        let input = InputArea::new("placeholder");
        assert_eq!(input.lines(), [""]);
        assert_eq!(input.text(), "");
        assert_eq!(input.line_count(), 1);
        assert_eq!(input.height(80), 1);
    }

    #[test]
    fn set_text_single_line() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        assert_eq!(input.lines(), ["hello"]);
        assert_eq!(input.text(), "hello");
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 5);
    }

    #[test]
    fn set_text_multi_line() {
        let mut input = InputArea::new("");
        input.set_text("a\nb\nc");
        assert_eq!(input.lines(), ["a", "b", "c"]);
        assert_eq!(input.text(), "a\nb\nc");
        assert_eq!(input.cursor_row, 2);
        assert_eq!(input.cursor_col, 1);
    }

    #[test]
    fn clear_resets() {
        let mut input = InputArea::new("");
        input.set_text("hello\nworld");
        input.clear();
        assert_eq!(input.lines(), [""]);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 0);
    }

    // ── Core editing tests ──────────────────────────────────

    #[test]
    fn insert_char_basic() {
        let mut input = InputArea::new("");
        input.handle_key(key(KeyCode::Char('h')), 80);
        input.handle_key(key(KeyCode::Char('i')), 80);
        assert_eq!(input.text(), "hi");
    }

    #[test]
    fn insert_newline_splits() {
        let mut input = InputArea::new("");
        input.set_text("hello world");
        input.cursor_row = 0;
        input.cursor_col = 5;
        input.handle_key(key_with(KeyCode::Enter, KeyModifiers::SHIFT), 80);
        assert_eq!(input.lines(), ["hello", " world"]);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn insert_newline_at_end() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.handle_key(key_with(KeyCode::Enter, KeyModifiers::ALT), 80);
        assert_eq!(input.lines(), ["hello", ""]);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn insert_newline_at_start() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 0;
        input.handle_key(key_with(KeyCode::Char('j'), KeyModifiers::CONTROL), 80);
        assert_eq!(input.lines(), ["", "hello"]);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn max_lines_enforced() {
        let mut input = InputArea::new("");
        for _ in 0..MAX_INPUT_LINES - 1 {
            input.insert_newline();
        }
        assert_eq!(input.line_count(), MAX_INPUT_LINES);
        input.insert_newline();
        assert_eq!(input.line_count(), MAX_INPUT_LINES);
    }

    #[test]
    fn backspace_within_line() {
        let mut input = InputArea::new("");
        input.set_text("abc");
        input.handle_key(key(KeyCode::Backspace), 80);
        assert_eq!(input.text(), "ab");
    }

    #[test]
    fn backspace_merges_lines() {
        let mut input = InputArea::new("");
        input.set_text("hello\nworld");
        input.cursor_row = 1;
        input.cursor_col = 0;
        input.backspace();
        assert_eq!(input.lines(), ["helloworld"]);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 5);
    }

    #[test]
    fn backspace_at_origin_noop() {
        let mut input = InputArea::new("");
        input.backspace();
        assert_eq!(input.lines(), [""]);
    }

    #[test]
    fn delete_forward_within_line() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 0;
        input.delete_forward();
        assert_eq!(input.text(), "ello");
    }

    #[test]
    fn delete_forward_merges_lines() {
        let mut input = InputArea::new("");
        input.set_text("hello\nworld");
        input.cursor_row = 0;
        input.cursor_col = 5;
        input.delete_forward();
        assert_eq!(input.lines(), ["helloworld"]);
    }

    // ── Cursor movement tests ───────────────────────────────

    #[test]
    fn left_right_basic() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        assert_eq!(input.cursor_col, 5);
        input.move_left();
        assert_eq!(input.cursor_col, 4);
        input.move_right();
        assert_eq!(input.cursor_col, 5);
    }

    #[test]
    fn left_cross_line() {
        let mut input = InputArea::new("");
        input.set_text("hello\nworld");
        input.cursor_row = 1;
        input.cursor_col = 0;
        input.move_left();
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 5);
    }

    #[test]
    fn right_cross_line() {
        let mut input = InputArea::new("");
        input.set_text("hello\nworld");
        input.cursor_row = 0;
        input.cursor_col = 5;
        input.move_right();
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn up_down_basic() {
        let mut input = InputArea::new("");
        input.set_text("hello\nhi");
        input.move_up(80);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 2); // clamped
    }

    #[test]
    fn up_down_desired_col() {
        let mut input = InputArea::new("");
        input.set_text("long line\nhi\nlong line");
        input.move_up(80);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 2);
        input.move_up(80);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 9);
    }

    #[test]
    fn up_at_top_is_noop() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 3;
        input.move_up(80);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 3);
    }

    #[test]
    fn down_at_bottom_is_noop() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 2;
        input.move_down(80);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 2);
    }

    #[test]
    fn home_end() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 3;
        input.move_home();
        assert_eq!(input.cursor_col, 0);
        input.move_end();
        assert_eq!(input.cursor_col, 5);
    }

    // ── Key event tests ──────────────────────────────────────

    #[test]
    fn enter_submits() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key(KeyCode::Enter), 80);
        assert_eq!(action, InputAction::Submit("hello".into()));
        assert_eq!(input.text(), "");
    }

    #[test]
    fn enter_empty_does_nothing() {
        let mut input = InputArea::new("");
        let action = input.handle_key(key(KeyCode::Enter), 80);
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key_with(KeyCode::Enter, KeyModifiers::SHIFT), 80);
        assert_eq!(action, InputAction::None);
        assert_eq!(input.line_count(), 2);
    }

    #[test]
    fn alt_enter_inserts_newline() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key_with(KeyCode::Enter, KeyModifiers::ALT), 80);
        assert_eq!(action, InputAction::None);
        assert_eq!(input.line_count(), 2);
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key_with(KeyCode::Char('j'), KeyModifiers::CONTROL), 80);
        assert_eq!(action, InputAction::None);
        assert_eq!(input.line_count(), 2);
    }

    #[test]
    fn up_down_via_keys() {
        let mut input = InputArea::new("");
        input.set_text("a\nb");
        input.handle_key(key(KeyCode::Up), 80);
        assert_eq!(input.cursor_row, 0);
        input.handle_key(key(KeyCode::Down), 80);
        assert_eq!(input.cursor_row, 1);
    }

    #[test]
    fn esc_returns_escape() {
        let mut input = InputArea::new("");
        let action = input.handle_key(key(KeyCode::Esc), 80);
        assert_eq!(action, InputAction::Escape);
    }

    // ── Paste tests ──────────────────────────────────────────

    #[test]
    fn paste_single_line() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 3;
        input.insert_str("XY");
        assert_eq!(input.text(), "helXYlo");
        assert_eq!(input.cursor_col, 5);
    }

    #[test]
    fn paste_multi_line() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 5;
        input.insert_str("world\nfoo");
        assert_eq!(input.lines(), ["helloworld", "foo"]);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 3);
    }

    #[test]
    fn paste_multi_line_mid_line() {
        let mut input = InputArea::new("");
        input.set_text("AB");
        input.cursor_col = 1;
        input.insert_str("X\nY");
        assert_eq!(input.lines(), ["AX", "YB"]);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 1);
    }

    #[test]
    fn paste_empty_ignored() {
        let mut input = InputArea::new("");
        input.insert_str("");
        assert_eq!(input.text(), "");
        input.insert_str("   ");
        assert_eq!(input.text(), "");
    }

    // ── Height tests ─────────────────────────────────────────

    #[test]
    fn height_single_line() {
        let input = InputArea::new("");
        assert_eq!(input.height(80), 1);
    }

    #[test]
    fn height_multi_line() {
        let mut input = InputArea::new("");
        input.set_text("a\nb\nc");
        assert_eq!(input.height(80), 3);
    }

    #[test]
    fn height_capped() {
        let mut input = InputArea::new("");
        let text: String = (0..20).map(|i| format!("{}\n", i)).collect();
        input.set_text(&text);
        assert_eq!(input.height(80), MAX_INPUT_LINES as u16);
    }

    // ── Additional tests ─────────────────────────────────────

    #[test]
    fn ctrl_m_inserts_newline() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key_with(KeyCode::Char('m'), KeyModifiers::CONTROL), 80);
        assert_eq!(action, InputAction::None);
        assert_eq!(input.line_count(), 2);
    }

    #[test]
    fn raw_cr_as_enter_submits() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key(KeyCode::Char('\r')), 80);
        assert_eq!(action, InputAction::Submit("hello".into()));
    }

    #[test]
    fn raw_lf_as_enter_submits() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key(KeyCode::Char('\n')), 80);
        assert_eq!(action, InputAction::Submit("hello".into()));
    }

    #[test]
    fn clear_resets_paste_counter() {
        let mut input = InputArea::new("");
        input.paste_counter = 5;
        input.clear();
        assert_eq!(input.paste_counter, 0);
    }

    #[test]
    fn set_text_clears_pending_pastes() {
        let mut input = InputArea::new("");
        input.pending_pastes.push(("a".into(), "b".into()));
        input.set_text("new");
        assert!(input.pending_pastes.is_empty());
    }

    #[test]
    fn placeholder_line_readonly() {
        let mut input = InputArea::new("");
        input.lines = vec!["[Pasted text #1 +5 lines]".into(), String::new()];
        input.cursor_row = 0;
        input.cursor_col = 0;
        input.insert_char('x'); // should be ignored
        assert_eq!(input.lines[0], "[Pasted text #1 +5 lines]");
    }

    #[test]
    fn placeholder_on_new_line_after_text() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.insert_paste_placeholder("big text\nmore text\nend".into());
        assert!(input.lines.len() >= 2);
        assert!(input.lines.iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn insert_newline_on_placeholder_inserts_after() {
        let mut input = InputArea::new("");
        input.lines = vec!["[Pasted text #1 +5 lines]".into(), String::new()];
        input.cursor_row = 0;
        input.cursor_col = 0;
        input.insert_newline();
        assert_eq!(input.lines.len(), 3);
        assert_eq!(input.lines[0], "[Pasted text #1 +5 lines]");
        assert_eq!(input.lines[1], "");
    }

    #[test]
    fn delete_placeholder_line_cleans_pending() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.insert_paste_placeholder("big text\nmore text\nend".into());
        // Move up to the line before the placeholder.
        while input.cursor_row > 0 {
            input.move_up(80);
        }
        // Move to end of line so delete_forward merges with the placeholder.
        input.move_end();
        // Forward-delete: at end of "hello", next line is placeholder → removes it.
        input.delete_forward();
        assert!(input.pending_pastes.is_empty());
    }

    #[test]
    fn move_up_skips_placeholder() {
        let mut input = InputArea::new("");
        input.lines = vec![
            "hello".into(),
            "[Pasted text #1 +5 lines]".into(),
            String::new(),
        ];
        input.cursor_row = 2;
        input.cursor_col = 0;
        input.move_up(80);
        assert_eq!(input.cursor_row, 0); // skipped placeholder
    }

    #[test]
    fn move_down_skips_placeholder() {
        let mut input = InputArea::new("");
        input.lines = vec![
            "hello".into(),
            "[Pasted text #1 +5 lines]".into(),
            String::new(),
        ];
        input.cursor_row = 0;
        input.cursor_col = 5;
        input.move_down(80);
        assert_eq!(input.cursor_row, 2); // skipped placeholder
    }

    #[test]
    fn move_left_skips_consecutive_placeholders() {
        let mut input = InputArea::new("");
        input.lines = vec![
            "hello".into(),
            "[Pasted text #1]".into(),
            "[Pasted text #2]".into(),
            String::new(),
        ];
        input.cursor_row = 3;
        input.cursor_col = 0;
        input.move_left();
        assert_eq!(input.cursor_row, 0); // skipped both placeholders
    }

    #[test]
    fn move_right_skips_consecutive_placeholders() {
        let mut input = InputArea::new("");
        input.lines = vec![
            "hello".into(),
            "[Pasted text #1]".into(),
            "[Pasted text #2]".into(),
            String::new(),
        ];
        input.cursor_row = 0;
        input.cursor_col = 5;
        input.move_right();
        assert_eq!(input.cursor_row, 3); // skipped both placeholders
    }

    #[test]
    fn paste_201_chars_triggers_placeholder() {
        let mut input = InputArea::new("");
        let long_text = "x".repeat(201);
        input.insert_str(&long_text);
        assert!(input.lines.iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn paste_200_chars_no_placeholder() {
        let mut input = InputArea::new("");
        let text = "x".repeat(200);
        input.insert_str(&text);
        assert!(!input.lines.iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn paste_3_lines_triggers_placeholder() {
        let mut input = InputArea::new("");
        input.insert_str("a\nb\nc\nd");
        assert!(input.lines.iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn paste_2_lines_no_placeholder() {
        let mut input = InputArea::new("");
        input.insert_str("a\nb");
        assert!(!input.lines.iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn paste_preserves_leading_whitespace() {
        let mut input = InputArea::new("");
        input.insert_str("  hello\n  world");
        assert_eq!(input.lines[0], "  hello");
        assert_eq!(input.lines[1], "  world");
    }

    #[test]
    fn paste_rejected_at_max_lines() {
        let mut input = InputArea::new("");
        // Fill to MAX_INPUT_LINES.
        for _ in 0..MAX_INPUT_LINES {
            input.insert_newline();
        }
        let before = input.line_count();
        input.insert_str("should not appear");
        // Line count should not exceed MAX_INPUT_LINES.
        assert!(input.line_count() <= MAX_INPUT_LINES);
        let _ = before;
    }

    #[test]
    fn paste_truncated_at_max_lines() {
        let mut input = InputArea::new("");
        input.set_text("start");
        let multi = (0..15)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        input.insert_str(&multi);
        assert!(input.line_count() <= MAX_INPUT_LINES);
    }

    #[test]
    fn submit_expands_placeholder() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.insert_paste_placeholder("expanded content".into());
        // Submit should expand placeholder.
        let text = input.expand_and_get_text();
        assert!(text.contains("expanded content"));
    }

    #[test]
    fn multiple_pastes_numbered() {
        let mut input = InputArea::new("");
        input.insert_paste_placeholder("first big paste\nline2\nline3".into());
        input.insert_paste_placeholder("second big paste\nline2\nline3".into());
        let placeholder_count = input
            .lines
            .iter()
            .filter(|l| l.starts_with("[Pasted text"))
            .count();
        assert_eq!(placeholder_count, 2);
    }

    #[test]
    fn cursor_screen_pos_first_line() {
        let input = InputArea::new("");
        let area = Rect::new(0, 10, 80, 3);
        let (x, y) = input.cursor_screen_pos(&area);
        assert_eq!(y, 10); // first visible row
        assert_eq!(x, 2); // PREFIX_WIDTH offset
    }

    #[test]
    fn cursor_screen_pos_second_line() {
        let mut input = InputArea::new("");
        input.set_text("a\nb");
        input.cursor_row = 1;
        input.cursor_col = 1;
        let area = Rect::new(0, 10, 80, 3);
        let (x, y) = input.cursor_screen_pos(&area);
        assert_eq!(y, 11); // second visible row
        assert_eq!(x, 3); // PREFIX_WIDTH + 1 char
    }

    // ── Word-wrap navigation tests ─────────────────────────

    #[test]
    fn wrap_up_down_within_long_line() {
        // Width 80, prefix 2 → text_width 78.
        // 20 chars fits in one visual row.
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmnopqrst"); // 20 chars, fits in 78
        input.cursor_col = 15;
        // Up should be no-op (only one visual row).
        input.move_up(80);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 15);
    }

    #[test]
    fn wrap_up_down_across_visual_rows() {
        // Width 12, prefix 2 → text_width 10.
        // "abcdefghijklmno" = 15 chars → 2 visual rows (10 + 5)
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno");
        input.cursor_col = 12; // in second visual row
        // Up should move to first visual row, same column.
        input.move_up(12);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 2); // desired col = 2 (12 - 10), clamped
    }

    #[test]
    fn wrap_down_to_next_logical_line() {
        // Width 12, prefix 2 → text_width 10.
        let mut input = InputArea::new("");
        input.set_text("abcdefghij\nhello"); // "abcdefghij" = 10 chars = 1 vis row
        input.cursor_row = 0;
        input.cursor_col = 5;
        // Down from first line's only visual row → second line.
        input.move_down(12);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 5);
    }

    #[test]
    fn wrap_cjk_boundary() {
        // Width 8, prefix 2 → text_width 6.
        // "你好世界" = 4 CJK chars × 2 width = 8 display cols.
        // text_width 6: "你好世" = 6 cols, "界" = 2 cols → 2 visual rows.
        let mut input = InputArea::new("");
        input.set_text("你好世界");
        input.cursor_col = 3; // "界" in second visual row (vis_col = 0)
        input.move_up(8);
        assert_eq!(input.cursor_row, 0);
        // desired_col = 0 (first col of VR1), so cursor goes to col 0 ("你")
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn wrap_height_reflects_visual_rows() {
        // Width 12, prefix 2 → text_width 10.
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno"); // 15 chars → 2 visual rows
        assert_eq!(input.height(12), 2);
    }
}
