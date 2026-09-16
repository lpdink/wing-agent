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

use crate::config::rendering::ThinkingMode;
use crate::render::Renderable;
use crate::render::markdown::ComposedLines;
use crate::render::markdown::LinkSpan;
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
    /// The width at which the cell's current lines are pre-wrapped (≤
    /// width, blittable directly). `None` = not pre-wrapped. Lives at the
    /// (width, generation) lifecycle of the lines: a width change or a
    /// re-render through `to_lines` clears it, so stale pre-wrapped lines
    /// are never blitted (they would truncate instead of wrapping).
    prewrapped_width: Option<u16>,
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
    /// Lines + their markdown link spans. The spans ride along so the render
    /// loop can inject OSC8 and hit-test clicks without re-rendering the cell
    /// (`tui-link-open`).
    composed: ComposedLines,
    /// Whether each line maps to exactly one screen row, i.e. whether the link
    /// spans' row arithmetic holds (see where it is computed for the one case
    /// that breaks it). Always meaningful for link-bearing lines; `true` for
    /// line sets without links (nothing to place).
    rows_exact: bool,
}

/// What the chat view needs to render a cell: its lines, their link spans and
/// whether screen row == line index.
pub struct CellLines<'a> {
    pub lines: &'a [Line<'static>],
    /// Index-aligned with `lines`.
    pub links: &'a [Vec<LinkSpan>],
    /// Row arithmetic is exact (see [`CachedLines::rows_exact`]).
    pub rows_exact: bool,
}

impl CellLines<'_> {
    /// Whether any line carries a link — the render loop's gate for the
    /// per-frame link work (no allocation, no clone: a cell without links
    /// stays free).
    pub fn has_links(&self) -> bool {
        self.links.iter().any(|line| !line.is_empty())
    }
}

impl CachedCell {
    pub fn new(cell: ChatCell) -> Self {
        Self {
            cell,
            generation: 0,
            cached_height: None,
            cached_lines: None,
            stream: None,
            prewrapped_width: None,
            pending_finalize: false,
        }
    }

    /// Access the inner cell.
    pub fn cell(&self) -> &ChatCell {
        &self.cell
    }

    /// Mutate the inner cell and bump generation (invalidates all caches).
    ///
    /// CONTRACT: a streaming cell's text may only grow through
    /// [`append_stream`](CachedCell::append_stream) — the incremental
    /// render state holds its own copy of the text, and a mutation that
    /// changes the text would desynchronize the two (dirty render). The
    /// debug assertion below guards the invariant; non-text mutations
    /// (e.g. the thinking event counter) are fine.
    pub fn mutate<F: FnOnce(&mut ChatCell)>(&mut self, f: F) {
        let text_len_before = self.stream_text_len();
        f(&mut self.cell);
        self.invalidate();
        debug_assert_eq!(
            text_len_before,
            self.stream_text_len(),
            "CachedCell::mutate changed streaming text — streaming cells \
             must only grow via append_stream (stream buffer would desync)"
        );
    }

    /// Bump the generation and drop the blit fast path — the canonical
    /// "content changed" invalidation.
    fn invalidate(&mut self) {
        self.generation += 1;
        // The lines will be rebuilt by `to_lines` (which never pre-wraps):
        // drop the blit fast path here rather than relying on
        // `update_heights` having refreshed it earlier in the same frame.
        self.prewrapped_width = None;
    }

