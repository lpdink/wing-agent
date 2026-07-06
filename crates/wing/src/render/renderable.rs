//! Renderable trait — contract for width-aware renderable elements.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::config::LayoutConfig;
use crate::config::ThemePalette;
use crate::config::rendering::ThinkingMode;

/// Context passed to cell rendering methods.
pub struct CellContext<'a> {
    pub palette: &'a ThemePalette,
    pub thinking_mode: ThinkingMode,
    pub layout: &'a LayoutConfig,
}

/// A renderable element with width-aware height estimation.
///
/// Implementors MUST ensure `desired_height(width)` and `render(area, buf)`
/// use the same line generation logic and wrapping configuration.
/// Violating this contract causes rendering artifacts (clipping, gaps).
pub trait Renderable {
    /// Render this element into the given area.
    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &CellContext<'_>);

    /// Returns the number of terminal rows needed to render this element
    /// at the given width, accounting for word-wrap.
    fn desired_height(&self, width: u16, ctx: &CellContext<'_>) -> usize;
}
