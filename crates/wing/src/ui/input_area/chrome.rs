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
//! ╭─────────────────────────────────────────  Enter  发送 ─╮   ← nothing typed
//! │ ❯ ask anything                                         │
//! ╰─ ~/ws/wing ────────────────────────────────────────────╯
//! ```
//!
//! The card **floats**: it keeps [`CARD_MARGIN`] columns of air on each side (the layout turns the
//! composer block into [`card_area`] before anything else looks at it) and its body stays on the
//! terminal's own background — the boundary is the frame, and nothing else.
//!
//! The states the frame does carry are about the keyboard — see [`Focus`]: an empty card draws its
//! outline in the box ink (`tool_result`) so the card reads as something to type into, then invites
//! with an `Enter` key on the top rail; a waiting draft turns that same outline toward the accent
//! (hue, not weight); and a panel that is swallowing the composer's keys ghosts the whole thing (the
//! draft is drawn as inactive text) so "typing will not land here" is visible before a key is pressed.
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

/// The key the invite names. A keycap is a *key* and the verb goes beside it in
/// dim — the split the nameplate uses (`Esc` + a dim verb) — and the name is
/// spelled out: a return glyph would be one ambiguous-width cell on the border
/// row, where an extra column would push the corner out.
const SEND_KEY: &str = "Enter";
/// What that key does, said once, on the empty card's top rail.
const SEND_VERB: &str = "发送";
/// How far the focus ink leans from `dim` toward `accent` (0 = dim, 1 = accent).
///
/// A little under half: enough for the hue to read as "this card is waiting on
/// you", not enough to reach the accent the caret and the commands use (the
/// accent budget, see `docs/dev/tui-input.md`).
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

/// The card's three faces: what the keyboard is doing with it, and whether
/// anything is waiting there. Orthogonal to a turn running — a card can be
/// `Empty` while the agent is busy (the rail then belongs to the spinner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    /// Nothing in the draft yet — the card is asking for the first keystroke:
    /// the outline wears the box ink (a box, not a hairline) and the top rail
    /// says what one would do ([`SEND_KEY`] + [`SEND_VERB`]).
    Empty,
    /// A draft is waiting in the card: the outline turns toward the accent, so
    /// "this is where typing goes" is visible without reading a word.
    Draft,
    /// Another layer owns the keyboard (see `ComposerWidget::keyboard_held`):
    /// the card promises nothing — quiet outline, no invite.
    Ghost,
}

impl Focus {
    /// Which face the widget is drawing.
    pub(super) fn of(keyboard_held: bool, has_draft: bool) -> Self {
        match (keyboard_held, has_draft) {
            (true, _) => Self::Ghost,
            (false, false) => Self::Empty,
            (false, true) => Self::Draft,
        }
    }
}

