//! ThinkingBlock — reasoning content (always expanded).
//!
//! Renders with thinking color and `⦁ ` prefix, no header or border:
//!   ⦁ reasoning content in thinking color...
//!     continuation lines indented...

use crate::config::ThemePalette;
use crate::config::rendering::ThinkingMode;
use crate::render::markdown::render_markdown_with_width;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// A block showing agent reasoning/thinking content.
#[derive(Debug, Clone)]
pub struct ThinkingBlock {
    /// Accumulated reasoning text (streamed).
    pub content: String,
    /// Number of thinking events received (for hidden mode indicator).
    pub event_count: usize,
}

impl ThinkingBlock {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            event_count: 0,
        }
    }

    /// Append reasoning content (streaming).
    pub fn append(&mut self, text: &str) {
        self.content.push_str(text);
    }

    /// Render to lines based on thinking mode.
    ///
    /// `width` is the full content width; 2 columns are reserved for the line
    /// prefix so tables balance to fit.
    pub fn to_lines(
        &self,
        palette: &ThemePalette,
        mode: ThinkingMode,
        width: u16,
    ) -> Vec<Line<'static>> {
        let dim = Style::default().fg(palette.dim);
        match mode {
            ThinkingMode::Visible => self.render_visible(palette, width),
            ThinkingMode::Hidden => self.render_hidden(dim),
        }
    }

    fn render_visible(&self, palette: &ThemePalette, width: u16) -> Vec<Line<'static>> {
        let thinking_style = Style::default().fg(palette.thinking);
        let mut lines = Vec::new();
        let md_width = Some(width.saturating_sub(2));
        let md_lines = render_markdown_with_width(&self.content, md_width, palette);
        for (i, line) in md_lines.iter().enumerate() {
            if i == 0 {
                let mut spans = vec![Span::styled("⦁ ", thinking_style)];
                for span in &line.spans {
                    spans.push(Span::styled(span.content.clone(), thinking_style));
                }
                lines.push(Line::from(spans));
            } else {
                let mut spans = vec![Span::styled("  ", thinking_style)];
                for span in &line.spans {
                    spans.push(Span::styled(span.content.clone(), thinking_style));
                }
                lines.push(Line::from(spans));
            }
        }
        lines.push(Line::from(""));
        lines
    }

    fn render_hidden(&self, dim: Style) -> Vec<Line<'static>> {
        let text = if self.event_count > 0 {
            format!("Thinking... ({} events)", self.event_count)
        } else {
            "Thinking...".to_string()
        };
        vec![Line::from(Span::styled(text, dim)), Line::from("")]
    }
}

impl Default for ThinkingBlock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::rendering::ThinkingMode;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    #[test]
    fn test_thinking_renders_content() {
        let mut block = ThinkingBlock::new();
        block.append("Let me think about this...");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("⦁ "), "missing bullet prefix: {text}");
        assert!(text.contains("Let me think"), "missing content: {text}");
        // No header, no border
        assert!(!text.contains("thinking"), "should not have header: {text}");
        assert!(!text.contains("│"), "should not have border: {text}");
    }

    #[test]
    fn test_thinking_empty() {
        let block = ThinkingBlock::new();
        let lines = block.to_lines(&p(), ThinkingMode::Visible, 80);
        // Empty content → just blank line
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn test_thinking_hidden_no_events() {
        let mut block = ThinkingBlock::new();
        block.append("secret reasoning");
        let lines = block.to_lines(&p(), ThinkingMode::Hidden, 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Thinking..."), "missing indicator: {text}");
        assert!(!text.contains("secret"), "content should be hidden: {text}");
    }

    #[test]
    fn test_thinking_hidden_with_events() {
        let mut block = ThinkingBlock::new();
        block.append("reasoning");
        block.event_count = 5;
        let lines = block.to_lines(&p(), ThinkingMode::Hidden, 80);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("5 events"), "missing event count: {text}");
    }
}
