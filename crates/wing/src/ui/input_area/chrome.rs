//! The composer's card — its frame, its geometry, and the two rails.
//!
//! The composer is drawn as a **card**: a rounded box whose text rows hold the draft and whose two
//! horizontal rules carry the live state that used to live on rows of its own above the input. Framing
//! the draft is what gives it an edge against the chat above, and folding the status rows into the
//! frame is what keeps the frame from costing rows: the working indicator rides the top border (the
//! *activity rail*), the workdir / usage / scroll read-out rides the bottom one (the *meta rail*), and
//! both keep the styling they had.
//!
//! ```text
//! ╭─ ⠋ Working... (12s) · Esc to interrupt ────────────────────╮
//! │ ❯ explain the event flow of the gateway                   │
//! │   and where the resume path hooks in                      │
//! ╰─ ~/ws/wing · 1.2k in · 340 out · 42.5 t/s ──────── 87% ───╯
//!
//! ╭──────────────────────────────────────────────── ⏎ 发送 ─╮   ← nothing typed: the invite
//! │ ❯ ask anything                                          │
//! ╰─ ~/ws/wing ─────────────────────────────────────────────╯
//! ```
//!
//! The card **floats**: it keeps [`CARD_MARGIN`] columns of air on each side (the layout turns the
//! composer block into [`card_area`] before anything else looks at it) and its body stays on the
//! terminal's own background — the boundary is the frame, and nothing else.
//!
//! The states the frame does carry are about the keyboard — see [`Focus`]: a waiting draft lifts the
//! outline one notch toward the accent, an empty card invites with a `⏎ 发送` keycap on the top rail,
//! and a panel that is swallowing the composer's keys ghosts the whole thing (the draft is drawn as
//! inactive text) so "typing will not land here" is visible before a key is pressed.
//!
//! **One geometry, one writer.** [`Chrome`] is the single description of where the card's columns are;
//! the widget, the wrap width, the cursor placement and the pointer mapping all read it, so a pointer
//! cannot describe a frame the renderer did not draw.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use crate::config::ThemePalette;
use crate::ui::chip;
use crate::ui::shimmer::mix;
use crate::ui::shimmer::to_rgb;
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
/// Columns of air the floating card keeps on each side of its block.
pub const CARD_MARGIN: u16 = 1;
/// Width assumed before the first frame is drawn.
///
/// Nothing in production reads the chrome of a frame that was never laid out —
/// the run loop draws before it can deliver a key — but the editor is driven
/// without one in the tests, and it needs columns to wrap against. 80 is the
/// width the pre-first-frame geometry falls back to (see
/// `app::frame::UNFRAMED_WIDTH`, which points here), so "no frame yet" behaves
/// the same on both sides.
pub const UNFRAMED_WIDTH: u16 = 80;

/// What Enter does, said once, on the empty card's top rail.
const SEND_HINT: &str = "⏎ 发送";
/// How far the focus ink leans from `dim` toward `accent` (0 = dim, 1 = accent).
///
/// A little under half: enough to leave `dim`, not enough to reach the saturated
/// accent the workdir owns (the accent budget, see `docs/dev/tui-input.md`).
const FOCUS_LEAN: f32 = 0.45;

/// The card's rect inside the composer block the layout allocated.
///
/// **The one place the float happens**: `App::draw` asks the composer for its
/// height with this rect's width, records this rect as the composer's, and
/// renders the widget into it — so the height request, the editor's wrap width
/// and the frame all describe the same columns.
pub fn card_area(block: Rect) -> Rect {
    Rect {
        x: block.x.saturating_add(CARD_MARGIN),
        width: block.width.saturating_sub(CARD_MARGIN * 2),
        ..block
    }
}

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

/// The card's three faces: what the keyboard is doing with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Nothing in the draft yet — the card is asking for the first keystroke, so
    /// the top rail says what one would do ([`SEND_HINT`]).
    Idle,
    /// A draft is waiting in the card: the outline steps one notch toward the
    /// accent, so "this is where typing goes" is visible without reading a word.
    Draft,
    /// Another layer owns the keyboard (see `ComposerWidget::keyboard_held`):
    /// the card promises nothing — quiet outline, no invite.
    Ghost,
}

