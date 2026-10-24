//! ComposerWidget — renders the composer card in the terminal: the frame, the
//! two rails and the word-wrapped draft.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::InputArea;
use super::chrome;
use super::chrome::ActivityRail;
use super::chrome::Chrome;
use super::chrome::Focus;
use super::chrome::MetaRail;
use super::model;
use super::wrap;
use crate::config::ThemePalette;
use crate::render::markdown::truncate_to_display_width;

/// The composer — the card holding the draft, its activity rail and its meta
/// rail (see [`chrome`] for the shape).
pub struct ComposerWidget<'a> {
    input: &'a mut InputArea,
    palette: &'a ThemePalette,
    /// Top rail content: `None` while no turn is running — the rail then shows
    /// the frame's own state (the empty card's invite, or a plain rule).
    activity: Option<ActivityRail<'a>>,
    meta: MetaRail<'a>,
    /// Whether another layer owns the keyboard (see [`Self::keyboard_held`]).
    keyboard_held: bool,
}

impl<'a> ComposerWidget<'a> {
    pub fn new(
        input: &'a mut InputArea,
        palette: &'a ThemePalette,
        activity: Option<ActivityRail<'a>>,
        meta: MetaRail<'a>,
    ) -> Self {
        Self {
            input,
            palette,
            activity,
            meta,
            keyboard_held: false,
        }
    }

    /// A panel (ask / model picker) owns the keyboard: the draft is frozen
    /// behind it, so the card *ghosts* — the text and the outline go quiet,
    /// nothing promises that typing lands here — instead of looking ready for a
    /// keystroke it will never get.
    pub fn keyboard_held(mut self, held: bool) -> Self {
        self.keyboard_held = held;
        self
    }
}

impl Widget for ComposerWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Record the rect this frame drew into (empty when collapsed) — mouse
        // events arrive between frames and hit testing reads it back.
        self.input.rendered_area = area;
        if area.height == 0 || area.width == 0 {
            return;
        }

        let chrome = Chrome::of(area);
        // Every width below is the one the wrapping was computed with, so the
        // drawn rows and the pointer mapping cannot disagree.
        let text_area_w = chrome.text_width as usize;

        // Build visual rows.
        let vis_rows =
            wrap::build_visual_rows(&self.input.lines, &self.input.chips, text_area_w.max(1));

        // Update vertical scroll.
        self.input.update_vertical_scroll(chrome);

        // The one description of what the card is; every register below reads it,
        // so the frame, the caret and the chips cannot drift apart.
        let has_draft = !self.input.is_empty();
        let focus = Focus::of(self.keyboard_held, has_draft);

        // The frame first: the draft is painted into the rows it leaves.
        chrome::paint(
            buf,
            area,
            chrome,
            self.activity.as_ref(),
            &self.meta,
            focus,
            self.palette,
        );

        // A held draft is not editable right now: it is drawn the way the
        // chat draws text that is not active (dim), so the state is visible
        // without a word of explanation.
        let ghosted = matches!(focus, Focus::Ghost);
        // The body ink of a *draft*. An empty card has no body: its invitation
        // is a hint, so it wears the card's own ink (the frame's, see
        // `chrome::card_ink`) — the same brightness as a draft's text is what
        // made an empty card read as "there is already something here".
        let body_ink = if ghosted {
            self.palette.dim
        } else {
            self.palette.text
        };
        let draft_style = Style::default().fg(body_ink);

        let first_row_y = area.y + chrome.top_row();
        let visible_rows = chrome.text_rows as usize;
        let start_vis = self.input.vertical_scroll;
        let end_vis = (start_vis + visible_rows).min(vis_rows.len());

        for (display_idx, vis_idx) in (start_vis..end_vis).enumerate() {
            let y = first_row_y + display_idx as u16;
            let vr = &vis_rows[vis_idx];

            // The prompt glyph opens the draft — first visual row of the first
            // logical line only; every other row keeps the text alignment.
            //
            // It is the pointer at the place typing lands, so it follows the
            // draft: `accent` while something actually waits there (the caret
            // and the pasted chips are the row's saturated inks), the card's
            // own ink while the card is still empty — an empty card carries
            // one register, and the point of it is that the invitation must
            // not look like content. A card whose keys another layer holds
            // keeps the ghost's `dim` (which is what the card's ink is then).
            if chrome.card && vr.logical_line == 0 && vr.char_start == 0 {
                let ink = match focus {
                    Focus::Draft => self.palette.accent,
                    // 空卡与幽灵态都穿卡自己的墨（`tool_result` / `dim`）：一个来源，
                    // 见 `chrome::card_ink`。
                    quiet => chrome::card_ink(quiet, self.palette),
                };
                buf.set_span(
                    area.x + chrome::PROMPT_X,
                    y,
                    &Span::styled(chrome::PROMPT, Style::default().fg(ink)),
                    2,
                );
            }

            let text_x = area.x + chrome.text_x;

            // Empty first line → show the placeholder, in the card's own ink: it
            // is a sentence that tells you what could go here, not something you
            // wrote, so it stays one notch under the body. A ghosted card drops
            // it to `dim` with its draft — the card promises nothing right now.
            if vr.logical_line == 0 && vr.char_start == 0 && !has_draft {
                let placeholder_w = self.input.placeholder.width();
                let placeholder = if placeholder_w > text_area_w {
                    truncate_to_display_width(&self.input.placeholder, text_area_w)
                } else {
                    self.input.placeholder.clone()
                };
                buf.set_line(
                    text_x,
                    y,
                    &Line::from(Span::styled(
                        placeholder,
                        Style::default().fg(chrome::card_ink(focus, self.palette)),
                    )),
                    chrome.text_width,
                );
                continue;
            }

            // Extract the visual row's runs from the logical line: the text
            // around a chip is drawn as text, the chip as the one *thing* it
            // is — the accent is what tells a paste from typing.
            let runs = model::runs(
                &self.input.lines[vr.logical_line],
                &self.input.chips,
                vr.char_start..vr.char_end,
            );
            if runs.is_empty() {
                continue;
            }
            let chip_style = if ghosted {
                draft_style
            } else {
                Style::default().fg(self.palette.accent)
            };
            let spans: Vec<Span> = runs
                .into_iter()
                .map(|(text, is_chip)| {
                    Span::styled(text, if is_chip { chip_style } else { draft_style })
                })
                .collect();

            buf.set_line(text_x, y, &Line::from(spans), chrome.text_width);
        }
    }
}

