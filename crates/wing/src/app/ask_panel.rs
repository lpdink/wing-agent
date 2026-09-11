//! AskPanel — AskUserQuestion adapter on top of the selection-panel kernel.
//!
//! Kernel responsibilities ([`SelectionPanel`]): page switching, per-question
//! cursor memory, single-select commit capture. Adapter responsibilities
//! (here): multi-select toggles, the inline free-form editor, the confirm page
//! (registered as a *custom* page — the kernel keeps its tab slot but has no
//! cursor on it), and reply construction.
//!
//! The panel renders a tab bar (question headers + a final confirm page),
//! one question at a time with selectable options, and a free-form
//! "Type Something" row. Its key map:
//!
//! - `↑`/`↓` — move the option cursor (wraps; the last row is the free-form row)
//! - `←`/`→` — switch question tab (wraps; the last tab is the confirm page)
//! - `Space`/`Tab` — toggle the option under the cursor (multi-select only)
//! - `Enter` — advance to the next tab; on a single-select option row it also
//!   commits that option; on the free-form row it starts inline editing;
//!   while editing it confirms the text and advances
//! - edit mode: `←`/`→` move the text cursor, `Backspace`/`Delete`/`Home`/`End`
//!   edit the buffer, `↑`/`↓` leave the editor keeping the draft
//! - `Esc` is NOT consumed here — the app owns it (interrupt) at any time.
//!
//! Answers are **explicit acts only** — browsing (moving the cursor, switching
//! tabs) never records an answer, so tabs only turn "answered" when the user
//! actually chose something:
//! - single-select: the option committed with Enter (captured via the kernel,
//!   not derived — later cursor movement cannot change it; re-commit overrides)
//! - multi-select: the toggled options (each toggle is itself an explicit act)
//! - free-form: the text confirmed with Enter in the editor
//!
//! The user may leave questions unanswered: Submit is not gated, unanswered
//! questions are sent as `(user did not answer)` and the confirm page warns
//! about them. An empty free-form row never counts as an answer.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;

use crate::app::selection_panel::PageKind;
use crate::app::selection_panel::SelectionPanel;
use crate::app::selection_panel::wrap_index;
use crate::protocol::AskQuestion;

/// Content sent to the backend when the user cancels from the confirm page.
/// The AskUserQuestion tool translates it into a normal "user cancelled"
/// result — it does NOT interrupt the turn.
pub const ASK_CANCEL_CONTENT: &str = "__wing_ask_cancelled__";

/// Placeholder sent for a question the user left unanswered.
pub const UNANSWERED_PLACEHOLDER: &str = "(user did not answer)";

/// Terminal state of a finished panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelFinish {
    Submitted,
    Cancelled,
}

/// Outcome of a key event for the app to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelAction {
    /// Key consumed, nothing to send.
    None,
    /// Send this content to the backend (resolves the ask feedback waiter).
    Reply(String),
}

/// Per-question interaction state.
///
/// `cursor` and `selected` are the storage backing the kernel's per-page
/// cursor / committed-row accessors (see the `SelectionPanel` impl below);
/// the rest is adapter state.
#[derive(Debug, Clone, Default)]
pub struct QuestionState {
    /// Cursor row: `0..options.len()` = option, `options.len()` = free-form row.
    /// Pure navigation — never part of the answer.
    pub cursor: usize,
    /// Multi-select toggle state (parallel to `options`).
    pub toggles: Vec<bool>,
    /// Committed single-select option index (Enter on an option row).
    /// None = no option committed yet; captured at commit time so later
    /// cursor movement cannot change it.
    pub selected: Option<usize>,
    /// Confirmed free-form text (None = not used). Confirming text clears
    /// `selected` — the last explicit choice wins.
    pub custom: Option<String>,
    /// Whether the inline editor is active on the free-form row.
    pub editing: bool,
    /// Editor buffer; survives leaving the editor (kept as an uncommitted draft).
    pub draft: String,
    /// Editor cursor position in chars.
    pub edit_cursor: usize,
}

