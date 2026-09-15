//! AskMessage — renders agent questions to the user.
//!
//! One model ([`AskPanel`]), one renderer ([`to_lines`]), dispatching by
//! [`PanelMode`]:
//!
//! - `Question` — the AskUserQuestion panel (tab bar + confirm page, one
//!   question at a time, single/multi-select, a free-form row with an inline
//!   editor, and a footer with the key hints).
//! - `RequiredChoice` — the retired Bash confirmation, normalized: question
//!   markdown + selectable option rows (no free-form row, no confirm page, no
//!   tab bar) + footer. `Enter` on an option answers right away.
//! - `Notice` — a retired non-required ask: static display with the legacy
//!   `? ` marker + dim bullets. Not interactive.
//!
//! No synthetic identifiers are ever added to options: labels render verbatim
//! and a chosen option is sent back verbatim.

use crate::config::ThemePalette;
use crate::render::markdown::render_markdown_with_width;
use crate::shared::panels::PANEL_WINDOW;
use crate::shared::panels::ask::AskPanel;
use crate::shared::panels::ask::PanelFinish;
use crate::shared::panels::ask::PanelMode;
use crate::shared::panels::ask::QuestionState;
use crate::shared::panels::ask::UNANSWERED_PLACEHOLDER;
use crate::shared::panels::window_range;
use crate::ui::panel::Tab;
use crate::ui::panel::TabState;
use crate::ui::panel::cursor_span;
use crate::ui::panel::footer;
use crate::ui::panel::label_span;
use crate::ui::panel::label_style;
use crate::ui::panel::tab_bar;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// Description indent under option rows: cursor(2) + number(3) + marker(4)
/// columns — the label always starts at column 9.
const OPTION_DESC_INDENT: usize = 9;
/// Description indent under marker-less rows (free-form / confirm rows):
/// cursor(2) + number(3) columns.
const ROW_DESC_INDENT: usize = 5;

/// An agent question cell — always holds a normalized [`AskPanel`].
#[derive(Debug, Clone)]
pub struct AskMessage {
    /// The normalized panel (render snapshot; interactive state is owned by
    /// the App and mirrored back through [`ChatView::update_ask_panel`]).
    pub panel: AskPanel,
}

impl AskMessage {
    /// Wrap a normalized ask panel into a chat cell.
    pub fn new(panel: AskPanel) -> Self {
        Self { panel }
    }

    /// Render to lines.
    pub fn to_lines(&self, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
        match self.panel.mode {
            PanelMode::Notice => notice_lines(&self.panel, palette, width),
            _ => panel_lines(&self.panel, palette, width),
        }
    }
}

// ── Panel rendering (Question + RequiredChoice) ──────────────────

