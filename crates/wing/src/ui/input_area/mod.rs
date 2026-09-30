//! Composer — the multi-line text input at the bottom, drawn as a card.

pub(crate) mod chrome;
pub(crate) mod editing;
pub(crate) mod helpers;
pub(crate) mod model;
pub(crate) mod movement;
pub(crate) mod paste;
pub mod pointer;
pub(crate) mod widget;
pub(crate) mod wrap;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

use chrome::Chrome;
use model::Line;
use model::Pastes;

// Re-exports for external use.
pub use chrome::ActivityRail;
pub use chrome::MetaRail;
pub use widget::ComposerWidget;
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
/// Text is stored as lines of [`model::Segment`]s (no embedded newlines):
/// literal text, plus the chips of large pastes. Cursor position is
/// `(row, col)` where `col` is a char index into the line's flat projection —
/// a chip counts as its visible text (see [`model`]), and the cursor never
/// rests inside one.
pub struct InputArea {
    /// Lines of text. Always has at least one element.
    pub(crate) lines: Vec<Line>,
    /// Cursor row (0-based, index into `lines`).
    pub(crate) cursor_row: usize,
    /// Cursor column (0-based, char index within the current line).
    pub(crate) cursor_col: usize,
    /// Sticky display column for Up/Down navigation.
    pub(crate) desired_col: Option<usize>,
    /// Placeholder text shown when empty.
    pub(crate) placeholder: String,
    /// Vertical scroll offset (first visible visual row index).
    pub(crate) vertical_scroll: usize,
    /// The pastes the draft's chips stand for (expanded on submit).
    pub(crate) pastes: Pastes,
    /// Maximum number of input lines (configurable).
    pub(crate) max_lines: usize,
    /// The rect the widget rendered into on the last frame.
    ///
    /// Recorded at render time — like [`Self::vertical_scroll`], which the
    /// render also updates — because mouse events arrive *between* frames: hit
    /// testing has to describe the screen the user is actually pointing at.
    /// Empty before the first frame, which makes every hit test fail instead of
    /// guessing.
    rendered_area: Rect,
}

impl InputArea {
    pub fn new(placeholder: &str) -> Self {
        Self::with_max_lines(placeholder.to_string(), helpers::MAX_INPUT_LINES)
    }

    pub fn with_max_lines(placeholder: String, max_lines: usize) -> Self {
        Self {
            lines: vec![Vec::new()],
            cursor_row: 0,
            cursor_col: 0,
            desired_col: None,
            placeholder,
            vertical_scroll: 0,
            pastes: Pastes::default(),
            max_lines,
            rendered_area: Rect::default(),
        }
    }

    // ── Content access ──────────────────────────────────────────