impl Focus {
    /// Which face the widget is drawing.
    pub fn of(keyboard_held: bool, has_draft: bool) -> Self {
        match (keyboard_held, has_draft) {
            (true, _) => Self::Ghost,
            (false, false) => Self::Idle,
            (false, true) => Self::Draft,
        }
    }
}

/// The outline's ink: `dim` at rest, one notch toward `accent` while a draft waits.
///
/// A mix rather than a palette slot: there is no "muted accent" slot, and the
/// accent budget wants this to be a *washed* accent — the workdir on the meta
/// rail keeps the saturated one (see `docs/dev/tui-input.md`). Named colours are
/// not frozen into RGB (they follow the terminal's theme, the same rule as
/// `chip::quiet_band`), so such a palette keeps the plain `dim` frame.
fn frame_ink(focus: Focus, palette: &ThemePalette) -> Color {
    let rgb_inks = matches!(
        (palette.dim, palette.accent),
        (Color::Rgb(..), Color::Rgb(..))
    );
    if focus != Focus::Draft || !rgb_inks {
        return palette.dim;
    }
    let (r, g, b) = mix(to_rgb(palette.dim), to_rgb(palette.accent), FOCUS_LEAN);
    Color::Rgb(r, g, b)
}

/// The card's top-border content: what the agent is doing right now.
#[derive(Debug, Clone, Copy)]
pub struct ActivityRail<'a> {
    /// The live spinner frame.
    pub spinner: &'a SpinnerState,
    /// When the turn started (elapsed shown next to the label).
    pub started_at: Instant,
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

/// Paint the card's frame: the four borders and the two rails. The text rows
/// are left blank for the widget's own pass — the draft keeps the terminal's
/// background, like every other line of the UI.
pub(super) fn paint(
    buf: &mut Buffer,
    area: Rect,
    chrome: Chrome,
    activity: Option<&ActivityRail<'_>>,
    meta: &MetaRail<'_>,
    focus: Focus,
    palette: &ThemePalette,
) {
    if !chrome.card {
        return;
    }
    let outline = Style::default().fg(frame_ink(focus, palette));
    let top_y = area.y;
    let bottom_y = area.bottom() - 1;

    // Vertical borders, every text row.
    for y in (top_y + 1)..bottom_y {
        buf.set_span(area.x, y, &Span::styled("│", outline), 1);
        buf.set_span(area.right() - 1, y, &Span::styled("│", outline), 1);
    }

    // Top border: the activity rail, or — while idle — the invite that says what
    // Enter would do. One subject per rail: the spinner's turn takes the row.
    let activity_items: Vec<Vec<Span<'static>>> = activity
        .map(|a| vec![activity_spans(a, palette)])
        .unwrap_or_default();
    let invite: Vec<Span<'static>> = if matches!(focus, Focus::Idle) && activity.is_none() {
        vec![chip::Chip::key(SEND_HINT).span(palette, chip::Pad::One)]
    } else {
        Vec::new()
    };
    rail(
        buf,
        area,
        top_y,
        RailLine {
            left_corner: '╭',
            right_corner: '╮',
            items: activity_items,
            right: invite,
            ink: outline,
        },
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
            ink: outline,
        },
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
    /// The frame's own ink — corners, rule and the air around the content.
    /// The items carry their own styles: only the boundary follows the focus.
    ink: Style,
}

