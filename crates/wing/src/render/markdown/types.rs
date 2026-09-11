//! Core data types for the markdown renderer.
//!
//! `MarkdownSegment` and `MarkdownLine` are the intermediate representation
//! produced by the parser. They are converted to `ratatui::text::Line` for
//! actual terminal rendering.

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;

// ============================================================
// SegmentKind — semantic element origin of a segment
// ============================================================

/// Semantic element kind a segment originates from.
///
/// Lets downstream renderers make element-aware decisions (e.g. the thinking
/// block recolors prose but preserves code colors) without reverse-engineering
/// styles. Defaults to [`SegmentKind::Text`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SegmentKind {
    /// Plain prose (default).
    #[default]
    Text,
    /// Heading text.
    Heading,
    /// Inline code span (`` `code` ``).
    InlineCode,
    /// Fenced code block content, including syntax-highlighted and diff lines.
    CodeBlock,
    /// Link label or rendered URL.
    Link,
    /// List bullet, ordered-list number, or task-list marker.
    Marker,
    /// Decorative chrome: code block frames, table rules, horizontal rules,
    /// blockquote bars.
    Border,
    /// Code block line-number gutter.
    Gutter,
}

// ============================================================
// MarkdownSegment — a styled text fragment
// ============================================================

/// A single styled text segment within a markdown line.
#[derive(Clone, Debug)]
pub struct MarkdownSegment {
    pub kind: SegmentKind,
    pub style: Style,
    pub text: String,
    pub link_target: Option<String>,
}

impl MarkdownSegment {
    pub fn new(kind: SegmentKind, style: Style, text: impl Into<String>) -> Self {
        Self {
            kind,
            style,
            text: text.into(),
            link_target: None,
        }
    }

    pub fn with_link(
        kind: SegmentKind,
        style: Style,
        text: impl Into<String>,
        link_target: Option<String>,
    ) -> Self {
        Self {
            kind,
            style,
            text: text.into(),
            link_target,
        }
    }

    /// Display width of this segment's text (CJK-aware).
    pub fn width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }

    /// Whether this segment is purely whitespace.
    pub fn is_whitespace(&self) -> bool {
        self.text.trim().is_empty()
    }
}

// ============================================================
// MarkdownLine — a line composed of styled segments
// ============================================================

/// A rendered line composed of styled segments.
#[derive(Clone, Debug, Default)]
pub struct MarkdownLine {
    pub segments: Vec<MarkdownSegment>,
}

impl MarkdownLine {
    /// Append a segment. Every callsite must declare the segment's
    /// [`SegmentKind`] explicitly — kind drives element-aware rendering
    /// downstream (e.g. thinking recolors prose but not code), so there is
    /// no default: use [`SegmentKind::Text`] for plain prose.
    pub fn push_segment(&mut self, kind: SegmentKind, style: Style, text: &str) {
        self.push_segment_with_link(kind, style, text, None);
    }

    /// Append a segment with an optional link target, merging adjacent
    /// segments that share the same kind, style, and link target.
    pub fn push_segment_with_link(
        &mut self,
        kind: SegmentKind,
        style: Style,
        text: &str,
        link_target: Option<String>,
    ) {
        if text.is_empty() {
            return;
        }
        // Merge with previous segment if kind, style, and link match.
        if let Some(last) = self.segments.last_mut()
            && last.kind == kind
            && last.style == style
            && last.link_target == link_target
        {
            last.text.push_str(text);
            return;
        }
        self.segments
            .push(MarkdownSegment::with_link(kind, style, text, link_target));
    }

    /// Whether this line contains only whitespace segments.
    pub fn is_empty(&self) -> bool {
        self.segments
            .iter()
            .all(|segment| segment.text.trim().is_empty())
    }

    /// Total display width of this line (CJK-aware).
    pub fn width(&self) -> usize {
        self.segments.iter().map(|seg| seg.width()).sum()
    }

    /// Plain text content of this line (no styling).
    pub fn to_plain(&self) -> String {
        self.segments.iter().map(|seg| seg.text.as_str()).collect()
    }
}

// ============================================================
// MarkdownTheme — style configuration for the renderer
// ============================================================

