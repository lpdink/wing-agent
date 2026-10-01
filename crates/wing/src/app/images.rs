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
//!
//! # Freshness: a picture that is rewritten must come back
//!
//! The store never re-`stat`s a path on its own (that would be I/O on the
//! render path), so "the model overwrote `plot.png`" is invisible until
//! somebody says so. [`Images::poll_freshness`] is that somebody — the only
//! file I/O this lane does, kept **out** of the render path:
//!
//! ```text
//! App::draw ── observe_visible(frame_images)   ← the target set: what is on screen
//!                                                     │
//! event loop ── freshness_deadline(now) ──────────────┤  parks when nothing is anchored
//!            └─ poll_freshness(now)  ─── fs::metadata ─┘  1/s, bounded by the frame's
//!                  └─ ImageStore::refresh(path)             own placement table
//!                       └─ next frame's sync re-probes → new header → new encode
//! ```
//!
//! Three properties are the contract (and the tests): **bounded** (one entry
//! per visible anchor, deduped), **throttled** ([`FRESHNESS_INTERVAL`], with
//! the clock injected so the tests need no sleeping), and **not on the render
//! path** (the check runs from the event loop, never from `draw`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};

use crate::config::rendering::ImagesMode;
use crate::render::markdown::CellPixels;
use crate::render::markdown::ImageEntry;
use crate::render::markdown::ImageOpts;
use crate::render::markdown::ImageShape;
use crate::render::markdown::images::CAPTION_MARKER;
use crate::ui::chat_view::FrameImage;
use crate::ui::image::ImageState;
use crate::ui::image::ImageStore;
use crate::ui::image::ImageStoreConfig;
use crate::ui::image::ImageSupport;
use crate::ui::image::MetaState;
use crate::ui::image::paint as paint_image;

/// How often the freshness lane may re-`stat` the pictures that are on screen.
///
/// One second is the task's own bound ("at most once a second") and is well
/// below the cadence at which a rewrite becomes interesting: a model that
/// regenerates a chart writes the file during a tool call, and the check that
/// follows a second later re-reads it. A shorter interval multiplies `stat`
/// calls for no visible gain (the re-probe + re-encode behind it are the slow
/// half); a longer one starts to feel like the picture never updates.
pub(crate) const FRESHNESS_INTERVAL: Duration = Duration::from_secs(1);

/// One path's file version, as the lane last saw it.
///
/// The pair is exactly what [`ImageMeta`](crate::ui::image::ImageMeta) carries,
/// so "the version the store read" and "the version on disk" are comparable
/// without re-reading anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stamp {
    /// The path did not `stat` (gone, unreadable, a dangling symlink). A
    /// version in its own right: a file that *appears* is a change, which is
    /// what makes a deleted-then-restored picture recover.
    Missing,
    /// Byte length and modification time — the same identity the store's cache
    /// key is built from.
    File {
        bytes: u64,
        mtime: Option<SystemTime>,
    },
}

impl Stamp {
    /// What `path` looks like right now — the lane's single `stat`.
    fn read(path: &Path) -> Self {
        match fs::metadata(path) {
            Ok(stat) => Self::File {
                bytes: stat.len(),
                mtime: stat.modified().ok(),
            },
            Err(_) => Self::Missing,
        }
    }