    /// Text length of a streaming cell (None when not streaming) — the
    /// debug-checked invariant anchor for `mutate`.
    fn stream_text_len(&self) -> Option<usize> {
        self.stream.as_ref()?;
        match &self.cell {
            ChatCell::Thinking(block) => Some(block.content.len()),
            ChatCell::AssistantMessage(text) => Some(text.len()),
            _ => None,
        }
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
        self.prewrapped_width = None;
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
        // Existing text, captured BEFORE this delta lands in the cell —
        // only the delta that CREATES the stream needs it (replay/resume
        // path: the cell already carries content that the fresh stream
        // would otherwise not know about, and the rendered view would drop
        // the replayed prefix). Later deltas must not pay an O(text) clone
        // per event, which is why this is not hoisted out of the branch.
        let seed = if self.stream.is_none() {
            Some(match &self.cell {
                ChatCell::Thinking(block) => block.content.clone(),
                ChatCell::AssistantMessage(text) => text.clone(),
                _ => unreachable!("profile checked above"),
            })
        } else {
            None
        };
        match &mut self.cell {
            ChatCell::Thinking(block) => block.append(delta),
            ChatCell::AssistantMessage(content) => content.push_str(delta),
            _ => unreachable!("profile checked above"),
        }
        if let Some(stream) = self.stream.as_mut() {
            stream.push(delta);
        } else {
            // Invariant: stream.buf == cell text at all times.
            let mut stream = StreamingRender::new(profile);
            if let Some(seed) = seed.filter(|s| !s.is_empty()) {
                stream.push(&seed);
            }
            stream.push(delta);
            self.stream = Some(stream);
        }
    }

    /// Whether this cell is currently rendering through the incremental
    /// stream.
    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// Append a streaming tool-args fragment WITHOUT invalidating the
    /// generation cache.
    ///
    /// Appending is O(1); the parse + highlight refresh are deferred to the
    /// frame boundary ([`Self::flush_pending_args`], driven by the render
    /// path), so the per-event cost no longer scales with the accumulated
    /// payload — the O(n²) path that saturated the event channel during
    /// large Write/Bash streams.
    pub fn append_tool_args_fragment(&mut self, fragment: &str) {
        if let ChatCell::ToolCall(block) = &mut self.cell {
            block.append_args_fragment(fragment);
        }
    }

    /// Frame boundary for deferred tool-args work: parse once when
    /// fragments arrived since the last flush, then invalidate the render
    /// cache exactly once (the displayed content may have changed).
    pub fn flush_pending_args(&mut self) -> bool {
        let parsed = match &mut self.cell {
            ChatCell::ToolCall(block) => block.flush_pending_args(),
            _ => false,
        };
        if parsed {
            self.invalidate();
        }
        parsed
    }

    /// Frame boundary for the pending Bash timer: invalidate only when the
    /// timer's *displayed* value moved (the heartbeat is 100 ms, the
    /// display is whole seconds).
    pub fn tick_bash_timer(&mut self) -> bool {
        let moved = match &mut self.cell {
            ChatCell::ToolCall(block) => block.tick_timer(),
            _ => false,
        };
        if moved {
            self.invalidate();
        }
        moved
    }

    /// Whether the incremental stream is the active RENDER authority at
    /// this context. Hidden thinking mode bypasses it: the stream keeps
    /// accumulating (for a later mode change / finalize) but the visible
    /// lines come from the cell's own renderer (the hidden indicator),
    /// which is the only place ThinkingMode is honored.
    fn stream_render_active(&self, ctx: &CellContext<'_>) -> bool {
        if self.stream.is_none() {
            return false;
        }
        match &self.cell {
            ChatCell::Thinking(_) => ctx.thinking_mode == ThinkingMode::Visible,
            _ => true,
        }
    }

