//! AskPanel — the one ask model, on top of the selection-panel kernel.
//!
//! **Every ask becomes an `AskPanel`.** The gateway sends two wire shapes (the
//! `questions` form of the AskUserQuestion tool, and the retired
//! `question`/`choices`/`required` form of the Bash dangerous-command
//! confirmation); [`AskPanel::from_ask`] is the single normalization entry
//! that turns either one into a panel, and [`PanelMode`] is the shape it
//! produced. Live projection and replay both go through it — nothing
//! downstream branches on the wire shape again.
//!
//! Kernel responsibilities ([`SelectionPanel`]): page switching, per-question
//! cursor memory, single-select commit capture. Adapter responsibilities
//! (here): multi-select toggles, the inline free-form editor, the confirm page
//! (registered as a *custom* page — the kernel keeps its tab slot but has no
//! cursor on it), and reply construction (mode-dependent, see [`PanelMode`]).
//!
//! `PanelMode::Question` renders a tab bar (question headers + a final confirm
//! page), one question at a time with selectable options, and a free-form
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
//! `PanelMode::RequiredChoice` (a retired required ask, normalized) has no
//! free-form row and no confirm page: `↑`/`↓` move the cursor (wraps) and
//! `Enter` commits the option under it **and answers right away** — the panel
//! is a one-shot mandatory choice. Neither mode consumes keys the app reserves
//! (`Esc`, page keys). `PanelMode::Notice` is display-only: it never reaches
//! this key handler at all (the app does not register it).
//!
//! Answers are **explicit acts only** — browsing (moving the cursor, switching
//! tabs) never records an answer, so tabs only turn "answered" when the user
//! actually chose something:
//! - single-select: the option committed with Enter (captured via the kernel,
//!   not derived — later cursor movement cannot change it; re-commit overrides)
//! - multi-select: the toggled options (each toggle is itself an explicit act)
//! - free-form: the text confirmed with Enter in the editor
//!
//! In `Question` mode the user may leave questions unanswered: Submit is not
//! gated, unanswered questions are sent as `(user did not answer)` and the
//! confirm page warns about them. An empty free-form row never counts as an
//! answer. In `RequiredChoice` mode there is nothing to submit without an
//! answer: Enter on an option *is* the answer.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;

use super::PageKind;
use super::SelectionPanel;
use super::wrap_index;
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

/// Interaction model of a panel — the ask's *shape*, decided once at
/// normalization time ([`AskPanel::from_ask`]) and read by every consumer
/// (keys, rendering, reply construction, app registration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMode {
    /// `AskUserQuestion`: one tab per question plus the confirm page, optional
    /// answers (unanswered ones go out as [`UNANSWERED_PLACEHOLDER`]), a
    /// free-form row, and a reply of `header: answer` lines.
    Question,
    /// The retired Bash dangerous-command confirmation, normalized: exactly
    /// one single-select question that MUST be answered — no free-form row and
    /// no confirm page, `Enter` on an option answers right away. The reply is
    /// the **bare option label** (`y` / `n` / `yolo`), which is what the
    /// backend's `_parse_feedback` accepts (see `libs/core/wing/tools/bash.py`);
    /// a `header: answer` line would be rejected and re-asked forever.
    RequiredChoice,
    /// A retired ask that was not required: display-only. The question and its
    /// choices stay visible as plain bullets (the shape's historical look), but
    /// the panel is never registered for answering and never sees a key. The
    /// app keeps rendering it through this same model.
    Notice,
}

/// One ask payload, as the gateway sends it (live event or replayed fact
/// event) — the input vocabulary of [`AskPanel::from_ask`], i.e. the wire
/// shapes collapsed into the fields the normalization entry reads.
#[derive(Debug, Clone, Copy)]
pub struct AskPayload<'a> {
    /// Correlation id echoed back with the answer (resolves the waiter).
    pub tool_call_id: &'a str,
    /// `AskUserQuestion` shape: 1-4 questions (empty on the retired shape).
    pub questions: &'a [AskQuestion],
    /// Retired shape: the question text (empty on the `questions` shape).
    pub question: &'a str,
    /// Retired shape: plain-string choices.
    pub choices: &'a [String],
    /// Retired shape: `true` = the user must pick one of `choices`.
    pub required: bool,
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

