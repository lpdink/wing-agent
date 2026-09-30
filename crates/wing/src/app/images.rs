//! The App's image lane: capability, store, metadata table and the drawing pass.
//!
//! One owner for everything the picture path needs, so the render layer and the
//! event loop stay free of it:
//!
//! ```text
//! run_app ── ImageSupport::detect() ──► Images::new(mode, support, waker)
//!                                            │
//! render layer ◄── Images::opts() ───────────┤  (mode + workspace + shapes)
//!                                            │
//! chat view ── FrameImage[] ────────────────►│  Images::paint()  → ImageStore::request
//!                                            │       │
//!                                            │       └─► ui::image::paint()
//!                                            │
//! worker thread ── waker ────────────────────┘  (wake the event loop, then poll)
//! ```
//!
//! # The two-tier rule, enforced here
//!
//! `Images` is *the* gate: when the mode is `off`, the terminal has no graphics
//! protocol, or the store could not start, `is_enabled()` is false, `opts()`
//! returns the shared [`ImageOpts::off`] and the render layer produces no
//! anchors at all — the markdown image keeps its existing link rendering, cell
//! for cell. Nothing is queued and no file is read. There is no third tier.
//!
//! # Degradation ladder
//!
//! | state | what the user sees |
//! |---|---|
//! | off / no protocol / probe failed | the legacy link path |
//! | metadata unknown (probe in flight) | the legacy link path (no table entry yet) |
//! | metadata unavailable (missing, corrupt, oversized, worker died) | the legacy link path (never enters the table) |
//! | metadata known, encode pending / failed | the anchor box with its caption — never a blank hole |
//! | ready | the picture over the box |
//!
//! # Zero I/O on the render path
//!
//! [`Images::sync`] and [`Images::request`] are hash lookups into the store's
//! memo and LRU; every file read, decode and protocol encode happens on the
//! store's worker thread (see [`crate::ui::image`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::style::Style;

use crate::config::rendering::ImagesMode;
use crate::render::markdown::ImageEntry;
use crate::render::markdown::ImageOpts;
use crate::render::markdown::ImageShape;
use crate::ui::chat_view::FrameImage;
use crate::ui::image::ImageState;
use crate::ui::image::ImageStore;
use crate::ui::image::ImageStoreConfig;
use crate::ui::image::ImageSupport;
use crate::ui::image::MetaState;
use crate::ui::image::paint as paint_image;

/// The App's image lane — see the module docs.
pub(crate) struct Images {
    /// `None` when nothing can be drawn: the mode is `off`, the terminal has no
    /// graphics protocol, or the store's worker could not start. Every entry
    /// point short-circuits on it, so "off" costs nothing per frame.
    store: Option<ImageStore>,
    /// The configured mode (kept for diagnostics / the enabled gate).
    mode: ImagesMode,
    /// Workspace root for relative markdown image paths (the session workdir,
    /// falling back to the TUI's launch directory). `None` rejects relative
    /// paths — absolute markdown paths still resolve.
    workspace: Option<PathBuf>,
    /// Metadata of every image the conversation references: `resolve_image_path`
    /// key → header pixel size. **Only ever grows** for a given workspace: a
    /// path that leaves the candidate list (its anchor replaced the link span)
    /// must not lose its shape, or the anchor would flip back to the link path
    /// and the row count would oscillate.
    known: BTreeMap<PathBuf, ImageShape>,
    /// The chat structure epoch this lane last saw (see
    /// [`Images::set_structure_epoch`]).
    epoch: u64,
    /// The options handed to the render layer — `Off` while disabled, otherwise
    /// `Anchor(workspace, known)`.
    opts: ImageOpts,
}

impl Images {
    /// A lane that cannot draw anything (no store, `Off` options).
    ///
    /// What `App::new` gets: the tests and every non-image code path behave
    /// exactly as before, and no probe, thread or file read ever happens.
    pub(crate) fn disabled() -> Self {
        Self {
            store: None,
            mode: ImagesMode::Off,
            workspace: None,
            known: BTreeMap::new(),
            epoch: 0,
            opts: ImageOpts::off().clone(),
        }
    }

