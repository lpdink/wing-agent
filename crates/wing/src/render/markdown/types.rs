//! Core data types for the markdown renderer.
//!
//! `MarkdownSegment` and `MarkdownLine` are the intermediate representation
//! produced by the parser. They are converted to `ratatui::text::Line` for
//! actual terminal rendering.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;

/// 一行的显示宽度：各 span 内容宽度之和（CJK / 表情按显示列算）。
pub(crate) fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

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
    /// Blockquote content: the quoted voice (the bar is chrome, [`Self::Border`]).
    Quote,
    /// Decorative chrome: code block frames, table rules, horizontal rules,
    /// blockquote bars.
    Border,
    /// Code block line-number gutter.
    Gutter,
    /// Math (LaTeX) content: a rendered formula or, when the engine declines
    /// to render it, the formula's source between its delimiters.
    Math,
    /// An image anchor: the caption line of a `![alt](path)` block, and the
    /// blank cover rows under it (see [`super::images`]). Never produced in
    /// [`ImageMode::Off`](super::images::ImageMode::Off), where an image
    /// renders as a link.
    Image,
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
    /// Image anchor payload, present on the anchor's caption line (see
    /// [`super::images`]). The line stays a single line here; the block's
    /// cover rows are expanded at compose time, so the markdown layer's
    /// line-count rules (trailing-blank trimming, blank-line dedup, the
    /// streaming engine's block separators) never see the block's height.
    pub image: Option<Box<super::images::ImageAnchor>>,
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
    /// Emphasis (`**bold**`): the `strong` hue plus weight, applied to plain
    /// prose only — see [`MarkdownTheme::emphasized`].
    pub strong: Style,
    pub italic: Style,
    pub strikethrough: Style,
    /// Inline code (`` `code` ``) — and the plain text of an unhighlighted
    /// code block, so "literal things" read as one family.
    pub code: Style,
    pub code_block: Style,
    pub code_block_gutter: Style,
    /// Rendered math grids and their literal fallback (see
    /// [`crate::render::markdown::math`]).
    pub math: Style,
    pub link: Style,
    /// Blockquote: the quoted voice and its bar.
    pub blockquote: Style,
    /// The blockquote's leading `│` bar (a quieter take on `blockquote`).
    pub quote_bar: Style,
    /// List marks: bullets, ordered numbers, task boxes.
    pub marker: Style,
    pub border: Style,
    pub base: Style,
    pub dimmed: Style,
    /// Diff addition lines (+): marker color.
    pub diff_add: Style,
    /// Diff deletion lines (-): marker color.
    pub diff_del: Style,
    /// Diff hunk headers (@@).
    pub diff_hunk: Style,
    /// Diff addition row background (code text keeps syntax colors).
    pub diff_add_bg: Style,
    /// Diff deletion row background.
    pub diff_del_bg: Style,
    /// Which syntect theme code blocks are tinted with. Carried here (not read
    /// from a global) so the light and dark palettes can be rendered side by
    /// side — `theme_preview` does exactly that.
    pub syntax: crate::config::SyntaxTheme,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        let p = ThemePalette::default();
        Self::from_palette(&p)
    }
}

impl MarkdownTheme {
    /// Build a theme from the semantic palette.
    ///
    /// Every element takes its hue from a palette slot (structure violet,
    /// literals rose, interaction cyan, state green/amber/red); hierarchy
    /// *inside* a role comes from weight (see the heading ladder).
    pub fn from_palette(p: &ThemePalette) -> Self {
        let heading = Style::new().fg(p.heading);
        Self {
            // Level 1 takes the top of the ladder (lifted heading hue) — an
            // underline would read as a link, so the ramp is colour + weight.
            h1: Style::new().fg(p.heading_bright).bold(),
            h2: heading.bold(),
            h3: heading.bold().italic(),
            h4: heading.italic(),
            h5: heading.italic(),
            h6: heading.italic(),
            strong: Style::new().fg(p.strong).bold(),
            italic: Style::new().italic(),
            strikethrough: Style::new().crossed_out(),
            code: Style::new().fg(p.inline_code),
            code_block: Style::new().fg(p.inline_code),
            code_block_gutter: Style::new().fg(p.dim),
            math: Style::new().fg(p.math),
            link: Style::new().fg(p.accent).underlined(),
            blockquote: Style::new().fg(p.quote),
            quote_bar: Style::new().fg(p.quote).add_modifier(Modifier::DIM),
            marker: Style::new().fg(p.marker),
            border: Style::new().fg(p.dim),
            base: Style::new().fg(p.text),
            dimmed: Style::new().dim(),
            diff_add: Style::new().fg(p.success),
            diff_del: Style::new().fg(p.danger),
            diff_hunk: Style::new().fg(p.accent).bold(),
            diff_add_bg: Style::new().bg(p.diff_add_bg),
            diff_del_bg: Style::new().bg(p.diff_del_bg),
            syntax: p.syntax,
        }
    }