/// Style configuration for markdown rendering.
///
/// Provides the set of styles applied to different markdown elements.
/// All fields use `ratatui::style::Style`.
#[derive(Clone, Debug)]
pub struct MarkdownTheme {
    pub h1: Style,
    pub h2: Style,
    pub h3: Style,
    pub h4: Style,
    pub h5: Style,
    pub h6: Style,
    pub bold: Style,
    pub italic: Style,
    pub bold_italic: Style,
    pub strikethrough: Style,
    pub code: Style,
    pub code_block: Style,
    pub code_block_gutter: Style,
    pub link: Style,
    pub blockquote: Style,
    pub border: Style,
    pub base: Style,
    pub dimmed: Style,
    pub heading_prefix: Style,
    /// Diff addition lines (+).
    pub diff_add: Style,
    /// Diff deletion lines (-).
    pub diff_del: Style,
    /// Diff hunk headers (@@).
    pub diff_hunk: Style,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        let p = ThemePalette::default();
        Self::from_palette(&p)
    }
}

impl MarkdownTheme {
    /// Build a theme from the semantic palette.
    pub fn from_palette(p: &ThemePalette) -> Self {
        Self {
            h1: Style::new().bold().underlined(),
            h2: Style::new().bold(),
            h3: Style::new().bold().italic(),
            h4: Style::new().italic(),
            h5: Style::new().italic(),
            h6: Style::new().italic(),
            bold: Style::new().bold(),
            italic: Style::new().italic(),
            bold_italic: Style::new().bold().italic(),
            strikethrough: Style::new().crossed_out(),
            code: Style::new().fg(p.accent),
            code_block: Style::new().fg(p.accent),
            code_block_gutter: Style::new().fg(p.dim),
            link: Style::new().fg(p.accent).underlined(),
            blockquote: Style::new().fg(p.success),
            border: Style::new().fg(p.dim),
            base: Style::new().fg(p.text),
            dimmed: Style::new().dim(),
            heading_prefix: Style::new().fg(p.dim),
            diff_add: Style::new().fg(p.success),
            diff_del: Style::new().fg(p.danger),
            diff_hunk: Style::new().fg(p.accent).bold(),
        }
    }
}

// ============================================================
// Conversion: MarkdownLine → ratatui::Line<'static>
// ============================================================

impl From<MarkdownLine> for Line<'static> {
    fn from(line: MarkdownLine) -> Self {
        let spans: Vec<Span<'static>> = line
            .segments
            .into_iter()
            .map(|seg| Span::styled(seg.text, seg.style))
            .collect();
        Line::from(spans)
    }
}

impl<'a> From<&'a MarkdownLine> for Line<'a> {
    fn from(line: &'a MarkdownLine) -> Self {
        let spans: Vec<Span<'a>> = line
            .segments
            .iter()
            .map(|seg| Span::styled(seg.text.as_str(), seg.style))
            .collect();
        Line::from(spans)
    }
}

/// Map a markdown segment's style into the thinking block's visual layer.
///
/// Code-like and decorative elements keep their theme colors so inline code
/// and code blocks stay distinguishable inside reasoning content; prose
/// elements inherit the thinking foreground while everything else (bold,
/// italic, dim, background, underline color) is preserved untouched.
///
/// Lives at the render layer (not in the thinking cell) so the streaming
/// renderer can share the exact same recolor semantics.
pub fn thinking_segment_style(kind: SegmentKind, original: Style, thinking_style: Style) -> Style {
    match kind {
        SegmentKind::InlineCode
        | SegmentKind::CodeBlock
        | SegmentKind::Link
        | SegmentKind::Border
        | SegmentKind::Gutter => original,
        SegmentKind::Text | SegmentKind::Heading | SegmentKind::Marker => Style {
            fg: thinking_style.fg,
            ..original
        },
    }
}

