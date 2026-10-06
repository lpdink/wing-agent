//! Renderable trait — contract for width-aware renderable elements.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::config::LayoutConfig;
use crate::config::ThemePalette;
use crate::config::rendering::ThinkingMode;
use crate::render::markdown::ImageOpts;

/// Context passed to cell rendering methods.
pub struct CellContext<'a> {
    pub palette: &'a ThemePalette,
    /// `rendering.thinking` — 思考块默认怎么呈现：`visible` 默认展开、
    /// `hidden` 默认折叠（标题行是所有思考块的固有部分）。
    pub thinking_mode: ThinkingMode,
    /// `Ctrl+O` 的全局展开覆盖（`None` = 跟随 [`Self::thinking_mode`] 的默认）。
    ///
    /// 一个会话级开关、渲染期经它下发（见 [`ThinkingMode::expanded`]）：
    /// 整条 transcript 一起切，会话内一直有效。
    pub thinking_expanded: Option<bool>,
    pub layout: &'a LayoutConfig,
    /// Image options (mode, workspace root, metadata table) for this frame.
    ///
    /// One value per frame and the **only** source of truth for a cell's
    /// image anchors: [`CachedCell`](crate::ui::cached_cell::CachedCell)
    /// adopts it before every projection, so the non-streaming `RenderOpts`
    /// and the streaming engine can never disagree about the row count of
    /// the same picture. [`ImageOpts::off`] is the shared "no anchors" value.
    pub images: &'a ImageOpts,
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
