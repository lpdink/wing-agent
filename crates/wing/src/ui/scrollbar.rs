//! Overlay scrollbar — geometry, hit testing and painting.
//!
//! The bar lives on the **last column of the chat viewport**: it is painted
//! on top of the content by a buffer patch (like the info separator and the
//! toast), so it never takes layout width and never reflows the chat cells.
//! It only exists while the content overflows the viewport — the same
//! predicate the info separator's `pos/total · %` indicator uses.
//!
//! Everything geometric is a pure function over `(area, content, offset)`, so
//! the edge cases that matter in practice (content that exactly fits, a thumb
//! at 10 000 lines of history in a 10-row viewport, dragging the pointer out
//! of the track) are unit-testable without a terminal.
//!
//! Interaction model (see the change's design.md):
//! - press on the bare track → jump there (thumb centred on the pointer);
//! - press on the thumb → keep the grabbed row under the pointer while dragging;
//! - drag clamps into the track, so pulling the pointer past either end keeps
//!   tracking that end instead of stalling;
//! - the wheel is *not* part of this module: `App::handle_mouse` matches it
//!   before the bar ever sees the event.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

use crate::config::ThemePalette;

/// Smallest thumb the bar will draw, in rows.
///
/// The proportional height rounds to zero at extreme ratios (10 000 rows of
/// history in a 10-row viewport); a 1-row thumb is technically visible but
/// nearly impossible to grab, so the floor is 2 rows.
pub const MIN_THUMB_HEIGHT: u16 = 2;

/// Screen geometry of the overlay scrollbar for one frame.
///
/// All row fields are inclusive screen coordinates inside the chat area the
/// geometry was derived from; the `content_height` / `viewport_height` pair
/// keeps the reverse mapping (`row → scroll offset`) consistent with the
/// forward one, whatever the caller's frame state was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    /// The single screen column the bar occupies (chat area's rightmost).
    pub column: u16,
    /// First / last screen row of the track (inclusive) — the chat viewport.
    pub track_top: u16,
    pub track_bottom: u16,
    /// First / last screen row of the thumb (inclusive).
    pub thumb_top: u16,
    pub thumb_bottom: u16,
    /// Content height (virtual lines) the thumb position was derived from.
    pub content_height: usize,
    /// Viewport height (rows) the thumb size was derived from.
    pub viewport_height: usize,
}

impl ScrollbarGeometry {
    /// Number of rows the track spans.
    pub fn track_height(&self) -> u16 {
        self.track_bottom - self.track_top + 1
    }

    /// Number of rows the thumb spans.
    pub fn thumb_height(&self) -> u16 {
        self.thumb_bottom - self.thumb_top + 1
    }

    /// Largest meaningful scroll offset for this frame.
    pub fn max_scroll(&self) -> usize {
        self.content_height.saturating_sub(self.viewport_height)
    }
}

/// Interaction state of the overlay scrollbar (owned by `App`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollbarState {
    /// Pointer sits on the bar column (only reportable with `?1003` on).
    pub hovered: bool,
    /// The left button went down on the bar and owns the pointer.
    pub dragging: bool,
    /// Row offset inside the thumb kept under the pointer while dragging.
    pub grip: u16,
}

impl ScrollbarState {
    /// Whether the bar should render its emphasised (thicker / brighter) look.
    pub fn is_active(&self) -> bool {
        self.hovered || self.dragging
    }

    /// Drop hover + drag. Returns `true` when something actually changed, so
    /// callers can skip a redraw on no-op cleanups.
    pub fn clear(&mut self) -> bool {
        let changed = self.is_active();
        self.hovered = false;
        self.dragging = false;
        self.grip = 0;
        changed
    }
}