    /// The version a store probe observed: a `Known` metadata *is* "these
    /// bytes, this mtime".
    fn of_meta(meta: &crate::ui::image::ImageMeta) -> Self {
        Self::File {
            bytes: meta.bytes,
            mtime: meta.mtime,
        }
    }
}

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
    /// What the freshness lane last saw on disk for the paths it watches. The
    /// fallback baseline for a path whose store answer is not a usable one (a
    /// probe in flight, or a path that degraded) — see
    /// [`Images::poll_freshness`]. Pruned to the watch set every frame.
    stamps: BTreeMap<PathBuf, Stamp>,
    /// The pictures the last frame put on screen, in anchor order and deduped:
    /// the freshness lane's target set. Bounded by the frame's own placement
    /// table (one entry per visible anchor), so the work per check does not
    /// grow with the session.
    watch: Vec<PathBuf>,
    /// When the last freshness check ran. `None` = never checked.
    last_check: Option<Instant>,
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
            stamps: BTreeMap::new(),
            watch: Vec::new(),
            last_check: None,
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
            stamps: BTreeMap::new(),
            watch: Vec::new(),
            last_check: None,
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
    /// nothing when the value is unchanged. A change restarts the metadata
    /// table: its keys were resolved against the *old* root, so a relative path
    /// may now name a different file (or none). Entries for absolute markdown
    /// paths go too — re-probing them costs one header read each, and "one
    /// workspace, one table" is the invariant that keeps the two in step.
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
            self.known.clear();
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

    /// Record the pictures the frame just laid out: the freshness lane's target
    /// set.
    ///
    /// Called once per frame, **before** the drawing gates — a picture
    /// suppressed by a toast or by a drag selection is still on screen, and
    /// still worth keeping in step with its file. Deduped here (a session that
    /// shows one picture in three cells watches one path) and pruned here too,
    /// so a path that left the screen keeps no baseline and the two structures
    /// stay the same size.
    pub(crate) fn observe_visible(&mut self, frames: &[FrameImage]) {
        if self.store.is_none() {
            return;
        }
        self.watch.clear();
        for frame in frames {
            if !self.watch.contains(&frame.path) {
                self.watch.push(frame.path.clone());
            }
        }
        let watch = &self.watch;
        self.stamps.retain(|path, _| watch.contains(path));
    }

    /// When the next freshness check is due, or `None` when there is nothing to
    /// watch — the lane is off, or no picture is on screen.
    ///
    /// The event loop parks its timer arm on `None`, so a session with nothing
    /// anchored pays exactly zero wake-ups for this lane; with pictures on
    /// screen it wakes at most once per [`FRESHNESS_INTERVAL`], and only marks
    /// the frame dirty when a file really changed.
    pub(crate) fn freshness_deadline(&self, now: Instant) -> Option<Instant> {
        if self.store.is_none() || self.watch.is_empty() {
            return None;
        }
        Some(match self.last_check {
            // Never checked: due now. The first check is the one that records
            // what the files look like.
            None => now,
            Some(last) => last + FRESHNESS_INTERVAL,
        })
    }

    /// Re-`stat` the pictures on screen and drop the store's memo for the ones
    /// whose file changed — the freshness half of the lane.
    ///
    /// * **Bounded**: the target set is [`Images::watch`], one deduped entry
    ///   per visible anchor, so a session with a thousand pictures costs the
    ///   same as one with two.
    /// * **Throttled**: at most one check per [`FRESHNESS_INTERVAL`]. `now` is
    ///   the caller's clock — the event loop passes `Instant::now()`, the tests
    ///   a synthetic instant, so the interval is testable without sleeping.
    /// * **I/O lives here and nowhere else**: one `fs::metadata` per watched
    ///   path, from the event loop — never from the render path, never from
    ///   [`crate::ui::image::paint`].
    ///
    /// The baseline a path is compared against is the version the **store**
    /// read (its memoised [`ImageMeta`](crate::ui::image::ImageMeta)) whenever
    /// it has one: that is what the picture on screen was encoded from, so a
    /// rewrite that raced the first check is still caught. When the store has
    /// no usable answer — a probe is in flight, or the path degraded into
    /// `Unavailable` — the lane falls back to its own last-seen stamp, which is
    /// what makes a mid-write file (truncated, empty, half a header) recover
    /// once the write finishes instead of staying broken until a content
    /// rebuild.
    ///
    /// Returns whether anything was refreshed: the caller redraws, and the
    /// ordinary `sync` → probe → `poll` path turns it into a fresh header, a
    /// re-laid-out box and a new encoding.
    pub(crate) fn poll_freshness(&mut self, now: Instant) -> bool {
        if self.watch.is_empty() {
            return false;
        }
        if self
            .last_check
            .is_some_and(|last| now.saturating_duration_since(last) < FRESHNESS_INTERVAL)
        {
            return false;
        }
        self.last_check = Some(now);
        let Some(store) = self.store.as_mut() else {
            return false;
        };
        let mut changed = false;
        for path in &self.watch {
            let baseline = match store.meta(path) {
                MetaState::Known(meta) => Some(Stamp::of_meta(&meta)),
                // No usable answer (in flight, or degraded): what we last saw
                // is the only baseline that keeps a broken path recoverable.
                _ => self.stamps.get(path).copied(),
            };
            // The one `stat` of the whole check, and the only I/O on this lane.
            let current = Stamp::read(path);
            if baseline.is_some_and(|baseline| baseline != current) {
                store.refresh(path);
                changed = true;
            }
            self.stamps.insert(path.clone(), current);
        }
        changed
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
            // The picture is painted over its caption — the box's first row *is*
            // the caption row (`▢ alt · W×H`, truncated to the box). Verifying
            // that here is the narrow form of "the anchor is where the row
            // arithmetic says it is": a mismatch (a row metric that drifted from
            // the renderer) leaves the caption in place instead of covering
            // someone else's text. Skipped when the box starts above the band —
            // there the caption row is scrolled away and the check is
            // unanswerable; the rows we would cover are clipped anyway.
            if frame.offset.1 >= 0 && !caption_at(frame.area, buf) {
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
    /// Rebuild the options the render layer reads (mode, workspace, table, cell).
    ///
    /// The cell pixels come from the store's own capability — the same
    /// `ImageSupport` the encoder encodes with — so the layout's rows and the
    /// drawn footprint can never be computed from two different terminals.
    fn rebuild_opts(&mut self) {
        let Some(store) = self.store.as_ref() else {
            self.opts = ImageOpts::off().clone();
            return;
        };
        // `is_enabled()` guarantees a valid cell; `None` here would mean a
        // probe-less capability, which `Images::new` never builds.
        let Some(cell) = store.support().cell_pixel_size() else {
            self.opts = ImageOpts::off().clone();
            return;
        };
        let entries = self
            .known
            .iter()
            .map(|(path, shape)| ImageEntry::new(path.clone(), *shape))
            .collect();
        self.opts = ImageOpts::anchor(
            self.workspace.clone(),
            entries,
            CellPixels::new(cell.width, cell.height),
        );
    }
}

/// Blank the part of the anchor's caption row the picture does not cover.
///
/// The box reserves exactly the rows the picture occupies, but the two can
/// still disagree on the **column** axis: a picture whose
/// [`MAX_ANCHOR_ROWS`](crate::render::markdown::MAX_ANCHOR_ROWS) cap binds (a
/// tall image) comes back fewer columns wide than the box (fit, not stretched —
/// see [`crate::ui::image`]), and a long caption would then peek out to the
/// right of the picture. The row is part of the picture's box, so the picture
/// owns it: everything the picture did not cover is cleared.
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
        // The symbol only: the row keeps whatever style it was rendered with
        // (a background tint stays a background tint — this is a patch, not a
        // reset).
        buf[(x, row)].set_symbol(" ");
    }
}