    /// A lane for `mode` and the terminal's capability.
    ///
    /// `ImagesMode::Off` or an incapable terminal builds the same disabled lane
    /// as [`Images::disabled`] — the config gate and the capability gate are
    /// one code path, which is what makes "off" provably free.
    ///
    /// `waker` runs on the store's worker thread after every finished job and
    /// must be cheap (see [`ImageStoreConfig::waker`]); `None` means the host
    /// polls per frame instead, up to one frame later.
    pub(crate) fn new(
        mode: ImagesMode,
        support: ImageSupport,
        waker: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        if mode == ImagesMode::Off || !support.is_enabled() {
            return Self::disabled();
        }
        let store = ImageStore::with_config(
            support,
            ImageStoreConfig {
                waker,
                ..ImageStoreConfig::default()
            },
        );
        let mut lane = Self {
            store: Some(store),
            mode,
            workspace: None,
            known: BTreeMap::new(),
            epoch: 0,
            opts: ImageOpts::off().clone(),
        };
        lane.rebuild_opts();
        lane
    }

    /// Whether pictures can be drawn at all (mode on **and** a live store).
    ///
    /// The two gates are one code path by construction ([`Images::new`] builds
    /// the disabled lane for either), so this is the whole capability question
    /// the rest of the app has to ask.
    pub(crate) fn is_enabled(&self) -> bool {
        self.mode != ImagesMode::Off && self.store.is_some()
    }

    /// The configured mode (diagnostics / tests).
    #[cfg(test)]
    pub(crate) fn mode(&self) -> ImagesMode {
        self.mode
    }

    /// Store counters (tests only): what is cached, what is in flight, whether
    /// a worker is alive.
    #[cfg(test)]
    pub(crate) fn stats(&self) -> Option<crate::ui::image::StoreStats> {
        self.store.as_ref().map(|store| store.stats())
    }

    /// The options the render layer renders with this frame.
    pub(crate) fn opts(&self) -> &ImageOpts {
        &self.opts
    }

    /// Point the lane at the workspace relative image paths resolve against.
    ///
    /// The session workdir arrives asynchronously (session info) and can change
    /// (`/workdir`, session switch), so this is called once per frame and does
    /// nothing when the value is unchanged. A change rebuilds the options —
    /// cells invalidate, their anchors re-resolve against the new root.
    pub(crate) fn set_workspace(&mut self, workspace: Option<&str>) -> bool {
        if self.store.is_none() {
            return false;
        }
        let changed = match (self.workspace.as_deref(), workspace) {
            (None, None) => false,
            (Some(current), Some(next)) => current != Path::new(next),
            _ => true,
        };
        if changed {
            self.workspace = workspace.map(PathBuf::from);
            self.rebuild_opts();
        }
        changed
    }

    /// Announce the view's structure epoch
    /// ([`ChatView::structure_epoch`](crate::ui::chat_view::ChatView::structure_epoch)).
    ///
    /// A rebuild — session switch, compaction re-sync, rewind replay — replaces
    /// the whole content list, so the files the *new* content names may have
    /// changed on disk since they were probed (there is no file watcher), and
    /// the old content's pictures are gone. Everything is therefore dropped:
    /// the encodings, the store's metadata memo **and** this lane's table —
    /// which the next frames rebuild from the new content's candidates. A
    /// picture that was replaced is laid out from its new header; one that
    /// vanished stops producing an anchor at all.
    pub(crate) fn set_structure_epoch(&mut self, epoch: u64) {
        if self.epoch == epoch {
            return;
        }
        self.epoch = epoch;
        if let Some(store) = self.store.as_mut() {
            store.reset();
        }
        // Within one content generation the table only grows (an anchored
        // picture stops being a *link*, and shrinking the table would flip it
        // back); a rebuild starts a new generation, so it starts empty.
        self.known.clear();
        self.rebuild_opts();
    }