/// Interactive state for one AskUserQuestion panel.
#[derive(Debug, Clone)]
pub struct AskPanel {
    /// Correlation id echoed back when sending the reply.
    pub tool_call_id: String,
    /// Questions (option-normalized on construction).
    pub questions: Vec<AskQuestion>,
    /// Per-question interaction state (parallel to `questions`).
    pub states: Vec<QuestionState>,
    /// Active tab: `0..questions.len()` = question, `questions.len()` = confirm page.
    pub current: usize,
    /// Cursor on the confirm page (0 = Submit, 1 = Cancel).
    pub confirm_cursor: usize,
    /// Set once the panel has been submitted/cancelled (terminal state).
    pub finished: Option<PanelFinish>,
}

impl AskPanel {
    pub fn new(tool_call_id: String, questions: Vec<AskQuestion>) -> Self {
        let questions: Vec<AskQuestion> = questions.into_iter().map(normalize_question).collect();
        let states = questions
            .iter()
            .map(|q| QuestionState {
                toggles: vec![false; q.options.len()],
                ..Default::default()
            })
            .collect();
        Self {
            tool_call_id,
            questions,
            states,
            current: 0,
            confirm_cursor: 0,
            finished: None,
        }
    }

    /// Whether the confirm page is active.
    pub fn on_confirm_page(&self) -> bool {
        self.current >= self.questions.len()
    }

    /// Whether the inline editor is active right now.
    pub fn editing(&self) -> bool {
        !self.on_confirm_page() && self.states[self.current].editing
    }

    /// Whether the cursor is on the free-form row of the current question.
    fn on_custom_row(&self) -> bool {
        !self.on_confirm_page()
            && self.states[self.current].cursor >= self.questions[self.current].options.len()
    }