/// The outline's ink: one register per face (see [`Focus`]).
///
/// * `Empty` — the card is a box on the screen before anything is typed in it,
///   so the outline wears `tool_result`, the ink a keycap's label wears. That is
///   what makes the card read as a box: `dim` next to the nameplate's rule is a
///   hairline.
/// * `Draft` — a *washed* accent (a mix: there is no "muted accent" slot), so
///   the frame says "waiting on your keys" by **hue**, not by weight: on both
///   presets it lands in the same contrast band as the empty frame (the numbers
///   are pinned in `the_outline_faces_keep_their_ladder`, recorded in
///   `docs/dev/tui-input.md`).
/// * `Ghost` — `dim`: another layer owns the keys, the card promises nothing.
///
/// Named colours are not frozen into RGB (they follow the terminal's theme —
/// the same rule as `chip::quiet_band`), so a palette whose `dim` or `accent`
/// is not a concrete RGB keeps the plain `dim` outline while a draft waits.
fn frame_ink(focus: Focus, palette: &ThemePalette) -> Color {
    match focus {
        Focus::Empty => palette.tool_result,
        Focus::Ghost => palette.dim,
        Focus::Draft => match (palette.dim, palette.accent) {
            (Color::Rgb(..), Color::Rgb(..)) => {
                let (r, g, b) = mix(to_rgb(palette.dim), to_rgb(palette.accent), FOCUS_LEAN);
                Color::Rgb(r, g, b)
            }
            _ => palette.dim,
        },
    }
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
    /// Session workdir (the row's leading item).
    ///
    /// A fact about the environment, not something you can type: it keeps the
    /// quiet ink, and while the card is lit the outline around it is the row's
    /// one bright thing.
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
    let dim = Style::default().fg(palette.dim);
    let outline = Style::default().fg(frame_ink(focus, palette));
    let top_y = area.y;
    let bottom_y = area.bottom() - 1;

    // Vertical borders, every text row.
    for y in (top_y + 1)..bottom_y {
        buf.set_span(area.x, y, &Span::styled("│", outline), 1);
        buf.set_span(area.right() - 1, y, &Span::styled("│", outline), 1);
    }

    // Top border: one subject per rail. The invite speaks only while the draft
    // is empty *and* nothing else is claiming the row — a running turn puts its
    // spinner here, and two read-outs on one rule read as neither.
    let activity_items: Vec<Vec<Span<'static>>> = activity
        .map(|a| vec![activity_spans(a, palette)])
        .unwrap_or_default();
    let invite: Vec<Span<'static>> = if matches!(focus, Focus::Empty) && activity.is_none() {
        vec![
            chip::Chip::key(SEND_KEY).span(palette, chip::Pad::One),
            Span::styled(format!(" {SEND_VERB}"), dim),
        ]
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
            sep: dim,
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
            sep: dim,
        },
    );
}

/// One border row's content: the corner glyphs and the two sides.
struct RailLine {
    left_corner: char,
    right_corner: char,
    /// Leading items, joined with ` · ` and dropped whole when out of room.
    items: Vec<Vec<Span<'static>>>,
    /// Trailing block — the read-out, or the empty card's invite — kept whole
    /// (it is short, and it is reserved before the leading items are laid out).
    right: Vec<Span<'static>>,
    /// The frame's own ink — corners, rule and the air around the content.
    /// The items carry their own styles: only the boundary follows the focus.
    ink: Style,
    /// Ink between two items. A separator belongs to the content, so it stays
    /// quiet even on a lit frame — otherwise the punctuation would outshout the
    /// numbers it separates.
    sep: Style,
}

