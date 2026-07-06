//! InputAreaWidget — renders the input area in the terminal with word-wrap.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::InputArea;
use super::helpers::PREFIX_WIDTH;
use super::helpers::char_to_byte;
use super::helpers::is_placeholder_line;
use super::helpers::truncate_by_width;
use super::wrap;
use crate::config::ThemePalette;
use crate::render::markdown::truncate_to_display_width;

/// Render the input area (multi-line with word-wrap).
pub struct InputAreaWidget<'a> {
    input: &'a mut InputArea,
    palette: &'a ThemePalette,
}

impl<'a> InputAreaWidget<'a> {
    pub fn new(input: &'a mut InputArea, palette: &'a ThemePalette) -> Self {
        Self { input, palette }
    }
}

impl Widget for InputAreaWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let text_area_w = area.width.saturating_sub(PREFIX_WIDTH);
        let text_width = text_area_w as usize;

        // Build visual rows.
        let vis_rows = wrap::build_visual_rows(&self.input.lines, text_width.max(1));

        // Update vertical scroll.
        self.input.update_vertical_scroll(area.height, area.width);

        let visible_rows = area.height as usize;
        let start_vis = self.input.vertical_scroll;
        let end_vis = (start_vis + visible_rows).min(vis_rows.len());

        // Track which logical line's first visual row we've rendered
        // (for prefix: "> " on first vis row of logical line 0, "  " otherwise).
        for (display_idx, vis_idx) in (start_vis..end_vis).enumerate() {
            let y = area.y + display_idx as u16;
            let vr = &vis_rows[vis_idx];
            let line_text = &self.input.lines[vr.logical_line];

            // Prefix: "> " for first visual row of first logical line,
            // "  " for all other visual rows.
            let prefix = if vr.logical_line == 0 && vr.char_start == 0 {
                "> "
            } else {
                "  "
            };
            buf.set_line(
                area.x,
                y,
                &Line::from(Span::styled(prefix, Style::default().fg(self.palette.dim))),
                PREFIX_WIDTH,
            );

            let text_x = area.x + PREFIX_WIDTH;

            // Empty first line → show placeholder.
            if vr.logical_line == 0 && vr.char_start == 0 && self.input.is_empty() {
                let placeholder_w = self.input.placeholder.width();
                let placeholder = if placeholder_w > text_area_w as usize {
                    truncate_to_display_width(&self.input.placeholder, text_area_w as usize)
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
                    text_area_w,
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

            let clipped = truncate_by_width(vis_text, text_area_w as usize);

            let style = if is_placeholder_line(line_text) {
                Style::default().fg(self.palette.accent)
            } else {
                Style::default().fg(self.palette.text)
            };

            buf.set_line(
                text_x,
                y,
                &Line::from(Span::styled(clipped, style)),
                text_area_w,
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