/// Render one border row: `╭─ <items> ───── <right> ─╮`.
///
/// The row is built right-first — the trailing read-out (the scroll
/// percentage) is reserved before the leading one is laid out — and the
/// leading side is a list of *items* joined with ` · `: an item that does not
/// fit is dropped whole, so a narrow border loses its least useful number
/// instead of showing half of it. Only a single item that overflows on its own
/// (a long workdir) is clipped.
fn rail(buf: &mut Buffer, area: Rect, y: u16, line: RailLine) {
    let RailLine {
        left_corner,
        right_corner,
        items,
        right,
        ink,
    } = line;
    // Inner width, minus the two corners. Saturating throughout: the caller
    // draws rails only for a card (>= MIN_CARD_WIDTH), and a squeezed one must
    // still stay inside its own row rather than underflow.
    let inner = (area.width as usize).saturating_sub(2);
    // `─ ` … ` ` on the left, ` ` … ` ─` on the right: 3 cells of framing.
    const FRAME: usize = 3;

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(items.len() * 2 + right.len() + 4);
    spans.push(Span::styled(left_corner.to_string(), ink));

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
                laid.push(Span::styled(" · ", ink));
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
        spans.push(Span::styled("─ ", ink));
        spans.extend(laid);
        spans.push(Span::styled(" ", ink));
        used += FRAME;
    }

    let dashes = inner.saturating_sub(used).saturating_sub(reserved);
    spans.push(Span::styled("─".repeat(dashes), ink));

    if keep_right {
        spans.push(Span::styled(" ", ink));
        spans.extend(right);
        spans.push(Span::styled(" ─", ink));
    }
    spans.push(Span::styled(right_corner.to_string(), ink));

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
    spans.push(Span::styled(
        " Working...".to_string(),
        Style::default().fg(palette.text),
    ));
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
    items.extend(meta.usage.items(palette));
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
    // Overflowing by construction (the guard above): `max_scroll >= 1`.
    let max_scroll = meta.total_lines - meta.visible_height;
    Some(((meta.scroll_offset.min(max_scroll) as f32 / max_scroll as f32) * 100.0).round() as u8)
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
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            None,
            &meta,
            Focus::Draft,
            &palette(),
        );

        let top = row_text(&buf, 0);
        assert_eq!(
            top,
            format!("╭{}╮", "─".repeat(38)),
            "nothing on the left: a plain rule"
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
        };
        let usage = TurnUsage::default();
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            Some(&activity),
            &bare(&usage),
            Focus::Idle,
            &palette(),
        );
        let top = row_text(&buf, 0);
        assert!(top.starts_with("╭─ ⠋ Working... "), "{top}");
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
        };
        let usage = TurnUsage::default();
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            Some(&activity),
            &bare(&usage),
            Focus::Idle,
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

    /// The meta rail with every number a turn can report.
    fn busy_rail(usage: &TurnUsage) -> MetaRail<'_> {
        MetaRail {
            workdir: Some("/Users/someone/ws/wing"),
            usage,
            total_lines: 200,
            visible_height: 20,
            scroll_offset: 90,
        }
    }

    fn busy_usage() -> TurnUsage {
        TurnUsage {
            prompt_tokens: 1200,
            completion_tokens: 340,
            cached_tokens: 800,
            tokens_per_sec: 42.5,
            ttft_ms: 320.0,
            ..TurnUsage::default()
        }
    }

    /// Paint just the bottom border and return it as text.
    fn meta_row(width: u16, meta: &MetaRail<'_>) -> String {
        let area = Rect::new(0, 0, width, 3);
        let mut buf = Buffer::empty(area);
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            None,
            meta,
            Focus::Draft,
            &palette(),
        );
        row_text(&buf, 2)
    }

    #[test]
    fn meta_items_are_dropped_whole_when_the_border_runs_out() {
        let usage = busy_usage();
        let meta = busy_rail(&usage);

        // Wide enough for everything: the workdir, all five numbers and both
        // ends of the scroll read-out.
        let wide = meta_row(120, &meta);
        for expected in [
            "/Users/someone/ws/wing",
            "1.2k in",
            "340 out",
            "66.7% cache",
            "42.5 t/s",
            "320ms ttft",
            "110/200",
            "50%",
        ] {
            assert!(wide.contains(expected), "missing {expected:?} in:\n{wide}");
        }

        // Narrower: whole items leave from the tail, most expendable first,
        // and the separator goes with them — never a dangling ` · `.
        let narrow = meta_row(74, &meta);
        assert!(!narrow.contains("ttft"), "{narrow}");
        assert!(!narrow.contains(" · ╯"), "no dangling separator:\n{narrow}");

        // Narrower still: the trailing percentage survives every cut (it is
        // reserved before the leading items are laid out).
        for width in [30, 40, 50, 60] {
            let row = meta_row(width, &meta);
            assert!(
                row.contains('%') && row.ends_with("─╯"),
                "width {width} loses the scroll read-out:\n{row}"
            );
            assert_eq!(row.chars().count(), width as usize, "width {width}: {row}");
        }

        // At 20 columns only the workdir (clipped to the room the reserved
        // percentage leaves) and that percentage remain.
        let tiny = meta_row(20, &meta);
        assert_eq!(tiny, "╰─ /Users/so  50% ─╯");
    }

    #[test]
    fn the_rail_only_keeps_what_fits_whole() {
        let usage = TurnUsage {
            prompt_tokens: 900,
            ..TurnUsage::default()
        };
        let meta = MetaRail {
            workdir: None,
            usage: &usage,
            total_lines: 0,
            visible_height: 0,
            scroll_offset: 0,
        };
        // Inner room = width - 2 - 3 (framing): the item fits at 14 columns,
        // and at the minimum card width it is clipped rather than dropped —
        // the leading item is the rail's subject, not a number among many.
        assert!(meta_row(14, &meta).starts_with("╰─ 900 in"));
        assert!(
            meta_row(MIN_CARD_WIDTH, &meta).starts_with("╰─ 900"),
            "clipped"
        );
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
            Focus::Idle,
            &palette(),
        );
        let top = row_text(&buf, 0);
        assert_eq!(top, format!("╭{}╮", "─".repeat(10)));
    }

    // ── focus state / invite ──────────────────────────────────────

    fn light() -> ThemePalette {
        ThemePalette::from_config(&crate::config::ColorsConfig {
            preset: crate::config::ColorPreset::Light,
            ..crate::config::ColorsConfig::default()
        })
    }

    /// Render a card and hand back its buffer, for tests that read cells.
    fn card_buf(
        width: u16,
        focus: Focus,
        activity: Option<&ActivityRail<'_>>,
        palette: &ThemePalette,
    ) -> Buffer {
        let area = Rect::new(0, 0, width, 3);
        let mut buf = Buffer::empty(area);
        let usage = TurnUsage::default();
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            activity,
            &bare(&usage),
            focus,
            palette,
        );
        buf
    }

    /// The top border as text.
    fn top_row(width: u16, focus: Focus, activity: Option<&ActivityRail<'_>>) -> String {
        row_text(&card_buf(width, focus, activity, &palette()), 0)
    }

    /// `row_text` gives one char per cell, so a CJK glyph leaves a filler cell
    /// between it and the next: comparisons against the copy drop whitespace.
    fn compact(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    #[test]
    fn the_faces_follow_the_keyboard_and_the_draft() {
        assert_eq!(Focus::of(false, false), Focus::Idle);
        assert_eq!(Focus::of(false, true), Focus::Draft);
        assert_eq!(Focus::of(true, true), Focus::Ghost);
        assert_eq!(
            Focus::of(true, false),
            Focus::Ghost,
            "a held keyboard outranks an empty draft"
        );
    }

    /// While nothing is typed, the top rail says what one keystroke would do —
    /// as a keycap (a face, not a brighter glyph), and without widening the row.
    #[test]
    fn the_empty_card_says_what_enter_does() {
        let palette = palette();
        let buf = card_buf(60, Focus::Idle, None, &palette);
        let top = row_text(&buf, 0);
        assert_eq!(
            top.chars().count(),
            60,
            "the invite does not widen the rail"
        );
        assert!(
            compact(&top).contains(&compact(SEND_HINT)),
            "the invite is on the rail: {top}"
        );
        assert!(
            top.ends_with(" ─╮"),
            "it sits against the right corner: {top}"
        );

        // The chips are `ui::chip`'s: a band covers the padding too.
        let x = top.chars().position(|c| c == '⏎').expect("invite glyph") as u16;
        assert_eq!(buf[(x, 0)].bg, palette.surface, "a keycap is a small face");
        assert_eq!(buf[(x, 0)].fg, palette.tool_result, "quiet ink");

        // The rule still reaches the left corner: the pill only eats its own width.
        assert!(top.starts_with("╭───"), "{top}");
    }

    #[test]
    fn the_invite_never_shares_the_rail() {
        let spinner = SpinnerState::new();
        let activity = ActivityRail {
            spinner: &spinner,
            started_at: Instant::now(),
        };
        let working = top_row(60, Focus::Idle, Some(&activity));
        assert!(
            !compact(&working).contains("发送"),
            "the spinner's turn owns the rail: {working}"
        );
        let drafted = top_row(60, Focus::Draft, None);
        assert!(
            !compact(&drafted).contains("发送"),
            "a draft is its own invitation: {drafted}"
        );
        let ghosted = top_row(60, Focus::Ghost, None);
        assert!(
            !compact(&ghosted).contains("发送"),
            "a card that will not take the key promises nothing: {ghosted}"
        );
    }

    #[test]
    fn the_invite_drops_whole_when_the_border_runs_out() {
        // 8 columns of pill + the rail's two 3-cell framings need 16 columns of card.
        assert!(
            compact(&top_row(16, Focus::Idle, None)).contains(&compact(SEND_HINT)),
            "16 columns is exactly enough"
        );
        // One column short: the whole invite goes, and the rule closes over it.
        assert_eq!(
            top_row(15, Focus::Idle, None),
            format!("╭{}╮", "─".repeat(13)),
            "no half a keycap"
        );
    }

    /// The outline leaves `dim` for a washed accent while a draft waits, and
    /// only then: the frames of `Idle` / `Ghost` are the plain quiet rule.
    #[test]
    fn the_outline_takes_the_focus_ink_only_while_a_draft_waits() {
        for palette in [palette(), light()] {
            let ink = frame_ink(Focus::Draft, &palette);
            assert_ne!(ink, palette.dim, "the focus state has to be visible");
            assert_ne!(ink, palette.accent, "and it is not the full accent");
            for focus in [Focus::Idle, Focus::Draft, Focus::Ghost] {
                let buf = card_buf(40, focus, None, &palette);
                let expected = if focus == Focus::Draft {
                    ink
                } else {
                    palette.dim
                };
                // Both rails, both sides and all four corners are one outline.
                for (x, y) in [
                    (0u16, 0u16),
                    (39, 0),
                    (0, 2),
                    (39, 2),
                    (5, 0),
                    (0, 1),
                    (39, 1),
                    (5, 2),
                ] {
                    assert_eq!(
                        buf[(x, y)].fg,
                        expected,
                        "{:?} {focus:?} at ({x},{y})",
                        palette.appearance
                    );
                }
            }
        }
    }

    /// The focus ink is a *washed* accent: its hue leans toward `accent` while
    /// its chroma stays under it (the workdir is the saturated one).
    #[test]
    fn the_focus_ink_is_a_washed_accent() {
        fn chroma((r, g, b): crate::ui::shimmer::Rgb) -> f32 {
            let max = r.max(g).max(b) as f32;
            let min = r.min(g).min(b) as f32;
            if max == 0.0 { 0.0 } else { (max - min) / max }
        }
        for palette in [palette(), light()] {
            let ink = to_rgb(frame_ink(Focus::Draft, &palette));
            assert!(
                chroma(ink) > chroma(to_rgb(palette.dim)),
                "{:?}：比 dim 更有色相",
                palette.appearance
            );
            assert!(
                chroma(ink) < chroma(to_rgb(palette.accent)),
                "{:?}：比 workdir 的满 accent 弱一档",
                palette.appearance
            );
        }
    }

    /// The lean is a fixed look, not a knob: both presets land where they were
    /// tuned (`docs/dev/tui-input.md`), so changing `FOCUS_LEAN` has to be a
    /// decision someone re-took — not a quiet drift.
    #[test]
    fn the_focus_lean_is_the_recorded_one() {
        assert_eq!(
            frame_ink(Focus::Draft, &palette()),
            Color::Rgb(76, 144, 167)
        );
        assert_eq!(frame_ink(Focus::Draft, &light()), Color::Rgb(60, 111, 128));
    }

    #[test]
    fn a_named_ink_keeps_the_plain_frame() {
        // 命名色跟终端主题走，不冻成 RGB（与 `chip::quiet_band` 同一条规矩）。
        for palette in [
            ThemePalette {
                dim: Color::Gray,
                ..ThemePalette::default()
            },
            ThemePalette {
                accent: Color::Cyan,
                ..ThemePalette::default()
            },
            ThemePalette {
                dim: Color::Reset,
                ..ThemePalette::default()
            },
        ] {
            assert_eq!(
                frame_ink(Focus::Draft, &palette),
                palette.dim,
                "{:?} / {:?} 不该被算成一个假颜色",
                palette.dim,
                palette.accent
            );
        }
    }
}