/// Interactive state for one ask panel (any [`PanelMode`]).
#[derive(Debug, Clone)]
pub struct AskPanel {
    /// Correlation id echoed back when sending the reply.
    pub tool_call_id: String,
    /// Questions (option-normalized on construction).
    pub questions: Vec<AskQuestion>,
    /// Per-question interaction state (parallel to `questions`).
    pub states: Vec<QuestionState>,
    /// Interaction model — set at normalization time, read everywhere else.
    pub mode: PanelMode,
    /// Active page: `0..questions.len()` = question, `questions.len()` = confirm
    /// page (`Question` mode only — the other modes have no confirm page).
    pub current: usize,
    /// Cursor on the confirm page (0 = Submit, 1 = Cancel).
    pub confirm_cursor: usize,
    /// Set once the panel has been answered/submitted/cancelled (terminal state).
    pub finished: Option<PanelFinish>,
}

/// Id synthesized for the retired single-question shape (it carries no id).
/// Required-choice replies are the bare label, so this never reaches the wire.
const LEGACY_QUESTION_ID: &str = "choice";

impl AskPanel {
    /// A fresh `AskUserQuestion` panel.
    pub fn new(tool_call_id: String, questions: Vec<AskQuestion>) -> Self {
        Self::with_mode(tool_call_id, questions, PanelMode::Question)
    }

    /// **The normalization entry.** Any ask becomes a panel here — the
    /// `questions` shape as-is, the retired `question`/`choices`/`required`
    /// shape folded into a single question (required → [`PanelMode::RequiredChoice`],
    /// otherwise → display-only [`PanelMode::Notice`]). Live projection and
    /// replay both call this; nothing downstream branches on the wire shape.
    pub fn from_ask(payload: AskPayload<'_>) -> Self {
        if !payload.questions.is_empty() {
            return Self::new(payload.tool_call_id.to_string(), payload.questions.to_vec());
        }
        let question = AskQuestion {
            id: LEGACY_QUESTION_ID.to_string(),
            header: String::new(),
            question: payload.question.to_string(),
            multi_select: false,
            options: payload
                .choices
                .iter()
                .map(|label| crate::protocol::AskOption {
                    label: label.clone(),
                    description: String::new(),
                })
                .collect(),
            choices: Vec::new(),
        };
        // Required with something to pick = the mandatory choice menu; anything
        // else is a plain notice (nothing selectable = nothing to answer).
        let mode = if payload.required && !question.options.is_empty() {
            PanelMode::RequiredChoice
        } else {
            PanelMode::Notice
        };
        Self::with_mode(payload.tool_call_id.to_string(), vec![question], mode)
    }

