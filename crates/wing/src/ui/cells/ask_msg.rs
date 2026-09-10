//! AskMessage — renders agent questions to the user.
//!
//! Two modes:
//! - **Panel**: the AskUserQuestion panel — a tab bar (question headers +
//!   confirm page), one question at a time, single/multi-select options, a
//!   free-form row with an inline editor, and a footer with the key hints.
//!   Interaction state lives in `app::ask_panel::AskPanel`; this cell stores
//!   a render snapshot of it.
//! - **Legacy**: a single question with optional plain choices (e.g. the Bash
//!   dangerous-command confirmation). A `▸` cursor highlights the current
//!   choice when the ask requires a selection.
//!
//! No synthetic identifiers are ever added to options: labels render verbatim
//! and a chosen option is sent back verbatim.

use crate::app::ask_panel::AskPanel;
use crate::app::ask_panel::PanelFinish;
use crate::config::ThemePalette;
use crate::render::markdown::render_markdown_with_width;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// Indent of option descriptions (aligned under the option label).
const DESC_INDENT: usize = 6;

/// An agent question: an interactive panel, or a legacy single-question prompt.
#[derive(Debug, Clone)]
pub struct AskMessage {
    /// Correlation id of the Ask event (used to locate this cell when
    /// updating panel state under concurrent asks).
    pub tool_call_id: String,
    /// Interactive AskUserQuestion panel (None → legacy single-question form).
    pub panel: Option<AskPanel>,
    pub question: String,
    pub choices: Vec<String>,
    /// When Some(i), choice i is highlighted with a ▸ cursor (legacy
    /// interactive selection menus).
    pub selected: Option<usize>,
}

impl AskMessage {
    /// Create a panel-backed message (AskUserQuestion).
    pub fn new_panel(tool_call_id: String, panel: AskPanel) -> Self {
        Self {
            tool_call_id,
            panel: Some(panel),
            question: String::new(),
            choices: Vec::new(),
            selected: None,
        }
    }

    /// Create a legacy single-question message.
    pub fn new_legacy(tool_call_id: String, question: String, choices: Vec<String>) -> Self {
        Self {
            tool_call_id,
            panel: None,
            question,
            choices,
            selected: None,
        }
    }

    /// Render to lines.
    pub fn to_lines(&self, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
        match &self.panel {
            Some(panel) => panel_lines(panel, palette, width),
            None => legacy_lines(&self.question, &self.choices, self.selected, palette, width),
        }
    }
}

// ── Panel rendering ──────────────────────────────────────────────

fn panel_lines(panel: &AskPanel, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(tab_bar(panel, palette));
    lines.push(Line::from(""));

    match panel.finished {
        Some(PanelFinish::Submitted) => {
            let success = Style::default().fg(palette.success);
            for line in panel.build_response().lines() {
                lines.push(Line::from(vec![
                    Span::styled("✓ ", success),
                    Span::styled(line.to_string(), dim),
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
                    lines.push(Line::from(Span::styled("(Select all that apply)", dim)));
                }
                lines.push(Line::from(""));
                option_rows(panel, palette, width, &mut lines);
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(footer_hint(panel), dim)));
        }
    }
    lines.push(Line::from(""));
    lines
}

/// `? 配色方案 > 测试项 > Submit` — question tabs + confirm page.
fn tab_bar(panel: &AskPanel, palette: &ThemePalette) -> Line<'static> {
    let accent = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(palette.dim);
    let success = Style::default().fg(palette.success);

    let mut spans: Vec<Span<'static>> = vec![Span::styled("? ", accent)];
    for (i, q) in panel.questions.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" > ", dim));
        }
        let style = if panel.finished.is_some() {
            dim
        } else if i == panel.current {
            accent
        } else if panel.is_answered(i) {
            success
        } else {
            dim
        };
        spans.push(Span::styled(q.tab_label().to_string(), style));
    }
    spans.push(Span::styled(" > ", dim));
    let submit_style = if panel.finished.is_some() {
        dim
    } else if panel.on_confirm_page() {
        accent
    } else if panel.all_answered() {
        success
    } else {
        dim
    };
    spans.push(Span::styled("Submit", submit_style));
    Line::from(spans)
}

