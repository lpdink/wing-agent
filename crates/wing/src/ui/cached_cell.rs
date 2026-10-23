//! CachedCell — ChatCell wrapper with generation-based caching.
//!
//! Caches both rendered lines and width-aware height to avoid redundant `render_markdown()` calls
//! during rendering.
//!
//! Streaming cells (Thinking / AssistantMessage while a turn is active) carry a [`StreamingRender`]
//! instead: deltas append through `append_stream` without invalidating the generation cache — only
//! the active tail re-renders each sync, and heights come from the flat line count (O(1)). At turn
//! end the stream is reconciled (`request_finalize` → the next render installs the full reference
//! render as the cached lines, flagged `prewrapped`).

use std::path::PathBuf;

use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

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
    ///
    /// Only the streaming renderer (and the turn-end reconcile) writes it. A
    /// frozen cell does not need it: whether *its* lines can be blitted is read
    /// off the line set itself (`CachedLines::fits`) — a cell may wrap inside
    /// its own `to_lines` (DiffView hangs over-wide rows under the gutter)
    /// without being uniformly pre-wrapped.
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
    /// Bumped whenever [`Self::image_candidates`] is rebuilt. The view folds
    /// these into one signature to decide whether its concatenated buffer is
    /// stale — a line rebuild happens inside the cell, so the view cannot know
    /// about it by itself.
    ///
    /// The fold (`ChatView::image_candidates`) relies on `epoch == 0` meaning
    /// "never projected, so the list is empty": a removed cell whose epoch is
    /// still 0 therefore leaves nothing behind in the view's buffer.
    candidates_epoch: u64,
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
    /// line sets without links (nothing to place) — which is why it is not the
    /// blit gate. For a line set that carries a side channel it is exactly
    /// [`Self::fits`].
    rows_exact: bool,
    /// Wrapped screen row of each anchored line, for cells whose lines do not
    /// map one row each — the picture placement's own arithmetic, which is
    /// per-anchor rather than per-cell (see [`image_rows_in_wrapped_rows`]).
    /// Empty when the line index *is* the row index.
    image_rows: Vec<(usize, usize)>,
    /// Screen rows the line set occupies at the width it was built for — the
    /// cell's height before the user-bubble padding (see `compute_height`).
    /// Cached because the height is asked for every cell every frame, while the
    /// line set only changes when the caches are invalidated.
    ///
    /// Every line contributes at least one row, so "every line fits" implies
    /// `rows == the line count` — but not the other way round (see [`Self::fits`]).
    rows: usize,
    /// Whether every line fits the width its renderer wraps at — the blit
    /// path's precondition (see [`CachedCell::is_prewrapped`]).
    ///
    /// Measured at `content_width` (the cell width minus a user bubble's text
    /// inset; see where it is computed), while the `width` above is the cell's
    /// box. The two differ only for bubbles, and a bubble never reads this:
    /// `is_prewrapped` short-circuits on the bubble first.
    ///
    /// **Not** derivable from `rows == composed.lines().len()`: the wrapper's
    /// break test is a column accumulation, so a line that is over-wide by
    /// exactly the second half of a wide grapheme (`"a" * 79 + "你"` at width
    /// 80) still counts as one row. Such a line must not be blitted — `Line`
    /// skips a grapheme that does not fit, `Paragraph` writes it into the last
    /// cell. See `one_row_does_not_imply_the_line_fits`.
    ///
    /// At `content_width == 0` the width test is false for any non-empty line
    /// (`rows_are_exact`), which is the conservative direction; the render path
    /// never asks.
    fits: bool,
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
/// A line that fits is one row — which is what makes [`row_count`] (the sum)
/// equal the line count whenever every line fits; the converse does not hold
/// (see `CachedLines::fits`). An over-wide one is measured by `Paragraph` itself,
/// so the row index the picture is placed at — and the cell's height — are the
/// rows the line is really rendered at. No re-implementation of the wrapper, and
/// no cross-line state to reproduce (`WordWrapper` resets per input line). The
/// clone is paid per over-wide line, at cache-build time only.
fn wrapped_rows(line: &Line<'static>, width: u16) -> usize {
    if width == 0 || line.width() <= usize::from(width) {
        return 1;
    }
    Paragraph::new(vec![line.clone()])
        .wrap(Wrap { trim: false })
        .line_count(width)
}