/// Render an interactive panel: tab bar (`Question` only), current page,
/// or finished summary + footer. See [`PanelMode`] for mode-specific chrome.
fn panel_lines(panel: &AskPanel, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Tab bar: visible only for the multi-question form (it has navigation
    // targets). A required choice has one question and no confirm page.
    if panel.mode == PanelMode::Question {
        lines.push(ask_tab_bar(panel, palette));
        lines.push(Line::from(""));
    }

    match panel.finished {
        Some(PanelFinish::Submitted) => {
            let success = Style::default().fg(palette.success);
            for line in panel.build_response().lines() {
                lines.push(Line::from(vec![
                    Span::styled("✓ ", success),
                    Span::styled(line.to_string(), Style::default().fg(palette.dim)),
                ]));
            }
        }
        Some(PanelFinish::Cancelled) => {
            lines.push(Line::from(Span::styled(
                "✗ Cancelled — the agent was told you cancelled.",
                Style::default().fg(palette.danger),
            )));
        }
        None => {
            if panel.on_confirm_page() {
                // Confirm page: only on Question mode (required choice skips it).
                lines.push(Line::from(Span::styled(
                    "Submit your answers?",
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(""));
                confirm_rows(panel, palette, width, &mut lines);
            } else {
                let qi = panel.current;
                let q = &panel.questions[qi];
                let md_width = Some(width.saturating_sub(2));
                lines.extend(render_markdown_with_width(&q.question, md_width, palette));
                if q.multi_select {
                    lines.push(Line::from(Span::styled(
                        "(Select all that apply)",
                        Style::default().fg(palette.dim),
                    )));
                }
                lines.push(Line::from(""));
                option_rows(panel, palette, width, &mut lines);
            }
            lines.push(Line::from(""));
            lines.push(footer(footer_hint(panel), palette));
        }
    }
    lines.push(Line::from(""));
    lines
}

// ── Notice (static, non-interactive) rendering ───────────────────

/// Retired non-interactive ask: kept as-is — question markdown with the
/// legacy `? ` marker and dim `• choice` bullets. No cursor, no footer.
fn notice_lines(panel: &AskPanel, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
    let accent_style = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);

    // Render question body as markdown with a `? ` marker on the first line.
    let question = &panel.questions[0].question;
    let md_width = Some(width.saturating_sub(2));
    let mut md_lines = render_markdown_with_width(question, md_width, palette);
    if let Some(first) = md_lines.first_mut() {
        let marker = Span::styled("?", accent_style);
        let mut spans = vec![marker, Span::raw(" ")];
        spans.append(&mut first.spans);
        *first = Line::from(spans);
    } else {
        md_lines.push(Line::from(vec![Span::styled("?", accent_style)]));
    }

    // Choices: plain dim suggestions. Deliberately NO synthetic A/B/C
    // letters — the payload never carried them and they must not appear.
    let dim_style = Style::default().fg(palette.dim);
    for option in &panel.questions[0].options {
        md_lines.push(Line::from(vec![
            Span::styled("  • ", dim_style),
            Span::styled(option.label.clone(), dim_style),
        ]));
    }

    md_lines.push(Line::from(""));
    md_lines
}

// ── Tab bar ──────────────────────────────────────────────────────

/// `? 配色方案 > 测试项 > Submit` — question tabs + confirm page, windowed.
/// Only rendered in `Question` mode.
fn ask_tab_bar(panel: &AskPanel, palette: &ThemePalette) -> Line<'static> {
    let mut tabs: Vec<Tab> = panel
        .questions
        .iter()
        .enumerate()
        .map(|(i, q)| Tab {
            label: q.tab_label(),
            state: if panel.finished.is_some() {
                TabState::Normal
            } else if i == panel.current {
                TabState::Active
            } else if panel.is_answered(i) {
                TabState::Marked
            } else {
                TabState::Normal
            },
        })
        .collect();
    tabs.push(Tab {
        label: "Submit",
        state: if panel.finished.is_some() {
            TabState::Normal
        } else if panel.on_confirm_page() {
            TabState::Active
        } else if panel.all_answered() {
            TabState::Marked
        } else {
            TabState::Normal
        },
    });
    tab_bar("? ", &tabs, panel.current, palette)
}

// ── Option rows ──────────────────────────────────────────────────

/// Option rows of the current question, windowed (the cursor row stays
/// centered while scrolling). In `RequiredChoice` mode there is no free-form
/// row — the rows are only the options.
fn option_rows(
    panel: &AskPanel,
    palette: &ThemePalette,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let qi = panel.current;
    let q = &panel.questions[qi];
    let st = &panel.states[qi];
    let rows = if panel.has_free_form() {
        q.options.len() + 1
    } else {
        q.options.len()
    };
    let range = window_range(st.cursor, rows, PANEL_WINDOW);

    for i in range {
        let is_cursor = st.cursor == i;
        if i < q.options.len() {
            let option = &q.options[i];
            let mut spans = vec![
                cursor_span(is_cursor, palette),
                number_span(i + 1, is_cursor, palette),
            ];
            if q.multi_select {
                spans.push(checkbox_span(st.toggles[i], palette));
            } else {
                spans.push(radio_span(st.selected == Some(i), palette));
            }
            spans.push(label_span(&option.label, is_cursor, palette));
            lines.push(Line::from(spans));
            push_description(
                &option.description,
                palette,
                width,
                lines,
                OPTION_DESC_INDENT,
            );
        } else {
            custom_row(st, q.options.len(), palette, width, lines);
        }
    }
}

/// The free-form row (always the last row of a question): placeholder,
/// committed text, kept draft, or the inline editor.
fn custom_row(
    st: &QuestionState,
    custom_idx: usize,
    palette: &ThemePalette,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let is_cursor = st.cursor == custom_idx;
    let mut spans = vec![
        cursor_span(is_cursor, palette),
        number_span(custom_idx + 1, is_cursor, palette),
    ];
    if st.editing {
        // Reserve the cursor/number columns plus the label prefix — editor
        // text must not push the row beyond the panel width.
        let used = 2 + 3 + "Type Something: ".chars().count();
        spans.push(Span::styled(
            "Type Something: ",
            label_style(is_cursor, palette),
        ));
        let budget = (width as usize).saturating_sub(used);
        spans.extend(edit_spans(&st.draft, st.edit_cursor, palette, budget));
        lines.push(Line::from(spans));
    } else if let Some(text) = st.custom.as_deref() {
        spans.push(Span::styled(
            "Type Something: ",
            label_style(is_cursor, palette),
        ));
        spans.push(Span::styled(
            text.to_string(),
            label_style(is_cursor, palette),
        ));
        lines.push(Line::from(spans));
    } else if !st.draft.trim().is_empty() {
        // Kept draft (editor left via ↑/↓) — uncommitted but visible.
        spans.push(Span::styled(
            "Type Something: ",
            label_style(is_cursor, palette),
        ));
        spans.push(Span::styled(
            st.draft.clone(),
            Style::default().fg(palette.dim),
        ));
        lines.push(Line::from(spans));
    } else {
        spans.push(Span::styled(
            "Type Something",
            label_style(is_cursor, palette),
        ));
        lines.push(Line::from(spans));
        push_description(
            "Enter a custom response",
            palette,
            width,
            lines,
            ROW_DESC_INDENT,
        );
    }
}

// ── Confirm page rows ────────────────────────────────────────────

/// Confirm-page rows: Submit / Cancel.
fn confirm_rows(
    panel: &AskPanel,
    palette: &ThemePalette,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    // Unanswered-question warning: the user may still submit (unanswered
    // questions are sent as the placeholder), but never silently.
    let unanswered = panel.unanswered_headers();
    if !unanswered.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "⚠ Unanswered: {} — they will be sent as \"{UNANSWERED_PLACEHOLDER}\"",
                unanswered.join(", ")
            ),
            Style::default().fg(palette.warning),
        )));
        lines.push(Line::from(""));
    }
    const ROWS: [(&str, &str); 2] = [
        ("Submit", "Send the collected answers to the agent."),
        (
            "Cancel",
            "Tell the agent you cancelled — the turn continues.",
        ),
    ];
    for (i, (label, description)) in ROWS.iter().enumerate() {
        let is_cursor = panel.confirm_cursor == i;
        let spans = vec![
            cursor_span(is_cursor, palette),
            number_span(i + 1, is_cursor, palette),
            label_span(label, is_cursor, palette),
        ];
        lines.push(Line::from(spans));
        push_description(description, palette, width, lines, ROW_DESC_INDENT);
    }
}