/// Option rows of the current question, plus the free-form row.
fn option_rows(
    panel: &AskPanel,
    palette: &ThemePalette,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    let qi = panel.current;
    let q = &panel.questions[qi];
    let st = &panel.states[qi];

    for (i, option) in q.options.iter().enumerate() {
        let is_cursor = st.cursor == i;
        let mut spans = vec![
            cursor_span(is_cursor, palette),
            number_span(i + 1, is_cursor, palette),
        ];
        if q.multi_select {
            spans.push(checkbox_span(st.toggles[i], palette));
        }
        spans.push(label_span(&option.label, is_cursor, palette));
        lines.push(Line::from(spans));
        push_description(&option.description, palette, width, lines);
    }

    // Free-form row (always last).
    let custom_idx = q.options.len();
    let is_cursor = st.cursor == custom_idx;
    let mut spans = vec![
        cursor_span(is_cursor, palette),
        number_span(custom_idx + 1, is_cursor, palette),
    ];
    if st.editing {
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
        push_description("Enter a custom response", palette, width, lines);
    }
}

/// Confirm-page rows: Submit / Cancel.
fn confirm_rows(
    panel: &AskPanel,
    palette: &ThemePalette,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
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
        push_description(description, palette, width, lines);
    }
}

fn cursor_span(is_cursor: bool, palette: &ThemePalette) -> Span<'static> {
    if is_cursor {
        Span::styled(
            "❯ ",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    }
}

fn number_span(index: usize, is_cursor: bool, palette: &ThemePalette) -> Span<'static> {
    // Decorative ordinal only — never part of the answer text.
    Span::styled(format!("{index}. "), label_style(is_cursor, palette))
}

fn label_span(label: &str, is_cursor: bool, palette: &ThemePalette) -> Span<'static> {
    Span::styled(label.to_string(), label_style(is_cursor, palette))
}

fn label_style(is_cursor: bool, palette: &ThemePalette) -> Style {
    if is_cursor {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(palette.text)
    }
}

fn checkbox_span(checked: bool, palette: &ThemePalette) -> Span<'static> {
    if checked {
        Span::styled("[x] ", Style::default().fg(palette.success))
    } else {
        Span::styled("[ ] ", Style::default().fg(palette.dim))
    }
}