/// Get the cursor screen position for external cursor positioning.
///
/// Returns `(x, y)` absolute coordinates.
pub fn cursor_screen_pos(input: &InputArea, area: &Rect) -> (u16, u16) {
    input.cursor_screen_pos(area)
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;

    use super::*;
    use crate::ui::status_bar::TurnUsage;

    /// The two inks the card's first text row opens with.
    struct FirstRow {
        /// The `❯` glyph.
        caret: Option<Color>,
        /// The first cell of the text behind it (placeholder or draft).
        text: Option<Color>,
    }

    /// Render one 60-column card and read the inks of its first text row.
    fn first_row(draft: &str, keyboard_held: bool) -> FirstRow {
        let palette = ThemePalette::default();
        let mut input = InputArea::new("ask anything");
        input.set_text(draft);
        let usage = TurnUsage::default();
        let area = Rect::new(0, 0, 60, 3);
        let mut buf = Buffer::empty(area);
        ComposerWidget::new(&mut input, &palette, None, MetaRail::bare(&usage))
            .keyboard_held(keyboard_held)
            .render(area, &mut buf);
        // The prompt opens the first draft row, right of the card's border; the
        // text starts two cells further right.
        let chrome = Chrome::of(area);
        let y = area.y + chrome.top_row();
        FirstRow {
            caret: buf[(area.x + chrome::PROMPT_X, y)].style().fg,
            text: buf[(area.x + chrome.text_x, y)].style().fg,
        }
    }

    /// The caret is the pointer at the place typing lands, so it follows the
    /// draft: `accent` while a draft waits in the card, the frame's own
    /// `tool_result` while the card is still empty (one register for a card
    /// that has nothing in it), and `dim` when another layer holds the keys.
    #[test]
    fn the_caret_follows_the_draft() {
        let palette = ThemePalette::default();
        assert_eq!(
            first_row("", false).caret,
            Some(palette.tool_result),
            "empty: the caret belongs to the card, not to a draft"
        );
        assert_eq!(
            first_row("hello", false).caret,
            Some(palette.accent),
            "draft: accent"
        );
        for draft in ["", "hello"] {
            assert_eq!(
                first_row(draft, true).caret,
                Some(palette.dim),
                "held keys: dim, with or without a draft ({draft:?})"
            );
        }
    }

    /// The placeholder is a hint, so it wears the hint ink (`tool_result`) —
    /// clearly under the body a draft is written in. That the two differ is the
    /// whole point: an invitation drawn at body brightness reads as content. A
    /// ghosted card drops it to `dim` with everything else.
    #[test]
    fn the_placeholder_is_a_hint_not_the_body() {
        let palette = ThemePalette::default();
        assert_eq!(
            first_row("", false).text,
            Some(palette.tool_result),
            "empty: the invitation is one notch under the body"
        );
        assert_ne!(
            first_row("hello", false).text,
            first_row("", false).text,
            "a draft and the invitation are not the same ink"
        );
        assert_eq!(
            first_row("hello", false).text,
            Some(palette.text),
            "a draft is the body"
        );
        assert_eq!(
            first_row("", true).text,
            Some(palette.dim),
            "held keys: nothing promises this is live"
        );
    }
}
