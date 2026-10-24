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
    /// Top rail content: `None` while idle (the border is then a plain rule).
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

        // The frame first: the draft is painted into the rows it leaves.
        let focus = Focus::of(self.keyboard_held, !self.input.is_empty());
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
        let draft_style = if self.keyboard_held {
            Style::default().fg(self.palette.dim)
        } else {
            Style::default().fg(self.palette.text)
        };

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
            // accent = "there is a draft here"; an **empty** composer keeps it
            // dim, like the placeholder it sits next to (the empty session's
            // accent budget belongs to the workdir rail, not a caret with
            // nothing to point at). Same when another layer owns the keyboard:
            // the draft is drawn dim there, so a lit caret would contradict it.
            if chrome.card && vr.logical_line == 0 && vr.char_start == 0 {
                let ink = if self.keyboard_held || self.input.is_empty() {
                    self.palette.dim
                } else {
                    self.palette.accent
                };
                buf.set_span(
                    area.x + chrome::PROMPT_X,
                    y,
                    &Span::styled(chrome::PROMPT, Style::default().fg(ink)),
                    2,
                );
            }

            let text_x = area.x + chrome.text_x;

            // Empty first line → show placeholder.
            if vr.logical_line == 0 && vr.char_start == 0 && self.input.is_empty() {
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
                        Style::default().fg(self.palette.dim),
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
            let chip_style = if self.keyboard_held {
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

    /// Render one composer card and return the prompt glyph's foreground.
    fn prompt_ink(draft: &str) -> Option<Color> {
        let palette = ThemePalette::default();
        let mut input = InputArea::new("ask anything");
        input.set_text(draft);
        let usage = TurnUsage::default();
        let area = Rect::new(0, 0, 60, 3);
        let mut buf = Buffer::empty(area);
        ComposerWidget::new(&mut input, &palette, None, MetaRail::bare(&usage))
            .render(area, &mut buf);
        // The prompt opens the first draft row, right of the card's border.
        let x = area.x + chrome::PROMPT_X;
        let y = area.y + Chrome::of(area).top_row();
        buf[(x, y)].style().fg
    }

    /// An empty composer keeps the caret dim (it sits next to a dim
    /// placeholder); a draft lights it with the accent.
    #[test]
    fn the_caret_follows_whether_a_draft_exists() {
        let palette = ThemePalette::default();
        assert_eq!(prompt_ink(""), Some(palette.dim), "empty: dim");
        assert_eq!(prompt_ink("hello"), Some(palette.accent), "draft: accent");
    }
}
