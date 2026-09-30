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
use super::chrome::MetaRail;
use super::helpers::char_to_byte;
use super::helpers::is_placeholder_line;
use super::helpers::truncate_by_width;
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
    /// behind it, so the card *ghosts* — the text goes quiet, nothing promises
    /// that typing lands here — instead of looking ready for a keystroke it
    /// will never get.
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
        let vis_rows = wrap::build_visual_rows(&self.input.lines, text_area_w.max(1));

        // Update vertical scroll.
        self.input.update_vertical_scroll(chrome);

        // The frame first: the draft is painted into the rows it leaves.
        chrome::paint(
            buf,
            area,
            chrome,
            self.activity.as_ref(),
            &self.meta,
            self.palette,
        );

        // The prompt glyph is what says "there is something to send": the one
        // accent the card carries, and only while it can be acted on.
        let live = !self.keyboard_held && !self.input.is_empty();

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
            let line_text = &self.input.lines[vr.logical_line];

            // The prompt glyph opens the draft — first visual row of the first
            // logical line only; every other row keeps the text alignment.
            // It lights up as soon as there is something to send.
            if chrome.card && vr.logical_line == 0 && vr.char_start == 0 {
                let style = if live {
                    Style::default().fg(self.palette.accent)
                } else {
                    Style::default().fg(self.palette.dim)
                };
                buf.set_span(
                    area.x + chrome::PROMPT_X,
                    y,
                    &Span::styled(chrome::PROMPT, style),
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

            // Extract the visual row's text slice from the logical line.
            let byte_start = char_to_byte(line_text, vr.char_start);
            let byte_end = char_to_byte(line_text, vr.char_end);
            let vis_text = &line_text[byte_start..byte_end];

            if vis_text.is_empty() {
                continue;
            }

            let clipped = truncate_by_width(vis_text, text_area_w);

            let style = if is_placeholder_line(line_text) && !self.keyboard_held {
                Style::default().fg(self.palette.accent)
            } else {
                draft_style
            };

            buf.set_line(
                text_x,
                y,
                &Line::from(Span::styled(clipped, style)),
                chrome.text_width,
            );
        }
    }
}

/// Get the cursor screen position for external cursor positioning.
///
/// Returns `(x, y)` absolute coordinates.
pub fn cursor_screen_pos(input: &InputArea, area: &Rect) -> (u16, u16) {
    input.cursor_screen_pos(area)
}