fn number_span(index: usize, is_cursor: bool, palette: &ThemePalette) -> Span<'static> {
    // Decorative ordinal only — never part of the answer text.
    Span::styled(format!("{index}. "), label_style(is_cursor, palette))
}

fn checkbox_span(checked: bool, palette: &ThemePalette) -> Span<'static> {
    if checked {
        Span::styled("[x] ", Style::default().fg(palette.success))
    } else {
        Span::styled("[ ] ", Style::default().fg(palette.dim))
    }
}

/// Single-select marker: `( )` uncommitted, `(●)` the committed option.
/// Mirrors the multi-select checkbox column.
fn radio_span(selected: bool, palette: &ThemePalette) -> Span<'static> {
    if selected {
        Span::styled("(●) ", Style::default().fg(palette.success))
    } else {
        Span::styled("( ) ", Style::default().fg(palette.dim))
    }
}

fn push_description(
    text: &str,
    palette: &ThemePalette,
    width: u16,
    lines: &mut Vec<Line<'static>>,
    indent: usize,
) {
    if text.trim().is_empty() {
        return;
    }
    let dim = Style::default().fg(palette.dim);
    let indent_str = " ".repeat(indent);
    let avail = (width as usize).saturating_sub(indent);
    for wrapped in wrap_plain(text, avail) {
        lines.push(Line::from(vec![
            Span::raw(indent_str.clone()),
            Span::styled(wrapped, dim),
        ]));
    }
}

