//! Link handling utilities.
//!
//! Adapted from VTCode (MIT license). Simplified to remove regex dependency.
//!
//! Besides the destination-display rules, this module owns the *link span*
//! side channel: [`LinkSpan`] locates a link inside a rendered line (display
//! columns), and [`ComposedLines`] carries the spans next to the ratatui lines
//! they belong to. `Line`/`Span` themselves have no room for a target, so the
//! target travels beside them — from the markdown IR to the OSC8 injection and
//! the click hit test (see the `tui-link-open` change).

use std::borrow::Cow;

use unicode_width::UnicodeWidthStr;

use super::types::MarkdownLine;
use super::types::MarkdownSegment;

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::types::SegmentKind;

/// A link inside one rendered line.
///
/// `start..end` is a half-open range of **display columns relative to the line
/// start** (what the terminal shows, not bytes, not chars). The range is stable
/// across styling, wide characters and the cell prefix — the only thing that
/// shifts it is content before the link on the same line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    /// First display column of the link text (inclusive).
    pub start: u16,
    /// One past the last display column of the link text (exclusive).
    pub end: u16,
    /// The markdown link destination (not sanitised — see [`sanitize_osc8_target`]).
    pub target: String,
}

/// Link spans of a single rendered line, in left-to-right order.
pub fn line_link_spans(line: &MarkdownLine) -> Vec<LinkSpan> {
    let mut spans = Vec::new();
    let mut col: usize = 0;
    for seg in &line.segments {
        let width = seg.width();
        if let Some(target) = seg.link_target.as_ref() {
            spans.push(LinkSpan {
                start: col as u16,
                end: (col + width) as u16,
                target: target.clone(),
            });
        }
        col += width;
    }
    spans
}

/// [`line_link_spans`] for a whole render, parallel to the input lines.
///
/// Lines without links get an empty vector, so the result can be indexed by
/// line number (`links_of(line)` == `links[line]`).
pub fn links_for_lines(lines: &[MarkdownLine]) -> Vec<Vec<LinkSpan>> {
    lines.iter().map(line_link_spans).collect()
}

// ============================================================
// OSC8 hyperlinks
// ============================================================

/// OSC8 hyperlink opener: `ESC ] 8 ; ; <target> ST`.
///
/// `ST` is the string terminator (`ESC \`) — the form Ghostty / kitty / iTerm2
/// all accept, and the one the OSC 8 specification spells out.
pub fn osc8_open(target: &str) -> String {
    format!("\u{1b}]8;;{target}\u{1b}\\")
}

/// OSC8 hyperlink closer: `ESC ] 8 ; ; ST`.
pub fn osc8_close() -> String {
    "\u{1b}]8;;\u{1b}\\".to_string()
}

/// Strip everything that could terminate or forge the escape sequence.
///
/// The target comes from model output (markdown), so it must never be able to
/// close the OSC 8 sequence early or start one of its own: a raw `ESC` (or
/// `BEL`, or any other control character) in a link destination would let the
/// message paint arbitrary terminal state. C0, C1 and DEL all go.
pub fn sanitize_osc8_target(raw: &str) -> String {
    raw.chars().filter(|c| !is_control_char(*c)).collect()
}

fn is_control_char(c: char) -> bool {
    c.is_control() || matches!(c, '\u{7f}'..='\u{9f}')
}

