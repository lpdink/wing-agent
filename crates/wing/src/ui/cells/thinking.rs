//! ThinkingBlock — reasoning content (always expanded).
//!
//! Renders with thinking color and `⦁ ` prefix, no header or border:
//!   ⦁ reasoning content in thinking color...
//!     continuation lines indented...
//!
//! Only plain prose (text, headings, list markers) inherits the thinking
//! color — with the foreground swapped while everything else (modifiers,
//! background) is preserved. Code-like and decorative segments (inline
//! code, code blocks, links, borders, gutters) keep their own theme
//! colors so they stay distinguishable inside reasoning content.

use crate::config::ThemePalette;
use crate::config::rendering::ThinkingMode;
use crate::render::markdown::RenderOpts;
use crate::render::markdown::render_markdown_lines_with;
use crate::render::markdown::types::thinking_segment_style;
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
        // Thinking renders code blocks plain — no syntect, no gutter —
        // permanently (streaming AND final). Keeps the incremental
        // streaming path stateless and the visual consistent across the
        // whole turn (no highlight popping in at turn end).
        let md_lines = render_markdown_lines_with(
            &self.content,
            md_width,
            palette,
            RenderOpts {
                code_highlight: false,
                trim_trailing_blank: true,
            },
        );
        for (i, md_line) in md_lines.iter().enumerate() {
            let prefix = if i == 0 { "⦁ " } else { "  " };
            let mut spans = vec![Span::styled(prefix.to_string(), thinking_style)];
            for seg in &md_line.segments {
                spans.push(Span::styled(
                    seg.text.clone(),
                    thinking_segment_style(seg.kind, seg.style, thinking_style),
                ));
            }
            lines.push(Line::from(spans));
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
    use crate::render::markdown::SegmentKind;
    use ratatui::style::Color;
    use ratatui::style::Modifier;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    /// Collect (text, style) pairs across all rendered spans.
    fn span_pairs(lines: &[Line<'static>]) -> Vec<(String, Style)> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    fn find_span<'a>(pairs: &'a [(String, Style)], needle: &str) -> &'a (String, Style) {
        pairs
            .iter()
            .find(|(text, _)| text.contains(needle))
            .unwrap_or_else(|| panic!("span containing {needle:?} not found: {pairs:?}"))
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
    fn test_thinking_prose_uses_thinking_color() {
        let mut block = ThinkingBlock::new();
        block.append("plain reasoning text");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, 80);
        let pairs = span_pairs(&lines);
        let (_, style) = find_span(&pairs, "plain reasoning");
        assert_eq!(style.fg, Some(Color::Gray), "prose fg: {style:?}");
    }

    #[test]
    fn test_thinking_keeps_inline_code_color() {
        let mut block = ThinkingBlock::new();
        block.append("run `cargo build` now");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, 80);
        let pairs = span_pairs(&lines);
        // Inline code keeps the accent color.
        let (_, code_style) = find_span(&pairs, "cargo build");
        assert_eq!(code_style.fg, Some(Color::Cyan), "code fg: {code_style:?}");
        // Surrounding prose is recolored to thinking gray.
        let (_, prose_style) = find_span(&pairs, "run ");
        assert_eq!(
            prose_style.fg,
            Some(Color::Gray),
            "prose fg: {prose_style:?}"
        );
    }

    #[test]
    fn test_thinking_bold_keeps_modifier_with_gray_fg() {
        let mut block = ThinkingBlock::new();
        block.append("this is **important** indeed");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, 80);
        let pairs = span_pairs(&lines);
        let (_, style) = find_span(&pairs, "important");
        assert_eq!(style.fg, Some(Color::Gray), "bold fg: {style:?}");
        assert!(
            style.add_modifier.contains(Modifier::BOLD),
            "bold modifier lost: {style:?}"
        );
    }

    #[test]
    fn test_thinking_prose_recolor_preserves_bg_and_sub_modifier() {
        // Only the foreground is swapped; bg and sub-modifiers (set via
        // remove_modifier) must survive untouched.
        let original = Style::default()
            .bg(Color::Red)
            .remove_modifier(Modifier::ITALIC);
        let out = thinking_segment_style(
            SegmentKind::Text,
            original,
            Style::default().fg(Color::Gray),
        );
        assert_eq!(out.fg, Some(Color::Gray), "fg not swapped: {out:?}");
        assert_eq!(out.bg, Some(Color::Red), "bg lost: {out:?}");
        assert!(
            out.sub_modifier.contains(Modifier::ITALIC),
            "sub_modifier lost: {out:?}"
        );
    }

    #[test]
    fn test_thinking_code_kind_keeps_style_verbatim() {
        let original = Style::default().fg(Color::Cyan).bold();
        for kind in [
            SegmentKind::InlineCode,
            SegmentKind::CodeBlock,
            SegmentKind::Link,
            SegmentKind::Border,
            SegmentKind::Gutter,
        ] {
            let out = thinking_segment_style(kind, original, Style::default().fg(Color::Gray));
            assert_eq!(out, original, "kind {kind:?} should keep its style");
        }
    }

    #[test]
    fn test_thinking_keeps_code_block_colors() {
        let mut block = ThinkingBlock::new();
        block.append("like this:\n```\nlet x = 1;\n```");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, 80);
        let pairs = span_pairs(&lines);
        // Code block content keeps the accent color, not thinking gray.
        let (_, style) = find_span(&pairs, "let x = 1;");
        assert_eq!(style.fg, Some(Color::Cyan), "code block fg: {style:?}");
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