/// Geometry of the overlay scrollbar, or `None` when there is nothing to
/// scroll (content fits the viewport) or when the area is degenerate.
///
/// The thumb height is proportional to `viewport / content`, floored at
/// [`MIN_THUMB_HEIGHT`] and clamped to the track; the thumb position is
/// proportional to `offset / max_scroll`. Both use integer round-half-up, so
/// the extremes are exact: offset `0` puts the thumb flush with the top,
/// offset `max_scroll` flush with the bottom — which is what re-arms the
/// follow contract when the user drags all the way down.
pub fn geometry(
    area: Rect,
    content_height: usize,
    scroll_offset: usize,
) -> Option<ScrollbarGeometry> {
    let viewport = area.height as usize;
    if area.width == 0 || area.height == 0 || content_height <= viewport {
        return None;
    }

    let max_scroll = content_height - viewport;
    let track_h = area.height;
    let proportional = ((viewport as u64 * track_h as u64 + content_height as u64 / 2)
        / content_height as u64) as u16;
    let thumb_h = proportional.max(MIN_THUMB_HEIGHT).min(track_h);
    let travel = track_h - thumb_h;

    let offset = scroll_offset.min(max_scroll);
    let thumb_off = if travel == 0 {
        0
    } else {
        ((offset as u64 * travel as u64 + max_scroll as u64 / 2) / max_scroll as u64) as u16
    };
    let thumb_top = area.y + thumb_off;

    Some(ScrollbarGeometry {
        column: area.right() - 1,
        track_top: area.y,
        track_bottom: area.bottom() - 1,
        thumb_top,
        thumb_bottom: thumb_top + thumb_h - 1,
        content_height,
        viewport_height: viewport,
    })
}

/// Whether a pointer position is on the bar: **exactly** its column, and a row
/// inside the track.
///
/// The column test is exact (not "the last few columns") so that clicking or
/// selecting text at the right edge of the chat is not swallowed by the bar.
pub fn hit(geom: &ScrollbarGeometry, column: u16, row: u16) -> bool {
    column == geom.column && row >= geom.track_top && row <= geom.track_bottom
}

/// Which row inside the thumb a press at `row` should keep under the pointer.
///
/// Pressing the thumb keeps the grabbed row (`row - thumb_top`); pressing the
/// bare track centres the thumb on the pointer, so a track click jumps to the
/// clicked line rather than to a corner.
pub fn grip_at(geom: &ScrollbarGeometry, row: u16) -> u16 {
    let row = row.clamp(geom.track_top, geom.track_bottom);
    if row >= geom.thumb_top && row <= geom.thumb_bottom {
        row - geom.thumb_top
    } else {
        geom.thumb_height() / 2
    }
}

/// Target scroll offset for a pointer row — the one mapping used by both the
/// track press (jump) and the thumb drag.
///
/// **Proportional, and that is what keeps the bar 1:1 with the pointer**: the
/// thumb's row is the content's progress along the track, so the thumb travels
/// its whole range exactly as the pointer travels the track — it never lags
/// behind the finger, and one gesture reaches any position.
///
/// The price is the step size — `max_scroll / travel` lines per pointer row,
/// a few lines on a short session and tens of lines on a long one. That is
/// *not* a tunable: "the thumb stays under the pointer" and "one pointer row
/// moves three lines" are the same statement only when `max_scroll == 3 ×
/// travel`. A terminal has rows, not pixels (a browser scrollbar is
/// proportional too — it just quantises finely enough to look continuous), so
/// the fine-grained entries are the wheel (3 lines per notch, over the bar as
/// well) and the keyboard.
///
/// The row is clamped into the track first, so a drag that leaves the chat
/// area (or the bar column) keeps dragging along the extremes instead of
/// jumping or stalling.
pub fn offset_for_row(geom: &ScrollbarGeometry, row: u16, grip: u16) -> usize {
    let max_scroll = geom.max_scroll();
    let travel = geom.track_height().saturating_sub(geom.thumb_height());
    if travel == 0 || max_scroll == 0 {
        // Degenerate viewport (the thumb fills the whole track): there is no
        // travel to map onto, so the bar stays put.
        return 0;
    }

    let row = row.clamp(geom.track_top, geom.track_bottom);
    let last_thumb_top = geom.track_bottom - (geom.thumb_height() - 1);
    let thumb_top = row
        .saturating_sub(grip)
        .clamp(geom.track_top, last_thumb_top);

    let rel = (thumb_top - geom.track_top) as u64;
    let travel = travel as u64;
    let offset = (rel * max_scroll as u64 + travel / 2) / travel;
    (offset as usize).min(max_scroll)
}