fn push_description(
    text: &str,
    palette: &ThemePalette,
    width: u16,
    lines: &mut Vec<Line<'static>>,
) {
    if text.trim().is_empty() {
        return;
    }
    let dim = Style::default().fg(palette.dim);
    let indent = " ".repeat(DESC_INDENT);
    let avail = (width as usize).saturating_sub(DESC_INDENT);
    for wrapped in wrap_plain(text, avail) {
        lines.push(Line::from(vec![
            Span::raw(indent.clone()),
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

/// Footer key hints — variant per interaction state.
fn footer_hint(panel: &AskPanel) -> &'static str {
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

// ── Legacy rendering ─────────────────────────────────────────────

/// Legacy single-question mode: markdown question + optional choices.
fn legacy_lines(
    question: &str,
    choices: &[String],
    selected: Option<usize>,
    palette: &ThemePalette,
    width: u16,
) -> Vec<Line<'static>> {
    let accent_style = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);

    // Render question body as markdown with a `? ` marker on the first line.
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

    // Choices: interactive ▸ cursor, or plain dim suggestions. Deliberately
    // NO synthetic A/B/C letters — the user's answer is sent back verbatim,
    // and letters the model never defined must not look like identifiers.
    let normal_style = Style::default().fg(palette.text);
    let dim_style = Style::default().fg(palette.dim);
    for (i, choice) in choices.iter().enumerate() {
        if let Some(sel) = selected {
            let is_selected = i == sel;
            let (cursor, style) = if is_selected {
                ("▸ ", accent_style)
            } else {
                ("  ", dim_style)
            };
            md_lines.push(Line::from(vec![
                Span::styled(cursor, style),
                Span::styled(
                    choice.clone(),
                    if is_selected {
                        accent_style
                    } else {
                        normal_style
                    },
                ),
            ]));
        } else {
            md_lines.push(Line::from(vec![
                Span::styled("  • ", dim_style),
                Span::styled(choice.clone(), dim_style),
            ]));
        }
    }

    md_lines.push(Line::from(""));
    md_lines
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
    use crate::app::ask_panel::AskPanel;
    use crate::protocol::{AskOption, AskQuestion};

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
        let msg = AskMessage::new_panel("tc".into(), panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("配色方案 > 测试项 > Submit"), "{out}");
        assert!(out.contains("theme?"), "{out}");
        assert!(out.contains("❯ 1. 浅色主题"), "{out}");
        assert!(out.contains("白色背景、深色文字"), "{out}");
        assert!(out.contains("2. 深色主题"), "{out}");
        assert!(out.contains("3. Type Something"), "{out}");
        assert!(out.contains("Enter a custom response"), "{out}");
        assert!(out.contains("↑↓ select · Enter next"), "{out}");
        // No synthetic letters anywhere.
        assert!(!out.contains("A."), "{out}");
        assert!(!out.contains("B."), "{out}");
    }

    #[test]
    fn multi_select_renders_checkboxes_and_hint() {
        let mut panel = multi_panel();
        panel.current = 1;
        let msg = AskMessage::new_panel("tc".into(), panel);
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
        let msg = AskMessage::new_panel("tc".into(), panel);
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
        let msg = AskMessage::new_panel("tc".into(), panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("Type Something: 深色"), "{out}");
        assert!(out.contains("type · Enter confirm"), "{out}");
    }

    #[test]
    fn confirm_page_renders_submit_and_cancel() {
        let mut panel = multi_panel();
        panel.current = 2;
        let msg = AskMessage::new_panel("tc".into(), panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("Submit your answers?"), "{out}");
        assert!(out.contains("❯ 1. Submit"), "{out}");
        assert!(out.contains("2. Cancel"), "{out}");
        assert!(out.contains("↑↓ select · Enter confirm"), "{out}");
    }

    #[test]
    fn finished_panel_renders_summary() {
        let mut panel = multi_panel();
        panel.states[0].visited = true;
        panel.states[1].visited = true;
        panel.states[1].toggles[0] = true;
        panel.finished = Some(PanelFinish::Submitted);
        let msg = AskMessage::new_panel("tc".into(), panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("✓ 配色方案: 浅色主题"), "{out}");
        assert!(out.contains("✓ 测试项: 多选交互"), "{out}");
    }

    #[test]
    fn cancelled_panel_renders_notice() {
        let mut panel = multi_panel();
        panel.finished = Some(PanelFinish::Cancelled);
        let msg = AskMessage::new_panel("tc".into(), panel);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("Cancelled"), "{out}");
    }

    #[test]
    fn legacy_static_choices_do_not_add_letters() {
        let msg = AskMessage::new_legacy(
            "tc".into(),
            "Proceed?".into(),
            vec!["yes".into(), "no".into()],
        );
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("yes"), "{out}");
        assert!(out.contains("no"), "{out}");
        assert!(!out.contains("A."), "no synthetic letters: {out}");
        assert!(!out.contains("B."), "no synthetic letters: {out}");
    }

    #[test]
    fn legacy_selection_cursor_still_renders() {
        let mut msg = AskMessage::new_legacy(
            "tc".into(),
            "Proceed?".into(),
            vec!["y".into(), "n".into(), "yolo".into()],
        );
        msg.selected = Some(1);
        let out = text(&msg.to_lines(&p(), 80));
        assert!(out.contains("▸ n"), "{out}");
        assert!(!out.contains("▸ y"), "{out}");
    }

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
        let msg = AskMessage::new_panel("tc".into(), panel);
        let out = text(&msg.to_lines(&p(), 20));
        // Description must live on its own indented lines.
        assert!(out.contains("      一段比较"), "{out}");
    }
}
