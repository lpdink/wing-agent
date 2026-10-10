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

use super::images::{ImageAnchor, ImageSpan, cover_span, span_for_anchor};
use super::types::MarkdownLine;
use super::types::MarkdownSegment;

use ratatui::buffer::CellWidth;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::types::SegmentKind;

/// Width of the per-line cell prefix (`⦁ ` / `  ` / `? `) that every cell
/// renderer prepends before the markdown content.
///
/// Link columns are measured inside the markdown line, so they must be shifted
/// by this much before they can address screen columns — the streaming engine,
/// the assistant cell and the thinking cell all use the same prefix.
pub(crate) const CELL_PREFIX_WIDTH: u16 = 2;

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
    // `char::is_control` is Unicode `Cc`: C0, DEL and C1 — exactly the set that
    // can terminate or forge an escape sequence.
    c.is_control()
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

/// Display width of a rendered symbol, ignoring an injected OSC8 sequence —
/// the columns the terminal advances for it.
///
/// This is ratatui's own measure ([`CellWidth for str`]) rather than plain
/// `unicode-width`, and the two differ in exactly one place: halfwidth katakana
/// sound marks (`U+FF9E` / `U+FF9F`) are `Grapheme_Extend`, so `unicode-width`
/// calls them zero-width, while terminals render them in a column of their own —
/// and ratatui compensates (`count_halfwidth_sound_marks`). `Buffer::set_stringn`
/// lays a `ｶﾞ` out in two columns for that reason, so anything that walks or
/// measures the cells has to use the same ruler: the diff and the backend's
/// cursor model do, and a caller that pins a *declared* width (the OSC8
/// injection, [`crate::ui::emoji_width`]) has to as well or the terminal drifts
/// by a column.
///
/// [`CellWidth for str`]: ratatui::buffer::CellWidth
pub fn symbol_width(symbol: &str) -> u16 {
    strip_osc8(symbol).cell_width()
}

// ============================================================
// ComposedLines — rendered lines + their link spans
// ============================================================/// Rendered cell content: ratatui lines plus, for every line, the side
/// channels the ui layer needs — the link spans (OSC8 injection, click hit
/// testing) and the image anchors (the drawing layer).
///
/// `links` and `images` are always parallel to `lines` (a line without a link
/// or an anchor has an empty entry), which keeps them in sync through every
/// transform the renderers apply (prefixing, streaming promotion, hard
/// wrapping).
#[derive(Debug, Clone, Default)]
pub struct ComposedLines {
    lines: Vec<Line<'static>>,
    links: Vec<Vec<LinkSpan>>,
    images: Vec<Vec<ImageSpan>>,
}

impl ComposedLines {
    /// Pair lines with their per-line link spans (no image anchors).
    ///
    /// `links` may be empty (the "no links anywhere" case) or shorter than
    /// `lines`; it is padded with empty entries so the two are always parallel.
    pub fn new(lines: Vec<Line<'static>>, links: Vec<Vec<LinkSpan>>) -> Self {
        Self::with_images(lines, links, Vec::new())
    }

    /// [`new`](Self::new) with image anchors as well.
    ///
    /// Both side-channel vectors are padded/truncated to the line count.
    pub fn with_images(
        lines: Vec<Line<'static>>,
        links: Vec<Vec<LinkSpan>>,
        images: Vec<Vec<ImageSpan>>,
    ) -> Self {
        let mut links = links;
        links.truncate(lines.len());
        links.resize(lines.len(), Vec::new());
        let mut images = images;
        images.truncate(lines.len());
        images.resize(lines.len(), Vec::new());
        Self {
            lines,
            links,
            images,
        }
    }