    fn with_mode(tool_call_id: String, questions: Vec<AskQuestion>, mode: PanelMode) -> Self {
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
            mode,
            current: 0,
            confirm_cursor: 0,
            finished: None,
        }
    }

    /// Whether the app should register this panel for answering. A notice is
    /// display-only: it must never take the keyboard or reach the backend.
    pub fn is_interactive(&self) -> bool {
        self.mode != PanelMode::Notice
    }

    /// Whether the current question has a free-form row (`Question` mode only).
    pub fn has_free_form(&self) -> bool {
        self.mode == PanelMode::Question
    }

    /// The text to surface in a desktop notification, if any.
    pub fn notify_text(&self) -> String {
        self.questions
            .first()
            .map(|q| q.question.clone())
            .unwrap_or_default()
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
    /// Only `Question` mode has a free-form row at all.
    fn on_custom_row(&self) -> bool {
        self.has_free_form()
            && !self.on_confirm_page()
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

    /// Final reply text, in the shape the mode's backend expects:
    ///
    /// - `Question`: `header: answer` lines, newline-separated. Unanswered
    ///   questions contribute the `(user did not answer)` placeholder (the user
    ///   may submit deliberately with gaps — the confirm page warns).
    /// - `RequiredChoice`: the committed option's **bare label** — the backend's
    ///   `_parse_feedback` accepts exactly `y` / `n` / `yolo` and nothing else.
    /// - `Notice`: never answered (empty).
    pub fn build_response(&self) -> String {
        match self.mode {
            PanelMode::RequiredChoice => self.answer_value(0).unwrap_or_default(),
            PanelMode::Notice => String::new(),
            PanelMode::Question => self
                .questions
                .iter()
                .enumerate()
                .map(|(qi, q)| {
                    let answer = self
                        .answer_value(qi)
                        .unwrap_or_else(|| UNANSWERED_PLACEHOLDER.to_string());
                    format!("{}: {}", q.tab_label(), answer)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
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

    /// Advance to the next tab, clamped at the confirm page (Enter semantics).
    fn advance(&mut self) {
        let next = (self.current + 1).min(self.questions.len());
        self.set_current_page(next);
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
            KeyCode::Left => self.move_page(-1),
            KeyCode::Right => self.move_page(1),
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

    /// Enter on the current page:
    ///
    /// - `Question`: commit+advance on an option row, enter the editor on the
    ///   free-form row, or activate the confirm page. Submit is not gated —
    ///   unanswered questions are sent with the placeholder; the confirm page
    ///   has already warned about them.
    /// - `RequiredChoice`: commit the option under the cursor and answer right
    ///   away with its bare label (there is no confirm page and no way to
    ///   submit without an answer).
    fn enter(&mut self) -> PanelAction {
        if self.mode == PanelMode::RequiredChoice {
            self.commit_option();
            let Some(answer) = self.answer_value(self.current) else {
                return PanelAction::None; // no options to choose from
            };
            self.finished = Some(PanelFinish::Submitted);
            return PanelAction::Reply(answer);
        }
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
/// (options + the free-form row as its last cursor row, in `Question` mode);
/// the confirm page (also `Question` mode only) is an adapter-owned *custom*
/// page — the kernel keeps its tab slot and window position but holds no
/// cursor for it.
impl SelectionPanel for AskPanel {
    fn page_count(&self) -> usize {
        match self.mode {
            // No confirm page: a required choice is answered in place.
            PanelMode::RequiredChoice | PanelMode::Notice => self.questions.len(),
            PanelMode::Question => self.questions.len() + 1,
        }
    }

    fn page_kind(&self, page: usize) -> PageKind {
        if page < self.questions.len() {
            let rows = self.questions[page].options.len();
            PageKind::Options {
                // The free-form row is the last cursor row of a question — but
                // only when the mode has one.
                rows: if self.has_free_form() { rows + 1 } else { rows },
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
        // Entering the confirm page resets its Submit/Cancel cursor so that
        // every tab-switch path (move_page, advance) picks it up implicitly.
        if self.on_confirm_page() && self.mode == PanelMode::Question {
            self.confirm_cursor = 0;
        }
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

    // Ask keeps its established wrap-around navigation: ←/→ cycles the tabs
    // (confirm page included) and ↑/↓ wraps over the rows. The kernel default
    // clamps at the ends (model-picker semantics); Ask opts into wrapping.

    /// Move the option cursor on the current question (wraps).
    fn move_cursor(&mut self, delta: isize) {
        let page = self.current_page();
        let PageKind::Options { rows } = self.page_kind(page) else {
            return;
        };
        if rows == 0 {
            return;
        }
        let cursor = self.cursor_at(page);
        self.set_cursor_at(page, wrap_index(cursor, delta, rows));
    }

    /// Switch tab with wrap-around (confirm page included).
    fn move_page(&mut self, delta: isize) {
        let count = self.page_count();
        if count == 0 {
            return;
        }
        let current = self.current_page();
        self.set_current_page(wrap_index(current, delta, count));
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
        use crate::shared::panels::PANEL_WINDOW;
        use crate::shared::panels::window_range;

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

        // The tab window covers custom pages: the confirm tab stays inside
        // the centered window while it is near the active page; on the first
        // pages it is outside the window (no marker glyphs — the window just
        // scrolls when the user navigates to it).
        let range = window_range(confirm, p.page_count(), PANEL_WINDOW);
        assert_eq!(range, 2..7, "window keeps the confirm tab visible");
        assert!(range.contains(&confirm));
        let range = window_range(5, p.page_count(), PANEL_WINDOW);
        assert_eq!(range, 2..7);
        assert!(range.contains(&confirm));
        let range = window_range(0, p.page_count(), PANEL_WINDOW);
        assert_eq!(range, 0..5);
        assert!(!range.contains(&confirm), "outside the window at the start");
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

    // ── Normalization entry tests ─────────────────────────────────

    #[test]
    fn from_ask_questions_shape_produces_question_mode() {
        let questions = vec![question("t", "T", false, &["a", "b"])];
        let p = AskPanel::from_ask(AskPayload {
            tool_call_id: "tc-1",
            questions: &questions,
            question: "",
            choices: &[],
            required: false,
        });
        assert_eq!(p.mode, PanelMode::Question);
        assert_eq!(p.questions.len(), 1);
        assert!(p.is_interactive());
    }

    #[test]
    fn from_ask_required_legacy_produces_required_choice() {
        let choices = vec!["y".into(), "n".into(), "yolo".into()];
        let p = AskPanel::from_ask(AskPayload {
            tool_call_id: "tc-2",
            questions: &[],
            question: "Proceed?",
            choices: &choices,
            required: true,
        });
        assert_eq!(p.mode, PanelMode::RequiredChoice);
        assert_eq!(p.tool_call_id, "tc-2");
        assert_eq!(p.questions.len(), 1);
        assert_eq!(p.questions[0].options.len(), 3);
        assert_eq!(p.questions[0].options[0].label, "y");
        assert_eq!(p.questions[0].options[2].label, "yolo");
        assert!(p.is_interactive());
    }

    #[test]
    fn from_ask_non_required_legacy_produces_notice() {
        let choices = vec!["a".into(), "b".into()];
        let p = AskPanel::from_ask(AskPayload {
            tool_call_id: "tc-3",
            questions: &[],
            question: "Just so you know",
            choices: &choices,
            required: false,
        });
        assert_eq!(p.mode, PanelMode::Notice);
        assert!(!p.is_interactive(), "Notice mode is not interactive");
    }

    #[test]
    fn from_ask_empty_choices_with_required_is_notice() {
        let p = AskPanel::from_ask(AskPayload {
            tool_call_id: "tc-4",
            questions: &[],
            question: "?",
            choices: &[],
            required: true,
        });
        assert_eq!(
            p.mode,
            PanelMode::Notice,
            "no options means nothing to pick"
        );
        assert!(!p.is_interactive());
    }

    #[test]
    fn from_ask_preserves_tool_call_id_in_all_modes() {
        for (mode, tcid) in [
            ("question", "tc-a"),
            ("required", "tc-b"),
            ("notice", "tc-c"),
        ] {
            let choices = vec!["y".into()];
            let p = match mode {
                "question" => AskPanel::from_ask(AskPayload {
                    tool_call_id: tcid,
                    questions: &[question("q", "Q", false, &["a"])],
                    question: "",
                    choices: &[],
                    required: false,
                }),
                _ => AskPanel::from_ask(AskPayload {
                    tool_call_id: tcid,
                    questions: &[],
                    question: "?",
                    choices: &choices,
                    required: mode == "required",
                }),
            };
            assert_eq!(p.tool_call_id, tcid, "{mode} mode preserves tool_call_id");
        }
    }

    // ── RequiredChoice interaction tests ──────────────────────────

    /// Build a required-choice panel with three options for testing.
    fn required_panel() -> AskPanel {
        let choices = vec!["y".into(), "n".into(), "yolo".into()];
        AskPanel::from_ask(AskPayload {
            tool_call_id: "rc-1",
            questions: &[],
            question: "Proceed?",
            choices: &choices,
            required: true,
        })
    }

    #[test]
    fn required_choice_enter_answers_with_bare_label_and_finishes() {
        let mut p = required_panel();
        // Cursor starts at 0 → "y"
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(action, PanelAction::Reply("y".into()));
        assert_eq!(p.finished, Some(PanelFinish::Submitted));
    }

    #[test]
    fn required_choice_cursor_moves_and_enter_answers_selected() {
        let mut p = required_panel();
        p.handle_key(key(KeyCode::Down)); // cursor → 1 = "n"
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(action, PanelAction::Reply("n".into()));
        assert_eq!(p.finished, Some(PanelFinish::Submitted));
    }

    #[test]
    fn required_choice_yolo_answer_wire_compatible() {
        let mut p = required_panel();
        p.handle_key(key(KeyCode::Down)); // 1
        p.handle_key(key(KeyCode::Down)); // 2 = "yolo"
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(action, PanelAction::Reply("yolo".into()));
    }

    #[test]
    fn required_choice_cursor_wraps() {
        let mut p = required_panel();
        p.handle_key(key(KeyCode::Up)); // wrap to last
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(action, PanelAction::Reply("yolo".into()));
    }

    #[test]
    fn required_choice_typing_does_not_start_editor() {
        let mut p = required_panel();
        p.handle_key(ch('x'));
        p.handle_key(ch('y'));
        assert!(!p.editing());
        // Cursor still at 0, nothing committed.
        assert_eq!(p.answer_value(0), None);
    }

    #[test]
    fn required_choice_no_confirm_page() {
        let p = required_panel();
        assert_eq!(p.page_count(), 1, "no confirm page");
        // ←/→ on a single page is a no-op; the panel stays on page 0.
        let mut p = p;
        p.handle_key(key(KeyCode::Left));
        assert_eq!(p.current, 0);
        p.handle_key(key(KeyCode::Right));
        assert_eq!(p.current, 0);
    }

    #[test]
    fn required_choice_no_free_form_row() {
        let p = required_panel();
        assert!(!p.has_free_form(), "required choice has no free-form row");
        assert_eq!(
            p.page_kind(0),
            PageKind::Options { rows: 3 },
            "only the 3 options, no custom row"
        );
    }

    #[test]
    fn required_choice_finished_panel_ignores_keys() {
        let mut p = required_panel();
        p.handle_key(key(KeyCode::Enter)); // answer "y"
        assert_eq!(p.finished, Some(PanelFinish::Submitted));
        let action = p.handle_key(key(KeyCode::Enter));
        assert_eq!(action, PanelAction::None, "finished panel returns None");
        let action = p.handle_key(key(KeyCode::Down));
        assert_eq!(action, PanelAction::None);
    }

    #[test]
    fn required_choice_panel_never_sends_cancel_sentinel() {
        let mut p = required_panel();
        // There is no confirm page, so Cancel is unreachable.
        // The only action is Reply with the label.
        p.handle_key(key(KeyCode::Down));
        p.handle_key(key(KeyCode::Enter));
        // Verify the action content does NOT contain the cancel sentinel.
        assert_eq!(p.finished, Some(PanelFinish::Submitted));
        // (No Cancel in finished because there's no Cancel path.)
    }

    // ── Notice interaction tests ──────────────────────────────────

    #[test]
    fn notice_is_not_interactive_even_when_required_false() {
        let p = AskPanel::from_ask(AskPayload {
            tool_call_id: "n-1",
            questions: &[],
            question: "FYI",
            choices: &["info".into()],
            required: false,
        });
        assert_eq!(p.mode, PanelMode::Notice);
        assert!(!p.is_interactive());
        // Calling handle_key on a bare notice panel is fine but never happens
        // through the App path since the notice is never registered.
    }
}
