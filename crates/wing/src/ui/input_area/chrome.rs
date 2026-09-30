//! The composer's card — its frame, its geometry, and the two rails.
//!
//! The composer is drawn as a **card**: a rounded box whose text rows hold the
//! draft and whose two horizontal rules carry the live state that used to live
//! on rows of its own above the input. Framing the draft is what gives it an
//! edge against the chat above — the old composer was a bare `> ` line that
//! read as one more chat row — and folding the status rows into the frame is
//! what keeps the frame from costing rows: the working indicator rides the top
//! border (the *activity rail*), the workdir / usage / scroll read-out rides
//! the bottom one (the *meta rail*), and both keep the styling they had.
//!
//! ```text
//! ╭─ ⠋ Working... (12s) · Esc to interrupt ────────────────────╮
//! │ ❯ explain the event flow of the gateway                   │
//! │   and where the resume path hooks in                      │
//! ╰─ ~/ws/wing · 1.2k in · 340 out · 42.5 t/s ───────── 87% ───╯
//! ```
//!
//! **One geometry, one writer.** [`Chrome`] is the single description of where
//! the card's columns are; the widget, the wrap width, the cursor placement and
//! the pointer mapping all read it, so a pointer cannot describe a frame the
//! renderer did not draw.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use crate::config::ThemePalette;
use crate::ui::spinner::SpinnerState;
use crate::ui::status_bar::TurnUsage;

/// Column of the prompt glyph inside the card (right of the border).
pub(super) const PROMPT_X: u16 = 2;
/// The prompt glyph and the gap after it — two cells wide.
pub(super) const PROMPT: &str = "❯ ";
/// First text column, relative to the card's left edge (prompt included).
const TEXT_X: u16 = PROMPT_X + 2;
/// Columns right of the text: the gap and the right border.
const TEXT_OUT: u16 = 2;
/// Rows the frame adds around the text rows.
pub const BORDER_ROWS: u16 = 2;
/// Below this width the frame would leave nothing for the text: the composer
/// degrades to a bare text area (no borders, no rails) instead of drawing a
/// box the draft cannot live in.
const MIN_CARD_WIDTH: u16 = 12;

/// Column geometry of a composer laid out into an area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chrome {
    /// Whether the card is drawn.
    pub card: bool,
    /// Offset of the text area from the card's left edge.
    pub text_x: u16,
    /// Columns available to the draft.
    pub text_width: u16,
    /// Rows available to the draft (the card's height minus its borders).
    pub text_rows: u16,
}

impl Chrome {
    /// The chrome of a composer rendered into `area`.
    pub fn of(area: Rect) -> Self {
        if area.width >= MIN_CARD_WIDTH && area.height > BORDER_ROWS {
            Self {
                card: true,
                text_x: TEXT_X,
                text_width: area.width - TEXT_X - TEXT_OUT,
                text_rows: area.height - BORDER_ROWS,
            }
        } else {
            Self {
                card: false,
                text_x: 0,
                text_width: area.width,
                text_rows: area.height,
            }
        }
    }

    /// The chrome of a composer `width` columns wide, for the callers that
    /// decide a layout before an area exists (the height request, the key
    /// handlers' wrap width). The card is assumed to fit; a laid-out area that
    /// does not is [`Chrome::of`]'s call, not this one's.
    pub fn for_width(width: u16) -> Self {
        Self::of(Rect::new(0, 0, width, BORDER_ROWS + 1))
    }

    /// Rows of frame above the text area (0 or 1) — the offset from the card's
    /// top row to its first text row.
    pub fn top_row(self) -> u16 {
        u16::from(self.card)
    }

    /// The rows a composer of `text_rows` draft rows needs in this chrome —
    /// the frame included when there is one.
    pub fn height_for(self, text_rows: u16) -> u16 {
        text_rows + if self.card { BORDER_ROWS } else { 0 }
    }
}

/// The card's top-border content: what the agent is doing right now.
#[derive(Debug, Clone, Copy)]
pub struct ActivityRail<'a> {
    /// The live spinner frame.
    pub spinner: &'a SpinnerState,
    /// When the turn started (elapsed shown next to the label).
    pub started_at: Instant,
    /// Goal-mode role label (`Executor working`, `Checker reviewing`, …).
    pub role: Option<&'a str>,
}

/// The card's bottom-border content: where the session is and how big the
/// conversation has grown.
#[derive(Debug, Clone, Copy)]
pub struct MetaRail<'a> {
    /// Session workdir (the row's leading, accented item).
    pub workdir: Option<&'a str>,
    /// Per-turn token usage.
    pub usage: &'a TurnUsage,
    /// Total rendered chat rows (0 = nothing to scroll).
    pub total_lines: usize,
    /// Chat rows the viewport shows.
    pub visible_height: usize,
    /// Current scroll offset of the chat viewport.
    pub scroll_offset: usize,
}

