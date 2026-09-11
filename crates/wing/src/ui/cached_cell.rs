//! CachedCell — ChatCell wrapper with generation-based caching.
//!
//! Caches both rendered lines and width-aware height to avoid
//! redundant `render_markdown()` calls during rendering.
//!
//! Streaming cells (Thinking / AssistantMessage while a turn is active)
//! carry a [`StreamingRender`] instead: deltas append through
//! `append_stream` without invalidating the generation cache — only the
//! active tail re-renders each sync, and heights come from the flat line
//! count (O(1)). At turn end the stream is reconciled
//! (`request_finalize` → the next render installs the full reference
//! render as the cached lines, flagged `prewrapped`).

use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::render::Renderable;
use crate::render::markdown::stream::{Profile, StreamingRender};
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
    /// Incremental rendering state for streaming cells. While present it
    /// is the render authority (the cell's text stays in sync for readers
    /// like `last_assistant_text`).
    stream: Option<StreamingRender>,
    /// True while the cell's lines are pre-wrapped (≤ width) and can be
    /// blitted directly — set for streaming cells and kept after their
    /// finalize.
    prewrapped: bool,
    /// Turn-end reconcile requested: the next `compute_lines` /
    /// `compute_height` installs the full reference render.
    pending_finalize: bool,
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
            stream: None,
            prewrapped: false,
            pending_finalize: false,
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
        self.stream = None;
        self.prewrapped = false;
        self.pending_finalize = false;
    }

    /// Append a streaming delta to this cell (Thinking / AssistantMessage).
    ///
    /// The cell's own text stays in sync (for text readers), and the
    /// incremental render state receives the delta WITHOUT invalidating
    /// the generation caches — only the active tail re-renders on the
    /// next sync.
    pub fn append_stream(&mut self, delta: &str) {
        let profile = match &self.cell {
            ChatCell::Thinking(_) => Profile::Thinking,
            ChatCell::AssistantMessage(_) => Profile::Content,
            _ => return,
        };
        match &mut self.cell {
            ChatCell::Thinking(block) => block.append(delta),
            ChatCell::AssistantMessage(content) => content.push_str(delta),
            _ => unreachable!("profile checked above"),
        }
        let stream = self
            .stream
            .get_or_insert_with(|| StreamingRender::new(profile));
        stream.push(delta);
    }

    /// Whether this cell is currently rendering through the incremental
    /// stream.
    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// Whether the cell's lines are pre-wrapped (≤ width) — the render
    /// loop blits these directly instead of going through `Paragraph`.
    pub fn is_prewrapped(&self) -> bool {
        self.prewrapped
    }

    /// Request the turn-end reconcile: the next render call installs the
    /// full reference render (Content keeps highlighting; Thinking stays
    /// plain) as the cell's cached lines and drops the stream state.
    pub fn request_finalize(&mut self) {
        if self.stream.is_some() {
            self.pending_finalize = true;
        }
    }

    /// Install the finalized reference render as the cached lines.
    fn run_finalize(&mut self, width: u16, ctx: &CellContext<'_>) {
        if let Some(mut stream) = self.stream.take() {
            stream.finalize(width, ctx.palette);
            let lines = stream.lines(width, ctx.palette).to_vec();
            let height = lines.len();
            self.cached_lines = Some(CachedLines {
                width,
                generation: self.generation,
                lines,
            });
            self.cached_height = Some(CachedHeight {
                width,
                generation: self.generation,
                height,
            });
        }
        self.pending_finalize = false;
        self.prewrapped = true;
    }

    /// Consume the wrapper and return the inner cell.
    pub fn into_inner(self) -> ChatCell {
        self.cell
    }

    /// Get or compute cached lines. Single source of truth for rendered output.
    ///
    /// Width-aware: invalidates cache when width changes (for full-width elements
    /// like Separator and UserMessage card).
    pub fn compute_lines(&mut self, width: u16, ctx: &CellContext<'_>) -> &[Line<'static>] {
        if self.pending_finalize {
            self.run_finalize(width, ctx);
        }
        if let Some(stream) = self.stream.as_mut() {
            self.prewrapped = true;
            return stream.lines(width, ctx.palette);
        }
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
    /// Reuses cached lines from `compute_lines` to compute height, avoiding a
    /// second markdown render that `Cell::desired_height()` would incur.
    /// Cell-specific layout (UserMessage padding) is handled by `lines_height`.
    ///
    /// Note: we clone the cached lines (to_vec) to release the internal borrow
    /// before the mutable `cached_height` assignment.  The clone is cheap
    /// (~50–100µs at 124 KB) relative to the saved markdown re‑render.
    pub fn compute_height(&mut self, width: u16, ctx: &CellContext<'_>) -> usize {
        if self.pending_finalize {
            self.run_finalize(width, ctx);
        }
        if let Some(stream) = self.stream.as_mut() {
            let height = stream.lines(width, ctx.palette).len();
            self.cached_height = Some(CachedHeight {
                width,
                generation: self.generation,
                height,
            });
            return height;
        }
        if self.prewrapped {
            // Finalized streaming cell — lines are pre-wrapped; the line
            // count is the height.
            if let Some(c) = self.cached_height
                && c.width == width
                && c.generation == self.generation
            {
                return c.height;
            }
            let lines_valid = self
                .cached_lines
                .as_ref()
                .is_some_and(|c| c.generation == self.generation && c.width == width);
            if !lines_valid {
                let _ = self.compute_lines(width, ctx);
            }
            let height = self
                .cached_lines
                .as_ref()
                .map(|c| c.lines.len())
                .unwrap_or(0);
            self.cached_height = Some(CachedHeight {
                width,
                generation: self.generation,
                height,
            });
            return height;
        }
        if let Some(c) = self.cached_height
            && c.width == width
            && c.generation == self.generation
        {
            return c.height;
        }
        // Compute lines first (caches them), then derive height from rendered
        // lines instead of calling cell.desired_height() which re‑renders.
        let height = {
            let li = self.compute_lines(width, ctx).to_vec();
            Self::lines_height(&self.cell, &li, width)
        };
        self.cached_height = Some(CachedHeight {
            width,
            generation: self.generation,
            height,
        });
        height
    }

    /// Compute height from already-rendered lines — no markdown re-render.
    ///
    /// Mirrors the cell-type-specific layout logic from `ChatCell::desired_height`
    /// (UserMessage inset padding vs. full-width wrapping) but works on the
    /// cached lines, avoiding a second call to `to_lines`.
    fn lines_height(cell: &ChatCell, lines: &[Line<'static>], width: u16) -> usize {
        match cell {
            ChatCell::UserMessage(_)
            | ChatCell::PendingUserMessage(_)
            | ChatCell::DiscardedUserMessage(_) => {
                let text_width = width.saturating_sub(3);
                Paragraph::new(lines.to_vec())
                    .wrap(Wrap { trim: false })
                    .line_count(text_width)
                    + 2
            }
            _ => Paragraph::new(lines.to_vec())
                .wrap(Wrap { trim: false })
                .line_count(width),
        }
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
