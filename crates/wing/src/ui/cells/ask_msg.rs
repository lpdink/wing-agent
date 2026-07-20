//! AskMessage — renders agent questions to the user.
//!
//! Displays the question with optional lettered choices. When `selected`
//! is set (interactive selection menu), a `▸` cursor highlights the
//! current choice instead of static letter prefixes.

use crate::config::ThemePalette;
use crate::render::markdown::render_markdown_with_width;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// An agent question with optional choices.
#[derive(Debug, Clone)]
pub struct AskMessage {
    pub question: String,
    pub choices: Vec<String>,
    /// When Some(i), choice i is highlighted with a ▸ cursor.
    /// Used for interactive selection menus (required Ask events).
    pub selected: Option<usize>,
}

impl AskMessage {
    pub fn new(question: String, choices: Vec<String>) -> Self {
        Self {
            question,
            choices,
            selected: None,
        }
    }

    /// Render to lines.
    ///
    /// Question body is rendered as markdown. The `?` marker is prepended
    /// to the first line. Choices are rendered as:
    /// - Static mode (selected = None): `A. choice`
    /// - Interactive mode (selected = Some(i)): `▸ choice` for i, `  choice` for others
    ///
    /// `width` is the full content width; 2 columns are reserved for the `? `
    /// marker so tables balance to fit.
    pub fn to_lines(&self, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
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
        let msg = AskMessage::new("Continue?".into(), vec![]);
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
        // Should have ▸ on "n", not on "y" or "yolo"
        assert!(text.contains("▸ n"), "missing cursor on n: {text}");
        assert!(!text.contains("▸ y "), "unexpected cursor on y: {text}");
        // No A./B./C. letters in interactive mode
        assert!(
            !text.contains("A."),
            "should not have letter prefix: {text}"
        );
    }

    #[test]
    fn test_ask_selection_first() {
        let mut msg = AskMessage::new("Proceed?".into(), vec!["y".into(), "n".into()]);
        msg.selected = Some(0);
        let lines = msg.to_lines(&p(), 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("▸ y"), "missing cursor on y: {text}");
    }
}