impl MetaRail<'_> {
    /// A rail that reports usage alone — the shape the tests render with.
    #[cfg(test)]
    pub fn bare(usage: &TurnUsage) -> MetaRail<'_> {
        MetaRail {
            workdir: None,
            usage,
            total_lines: 0,
            visible_height: 0,
            scroll_offset: 0,
        }
    }
}

/// Paint the frame: the four borders plus the two rails. The text rows are
/// left blank for the widget's own pass — this only lays the card down.
pub(super) fn paint(
    buf: &mut Buffer,
    area: Rect,
    chrome: Chrome,
    activity: Option<&ActivityRail<'_>>,
    meta: &MetaRail<'_>,
    palette: &ThemePalette,
) {
    if !chrome.card {
        return;
    }
    let dim = Style::default().fg(palette.dim);
    let top_y = area.y;
    let bottom_y = area.bottom() - 1;

    // Vertical borders, every text row.
    for y in (top_y + 1)..bottom_y {
        buf.set_span(area.x, y, &Span::styled("│", dim), 1);
        buf.set_span(area.right() - 1, y, &Span::styled("│", dim), 1);
    }

    // Top border: the activity rail (or a plain rule while idle).
    let activity_items: Vec<Vec<Span<'static>>> = activity
        .map(|a| vec![activity_spans(a, palette)])
        .unwrap_or_default();
    rail(
        buf,
        area,
        top_y,
        RailLine {
            left_corner: '╭',
            right_corner: '╮',
            items: activity_items,
            right: Vec::new(),
        },
        palette,
    );

    // Bottom border: the meta rail — where we are, what it cost, where in the
    // history the viewport sits.
    let (items, right) = meta_spans(meta, palette);
    rail(
        buf,
        area,
        bottom_y,
        RailLine {
            left_corner: '╰',
            right_corner: '╯',
            items,
            right,
        },
        palette,
    );
}

/// One border row's content: the corner glyphs and the two sides.
struct RailLine {
    left_corner: char,
    right_corner: char,
    /// Leading items, joined with ` · ` and dropped whole when out of room.
    items: Vec<Vec<Span<'static>>>,
    /// Trailing read-out, kept whole (it is short and it is reserved first).
    right: Vec<Span<'static>>,
}

/// Render one border row: `╭─ <items> ───── <right> ─╮`.
///
/// The row is built right-first — the trailing read-out (the scroll
/// percentage) is reserved before the leading one is laid out — and the
/// leading side is a list of *items* joined with ` · `: an item that does not
/// fit is dropped whole, so a narrow border loses its least useful number
/// instead of showing half of it. Only a single item that overflows on its own
/// (a long workdir) is clipped.
fn rail(buf: &mut Buffer, area: Rect, y: u16, line: RailLine, palette: &ThemePalette) {
    let RailLine {
        left_corner,
        right_corner,
        items,
        right,
    } = line;
    let width = area.width as usize;
    if width == 0 {
        return;
    }
    let dim = Style::default().fg(palette.dim);
    if width < 3 {
        // Degenerate: not even a corner pair fits — draw what does.
        let text: String = std::iter::once(left_corner)
            .chain(std::iter::once(right_corner))
            .take(width)
            .collect();
        buf.set_span(area.x, y, &Span::styled(text, dim), width as u16);
        return;
    }

    // Inner width, minus the two corners.
    let inner = width - 2;
    // `─ ` … ` ` on the left, ` ` … ` ─` on the right: 3 cells of framing.
    const FRAME: usize = 3;

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(items.len() * 2 + right.len() + 4);
    spans.push(Span::styled(left_corner.to_string(), dim));

    // The right block first: it is short, and losing it would lose the scroll
    // read-out that is the whole point of reporting the position here.
    let right_w: usize = right.iter().map(|s| s.width()).sum();
    let keep_right = right_w > 0 && right_w + FRAME + FRAME <= inner;
    let reserved = if keep_right { right_w + FRAME } else { 0 };

    // The left items, widest-first-fits: whole items only, ` · ` between them.
    let room = inner.saturating_sub(reserved).saturating_sub(FRAME);
    let mut laid: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for item in items {
        let item_w: usize = item.iter().map(|s| s.width()).sum();
        let sep = if laid.is_empty() { 0 } else { 3 };
        if used + sep + item_w <= room {
            if sep > 0 {
                laid.push(Span::styled(" · ", dim));
            }
            used += sep + item_w;
            laid.extend(item);
            continue;
        }
        if laid.is_empty() {
            // Nothing shown yet: clip this item into whatever there is.
            laid.extend(fit(item, room));
            used = room;
            break;
        }
        break;
    }
    if !laid.is_empty() {
        spans.push(Span::styled("─ ", dim));
        spans.extend(laid);
        spans.push(Span::styled(" ", dim));
        used += FRAME;
    }

    let dashes = inner.saturating_sub(used).saturating_sub(reserved);
    spans.push(Span::styled("─".repeat(dashes), dim));

    if keep_right {
        spans.push(Span::styled(" ", dim));
        spans.extend(right);
        spans.push(Span::styled(" ─", dim));
    }
    spans.push(Span::styled(right_corner.to_string(), dim));

    let mut x = area.x;
    for span in spans {
        let w = span.width() as u16;
        if w == 0 {
            continue;
        }
        buf.set_span(x, y, &span, w);
        x = x.saturating_add(w);
        if x >= area.right() {
            break;
        }
    }
}