    /// One frame's plumbing: drain the worker, ask about every candidate, keep
    /// the metadata table current. Returns whether anything the frame depends
    /// on changed — the metadata table, or a picture that just finished
    /// encoding — which means the caller must redraw.
    ///
    /// Called twice per frame — before the render (so a result that landed is
    /// drawn *this* frame) and after it (so candidates the render just
    /// discovered get probed, which is what wakes the loop when it answers).
    /// Both halves are hash lookups: [`ImageStore::meta`] enqueues a probe only
    /// the first time it sees a path.
    pub(crate) fn sync(&mut self, candidates: &[PathBuf]) -> bool {
        let Some(store) = self.store.as_mut() else {
            return false;
        };
        let polled = store.poll();
        let mut table_changed = false;
        for path in candidates {
            let MetaState::Known(meta) = store.meta(path) else {
                continue;
            };
            if meta.px_w == 0 || meta.px_h == 0 {
                continue;
            }
            let shape = ImageShape::new(meta.px_w, meta.px_h);
            if self.known.get(path) != Some(&shape) {
                self.known.insert(path.clone(), shape);
                table_changed = true;
            }
        }
        if table_changed {
            // Only a *table* change can move an anchor's rows; a finished
            // encode just means the picture can be drawn now.
            self.rebuild_opts();
        }
        table_changed || polled
    }

    /// The picture for one anchor, or why there is none.
    ///
    /// A hash lookup plus a redraw at most: encoding happens on the worker, and
    /// asking again in the next frame is the normal case (`Pending`).
    pub(crate) fn request(&mut self, path: &Path, target: Size) -> ImageState {
        match self.store.as_mut() {
            Some(store) => store.request(path, target),
            None => ImageState::Unavailable(crate::ui::image::Unavailable::Disabled),
        }
    }

    /// The terminal may no longer show what we sent it (`terminal.clear()`,
    /// resize, focus regain): drop every encoding and revoke the handles.
    ///
    /// Metadata is kept — the files on disk did not change — so the next frame
    /// re-encodes from the same table. See [`ImageStore::invalidate`].
    pub(crate) fn invalidate(&mut self) {
        if let Some(store) = self.store.as_mut() {
            store.invalidate();
        }
    }

    /// Draw this frame's recorded pictures — the last write over their rects.
    ///
    /// `clip` is the region pictures may touch (the chat band's content rect:
    /// the scrollbar gutter is not in it). `mask` is an overlay painted on top
    /// of the chat (the toast): a picture that would be covered is skipped
    /// **whole** rather than partially — the protocols carry their payload in
    /// individual cells (`paint`'s contract), so a partial overdraw would break
    /// the image instead of hiding part of it. The anchor's caption stays
    /// visible in that case, which is exactly the reserved box's job.
    ///
    /// Callers gate this on the selection (a drag captures text; see
    /// `App::draw`) — this function assumes it may draw.
    pub(crate) fn paint(
        &mut self,
        frame_images: &[FrameImage],
        clip: Rect,
        mask: Option<Rect>,
        buf: &mut Buffer,
    ) {
        if self.store.is_none() || frame_images.is_empty() {
            return;
        }
        for frame in frame_images {
            if frame.area.width == 0 || frame.area.height == 0 {
                continue;
            }
            // Scrolled out of the band, or under an overlay.
            if !frame.area.intersects(clip) {
                continue;
            }
            if mask.is_some_and(|mask| frame.area.intersects(mask)) {
                continue;
            }
            let ImageState::Ready(image) = self.request(&frame.path, frame.target) else {
                // Pending / unavailable: the caption is the fallback, and it is
                // already on screen — never blank the box.
                continue;
            };
            // The picture is placed at the box's top-left corner; `paint` clips
            // it against the band and refuses anything wider than the clip (a
            // caller bug rather than something to paper over).
            let Some(covered) = paint_image(&image, clip, frame.offset, buf) else {
                continue;
            };
            clear_uncovered_caption(frame, covered, buf);
        }
    }
}