/// Render one border row: `╭─ <items> ───── <right> ─╮`.
///
/// The row is built right-first: the trailing block is whatever must not be
/// lost — the scroll percentage (the point of reporting the position there) or
/// the empty card's invite (which has nowhere else to live) — so it is reserved
/// before the leading items are laid out. The leading side is a list of *items*
/// joined with ` · `: an item that does not fit is dropped whole, so a narrow
/// border loses its least useful number instead of showing half of it. Only a
/// single item that overflows on its own (a long workdir) is clipped.
fn rail(buf: &mut Buffer, area: Rect, y: u16, line: RailLine) {
    let RailLine {
        left_corner,
        right_corner,
        items,
        right,
        ink,
        sep,
    } = line;
    // Inner width, minus the two corners. Saturating throughout: the caller
    // draws rails only for a card (>= MIN_CARD_WIDTH), and a squeezed one must
    // still stay inside its own row rather than underflow.
    let inner = (area.width as usize).saturating_sub(2);
    // `─ ` … ` ` on the left, ` ` … ` ─` on the right: 3 cells of framing.
    const FRAME: usize = 3;

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(items.len() * 2 + right.len() + 4);
    spans.push(Span::styled(left_corner.to_string(), ink));

    // The right block first — see this function's doc for why it is reserved.
    let right_w: usize = right.iter().map(|s| s.width()).sum();
    let keep_right = right_w > 0 && right_w + FRAME + FRAME <= inner;
    let reserved = if keep_right { right_w + FRAME } else { 0 };

    // The left items, widest-first-fits: whole items only, ` · ` between them.
    let room = inner.saturating_sub(reserved).saturating_sub(FRAME);
    let mut laid: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for item in items {
        let item_w: usize = item.iter().map(|s| s.width()).sum();
        let gap = if laid.is_empty() { 0 } else { 3 };
        if used + gap + item_w <= room {
            if gap > 0 {
                laid.push(Span::styled(" · ", sep));
            }
            used += gap + item_w;
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
        items.push(vec![Span::styled(collapse_home(wd), dim)]);
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
            Focus::Empty,
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
            Focus::Empty,
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

    /// Paint just the bottom border and return it as text (on a drafted card,
    /// where the frame is lit).
    fn meta_row(width: u16, meta: &MetaRail<'_>) -> String {
        row_text(&card_buf(width, Focus::Draft, None, meta, &palette()), 2)
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
            Focus::Empty,
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
        meta: &MetaRail<'_>,
        palette: &ThemePalette,
    ) -> Buffer {
        let area = Rect::new(0, 0, width, 3);
        let mut buf = Buffer::empty(area);
        paint(
            &mut buf,
            area,
            Chrome::of(area),
            activity,
            meta,
            focus,
            palette,
        );
        buf
    }

    /// The same card with nothing to report but usage.
    fn bare_card(width: u16, focus: Focus, activity: Option<&ActivityRail<'_>>) -> Buffer {
        let usage = TurnUsage::default();
        card_buf(width, focus, activity, &bare(&usage), &palette())
    }

    /// The top border as text.
    fn top_row(width: u16, focus: Focus, activity: Option<&ActivityRail<'_>>) -> String {
        row_text(&bare_card(width, focus, activity), 0)
    }

    /// Column where `needle` starts in a [`row_text`] projection (one char per
    /// cell, so a char offset is a screen column).
    fn column_of(row: &str, needle: &str) -> usize {
        let cells: Vec<char> = row.chars().collect();
        let needle: Vec<char> = needle.chars().collect();
        cells
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("the needle is on the row")
    }

    #[test]
    fn the_faces_follow_the_keyboard_and_the_draft() {
        assert_eq!(Focus::of(false, false), Focus::Empty);
        assert_eq!(Focus::of(false, true), Focus::Draft);
        assert_eq!(Focus::of(true, true), Focus::Ghost);
        assert_eq!(
            Focus::of(true, false),
            Focus::Ghost,
            "a held keyboard outranks an empty draft"
        );
    }

    /// While nothing is typed, the top rail says what one keystroke would do:
    /// the key is a keycap (a face, not a brighter glyph) and the verb sits
    /// beside it in dim — the same split the nameplate uses (`Esc 中断`).
    #[test]
    fn the_empty_card_says_what_enter_does() {
        let palette = palette();
        let buf = bare_card(60, Focus::Empty, None);
        let top = row_text(&buf, 0);
        assert_eq!(
            top.chars().count(),
            60,
            "the invite does not widen the rail"
        );
        assert!(
            top.ends_with(" ─╮"),
            "it sits against the right corner: {top}"
        );
        assert!(
            top.starts_with("╭───"),
            "and the rule still opens at the left: {top}"
        );

        let key = column_of(&top, SEND_KEY) as u16;
        let band = chip::Chip::key(SEND_KEY)
            .span(&palette, chip::Pad::One)
            .style
            .bg;
        assert!(band.is_some(), "a keycap is a face");
        for x in key - 1..key + SEND_KEY.len() as u16 + 1 {
            assert_eq!(
                Some(buf[(x, 0)].bg),
                band,
                "the band covers the padding ({x})"
            );
        }
        // The verb is *not* part of the key: it lies on the terminal's own
        // background, in the ink the nameplate gives a verb.
        // A CJK glyph leaves a filler cell in this projection: anchor the verb
        // on its first character.
        let verb = column_of(&top, &SEND_VERB.chars().take(1).collect::<String>()) as u16;
        assert_eq!(buf[(verb, 0)].bg, Color::Reset, "the verb is not a key");
        assert_eq!(buf[(verb, 0)].fg, palette.dim, "verbs are quiet");
    }

    #[test]
    fn the_invite_never_shares_the_rail() {
        let spinner = SpinnerState::new();
        let activity = ActivityRail {
            spinner: &spinner,
            started_at: Instant::now(),
        };
        let working = top_row(60, Focus::Empty, Some(&activity));
        assert!(
            !working.contains(SEND_KEY),
            "the spinner's turn owns the rail: {working}"
        );
        let drafted = top_row(60, Focus::Draft, None);
        assert!(
            !drafted.contains(SEND_KEY),
            "a draft is its own invitation: {drafted}"
        );
        let ghosted = top_row(60, Focus::Ghost, None);
        assert!(
            !ghosted.contains(SEND_KEY),
            "a card that will not take the key promises nothing: {ghosted}"
        );
    }

    #[test]
    fn the_invite_drops_whole_when_the_border_runs_out() {
        // The block is `Enter` + padding (7) plus ` 发送` (5); with the rail's two
        // 3-cell framings it needs 20 columns of card — and nothing less.
        assert!(
            top_row(20, Focus::Empty, None).contains(SEND_KEY),
            "20 columns is exactly enough"
        );
        // One column short: the whole invite goes, and the rule closes over it.
        assert_eq!(
            top_row(19, Focus::Empty, None),
            format!("╭{}╮", "─".repeat(17)),
            "no half a keycap"
        );
    }

    /// The outline carries the face: `tool_result` while the card waits (a box,
    /// not a hairline), a washed accent while a draft waits (the same band, a
    /// teal hue — the focus signal is colour, not weight), and the plain quiet
    /// rule when another layer holds the keys.
    #[test]
    fn the_outline_carries_the_face() {
        for palette in [palette(), light()] {
            let washed = frame_ink(Focus::Draft, &palette);
            assert_ne!(washed, palette.dim, "the focus state has to be visible");
            assert_ne!(washed, palette.accent, "and it is not the full accent");
            let expected = [
                (Focus::Empty, palette.tool_result),
                (Focus::Draft, washed),
                (Focus::Ghost, palette.dim),
            ];
            let usage = TurnUsage::default();
            for (focus, ink) in expected {
                let buf = card_buf(40, focus, None, &bare(&usage), &palette);
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
                        ink,
                        "{:?} {focus:?} at ({x},{y})",
                        palette.appearance
                    );
                }
            }
            // The three faces are three inks: an empty card is not a ghosted
            // one, and a drafted card is not an empty one.
            assert_ne!(
                frame_ink(Focus::Empty, &palette),
                frame_ink(Focus::Ghost, &palette)
            );
            assert_ne!(
                frame_ink(Focus::Empty, &palette),
                frame_ink(Focus::Draft, &palette)
            );
        }
    }

    /// The ladder the three faces sit on, measured the way a reader sees it —
    /// contrast against the paper the preset actually paints
    /// (`ui::html::page_colors`, the same page `splash_preview --html` exports,
    /// so these numbers match the measurements in `docs/dev/tui-input.md`). The
    /// gate itself is `chip::contrast`, the one the bands use.
    ///
    /// Pinned rather than left in prose: an empty card is the strongest face (a
    /// box), a drafted card sits in the same band (hue tells it apart), a
    /// ghosted one trails.
    #[test]
    fn the_outline_faces_keep_their_ladder() {
        use crate::config::colors::parse_color;
        use crate::ui::chip;
        use crate::ui::html::page_colors;

        let page = |palette: &ThemePalette| {
            let (background, _) = page_colors(palette);
            to_rgb(parse_color(background).expect("the page colour parses"))
        };

        for (palette, expected) in [
            // (empty, draft, ghost) — dark paper first.
            (palette(), (6.07, 5.35, 3.23)),
            (light(), (6.39, 5.56, 5.49)),
        ] {
            let ink = |focus| chip::contrast(to_rgb(frame_ink(focus, &palette)), page(&palette));
            let (empty, draft, ghost) = (ink(Focus::Empty), ink(Focus::Draft), ink(Focus::Ghost));
            for (got, want, face) in [
                (empty, expected.0, "empty"),
                (draft, expected.1, "draft"),
                (ghost, expected.2, "ghost"),
            ] {
                assert!(
                    (got - want).abs() < 0.01,
                    "{:?} {face}: recorded {want}, measured {got}",
                    palette.appearance
                );
            }
            // The box leads, the ghost trails, and the two live faces share a
            // band (they are told apart by hue, not by weight).
            assert!(empty > draft, "{:?}", palette.appearance);
            assert!(empty > ghost, "{:?}", palette.appearance);
            assert!(draft >= ghost, "{:?}", palette.appearance);
            assert!(
                (empty - draft) / empty < 0.2,
                "{:?}: the two live faces drifted apart ({empty} vs {draft})",
                palette.appearance
            );
        }
    }

    /// The air the rail puts around its items belongs to the frame, not to the
    /// items: on a lit frame the framing cells are lit while the workdir keeps
    /// the quiet ink — a path is a fact about the environment, not something you
    /// can type, so it never takes the accent.
    #[test]
    fn the_frame_lights_the_air_around_an_item_not_the_item() {
        let palette = palette();
        let usage = TurnUsage::default();
        let meta = MetaRail {
            workdir: Some("/tmp/ws"),
            usage: &usage,
            total_lines: 0,
            visible_height: 0,
            scroll_offset: 0,
        };
        for focus in [Focus::Empty, Focus::Draft] {
            let buf = card_buf(60, focus, None, &meta, &palette);
            let ink = frame_ink(focus, &palette);
            // `╰─ <workdir> ────╯`: the `─ ` before the item and the ` ` after it.
            assert_eq!(buf[(1, 2)].fg, ink, "{focus:?}: the framing dash");
            assert_eq!(buf[(2, 2)].fg, ink, "{focus:?}: the air before the item");
            assert_eq!(
                buf[(3, 2)].fg,
                palette.dim,
                "{focus:?}: the workdir stays quiet"
            );
            let after_item = 3 + "/tmp/ws".chars().count() as u16;
            assert_eq!(buf[(after_item, 2)].fg, ink, "{focus:?}: the air after it");
        }
    }

    /// The frame follows the focus; the punctuation between two items does not.
    /// Otherwise the quietest glyph on the rail outshouts the numbers it
    /// separates (and the same ` · ` inside the activity label stays dim).
    #[test]
    fn the_separator_stays_quiet_on_a_lit_frame() {
        let palette = palette();
        let usage = TurnUsage {
            prompt_tokens: 1200,
            completion_tokens: 340,
            ..TurnUsage::default()
        };
        let meta = MetaRail {
            workdir: None,
            usage: &usage,
            total_lines: 0,
            visible_height: 0,
            scroll_offset: 0,
        };
        let buf = card_buf(80, Focus::Draft, None, &meta, &palette);
        let row = row_text(&buf, 2);
        let ink = frame_ink(Focus::Draft, &palette);
        assert_ne!(ink, palette.dim, "the frame is lit in this state");

        let dots = column_of(&row, " · ");
        assert_eq!(
            buf[(dots as u16, 2)].fg,
            palette.dim,
            "a separator keeps the quiet ink: {row}"
        );
        assert_eq!(
            buf[(dots as u16 + 1, 2)].fg,
            palette.dim,
            "…and so does its dot"
        );
        // The rule right beside it is lit, so this is not just "nothing is lit".
        assert_eq!(buf[(0, 2)].fg, ink, "the corner is lit: {row}");
    }

    /// The focus ink is a *washed* accent: its hue leans toward `accent` while
    /// its chroma stays under it (the caret and the commands are the saturated
    /// ones).
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
                "{:?}: more hue than dim",
                palette.appearance
            );
            assert!(
                chroma(ink) < chroma(to_rgb(palette.accent)),
                "{:?}: a notch under the saturated accent",
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
        // Named colours follow the terminal's theme; they are not frozen into
        // RGB (the same rule as `chip::quiet_band`).
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
                "{:?} / {:?} must not be turned into a made-up RGB",
                palette.dim,
                palette.accent
            );
        }
    }
}
