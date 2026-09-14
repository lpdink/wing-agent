//! Shared fixtures for the chat view's test modules.
//!
//! One definition each, so every topic module's tests stay about their own
//! subject instead of re-declaring render helpers. Only used under
//! `cfg(test)`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::config::rendering::ThinkingMode;
use crate::config::{LayoutConfig, ThemePalette};
use crate::render::renderable::CellContext;
use crate::ui::selection::Selection;
use crate::ui::selection::SelectionPoint;

use super::ChatView;
use super::ChatViewWidget;

pub(super) fn test_ctx() -> (ThemePalette, LayoutConfig) {
    (ThemePalette::default(), LayoutConfig::default())
}

pub(super) fn make_ctx<'a>(p: &'a ThemePalette, l: &'a LayoutConfig) -> CellContext<'a> {
    CellContext {
        palette: p,
        thinking_mode: ThinkingMode::Visible,
        layout: l,
    }
}

/// Render the view into a buffer at the given size.
pub(super) fn render_view(view: &mut ChatView, width: u16, height: u16) -> Buffer {
    let (p, l) = test_ctx();
    let ctx = make_ctx(&p, &l);
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    ChatViewWidget::new(view, ctx).render(area, &mut buf);
    buf
}

/// Render the view into a `screen`-sized buffer, laying the chat band out
/// at `band` — lets tests assert that nothing outside the band is touched.
pub(super) fn render_view_in(view: &mut ChatView, screen: Rect, band: Rect) -> Buffer {
    let (p, l) = test_ctx();
    let ctx = make_ctx(&p, &l);
    let mut buf = Buffer::empty(screen);
    ChatViewWidget::new(view, ctx).render(band, &mut buf);
    buf
}

/// Flatten a Buffer into a string (one line per row) for assertions.
pub(super) fn buffer_text(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in buf.area.y..buf.area.bottom() {
        for x in buf.area.x..buf.area.right() {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// A selection covering `bounds` (press + drag), for the paint tests.
pub(super) fn selection_over(bounds: (SelectionPoint, SelectionPoint)) -> Selection {
    let mut selection = Selection::default();
    selection.begin(bounds.0);
    selection.drag_to(bounds.1);
    selection
}

pub(super) fn reversed_columns(buf: &Buffer, row: u16) -> Vec<u16> {
    (buf.area.x..buf.area.right())
        .filter(|&x| buf[(x, row)].modifier.contains(Modifier::REVERSED))
        .collect()
}

pub(super) fn span_texts(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.to_string())
        .collect()
}