impl Images {
    /// Rebuild the options the render layer reads (mode, workspace, table).
    fn rebuild_opts(&mut self) {
        if self.store.is_none() {
            self.opts = ImageOpts::off().clone();
            return;
        }
        let entries = self
            .known
            .iter()
            .map(|(path, shape)| ImageEntry::new(path.clone(), *shape))
            .collect();
        self.opts = ImageOpts::anchor(self.workspace.clone(), entries);
    }
}

/// Blank the part of the anchor's caption row the picture does not cover.
///
/// The box is sized from the image's aspect ratio, but the encoded footprint is
/// capped by the terminal's cell size: a picture taller than
/// [`MAX_ANCHOR_ROWS`](crate::render::markdown::MAX_ANCHOR_ROWS) comes back
/// narrower than the box (fit, not stretched — see [`crate::ui::image`]), and a
/// long caption would then peek out to the right of the picture. The row is
/// part of the picture's box, so the picture owns it: everything the picture
/// did not cover is cleared.
///
/// Only the caption row carries text (the rows below it are blank cover rows),
/// and only when the box's first row is on screen at all (`offset.1 < 0` means
/// the box starts above the band). Cells the picture **did** cover are never
/// touched: the kitty placeholders and sixel anchors live there.
fn clear_uncovered_caption(frame: &FrameImage, covered: Rect, buf: &mut Buffer) {
    if frame.offset.1 < 0 || covered.y != frame.area.y {
        return;
    }
    let row = frame.area.y;
    if row < buf.area.y || row >= buf.area.bottom() {
        return;
    }
    let from = covered.right().max(buf.area.x);
    let to = frame.area.right().min(buf.area.right());
    for x in from..to {
        buf.set_string(x, row, " ", Style::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::image::CellPixels;
    use crate::ui::image::ImageProtocol;

    #[test]
    fn the_off_mode_and_an_incapable_terminal_build_the_same_disabled_lane() {
        let support =
            ImageSupport::from_parts(ImageProtocol::Kitty, CellPixels::new(10, 20), false);
        let off = Images::new(ImagesMode::Off, support.clone(), None);
        assert!(!off.is_enabled());
        assert_eq!(off.mode(), ImagesMode::Off);
        assert!(!off.opts().is_enabled(), "off must render the link path");

        let incapable = Images::new(ImagesMode::Auto, ImageSupport::disabled(), None);
        assert!(!incapable.is_enabled());
        assert_eq!(incapable.mode(), ImagesMode::Off, "the gate collapses");
        assert!(!incapable.opts().is_enabled());
    }

    #[test]
    fn an_enabled_lane_anchors_once_metadata_is_known() {
        let support =
            ImageSupport::from_parts(ImageProtocol::Kitty, CellPixels::new(10, 20), false);
        let mut images = Images::new(ImagesMode::Auto, support, None);
        assert!(images.is_enabled());
        assert_eq!(images.mode(), ImagesMode::Auto);
        // Anchors are on (mode), the table starts empty.
        assert!(images.opts().is_enabled());
        assert!(images.opts().shapes().is_empty());

        assert!(images.set_workspace(Some("/ws")));
        assert!(images.opts().workspace().is_some());
        // Idempotent: the same workspace is not a change.
        assert!(!images.set_workspace(Some("/ws")));
        assert!(images.set_workspace(Some("/other")));
    }

    #[test]
    fn a_disabled_lane_never_asks_and_never_draws() {
        let mut images = Images::disabled();
        assert!(!images.set_workspace(Some("/ws")));
        assert!(!images.sync(&[PathBuf::from("/ws/plot.png")]));
        assert!(matches!(
            images.request(Path::new("/ws/plot.png"), Size::new(10, 4)),
            ImageState::Unavailable(_)
        ));
        images.invalidate();
        let clip = Rect::new(0, 0, 10, 4);
        let mut buf = Buffer::empty(clip);
        let before = buf.clone();
        images.paint(&[], clip, None, &mut buf);
        assert_eq!(buf, before);
    }
}