    /// Answer value for question `qi`, or None when unanswered.
    ///
    /// Single-select: the committed option (Enter), falling back to confirmed
    /// free-form text. Multi-select: toggled labels plus the free-form text
    /// (comma-joined). Browsing never produces an answer.
    pub fn answer_value(&self, qi: usize) -> Option<String> {
        let q = &self.questions[qi];
        let st = &self.states[qi];
        let custom = st.custom.as_deref().filter(|s| !s.trim().is_empty());
        if q.multi_select {
            let mut parts: Vec<String> = q
                .options
                .iter()
                .zip(st.toggles.iter())
                .filter(|(_, on)| **on)
                .map(|(o, _)| o.label.clone())
                .collect();
            if let Some(text) = custom {
                parts.push(text.to_string());
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(", "))
            }
        } else {
            st.selected
                .and_then(|i| q.options.get(i))
                .map(|o| o.label.clone())
                .or_else(|| custom.map(|s| s.to_string()))
        }
    }

    /// Whether question `qi` holds a usable answer (tab shows the answered state).
    pub fn is_answered(&self, qi: usize) -> bool {
        self.answer_value(qi).is_some()
    }

    /// Whether every question holds a usable answer.
    pub fn all_answered(&self) -> bool {
        (0..self.questions.len()).all(|qi| self.is_answered(qi))
    }

    /// Tab labels of the questions left unanswered (for the confirm-page hint).
    pub fn unanswered_headers(&self) -> Vec<&str> {
        self.questions
            .iter()
            .enumerate()
            .filter(|(qi, _)| !self.is_answered(*qi))
            .map(|(_, q)| q.tab_label())
            .collect()
    }

    /// Final reply text: `header: answer` lines, newline-separated.
    /// Unanswered questions contribute the `(user did not answer)` placeholder
    /// (the user may submit deliberately with gaps — the confirm page warns).
    pub fn build_response(&self) -> String {
        self.questions
            .iter()
            .enumerate()
            .map(|(qi, q)| {
                let answer = self
                    .answer_value(qi)
                    .unwrap_or_else(|| UNANSWERED_PLACEHOLDER.to_string());
                format!("{}: {}", q.tab_label(), answer)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── Cursor / toggles ────────────────────────────────────────

    /// Toggle the option under the cursor (multi-select questions only).
    fn toggle(&mut self) {
        if self.on_confirm_page() {
            return;
        }
        let qi = self.current;
        if !self.questions[qi].multi_select {
            return;
        }
        let st = &mut self.states[qi];
        if st.cursor < st.toggles.len() {
            st.toggles[st.cursor] = !st.toggles[st.cursor];
        }
    }

    /// Switch tab with wrap-around (confirm page included); entering the
    /// confirm page resets its Submit/Cancel cursor.
    fn switch_tab(&mut self, delta: isize) {
        self.move_page(delta);
        if self.on_confirm_page() {
            self.confirm_cursor = 0;
        }
    }

    /// Advance to the next tab, clamped at the confirm page (Enter semantics).
    fn advance(&mut self) {
        if self.current < self.questions.len() {
            self.current += 1;
        }
        if self.on_confirm_page() {
            self.confirm_cursor = 0;
        }
    }

    /// Commit the option under the cursor as the single-select answer
    /// (Enter on an option row). The kernel captures the choice — later
    /// cursor movement cannot change a committed answer; re-commit overrides.
    fn commit_option(&mut self) {
        if self.on_custom_row() {
            return; // the free-form row is no option
        }
        if self.commit_current().is_some() {
            self.states[self.current].custom = None; // last explicit choice wins
        }
    }

    // ── Editor ──────────────────────────────────────────────────

    fn start_editing(&mut self) {
        if self.on_confirm_page() {
            return;
        }
        let st = &mut self.states[self.current];
        st.editing = true;
        st.edit_cursor = st.draft.chars().count();
    }

    fn exit_editing_keep_draft(&mut self) {
        if self.on_confirm_page() {
            return;
        }
        self.states[self.current].editing = false;
    }

    fn confirm_editing(&mut self) {
        if self.on_confirm_page() {
            return;
        }
        let st = &mut self.states[self.current];
        st.editing = false;
        st.custom = if st.draft.trim().is_empty() {
            None
        } else {
            Some(st.draft.clone())
        };
        st.selected = None; // free-form text overrides a committed option
    }

    fn edit_insert_char(&mut self, c: char) {
        let st = &mut self.states[self.current];
        let byte = char_to_byte(&st.draft, st.edit_cursor);
        st.draft.insert(byte, c);
        st.edit_cursor += 1;
    }

    fn edit_backspace(&mut self) {
        let st = &mut self.states[self.current];
        if st.edit_cursor == 0 {
            return;
        }
        let from = char_to_byte(&st.draft, st.edit_cursor - 1);
        let to = char_to_byte(&st.draft, st.edit_cursor);
        st.draft.replace_range(from..to, "");
        st.edit_cursor -= 1;
    }

    fn edit_delete(&mut self) {
        let st = &mut self.states[self.current];
        if st.edit_cursor >= st.draft.chars().count() {
            return;
        }
        let from = char_to_byte(&st.draft, st.edit_cursor);
        let to = char_to_byte(&st.draft, st.edit_cursor + 1);
        st.draft.replace_range(from..to, "");
    }

    fn edit_move(&mut self, delta: isize) {
        let st = &mut self.states[self.current];
        let len = st.draft.chars().count();
        st.edit_cursor = (st.edit_cursor as isize + delta).clamp(0, len as isize) as usize;
    }

    /// Insert pasted text into the editor (starts the editor on the free-form
    /// row). Newlines are folded to spaces — the editor is single-line.
    pub fn insert_paste(&mut self, text: &str) {
        if self.on_confirm_page() {
            return;
        }
        if !self.states[self.current].editing {
            if !self.on_custom_row() {
                return;
            }
            self.start_editing();
        }
        for c in text.chars() {
            self.edit_insert_char(if c == '\n' || c == '\r' { ' ' } else { c });
        }
    }

    // ── Key handling ────────────────────────────────────────────

    /// Handle one key event. `Esc` is intentionally not handled — the app
    /// keeps it as the global interrupt at any time.
    pub fn handle_key(&mut self, key: KeyEvent) -> PanelAction {
        if self.finished.is_some() {
            return PanelAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if self.editing() {
            match key.code {
                KeyCode::Char(c) if !ctrl && !alt => self.edit_insert_char(c),
                KeyCode::Backspace => self.edit_backspace(),
                KeyCode::Delete => self.edit_delete(),
                KeyCode::Left => self.edit_move(-1),
                KeyCode::Right => self.edit_move(1),
                KeyCode::Home => self.states[self.current].edit_cursor = 0,
                KeyCode::End => {
                    let st = &mut self.states[self.current];
                    st.edit_cursor = st.draft.chars().count();
                }
                KeyCode::Up => {
                    self.exit_editing_keep_draft();
                    self.move_cursor(-1);
                }
                KeyCode::Down => {
                    self.exit_editing_keep_draft();
                    self.move_cursor(1);
                }
                KeyCode::Enter => {
                    self.confirm_editing();
                    self.advance();
                }
                _ => {}
            }
            return PanelAction::None;
        }

        match key.code {
            KeyCode::Up if self.on_confirm_page() => self.move_confirm_cursor(-1),
            KeyCode::Down if self.on_confirm_page() => self.move_confirm_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Left => self.switch_tab(-1),
            KeyCode::Right => self.switch_tab(1),
            KeyCode::Tab => self.toggle(),
            KeyCode::Char(c) if !ctrl && !alt => {
                if self.on_custom_row() {
                    // Typing on the free-form row starts the inline editor.
                    self.start_editing();
                    self.edit_insert_char(c);
                } else if c == ' ' {
                    self.toggle();
                }
            }
            KeyCode::Enter => return self.enter(),
            _ => {}
        }
        PanelAction::None
    }

    /// Enter on the current tab: commit+advance (option row), enter the
    /// editor (free-form row), or activate the confirm page. Submit is not
    /// gated — unanswered questions are sent with the placeholder; the
    /// confirm page has already warned about them.
    fn enter(&mut self) -> PanelAction {
        if self.on_confirm_page() {
            if self.confirm_cursor == 0 {
                self.finished = Some(PanelFinish::Submitted);
                PanelAction::Reply(self.build_response())
            } else {
                self.finished = Some(PanelFinish::Cancelled);
                PanelAction::Reply(ASK_CANCEL_CONTENT.to_string())
            }
        } else if self.on_custom_row() {
            self.start_editing();
            PanelAction::None
        } else {
            if !self.questions[self.current].multi_select {
                self.commit_option();
            }
            self.advance();
            PanelAction::None
        }
    }

    /// Move the confirm-page cursor (Submit/Cancel).
    fn move_confirm_cursor(&mut self, delta: isize) {
        self.confirm_cursor = wrap_index(self.confirm_cursor, delta, 2);
    }
}

/// AskPanel as a selection-panel adapter: each question is an options page
/// (options + the free-form row as its last cursor row); the confirm page is
/// an adapter-owned *custom* page — the kernel keeps its tab slot and window
/// position but holds no cursor for it.
impl SelectionPanel for AskPanel {
    fn page_count(&self) -> usize {
        self.questions.len() + 1
    }

    fn page_kind(&self, page: usize) -> PageKind {
        if page < self.questions.len() {
            PageKind::Options {
                rows: self.questions[page].options.len() + 1,
            }
        } else {
            PageKind::Custom
        }
    }

    fn current_page(&self) -> usize {
        self.current
    }

    fn set_current_page(&mut self, page: usize) {
        self.current = page;
    }

    fn cursor_at(&self, page: usize) -> usize {
        self.states.get(page).map_or(0, |st| st.cursor)
    }

    fn set_cursor_at(&mut self, page: usize, row: usize) {
        if let Some(st) = self.states.get_mut(page) {
            st.cursor = row;
        }
    }

    fn committed_at(&self, page: usize) -> Option<usize> {
        self.states.get(page).and_then(|st| st.selected)
    }

    fn set_committed_at(&mut self, page: usize, row: Option<usize>) {
        if let Some(st) = self.states.get_mut(page) {
            st.selected = row;
        }
    }
}

/// Normalize a question in place: legacy `choices` become options and the
/// cursor bounds stay consistent.
fn normalize_question(mut q: AskQuestion) -> AskQuestion {
    if q.options.is_empty() && !q.choices.is_empty() {
        q.options = q
            .choices
            .iter()
            .map(|c| crate::protocol::AskOption {
                label: c.clone(),
                description: String::new(),
            })
            .collect();
        q.choices.clear();
    }
    q
}

/// Byte offset of the `char_idx`-th char in `s` (clamped to s.len()).
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AskOption;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn option(label: &str) -> AskOption {
        AskOption {
            label: label.into(),
            description: String::new(),
        }
    }

    fn question(id: &str, header: &str, multi: bool, options: &[&str]) -> AskQuestion {
        AskQuestion {
            id: id.into(),
            header: header.into(),
            question: format!("{id}?"),
            multi_select: multi,
            options: options.iter().map(|o| option(o)).collect(),
            choices: vec![],
        }
    }

    /// Two-question panel: single-select + multi-select.
    fn two_question_panel() -> AskPanel {
        AskPanel::new(
            "tc-1".into(),
            vec![
                question("theme", "配色方案", false, &["浅色", "深色"]),
                question("features", "测试项", true, &["多选", "预览", "换行"]),
            ],
        )
    }

    #[test]
    fn cursor_wraps_but_browsing_never_answers() {
        let mut p = two_question_panel();
        assert_eq!(p.states[0].cursor, 0);
        p.handle_key(key(KeyCode::Down));
        assert_eq!(p.states[0].cursor, 1);
        p.handle_key(key(KeyCode::Down));
        assert_eq!(p.states[0].cursor, 2, "third row is the free-form row");
        p.handle_key(key(KeyCode::Down));
        assert_eq!(p.states[0].cursor, 0, "cursor wraps");
        // Pure navigation (cursor moves + tab switches) records no answer —
        // the tab must not turn green just because the user looked around.
        p.handle_key(key(KeyCode::Right));
        assert_eq!(p.current, 1);
        assert!(!p.is_answered(0), "browsing must not answer q1");
        assert_eq!(p.answer_value(0), None);
    }

    #[test]
    fn enter_commits_single_select_option_and_advances() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down)); // cursor → 深色
        p.handle_key(key(KeyCode::Enter)); // commit + advance
        assert_eq!(p.states[0].selected, Some(1));
        assert_eq!(p.answer_value(0).as_deref(), Some("深色"));
        assert!(p.is_answered(0));
        assert_eq!(p.current, 1);
    }

    #[test]
    fn committed_answer_survives_cursor_movement() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter)); // commit 深色
        p.handle_key(key(KeyCode::Left)); // back to q1
        p.handle_key(key(KeyCode::Up)); // cursor → 浅色 (row 0)
        assert_eq!(
            p.answer_value(0).as_deref(),
            Some("深色"),
            "committed answer is captured, not derived from the cursor"
        );
        // Re-commit overrides.
        p.handle_key(key(KeyCode::Enter));
        assert_eq!(p.answer_value(0).as_deref(), Some("浅色"));
    }

    #[test]
    fn custom_text_overrides_committed_option() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter)); // commit 深色, advance to q2
        p.handle_key(key(KeyCode::Left)); // back to q1 (cursor still row 1)
        p.handle_key(key(KeyCode::Down)); // → free-form row
        p.handle_key(ch('h'));
        p.handle_key(key(KeyCode::Enter)); // confirm text
        assert_eq!(p.answer_value(0).as_deref(), Some("h"));
        // Committing an option again overrides back.
        p.handle_key(key(KeyCode::Left));
        p.handle_key(key(KeyCode::Up));
        p.handle_key(key(KeyCode::Up)); // cursor → row 0
        p.handle_key(key(KeyCode::Enter));
        assert_eq!(p.answer_value(0).as_deref(), Some("浅色"));
    }

    #[test]
    fn multi_select_toggles_with_space_and_tab() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Right)); // to multi question
        assert_eq!(p.current, 1);
        p.handle_key(ch(' '));
        assert_eq!(p.answer_value(1).as_deref(), Some("多选"));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Tab));
        assert_eq!(p.answer_value(1).as_deref(), Some("多选, 预览"));
        p.handle_key(ch(' ')); // toggle the second option off again
        assert_eq!(p.answer_value(1).as_deref(), Some("多选"));
        assert_eq!(p.states[1].toggles, vec![true, false, false]);
    }

    #[test]
    fn tab_and_space_do_not_touch_single_select() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Tab));
        p.handle_key(ch(' '));
        assert_eq!(p.states[0].toggles, vec![false, false]);
        assert_eq!(p.states[0].selected, None);
        assert_eq!(p.answer_value(0), None);
    }

    #[test]
    fn typing_on_custom_row_starts_inline_editing() {
        let mut p = two_question_panel();
        // slide to the free-form row (last row)
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        assert!(p.on_custom_row());
        p.handle_key(ch('深'));
        p.handle_key(ch('色'));
        assert!(p.editing());
        assert_eq!(p.states[0].draft, "深色");
        p.handle_key(key(KeyCode::Enter)); // confirm + advance
        assert!(!p.editing());
        assert_eq!(p.current, 1);
        assert_eq!(p.answer_value(0).as_deref(), Some("深色"));
    }

    #[test]
    fn enter_on_empty_custom_row_enters_editor() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter));
        assert!(p.editing());
        assert_eq!(p.states[0].draft, "");
    }

    #[test]
    fn edit_cursor_movement_and_deletion() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter)); // start editor
        for c in ['a', 'b', 'c'] {
            p.handle_key(ch(c));
        }
        assert_eq!(p.states[0].draft, "abc");
        p.handle_key(key(KeyCode::Left));
        p.handle_key(ch('X'));
        assert_eq!(p.states[0].draft, "abXc");
        p.handle_key(key(KeyCode::Home));
        p.handle_key(key(KeyCode::Delete));
        assert_eq!(p.states[0].draft, "bXc");
        p.handle_key(key(KeyCode::End));
        p.handle_key(key(KeyCode::Backspace));
        assert_eq!(p.states[0].draft, "bX");
        p.handle_key(key(KeyCode::Left)); // editor owns ←, no tab switch
        assert_eq!(p.current, 0);
    }

    #[test]
    fn edit_up_down_keeps_draft_and_reopens_it() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter));
        p.handle_key(ch('h'));
        p.handle_key(ch('i'));
        p.handle_key(key(KeyCode::Up)); // leave editor, keep draft
        assert!(!p.editing());
        assert_eq!(p.states[0].cursor, 1);
        assert_eq!(p.states[0].draft, "hi");
        p.handle_key(key(KeyCode::Down)); // back to custom row
        p.handle_key(key(KeyCode::Enter));
        assert!(p.editing());
        assert_eq!(p.states[0].draft, "hi");
    }

    #[test]
    fn leaving_a_question_never_auto_commits_a_draft() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down)); // custom row
        p.handle_key(ch('x')); // uncommitted draft
        p.handle_key(key(KeyCode::Up)); // leave the editor, keep the draft
        p.handle_key(key(KeyCode::Right)); // leave the question
        assert_eq!(p.current, 1);
        assert_eq!(
            p.answer_value(0),
            None,
            "a draft without Enter is no answer"
        );
        assert_eq!(p.states[0].draft, "x", "the draft itself is preserved");
    }

    #[test]
    fn build_response_lines_with_headers_and_multi() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down)); // 深色
        p.handle_key(key(KeyCode::Enter)); // Q2
        p.handle_key(ch(' ')); // 多选 on
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Tab)); // 预览 on
        p.handle_key(key(KeyCode::Right)); // leave Q2 → confirm page
        assert_eq!(p.build_response(), "配色方案: 深色\n测试项: 多选, 预览");
    }

    #[test]
    fn build_response_appends_custom_text_for_multi() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Right)); // browse to Q2 — Q1 left unanswered
        p.handle_key(ch(' ')); // 多选 on
        for _ in 0..3 {
            p.handle_key(key(KeyCode::Down)); // slide to the free-form row
        }
        for c in ['X', 'Y'] {
            p.handle_key(ch(c));
        }
        p.handle_key(key(KeyCode::Enter)); // confirm, advance to confirm page
        assert_eq!(
            p.build_response(),
            "配色方案: (user did not answer)\n测试项: 多选, XY"
        );
        assert!(
            !p.is_answered(0),
            "browsed-but-uncommitted Q1 stays unanswered"
        );
        assert!(p.is_answered(1));
    }

    #[test]
    fn submit_is_not_gated_and_uses_placeholder_for_unanswered() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Enter)); // commit 浅色 → Q2
        assert!(p.is_answered(0));
        p.handle_key(key(KeyCode::Right)); // Q2 (unanswered) → confirm page
        assert_eq!(p.current, 2);
        assert_eq!(
            p.unanswered_headers(),
            vec!["测试项"],
            "confirm-page hint lists the unanswered question"
        );
        // Submit goes through — no bounce.
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(
            action,
            PanelAction::Reply("配色方案: 浅色\n测试项: (user did not answer)".into())
        );
        assert_eq!(p.finished, Some(PanelFinish::Submitted));
    }

    #[test]
    fn multi_toggle_back_to_none_is_unanswered() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Right));
        p.handle_key(ch(' ')); // on
        p.handle_key(ch(' ')); // off
        assert_eq!(p.answer_value(1), None);
    }

    #[test]
    fn cancel_from_confirm_page_sends_sentinel() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Left)); // wrap to confirm page
        assert_eq!(p.current, 2);
        p.handle_key(key(KeyCode::Down)); // Cancel row
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(action, PanelAction::Reply(ASK_CANCEL_CONTENT.to_string()));
        assert_eq!(p.finished, Some(PanelFinish::Cancelled));
    }

    #[test]
    fn finished_panel_ignores_further_keys() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Left));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter)); // cancel
        assert!(p.finished.is_some());
        assert_eq!(p.handle_key(key(KeyCode::Up)), PanelAction::None);
        assert_eq!(p.handle_key(key(KeyCode::Enter)), PanelAction::None);
    }

    #[test]
    fn switch_tab_wraps_including_confirm_page() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Left));
        assert_eq!(p.current, 2, "← from first question wraps to confirm");
        p.handle_key(key(KeyCode::Right));
        assert_eq!(p.current, 0);
        p.handle_key(key(KeyCode::Right));
        assert_eq!(p.current, 1);
    }

    #[test]
    fn confirm_page_is_a_custom_page_in_navigation_and_window() {
        use crate::app::selection_panel::PANEL_WINDOW;
        use crate::app::selection_panel::window_range;

        let questions: Vec<AskQuestion> = (0..6)
            .map(|i| question(&format!("q{i}"), &format!("Q{i}"), false, &["a", "b"]))
            .collect();
        let mut p = AskPanel::new("tc".into(), questions);
        let confirm = 6;

        // The confirm page is a tab like any other — but adapter-owned.
        assert_eq!(p.page_count(), confirm + 1, "custom page counts as a tab");
        assert_eq!(p.page_kind(confirm), PageKind::Custom);
        assert_eq!(
            p.page_kind(0),
            PageKind::Options { rows: 3 },
            "2 options + the free-form row"
        );

        // ← from the first question wraps onto the confirm page.
        p.move_page(-1);
        assert_eq!(p.current_page(), confirm);
        // Kernel cursor operations are no-ops on the custom page.
        p.move_cursor(1);
        assert_eq!(p.cursor_at(confirm), 0);
        p.move_cursor(-1);
        assert_eq!(p.cursor_at(confirm), 0);
        // → wraps back to the first question.
        p.move_page(1);
        assert_eq!(p.current_page(), 0);
        // Per-question cursor memory survives the round trip.
        p.set_cursor_at(0, 1);
        p.move_page(-1);
        p.move_page(1);
        assert_eq!(p.cursor_at(0), 1);

        // The tab window covers custom pages: standing on confirm, the window
        // slides to keep it visible; standing on the last question it is one
        // `›` away (hidden right).
        let range = window_range(confirm, p.page_count(), PANEL_WINDOW);
        assert_eq!(range, 2..7, "window slides to keep the confirm tab visible");
        assert!(range.contains(&confirm));
        let range = window_range(5, p.page_count(), PANEL_WINDOW);
        assert_eq!(range, 1..6);
        assert!(!range.contains(&confirm), "hidden behind the › marker");
    }

    #[test]
    fn legacy_choices_are_normalized_into_options() {
        let mut q = question("q1", "", false, &[]);
        q.choices = vec!["y".into(), "n".into()];
        let mut p = AskPanel::new("tc".into(), vec![q]);
        assert_eq!(p.questions[0].options.len(), 2);
        assert!(p.questions[0].choices.is_empty());
        assert_eq!(p.questions[0].tab_label(), "q1", "header falls back to id");
        assert_eq!(p.answer_value(0), None, "no implicit answer before Enter");
        p.handle_key(key(KeyCode::Enter)); // commit first option
        assert_eq!(p.answer_value(0).as_deref(), Some("y"));
    }

    #[test]
    fn free_form_only_question_answers_via_custom_row() {
        let mut p = AskPanel::new("tc".into(), vec![question("chat", "聊天", false, &[])]);
        // No options → the only row is the free-form row; typing works directly.
        p.handle_key(ch('h'));
        p.handle_key(ch('i'));
        p.handle_key(key(KeyCode::Enter)); // confirm + advance to confirm page
        assert_eq!(p.current, 1);
        assert_eq!(p.answer_value(0).as_deref(), Some("hi"));
        let action = p.handle_key(key(KeyCode::Enter)); // Submit
        assert_eq!(action, PanelAction::Reply("聊天: hi".into()));
    }

    #[test]
    fn paste_inserts_at_the_editor_cursor() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter));
        p.handle_key(ch('a'));
        p.insert_paste("b\nc");
        assert_eq!(p.states[0].draft, "ab c");
        p.handle_key(key(KeyCode::Left));
        p.insert_paste("X");
        assert_eq!(p.states[0].draft, "ab Xc");
    }

    #[test]
    fn paste_outside_custom_row_is_ignored() {
        let mut p = two_question_panel();
        p.insert_paste("hello");
        assert_eq!(p.states[0].draft, "");
        assert!(!p.states[0].editing);
    }

    #[test]
    fn clearing_custom_text_clears_the_answer() {
        let mut p = two_question_panel();
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Down));
        p.handle_key(ch('x'));
        p.handle_key(key(KeyCode::Enter)); // confirm + advance
        assert_eq!(p.answer_value(0).as_deref(), Some("x"));
        // Revisit; the cursor is still on the free-form row.
        p.handle_key(key(KeyCode::Left));
        assert_eq!(p.current, 0);
        p.handle_key(key(KeyCode::Enter)); // re-enter the editor (draft kept)
        p.handle_key(key(KeyCode::Backspace));
        p.handle_key(key(KeyCode::Enter)); // confirm empty + advance
        assert_eq!(p.answer_value(0), None);
    }
}