/// Inline editor rendering: the buffer window around the cursor, with a
/// reverse-video cursor cell. `max_width` is the space left on the line.
fn edit_spans(
    draft: &str,
    cursor: usize,
    palette: &ThemePalette,
    max_width: usize,
) -> Vec<Span<'static>> {
    if max_width == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = draft.chars().collect();
    let cursor = cursor.min(chars.len());
    let dim = Style::default().fg(palette.dim);

    // Window: extend left while it fits, then right from the cursor cell.
    let mut start = cursor;
    let mut left_width = 0usize;
    while start > 0 {
        let cw = chars[start - 1].width().unwrap_or(0);
        if left_width + cw > max_width.saturating_sub(1) {
            break;
        }
        left_width += cw;
        start -= 1;
    }
    let mut end = (cursor + 1).min(chars.len());
    let mut used = left_width + if cursor < chars.len() { 1 } else { 0 };
    while end < chars.len() {
        let cw = chars[end].width().unwrap_or(0);
        if used + cw > max_width {
            break;
        }
        used += cw;
        end += 1;
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    if start > 0 {
        spans.push(Span::styled("…", dim));
    }
    if start < cursor {
        spans.push(Span::raw(chars[start..cursor].iter().collect::<String>()));
    }
    let cursor_style = Style::default().add_modifier(Modifier::REVERSED);
    match chars.get(cursor) {
        Some(c) => spans.push(Span::styled(c.to_string(), cursor_style)),
        None => spans.push(Span::styled(" ", cursor_style)),
    }
    if cursor + 1 < end {
        spans.push(Span::raw(chars[cursor + 1..end].iter().collect::<String>()));
    }
    if end < chars.len() {
        spans.push(Span::styled("…", dim));
    }
    spans
}