/// Screen rows a whole line set occupies at `width`.
///
/// The sum of the per-line measure below: `WordWrapper` resets its state per
/// input line, so the composer's total is exactly the sum of the lines' own
/// row counts (`cached_rows_match_the_paragraph_line_count` pins it against
/// `Paragraph::line_count`).
///
/// A zero width is degenerate and unreachable from the render path (a band of
/// width 0 never lays cells out); it answers 0 because `WordWrapper` yields no
/// line at all in that case — the same thing `Paragraph::line_count(0)` says.
fn row_count(lines: &[Line<'static>], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    lines.iter().map(|line| wrapped_rows(line, width)).sum()
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
        // 媒体锚点不参与头信息探测：形状由 `MediaRef` 自带（见 `ui::chat_view::media`）。
        if let Some(path) = span.source.as_file() {
            push(path.to_path_buf());
        }
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
            candidates_epoch: 0,
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

    /// See [`Self::candidates_epoch`].
    pub(crate) fn candidates_epoch(&self) -> u64 {
        self.candidates_epoch
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
    ///
    /// 也供 `ChatView` 用：Ctrl+O 翻转思考的呈现时，所有 Thinking cell 的
    /// 行 / 高度缓存都要作废（presentation 不在缓存键里，见 `CellContext`）。
    pub(crate) fn invalidate(&mut self) {
        self.generation += 1;
        // The lines are rebuilt by `to_lines` — the generic path, which is
        // not the blit path's uniformly pre-wrapped output (a cell may wrap
        // inside it, e.g. DiffView): drop the flag here rather than relying
        // on `update_heights` having refreshed it earlier in the same frame.
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
    pub fn flush_pending_args(&mut self, syntax: crate::config::SyntaxTheme) -> bool {
        let parsed = match &mut self.cell {
            ChatCell::ToolCall(block) => block.flush_pending_args(syntax),
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
    /// 但可见行来自 cell 自己的渲染器（标题行）。展开与否由帧上的
    /// `thinking_mode` + Ctrl+O 覆盖解析（见 `ThinkingMode::expanded`），
    /// 不落在块上。
    fn stream_render_active(&self, ctx: &CellContext<'_>) -> bool {
        if self.stream.is_none() {
            return false;
        }
        match &self.cell {
            ChatCell::Thinking(_) => ctx.thinking_mode.expanded(ctx.thinking_expanded),
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

    /// 把标题行接到流式正文的头部（第 0 行 = 标题，正文拿续行缩进）。
    ///
    /// 每帧调用：秒数与刷光相位都在动，值不变时 `StreamingRender::set_header`
    /// 直接返回，不做无谓搬运。所有 Thinking 块都有标题行；折叠时整条流
    /// 不显示（[`Self::stream_render_active`]），标题照设 —— 展开 / 折叠
    /// 切换时行集已经就位。
    fn sync_stream_header(&mut self, width: u16, ctx: &CellContext<'_>) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let header = match &self.cell {
            ChatCell::Thinking(block) => {
                Some(block.label_line(ctx.palette, width, ctx.thinking_mode, ctx.thinking_expanded))
            }
            _ => None,
        };
        stream.set_header(header);
    }

    /// Whether the cell's lines at `width` are pre-wrapped (≤ width) —
    /// the render loop blits these directly instead of going through
    /// `Paragraph`. Width- and generation-aware: a resize or a re-render
    /// through `to_lines` invalidates it.
    ///
    /// 冻结 cell 走同一条快路的条件是「每行都装得下」（`CachedLines::fits`）：
    /// 那时行号就是屏幕行号，逐行 blit 与 `Paragraph` 的 wrap + scroll 逐格等价
    /// （`ui::chat_view::viewport` 的 `a_fitting_line_set_blits_identically_to_a_paragraph`）。
    ///
    /// 用户气泡**不**走这条快路，尽管它的行也可能装得下：气泡先整块填背景再画文本，
    /// 而 blit 用的 `Line::render` 会把宽字素的隐藏格 `reset()`（`Cell::reset` = `EMPTY`，
    /// 连背景一起清掉），`Paragraph` 的 `render_line` 只写首格、隐藏格保留预填背景。
    /// CJK 文本下两者逐格不同。
    pub fn is_prewrapped(&self, width: u16, ctx: &CellContext<'_>) -> bool {
        if self.stream_render_active(ctx) {
            return true;
        }
        if self.prewrapped_width == Some(width) {
            return true;
        }
        !self.cell.is_user_bubble() && self.lines_fit(width)
    }

    /// 缓存的行集在 `width` 下每行都装得下吗（从而可以逐行 blit）。
    fn lines_fit(&self, width: u16) -> bool {
        self.cached_lines
            .as_ref()
            .is_some_and(|c| c.width == width && c.generation == self.generation && c.fits)
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
    /// 折叠的思考块从不走流式渲染 —— 丢掉流、回到 cell 自己的渲染器（标题行）；
    /// 正文文本留在 cell 里（展开时还要用）。
    fn run_finalize(&mut self, width: u16, ctx: &CellContext<'_>) {
        self.pending_finalize = false;
        let collapsed_thinking = matches!(&self.cell, ChatCell::Thinking(_))
            && !ctx.thinking_mode.expanded(ctx.thinking_expanded);
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
            // The stream hard-wraps every line to `width`, so this is normally
            // true; it is measured rather than asserted, because a width that
            // cannot hold one grapheme (1 column) yields wider lines, and a
            // lying flag would be a trap for the next reader.
            let fits = composed.rows_are_exact(width);
            self.cached_lines = Some(CachedLines {
                width,
                generation: self.generation,
                composed,
                // A finalized stream renders one line per row (see above).
                rows_exact: true,
                rows: height,
                fits,
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
        self.flush_pending_args(ctx.palette.syntax);
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
            self.candidates_epoch += 1;
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
            // `width` is the cell's box; a user message renders its text in an inset area
            // (2 left, 1 right), so the row walk and the "does every line fit" test below
            // must measure against the width its lines actually wrap at.
            let content_width = if self.cell.is_user_bubble() {
                width.saturating_sub(crate::ui::chat_view::media::TEXT_WIDTH_SUB)
            } else {
                width
            };
            // The row arithmetic is what the link hit boxes and the image
            // anchors depend on: a line wider than the render area is wrapped
            // by `Paragraph` into two screen rows, and every link/anchor row
            // after it would land on the wrong text. Markdown pre-wraps prose,
            // but code blocks and indented code are exempt
            // (`wrap::is_prose_line`), so an over-wide code line in the same
            // cell really does break the mapping — measure and refuse rather
            // than point a click or a picture at the wrong rows.
            let fits = composed.rows_are_exact(content_width);
            let rows_exact = !composed.has_spans() || fits;
            let rows = row_count(composed.lines(), content_width);
            // The pictures' own row arithmetic: an over-wide line above an
            // anchor wraps into extra rows and pushes it down, and the widget's
            // `Paragraph` scroll counts rows — so the anchor's screen row is the
            // wrapped one, not its line index. Only the inexact case needs the
            // walk: when every line fits, the index *is* the row
            // (`CellFrame::image_row` falls back to it).
            let needs_row_walk =
                !rows_exact && composed.images().iter().any(|spans| !spans.is_empty());
            let image_rows = if needs_row_walk {
                image_rows_in_wrapped_rows(composed.lines(), composed.images(), content_width)
            } else {
                Vec::new()
            };
            // Discover the pictures this cell may want drawn while its lines
            // are being built (the harvest rides the same invalidation).
            self.image_candidates =
                image_candidates_of(composed.links(), composed.images(), ctx.images);
            self.candidates_epoch += 1;
            self.cached_lines = Some(CachedLines {
                width,
                generation: self.generation,
                composed,
                rows_exact,
                image_rows,
                rows,
                fits,
            });
            // `prewrapped_width` is the streaming engine's flag, and this line
            // set came from the cell's own `to_lines`: drop it. Whether *these*
            // lines may be blitted is `CachedLines::fits`, decided per line set.
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
    /// The height is the cached line set's own row count ([`CachedLines::rows`]) —
    /// never a second markdown render, and never a copy of the lines.
    pub fn compute_height(&mut self, width: u16, ctx: &CellContext<'_>) -> usize {
        // Frame boundary — see `compute_cell_lines`. Heights for a
        // streaming tool cell depend on the parsed args (command line,
        // preview lines), so the flush must run before the cache check.
        self.flush_pending_args(ctx.palette.syntax);
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
        // The line set is built (and its row count measured) here at most once;
        // the height then reads that number instead of re-measuring the text.
        if !self
            .cached_lines
            .as_ref()
            .is_some_and(|c| c.width == width && c.generation == self.generation)
        {
            self.compute_cell_frame(width, ctx);
        }
        // Take the cache out to read it: the borrow it hands out would collide
        // with the `cached_height` assignment below (the old shape cloned the
        // whole line set to release it).
        let cached = self
            .cached_lines
            .take()
            .expect("compute_cell_frame just built the lines");
        // A user bubble renders its text in the inset area (the row count was
        // measured at that narrower width) plus one padding row above and below.
        let height = if self.cell.is_user_bubble() {
            cached.rows + 2
        } else {
            cached.rows
        };
        self.cached_lines = Some(cached);
        self.cached_height = Some(CachedHeight {
            width,
            generation: self.generation,
            height,
        });
        height
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

    /// `rows == 行数` **不**等价于「每行都装得下」：包装器的换行判据是列累加，
    /// 一行比宽度多出的那一列若属于最后一个宽字素的第二半，它不折行 —— 于是这行算
    /// 一行（`rows`）却比宽度宽。
    ///
    /// 本用例只钉上游度量（同一行上 `wrapped_rows` 与 `rows_are_exact` 分歧），不碰
    /// 生产代码；字段那一侧由 `a_line_over_wide_by_half_a_wide_grapheme_is_never_blittable`
    /// 钉住。
    #[test]
    fn one_row_does_not_imply_the_line_fits() {
        let line = Line::from(format!("{}{}", "a".repeat(79), "你"));
        assert_eq!(line.width(), 81);
        assert_eq!(wrapped_rows(&line, 80), 1, "包装器不折这一行");
        assert!(
            !ComposedLines::plain(vec![line]).rows_are_exact(80),
            "但它确实装不下"
        );
    }

    /// 宽一列、且那一列属于最后一个宽字素的第二半：包装器**不折**这一行，于是
    /// 「行数 == 行数之和」成立，而这一行**装不下**。`fits` 因此不能由 `rows` 推
    /// 出来 —— 反例若被 blit，最后一列会少画一个字素（`Line` 整字素跳过，`Paragraph`
    /// 写进最后一格）。
    ///
    /// 这条钉的是**字段本身**（`fits` 为假 + 前提成立）；只钉上游度量的那条是
    /// `one_row_does_not_imply_the_line_fits`。
    #[test]
    fn a_line_over_wide_by_half_a_wide_grapheme_is_never_blittable() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        // 代码行不折行；代码块的 2 列前缀 + 边框让这一行 = 81 列，比 80 宽一列。
        let mut cell = CachedCell::new(ChatCell::AssistantMessage(format!(
            "```\n{}你\n```",
            "a".repeat(75)
        )));
        let _ = cell.compute_cell_frame(80, &ctx);
        let cached = cell.cached_lines.as_ref().expect("just built");
        assert!(
            cached.composed.lines().iter().any(|line| line.width() > 80),
            "前提：有一行超出 80 列"
        );
        assert_eq!(
            cached.rows,
            cached.composed.lines().len(),
            "前提：这一行仍只算一行（所以「行数 == 行数之和」不能当作判据）"
        );
        assert!(!cached.fits, "比宽度宽的行集不许被宣告为「装得下」");
        assert!(!cell.is_prewrapped(80, &ctx), "因此不许走 blit 快路");
    }

    /// 超宽行（代码行不折行）让整个 cell 留在 `Paragraph` 路径上。
    #[test]
    fn a_wider_than_band_line_keeps_the_cell_on_the_paragraph_path() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::AssistantMessage(
            "```\nlet x = \"一个很长很长很长很长很长很长的字符串\";\n```".to_string(),
        ));
        let _ = cell.compute_cell_frame(20, &ctx);
        assert!(
            !cell.is_prewrapped(20, &ctx),
            "代码行装不下 20 列时不许走 blit 快路"
        );
    }

    /// 负控：**预填了背景**之后两种画法逐格不同 —— 这就是气泡被排除的机制本身。
    ///
    /// 它锁的是两个上游原语的分歧（本用例不碰生产代码）；生产侧的排除由
    /// `a_user_bubble_never_takes_the_blit_path` 钉住。
    #[test]
    fn a_prefilled_background_separates_the_two_primitives() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::style::Color;
        use ratatui::style::Style;
        use ratatui::widgets::Widget;

        let area = Rect::new(0, 0, 12, 1);
        let lines = vec![Line::from("你好世界")];
        let background = Style::default().bg(Color::Red);
        let prefill = |buf: &mut Buffer| {
            for x in area.x..area.right() {
                buf[(x, area.y)].set_style(background);
            }
        };

        let mut paragraph = Buffer::empty(area);
        prefill(&mut paragraph);
        Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .render(area, &mut paragraph);

        let mut blitted = Buffer::empty(area);
        prefill(&mut blitted);
        for (row, line) in lines.iter().enumerate() {
            line.render(
                Rect::new(area.x, area.y + row as u16, area.width, 1),
                &mut blitted,
            );
        }

        assert_eq!(
            paragraph[(1, area.y)].bg,
            Color::Red,
            "Paragraph 保留隐藏格的背景"
        );
        assert_eq!(
            blitted[(1, area.y)].bg,
            Color::Reset,
            "blit 把宽字素的隐藏格 reset 掉了"
        );
    }

    /// 等号那一侧：最宽行恰好等于 cell 宽度时仍算「装得下」（`<=`），快路照走；
    /// 少一列就不算。夹具用代码块（它的行不折行，宽度可预测）。
    #[test]
    fn a_line_exactly_as_wide_as_the_box_still_blits() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let text = "```\nfn main() {}\n```".to_string();

        let mut probe = CachedCell::new(ChatCell::AssistantMessage(text.clone()));
        let widest = probe
            .compute_cell_frame(60, &ctx)
            .lines
            .iter()
            .map(Line::width)
            .max()
            .expect("the fixture renders at least one line");

        for (width, expected) in [(widest, true), (widest - 1, false)] {
            let mut cell = CachedCell::new(ChatCell::AssistantMessage(text.clone()));
            let _ = cell.compute_cell_frame(width as u16, &ctx);
            assert_eq!(
                cell.is_prewrapped(width as u16, &ctx),
                expected,
                "width = {width}（最宽行 {widest} 列）"
            );
        }
    }

    /// 缓存的 `rows` 必须与 `Paragraph::line_count` 逐宽度相等 —— 高度现在读它，
    /// 而布局的另一半（图片锚点的行号）仍由 `Paragraph` 的度量给出。
    ///
    /// 语料覆盖空行 / 超宽行 / CJK / 尾随空行 / 零宽。
    #[test]
    fn cached_rows_match_the_paragraph_line_count() {
        use ratatui::style::Style;
        use ratatui::text::Span;

        let lines: Vec<Line<'static>> = vec![
            Line::from(""),
            Line::from("fits easily"),
            Line::from("x".repeat(40)),
            Line::from("中文宽度按两列算的一行"),
            Line::from(vec![
                Span::styled("styled", Style::default()),
                Span::raw(" tail"),
            ]),
            Line::from(""),
            Line::from("y".repeat(7)),
        ];
        for width in 0..=48u16 {
            let want = Paragraph::new(lines.clone())
                .wrap(Wrap { trim: false })
                .line_count(width);
            assert_eq!(row_count(&lines, width), want, "width = {width}");
        }
    }

    /// 用户气泡即使每行都装得下也不走 blit 快路（理由见 [`CachedCell::is_prewrapped`]）。
    /// 同样的行集长在普通 cell 上则走快路 —— 两条一起钉住「排除的是气泡，不是短行」。
    #[test]
    fn a_user_bubble_never_takes_the_blit_path() {
        let (_p, l) = (ThemePalette::default(), LayoutConfig::default());
        let palette = ThemePalette::default();
        let ctx = test_ctx(&palette, &l);

        let mut bubble = CachedCell::new(ChatCell::UserMessage("short".into()));
        let _ = bubble.compute_cell_frame(40, &ctx);
        assert!(!bubble.is_prewrapped(40, &ctx));

        let mut plain = CachedCell::new(ChatCell::AssistantMessage("short".into()));
        let _ = plain.compute_cell_frame(40, &ctx);
        assert!(plain.is_prewrapped(40, &ctx));
    }

    fn test_ctx<'a>(palette: &'a ThemePalette, layout: &'a LayoutConfig) -> CellContext<'a> {
        CellContext {
            palette,
            thinking_mode: ThinkingMode::Visible,
            thinking_expanded: None,
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
        assert_eq!(
            anchor.source,
            crate::render::picture::PictureSource::File(std::path::PathBuf::from("/ws/plot.png"))
        );
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

    // ── User-message pictures (media anchors) ──────────────────────

    fn media_picture() -> crate::protocol::MediaRef {
        crate::protocol::MediaRef {
            id: "a".repeat(64),
            mime: "image/jpeg".into(),
            bytes: 4096,
            width: 800,
            height: 600,
            name: None,
        }
    }

    fn user_body(text: &str, media: Vec<crate::protocol::MediaRef>) -> ChatCell {
        ChatCell::UserMessage(crate::ui::chat_view::media::UserMessageBody::new(
            text, media,
        ))
    }

    #[test]
    fn a_user_picture_projects_its_block_geometry() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let opts = ImageOpts::anchor(None, Vec::new(), LAYOUT_CELL);
        let ctx = image_ctx(&palette, &layout, &opts);
        let mut cell = CachedCell::new(user_body("看这张", vec![media_picture()]));

        let width = 80u16;
        let text_width = width - 3;
        let expected_rows = crate::render::markdown::anchor_rows(
            text_width,
            crate::render::markdown::ImageShape::new(800, 600),
            LAYOUT_CELL,
        );
        let (span, lines) = {
            let frame = cell.compute_cell_frame(width, &ctx);
            assert!(frame.has_images());
            let spans = frame.images.iter().flatten().collect::<Vec<_>>();
            assert_eq!(spans.len(), 1);
            let span = spans[0].clone();
            assert_eq!(frame.lines[span.line].to_string(), "▢ Image #1 · 800×600");
            assert!(
                matches!(span.source, crate::render::picture::PictureSource::Media(_)),
                "user pictures are media objects"
            );
            (span, frame.lines.len())
        };
        assert_eq!(span.column, 2, "the bubble's left inset");
        assert_eq!(span.cols, text_width, "the bubble's text width");
        assert_eq!(span.rows, expected_rows);
        assert_eq!(lines, 1 + usize::from(expected_rows), "正文 + 图块");
        assert_eq!(
            cell.compute_height(width, &ctx),
            lines + 2,
            "气泡的上下各一行内边距"
        );

        // With pictures off the same cell is the label form (no anchors, two lines).
        let off = test_ctx(&palette, &layout);
        let frame = cell.compute_cell_frame(width, &off);
        assert!(!frame.has_images());
        assert_eq!(frame.lines.len(), 2);
        assert_eq!(frame.lines[1].to_string(), "[Image #1 800×600 4KB]");
    }

    /// A wrapped line above the anchor pushes it down by the extra rows — measured at the
    /// bubble's **text** width, not the cell's.
    #[test]
    fn an_over_wide_user_line_moves_the_picture_to_its_wrapped_row() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let opts = ImageOpts::anchor(None, Vec::new(), LAYOUT_CELL);
        let ctx = image_ctx(&palette, &layout, &opts);
        let text = format!("{}\n\n", "字".repeat(60));
        let mut cell = CachedCell::new(user_body(&text, vec![media_picture()]));

        let width = 40u16;
        let (span_line, span_row, rows_exact) = {
            let frame = cell.compute_cell_frame(width, &ctx);
            let span = frame
                .images
                .iter()
                .flatten()
                .next()
                .cloned()
                .expect("anchor");
            (span.line, frame.image_row(span.line), frame.rows_exact)
        };
        assert!(!rows_exact, "60 CJK columns do not fit 37 columns of text");
        assert!(
            span_row > span_line,
            "the wrapped line above pushes the picture down: row {span_row} vs line {span_line}"
        );
    }

    #[test]
    fn test_cached_cell_width_change() {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout);
        let mut cell = CachedCell::new(ChatCell::UserMessage("a ".repeat(100).into()));
        let h_narrow = cell.compute_height(40, &ctx);
        let h_wide = cell.compute_height(120, &ctx);
        assert!(h_narrow >= h_wide);
    }

    /// 折叠的思考块：标题行来自 cell 自己的渲染器，tick 推进秒数并作废缓存。
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

    /// 展开的思考块：标题行接到流式正文的头部（第 0 行 = 标题，正文续行缩进）。
    #[test]
    fn expanded_thinking_keeps_the_label_as_stream_header() {
        use std::time::Duration;
        use std::time::Instant;

        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        // Ctrl+O 的展开覆盖（全局）在帧上经 ctx 下发。
        let ctx = CellContext {
            thinking_mode: ThinkingMode::Hidden,
            thinking_expanded: Some(true),
            ..test_ctx(&palette, &layout)
        };

        let start = Instant::now();
        let mut block = ThinkingBlock::new();
        block.start(start);
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

    /// 默认展开（`visible` + 未按过 Ctrl+O）：流式路径同样带标题行 ——
    /// 这是存量用户（默认配置）现在看到的形态，提示指向「折叠」；并且
    /// 收起 → 再展开的往返（默认路径最频繁的动作）要在 CachedCell 边界上
    /// 保持子弹接管 / 归还的完整。
    #[test]
    fn a_visible_default_thinking_stream_carries_the_header() {
        use std::time::Instant;

        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let ctx = test_ctx(&palette, &layout); // visible + None

        let start = Instant::now();
        let mut block = ThinkingBlock::new();
        block.start(start);
        let mut cell = CachedCell::new(ChatCell::Thinking(block));
        cell.append_stream("the reasoning body");

        let frame = cell.compute_cell_frame(80, &ctx);
        let lines: Vec<String> = frame.lines.iter().map(|l| l.to_string()).collect();
        assert!(
            lines[0].contains("深度思考中") && lines[0].contains("Ctrl+O 折叠"),
            "默认展开的进行中标题应在第 0 行并提示折叠：{lines:?}"
        );
        let body = lines
            .iter()
            .find(|l| l.contains("the reasoning body"))
            .expect("正文应在行集里");
        assert!(body.starts_with("  "), "正文应保持两列缩进：{body:?}");

        // 收起（同一帧的 ctx 翻转）：只剩标题、正文不泄露。
        let collapsed = CellContext {
            thinking_expanded: Some(false),
            ..test_ctx(&palette, &layout)
        };
        let text = cell
            .compute_cell_frame(80, &collapsed)
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("深度思考中"), "{text}");
        assert!(
            !text.contains("the reasoning body"),
            "收起不泄露正文：{text}"
        );

        // 再展开：正文回来、标题仍在第 0 行，且首行前缀**恰好**两格
        // （子弹归标题行，接管 / 归还往返不残留、不多缩进）。
        let frame = cell.compute_cell_frame(80, &ctx);
        let lines: Vec<String> = frame.lines.iter().map(|l| l.to_string()).collect();
        assert!(
            lines[0].contains("深度思考中"),
            "标题应在第 0 行：{lines:?}"
        );
        let body_line = frame
            .lines
            .iter()
            .find(|l| l.to_string().contains("the reasoning body"))
            .expect("再展开后正文应回来");
        assert_eq!(
            body_line.spans[0].content.as_ref(),
            "  ",
            "正文首行前缀恰好两格：{body_line:?}"
        );
    }
}
