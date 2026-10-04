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

use std::path::PathBuf;

use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::render::Renderable;
use crate::render::markdown::ComposedLines;
use crate::render::markdown::ImageOpts;
use crate::render::markdown::ImageSpan;
use crate::render::markdown::LinkSpan;
use crate::render::markdown::Profile;
use crate::render::markdown::resolve_image_path;
use crate::render::markdown::stream::StreamingRender;
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
    /// Image options the cell renders with (mode, workspace, metadata) — see
    /// [`CachedCell::set_image_opts`]. The streaming engine owns a copy (it is
    /// its own render authority); this one is what a freshly started stream
    /// is seeded with.
    image_opts: ImageOpts,
    /// Local image paths the current lines reference — see
    /// [`CachedCell::image_candidates`]. Rebuilt with the lines, never with
    /// per-frame work.
    image_candidates: Vec<PathBuf>,
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
    /// Wrapped screen row of each anchored line, for cells whose lines do not
    /// map one row each — the picture placement's own arithmetic, which is
    /// per-anchor rather than per-cell (see [`image_rows_in_wrapped_rows`]).
    /// Empty when the line index *is* the row index.
    image_rows: Vec<(usize, usize)>,
}

/// The wrapped screen row of every anchored line, for a cell whose lines do not
/// map one row each (the caller only asks then — otherwise the index *is* the
/// row).
///
/// A line's screen row is its index **as long as every line above it fits the
/// width**: the widget renders a cell through a wrapping `Paragraph` whose
/// `scroll` is measured in rows, so an over-wide line above an anchor pushes it
/// down by the extra rows it wrapped into. Counting that per anchor — instead of
/// refusing the whole cell when *any* line is over-wide — is what lets a picture
/// after a long code line draw normally.
///
/// The rows come from [`wrapped_rows`], i.e. from the same `Paragraph` metric
/// the layout uses, so this cannot drift from what is rendered.
fn image_rows_in_wrapped_rows(
    lines: &[Line<'static>],
    anchors: &[Vec<ImageSpan>],
    width: u16,
) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut rows = 0usize;
    for (index, line) in lines.iter().enumerate() {
        if let Some(span) = anchors.get(index).and_then(|spans| spans.first()) {
            out.push((span.line, rows));
        }
        rows += wrapped_rows(line, width);
    }
    out
}

/// The screen rows one line occupies inside the widget's wrapping `Paragraph`.
///
/// A line that fits is one row (that is the whole point of `rows_exact`); an
/// over-wide one is measured by `Paragraph` itself — the very same call
/// [`CachedCell::lines_height`] uses for the cell's height, so the row index the
/// picture is placed at is the row the line is really rendered at. No
/// re-implementation of the wrapper, and no cross-line state to reproduce
/// (`WordWrapper` resets per input line).
fn wrapped_rows(line: &Line<'static>, width: u16) -> usize {
    if width == 0 || line.width() <= usize::from(width) {
        return 1;
    }
    Paragraph::new(vec![line.clone()])
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// What the chat view needs to render a cell: its lines, their link spans and
/// whether screen row == line index.
///
/// The narrow view of [`CellFrame`] — kept as it is because the link path and
/// the layout code consume exactly these three things.
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

/// [`CellLines`] plus the cell's image anchors — the superset the drawing
/// path reads (see [`CachedCell::compute_cell_frame`]).
///
/// `images[i]` belongs to `lines[i]`; a line that opens an anchor carries
/// exactly one entry, whose `rows` cover the lines below it (see
/// [`ImageSpan`]).
pub struct CellFrame<'a> {
    pub lines: &'a [Line<'static>],
    /// Index-aligned with `lines`.
    pub links: &'a [Vec<LinkSpan>],
    /// Index-aligned with `lines`.
    pub images: &'a [Vec<ImageSpan>],
    /// Row arithmetic is exact (see [`CachedLines::rows_exact`]) — the link
    /// boxes' and OSC8 injection's precondition.
    pub rows_exact: bool,
    /// The screen row of every anchored line, in the **wrapped** rows the
    /// widget renders with — empty when the line index *is* the row index (see
    /// [`CellFrame::image_row`]).
    pub image_rows: &'a [(usize, usize)],
}