/// Clip a span list to `max` display columns (the one span that straddles the
/// edge is cut at the cell the terminal would show).
fn fit(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= max {
            break;
        }
        let w = span.width();
        if used + w <= max {
            used += w;
            out.push(span);
            continue;
        }
        let kept = fit_str(&span.content, max - used);
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
        break;
    }
    out
}

/// Longest prefix of `s` that fits in `max` display columns.
fn fit_str(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

/// The activity rail's content: `⠋ Working... (12s · Esc to interrupt)`.
fn activity_spans(activity: &ActivityRail<'_>, palette: &ThemePalette) -> Vec<Span<'static>> {
    let dim = Style::default().fg(palette.dim);
    let elapsed = activity.started_at.elapsed().as_secs();
    let mut spans = vec![Span::styled(
        activity.spinner.frame_str().to_string(),
        Style::default().fg(palette.accent),
    )];
    let label = match activity.role {
        Some(role) => format!(" {role}..."),
        None => " Working...".to_string(),
    };
    spans.push(Span::styled(label, Style::default().fg(palette.text)));
    if elapsed > 0 {
        spans.push(Span::styled(
            format!(" ({})", crate::ui::spinner::fmt_elapsed(elapsed)),
            dim,
        ));
    }
    spans.push(Span::styled(" · ", dim));
    spans.push(Span::styled("Esc to interrupt", dim));
    spans
}

/// The meta rail's content: the leading items (workdir, usage, position) and
/// the trailing scroll percentage.
fn meta_spans(
    meta: &MetaRail<'_>,
    palette: &ThemePalette,
) -> (Vec<Vec<Span<'static>>>, Vec<Span<'static>>) {
    let dim = Style::default().fg(palette.dim);
    let mut items: Vec<Vec<Span<'static>>> = Vec::new();

    if let Some(wd) = meta.workdir {
        items.push(vec![Span::styled(
            collapse_home(wd),
            Style::default().fg(palette.accent),
        )]);
    }
    items.extend(meta.usage.items());
    if meta.total_lines > 0 && meta.visible_height < meta.total_lines {
        let pos = format!(
            "{}/{}",
            meta.scroll_offset + meta.visible_height,
            meta.total_lines
        );
        items.push(vec![Span::styled(pos, dim)]);
    }

    let mut right: Vec<Span<'static>> = Vec::new();
    if let Some(percent) = scroll_percent(meta) {
        right.push(Span::styled(format!("{percent}%"), dim));
    }
    (items, right)
}

/// Scroll position as a percentage, `None` while the history fits.
fn scroll_percent(meta: &MetaRail<'_>) -> Option<u8> {
    if meta.total_lines == 0 || meta.visible_height >= meta.total_lines {
        return None;
    }
    let max_scroll = meta.total_lines - meta.visible_height;
    Some(if max_scroll == 0 {
        100
    } else {
        ((meta.scroll_offset.min(max_scroll) as f32 / max_scroll as f32) * 100.0).round() as u8
    })
}

/// `$HOME/…` → `~/…` (the path the rail shows, not the one the session uses).
fn collapse_home(path: &str) -> String {
    use std::sync::OnceLock;
    static HOME: OnceLock<Option<String>> = OnceLock::new();
    let home = HOME.get_or_init(|| std::env::var("HOME").ok().filter(|h| !h.is_empty()));
    if let Some(home) = home
        && let Some(rest) = path.strip_prefix(home.as_str())
    {
        return format!("~{rest}");
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    /// The meta rail with nothing to report but usage.
    fn bare(usage: &TurnUsage) -> MetaRail<'_> {
        MetaRail {
            workdir: None,
            usage,
            total_lines: 0,
            visible_height: 0,
            scroll_offset: 0,
        }
    }

    #[test]
    fn chrome_reserves_the_card_columns() {
        let chrome = Chrome::of(Rect::new(3, 7, 80, 5));
        assert!(chrome.card);
        assert_eq!(chrome.text_x, TEXT_X);
        assert_eq!(chrome.text_width, 80 - TEXT_X - TEXT_OUT);
        assert_eq!(chrome.text_rows, 3);
    }

    #[test]
    fn chrome_degrades_when_the_card_does_not_fit() {
        let narrow = Chrome::of(Rect::new(0, 0, MIN_CARD_WIDTH - 1, 5));
        assert!(!narrow.card);
        assert_eq!(narrow.text_x, 0);
        assert_eq!(narrow.text_width, MIN_CARD_WIDTH - 1);
        assert_eq!(narrow.text_rows, 5);

        // Same for an area with room for the text but not for the frame.
        let short = Chrome::of(Rect::new(0, 0, 80, BORDER_ROWS));
        assert!(!short.card);
    }

    #[test]
    fn height_for_adds_the_frame() {
        assert_eq!(Chrome::for_width(80).height_for(1), 1 + BORDER_ROWS);
        assert_eq!(
            Chrome::for_width(MIN_CARD_WIDTH - 1).height_for(1),
            1,
            "no frame, no rows for it"
        );
    }

    #[test]
    fn rails_paint_corners_dashes_and_content() {
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        let usage = TurnUsage::default();
        let meta = MetaRail {
            workdir: Some("/tmp/ws"),
            usage: &usage,
            total_lines: 0,
            visible_height: 0,
            scroll_offset: 0,
        };
        paint(&mut buf, area, Chrome::of(area), None, &meta, &palette());

        let top = row_text(&buf, 0);
        assert_eq!(
            top,
            format!("╭{}╮", "─".repeat(38)),
            "idle top rail is bare"
        );
        let bottom = row_text(&buf, 2);
        assert!(bottom.starts_with("╰─ /tmp/ws "), "bottom rail: {bottom}");
        assert!(bottom.ends_with("─╯"), "bottom rail: {bottom}");
        let text_row = row_text(&buf, 1);
        assert_eq!(text_row, format!("│{}│", " ".repeat(38)));
    }

    #[test]
    fn scroll_percent_is_the_rail_remainder() {
        let usage = TurnUsage::default();
        let meta = MetaRail {
            workdir: None,
            usage: &usage,
            total_lines: 100,
            visible_height: 20,
            scroll_offset: 80,
        };
        assert_eq!(scroll_percent(&meta), Some(100));
        let at_top = MetaRail {
            scroll_offset: 0,
            ..meta
        };
        assert_eq!(scroll_percent(&at_top), Some(0));
        // No overflow → no percentage.
        let fits = MetaRail {
            total_lines: 10,
            visible_height: 20,
            ..meta
        };
        assert_eq!(scroll_percent(&fits), None);
    }

    #[test]
    fn activity_rail_carries_spinner_label_and_hint() {
        let area = Rect::new(0, 0, 60, 3);
        let mut buf = Buffer::empty(area);
        let spinner = SpinnerState::new();
        let activity = ActivityRail {
            spinner: &spinner,
            started_at: Instant::now(),
            role: Some("Executor working"),
        };
        let usage = TurnUsage::default();
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            Some(&activity),
            &bare(&usage),
            &palette(),
        );
        let top = row_text(&buf, 0);
        assert!(top.starts_with("╭─ ⠋ Executor working... "), "{top}");
        assert!(top.contains("Esc to interrupt"), "{top}");
        assert!(top.ends_with("╮"), "{top}");
        assert_eq!(top.chars().count(), 60, "the rail fills its width: {top}");
    }

    #[test]
    fn rails_clip_content_instead_of_overflowing() {
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        let spinner = SpinnerState::new();
        let activity = ActivityRail {
            spinner: &spinner,
            started_at: Instant::now(),
            role: None,
        };
        let usage = TurnUsage::default();
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            Some(&activity),
            &bare(&usage),
            &palette(),
        );
        for y in 0..3 {
            let row = row_text(&buf, y);
            assert_eq!(row.chars().count(), 20, "row {y} width: {row}");
        }
        let top = row_text(&buf, 0);
        assert!(top.starts_with('╭'), "{top}");
        assert!(top.ends_with('╮'), "{top}");
    }

    #[test]
    fn narrow_cards_keep_their_corners() {
        let area = Rect::new(0, 0, MIN_CARD_WIDTH, 3);
        let mut buf = Buffer::empty(area);
        let usage = TurnUsage::default();
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            None,
            &bare(&usage),
            &palette(),
        );
        let top = row_text(&buf, 0);
        assert_eq!(top, format!("╭{}╮", "─".repeat(10)));
    }
}