    /// Lines with no links at all (every non-markdown cell, and markdown
    /// without links).
    pub fn plain(lines: Vec<Line<'static>>) -> Self {
        let links = vec![Vec::new(); lines.len()];
        let images = vec![Vec::new(); lines.len()];
        Self {
            lines,
            links,
            images,
        }
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    /// Append a blank line (kept parallel with the side channels).
    pub fn push_blank(&mut self) {
        self.lines.push(Line::from(""));
        self.links.push(Vec::new());
        self.images.push(Vec::new());
    }

    pub fn links(&self) -> &[Vec<LinkSpan>] {
        &self.links
    }

    /// Image anchors of every line, index-aligned with [`lines`](Self::lines).
    pub fn images(&self) -> &[Vec<ImageSpan>] {
        &self.images
    }

    /// The anchor payload of every line (the pre-compose form).
    ///
    /// Composed lines have already expanded their anchors into rows, so the
    /// payload is rebuilt from the side channel — the fields round-trip
    /// exactly. Used to carry anchors through a hard wrap (the wrap moves
    /// rows around, so the payload has to ride the line it belongs to).
    pub(crate) fn image_tags(&self) -> Vec<Option<ImageAnchor>> {
        let mut tags = vec![None; self.lines.len()];
        for (index, spans) in self.images.iter().enumerate() {
            if let Some(span) = spans.first() {
                tags[index] = Some(ImageAnchor {
                    path: span.path.clone(),
                    alt: span.alt.clone(),
                    shape: super::images::ImageShape::new(span.px_w, span.px_h),
                    cols: span.cols,
                    rows: span.rows,
                });
            }
        }
        tags
    }

    /// Split into the three parallel vectors (moving them out).
    pub fn into_parts(self) -> (Vec<Line<'static>>, Vec<Vec<LinkSpan>>, Vec<Vec<ImageSpan>>) {
        (self.lines, self.links, self.images)
    }

    /// Whether any line carries a link.
    pub fn has_links(&self) -> bool {
        self.links.iter().any(|l| !l.is_empty())
    }

    /// Whether any line opens an image anchor.
    pub fn has_images(&self) -> bool {
        self.images.iter().any(|l| !l.is_empty())
    }

    /// Whether any line carries a side channel at all — the gate for the row
    /// arithmetic ("screen row == line index") that both of them depend on.
    pub fn has_spans(&self) -> bool {
        self.has_links() || self.has_images()
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
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(md_lines.len());
    let mut links: Vec<Vec<LinkSpan>> = Vec::with_capacity(md_lines.len());
    let mut images: Vec<Vec<ImageSpan>> = Vec::with_capacity(md_lines.len());
    for md_line in md_lines {
        // The composed row index is not the markdown line index once an
        // anchor expands into its cover rows, so the prefix closure is fed
        // the *output* index (the `⦁ ` bullet belongs to the first row).
        let out_index = lines.len();
        let mut spans = Vec::with_capacity(md_line.segments.len() + 1);
        spans.push(prefix(out_index));
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
        // An anchor's caption row is this line; its cover rows follow it.
        let anchor = md_line.image.as_deref();
        let mut row_images = Vec::new();
        if let Some(anchor) = anchor {
            row_images.push(span_for_anchor(anchor, out_index, prefix_width));
        }
        images.push(row_images);
        if let Some(anchor) = anchor {
            for _ in 1..anchor.rows {
                let row = lines.len();
                lines.push(Line::from(vec![prefix(row), cover_span()]));
                links.push(Vec::new());
                images.push(Vec::new());
            }
        }
    }
    ComposedLines::with_images(lines, links, images)
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
/// For links like `[text](./file.rs#L10)`, returns `Some("#L10")`. Gated on
/// "looks like a local path" so a URL keeps its own `#fragment` / `:port`;
/// callers that already know they hold a path ask [`trailing_location_suffix`]
/// directly.
pub(crate) fn extract_hidden_location_suffix(dest_url: &str) -> Option<String> {
    is_local_path_like_link(dest_url)
        .then(|| trailing_location_suffix(dest_url))
        .flatten()
}

/// Extract a trailing location suffix, whatever the destination looks like.
///
/// Same patterns as [`extract_hidden_location_suffix`] (`#L10`, `#L10C5`,
/// `:10`, `:10:5`) without the local-path gate — the opener needs them for bare
/// relative forms like `src/main.rs:10`, which never look like a path to the
/// renderer.
pub(crate) fn trailing_location_suffix(raw: &str) -> Option<String> {
    // Look for hash location suffix like #L10, #L10C5.
    if let Some((_, fragment)) = raw.rsplit_once('#')
        && is_hash_location_suffix(fragment)
    {
        return Some(format!("#{fragment}"));
    }
    // Look for colon location suffix like :10, :10:5.
    if let Some(suffix) = extract_colon_suffix(raw) {
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
        MarkdownLine {
            segments,
            ..Default::default()
        }
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

    #[test]
    fn symbol_width_measures_what_the_terminal_advances() {
        // The halfwidth katakana sound mark is the one symbol where plain
        // `unicode-width` and the terminal disagree: it is `Grapheme_Extend`
        // (zero columns for that crate) but a terminal renders it in a column of
        // its own — which is what ratatui compensates for, and therefore what
        // `Buffer::set_stringn` lays the cells out with. Anything that walks the
        // cells or declares a width has to use this measure (see
        // `ui::emoji_width`).
        assert_eq!(symbol_width("ｶ"), 1, "the kana alone");
        assert_eq!(symbol_width("ﾞ"), 1, "the sound mark alone");
        assert_eq!(symbol_width("ｶﾞ"), 2, "the pair: two columns on a terminal");
        assert_eq!(symbol_width("你"), 2);
        assert_eq!(
            symbol_width(&format!("{}ｶﾞ{}", osc8_open("x"), osc8_close())),
            2,
            "the injected sequence is not columns"
        );
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

    #[test]
    fn composed_lines_side_channels_are_parallel_and_reported() {
        let plain = ComposedLines::plain(vec![Line::from("a")]);
        assert!(!plain.has_links() && !plain.has_images() && !plain.has_spans());
        assert_eq!(plain.images().len(), 1);

        let with_images = ComposedLines::with_images(
            vec![Line::from("a"), Line::from("b")],
            vec![vec![LinkSpan {
                start: 0,
                end: 1,
                target: "t".into(),
            }]],
            vec![Vec::new(), vec![image_span(1)]],
        );
        assert!(with_images.has_links() && with_images.has_images() && with_images.has_spans());
        // Both channels are padded to the line count.
        assert_eq!(with_images.images().len(), 2);
        assert_eq!(with_images.links().len(), 2);
        assert!(with_images.links()[1].is_empty());
        assert!(with_images.images()[0].is_empty());
    }

    // ── Image anchors ─────────────────────────────────────────────

    fn image_anchor(rows: u16) -> crate::render::markdown::ImageAnchor {
        crate::render::markdown::ImageAnchor {
            path: std::path::PathBuf::from("/ws/plot.png"),
            alt: "plot".into(),
            shape: crate::render::markdown::ImageShape::new(800, 600),
            cols: 78,
            rows,
        }
    }

    fn image_span(line: usize) -> ImageSpan {
        ImageSpan {
            line,
            column: 2,
            cols: 78,
            rows: 3,
            path: std::path::PathBuf::from("/ws/plot.png"),
            alt: "plot".into(),
            px_w: 800,
            px_h: 600,
        }
    }

    #[test]
    fn compose_expands_an_anchor_into_caption_and_cover_rows() {
        let mut anchored = MarkdownLine::default();
        anchored.push_segment(SegmentKind::Image, Style::new(), "▢ plot · 800×600");
        anchored.image = Some(Box::new(image_anchor(3)));
        let md_lines = vec![MarkdownLine::default(), anchored, MarkdownLine::default()];

        let composed = compose_lines(
            &md_lines,
            2,
            |i| Span::raw(if i == 0 { "★ " } else { "  " }),
            |_, style| style,
        );

        // Rows: separator, caption, 2 cover rows, separator.
        assert_eq!(composed.lines().len(), 5);
        assert_eq!(composed.lines()[1].to_string(), "  ▢ plot · 800×600");
        assert_eq!(composed.lines()[2].to_string(), "   ");
        assert_eq!(composed.lines()[3].to_string(), "   ");
        // …and the blank delimiter line is untouched (prefix only).
        assert_eq!(composed.lines()[4].to_string(), "  ");
        // The prefix closure is fed the OUTPUT row index: only row 0 gets `★ `.
        assert_eq!(composed.lines()[0].to_string(), "★ ");

        assert_eq!(composed.images().len(), 5);
        let spans = &composed.images()[1];
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].line, 1);
        assert_eq!(spans[0].column, 2);
        assert_eq!(spans[0].cols, 78);
        assert_eq!(spans[0].rows, 3);
        assert_eq!(spans[0].path, std::path::PathBuf::from("/ws/plot.png"));
        assert_eq!((spans[0].px_w, spans[0].px_h), (800, 600));
        assert_eq!(spans[0].rows_range(), 1..4);
        assert!(composed.images()[2].is_empty());
        assert!(composed.has_images());
    }

    #[test]
    fn compose_without_a_payload_leaves_the_images_channel_empty() {
        let mut line = MarkdownLine::default();
        line.push_segment(SegmentKind::Text, Style::new(), "plain");
        let composed = compose_lines(&[line], 2, |_| Span::raw("  "), |_, style| style);
        assert_eq!(composed.lines().len(), 1);
        assert!(!composed.has_images());
        assert!(composed.images()[0].is_empty());
    }

    #[test]
    fn a_single_row_anchor_is_just_the_caption() {
        let mut anchored = MarkdownLine::default();
        anchored.push_segment(SegmentKind::Image, Style::new(), "▢ tiny");
        anchored.image = Some(Box::new(image_anchor(1)));
        let composed = compose_lines(&[anchored], 2, |_| Span::raw("  "), |_, style| style);
        assert_eq!(composed.lines().len(), 1);
        assert_eq!(composed.images()[0][0].rows, 1);
        assert_eq!(composed.images()[0][0].rows_range(), 0..1);
    }

    #[test]
    fn image_anchors_gate_the_row_arithmetic_like_links_do() {
        // `rows_are_exact` is what the ui layer trusts before pointing a
        // click or a picture at a row: an anchor-bearing line set must refuse
        // it exactly like a link-bearing one when a line overflows.
        let mut wide = MarkdownLine::default();
        wide.push_segment(SegmentKind::CodeBlock, Style::new(), &"x".repeat(40));
        let mut caption = MarkdownLine::default();
        caption.push_segment(SegmentKind::Image, Style::new(), "▢ plot");
        caption.image = Some(Box::new(image_anchor(2)));
        let composed = compose_lines(&[wide, caption], 2, |_| Span::raw("  "), |_, style| style);
        assert_eq!(composed.lines().len(), 3);
        assert!(composed.has_spans());
        assert!(
            !composed.rows_are_exact(20),
            "an over-wide line must refuse the row arithmetic"
        );
        assert!(composed.rows_are_exact(42));
    }

    #[test]
    fn image_tags_round_trip_the_anchor_payload() {
        let composed = ComposedLines::with_images(
            vec![Line::from("caption"), Line::from("cover")],
            vec![Vec::new(), Vec::new()],
            vec![vec![image_span(0)], Vec::new()],
        );
        let tags = composed.image_tags();
        assert_eq!(tags.len(), 2);
        let anchor = tags[0].as_ref().expect("tag");
        assert_eq!(anchor.path, std::path::PathBuf::from("/ws/plot.png"));
        assert_eq!(anchor.alt, "plot");
        assert_eq!(anchor.rows, 3);
        assert_eq!(anchor.cols, 78);
        assert!(tags[1].is_none());
    }
}