/// Footer key hints — variant per mode and interaction state.
fn footer_hint(panel: &AskPanel) -> &'static str {
    match panel.mode {
        PanelMode::RequiredChoice => "↑↓ select · Enter confirm · Esc interrupt",
        PanelMode::Notice => "",
        PanelMode::Question => {
            if panel.on_confirm_page() {
                "↑↓ select · Enter confirm · ←→ switch · Esc interrupt"
            } else if panel.editing() {
                "type · Enter confirm · ←→ move cursor · ↑↓ back to list · Esc interrupt"
            } else if panel.questions[panel.current].multi_select {
                "↑↓ select · Space/Tab toggle · Enter next · ←→ switch · Esc interrupt"
            } else {
                "↑↓ select · Enter next · ←→ switch · Esc interrupt"
            }
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────

/// Wrap plain text to `max_width` display columns.
///
/// Breaks on whitespace where possible (latin words stay intact) and
/// hard-wraps long tokens char by char (CJK runs have no spaces).
fn wrap_plain(text: &str, max_width: usize) -> Vec<String> {
    let max = max_width.max(1);
    let mut lines: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut line_width = 0usize;
        for word in paragraph.split_whitespace() {
            let word_width = word.width();
            if word_width <= max {
                if line_width > 0 && line_width + 1 + word_width > max {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                if line_width > 0 {
                    line.push(' ');
                    line_width += 1;
                }
                line.push_str(word);
                line_width += word_width;
            } else {
                if line_width > 0 {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                }
                for ch in word.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if line_width > 0 && line_width + cw > max {
                        lines.push(std::mem::take(&mut line));
                        line_width = 0;
                    }
                    line.push(ch);
                    line_width += cw;
                }
            }
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AskOption, AskQuestion};
    use crate::shared::panels::ask::PanelMode;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    fn question(id: &str, header: &str, multi: bool, options: &[&str]) -> AskQuestion {
        AskQuestion {
            id: id.into(),
            header: header.into(),
            question: format!("{id}?"),
            multi_select: multi,
            options: options
                .iter()
                .map(|o| AskOption {
                    label: o.to_string(),
                    description: String::new(),
                })
                .collect(),
            choices: vec![],
        }
    }

    fn multi_panel() -> AskPanel {
        AskPanel::new(
            "tc".into(),
            vec![
                {
                    let mut q = question("theme", "配色方案", false, &["浅色主题", "深色主题"]);
                    q.options[0].description = "白色背景、深色文字".into();
                    q
                },
                question("features", "测试项", true, &["多选交互", "代码预览"]),
            ],
        )
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn panel_renders_tabs_options_and_free_form_row() {
        let panel = multi_panel();
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("配色方案 > 测试项 > Submit"), "{out}");
        assert!(out.contains("theme?"), "{out}");
        assert!(out.contains("❯ 1. ( ) 浅色主题"), "{out}");
        assert!(out.contains("白色背景、深色文字"), "{out}");
        assert!(out.contains("2. ( ) 深色主题"), "{out}");
        assert!(out.contains("3. Type Something"), "{out}");
        assert!(out.contains("Enter a custom response"), "{out}");
        assert!(out.contains("↑↓ select · Enter next"), "{out}");
        // No synthetic letters anywhere.
        assert!(!out.contains("A."), "{out}");
        assert!(!out.contains("B."), "{out}");
    }

    #[test]
    fn committed_single_select_renders_radio() {
        let mut panel = multi_panel();
        panel.states[0].cursor = 1;
        panel.states[0].selected = Some(1);
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("1. ( ) 浅色主题"), "{out}");
        assert!(out.contains("❯ 2. (●) 深色主题"), "{out}");
    }

    #[test]
    fn multi_select_renders_checkboxes_and_hint() {
        let mut panel = multi_panel();
        panel.current = 1;
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("(Select all that apply)"), "{out}");
        assert!(out.contains("❯ 1. [ ] 多选交互"), "{out}");
        assert!(out.contains("2. [ ] 代码预览"), "{out}");
        assert!(out.contains("Space/Tab toggle"), "{out}");
    }

    #[test]
    fn checked_option_renders_marker() {
        let mut panel = multi_panel();
        panel.current = 1;
        panel.states[1].toggles[0] = true;
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("❯ 1. [x] 多选交互"), "{out}");
    }

    #[test]
    fn editing_row_renders_buffer_with_cursor() {
        let mut panel = multi_panel();
        panel.states[0].cursor = 2; // free-form row
        panel.states[0].editing = true;
        panel.states[0].draft = "深色".into();
        panel.states[0].edit_cursor = 1;
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("Type Something: 深色"), "{out}");
        assert!(out.contains("type · Enter confirm"), "{out}");
    }

    #[test]
    fn confirm_page_renders_submit_and_cancel() {
        let mut panel = multi_panel();
        panel.current = 2;
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("Submit your answers?"), "{out}");
        // Both questions unanswered → the warning lists them with the placeholder.
        assert!(
            out.contains(&format!(
                "⚠ Unanswered: 配色方案, 测试项 — they will be sent as \"{UNANSWERED_PLACEHOLDER}\""
            )),
            "{out}"
        );
        assert!(out.contains("❯ 1. Submit"), "{out}");
        assert!(out.contains("2. Cancel"), "{out}");
        assert!(out.contains("↑↓ select · Enter confirm"), "{out}");
    }

    #[test]
    fn confirm_page_hides_warning_when_all_answered() {
        let mut panel = multi_panel();
        panel.states[0].selected = Some(0);
        panel.states[1].toggles[0] = true;
        panel.current = 2;
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(!out.contains("Unanswered"), "{out}");
    }

    #[test]
    fn finished_panel_renders_summary() {
        let mut panel = multi_panel();
        panel.states[0].selected = Some(0);
        panel.states[1].toggles[0] = true;
        panel.finished = Some(PanelFinish::Submitted);
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("✓ 配色方案: 浅色主题"), "{out}");
        assert!(out.contains("✓ 测试项: 多选交互"), "{out}");
    }

    #[test]
    fn cancelled_panel_renders_notice() {
        let mut panel = multi_panel();
        panel.finished = Some(PanelFinish::Cancelled);
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("Cancelled"), "{out}");
    }

    // ── Notice mode rendering (retired non-interactive) ────────────

    fn notice_panel() -> AskPanel {
        use crate::shared::panels::ask::AskPayload;
        AskPanel::from_ask(AskPayload {
            tool_call_id: "n-1",
            questions: &[],
            question: "Just so you know",
            choices: &["yes".into(), "no".into()],
            required: false,
        })
    }

    #[test]
    fn notice_renders_static_bullets_and_no_cursor() {
        let msg = AskMessage::new(notice_panel());
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("? Just so you know"), "{out}");
        assert!(out.contains("• yes"), "{out}");
        assert!(out.contains("• no"), "{out}");
        assert!(!out.contains("❯"), "no cursor: {out}");
        assert!(!out.contains("Enter confirm"), "no footer: {out}");
        assert!(!out.contains("A."), "no synthetic letters: {out}");
        assert!(!out.contains("B."), "no synthetic letters: {out}");
    }

    // ── RequiredChoice mode rendering ─────────────────────────────

    fn required_panel() -> AskPanel {
        use crate::shared::panels::ask::AskPayload;
        AskPanel::from_ask(AskPayload {
            tool_call_id: "rc-1",
            questions: &[],
            question: "Proceed?",
            choices: &["y".into(), "n".into(), "yolo".into()],
            required: true,
        })
    }

    #[test]
    fn required_choice_renders_options_and_no_tab_bar() {
        let msg = AskMessage::new(required_panel());
        let out = text(&msg.to_lines(&p(), 80));
        // No tab bar.
        assert!(!out.contains("> Submit"), "no tab bar: {out}");
        assert!(!out.contains("> choice"), "no tab bar: {out}");
        // Question body rendered.
        assert!(out.contains("Proceed?"), "{out}");
        // Options with cursor + radio.
        assert!(out.contains("❯ 1. ( ) y"), "{out}");
        assert!(out.contains("2. ( ) n"), "{out}");
        assert!(out.contains("3. ( ) yolo"), "{out}");
        // No free-form row.
        assert!(!out.contains("Type Something"), "no free-form row: {out}");
        // Footer.
        assert!(out.contains("Enter confirm"), "footer: {out}");
        assert!(out.contains("Esc interrupt"), "footer: {out}");
    }

    #[test]
    fn required_choice_finished_renders_bare_label() {
        let mut panel = required_panel();
        panel.states[0].selected = Some(2); // "yolo"
        panel.finished = Some(PanelFinish::Submitted);
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("✓ yolo"), "{out}");
        assert!(!out.contains("choice:"), "no header prefix: {out}");
    }

    #[test]
    fn required_choice_no_tab_bar_no_confirm_page() {
        let panel = required_panel();
        assert_eq!(panel.mode, PanelMode::RequiredChoice);
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(!out.contains("Submit"), "no confirm page: {out}");
        assert!(!out.contains("回答"), "no Chinese Submit: {out}");
    }

    // ── Shared helpers ────────────────────────────────────────────

    #[test]
    fn wrap_plain_breaks_words_and_cjk() {
        assert_eq!(
            wrap_plain("hello world foo", 11),
            vec!["hello world", "foo"]
        );
        assert_eq!(wrap_plain("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap_plain("a\nb", 10), vec!["a", "b"]);
    }

    #[test]
    fn descriptions_wrap_to_the_panel_width() {
        let mut panel = multi_panel();
        panel.questions[0].options[0].description = "一段比较长的描述文字用于验证换行行为".into();
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 20));
        // Description indents under the option label (col 9: cursor+number+radio).
        assert!(out.contains("         一段比较"), "{out}");
    }

    #[test]
    fn long_option_list_scrolls_a_centered_window() {
        // 8 options + free-form row, cursor on the 6th option (index 5): the
        // window centers the cursor → rows 3..8 (m3..m7), no marker glyphs.
        let mut panel = AskPanel::new(
            "tc".into(),
            vec![question(
                "q",
                "Q",
                false,
                &["m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7"],
            )],
        );
        panel.states[0].cursor = 5;
        let msg = AskMessage::new(panel);
        let out = text(&msg.to_lines(&p(), 80));
        for hidden in ["m0", "m1", "m2"] {
            assert!(!out.contains(hidden), "{hidden} above the window: {out}");
        }
        assert!(out.contains("m3") && out.contains("m7"), "window: {out}");
        assert!(out.contains("❯ 6. ( ) m5"), "cursor centered: {out}");
        assert!(
            !out.contains("Type Something"),
            "free-form row is outside the window: {out}"
        );
        assert!(
            !out.contains('‹') && !out.contains('›'),
            "no markers: {out}"
        );
    }
}