/// Whether `area`'s top-left cell holds the anchor's caption marker.
///
/// The caption is the anchor's first row and its first grapheme is
/// [`CAPTION_MARKER`]; the box's left column is where the caption text starts
/// (the 2-column cell prefix sits to its left), so this is an exact check of
/// "the picture goes here, over this caption".
fn caption_at(area: Rect, buf: &Buffer) -> bool {
    if area.x >= buf.area.right() || area.y >= buf.area.bottom() {
        return false;
    }
    buf[(area.x, area.y)].symbol().starts_with(CAPTION_MARKER)
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

    /// A `FrameImage` with only the fields the freshness lane reads.
    fn frame_image(path: &str) -> FrameImage {
        FrameImage {
            area: Rect::new(2, 0, 10, 4),
            offset: (2, 0),
            target: Size::new(10, 4),
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn the_freshness_timer_is_armed_only_while_a_picture_is_on_screen() {
        let support =
            ImageSupport::from_parts(ImageProtocol::Kitty, CellPixels::new(10, 20), false);
        let mut images = Images::new(ImagesMode::Auto, support, None);
        let now = Instant::now();
        images.observe_visible(&[]);
        assert_eq!(
            images.freshness_deadline(now),
            None,
            "nothing on screen is nothing to check: the timer arm parks"
        );

        let frame = frame_image("/ws/plot.png");
        images.observe_visible(&[frame.clone(), frame]);
        assert_eq!(images.watch.len(), 1, "one target per path, not per anchor");
        assert_eq!(
            images.freshness_deadline(now),
            Some(now),
            "a picture on screen with no check yet is due now"
        );
        images.last_check = Some(now);
        assert_eq!(
            images.freshness_deadline(now),
            Some(now + FRESHNESS_INTERVAL),
            "and then it waits out the window"
        );
    }

    #[test]
    fn the_freshness_bookkeeping_is_bounded_by_the_screen() {
        let support =
            ImageSupport::from_parts(ImageProtocol::Kitty, CellPixels::new(10, 20), false);
        let mut images = Images::new(ImagesMode::Auto, support, None);
        images
            .stamps
            .insert(PathBuf::from("/ws/old.png"), Stamp::Missing);
        images.observe_visible(&[frame_image("/ws/plot.png")]);
        assert!(
            images.stamps.is_empty(),
            "a path that left the screen keeps no baseline: the two sets stay \
             the same size, and neither grows with the session"
        );
    }

    #[test]
    fn a_disabled_lane_never_schedules_or_runs_a_check() {
        let mut images = Images::disabled();
        let now = Instant::now();
        images.observe_visible(&[frame_image("/ws/plot.png")]);
        assert_eq!(images.freshness_deadline(now), None);
        assert!(!images.poll_freshness(now), "and it stats nothing");
        assert!(images.stamps.is_empty());
    }
}