/// Remove every OSC8 sequence from a rendered symbol.
///
/// The injection in [`crate::ui::chat_view`] puts the sequences into `Cell`
/// symbols; anything that reads the buffer back (text extraction for copy,
/// width measurement, tests) must see the *displayed* text only. Returns the
/// input untouched (borrowed) when there is no sequence, so the hot path costs
/// one substring search.
pub fn strip_osc8(symbol: &str) -> Cow<'_, str> {
    const PREFIX: &str = "\u{1b}]8;;";
    if !symbol.contains(PREFIX) {
        return Cow::Borrowed(symbol);
    }
    let mut out = String::with_capacity(symbol.len());
    let mut rest = symbol;
    while let Some(start) = rest.find(PREFIX) {
        out.push_str(&rest[..start]);
        let after = &rest[start + PREFIX.len()..];
        // The sequence ends at ST (ESC \) or BEL — whichever comes first.
        let end = match (after.find('\u{1b}'), after.find('\u{7}')) {
            (Some(esc), Some(bel)) if esc < bel => {
                if after[esc..].starts_with("\u{1b}\\") {
                    esc + 2
                } else {
                    esc + 1
                }
            }
            (Some(_esc), Some(bel)) => bel + 1,
            (Some(esc), None) => {
                if after[esc..].starts_with("\u{1b}\\") {
                    esc + 2
                } else {
                    esc + 1
                }
            }
            (None, Some(bel)) => bel + 1,
            // Unterminated sequence: drop the rest.
            (None, None) => return Cow::Owned(out),
        };
        rest = &after[end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Display width of a rendered symbol, ignoring an injected OSC8 sequence.
pub fn symbol_width(symbol: &str) -> u16 {
    match strip_osc8(symbol) {
        Cow::Borrowed(s) => s.width() as u16,
        Cow::Owned(s) => s.width() as u16,
    }
}

// ============================================================
// ComposedLines — rendered lines + their link spans
// ============================================================/// Rendered cell content: ratatui lines plus, for every line, the link spans
/// the OSC8 injection and the click hit test need.
///
/// `links` is always parallel to `lines` (a line without links has an empty
/// entry), which keeps the two in sync through every transform the renderers
/// apply (prefixing, streaming promotion, hard wrapping).
#[derive(Debug, Clone, Default)]
pub struct ComposedLines {
    lines: Vec<Line<'static>>,
    links: Vec<Vec<LinkSpan>>,
}

impl ComposedLines {
    /// Pair lines with their per-line link spans.
    ///
    /// `links` may be empty (the "no links anywhere" case) or shorter than
    /// `lines`; it is padded with empty entries so the two are always parallel.
    pub fn new(lines: Vec<Line<'static>>, links: Vec<Vec<LinkSpan>>) -> Self {
        let mut links = links;
        links.truncate(lines.len());
        links.resize(lines.len(), Vec::new());
        Self { lines, links }
    }

    /// Lines with no links at all (every non-markdown cell, and markdown
    /// without links).
    pub fn plain(lines: Vec<Line<'static>>) -> Self {
        let links = vec![Vec::new(); lines.len()];
        Self { lines, links }
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// Append a blank line (kept parallel with the spans).
    pub fn push_blank(&mut self) {
        self.lines.push(Line::from(""));
        self.links.push(Vec::new());
    }

    pub fn links(&self) -> &[Vec<LinkSpan>] {
        &self.links
    }

    /// Link spans of one rendered line.
    pub fn links_at(&self, line: usize) -> &[LinkSpan] {
        self.links.get(line).map_or(&[], Vec::as_slice)
    }

    /// Whether any line carries a link.
    pub fn has_links(&self) -> bool {
        self.links.iter().any(|l| !l.is_empty())
    }

    /// Whether every line fits `width` — the precondition for "screen row ==
    /// line index".
    ///
    /// `Paragraph` with `Wrap { trim: false }` never splits a line that fits,
    /// so when this holds the widget's row arithmetic is exact and link columns
    /// computed here land on the right cells. It is computed lazily (the
    /// producer only asks when the cell actually has links).
    pub fn rows_are_exact(&self, width: u16) -> bool {
        self.lines.iter().all(|line| line.width() <= width as usize)
    }

    pub fn into_lines(self) -> Vec<Line<'static>> {
        self.lines
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }
}

/// Turn markdown IR lines into a cell's final lines + link spans.
///
/// The cell renderers all follow the same shape — one prefix span per line
/// (`⦁ ` / `  ` / `? `) followed by the line's segments, optionally with a
/// per-segment style rewrite (the thinking recolor). Doing it here keeps the
/// link columns consistent with the spans that actually reach the buffer: the
/// spans are shifted by `prefix_width`, so the widget can lay them out
/// directly.
pub fn compose_lines(
    md_lines: &[MarkdownLine],
    prefix_width: u16,
    mut prefix: impl FnMut(usize) -> Span<'static>,
    mut map_style: impl FnMut(SegmentKind, Style) -> Style,
) -> ComposedLines {
    let mut lines = Vec::with_capacity(md_lines.len());
    let mut links = Vec::with_capacity(md_lines.len());
    for (i, md_line) in md_lines.iter().enumerate() {
        let mut spans = Vec::with_capacity(md_line.segments.len() + 1);
        spans.push(prefix(i));
        for seg in &md_line.segments {
            spans.push(Span::styled(
                seg.text.clone(),
                map_style(seg.kind, seg.style),
            ));
        }
        lines.push(Line::from(spans));
        links.push(
            line_link_spans(md_line)
                .into_iter()
                .map(|span| LinkSpan {
                    start: span.start.saturating_add(prefix_width),
                    end: span.end.saturating_add(prefix_width),
                    target: span.target,
                })
                .collect(),
        );
    }
    ComposedLines::new(lines, links)
}

/// Whether to render the link destination URL after the link text.
///
/// Local file paths are hidden (the link text alone is sufficient),
/// while remote URLs are shown for reference.
pub(crate) fn should_render_link_destination(dest_url: &str) -> bool {
    !is_local_path_like_link(dest_url)
}

/// Check if any of the label segments already have a location suffix.
pub(crate) fn label_segments_have_location_suffix(segments: &[MarkdownSegment]) -> bool {
    let Some(last) = segments.last() else {
        return false;
    };
    if label_has_location_suffix(&last.text) {
        return true;
    }
    if segments.len() == 1 {
        return false;
    }
    let mut label = String::with_capacity(segments.iter().map(|s| s.text.len()).sum());
    for segment in segments {
        label.push_str(&segment.text);
    }
    label_has_location_suffix(&label)
}

/// Extract a hidden location suffix from a local file link destination.
///
/// For links like `[text](./file.rs#L10)`, returns `Some("#L10")`.
pub(crate) fn extract_hidden_location_suffix(dest_url: &str) -> Option<String> {
    if !is_local_path_like_link(dest_url) {
        return None;
    }
    // Look for hash location suffix like #L10, #L10C5.
    if let Some((_, fragment)) = dest_url.rsplit_once('#')
        && is_hash_location_suffix(fragment)
    {
        return Some(format!("#{fragment}"));
    }
    // Look for colon location suffix like :10, :10:5.
    if let Some(suffix) = extract_colon_suffix(dest_url) {
        return Some(suffix);
    }
    None
}

fn label_has_location_suffix(text: &str) -> bool {
    // Check for hash suffix like #L10.
    if let Some((_, fragment)) = text.rsplit_once('#')
        && is_hash_location_suffix(fragment)
    {
        return true;
    }
    // Check for colon suffix like :10 or :10:5.
    extract_colon_suffix(text).is_some()
}

/// Check if fragment matches `L\d+(C\d+)?` pattern.
fn is_hash_location_suffix(fragment: &str) -> bool {
    let s = fragment;
    if !s.starts_with('L') {
        return false;
    }
    let after_l = &s[1..];
    if after_l.is_empty() {
        return false;
    }
    // Parse digits.
    let digits_end = after_l
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_l.len());
    if digits_end == 0 {
        return false;
    }
    let rest = &after_l[digits_end..];
    if rest.is_empty() {
        return true; // L10
    }
    // Optional C\d+ part.
    if !rest.starts_with('C') {
        return false;
    }
    let after_c = &rest[1..];
    !after_c.is_empty() && after_c.chars().all(|c| c.is_ascii_digit())
}

/// Extract trailing `:digits` or `:digits:digits` suffix.
fn extract_colon_suffix(text: &str) -> Option<String> {
    // Find the last colon that starts a number.
    let bytes = text.as_bytes();
    let len = bytes.len();
    if len < 2 {
        return None;
    }

    // Walk backwards to find `:digits` or `:digits:digits`.
    let mut i = len;
    // Find trailing digits.
    while i > 0 && bytes[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i == len || i == 0 || bytes[i - 1] != b':' {
        return None;
    }
    let colon_pos = i - 1;
    // Check for optional second `:digits` part.
    if colon_pos > 0 {
        let before = &text[..colon_pos];
        if let Some(second_colon) = before.rfind(':') {
            let between = &before[second_colon + 1..];
            if !between.is_empty() && between.chars().all(|c| c.is_ascii_digit()) {
                return Some(text[second_colon..].to_string());
            }
        }
    }
    Some(text[colon_pos..].to_string())
}

/// Whether a destination looks like a local path rather than a URL.
///
/// Shared with the opener (`util::open`) so classification cannot drift
/// between "how a link renders" and "what a click opens".
pub(crate) fn is_local_path_like_link(dest_url: &str) -> bool {
    dest_url.starts_with("file://")
        || dest_url.starts_with('/')
        || dest_url.starts_with("~/")
        || dest_url.starts_with("./")
        || dest_url.starts_with("../")
        || dest_url.starts_with("\\\\")
        || matches!(
            dest_url.as_bytes(),
            [drive, b':', separator, ..]
                if drive.is_ascii_alphabetic() && matches!(separator, b'/' | b'\\')
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::markdown::types::SegmentKind;

    #[test]
    fn remote_url_should_render() {
        assert!(should_render_link_destination("https://example.com"));
        assert!(should_render_link_destination("http://foo.bar"));
    }

    #[test]
    fn local_path_should_not_render() {
        assert!(!should_render_link_destination("./src/main.rs"));
        assert!(!should_render_link_destination("../lib.rs"));
        assert!(!should_render_link_destination("~/config"));
        assert!(!should_render_link_destination("/absolute/path"));
        assert!(!should_render_link_destination("file:///foo"));
    }

    #[test]
    fn extract_hash_location_suffix() {
        assert_eq!(
            extract_hidden_location_suffix("./file.rs#L10"),
            Some("#L10".to_string())
        );
        assert_eq!(
            extract_hidden_location_suffix("./file.rs#L10C5"),
            Some("#L10C5".to_string())
        );
        assert_eq!(extract_hidden_location_suffix("./file.rs"), None);
    }

    #[test]
    fn extract_colon_location_suffix() {
        assert_eq!(
            extract_hidden_location_suffix("./file.rs:10"),
            Some(":10".to_string())
        );
        assert_eq!(
            extract_hidden_location_suffix("./file.rs:10:5"),
            Some(":10:5".to_string())
        );
    }

    #[test]
    fn no_suffix_for_remote_url() {
        assert_eq!(extract_hidden_location_suffix("https://example.com"), None);
    }

    #[test]
    fn label_has_location_suffix_detection() {
        assert!(label_has_location_suffix("main.rs:10"));
        assert!(label_has_location_suffix("main.rs#L10"));
        assert!(!label_has_location_suffix("main.rs"));
    }

    #[test]
    fn hash_location_suffix_validation() {
        assert!(is_hash_location_suffix("L10"));
        assert!(is_hash_location_suffix("L10C5"));
        assert!(!is_hash_location_suffix("L"));
        assert!(!is_hash_location_suffix("LC5"));
        assert!(!is_hash_location_suffix("foo"));
    }

    // ── Link spans ────────────────────────────────────────────────

    fn line(segments: Vec<MarkdownSegment>) -> MarkdownLine {
        MarkdownLine { segments }
    }

    fn seg(kind: SegmentKind, text: &str, target: Option<&str>) -> MarkdownSegment {
        MarkdownSegment::with_link(
            kind,
            ratatui::style::Style::default(),
            text,
            target.map(String::from),
        )
    }

    #[test]
    fn line_link_spans_locates_a_link_in_the_middle_of_a_line() {
        let l = line(vec![
            seg(SegmentKind::Text, "see ", None),
            seg(SegmentKind::Link, "docs", Some("https://example.com")),
            seg(SegmentKind::Text, " now", None),
        ]);
        let spans = line_link_spans(&l);
        assert_eq!(
            spans,
            vec![LinkSpan {
                start: 4,
                end: 8,
                target: "https://example.com".into()
            }]
        );
    }

    #[test]
    fn line_link_spans_handles_two_links_and_missing_targets() {
        let l = line(vec![
            seg(SegmentKind::Link, "a", Some("https://a")),
            seg(SegmentKind::Text, " and ", None),
            seg(SegmentKind::Link, "b", Some("https://b")),
            seg(SegmentKind::Link, "c", None),
        ]);
        let spans = line_link_spans(&l);
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].start, spans[0].end), (0, 1));
        assert_eq!((spans[1].start, spans[1].end), (6, 7));
    }

    #[test]
    fn line_link_spans_uses_display_columns_for_cjk() {
        let l = line(vec![
            seg(SegmentKind::Text, "见", None),
            seg(SegmentKind::Link, "你好", Some("https://example.com")),
        ]);
        let spans = line_link_spans(&l);
        assert_eq!((spans[0].start, spans[0].end), (2, 6));
    }

    #[test]
    fn line_link_spans_is_empty_for_lines_without_links() {
        let l = line(vec![seg(SegmentKind::Text, "nothing here", None)]);
        assert!(line_link_spans(&l).is_empty());
        assert!(links_for_lines(&[l]).iter().all(Vec::is_empty));
    }

    #[test]
    fn links_for_lines_is_parallel_to_the_input() {
        let lines = vec![
            line(vec![seg(SegmentKind::Link, "a", Some("https://a"))]),
            line(vec![seg(SegmentKind::Text, "", None)]),
            line(vec![seg(SegmentKind::Link, "b", Some("https://b"))]),
        ];
        let links = links_for_lines(&lines);
        assert_eq!(links.len(), lines.len());
        assert_eq!(links[0].len(), 1);
        assert!(links[1].is_empty());
        assert_eq!(links[2][0].target, "https://b");
    }

    // ── OSC8 ──────────────────────────────────────────────────────

    #[test]
    fn osc8_sequences_have_the_documented_shape() {
        assert_eq!(
            osc8_open("https://example.com"),
            "\u{1b}]8;;https://example.com\u{1b}\\"
        );
        assert_eq!(osc8_close(), "\u{1b}]8;;\u{1b}\\");
    }

    #[test]
    fn sanitize_osc8_target_strips_control_characters() {
        assert_eq!(
            sanitize_osc8_target("https://e\u{1b}]8;;evil\u{7}x"),
            "https://e]8;;evilx"
        );
        assert_eq!(sanitize_osc8_target("a\u{0}b\u{7f}c"), "abc");
        assert_eq!(
            sanitize_osc8_target("https://ok/path?q=1#frag"),
            "https://ok/path?q=1#frag"
        );
    }

    #[test]
    fn strip_osc8_returns_the_visible_text() {
        let symbol = format!("{}docs{}", osc8_open("https://example.com"), osc8_close());
        assert_eq!(strip_osc8(&symbol), "docs");
        assert_eq!(symbol_width(&symbol), 4);
        // Idempotent, and clean input is borrowed (no allocation).
        assert!(matches!(strip_osc8("docs"), Cow::Borrowed(_)));
        assert_eq!(strip_osc8(&strip_osc8(&symbol)), "docs");
    }

    #[test]
    fn strip_osc8_handles_wide_symbols_and_bel_terminator() {
        assert_eq!(
            symbol_width(&format!("{}你{}", osc8_open("x"), osc8_close())),
            2
        );
        assert_eq!(strip_osc8("\u{1b}]8;;https://e\u{7}hi"), "hi");
        // Truncated sequence: everything after the opener goes.
        assert_eq!(strip_osc8("\u{1b}]8;;https://e"), "");
    }

    // ── ComposedLines ─────────────────────────────────────────────

    #[test]
    fn composed_lines_pads_links_to_the_line_count() {
        let lines = vec![Line::from("a"), Line::from("b")];
        let composed = ComposedLines::new(lines.clone(), vec![]);
        assert_eq!(composed.links().len(), 2);
        assert!(!composed.has_links());
        assert_eq!(composed.into_lines(), lines);
    }

    #[test]
    fn composed_lines_rows_are_exact_only_when_every_line_fits() {
        let composed = ComposedLines::plain(vec![Line::from("12345"), Line::from("12345")]);
        assert!(composed.rows_are_exact(5));
        assert!(composed.rows_are_exact(10));
        assert!(!composed.rows_are_exact(4));
    }
}
