//! CachedCell — ChatCell wrapper with generation-based caching.
//!
//! Caches both rendered lines and width-aware height to avoid
//! redundant `render_markdown()` calls during rendering.

use ratatui::text::Line;

use crate::render::Renderable;
use crate::render::renderable::CellContext;
use crate::ui::chat_view::ChatCell;

/// A ChatCell wrapper with cached lines and width-aware height.
///
/// Lines and height are recomputed when:
/// - Terminal width changes (height only)
/// - Cell content changes (generation counter mismatch)
pub struct CachedCell {
    cell: ChatCell,
    /// Monotonically increasing counter, bumped on every content mutation.
    generation: u64,
    /// Cached height and the (width, generation) it was computed at.
    cached_height: Option<CachedHeight>,
    /// Cached rendered lines (generation-keyed).
    cached_lines: Option<CachedLines>,
}

#[derive(Clone, Copy)]
struct CachedHeight {
    width: u16,
    generation: u64,
    height: usize,
}

struct CachedLines {
    width: u16,
    generation: u64,
    lines: Vec<Line<'static>>,
}

impl CachedCell {
    pub fn new(cell: ChatCell) -> Self {
        Self {
            cell,
            generation: 0,
            cached_height: None,
            cached_lines: None,
        }
    }

    /// Access the inner cell.
    pub fn cell(&self) -> &ChatCell {
        &self.cell
    }

    /// Mutate the inner cell and bump generation (invalidates all caches).
    pub fn mutate<F: FnOnce(&mut ChatCell)>(&mut self, f: F) {
        f(&mut self.cell);
        self.generation += 1;
    }

    /// Current generation counter (test seam: observe cache invalidation).
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Replace the inner cell entirely and bump generation (invalidates all caches).
    pub fn replace(&mut self, cell: ChatCell) {
        self.cell = cell;
        self.generation += 1;
        self.cached_height = None;
        self.cached_lines = None;
    }

    /// Get or compute cached lines. Single source of truth for rendered output.
    ///
    /// Width-aware: invalidates cache when width changes (for full-width elements
    /// like Separator and UserMessage card).
    pub fn compute_lines(&mut self, width: u16, ctx: &CellContext<'_>) -> &[Line<'static>] {
        let cached_valid = self
            .cached_lines
            .as_ref()
            .is_some_and(|c| c.generation == self.generation && c.width == width);

        if !cached_valid {
            let lines = self.cell.to_lines(width, ctx);
            self.cached_lines = Some(CachedLines {
                width,
                generation: self.generation,
                lines,
            });
        }

        &self.cached_lines.as_ref().unwrap().lines
    }

    /// Width-aware height, using cache when possible.
    ///
    /// Delegates to `ChatCell::desired_height()` which handles cell-specific
    /// layout (e.g., UserMessage padding).
    pub fn compute_height(&mut self, width: u16, ctx: &CellContext<'_>) -> usize {
        if let Some(c) = self.cached_height
            && c.width == width
            && c.generation == self.generation
        {
            return c.height;
        }
        let height = self.cell.desired_height(width, ctx);
        self.cached_height = Some(CachedHeight {
            width,
            generation: self.generation,
            height,
        });
        height
    }

    /// Width-aware height, read-only (no caching side-effect on lines).
    pub fn desired_height(&self, width: u16, ctx: &CellContext<'_>) -> usize {
        if let Some(c) = self.cached_height
            && c.width == width
            && c.generation == self.generation
        {
            return c.height;
        }
        self.cell.desired_height(width, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::rendering::ThinkingMode;
    use crate::config::{LayoutConfig, ThemePalette};

    fn test_ctx<'a>(palette: &'a ThemePalette, layout: &'a LayoutConfig) -> CellContext<'a> {
        CellContext {
            palette,
            thinking_mode: ThinkingMode::Visible,
            layout,
        }
    }

    #[test]
    fn test_cached_cell_new() {
        let cell = CachedCell::new(ChatCell::UserMessage("hello".into()));
        assert_eq!(cell.generation, 0);
        assert!(cell.cached_height.is_none());
        assert!(cell.cached_lines.is_none());
    }

    #[test]
    fn test_cached_cell_height_cached() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::UserMessage("hello".into()));
        let h1 = cell.compute_height(80, &ctx);
        let h2 = cell.compute_height(80, &ctx);
        assert_eq!(h1, h2);
        assert!(cell.cached_height.is_some());
    }

    #[test]
    fn test_cached_cell_lines_cached() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::UserMessage("hello".into()));
        let _ = cell.compute_lines(80, &ctx);
        assert!(cell.cached_lines.is_some());
        let cached_gen = cell.cached_lines.as_ref().unwrap().generation;
        let _ = cell.compute_lines(80, &ctx);
        assert_eq!(cell.cached_lines.as_ref().unwrap().generation, cached_gen);
    }

    #[test]
    fn test_cached_cell_compute_height() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::AssistantMessage("# Hello\n\nWorld".into()));
        let h = cell.compute_height(80, &ctx);
        assert!(h > 0);
        assert!(cell.cached_height.is_some());
    }

    #[test]
    fn test_cached_cell_mutate_invalidates() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::AssistantMessage("hello".into()));
        let _ = cell.compute_height(80, &ctx);
        let _ = cell.compute_lines(80, &ctx);
        assert!(cell.cached_height.is_some());
        assert!(cell.cached_lines.is_some());

        cell.mutate(|c| {
            if let ChatCell::AssistantMessage(text) = c {
                text.push_str(" world");
            }
        });

        assert_eq!(cell.generation, 1);
        assert_ne!(cell.cached_height.unwrap().generation, cell.generation);
        assert_ne!(
            cell.cached_lines.as_ref().unwrap().generation,
            cell.generation
        );
    }

    #[test]
    fn test_cached_cell_width_change() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::UserMessage("a ".repeat(100)));
        let h_narrow = cell.compute_height(40, &ctx);
        let h_wide = cell.compute_height(120, &ctx);
        assert!(h_narrow >= h_wide);
    }
}