impl CellFrame<'_> {
    /// Whether any line carries a link.
    pub fn has_links(&self) -> bool {
        self.links.iter().any(|line| !line.is_empty())
    }

    /// Whether any line carries an image anchor.
    pub fn has_images(&self) -> bool {
        self.images.iter().any(|line| !line.is_empty())
    }

    /// The row (relative to the cell's first rendered row) at which the anchor
    /// on `line` starts.
    ///
    /// `line` itself unless an over-wide line above it was wrapped into extra
    /// rows by the widget's `Paragraph` — the cell-level `rows_exact` flag is
    /// *not* what the picture needs (an over-wide line **below** an anchor
    /// leaves the anchor's own rows perfectly exact), so the walk is done per
    /// anchor instead of refusing the whole cell. See
    /// [`image_rows_in_wrapped_rows`].
    pub fn image_row(&self, line: usize) -> usize {
        self.image_rows
            .iter()
            .find(|(anchor_line, _)| *anchor_line == line)
            .map_or(line, |(_, row)| *row)
    }
}

/// The local image paths a rendered cell references — the caller's probe list.
///
/// Two sources, both already on the rendered lines: the link destinations (an
/// image on the link path keeps its destination in the line's `LinkSpan`) and
/// the anchors that were produced (once a picture is anchored, its link span
/// is replaced by the caption — the side channel is then the only trace left).
///
/// Resolution goes through [`resolve_image_path`] — the very function the
/// markdown layer keys the metadata table with — so a probed path and an
/// anchor's path can never disagree; nothing here touches the filesystem.
/// Duplicates are dropped per cell (the app's lane dedupes across cells by
/// looking every path up in its own table anyway).
fn image_candidates_of(
    links: &[Vec<LinkSpan>],
    anchors: &[Vec<ImageSpan>],
    opts: &ImageOpts,
) -> Vec<PathBuf> {
    if !opts.is_enabled() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !out.contains(&path) {
            out.push(path);
        }
    };
    for span in anchors.iter().flatten() {
        push(span.path.clone());
    }
    for span in links.iter().flatten() {
        if let Ok(path) = resolve_image_path(opts.workspace(), &span.target) {
            push(path);
        }
    }
    out
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
            image_opts: ImageOpts::default(),
            image_candidates: Vec::new(),
        }
    }

    /// Adopt image options (mode, workspace root, metadata table).
    ///
    /// The row count of an image anchor is a function of the metadata, so a
    /// change (a header probe finishing, the workspace moving, the mode being
    /// switched by config) invalidates every cached line and height of this
    /// cell **and** the streaming engine's composed prefix — the next render
    /// rebuilds both. Cheap when nothing changes: the comparison is structural.
    pub fn set_image_opts(&mut self, opts: ImageOpts) {
        if self.image_opts == opts {
            return;
        }
        self.image_opts = opts;
        if let Some(stream) = self.stream.as_mut() {
            stream.set_image_opts(self.image_opts.clone());
        }
        self.invalidate();
    }

    /// Adopt the render context's image options (idempotent).
    ///
    /// This is what keeps the two render paths on **one** set of options: a
    /// streaming cell gets them through
    /// [`set_image_opts`](Self::set_image_opts) (which feeds the incremental
    /// engine), a non-streaming one through `CellContext`'s `images` — both
    /// sourced from the same value by the projection entry points
    /// ([`compute_height`](Self::compute_height) /
    /// [`compute_cell_frame`](Self::compute_cell_frame)) before they read or
    /// write any cache. Without it, a late-arriving probe could change the
    /// row count on one path and not the other.
    fn sync_image_opts(&mut self, images: &ImageOpts) {
        if self.image_opts == *images {
            return;
        }
        self.set_image_opts(images.clone());
    }

    /// The image options this cell renders with.
    pub fn image_opts(&self) -> &ImageOpts {
        &self.image_opts
    }

    /// Local image paths the cell's current lines reference.
    ///
    /// Every markdown destination that resolves to a drawable local file
    /// ([`resolve_image_path`]), read off the rendered lines while they were
    /// built — an image on the link path keeps its destination in the line's
    /// `LinkSpan`, so the anchors this cell *would* produce are discoverable
    /// before the metadata table knows their shape. Rebuilt whenever the lines
    /// are (content, width or image options changed); empty when the lines have
    /// not been computed yet or images are off.
    ///
    /// The caller (the app's image lane) probes these and fills its metadata
    /// table; that table only grows within a content generation, so a path that
    /// leaves this list (its anchor replaced the link span) keeps the shape it
    /// was probed with — see `app::images`.
    pub fn image_candidates(&self) -> &[PathBuf] {
        &self.image_candidates
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
            let mut stream = StreamingRender::with_images(profile, self.image_opts.clone());
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
    /// this context. 折叠的思考块绕过它：流仍在累积（供展开 / finalize），
    /// 但可见行来自 cell 自己的渲染器（折叠行 + 标题）。展开与否由**块自己**
    /// 决定（`expanded` 覆盖 or `rendering.thinking` 的默认）——见
    /// `ThinkingBlock::is_expanded`。
    fn stream_render_active(&self, ctx: &CellContext<'_>) -> bool {
        if self.stream.is_none() {
            return false;
        }
        match &self.cell {
            ChatCell::Thinking(block) => block.is_expanded(ctx.thinking_mode),
            _ => true,
        }
    }

    /// 帧边界：推进这个思考块的刷光相位 / 显示秒数。
    ///
    /// 与 [`Self::tick_bash_timer`] 同一职责（帧驱动、显示值真的会动才作废）：
    /// 刷光相位每帧都在动，所以"仍在计时"即作废。返回是否推进了。
    pub fn tick_thinking(&mut self, now: std::time::Instant) -> bool {
        let ChatCell::Thinking(block) = &mut self.cell else {
            return false;
        };
        if !block.is_active() {
            return false;
        }
        block.tick(now);
        self.invalidate();
        true
    }

    /// 展开时把折叠行接到流式正文的头部（第 0 行 = 标题，正文拿续行缩进）。
    ///
    /// 每帧调用：秒数与刷光相位都在动，值不变时 `StreamingRender::set_header`
    /// 直接返回，不做无谓搬运。
    fn sync_stream_header(&mut self, width: u16, ctx: &CellContext<'_>) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let header = match &self.cell {
            ChatCell::Thinking(block) if block.is_labeled(ctx.thinking_mode) => {
                Some(block.label_line(ctx.palette, width))
            }
            _ => None,
        };
        stream.set_header(header);
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
    /// 折叠的思考块从不走流式渲染 —— 丢掉流、回到 cell 自己的渲染器（折叠行）；
    /// 正文文本留在 cell 里（展开时还要用）。
    fn run_finalize(&mut self, width: u16, ctx: &CellContext<'_>) {
        self.pending_finalize = false;
        let collapsed_thinking = matches!(&self.cell, ChatCell::Thinking(block) if !block.is_expanded(ctx.thinking_mode));
        if collapsed_thinking {
            self.stream = None;
            self.cached_lines = None;
            self.cached_height = None;
            self.prewrapped_width = None;
            return;
        }
        if let Some(mut stream) = self.stream.take() {
            stream.finalize(width, ctx.palette);
            let rendered = stream.composed(width, ctx.palette);
            let composed = ComposedLines::with_images(
                rendered.lines.to_vec(),
                rendered.links.to_vec(),
                rendered.images.to_vec(),
            );
            let height = composed.lines().len();
            self.cached_lines = Some(CachedLines {
                width,
                generation: self.generation,
                composed,
                // A finalized stream renders pre-wrapped lines (every line was
                // hard-wrapped to the width), so the row maths is exact.
                rows_exact: true,
                // …and it is rendered line by line (the blit path), so an
                // anchor's row *is* its line index — no wrapped-row walk.
                image_rows: Vec::new(),
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
        let frame = self.compute_cell_frame(width, ctx);
        CellLines {
            lines: frame.lines,
            links: frame.links,
            rows_exact: frame.rows_exact,
        }
    }

    /// [`compute_cell_lines`](Self::compute_cell_lines) plus the image anchors
    /// of every line.
    ///
    /// This is the projection the drawing path uses: `images[i]` locates the
    /// picture a line opens (see [`ImageSpan`]), and `rows_exact` says whether
    /// the row arithmetic its geometry rests on holds at this width.
    pub fn compute_cell_frame(&mut self, width: u16, ctx: &CellContext<'_>) -> CellFrame<'_> {
        // Frame boundary: settle deferred streaming work (tool-args parse)
        // before the caches are consulted — the generation check below must
        // see the invalidation a flush produces.
        self.flush_pending_args();
        // One set of image options for both render paths: adopt the frame's
        // before anything is cached or returned (see `sync_image_opts`).
        self.sync_image_opts(ctx.images);
        // 帧边界先把标题装好：流式路径与 finalize 都从这份状态出发
        // （同一帧内"展开 + 回合结束"也不能丢标题）。
        self.sync_stream_header(width, ctx);
        if self.pending_finalize {
            self.run_finalize(width, ctx);
        }
        if self.stream_render_active(ctx) {
            // Order matters: the flag is written before the borrow that
            // produces the returned slices is taken.
            self.prewrapped_width = Some(width);
            let stream = self.stream.as_mut().expect("checked active");
            let rendered = stream.composed(width, ctx.palette);
            self.image_candidates =
                image_candidates_of(rendered.links, rendered.images, ctx.images);
            // Streaming lines are hard-wrapped to the width by construction, and
            // blitted line by line: the line index *is* the screen row.
            return CellFrame {
                lines: rendered.lines,
                links: rendered.links,
                images: rendered.images,
                rows_exact: true,
                image_rows: &[],
            };
        }
        let cached_valid = self
            .cached_lines
            .as_ref()
            .is_some_and(|c| c.generation == self.generation && c.width == width);

        if !cached_valid {
            let composed = self.cell.render_lines(width, ctx);
            // The row arithmetic is what the link hit boxes and the image
            // anchors depend on: a line wider than the render area is wrapped
            // by `Paragraph` into two screen rows, and every link/anchor row
            // after it would land on the wrong text. Markdown pre-wraps prose,
            // but code blocks and indented code are exempt
            // (`wrap::is_prose_line`), so an over-wide code line in the same
            // cell really does break the mapping — measure and refuse rather
            // than point a click or a picture at the wrong rows.
            let rows_exact = !composed.has_spans() || composed.rows_are_exact(width);
            // The pictures' own row arithmetic: an over-wide line above an
            // anchor wraps into extra rows and pushes it down, and the widget's
            // `Paragraph` scroll counts rows — so the anchor's screen row is the
            // wrapped one, not its line index. Only the inexact case needs the
            // walk: when every line fits, the index *is* the row
            // (`CellFrame::image_row` falls back to it).
            let needs_row_walk =
                !rows_exact && composed.images().iter().any(|spans| !spans.is_empty());
            let image_rows = if needs_row_walk {
                image_rows_in_wrapped_rows(composed.lines(), composed.images(), width)
            } else {
                Vec::new()
            };
            // Discover the pictures this cell may want drawn while its lines
            // are being built (the harvest rides the same invalidation).
            self.image_candidates =
                image_candidates_of(composed.links(), composed.images(), ctx.images);
            self.cached_lines = Some(CachedLines {
                width,
                generation: self.generation,
                composed,
                rows_exact,
                image_rows,
            });
            // Lines from `to_lines` are not pre-wrapped — never blit them.
            self.prewrapped_width = None;
        }

        let cached = self.cached_lines.as_ref().expect("just ensured");
        CellFrame {
            lines: cached.composed.lines(),
            links: cached.composed.links(),
            images: cached.composed.images(),
            rows_exact: cached.rows_exact,
            image_rows: &cached.image_rows,
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
        // Heights are what `update_heights` sums into the layout, so they must
        // come from the same image options the frame will be drawn with —
        // an anchor's rows move a whole cell's height (see `sync_image_opts`).
        self.sync_image_opts(ctx.images);
        // 帧边界先把标题装好：流式路径与 finalize 都从这份状态出发
        // （同一帧内"展开 + 回合结束"也不能丢标题）。
        self.sync_stream_header(width, ctx);
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
    use crate::render::markdown::CellPixels;
    use crate::ui::cells::thinking::ThinkingBlock;

    /// The terminal cell the anchor fixtures are laid out for.
    const LAYOUT_CELL: CellPixels = CellPixels::new(10, 20);

    fn test_ctx<'a>(palette: &'a ThemePalette, layout: &'a LayoutConfig) -> CellContext<'a> {
        CellContext {
            palette,
            thinking_mode: ThinkingMode::Visible,
            layout,
            images: ImageOpts::off(),
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

    // ── Image anchors through the projection ──────────────────────

    /// The anchor-capable options fixture: one known 800×600 image at `/ws`.
    fn image_opts() -> ImageOpts {
        ImageOpts::anchor(
            Some(std::path::PathBuf::from("/ws")),
            vec![crate::render::markdown::ImageEntry::new(
                std::path::PathBuf::from("/ws/plot.png"),
                crate::render::markdown::ImageShape::new(800, 600),
            )],
            LAYOUT_CELL,
        )
    }

    /// A context carrying explicit image options — the frame's authority (see
    /// [`CachedCell::sync_image_opts`]).
    fn image_ctx<'a>(
        palette: &'a ThemePalette,
        layout: &'a LayoutConfig,
        images: &'a ImageOpts,
    ) -> CellContext<'a> {
        CellContext {
            images,
            ..test_ctx(palette, layout)
        }
    }

    #[test]
    fn a_cell_without_image_opts_projects_no_anchors() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::AssistantMessage("![plot](./plot.png)".into()));
        let frame = cell.compute_cell_frame(80, &ctx);
        assert!(!frame.has_images());
        assert!(frame.images.iter().all(Vec::is_empty));
        // …and the cell still renders the alt text through the link path.
        let text = frame
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<String>();
        assert!(text.contains("plot"), "{text:?}");
        assert!(!text.contains('▢'), "{text:?}");
    }

    /// The finalized-stream path is the *cached* projection (ComposedLines →
    /// CellFrame), i.e. the same code path a non-streaming cell uses.
    #[test]
    fn the_cell_frame_projects_the_anchor_geometry() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let opts = image_opts();
        let ctx = image_ctx(&palette, &layout, &opts);
        let mut cell = CachedCell::new(ChatCell::AssistantMessage(String::new()));
        cell.append_stream("before\n\n![销售趋势](./plot.png)\n\nafter");
        cell.request_finalize();

        let width = 80u16;
        let expected_rows = crate::render::markdown::anchor_rows(
            width - 2,
            crate::render::markdown::ImageShape::new(800, 600),
            LAYOUT_CELL,
        );
        assert!(
            cell.is_streaming(),
            "finalize is deferred to the next render"
        );
        let (anchor, line_count) = {
            let frame = cell.compute_cell_frame(width, &ctx);
            assert!(frame.has_images(), "no anchor in the finalized frame");
            assert!(frame.rows_exact, "the fixture has no over-wide lines");
            let anchors = frame.images.iter().flatten().collect::<Vec<_>>();
            assert_eq!(anchors.len(), 1);
            let anchor = anchors[0].clone();
            // The caption is the anchor's first row.
            assert!(frame.lines[anchor.line].to_string().contains('▢'));
            (anchor, frame.lines.len())
        };
        assert_eq!(anchor.column, 2, "the cell prefix shifts the box");
        assert_eq!(anchor.cols, width - 2);
        assert_eq!(anchor.rows, expected_rows);
        assert_eq!(anchor.path, std::path::PathBuf::from("/ws/plot.png"));
        assert_eq!(anchor.alt, "销售趋势");
        assert_eq!(
            anchor.rows_range().end - anchor.line,
            usize::from(expected_rows)
        );
        // The height counts the whole block (the cache must agree with what
        // the drawing layer is told).
        assert_eq!(cell.compute_height(width, &ctx), line_count);
        // The cached lines are reused: a second frame is identical.
        let again = cell.compute_cell_frame(width, &ctx);
        assert_eq!(again.images.iter().flatten().next(), Some(&anchor));
    }

    /// Late metadata (a header probe finishing) arrives as new frame options:
    /// the cell adopts them, drops the row count that was computed without
    /// them, and keeps its text.
    #[test]
    fn late_metadata_invalidates_the_cell_but_keeps_the_text() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let off = test_ctx(&palette, &layout);
        let opts = image_opts();
        let with_images = image_ctx(&palette, &layout, &opts);
        let mut cell = CachedCell::new(ChatCell::AssistantMessage(String::new()));
        cell.append_stream("![plot](./plot.png)");

        let before = cell.compute_height(80, &off);
        let generation = cell.generation();
        let after = cell.compute_height(80, &with_images);
        assert!(
            cell.generation() > generation,
            "the metadata change must invalidate the cached lines/height"
        );
        assert!(
            after > before,
            "the anchor must reserve rows: {after} vs {before}"
        );
        assert!(cell.compute_cell_frame(80, &with_images).has_images());
        assert!(
            cell.image_opts().is_enabled(),
            "the engine adopted the frame's options"
        );
        // The cell's text is untouched by the metadata change.
        match cell.cell() {
            ChatCell::AssistantMessage(text) => assert_eq!(text, "![plot](./plot.png)"),
            other => panic!("unexpected cell: {other:?}"),
        }
        // Re-projecting at the same options is a no-op: no generation bump.
        let generation = cell.generation();
        let _ = cell.compute_height(80, &with_images);
        assert_eq!(cell.generation(), generation);
        // …and back to no metadata is a *change* again (the render layer falls
        // back to the link path, so the rows must shrink).
        assert_eq!(cell.compute_height(80, &off), before);
    }

    #[test]
    fn a_streaming_cell_projects_anchors_from_the_engine() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let opts = image_opts();
        let ctx = image_ctx(&palette, &layout, &opts);
        let mut cell = CachedCell::new(ChatCell::AssistantMessage(String::new()));
        cell.append_stream("before\n\n![plot](./plot.png)\n\nafter");
        assert!(cell.is_streaming());
        let frame = cell.compute_cell_frame(80, &ctx);
        let anchors = frame.images.iter().flatten().collect::<Vec<_>>();
        assert_eq!(anchors.len(), 1);
        assert_eq!(
            anchors[0].line,
            frame
                .lines
                .iter()
                .position(|l| l.to_string().contains('▢'))
                .unwrap()
        );
        // Streaming lines are pre-wrapped, so their row arithmetic is exact.
        assert!(frame.rows_exact);
    }

    /// The streaming engine and the reference render must agree about an
    /// image's rows — they get their options from **one** source (the frame's
    /// `CellContext`), which is the whole point of `sync_image_opts`.
    #[test]
    fn a_streaming_and_a_non_streaming_cell_agree_about_the_same_image() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let opts = image_opts();
        let ctx = image_ctx(&palette, &layout, &opts);
        let text = "before\n\n![plot](./plot.png)\n\nafter";

        let mut streaming = CachedCell::new(ChatCell::AssistantMessage(String::new()));
        streaming.append_stream(text);
        assert!(streaming.is_streaming());
        let mut plain = CachedCell::new(ChatCell::AssistantMessage(text.into()));

        let width = 80u16;
        let (streamed_anchor, streamed_lines) = {
            let frame = streaming.compute_cell_frame(width, &ctx);
            (
                frame.images.iter().flatten().next().cloned(),
                frame.lines.len(),
            )
        };
        let (plain_anchor, plain_lines) = {
            let frame = plain.compute_cell_frame(width, &ctx);
            (
                frame.images.iter().flatten().next().cloned(),
                frame.lines.len(),
            )
        };
        assert_eq!(streamed_anchor, plain_anchor, "anchor geometry");
        assert_eq!(streamed_lines, plain_lines, "line counts");
        assert_eq!(
            streaming.compute_height(width, &ctx),
            plain.compute_height(width, &ctx),
            "the heights the layout sums must agree"
        );
        // Both discovered the same candidate path for the app to probe.
        assert_eq!(
            streaming.image_candidates(),
            vec![std::path::PathBuf::from("/ws/plot.png")]
        );
        assert_eq!(plain.image_candidates(), streaming.image_candidates());
    }

    /// Candidates are the *discovery* channel: a cell whose image is anchored
    /// keeps reporting the path (its link span was replaced by the caption),
    /// and an image on the link path reports it too.
    #[test]
    fn image_candidates_survive_the_anchor_and_ignore_remote_links() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let opts = image_opts();
        let ctx = image_ctx(&palette, &layout, &opts);

        let mut anchored = CachedCell::new(ChatCell::AssistantMessage(
            "![plot](./plot.png)\n\n[remote](https://example.com/x.png) [doc](notes.txt)\n".into(),
        ));
        let _ = anchored.compute_cell_frame(80, &ctx);
        assert_eq!(
            anchored.image_candidates(),
            vec![std::path::PathBuf::from("/ws/plot.png")],
            "the anchored path stays a candidate; remote/non-image links never are"
        );

        // With images off nothing is discovered, and the same markdown renders
        // exactly like it did before anchors existed.
        let off = test_ctx(&palette, &layout);
        let mut plain = CachedCell::new(ChatCell::AssistantMessage(
            "![plot](./plot.png)\n\n[remote](https://example.com/x.png) [doc](notes.txt)\n".into(),
        ));
        let has_images = plain.compute_cell_frame(80, &off).has_images();
        assert!(!has_images);
        assert!(plain.image_candidates().is_empty());
    }

    #[test]
    fn an_over_wide_line_makes_the_row_arithmetic_inexact() {
        // The gate the drawing layer reads: a line the markdown pre-wrap
        // cannot fix (code, borders) shifts every row below it, so the cell
        // refuses the row arithmetic for links and anchors alike.
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::AssistantMessage(format!(
            "```\n{}\n```\n\n[plot](./plot.png)",
            "a".repeat(80)
        )));
        let frame = cell.compute_cell_frame(40, &ctx);
        assert!(frame.has_links());
        assert!(!frame.rows_exact, "an over-wide code line must refuse");
        // An anchor-bearing line set goes through the same gate
        // (`ComposedLines::has_spans`), see the `links` module tests.
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

    /// 折叠的思考块：标签行来自 cell 自己的渲染器，tick 推进秒数并作废缓存。
    #[test]
    fn collapsed_thinking_label_ticks_and_invalidates() {
        use std::time::Duration;
        use std::time::Instant;

        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = CellContext {
            thinking_mode: ThinkingMode::Hidden,
            ..test_ctx(&palette, &layout)
        };
        let frame_text = |cell: &mut CachedCell| -> String {
            cell.compute_cell_frame(80, &ctx)
                .lines
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let start = Instant::now();
        let mut block = ThinkingBlock::new();
        block.start(start);
        let mut cell = CachedCell::new(ChatCell::Thinking(block));
        cell.append_stream("SECRET-REASONING");

        let text = frame_text(&mut cell);
        assert!(text.contains("深度思考中"), "{text}");
        assert!(!text.contains("SECRET-REASONING"), "折叠不泄露正文：{text}");

        let generation = cell.generation();
        assert!(
            cell.tick_thinking(start + Duration::from_secs(4)),
            "活跃块应被推进"
        );
        assert_ne!(cell.generation(), generation, "tick 必须作废缓存");
        let text = frame_text(&mut cell);
        assert!(text.contains("深度思考中 4s"), "{text}");

        // 冻结后 tick 不再推进（帧驱动随即 park）。
        cell.mutate(|c| {
            if let ChatCell::Thinking(block) = c {
                block.finish(start + Duration::from_secs(9));
            }
        });
        assert!(!cell.tick_thinking(start + Duration::from_secs(20)));
        let text = frame_text(&mut cell);
        assert!(text.contains("深度思考 9s"), "{text}");
    }

    /// 展开的思考块：折叠行接到流式正文的头部（第 0 行 = 标题，正文续行缩进）。
    #[test]
    fn expanded_thinking_keeps_the_label_as_stream_header() {
        use std::time::Duration;
        use std::time::Instant;

        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = CellContext {
            thinking_mode: ThinkingMode::Hidden,
            ..test_ctx(&palette, &layout)
        };

        let start = Instant::now();
        let mut block = ThinkingBlock::new();
        block.start(start);
        block.set_expanded(Some(true));
        let mut cell = CachedCell::new(ChatCell::Thinking(block));
        cell.append_stream("the reasoning body");

        let frame = cell.compute_cell_frame(80, &ctx);
        let lines: Vec<String> = frame.lines.iter().map(|l| l.to_string()).collect();
        assert!(
            lines[0].contains("深度思考中"),
            "标题应是第 0 行：{lines:?}"
        );
        let body = lines
            .iter()
            .find(|l| l.contains("the reasoning body"))
            .expect("正文应在行集里");
        assert!(body.starts_with("  "), "正文应保持两列缩进：{body:?}");
        assert!(
            !lines[0].contains("the reasoning body"),
            "正文不该挤进标题行：{lines:?}"
        );

        // 秒数每帧在动：同一个 cell 第二帧的标题已更新（流上换行，不是新行）。
        cell.mutate(|c| {
            if let ChatCell::Thinking(block) = c {
                block.tick(start + Duration::from_secs(3));
            }
        });
        let frame = cell.compute_cell_frame(80, &ctx);
        let first = frame.lines[0].to_string();
        assert!(first.contains("深度思考中 3s"), "{first:?}");
    }
}