    /// Whether the cell's lines at `width` are pre-wrapped (≤ width) —
    /// the render loop blits these directly instead of going through
    /// `Paragraph`. Width- and generation-aware: a resize or a re-render
    /// through `to_lines` invalidates it.
    pub fn is_prewrapped(&self, width: u16, ctx: &CellContext<'_>) -> bool {
        if self.stream_render_active(ctx) {
            return true;
        }
        self.prewrapped_width == Some(width)
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
    ///
    /// Hidden thinking mode never rendered through the stream — drop it
    /// and fall back to the cell's own renderer (the hidden indicator
    /// line); the accumulated text stays in the cell.
    fn run_finalize(&mut self, width: u16, ctx: &CellContext<'_>) {
        self.pending_finalize = false;
        let hidden_thinking = matches!(self.cell, ChatCell::Thinking(_))
            && ctx.thinking_mode != ThinkingMode::Visible;
        if hidden_thinking {
            self.stream = None;
            self.cached_lines = None;
            self.cached_height = None;
            self.prewrapped_width = None;
            return;
        }
        if let Some(mut stream) = self.stream.take() {
            stream.finalize(width, ctx.palette);
            let (lines, links) = stream.lines_and_links(width, ctx.palette);
            let composed = ComposedLines::new(lines.to_vec(), links.to_vec());
            let height = composed.lines().len();
            self.cached_lines = Some(CachedLines {
                width,
                generation: self.generation,
                composed,
                // A finalized stream renders pre-wrapped lines (every line was
                // hard-wrapped to the width), so the row maths is exact.
                rows_exact: true,
            });
            self.cached_height = Some(CachedHeight {
                width,
                generation: self.generation,
                height,
            });
            self.prewrapped_width = Some(width);
        }
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
        self.compute_cell_lines(width, ctx).lines
    }

    /// [`compute_lines`](Self::compute_lines) plus the link spans of every
    /// line (index-aligned with them) and whether the row maths is exact.
    pub fn compute_cell_lines(&mut self, width: u16, ctx: &CellContext<'_>) -> CellLines<'_> {
        // Frame boundary: settle deferred streaming work (tool-args parse)
        // before the caches are consulted — the generation check below must
        // see the invalidation a flush produces.
        self.flush_pending_args();
        if self.pending_finalize {
            self.run_finalize(width, ctx);
        }
        if self.stream_render_active(ctx) {
            // Order matters: the flag is written before the borrow that
            // produces the returned slices is taken.
            self.prewrapped_width = Some(width);
            let stream = self.stream.as_mut().expect("checked active");
            let (lines, links) = stream.lines_and_links(width, ctx.palette);
            // Streaming lines are hard-wrapped to the width by construction.
            return CellLines {
                lines,
                links,
                rows_exact: true,
            };
        }
        let cached_valid = self
            .cached_lines
            .as_ref()
            .is_some_and(|c| c.generation == self.generation && c.width == width);

        if !cached_valid {
            let composed = self.cell.render_lines(width, ctx);
            // The row arithmetic is what the link hit boxes depend on: a line
            // wider than the render area is wrapped by `Paragraph` into two
            // screen rows, and every link row after it would land on the wrong
            // text. Markdown pre-wraps prose, but code blocks and indented code
            // are exempt (`wrap::is_prose_line`), so an over-wide code line in
            // the same cell really does break the mapping — measure and refuse
            // rather than point a click at code.
            let rows_exact = !composed.has_links() || composed.rows_are_exact(width);
            self.cached_lines = Some(CachedLines {
                width,
                generation: self.generation,
                composed,
                rows_exact,
            });
            // Lines from `to_lines` are not pre-wrapped — never blit them.
            self.prewrapped_width = None;
        }

        let cached = self.cached_lines.as_ref().expect("just ensured");
        CellLines {
            lines: cached.composed.lines(),
            links: cached.composed.links(),
            rows_exact: cached.rows_exact,
        }
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
        // Frame boundary — see `compute_cell_lines`. Heights for a
        // streaming tool cell depend on the parsed args (command line,
        // preview lines), so the flush must run before the cache check.
        self.flush_pending_args();
        if self.pending_finalize {
            self.run_finalize(width, ctx);
        }
        if self.stream_render_active(ctx) {
            // Pre-wrapped flat lines: the line count IS the height (O(1)).
            // (A finalized streaming cell's height was cached by
            // run_finalize and hits the cached_height check below.)
            let stream = self.stream.as_mut().expect("checked active");
            let height = stream.lines(width, ctx.palette).len();
            self.cached_height = Some(CachedHeight {
                width,
                generation: self.generation,
                height,
            });
            self.prewrapped_width = Some(width);
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
