//! AskMessage — renders agent questions to the user.
//!
//! Two modes:
//! - **Legacy**: single question with optional lettered choices.
//! - **Multi-question**: shows progress, answered questions (dim), and the
//!   current question (accent). Used by the AskUserQuestion tool.
//!
//! When `selected` is set (interactive selection menu for legacy required
//! Ask events), a `▸` cursor highlights the current choice.

use crate::config::ThemePalette;
use crate::protocol::AskQuestion;
use crate::render::markdown::render_markdown_with_width;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

/// An agent question with optional choices.
#[derive(Debug, Clone)]
pub struct AskMessage {
    /// Correlation id of the Ask event (used to locate this cell when
    /// updating progress/selection under concurrent asks).
    pub tool_call_id: String,
    pub question: String,
    pub choices: Vec<String>,
    /// When Some(i), choice i is highlighted with a ▸ cursor.
    /// Used for interactive selection menus (required Ask events).
    pub selected: Option<usize>,
    /// Multi-question mode: all questions from the Ask event.
    pub questions: Vec<AskQuestion>,
    /// Index of the current question being answered (multi-question mode).
    /// When >= questions.len(), all are answered.
    pub current_idx: usize,
    /// Collected answers so far (multi-question mode).
    pub answers: Vec<String>,
}

impl AskMessage {
    /// Create a legacy single-question message.
    pub fn new(tool_call_id: String, question: String, choices: Vec<String>) -> Self {
        Self {
            tool_call_id,
            question,
            choices,
            selected: None,
            questions: Vec::new(),
            current_idx: 0,
            answers: Vec::new(),
        }
    }

    /// Create a multi-question message.
    pub fn new_multi(tool_call_id: String, questions: Vec<AskQuestion>) -> Self {
        let len = questions.len();
        Self {
            tool_call_id,
            question: String::new(),
            choices: Vec::new(),
            selected: None,
            questions,
            current_idx: 0,
            answers: vec![String::new(); len],
        }
    }

    /// Whether this is a multi-question message.
    fn is_multi(&self) -> bool {
        !self.questions.is_empty()
    }

    /// Render to lines.
    pub fn to_lines(&self, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
        if self.is_multi() {
            self.to_lines_multi(palette, width)
        } else {
            self.to_lines_legacy(palette, width)
        }
    }

    /// Render multi-question mode.
    fn to_lines_multi(&self, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
        let accent_style = Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD);
        let dim_style = Style::default().fg(palette.dim);
        let success_style = Style::default().fg(palette.success);

        let mut lines: Vec<Line<'static>> = Vec::new();

        // Progress header: "Question 1/3"
        let total = self.questions.len();
        let current = self.current_idx.min(total);
        let progress = if current >= total {
            format!("Questions {total}/{total} — all answered")
        } else {
            format!("Question {}/{}", current + 1, total)
        };
        lines.push(Line::from(Span::styled(progress, dim_style)));
        lines.push(Line::from(""));

        // Render answered questions (dim, with ✓).
        for (i, q) in self.questions.iter().enumerate() {
            if i >= self.current_idx {
                break;
            }
            let answer = self.answers.get(i).map(|s| s.as_str()).unwrap_or("");
            // Question line (dim).
            let q_text = truncate_single_line(&q.question, width.saturating_sub(4));
            lines.push(Line::from(vec![
                Span::styled("✓ ", success_style),
                Span::styled(q_text, dim_style),
            ]));
            // Answer line (dim, indented).
            let a_text = truncate_single_line(answer, width.saturating_sub(6));
            lines.push(Line::from(vec![
                Span::styled("  → ", dim_style),
                Span::styled(a_text, dim_style),
            ]));
        }