    /// The `**bold**` patch, resolved against the enclosing style.
    ///
    /// Weight everywhere; the `strong` hue only where the voice is plain
    /// prose. A heading, a link or inline code keeps its own hue — emphasis
    /// inside a role must not repaint that role.
    pub fn emphasized(&self, current: Style) -> Style {
        let prose = current.fg.is_none() || current.fg == self.base.fg;
        current.patch(if prose {
            self.strong
        } else {
            Style::new().bold()
        })
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
/// Elements that carry a *role hue* keep their theme colours, so structure and
/// literals stay distinguishable inside reasoning content: code, links,
/// headings, quotes, math, list marks and chrome. Only plain prose inherits the
/// thinking foreground; weight, style, background and underline are preserved
/// untouched either way.
///
/// Math is structural like code — a rendered formula (and its source
/// fallback) keeps the math color in both profiles.
///
/// Lives at the render layer (not in the thinking cell) so the streaming
/// renderer can share the exact same recolor semantics.
pub fn thinking_segment_style(kind: SegmentKind, original: Style, thinking_style: Style) -> Style {
    match kind {
        SegmentKind::InlineCode
        | SegmentKind::CodeBlock
        | SegmentKind::Link
        | SegmentKind::Border
        | SegmentKind::Gutter
        | SegmentKind::Math
        | SegmentKind::Heading
        | SegmentKind::Quote
        | SegmentKind::Marker
        | SegmentKind::Image => original,
        SegmentKind::Text => Style {
            fg: thinking_style.fg,
            ..original
        },
    }
}

/// Truncate from the **left**, keeping the tail (`…/ws/wing-agent`).
///
/// 路径类文本的区分度在尾段：右截断（保留头部）留给所有路径一串相同的
/// `/Users/…` 前缀，等于什么都没说。省略号占 1 列、计入预算；本身放得下
/// 的文本原样返回（可以无条件调用）。宽度按 `UnicodeWidthChar` 记账（CJK 安全）。
pub fn truncate_left_to_display_width(text: &str, max_width: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let budget = max_width - 1; // 省略号的 1 列
    let mut tail: Vec<char> = Vec::new();
    let mut width = 0;
    for ch in text.chars().rev() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > budget {
            break;
        }
        width += char_width;
        tail.push(ch);
    }
    tail.reverse();
    format!("…{}", tail.into_iter().collect::<String>())
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
    fn truncate_left_keeps_the_tail_and_counts_the_ellipsis() {
        // 放得下就原样返回（可以无条件调用）。
        assert_eq!(truncate_left_to_display_width("/ws/wing", 20), "/ws/wing");
        // 省略号占 1 列，计入预算：总宽不超过 max_width。
        let out = truncate_left_to_display_width("/Users/me/ws/wing-agent", 12);
        assert_eq!(out, "…/wing-agent");
        assert_eq!(UnicodeWidthStr::width(out.as_str()), 12);
        // CJK 安全（按列不按字节）。
        let cjk = truncate_left_to_display_width("前缀/中文目录", 8);
        assert!(UnicodeWidthStr::width(cjk.as_str()) <= 8, "{cjk:?}");
        assert!(cjk.starts_with('…'));
        // 退化输入不炸。
        assert_eq!(truncate_left_to_display_width("abc", 0), "");
        assert_eq!(truncate_left_to_display_width("", 5), "");
    }

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
        let palette = ThemePalette::default();
        // Spot check a few key styles: the theme is palette-derived, not
        // hardcoded (a slot value change flows through).
        assert_eq!(theme.code.fg, Some(palette.inline_code));
        assert_eq!(theme.code_block.fg, Some(palette.inline_code));
        assert_eq!(theme.math.fg, Some(palette.math));
        assert_eq!(theme.h1.fg, Some(palette.heading_bright));
        assert_eq!(theme.h2.fg, Some(palette.heading));
        assert_eq!(theme.marker.fg, Some(palette.marker));
        assert_eq!(theme.blockquote.fg, Some(palette.quote));
        assert!(
            theme
                .h1
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        // The heading ladder is weight-only past level 1: an underline would
        // read as a link.
        assert!(
            !theme
                .h1
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
        );
    }

    /// Every role hue is one palette slot apart — the point of the redesign is
    /// that a document is not one colour (code ≠ link ≠ heading).
    #[test]
    fn the_theme_roles_stay_apart() {
        let theme = MarkdownTheme::default();
        let fgs = [
            theme.base.fg,
            theme.code.fg,
            theme.link.fg,
            theme.h2.fg,
            theme.marker.fg,
            theme.blockquote.fg,
        ];
        for (i, a) in fgs.iter().enumerate() {
            for b in fgs.iter().skip(i + 1) {
                assert_ne!(a, b, "two roles collapsed onto {a:?}: {fgs:?}");
            }
        }
    }

    /// Emphasis lifts plain prose but never repaints a role: `**bold**` inside
    /// a heading or a link keeps that element's hue.
    #[test]
    fn emphasis_lifts_prose_and_keeps_role_hues() {
        let palette = ThemePalette {
            text: Color::Rgb(0x80, 0x80, 0x80),
            strong: Color::Rgb(0xf0, 0xf0, 0xf0),
            ..ThemePalette::default()
        };
        let theme = MarkdownTheme::from_palette(&palette);
        // Prose (base fg, or no fg at all) takes the lifted colour.
        for current in [theme.base, Style::new()] {
            let strong = theme.emphasized(current);
            assert_eq!(strong.fg, Some(palette.strong), "{current:?}");
            assert!(strong.add_modifier.contains(Modifier::BOLD));
        }
        // A heading / link / inline code keeps its own hue, only the weight is
        // added.
        for current in [theme.h2, theme.link, theme.code] {
            let strong = theme.emphasized(current);
            assert_eq!(strong.fg, current.fg, "{current:?}");
            assert!(strong.add_modifier.contains(Modifier::BOLD));
        }
        // Italic composes with bold (`***x***`).
        let bold_italic = theme.emphasized(theme.italic);
        assert!(bold_italic.add_modifier.contains(Modifier::BOLD));
        assert!(bold_italic.add_modifier.contains(Modifier::ITALIC));
    }

    /// The math slot follows the palette (it is a real theme color, not a
    /// hardcoded one), and the thinking recolor keeps it: a formula stays
    /// the math color inside reasoning, like code does.
    #[test]
    fn theme_math_follows_palette_and_survives_thinking_recolor() {
        let palette = ThemePalette {
            math: Color::Magenta,
            inline_code: Color::Blue,
            ..ThemePalette::default()
        };
        let theme = MarkdownTheme::from_palette(&palette);
        assert_eq!(theme.math.fg, Some(Color::Magenta));
        assert_eq!(theme.code.fg, Some(Color::Blue));

        for kind in [SegmentKind::Math, SegmentKind::InlineCode] {
            let recolored = thinking_segment_style(kind, theme.math, Style::new().fg(Color::Gray));
            assert_eq!(recolored.fg, Some(Color::Magenta), "{kind:?}");
        }
        // Prose still takes the thinking foreground.
        assert_eq!(
            thinking_segment_style(SegmentKind::Text, theme.base, Style::new().fg(Color::Gray)).fg,
            Some(Color::Gray)
        );
    }

    /// Structure keeps its hue inside reasoning: a heading, a quote and a list
    /// mark are still recognisable there (only prose goes grey).
    #[test]
    fn thinking_keeps_the_structural_hues() {
        let theme = MarkdownTheme::default();
        let thinking = Style::new().fg(Color::Gray);
        for style in [theme.h2, theme.blockquote, theme.marker] {
            let recolored = thinking_segment_style(SegmentKind::Heading, style, thinking);
            assert_eq!(recolored.fg, style.fg);
        }
        // Quote and marker are their own kinds — same answer.
        for (kind, style) in [
            (SegmentKind::Quote, theme.blockquote),
            (SegmentKind::Marker, theme.marker),
        ] {
            let recolored = thinking_segment_style(kind, style, thinking);
            assert_eq!(recolored.fg, style.fg, "{kind:?}");
        }
    }
}