/// Truncate a string to fit within a given display width (CJK-safe).
///
/// Uses UnicodeWidthChar to measure each character's display width,
/// ensuring CJK characters (2 columns) don't cause over-truncation.
pub fn truncate_to_display_width(text: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut current_width = 0;

    for ch in text.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_width + char_width > max_width {
            break;
        }
        result.push(ch);
        current_width += char_width;
    }

    result
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn segment_width_ascii() {
        let seg = MarkdownSegment::new(SegmentKind::Text, Style::new(), "hello");
        assert_eq!(seg.width(), 5);
    }

    #[test]
    fn segment_width_cjk() {
        let seg = MarkdownSegment::new(SegmentKind::Text, Style::new(), "你好");
        assert_eq!(seg.width(), 4);
    }

    #[test]
    fn segment_is_whitespace() {
        assert!(MarkdownSegment::new(SegmentKind::Text, Style::new(), "   ").is_whitespace());
        assert!(!MarkdownSegment::new(SegmentKind::Text, Style::new(), "hi").is_whitespace());
    }

    #[test]
    fn line_push_merges_same_style() {
        let mut line = MarkdownLine::default();
        let style = Style::new().bold();
        line.push_segment(SegmentKind::Text, style, "hello ");
        line.push_segment(SegmentKind::Text, style, "world");
        assert_eq!(line.segments.len(), 1);
        assert_eq!(line.segments[0].text, "hello world");
    }

    #[test]
    fn line_push_no_merge_different_style() {
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Text, Style::new().bold(), "bold");
        line.push_segment(SegmentKind::Text, Style::new().italic(), "italic");
        assert_eq!(line.segments.len(), 2);
    }

    #[test]
    fn line_push_no_merge_different_link() {
        let mut line = MarkdownLine::default();
        let style = Style::new();
        line.push_segment_with_link(SegmentKind::Text, style, "a", Some("url1".into()));
        line.push_segment_with_link(SegmentKind::Text, style, "b", Some("url2".into()));
        assert_eq!(line.segments.len(), 2);
    }

    #[test]
    fn line_push_no_merge_different_kind() {
        let mut line = MarkdownLine::default();
        let style = Style::new();
        line.push_segment(SegmentKind::Text, style, "plain ");
        line.push_segment(SegmentKind::Marker, style, "marker");
        assert_eq!(line.segments.len(), 2);
        assert_eq!(line.segments[0].kind, SegmentKind::Text);
        assert_eq!(line.segments[1].kind, SegmentKind::Marker);
    }

    #[test]
    fn line_push_merges_same_kind() {
        let mut line = MarkdownLine::default();
        let style = Style::new();
        line.push_segment(SegmentKind::InlineCode, style, "foo ");
        line.push_segment(SegmentKind::InlineCode, style, "bar");
        assert_eq!(line.segments.len(), 1);
        assert_eq!(line.segments[0].text, "foo bar");
        assert_eq!(line.segments[0].kind, SegmentKind::InlineCode);
    }

    #[test]
    fn line_push_ignores_empty_text() {
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Text, Style::new(), "");
        assert!(line.segments.is_empty());
    }

    #[test]
    fn line_is_empty() {
        let mut line = MarkdownLine::default();
        assert!(line.is_empty());
        line.push_segment(SegmentKind::Text, Style::new(), "   ");
        assert!(line.is_empty());
        line.push_segment(SegmentKind::Text, Style::new(), "hi");
        assert!(!line.is_empty());
    }

    #[test]
    fn line_width_mixed() {
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Text, Style::new(), "ab"); // 2 cols
        line.push_segment(SegmentKind::Text, Style::new(), "你好"); // 4 cols
        assert_eq!(line.width(), 6);
    }

    #[test]
    fn line_to_plain() {
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Text, Style::new().bold(), "hello ");
        line.push_segment(SegmentKind::Text, Style::new().italic(), "world");
        assert_eq!(line.to_plain(), "hello world");
    }

    #[test]
    fn line_to_ratatui_line() {
        let mut md_line = MarkdownLine::default();
        md_line.push_segment(SegmentKind::Text, Style::new().bold(), "bold");
        md_line.push_segment(SegmentKind::Text, Style::new(), " normal");

        let ratatui_line: Line<'static> = md_line.into();
        assert_eq!(ratatui_line.spans.len(), 2);
        assert_eq!(ratatui_line.spans[0].content, "bold");
        assert_eq!(ratatui_line.spans[1].content, " normal");
    }

    #[test]
    fn theme_default_has_all_styles() {
        let theme = MarkdownTheme::default();
        // Spot check a few key styles.
        assert_eq!(theme.code.fg, Some(Color::Cyan));
        assert!(
            theme
                .h1
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }
}
