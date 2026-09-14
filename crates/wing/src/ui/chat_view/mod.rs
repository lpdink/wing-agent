//! Chat view — scrollable area for conversation cells.
//!
//! Split into four concerns, each with exactly one home:
//!
//! * **content model** (`model`) — the cells, the pending queue, anchored
//!   insertion and every streaming / by-index mutation. It never reads scroll
//!   state and never renders; the drawing and the frame state are somebody
//!   else's business.
//! * **viewport** (`viewport`) — the scroll offset, the follow contract
//!   (pinning / freezing), the per-frame geometry, the height cache refresh
//!   and the widget that draws the band. It knows *how tall* and *where*, not
//!   *what* the cells mean.
//! * **frame snapshot & selection** (`frame`) — `FrameSnapshot`, the
//!   copy-on-select source: row-level graphemes captured out of the drawn
//!   frame, with the frame's own scroll offset and content width. Screen ↔
//!   content mapping, highlight painting and text extraction read the
//!   snapshot, never the live buffer.
//! * **links** (`link`) — the per-frame link table ([`FrameLink`] hit
//!   boxes), the click hit test, masking for overlays and the OSC8 injection
//!   that makes the terminals linkify what the table promises.
//!
//! `cell` holds the content unit itself ([`ChatCell`] + its rendering) and
//! the pending message wrapper.
//!
//! The scroll bar drawn beside the band is an overlay owned by
//! `ui::scrollbar` + `App`; the height cache lives in `ui::cached_cell`.
//!
//! **Public API**: the split keeps `crate::ui::chat_view::{ChatCell, ChatView,
//! ChatViewWidget, ChatGeometry, FrameLink, PendingMessage,
//! render_info_separator}` exactly as they were — names, signatures and paths.
//! `app/**` is not allowed to notice this refactor.
//!
//! Width-aware virtualization: each cell's height comes from [`CachedCell`]
//! (generation-invalidated cache), and the widget walks the cells that
//! intersect the visible window.

mod cell;
mod frame;
mod link;
mod model;
mod viewport;

#[cfg(test)]
mod test_support;

pub use cell::ChatCell;
pub use cell::PendingMessage;
pub use link::FrameLink;
pub use viewport::ChatGeometry;
pub use viewport::ChatViewWidget;
pub(crate) use viewport::render_info_separator;

use ratatui::text::Line;

use crate::ui::cached_cell::CachedCell;

use frame::FrameSnapshot;
use link::LinkTable;

/// Scrollable chat view — the viewport the overlay scrollbar (`ui::scrollbar`)
/// tracks (scroll offset + follow state).
///
/// The fields are the state ledger of the four concerns above: the content
/// model (`cells` + `pending`, with their height caches), the viewport
/// (`scroll_offset` / `auto_scroll` / `follow_frozen` / `geometry` /
/// `last_total`), the copy snapshot (`FrameSnapshot`) and the frame's link
/// table (`LinkTable`). Private fields stay reachable from the child
/// modules — that is what lets each concern keep its own `impl ChatView`
/// block without widening the type's visibility.
pub struct ChatView {
    pub(crate) cells: Vec<CachedCell>,
    /// Cached wrap-aware line count per cell (mirrors cells.len()).
    cell_heights: Vec<usize>,
    /// User messages awaiting model acceptance, in send order. Rendered
    /// below all cells; promoted/discarded by the app on acceptance
    /// events (see `push_pending` and friends).
    pub(crate) pending: Vec<PendingMessage>,
    /// Cached wrap-aware line count per pending message.
    pending_heights: Vec<usize>,
    /// Scroll offset in lines (0 = top).
    pub(crate) scroll_offset: usize,
    /// Whether auto-scroll is active (follow bottom).
    auto_scroll: bool,
    /// Follow is frozen for the duration of a drag selection.
    ///
    /// `auto_scroll = false` alone is not enough: the render re-arms
    /// `auto_scroll` whenever the offset sits at the bottom edge, which is
    /// exactly where a drag on the newest output starts — the very next frame
    /// would undo the freeze. Only the scroll entries (the sole owners of the
    /// follow contract) may lift it.
    follow_frozen: bool,
    /// Header lines (wing logo + MOTD) — always rendered at the top,
    /// scroll with the content. Preserved across `clear()`.
    header_lines: Vec<Line<'static>>,
    /// Total content height (header + cells) from the last render.
    /// Used by `scroll_down` to detect the bottom edge and re-arm
    /// auto-scroll immediately, without waiting for the next render pass.
    pub(crate) last_total: usize,
    /// Counts full content rebuilds ([`Self::clear`]: session switch,
    /// compaction re-sync, rewind replay). A selection is anchored to content
    /// rows, so a rebuild invalidates it even when the rebuilt list happens to
    /// end up with the same number of cells — see `App::selection_guard`.
    rebuilds: u64,
    /// Geometry of the last render (see [`ChatGeometry`]).
    geometry: ChatGeometry,
    /// Links of the last rendered frame, per screen row (absolute columns).
    ///
    /// Rebuilt from scratch on every render: a press lands between frames, so
    /// the hit test has to answer from the frame the user was looking at —
    /// same "WYSIWYG" contract as the selection's row snapshot.
    frame_links: LinkTable,
    /// Graphemes of the visible chat rows as of the last *drag* frame — the
    /// copy-on-select source (see [`FrameSnapshot`]).
    ///
    /// Copy-on-select is WYSIWYG: the release event lands between frames, so
    /// the text has to be taken from a frame the user was actually looking
    /// at. Refreshed only while a drag is in flight
    /// ([`Self::capture_visible_rows`]), so an idle app pays nothing.
    snapshot: FrameSnapshot,
}

impl ChatView {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            cell_heights: Vec::new(),
            pending: Vec::new(),
            pending_heights: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            follow_frozen: false,
            header_lines: Vec::new(),
            last_total: 0,
            rebuilds: 0,
            geometry: ChatGeometry::default(),
            frame_links: LinkTable::new(),
            snapshot: FrameSnapshot::default(),
        }
    }
}

impl Default for ChatView {
    fn default() -> Self {
        Self::new()
    }
}