    /// Get current text as a single string (lines joined by `\n`).
    ///
    /// Chips show up as their visible text; the text that is *submitted* has
    /// them expanded ([`Self::expand_and_get_text`]).
    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(|line| model::flat(line, &self.pastes))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get the draft's lines as plain text (chips as their chip text).
    pub fn lines(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| model::flat(line, &self.pastes))
            .collect()
    }

    /// Number of lines.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The rect the widget rendered into on the last frame (see the field
    /// docs).
    pub fn rendered_area(&self) -> Rect {
        self.rendered_area
    }

    /// The chrome of the composer as it was last laid out.
    ///
    /// Everything that wraps or maps the draft reads *this*, never the width
    /// the layout asked for: a composer the terminal squeezed is drawn with
    /// the chrome of the area it actually got, and the editor has to re-wrap
    /// with the same columns the screen shows.
    pub fn chrome(&self) -> Chrome {
        if self.rendered_area.width == 0 {
            // No frame yet: the assumed width, not a collapsed canvas (see
            // [`chrome::UNFRAMED_WIDTH`]).
            return Chrome::for_width(chrome::UNFRAMED_WIDTH);
        }
        Chrome::of(self.rendered_area)
    }

    /// Screen position of the text area's first cell in the last frame —
    /// inside the card's frame, right of the prompt glyph.
    pub fn text_origin(&self) -> (u16, u16) {
        let chrome = Chrome::of(self.rendered_area);
        (
            self.rendered_area.x + chrome.text_x,
            self.rendered_area.y + chrome.top_row(),
        )
    }

    /// Desired height for layout (the card's frame + its visual rows).
    ///
    /// `available_width` is the width of the **card** (see
    /// [`chrome::card_area`]) — the request has to be made in the columns the
    /// card will really be laid out into, or the frame it asks for does not
    /// match the wrapping it will get.
    pub fn height(&self, available_width: u16) -> u16 {
        let chrome = Chrome::for_width(available_width);
        let text_width = chrome.text_width as usize;
        let rows = if text_width == 0 {
            1
        } else {
            wrap::build_visual_rows(&self.lines, &self.pastes, text_width)
                .len()
                .clamp(1, self.max_lines) as u16
        };
        chrome.height_for(rows)
    }

    /// Check if all lines are empty/whitespace.
    pub(crate) fn is_empty(&self) -> bool {
        self.lines
            .iter()
            .all(|line| model::flat(line, &self.pastes).trim().is_empty())
    }

    // ── Cursor helpers ──────────────────────────────────────────

    /// Length of the current line in chars.
    pub(crate) fn current_line_len(&self) -> usize {
        model::flat_len(&self.lines[self.cursor_row], &self.pastes)
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
    pub fn handle_key(&mut self, key: KeyEvent, chrome: Chrome) -> InputAction {
        let action = self.dispatch_key(key, chrome);
        // The cursor never rests inside a chip: the arrows step over one and
        // every edit keeps the position at a segment edge (see [`model`]). A
        // regression here would let typing corrupt a chip's label, so it is
        // worth catching at the key that did it rather than in a later test.
        debug_assert!(
            !model::inside_chip(&self.lines[self.cursor_row], &self.pastes, self.cursor_col),
            "cursor at ({}, {}) landed inside a chip",
            self.cursor_row,
            self.cursor_col
        );
        action
    }

    fn dispatch_key(&mut self, key: KeyEvent, chrome: Chrome) -> InputAction {
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
                self.move_up(chrome);
                InputAction::None
            }
            KeyCode::Down => {
                self.move_down(chrome);
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
    ///
    /// The text is taken literally: it can never grow a live chip, even if it
    /// spells one out — chips belong to the draft that inserted them, and this
    /// one has no payloads.
    pub fn set_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![Vec::new()]
        } else {
            text.split('\n').map(model::text_line).collect()
        };
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.current_line_len();
        self.desired_col = None;
        self.vertical_scroll = 0;
        self.pastes.clear();
    }

    /// Clear input and reset cursor.
    pub fn clear(&mut self) {
        self.lines = vec![Vec::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.desired_col = None;
        self.vertical_scroll = 0;
        self.pastes.clear();
    }

    // ── Scroll management ───────────────────────────────────────

    /// Update vertical scroll to keep cursor's visual row visible.
    pub(crate) fn update_vertical_scroll(&mut self, chrome: Chrome) {
        let h = chrome.text_rows as usize;
        if h == 0 {
            return;
        }
        // The rendered chrome's own width: the window this computes describes
        // the rows that were really drawn (a collapsed text area still wraps at
        // one column).
        let text_width = (chrome.text_width as usize).max(1);
        let vis_rows = wrap::build_visual_rows(&self.lines, &self.pastes, text_width);
        let (vis_row, _) = wrap::logical_to_visual(&vis_rows, self.cursor_row, self.cursor_col);

        if vis_row < self.vertical_scroll {
            self.vertical_scroll = vis_row;
        } else if vis_row >= self.vertical_scroll + h {
            self.vertical_scroll = vis_row - h + 1;
        }
    }

    // ── Cursor screen position ──────────────────────────────────

    /// Move the cursor to the position a visual coordinate points at.
    ///
    /// This is the click-to-place-cursor entry point: `vis_row` indexes the
    /// full visual-row list [`wrap::build_visual_rows`] produces (a row below
    /// the last one clamps to it), and `vis_col` is a **display column** inside
    /// the text area — `0` is the first cell after the prompt glyph, and a
    /// value past the row's text lands on that row's end. `chrome` is the one
    /// the widget rendered with ([`Self::chrome`]), so the wrapping matches the
    /// screen exactly.
    ///
    /// The mapping is char-granular: a pointer on a wide character resolves to
    /// the position *before* it ([`wrap::display_col_to_char`]), and one on a
    /// paste chip to the position before the whole chip
    /// ([`model::column_point`]) — the same unit-snapping the hit test reports.
    pub fn set_cursor_from_visual(&mut self, chrome: Chrome, vis_row: usize, vis_col: u16) {
        let text_width = chrome.text_width as usize;
        let vis_rows = wrap::build_visual_rows(&self.lines, &self.pastes, text_width.max(1));
        let Some(last) = vis_rows.len().checked_sub(1) else {
            return;
        };
        let row = vis_rows[vis_row.min(last)];
        let point = model::column_point(
            &self.lines[row.logical_line],
            &self.pastes,
            &row,
            vis_col as usize,
        );
        self.cursor_row = row.logical_line;
        self.cursor_col = point.point;
        self.desired_col = None;
    }

    /// Get cursor screen position as `(x, y)` relative to `area`.
    ///
    /// Returns absolute coordinates suitable for `MoveTo(x, y)`.
    pub fn cursor_screen_pos(&self, area: &Rect) -> (u16, u16) {
        let chrome = Chrome::of(*area);
        let text_width = chrome.text_width as usize;
        let vis_rows = wrap::build_visual_rows(&self.lines, &self.pastes, text_width.max(1));
        let (vis_row, vis_col) =
            wrap::logical_to_visual(&vis_rows, self.cursor_row, self.cursor_col);

        // Compute display width of the visual column portion.
        let line = model::flat(&self.lines[self.cursor_row], &self.pastes);
        let vr = &vis_rows[vis_row.min(vis_rows.len() - 1)];
        let col_display: usize = line
            .chars()
            .skip(vr.char_start)
            .take(vis_col)
            .map(|c| c.width().unwrap_or(0))
            .sum();

        // The cursor lives in the text area: right of the frame's chrome and
        // inside it — a draft cell can never land on a border. The one column
        // past the text (the card's right padding) is allowed: a cursor at the
        // end of a full row sits there instead of jumping a row early.
        let text_x = area.x.saturating_add(chrome.text_x);
        let right_edge = area
            .right()
            .saturating_sub(1 + u16::from(chrome.card))
            .min(text_x.saturating_add(chrome.text_width));
        let x = text_x.saturating_add(col_display as u16).min(right_edge);
        let visible_row = vis_row.saturating_sub(self.vertical_scroll);
        let y = area
            .y
            .saturating_add(chrome.top_row())
            .saturating_add(visible_row as u16);
        (
            x,
            y.min(
                area.bottom()
                    .saturating_sub(if chrome.card { 2 } else { 1 }),
            ),
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::helpers::MAX_INPUT_LINES;
    use super::*;
    use crossterm::event::KeyModifiers;

    /// The chrome of a `width`-column card — what the tests hand the editor.
    /// Same columns as a real area of that width: one text row, frame included.
    fn chrome(width: u16) -> Chrome {
        Chrome::for_width(width)
    }

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
        assert_eq!(input.height(80), 1 + chrome::BORDER_ROWS);
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
        input.handle_key(key(KeyCode::Char('h')), chrome(80));
        input.handle_key(key(KeyCode::Char('i')), chrome(80));
        assert_eq!(input.text(), "hi");
    }

    #[test]
    fn insert_newline_splits() {
        let mut input = InputArea::new("");
        input.set_text("hello world");
        input.cursor_row = 0;
        input.cursor_col = 5;
        input.handle_key(key_with(KeyCode::Enter, KeyModifiers::SHIFT), chrome(80));
        assert_eq!(input.lines(), ["hello", " world"]);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn insert_newline_at_end() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.handle_key(key_with(KeyCode::Enter, KeyModifiers::ALT), chrome(80));
        assert_eq!(input.lines(), ["hello", ""]);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn insert_newline_at_start() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 0;
        input.handle_key(
            key_with(KeyCode::Char('j'), KeyModifiers::CONTROL),
            chrome(80),
        );
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
        input.handle_key(key(KeyCode::Backspace), chrome(80));
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
        input.move_up(chrome(80));
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 2); // clamped
    }

    #[test]
    fn up_down_desired_col() {
        let mut input = InputArea::new("");
        input.set_text("long line\nhi\nlong line");
        input.move_up(chrome(80));
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 2);
        input.move_up(chrome(80));
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 9);
    }

    #[test]
    fn up_at_top_is_noop() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 3;
        input.move_up(chrome(80));
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 3);
    }

    #[test]
    fn down_at_bottom_is_noop() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        input.cursor_col = 2;
        input.move_down(chrome(80));
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
        let action = input.handle_key(key(KeyCode::Enter), chrome(80));
        assert_eq!(action, InputAction::Submit("hello".into()));
        assert_eq!(input.text(), "");
    }

    #[test]
    fn enter_empty_does_nothing() {
        let mut input = InputArea::new("");
        let action = input.handle_key(key(KeyCode::Enter), chrome(80));
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key_with(KeyCode::Enter, KeyModifiers::SHIFT), chrome(80));
        assert_eq!(action, InputAction::None);
        assert_eq!(input.line_count(), 2);
    }

    #[test]
    fn alt_enter_inserts_newline() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key_with(KeyCode::Enter, KeyModifiers::ALT), chrome(80));
        assert_eq!(action, InputAction::None);
        assert_eq!(input.line_count(), 2);
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(
            key_with(KeyCode::Char('j'), KeyModifiers::CONTROL),
            chrome(80),
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input.line_count(), 2);
    }

    #[test]
    fn up_down_via_keys() {
        let mut input = InputArea::new("");
        input.set_text("a\nb");
        input.handle_key(key(KeyCode::Up), chrome(80));
        assert_eq!(input.cursor_row, 0);
        input.handle_key(key(KeyCode::Down), chrome(80));
        assert_eq!(input.cursor_row, 1);
    }

    #[test]
    fn esc_returns_escape() {
        let mut input = InputArea::new("");
        let action = input.handle_key(key(KeyCode::Esc), chrome(80));
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
        // One text row inside the card's frame.
        assert_eq!(input.height(80), 1 + chrome::BORDER_ROWS);
    }

    #[test]
    fn height_multi_line() {
        let mut input = InputArea::new("");
        input.set_text("a\nb\nc");
        assert_eq!(input.height(80), 3 + chrome::BORDER_ROWS);
    }

    #[test]
    fn height_capped() {
        let mut input = InputArea::new("");
        let text: String = (0..20).map(|i| format!("{}\n", i)).collect();
        input.set_text(&text);
        assert_eq!(
            input.height(80),
            MAX_INPUT_LINES as u16 + chrome::BORDER_ROWS
        );
    }

    #[test]
    fn height_drops_the_frame_when_it_would_not_fit() {
        // Too narrow for a card: bare text rows, and `Chrome::of` agrees with
        // the height the app would lay out.
        let mut input = InputArea::new("");
        input.set_text("a\nb");
        let width = 8;
        assert_eq!(input.height(width), 2);
        let laid_out = Rect::new(0, 0, width, input.height(width));
        assert!(!Chrome::of(laid_out).card);
        assert_eq!(Chrome::of(laid_out).text_rows, 2);
    }

    // ── Additional tests ─────────────────────────────────────

    #[test]
    fn ctrl_m_inserts_newline() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(
            key_with(KeyCode::Char('m'), KeyModifiers::CONTROL),
            chrome(80),
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input.line_count(), 2);
    }

    #[test]
    fn raw_cr_as_enter_submits() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key(KeyCode::Char('\r')), chrome(80));
        assert_eq!(action, InputAction::Submit("hello".into()));
    }

    #[test]
    fn raw_lf_as_enter_submits() {
        let mut input = InputArea::new("");
        input.set_text("hello");
        let action = input.handle_key(key(KeyCode::Char('\n')), chrome(80));
        assert_eq!(action, InputAction::Submit("hello".into()));
    }

    #[test]
    fn clear_restarts_the_chip_numbering() {
        let mut input = InputArea::new("");
        input.insert_paste("a\nb\nc".into());
        assert_eq!(input.lines(), ["[Pasted text #1 +2 lines]"]);
        input.clear();
        input.insert_paste("a\nb".into());
        assert_eq!(input.lines(), ["[Pasted text #1 +1 lines]"]);
    }

    #[test]
    fn set_text_takes_its_text_literally() {
        let mut input = InputArea::new("");
        input.insert_paste("big\npaste\ntext".into());
        input.set_text("[Pasted text #1 +2 lines]");
        assert_eq!(
            input.pastes.payload(1),
            None,
            "a restored draft holds no payloads"
        );
        // …so the look-alike is ordinary text: Delete takes one character at
        // a time, not the whole string.
        input.cursor_col = 0;
        input.delete_forward();
        assert_eq!(input.text(), "Pasted text #1 +2 lines]");
    }

    #[test]
    fn a_chip_is_inserted_inline_and_the_cursor_stays_after_it() {
        let mut input = InputArea::new("");
        input.set_text("see ");
        input.insert_paste("a\nb\nc".into());
        let chip = "[Pasted text #1 +2 lines]";
        assert_eq!(input.lines(), [format!("see {chip}")]);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 4 + chip.chars().count());
        // Typing continues on the same line, right after the chip.
        input.insert_char('!');
        assert_eq!(input.lines(), [format!("see {chip}!")]);
    }

    #[test]
    fn a_chip_inserts_mid_line_without_reflowing_the_text() {
        let mut input = InputArea::new("");
        input.set_text("AB");
        input.cursor_col = 1;
        input.insert_paste("1\n2".into());
        let chip = "[Pasted text #1 +1 lines]";
        assert_eq!(input.lines(), [format!("A{chip}B")]);
        assert_eq!(input.cursor_col, 1 + chip.chars().count());
    }

    #[test]
    fn typing_at_a_chip_edge_never_enters_it() {
        let mut input = InputArea::new("");
        input.set_text("see ");
        input.insert_paste("a\nb\nc".into());
        let chip = "[Pasted text #1 +2 lines]";
        // At the leading edge: the text lands before the chip.
        input.cursor_col = 4;
        input.insert_char('<');
        assert_eq!(input.lines(), [format!("see <{chip}")]);
        // At the trailing edge: after it.
        input.cursor_col = 5 + chip.chars().count();
        input.insert_char('>');
        assert_eq!(input.lines(), [format!("see <{chip}>")]);
    }

    #[test]
    fn backspace_removes_a_chip_whole() {
        let mut input = InputArea::new("");
        input.set_text("see ");
        input.insert_paste("a\nb\nc".into());
        input.backspace();
        assert_eq!(input.lines(), ["see "]);
        assert_eq!(input.cursor_col, 4, "the cursor lands where the chip was");
        assert_eq!(
            input.pastes.payload(1),
            None,
            "the payload goes with the chip"
        );
        // …and the next backspace is a plain character again.
        input.backspace();
        assert_eq!(input.lines(), ["see"]);
    }

    #[test]
    fn delete_removes_a_chip_whole() {
        let mut input = InputArea::new("");
        input.set_text("see ");
        input.insert_paste("a\nb\nc".into());
        input.cursor_col = 4;
        input.delete_forward();
        assert_eq!(input.lines(), ["see "]);
        assert_eq!(input.pastes.payload(1), None);
    }

    #[test]
    fn arrows_step_over_a_chip() {
        let mut input = InputArea::new("");
        input.set_text("ab");
        input.insert_paste("x\ny".into());
        let end = input.cursor_col;
        input.move_left();
        assert_eq!(input.cursor_col, 2, "left lands before the chip");
        input.move_right();
        assert_eq!(input.cursor_col, end, "right steps over it whole");
    }

    #[test]
    fn a_newline_never_splits_a_chip() {
        let mut input = InputArea::new("");
        input.set_text("ab");
        input.insert_paste("x\ny".into());
        input.cursor_col = 2;
        input.insert_newline();
        assert_eq!(input.lines(), ["ab", "[Pasted text #1 +1 lines]"]);
        assert_eq!((input.cursor_row, input.cursor_col), (1, 0));
    }

    #[test]
    fn vertical_movement_onto_a_chip_lands_before_it() {
        let mut input = InputArea::new("");
        input.set_text("hi");
        input.insert_paste("x\ny".into()); // "hi[Pasted text #1 +1 lines]"
        input.insert_newline();
        input.insert_str("zebra");
        assert_eq!((input.cursor_row, input.cursor_col), (1, 5));
        input.move_up(chrome(80));
        assert_eq!(
            (input.cursor_row, input.cursor_col),
            (0, 2),
            "a column inside the chip resolves to its leading edge"
        );
    }

    #[test]
    fn chip_numbers_are_never_reused() {
        let mut input = InputArea::new("");
        input.insert_paste("first\npaste\nx".into());
        assert_eq!(input.lines(), ["[Pasted text #1 +2 lines]"]);
        input.cursor_col = 0;
        input.delete_forward();
        input.insert_paste("second\npaste\ny".into());
        assert_eq!(
            input.lines(),
            ["[Pasted text #2 +2 lines]"],
            "the freed number is not handed out again"
        );
    }

    #[test]
    fn enter_submits_the_expanded_paste() {
        let mut input = InputArea::new("");
        input.insert_str("1\n2\n3\n4\n5");
        let action = input.handle_key(key(KeyCode::Enter), chrome(80));
        assert_eq!(action, InputAction::Submit("1\n2\n3\n4\n5".into()));
    }

    #[test]
    fn paste_201_chars_becomes_a_chip() {
        let mut input = InputArea::new("");
        let long_text = "x".repeat(201);
        input.insert_str(&long_text);
        assert!(input.lines().iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn paste_200_chars_stays_text() {
        let mut input = InputArea::new("");
        let text = "x".repeat(200);
        input.insert_str(&text);
        assert!(!input.lines().iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn paste_3_lines_becomes_a_chip() {
        let mut input = InputArea::new("");
        input.insert_str("a\nb\nc\nd");
        assert!(input.lines().iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn paste_2_lines_stays_text() {
        let mut input = InputArea::new("");
        input.insert_str("a\nb");
        assert!(!input.lines().iter().any(|l| l.starts_with("[Pasted text")));
    }

    #[test]
    fn paste_preserves_leading_whitespace() {
        let mut input = InputArea::new("");
        input.insert_str("  hello\n  world");
        assert_eq!(input.lines()[0], "  hello");
        assert_eq!(input.lines()[1], "  world");
    }

    #[test]
    fn paste_at_max_lines_still_lands_on_the_last_line() {
        let mut input = InputArea::new("");
        // Fill to MAX_INPUT_LINES.
        for _ in 0..MAX_INPUT_LINES {
            input.insert_newline();
        }
        // A single-line paste adds no line, so the cap does not reject it.
        input.insert_str("should appear");
        assert_eq!(input.line_count(), MAX_INPUT_LINES);
        assert!(input.text().ends_with("should appear"));
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
    fn submit_expands_the_chip() {
        let mut input = InputArea::new("");
        input.set_text("hello ");
        input.insert_paste("expanded\ncontent\nhere".into());
        assert_eq!(input.expand_and_get_text(), "hello expanded\ncontent\nhere");
        // The draft still shows the chip.
        assert_eq!(input.lines(), ["hello [Pasted text #1 +2 lines]"]);
    }

    #[test]
    fn multiple_pastes_numbered() {
        let mut input = InputArea::new("");
        input.insert_paste("first big paste\nline2\nline3".into());
        input.insert_paste("second big paste\nline2\nline3".into());
        assert_eq!(
            input.text(),
            "[Pasted text #1 +2 lines][Pasted text #2 +2 lines]"
        );
    }

    #[test]
    fn cursor_screen_pos_first_line() {
        let input = InputArea::new("");
        let area = Rect::new(0, 10, 80, 3);
        let (x, y) = input.cursor_screen_pos(&area);
        assert_eq!(y, 11); // first text row (the top border is above it)
        assert_eq!(x, 4); // inside the frame, right of the prompt glyph
    }

    #[test]
    fn cursor_screen_pos_second_line() {
        let mut input = InputArea::new("");
        input.set_text("a\nb");
        input.cursor_row = 1;
        input.cursor_col = 1;
        let area = Rect::new(0, 10, 80, 4);
        let (x, y) = input.cursor_screen_pos(&area);
        assert_eq!(y, 12); // second text row
        assert_eq!(x, 5); // first text column + 1 char
    }

    #[test]
    fn cursor_screen_pos_never_lands_on_the_frame() {
        // Width 12 → a text area of 6 columns, filled to the last cell.
        let mut input = InputArea::new("");
        input.set_text("abcdef");
        input.cursor_col = 6;
        let area = Rect::new(0, 10, 12, 3);
        assert_eq!(
            input.cursor_screen_pos(&area),
            (10, 11),
            "the end of a full row sits in the card's padding, not on its border"
        );
    }

    // ── Word-wrap navigation tests ─────────────────────────

    #[test]
    fn wrap_up_down_within_long_line() {
        // Width 80 → text_width 74.
        // 20 chars fits in one visual row.
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmnopqrst"); // 20 chars, fits in 78
        input.cursor_col = 15;
        // Up should be no-op (only one visual row).
        input.move_up(chrome(80));
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 15);
    }

    #[test]
    fn wrap_up_down_across_visual_rows() {
        // Width 16 → text_width 10.
        // "abcdefghijklmno" = 15 chars → 2 visual rows (10 + 5)
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno");
        input.cursor_col = 12; // in second visual row
        // Up should move to first visual row, same column.
        input.move_up(chrome(16));
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 2); // desired col = 2 (12 - 10), clamped
    }

    #[test]
    fn wrap_down_to_next_logical_line() {
        // Width 16 → text_width 10.
        let mut input = InputArea::new("");
        input.set_text("abcdefghij\nhello"); // "abcdefghij" = 10 chars = 1 vis row
        input.cursor_row = 0;
        input.cursor_col = 5;
        // Down from first line's only visual row → second line.
        input.move_down(chrome(16));
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 5);
    }

    #[test]
    fn wrap_cjk_boundary() {
        // Width 12 → text_width 6.
        // "你好世界" = 4 CJK chars × 2 width = 8 display cols.
        // text_width 6: "你好世" = 6 cols, "界" = 2 cols → 2 visual rows.
        let mut input = InputArea::new("");
        input.set_text("你好世界");
        input.cursor_col = 3; // "界" in second visual row (vis_col = 0)
        input.move_up(chrome(12));
        assert_eq!(input.cursor_row, 0);
        // desired_col = 0 (first col of VR1), so cursor goes to col 0 ("你")
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn wrap_height_reflects_visual_rows() {
        // Width 16 → text_width 10.
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno"); // 15 chars → 2 visual rows
        assert_eq!(input.height(16), 2 + chrome::BORDER_ROWS);
    }

    // ── Click-to-place-cursor mapping ──────────────────────

    #[test]
    fn set_cursor_from_visual_maps_the_first_row() {
        // Width 80 → text_width 78, single visual row.
        let mut input = InputArea::new("");
        input.set_text("hello world");
        input.set_cursor_from_visual(chrome(80), 0, 6);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 6));
        // Past the row's text: the row end.
        input.set_cursor_from_visual(chrome(80), 0, 99);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 11));
        // Column 0: the row start.
        input.set_cursor_from_visual(chrome(80), 0, 0);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 0));
    }

    #[test]
    fn set_cursor_from_visual_follows_soft_wraps() {
        // Width 16 → text_width 10: "abcdefghijklmno" = "abcdefghij" | "klmno".
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno");
        input.set_cursor_from_visual(chrome(16), 0, 3);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 3));
        // Second visual row: char offset 10 + 2.
        input.set_cursor_from_visual(chrome(16), 1, 2);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 12));
        // The row boundary is shared: end of row 0 == start of row 1.
        input.set_cursor_from_visual(chrome(16), 0, 10);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 10));
        input.set_cursor_from_visual(chrome(16), 1, 0);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 10));
    }

    #[test]
    fn set_cursor_from_visual_maps_the_logical_row_under_a_wrapped_row() {
        // Width 16 → text_width 10: "abcdefghijklmno" | "hello".
        let mut input = InputArea::new("");
        input.set_text("abcdefghijklmno\nhello");
        input.set_cursor_from_visual(chrome(16), 2, 3);
        assert_eq!((input.cursor_row, input.cursor_col), (1, 3));
    }

    #[test]
    fn set_cursor_from_visual_handles_wide_characters() {
        // Width 12 → text_width 6: "你好世" | "界".
        let mut input = InputArea::new("");
        input.set_text("你好世界");
        // Either cell of '你' resolves to before it.
        input.set_cursor_from_visual(chrome(12), 0, 0);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 0));
        input.set_cursor_from_visual(chrome(12), 0, 1);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 0));
        // '世' spans columns 4..6 → before it.
        input.set_cursor_from_visual(chrome(12), 0, 5);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 2));
        // Second visual row: '界' is char 3.
        input.set_cursor_from_visual(chrome(12), 1, 1);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 3));
    }

    #[test]
    fn set_cursor_from_visual_handles_empty_lines_and_clamps_rows() {
        let mut input = InputArea::new("");
        input.set_text("a\n\nb");
        // The empty logical line is its own visual row.
        input.set_cursor_from_visual(chrome(80), 1, 5);
        assert_eq!((input.cursor_row, input.cursor_col), (1, 0));
        // A row below the last one clamps to it.
        input.set_cursor_from_visual(chrome(80), 99, 0);
        assert_eq!((input.cursor_row, input.cursor_col), (2, 0));

        // An empty draft maps to (0, 0) — the placeholder is not content.
        let mut input = InputArea::new("placeholder");
        input.set_cursor_from_visual(chrome(80), 0, 4);
        assert_eq!((input.cursor_row, input.cursor_col), (0, 0));
    }

    #[test]
    fn set_cursor_from_visual_uses_the_scrolled_window() {
        // Width 16 → text_width 10, max_lines 2: the window follows the
        // cursor, which sits on the last logical line.
        let mut input = InputArea::with_max_lines(String::new(), 2);
        input.set_text("one\ntwo\nthree");
        input.update_vertical_scroll(Chrome::of(Rect::new(0, 0, 16, 2 + chrome::BORDER_ROWS)));
        assert_eq!(input.vertical_scroll, 1, "window shows logical lines 1..2");
        // The caller (the pointer hit test) adds the window offset, so the
        // row handed in is absolute: window row 1 = visual row 2.
        input.set_cursor_from_visual(chrome(16), 2, 2);
        assert_eq!((input.cursor_row, input.cursor_col), (2, 2));
    }

    #[test]
    fn set_cursor_from_visual_round_trips_with_cursor_screen_pos() {
        let mut input = InputArea::new("");
        input.set_text("你好a好你\nsecond line");
        let area = Rect::new(0, 10, 16, 4);
        let chrome = Chrome::of(area);
        for vis_row in 0..chrome.text_rows as usize {
            let (x, y) = {
                input.set_cursor_from_visual(chrome, vis_row, 0);
                input.cursor_screen_pos(&area)
            };
            assert_eq!(
                y,
                area.y + chrome.top_row() + vis_row as u16,
                "row {vis_row} stays on screen"
            );
            assert_eq!(
                x,
                area.x + chrome.text_x,
                "column 0 is the row's first cell"
            );
        }
    }
}