        // Render current question (accent, with ? marker).
        if let Some(q) = self.questions.get(self.current_idx) {
            let md_width = Some(width.saturating_sub(2));
            let mut md_lines = render_markdown_with_width(&q.question, md_width, palette);
            if let Some(first) = md_lines.first_mut() {
                let marker = Span::styled("?", accent_style);
                let mut spans = vec![marker, Span::raw(" ")];
                spans.append(&mut first.spans);
                *first = Line::from(spans);
            } else {
                lines.push(Line::from(vec![
                    Span::styled("?", accent_style),
                    Span::raw(" "),
                ]));
            }
            lines.extend(md_lines);

            // Render choices as suggestions (if any).
            if !q.choices.is_empty() {
                let normal_style = Style::default().fg(palette.text);
                for (i, choice) in q.choices.iter().enumerate() {
                    let letter = (b'A' + i as u8) as char;
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{letter}."), Style::default().fg(palette.accent)),
                        Span::raw(" "),
                        Span::styled(choice.clone(), normal_style),
                    ]));
                }
            }
        }

        lines.push(Line::from(""));
        lines
    }

    /// Render legacy single-question mode.
    fn to_lines_legacy(&self, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
        let accent_style = Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD);

        // Render question body as markdown.
        let md_width = Some(width.saturating_sub(2));
        let mut md_lines = render_markdown_with_width(&self.question, md_width, palette);

        // Prepend `? ` marker to the first line.
        if let Some(first) = md_lines.first_mut() {
            let marker = Span::styled("?", accent_style);
            let mut spans = vec![marker, Span::raw(" ")];
            spans.append(&mut first.spans);
            *first = Line::from(spans);
        } else {
            let marker = Span::styled("?", accent_style);
            md_lines.push(Line::from(vec![marker, Span::raw(" ")]));
        }

        // Append choices.
        if !self.choices.is_empty() {
            let selected_style = Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD);
            let normal_style = Style::default().fg(palette.text);
            let dim_style = Style::default().fg(palette.dim);

            for (i, choice) in self.choices.iter().enumerate() {
                if let Some(sel) = self.selected {
                    // Interactive mode: ▸ cursor on selected, space on others.
                    let is_selected = i == sel;
                    let (cursor, style) = if is_selected {
                        ("▸ ", selected_style)
                    } else {
                        ("  ", dim_style)
                    };
                    md_lines.push(Line::from(vec![
                        Span::styled(cursor, style),
                        Span::styled(
                            choice.clone(),
                            if is_selected {
                                selected_style
                            } else {
                                normal_style
                            },
                        ),
                    ]));
                } else {
                    // Static mode: A. choice
                    let letter = (b'A' + i as u8) as char;
                    md_lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{letter}."), Style::default().fg(palette.accent)),
                        Span::raw(" "),
                        Span::from(choice.clone()),
                    ]));
                }
            }
        }

        md_lines.push(Line::from(""));
        md_lines
    }
}

/// Truncate a string to a single line for compact display.
///
/// Uses unicode display width (CJK chars count as 2 columns).
fn truncate_single_line(s: &str, max_width: u16) -> String {
    let one_line = s.lines().next().unwrap_or("");
    let max = max_width as usize;
    let mut width = 0;
    let mut result = String::new();
    for c in one_line.chars() {
        let cw = c.width().unwrap_or(0);
        if width + cw > max {
            result.push('…');
            return result;
        }
        result.push(c);
        width += cw;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemePalette;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    #[test]
    fn test_ask_with_choices() {
        let msg = AskMessage::new(
            "tc".into(),
            "Which approach?".into(),
            vec!["Option A".into(), "Option B".into()],
        );
        let lines = msg.to_lines(&p(), 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("?"), "missing question marker: {text}");
        assert!(text.contains("Which approach?"), "missing question: {text}");
        assert!(text.contains("A."), "missing choice A: {text}");
        assert!(text.contains("B."), "missing choice B: {text}");
    }

    #[test]
    fn test_ask_without_choices() {
        let msg = AskMessage::new("tc".into(), "Continue?".into(), vec![]);
        let lines = msg.to_lines(&p(), 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Continue?"), "missing question: {text}");
    }

    #[test]
    fn test_ask_with_selection_cursor() {
        let mut msg = AskMessage::new(
            "tc".into(),
            "Proceed?".into(),
            vec!["y".into(), "n".into(), "yolo".into()],
        );
        msg.selected = Some(1); // "n" is selected
        let lines = msg.to_lines(&p(), 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("▸ n"), "missing cursor on n: {text}");
        assert!(!text.contains("▸ y "), "unexpected cursor on y: {text}");
        assert!(
            !text.contains("A."),
            "should not have letter prefix: {text}"
        );
    }

    #[test]
    fn test_ask_selection_first() {
        let mut msg = AskMessage::new("tc".into(), "Proceed?".into(), vec!["y".into(), "n".into()]);
        msg.selected = Some(0);
        let lines = msg.to_lines(&p(), 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("▸ y"), "missing cursor on y: {text}");
    }

    #[test]
    fn test_multi_question_initial() {
        let questions = vec![
            AskQuestion {
                id: "q1".into(),
                question: "First?".into(),
                choices: vec!["A".into(), "B".into()],
            },
            AskQuestion {
                id: "q2".into(),
                question: "Second?".into(),
                choices: vec![],
            },
        ];
        let msg = AskMessage::new_multi("tc".into(), questions);
        let lines = msg.to_lines(&p(), 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Question 1/2"), "missing progress: {text}");
        assert!(text.contains("First?"), "missing first question: {text}");
        assert!(
            !text.contains("Second?"),
            "should not show second yet: {text}"
        );
        assert!(text.contains("A."), "missing choice A: {text}");
    }

    #[test]
    fn test_multi_question_after_answer() {
        let questions = vec![
            AskQuestion {
                id: "q1".into(),
                question: "First?".into(),
                choices: vec![],
            },
            AskQuestion {
                id: "q2".into(),
                question: "Second?".into(),
                choices: vec![],
            },
        ];
        let mut msg = AskMessage::new_multi("tc".into(), questions);
        msg.current_idx = 1;
        msg.answers[0] = "my answer".into();
        let lines = msg.to_lines(&p(), 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Question 2/2"), "missing progress: {text}");
        assert!(text.contains("✓"), "missing checkmark: {text}");
        assert!(text.contains("my answer"), "missing answer: {text}");
        assert!(text.contains("Second?"), "missing second question: {text}");
    }
}