/// Paint the bar into the frame buffer.
///
/// Called after the chat widget (so the bar overprints the content's
/// rightmost column) and before the toast (so a toast is never hidden by the
/// bar; the toast keeps a one-column right margin, so the two share rows but
/// never the bar's column).
///
/// Only the symbol, the foreground colour and the weight are written; the
/// background is inherited from the cell underneath on purpose, so the
/// overlay does not punch a differently-coloured hole in the chat. The
/// modifiers are *assigned* rather than merged (see below), because
/// `Cell::set_style` is additive and a `DIM` hint or a `BOLD` heading under
/// the bar would otherwise leak into its weight, line by line.
///
/// Not handled: when a double-width grapheme (CJK) ends exactly on the bar's
/// column, the bar replaces the grapheme's trailing half cell, so the
/// terminal renders a clipped wide glyph for that row.
pub fn paint(
    buf: &mut Buffer,
    geom: &ScrollbarGeometry,
    state: ScrollbarState,
    palette: &ThemePalette,
) {
    let active = state.is_active();
    let track_fg = if active {
        palette.thinking
    } else {
        palette.dim
    };
    let thumb_fg = if active {
        palette.accent
    } else {
        palette.thinking
    };
    // A cell cannot get wider, so "thicker" is expressed through the glyph
    // weight: hairline track → heavy vertical thumb → solid block while the
    // pointer is on the bar. It all stays inside the one column.
    let thumb_glyph = if active { "█" } else { "┃" };

    for row in geom.track_top..=geom.track_bottom {
        let in_thumb = row >= geom.thumb_top && row <= geom.thumb_bottom;
        let (glyph, fg, bold) = if in_thumb {
            (thumb_glyph, thumb_fg, active)
        } else {
            ("│", track_fg, false)
        };
        if let Some(cell) = buf.cell_mut((geom.column, row)) {
            cell.set_symbol(glyph);
            cell.set_fg(fg);
            cell.modifier = if bold {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    /// Content of 100 lines in a 20-row viewport: 4-row thumb, 16 rows of
    /// travel, 80 lines of scroll range — evenly divisible, so the mapping is
    /// exact and easy to reason about in assertions.
    fn even_geom(offset: usize) -> ScrollbarGeometry {
        geometry(Rect::new(0, 0, 80, 20), 100, offset).expect("content overflows")
    }

    // ── visibility ───────────────────────────────────────────────────────

    #[test]
    fn test_no_geometry_when_content_fits() {
        assert!(geometry(Rect::new(0, 0, 80, 20), 19, 0).is_none());
        // Content exactly one screen tall: no bar ("overflow" is the only
        // predicate, not "could overflow").
        assert!(geometry(Rect::new(0, 0, 80, 20), 20, 0).is_none());
        // Empty chat (nothing rendered yet).
        assert!(geometry(Rect::new(0, 0, 80, 20), 0, 0).is_none());
    }

    #[test]
    fn test_no_geometry_for_degenerate_area() {
        assert!(geometry(Rect::new(0, 0, 0, 20), 100, 0).is_none());
        assert!(geometry(Rect::new(0, 0, 80, 0), 100, 0).is_none());
    }

    // ── thumb size / position ────────────────────────────────────────────

    #[test]
    fn test_track_covers_the_viewport_and_thumb_is_proportional() {
        let geom = even_geom(0);
        assert_eq!(geom.column, 79, "the bar owns the rightmost column");
        assert_eq!(geom.track_top, 0);
        assert_eq!(geom.track_bottom, 19);
        assert_eq!(geom.track_height(), 20);
        assert_eq!(geom.thumb_height(), 4, "20/100 of the track");
        assert_eq!(geom.max_scroll(), 80);
    }

    #[test]
    fn test_geometry_uses_area_offset() {
        // The chat area is not at the origin in the real layout.
        let geom = geometry(Rect::new(3, 5, 10, 8), 40, 0).expect("content overflows");
        assert_eq!(geom.column, 12);
        assert_eq!(geom.track_top, 5);
        assert_eq!(geom.track_bottom, 12);
        assert!(geom.thumb_top >= geom.track_top);
        assert!(geom.thumb_bottom <= geom.track_bottom);
    }

    #[test]
    fn test_thumb_at_both_extremes_is_exact() {
        let top = even_geom(0);
        assert_eq!(top.thumb_top, top.track_top);
        assert_eq!(top.thumb_bottom, top.track_top + 3);

        let bottom = even_geom(80);
        assert_eq!(
            bottom.thumb_bottom, bottom.track_bottom,
            "dragged all the way down → flush with the track end"
        );
        assert_eq!(bottom.thumb_top, bottom.track_bottom - 3);
    }

    #[test]
    fn test_thumb_position_is_clamped_to_the_track() {
        let geom = even_geom(5000);
        assert_eq!(geom.thumb_bottom, geom.track_bottom);
        assert_eq!(geom.thumb_top, geom.track_bottom - geom.thumb_height() + 1);
    }

    #[test]
    fn test_extreme_ratio_keeps_a_grabbable_thumb() {
        // 10 000 lines of history in a 10-row viewport: the proportional
        // height rounds to 0 without the floor.
        let geom = geometry(Rect::new(0, 0, 40, 10), 10_000, 0).expect("content overflows");
        assert_eq!(geom.thumb_height(), MIN_THUMB_HEIGHT);
        assert!(geom.thumb_top >= geom.track_top);
        assert!(geom.thumb_bottom <= geom.track_bottom);

        let bottom = geometry(Rect::new(0, 0, 40, 10), 10_000, 9_990).expect("content overflows");
        assert_eq!(bottom.thumb_bottom, bottom.track_bottom);
        assert_eq!(bottom.thumb_height(), MIN_THUMB_HEIGHT);
    }

    #[test]
    fn test_tiny_viewport_caps_thumb_at_track_height() {
        // A 1-row chat viewport: the thumb cannot be taller than the track.
        let geom = geometry(Rect::new(0, 0, 10, 1), 100, 0).expect("content overflows");
        assert_eq!(geom.thumb_height(), 1);
        assert_eq!(geom.track_height(), 1);
        // No travel → any pointer row maps to offset 0.
        assert_eq!(offset_for_row(&geom, geom.track_top, 0), 0);
        assert_eq!(offset_for_row(&geom, geom.track_bottom, 1), 0);
    }

    // ── hit testing ──────────────────────────────────────────────────────

    #[test]
    fn test_hit_requires_the_exact_column_and_a_track_row() {
        let geom = even_geom(40);
        assert!(hit(&geom, 79, 0));
        assert!(hit(&geom, 79, 19));
        assert!(!hit(&geom, 78, 10), "one column to the left is chat text");
        assert!(!hit(&geom, 80, 10), "outside the chat area");
        assert!(!hit(&geom, 79, 20), "one row below the chat area");
    }

    // ── pointer → offset mapping ─────────────────────────────────────────

    #[test]
    fn test_track_click_at_extremes_jumps_to_the_ends() {
        let geom = even_geom(40);
        // Bare track (the thumb sits in the middle), grip = thumb_height / 2.
        let grip = grip_at(&geom, geom.track_top);
        assert_eq!(grip, 2);
        assert_eq!(offset_for_row(&geom, geom.track_top, grip), 0);
        assert_eq!(offset_for_row(&geom, geom.track_bottom, grip), 80);

        // Just above / below the thumb centre also lands at the ends once the
        // thumb is clamped into the track.
        assert_eq!(offset_for_row(&geom, geom.track_top + 1, grip), 0);
        assert_eq!(offset_for_row(&geom, geom.track_bottom - 1, grip), 80);
    }

    /// A press on the thumb keeps its position — the fine drag needs no `grip`.
    #[test]
    fn test_press_on_the_thumb_keeps_the_grabbed_row() {
        let geom = even_geom(40); // thumb rows 8..=11
        assert_eq!(geom.thumb_top, 8);
        for row in geom.thumb_top..=geom.thumb_bottom {
            let grip = grip_at(&geom, row);
            assert_eq!(grip, row - geom.thumb_top);
            // Grabbing a thumb row without moving the pointer must not jump.
            assert_eq!(
                offset_for_row(&geom, row, grip),
                40,
                "grabbing row {row} must be a no-op"
            );
        }
        // A row outside the thumb centres instead.
        assert_eq!(grip_at(&geom, geom.track_top), geom.thumb_height() / 2);
    }

    #[test]
    fn test_drag_clamps_at_both_ends() {
        let geom = even_geom(40);
        let grip = 1;
        // Pointer dragged far above the track (even out of the chat area).
        assert_eq!(offset_for_row(&geom, 0, grip), 0);
        for row in [geom.track_top, geom.track_top + 1] {
            assert_eq!(offset_for_row(&geom, row, grip), 0);
        }
        assert_eq!(offset_for_row(&geom, 9999, grip), 80);
        assert_eq!(offset_for_row(&geom, geom.track_bottom, grip), 80);
    }

    #[test]
    fn test_round_trip_is_lossless_when_the_ratio_divides_evenly() {
        // travel = 16, max_scroll = 80 → 5 lines per row.
        for offset in (0..=80).step_by(5) {
            let geom = even_geom(offset);
            let row = geom.thumb_top;
            let grip = 0;
            assert_eq!(
                offset_for_row(&geom, row, grip),
                offset,
                "offset {offset} → row {row} → offset"
            );
        }
    }

    #[test]
    fn test_mapping_is_monotonic_for_odd_ratios() {
        // 137 lines in 11 rows: fractional ratios everywhere, so the mapping
        // quantises — but it must never go backwards, must stay within one
        // row's worth of the requested offset, and must be exact at the end.
        let area = Rect::new(0, 0, 30, 11);
        let mut prev = 0;
        for offset in 0..=126 {
            let geom = geometry(area, 137, offset).expect("content overflows");
            let mapped = offset_for_row(&geom, geom.thumb_top, 0);
            assert!(mapped >= prev, "monotonic at offset {offset}");
            let drift = (mapped as i64 - offset as i64).abs();
            assert!(drift <= 14, "within one row's worth at offset {offset}");
            prev = mapped;
        }
        assert_eq!(prev, 126, "the bottom is exact");
    }

    #[test]
    fn test_state_clear_reports_changes() {
        let mut state = ScrollbarState::default();
        assert!(!state.clear(), "already idle → no change, no redraw");
        state.hovered = true;
        assert!(state.clear());
        assert!(!state.is_active());
        assert_eq!(state.grip, 0);

        state.dragging = true;
        state.grip = 3;
        assert!(state.clear());
        assert!(!state.is_active());
        assert_eq!(state.grip, 0);
    }

    // ── painting ─────────────────────────────────────────────────────────

    fn text_of_column(buf: &Buffer, column: u16) -> String {
        (buf.area.y..buf.area.bottom())
            .map(|row| buf[(column, row)].symbol())
            .collect()
    }

    #[test]
    fn test_paint_draws_track_and_thumb_and_nothing_else() {
        let area = Rect::new(0, 2, 20, 6);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 10));
        let geom = geometry(area, 100, 0).expect("content overflows");
        paint(&mut buf, &geom, ScrollbarState::default(), &palette());

        // Track column: thumb at the top, hairline below; rows outside the
        // chat area stay blank (rows 0/1 and 8/9 here).
        let column = text_of_column(&buf, 19);
        assert_eq!(column, "  ┃┃││││  ", "rows 2..4 thumb, 4..8 track");
        // Rows outside the chat area are untouched.
        assert_eq!(buf[(19, 0)].symbol(), " ");
        assert_eq!(buf[(19, 1)].symbol(), " ");
        assert_eq!(buf[(19, 8)].symbol(), " ");
        assert_eq!(buf[(19, 9)].symbol(), " ");
        // Nothing else is painted.
        for row in buf.area.y..buf.area.bottom() {
            for x in 0..19 {
                assert_eq!(buf[(x, row)].symbol(), " ", "cell ({x},{row}) untouched");
            }
        }
    }

    #[test]
    fn test_paint_emphasises_the_bar_while_active() {
        let area = Rect::new(0, 0, 10, 6);
        let geom = geometry(area, 100, 0).expect("content overflows");
        let idle = palette();

        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        paint(&mut buf, &geom, ScrollbarState::default(), &idle);
        let idle_thumb = buf[(9, 0)].clone();
        let idle_track = buf[(9, 5)].clone();
        assert_eq!(idle_thumb.symbol(), "┃", "idle thumb is the heavy hairline");
        assert_eq!(idle_thumb.style().fg, Some(idle.thinking));
        assert!(!idle_thumb.style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(idle_track.symbol(), "│", "idle track is the hairline");
        assert_eq!(idle_track.style().fg, Some(idle.dim));

        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        paint(
            &mut buf,
            &geom,
            ScrollbarState {
                hovered: true,
                ..Default::default()
            },
            &idle,
        );
        let hot_thumb = buf[(9, 0)].clone();
        let hot_track = buf[(9, 5)].clone();
        assert_eq!(hot_thumb.symbol(), "█", "hover thickens the thumb");
        assert_eq!(hot_thumb.style().fg, Some(idle.accent));
        assert!(hot_thumb.style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            hot_track.style().fg,
            Some(idle.thinking),
            "the track brightens too (dim → thinking, visible in the default palette)"
        );
        assert_ne!(
            idle.dim, idle.thinking,
            "the default palette distinguishes them"
        );

        // Dragging looks the same as hovering (the pointer is on the bar).
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 6));
        paint(
            &mut buf,
            &geom,
            ScrollbarState {
                dragging: true,
                grip: 1,
                ..Default::default()
            },
            &idle,
        );
        assert_eq!(buf[(9, 0)].symbol(), "█");
    }

    #[test]
    fn test_paint_keeps_the_background_of_the_content_below() {
        let area = Rect::new(0, 0, 6, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 4));
        for row in 0..4 {
            buf[(5, row)].set_bg(Color::Rgb(1, 2, 3));
        }
        let geom = geometry(area, 100, 0).expect("content overflows");
        paint(&mut buf, &geom, ScrollbarState::default(), &palette());
        assert_eq!(
            buf[(5, 3)].bg,
            Color::Rgb(1, 2, 3),
            "style merge: the overlay must not punch a hole in the content"
        );
    }

    #[test]
    fn test_paint_does_not_inherit_the_content_weights() {
        // `Cell::set_style` is additive, so a dim hint or a bold heading under
        // the bar would otherwise change the bar's own weight line by line.
        let area = Rect::new(0, 0, 8, 6);
        let geom = geometry(area, 100, 0).expect("content overflows");
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 6));
        for row in 0..6 {
            buf[(7, row)].modifier = Modifier::DIM | Modifier::BOLD;
        }

        paint(&mut buf, &geom, ScrollbarState::default(), &palette());
        for row in 0..6 {
            assert_eq!(
                buf[(7, row)].modifier,
                Modifier::empty(),
                "row {row}: the idle bar owns its weight"
            );
        }

        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 6));
        for row in 0..6 {
            buf[(7, row)].modifier = Modifier::DIM | Modifier::BOLD;
        }
        paint(
            &mut buf,
            &geom,
            ScrollbarState {
                dragging: true,
                grip: 0,
                ..Default::default()
            },
            &palette(),
        );
        for row in 0..6 {
            let expected = if buf[(7, row)].symbol() == "█" {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };
            assert_eq!(buf[(7, row)].modifier, expected, "row {row}");
        }
    }
}
