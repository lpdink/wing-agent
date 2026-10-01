//! Image anchors: `![alt](path)` as a reserved block plus a side channel.
//!
//! Markdown images have always rendered through the **link path** (the alt
//! text as a link, `Tag::Image` sharing `Tag::Link`'s handling). This module
//! adds the second tier: in [`ImageMode::Anchor`] an image that stands alone
//! on its line becomes an **anchor** — a block of `rows` lines whose first
//! line is a copyable caption, plus an [`ImageSpan`] telling the UI layer
//! where the picture goes. Nothing here draws anything; that is the chat
//! view's job (a later step).
//!
//! # The two-tier rule (the task's D2)
//!
//! [`ImageMode::Off`] — or an image with no metadata, a rejected path, or any
//! shape other than "alone on its line" — renders **exactly** like today:
//! every span identical to the link path. There is no third tier: no
//! half-blocks, no mosaics, no ASCII art.
//!
//! # Zero I/O (the task's D3/D4)
//!
//! This module never touches the filesystem: no `stat`, no decode, no
//! `canonicalize`. The pixel dimensions arrive in [`ImageOpts::shapes`] — a
//! table the caller (the chat view, backed by `ui::image::ImageStore`) fills
//! from header probes. A path with no table entry falls back to the link
//! path, which makes "the file does not exist / is not an image / is too
//! large" the caller's answer rather than something this layer guesses at.
//!
//! # The row count is a pure function of the box, the picture and the cell
//!
//! ```text
//! rows = fit_cells(px_w, px_h, (W, MAX_ANCHOR_ROWS), cell).height
//! ```
//!
//! `W` is the markdown render width (the cell width minus its 2-column
//! prefix — the anchor's [`ImageSpan::cols`]) and `cell` is the terminal's
//! character cell in pixels, injected through [`ImageOpts`] exactly like the
//! shape metadata. The box and the picture that the **drawing layer** encodes
//! are computed by the same shared function ([`crate::render::fit`]), so the
//! rows reserved here are the rows the picture actually occupies: a 512×512
//! image at a 10×20 cell in a 140-column box reserves 26 rows and is drawn
//! 52×26 cells — instead of reserving the 36 rows an aspect assumption would
//! have guessed and leaving the bottom blank.
//!
//! The cell size is an input, but it is a **stable** one, which is what keeps
//! the two invariants that a capability-dependent height would break: it is
//! probed once at startup and rides in `ImageOpts`'s structural equality, so
//! the same "the options changed, rebuild the cell" path that carries a
//! late-arriving shape also carries a cell change. `CachedCell`'s height cache
//! and the streaming engine's "resting state == the reference render"
//! invariant see the same rows for a given (width, shape, cell) triple, frame
//! after frame.
//!
//! Everything else is deliberately *not* an input: the picture's file, its
//! decode, the protocol and the terminal's answers beyond the cell size. The
//! row count is exact arithmetic over three numbers.
//!
//! # The anchor block
//!
//! Line 0 is the caption ([`anchor_caption`]): visible when no image is
//! painted, and the text a drag-selection copies. Lines 1..rows are blank
//! cover rows — the box the picture is painted over. A picture is only ever
//! anchored when it is **alone on its line at the top level**; anything else
//! (inline, in a list item, in a quote, in a table cell, inside a heading or
//! a link, inside a code fence, inside a nested prose block) stays on the
//! link path rather than producing a misaligned anchor.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use ratatui::layout::Size;

use super::types::truncate_to_display_width;
use crate::render::fit::CellPixels;
use crate::render::fit::fit_cells;

// ============================================================
// Layout constants (the shared fit's inputs)
// ============================================================

/// Fewest rows an anchor may reserve (a single caption row).
pub const MIN_ANCHOR_ROWS: u16 = 1;

/// Most rows an anchor may reserve — the height of the box the picture is
/// fitted into.
///
/// About one screen on a 40-row terminal: without it an 800×6000 sliver would
/// swallow the viewport. The cap enters through the *box* rather than as a
/// clamp on a finished number, and since a fit never grows its box, no anchor
/// can exceed it.
pub const MAX_ANCHOR_ROWS: u16 = 36;

/// Longest accepted image path, in characters.
pub const MAX_IMAGE_PATH_CHARS: usize = 512;

/// Extensions the image pipeline may draw (case-insensitive).
///
/// This is a cheap pre-filter, not the gate: the caller's metadata table only
/// holds paths whose header actually parsed, so a `.png`-named text file is
/// rejected by the missing table entry, not here.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

// ============================================================
// Modes and metadata
// ============================================================

/// How markdown images render.
///
/// [`ImageMode::Off`] is the default and the "existing behaviour" tier: an
/// image renders through the link path, span for span, exactly as before this
/// module existed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageMode {
    /// Link path only (no anchors, no metadata lookups).
    #[default]
    Off,
    /// Standalone images become anchors when their metadata is known.
    Anchor,
}

/// Pixel dimensions of one image, as read from its header.
///
/// Deliberately smaller than `ui::image::ImageMeta` (which also carries file
/// size and mtime): the layout needs both pixel dimensions — the fit is a
/// size computation, not a ratio one — and nothing else. The chat view builds
/// one from a probe result with `ImageShape::new(meta.px_w, meta.px_h)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageShape {
    /// Image width in pixels.
    pub px_w: u32,
    /// Image height in pixels.
    pub px_h: u32,
}

impl ImageShape {
    /// A shape from header pixel dimensions.
    pub const fn new(px_w: u32, px_h: u32) -> Self {
        Self { px_w, px_h }
    }

    /// Whether the shape can be laid out at all (both dimensions non-zero).
    pub const fn is_usable(&self) -> bool {
        self.px_w > 0 && self.px_h > 0
    }
}

/// One row of the caller's metadata table: a resolved path and its shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageEntry {
    /// Path as produced by [`resolve_image_path`] — the table's key.
    pub path: PathBuf,
    /// Header pixel dimensions.
    pub shape: ImageShape,
}

impl ImageEntry {
    /// A table row for `path`.
    pub fn new(path: impl Into<PathBuf>, shape: ImageShape) -> Self {
        Self {
            path: path.into(),
            shape,
        }
    }
}

/// Image rendering options: the mode, the workspace root, the metadata, and
/// the terminal's character cell.
///
/// Owned (not a borrowed view) because [`super::stream::StreamingRender`]
/// outlives a single render call and has to notice when an input changes (a
/// late-arriving probe, or a cell size, changes the row count, which must
/// force a rebuild). Equality is structural, which is exactly how that change
/// is detected — the cell rides the same channel as a shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageOpts {
    mode: ImageMode,
    workspace: Option<PathBuf>,
    shapes: Vec<ImageEntry>,
    /// The terminal's character cell in pixels: the unit the anchor's rows and
    /// the encoder's footprint are both expressed in. Only meaningful in
    /// [`ImageMode::Anchor`], where it is always valid (see
    /// [`anchor`](Self::anchor)); zero otherwise.
    cell: CellPixels,
}

/// The shared "images are off" options — what [`super::RenderOpts::new`]
/// borrows, so a render with default options pays nothing for images.
static NO_IMAGES: ImageOpts = ImageOpts {
    mode: ImageMode::Off,
    workspace: None,
    shapes: Vec::new(),
    cell: CellPixels::new(0, 0),
};

impl ImageOpts {
    /// The shared `Off` options (no anchors, no metadata lookups).
    pub fn off() -> &'static ImageOpts {
        &NO_IMAGES
    }

    /// `Anchor` options over `shapes`, resolving relative paths against
    /// `workspace` and sizing the boxes for a character cell of `cell` pixels.
    ///
    /// `workspace = None` means "relative paths are rejected" — the table's
    /// keys are then absolute paths only.
    ///
    /// A **degenerate cell** (either dimension zero) is not a layout input:
    /// the rows cannot be derived from it, and guessing one is the very
    /// mismatch this contract removes. Such a call answers with the shared
    /// [`ImageOpts::off`] — the picture keeps the link path — rather than
    /// reserving a box the encoder could never fill. In production the cell
    /// comes from [`ImageSupport::cell_pixel_size`], which is only `Some` for a
    /// non-degenerate cell (`ImageSupport::is_enabled` is false otherwise), so
    /// this branch is a guard, not a state a running app can reach.
    ///
    /// [`ImageSupport::cell_pixel_size`]: crate::ui::image::ImageSupport::cell_pixel_size
    ///
    /// Table contract: every key is the output of [`resolve_image_path`] for
    /// the destination as written in the markdown, and **a path appears at
    /// most once**. A duplicate is a caller bug (a directory scan that
    /// re-registers the same file): the lookup answers with the first row, so
    /// the result stays deterministic, but the contract is "one row per path".
    /// Keep the table scoped to what the view is about to draw rather than to
    /// "every image in the workspace" — see [`shape_for`](Self::shape_for).
    pub fn anchor(workspace: Option<PathBuf>, shapes: Vec<ImageEntry>, cell: CellPixels) -> Self {
        if !cell.is_valid() {
            return Self::off().clone();
        }
        Self {
            mode: ImageMode::Anchor,
            workspace,
            shapes,
            cell,
        }
    }

    /// The active mode.
    pub fn mode(&self) -> ImageMode {
        self.mode
    }

    /// The workspace root used to resolve relative paths.
    pub fn workspace(&self) -> Option<&Path> {
        self.workspace.as_deref()
    }

    /// The metadata table.
    pub fn shapes(&self) -> &[ImageEntry] {
        &self.shapes
    }

    /// The character cell every row count is computed against.
    ///
    /// Zero-sized when anchors are off; the lane that builds the options is the
    /// one that owns the terminal capability, so the layout and the encoder are
    /// handed the same numbers by construction.
    pub fn cell_pixels(&self) -> CellPixels {
        self.cell
    }

    /// Whether anchors are enabled at all.
    pub fn is_enabled(&self) -> bool {
        self.mode == ImageMode::Anchor
    }

    /// The shape recorded for `path` (an exact match of the resolved form).
    ///
    /// A linear scan: the table is expected to be the handful of images of one
    /// screen (the caller reuses its own probe cache to build it once per
    /// layout/metadata change, not once per image per frame), and the markdown
    /// renderer asks at most once per image per parse — with the streaming
    /// engine, once per image per *block*, not per frame. Duplicate rows answer
    /// with the first one (see [`anchor`](Self::anchor)).
    pub fn shape_for(&self, path: &Path) -> Option<ImageShape> {
        self.shapes
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.shape)
    }
}

// ============================================================
// Row count — the shared fit
// ============================================================

/// Rows an image anchor reserves at markdown width `width`.
///
/// The anchor's box is `width × MAX_ANCHOR_ROWS` cells; the picture is fitted
/// into it exactly as the drawing layer will fit it, and the rows reserved are
/// the rows the fit occupies — so the box is the size of the picture, and
/// nothing is left blank underneath. `cell` is the terminal's character cell in
/// pixels (see [`ImageOpts`]); the arithmetic itself is
/// [`crate::render::fit::fit_cells`], shared with `ui/image/encode.rs`.
///
/// Off-screen inputs are impossible: the cap is the box the fit is computed
/// against, and a fit never grows its box, so the answer is already inside
/// [`MIN_ANCHOR_ROWS`]..=[`MAX_ANCHOR_ROWS`]. The clamp is kept as the written
/// contract (and it is the guard that makes the lower bound explicit — the fit's
/// own `max(…, 1)` is one layer down).
///
/// Total: a zero width, a degenerate shape (`px_h == 0`) or a degenerate cell
/// answers [`MIN_ANCHOR_ROWS`] rather than panicking. The parse layer rejects
/// those cases before they can become an anchor (a degenerate cell switches
/// anchors off entirely — see [`ImageOpts::anchor`]).
pub fn anchor_rows(width: u16, shape: ImageShape, cell: CellPixels) -> u16 {
    if width == 0 || !shape.is_usable() || !cell.is_valid() {
        return MIN_ANCHOR_ROWS;
    }
    let fitted = fit_cells(
        shape.px_w,
        shape.px_h,
        Size::new(width, MAX_ANCHOR_ROWS),
        cell,
    );
    fitted.height.clamp(MIN_ANCHOR_ROWS, MAX_ANCHOR_ROWS)
}

// ============================================================
// Path policy — pure, no I/O
// ============================================================

/// Why a markdown image destination cannot be anchored.
///
/// Every variant means the same thing to the renderer: this image keeps the
/// link path. The reason exists for callers that want to log or explain it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathReject {
    /// The destination is empty (or whitespace only).
    Empty,
    /// The destination contains a control character (C0/C1/DEL) — it could
    /// forge a terminal escape sequence.
    ControlChar,
    /// The destination is longer than [`MAX_IMAGE_PATH_CHARS`] characters.
    TooLong,
    /// A remote or otherwise non-local scheme (`http:`, `https:`, `data:`…).
    RemoteUrl,
    /// A `~`-relative path — expanding it needs the environment, and this
    /// layer does not read it.
    HomeRelative,
    /// A relative path with no workspace root configured.
    NoWorkspace,
    /// Lexical normalisation left the workspace root (`..` escape).
    EscapesWorkspace,
    /// The extension is not in [`IMAGE_EXTENSIONS`].
    UnsupportedExtension,
}

impl fmt::Display for PathReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("empty image path"),
            Self::ControlChar => f.write_str("image path contains a control character"),
            Self::TooLong => f.write_str("image path is too long"),
            Self::RemoteUrl => f.write_str("image path is not a local file"),
            Self::HomeRelative => f.write_str("home-relative image paths are not resolved"),
            Self::NoWorkspace => f.write_str("no workspace root to resolve the image path"),
            Self::EscapesWorkspace => f.write_str("image path escapes the workspace"),
            Self::UnsupportedExtension => f.write_str("not a supported image extension"),
        }
    }
}

/// Whether the workspace containment check below folds ASCII case.
///
/// macOS and Windows *default* to case-insensitive filesystems; Linux does not.
/// (A macOS volume can be formatted case-sensitive — the rule is per filesystem,
/// and the platform default is the only thing a lexical check can follow.)
///
/// Exactly one input can tell the two rules apart, and it is the `..` fold:
/// under `/workspace`, the relative path `../WORKSPACE/a.png` names a file
/// *inside* the workspace on a case-insensitive filesystem, so refusing it would
/// be a false "escapes the workspace" that costs the user a picture. A variant
/// spelling *below* the root is admitted (or refused) identically under both
/// rules — the joined path carries the workspace's own spelling, byte for byte.
/// The path itself is never rewritten: this decides only what counts as *the
/// same* path.
const CASE_INSENSITIVE_PATHS: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// [`Path::starts_with`] under the platform's case rule (see
/// [`CASE_INSENSITIVE_PATHS`]) — still purely lexical, no `stat` and no
/// `canonicalize`, so the module's zero-I/O contract is untouched.
///
/// Only ASCII case is folded (Unicode folding would need a table this layer has
/// no business carrying), and symlinks are still compared by their own path:
/// the boundary here is lexical by design. Reachable only from the relative
/// branch of [`resolve_image_path`], i.e. only for paths that `..` folded out of
/// the workspace; everything else carries the workspace's own spelling.
fn is_inside(path: &Path, root: &Path) -> bool {
    is_inside_with(path, root, CASE_INSENSITIVE_PATHS)
}

/// [`is_inside`] with the case rule injected, so both rules stay testable on
/// every platform.
fn is_inside_with(path: &Path, root: &Path, case_insensitive: bool) -> bool {
    let mut components = path.components();
    for root_component in root.components() {
        let Some(component) = components.next() else {
            return false;
        };
        let same = match (component, root_component) {
            (Component::Normal(left), Component::Normal(right)) => {
                os_str_eq(left, right, case_insensitive)
            }
            // Roots, prefixes and `..` are compared exactly (`..` is rejected
            // before this runs anyway).
            (left, right) => left == right,
        };
        if !same {
            return false;
        }
    }
    true
}

fn os_str_eq(left: &std::ffi::OsStr, right: &std::ffi::OsStr, case_insensitive: bool) -> bool {
    if !case_insensitive {
        return left == right;
    }
    match (left.to_str(), right.to_str()) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        // Not UTF-8: nothing to fold (macOS/Windows paths are representable).
        _ => left == right,
    }
}

/// Resolve a markdown image destination to the path the metadata table is
/// keyed by (and the drawing layer feeds to `ImageStore`).
///
/// Pure and I/O-free: no existence check, no `canonicalize`, no symlink
/// resolution — the boundary here is lexical, and the file is only ever
/// *displayed*. The caller keys its table with this exact function, so the
/// two sides cannot disagree about normalisation.
///
/// Accepted (all normalised: `.`/`..` folded, duplicate separators collapsed):
/// absolute paths, `file://` URLs, and relative paths joined onto `workspace`.
/// Rejected (see [`PathReject`]): empty, control characters, over-long, remote
/// schemes, `~`, relative paths without a workspace, `..` escapes (the
/// containment check folds case on case-insensitive platforms — see
/// [`is_inside`]), and non-image extensions.
pub fn resolve_image_path(workspace: Option<&Path>, raw: &str) -> Result<PathBuf, PathReject> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(PathReject::Empty);
    }
    if raw.chars().any(char::is_control) {
        return Err(PathReject::ControlChar);
    }
    if raw.chars().count() > MAX_IMAGE_PATH_CHARS {
        return Err(PathReject::TooLong);
    }
    if raw.starts_with('~') {
        return Err(PathReject::HomeRelative);
    }

    let path = strip_scheme(raw)?;
    let resolved = if path.is_absolute() {
        normalize(&path)
    } else {
        let workspace = workspace.ok_or(PathReject::NoWorkspace)?;
        let joined = normalize(&workspace.join(&path));
        let root = normalize(workspace);
        if joined.components().any(|c| c == Component::ParentDir)
            || (!root.as_os_str().is_empty() && !is_inside(&joined, &root))
        {
            return Err(PathReject::EscapesWorkspace);
        }
        joined
    };

    if !has_image_extension(&resolved) {
        return Err(PathReject::UnsupportedExtension);
    }
    Ok(resolved)
}

/// Peel a `file:` scheme off the destination; everything else with a scheme is
/// remote. Windows drive letters (`C:\…`) are paths, not schemes.
fn strip_scheme(raw: &str) -> Result<PathBuf, PathReject> {
    let Some(colon) = raw.find(':') else {
        return Ok(PathBuf::from(raw));
    };
    let scheme = &raw[..colon];
    if !is_scheme(scheme) || is_drive_letter(scheme) {
        return Ok(PathBuf::from(raw));
    }
    if !scheme.eq_ignore_ascii_case("file") {
        return Err(PathReject::RemoteUrl);
    }
    let rest = &raw[colon + 1..];
    let Some(after) = rest.strip_prefix("//") else {
        // `file:/abs/path` and `file:rel/path` both stay usable.
        return Ok(PathBuf::from(rest));
    };
    // An authority component: `file:///x` has an empty one, `file://localhost/x`
    // is still this machine, anything else is a remote host.
    let (authority, tail) = match after.find('/') {
        Some(at) => (&after[..at], &after[at..]),
        None => (after, ""),
    };
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return Err(PathReject::RemoteUrl);
    }
    Ok(PathBuf::from(tail))
}

/// RFC 3986 scheme token (`ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`).
fn is_scheme(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// A single ASCII letter followed by `:` is a Windows drive, not a scheme.
fn is_drive_letter(token: &str) -> bool {
    token.len() == 1 && token.chars().all(|c| c.is_ascii_alphabetic())
}

/// Lexical normalisation: drop `.`, fold `..` against the previous component,
/// collapse redundant separators. No filesystem access (see the module docs).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether the path's extension is one the image pipeline may draw.
fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
}

// ============================================================
// Caption
// ============================================================

/// The lead-in marker of a caption line.
pub const CAPTION_MARKER: &str = "▢";

/// The caption of an anchor block, truncated to `max_cols` display columns.
///
/// `▢ alt · 800×600`, or `▢ plot.png · 800×600` when there is no alt text.
/// This is the block's visible fallback (and the text a selection copies), so
/// it must always fit the render width — the anchor's line cannot be wrapped
/// without breaking the row arithmetic.
pub fn anchor_caption(alt: &str, path: &Path, shape: ImageShape, max_cols: u16) -> String {
    let alt = alt.trim();
    let label = if alt.is_empty() {
        path.file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| path.display().to_string())
    } else {
        alt.to_string()
    };
    let caption = format!("{CAPTION_MARKER} {label} · {}×{}", shape.px_w, shape.px_h);
    truncate_to_display_width(&caption, usize::from(max_cols))
}

// ============================================================
// Anchors and spans
// ============================================================

/// An anchor carried by its caption line (the markdown IR payload).
///
/// The IR line stays a single line; the block's cover rows are produced at
/// compose time from `rows` (see the module docs), which keeps every
/// line-count-sensitive rule of the markdown layer untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageAnchor {
    /// Resolved path — the metadata table's key.
    pub path: PathBuf,
    /// Raw alt text (empty when the image has none).
    pub alt: String,
    /// Header pixel dimensions the row count was computed from.
    pub shape: ImageShape,
    /// Box width in cells (the markdown render width).
    pub cols: u16,
    /// Box height in rows, caption row included.
    pub rows: u16,
}

/// One image anchor inside a rendered cell, in display coordinates.
///
/// The side channel's shape mirrors [`super::LinkSpan`]: spans live in a
/// vector parallel to the rendered lines (a line without an anchor has an
/// empty vector), and `column` uses the same coordinate system as
/// `LinkSpan::start` — display columns from the line's left edge, the 2-column
/// cell prefix included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageSpan {
    /// Row of the anchor's first line (an index into the cell's lines).
    pub line: usize,
    /// Display column of the anchor's left edge.
    pub column: u16,
    /// Box width in cells.
    pub cols: u16,
    /// Box height in rows (the caption row is the first of them).
    pub rows: u16,
    /// Resolved path — hand it straight to `ImageStore`.
    pub path: PathBuf,
    /// Raw alt text (may be empty).
    pub alt: String,
    /// Pixel width the box was laid out from.
    pub px_w: u32,
    /// Pixel height the box was laid out from.
    pub px_h: u32,
}

impl ImageSpan {
    /// The anchor's row range within the cell (`line .. line + rows`).
    pub fn rows_range(&self) -> std::ops::Range<usize> {
        self.line..self.line + usize::from(self.rows)
    }
}

// ============================================================
// Compose helpers (shared by both compose paths)
// ============================================================

/// The single space an anchor's cover row carries.
const COVER_CELL: &str = " ";

/// The blank span of an anchor's cover row.
///
/// Both compose paths ([`super::links::compose_lines`] for the cached
/// reference render and `compose_into` for the streaming engine) must produce
/// **identical** rows — the reconcile matrix compares them span by span — so
/// the cover row's content is built here and nowhere else. A plain space with
/// the default style: the row is painted over by the picture, and a space is
/// the smallest thing that still occupies the cell.
pub(crate) fn cover_span() -> ratatui::text::Span<'static> {
    ratatui::text::Span::raw(COVER_CELL)
}

/// Whether a composed row is one of an anchor's cover rows.
///
/// The streaming compose dedups a batch's leading blank line against the
/// previous content ("did the last row already end this blank run?"). A cover
/// row *looks* blank but is not a markdown blank line — swallowing the
/// separator that follows an anchor block would drop a line from the stream.
///
/// The judgment is **structural**, derived from the anchor side channel: the
/// row is inside an anchor's row range but is not the anchor's caption row.
/// It must not be a content test ("is this span a space?"): at narrow widths a
/// hard wrap turns the 2-column cell prefix into ordinary rows whose only span
/// *is* a space, and a content test mistakes them for cover rows — which drops
/// the dedup and adds a blank line to the streaming resting state.
///
/// The scan is bounded by [`MAX_ANCHOR_ROWS`]: an anchor covering `last` starts
/// at most that many rows above it, and its side-channel entry lives on its
/// caption row.
pub(crate) fn row_is_cover_row(images: &[Vec<ImageSpan>], last: usize) -> bool {
    let from = last.saturating_sub(usize::from(MAX_ANCHOR_ROWS));
    images
        .get(from..=last)
        .unwrap_or_default()
        .iter()
        .flatten()
        .any(|span| span.line < last && span.rows_range().contains(&last))
}

/// Build the per-line anchor side channel for the rows described by `tags`.
///
/// `tags` is index-aligned with the wrapped rows and `base` is where those
/// rows start inside the buffer being described, `total_rows` its final row
/// count — the anchor's `line` is then simply its own row index, which is what
/// makes the row arithmetic exact without any index bookkeeping. The returned
/// vector is index-aligned with `tags` (the batch's own rows). An anchor whose
/// rows do not all exist (a truncated batch) is dropped rather than pointed at
/// a half-present box.
pub(crate) fn image_side_channel(
    tags: &[Option<ImageAnchor>],
    total_rows: usize,
    base: usize,
    column: u16,
) -> Vec<Vec<ImageSpan>> {
    let mut images = vec![Vec::new(); tags.len()];
    for (offset, tag) in tags.iter().enumerate() {
        let Some(anchor) = tag else { continue };
        let line = base + offset;
        if line + usize::from(anchor.rows) > total_rows {
            continue;
        }
        images[offset].push(span_for_anchor(anchor, line, column));
    }
    images
}

/// The side-channel entry for an anchor whose caption row is `line`, with
/// `column` the display column the box starts at.
pub(crate) fn span_for_anchor(anchor: &ImageAnchor, line: usize, column: u16) -> ImageSpan {
    ImageSpan {
        line,
        column,
        cols: anchor.cols,
        rows: anchor.rows,
        path: anchor.path.clone(),
        alt: anchor.alt.clone(),
        px_w: anchor.shape.px_w,
        px_h: anchor.shape.px_h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Row count: the shared fit ─────────────────────────────────

    /// The terminal every row count here is laid out for: a 10×20 pixel cell
    /// (an 8×16 font, the fixture the whole module's docs use).
    const CELL: CellPixels = CellPixels::new(10, 20);

    /// The step's acceptance table, asserted row by row: the layout reserves
    /// the rows the encoder draws — no more (no blank band), no less.
    #[test]
    fn anchor_rows_are_the_drawn_footprint() {
        let cases: &[(u32, u32, u16, u16)] = &[
            // (px_w, px_h, width, expected rows) at a 10×20 cell
            (512, 512, 140, 26),   // 52×26 — the user's fixture, no blank rows
            (512, 512, 40, 20),    // 40×20 — width-limited, box filled
            (768, 768, 140, 36),   // 72×36 — the cap, exactly filled
            (1920, 1080, 140, 36), // 128×36
            (1024, 576, 140, 29),  // 103×29
            (16, 16, 140, 1),      // 2×1 — an icon
            (100, 10000, 140, 36), // 1×36 — a sliver, capped by the box
        ];
        for &(px_w, px_h, width, want) in cases {
            assert_eq!(
                anchor_rows(width, ImageShape::new(px_w, px_h), CELL),
                want,
                "{px_w}x{px_h} at width {width}"
            );
        }
    }

    /// The same table over more shapes and widths, with the properties the
    /// contract promises rather than only the numbers.
    #[test]
    fn anchor_rows_stay_inside_the_box_and_the_picture() {
        for (px_w, px_h) in [
            (1u32, 1u32),
            (16, 16),
            (512, 512),
            (800, 500),
            (800, 600),
            (1600, 900),
            (1920, 1080),
            (800, 6000),
            (2000, 100),
            (50, 1),
            (1, 50),
        ] {
            for width in [1u16, 2, 40, 80, 118, 200] {
                let rows = anchor_rows(width, ImageShape::new(px_w, px_h), CELL);
                assert!(
                    (MIN_ANCHOR_ROWS..=MAX_ANCHOR_ROWS).contains(&rows),
                    "{px_w}x{px_h} at width {width} gave {rows}"
                );
                // The rows are exactly the height of the shared fit, and the
                // fit never grows its box or its picture.
                let fitted = crate::render::fit::fit_cells(
                    px_w,
                    px_h,
                    ratatui::layout::Size::new(width, MAX_ANCHOR_ROWS),
                    CELL,
                );
                assert_eq!(rows, fitted.height, "{px_w}x{px_h} at width {width}");
                assert!(fitted.height <= MAX_ANCHOR_ROWS);
                assert!(fitted.width <= width);
            }
        }
    }

    /// The encoder is handed the *box* (`cols × rows`); the layout computes the
    /// rows from the `MAX_ANCHOR_ROWS` box. The two must agree on the height —
    /// and the picture must stay inside the columns — for every shape, width
    /// and cell, not just for the table above.
    ///
    /// This is the invariant `ui::image::encode`'s reconciliation asserts with
    /// real pictures; here it is the arithmetic behind it, over a matrix that
    /// would be too slow to encode.
    #[test]
    fn the_reserved_rows_are_the_height_of_the_box_they_reserve() {
        let shapes: &[(u32, u32)] = &[
            (1, 1),
            (1, 5000),
            (16, 16),
            (50, 1),
            (64, 64),
            (100, 100),
            (200, 150),
            (512, 512),
            (800, 600),
            (1600, 900),
            (1920, 1080),
            (3840, 2160),
            (300, 4000),
            (100, 10000),
            (2000, 100),
            (8000, 100),
        ];
        let cells = [
            CellPixels::new(10, 20),
            CellPixels::new(8, 16),
            CellPixels::new(12, 24),
            CellPixels::new(9, 18),
            CellPixels::new(1, 1),
            CellPixels::new(40, 5),
        ];
        for cell in cells {
            for &(px_w, px_h) in shapes {
                for width in [1u16, 2, 3, 7, 40, 80, 118, 220, 1000] {
                    let shape = ImageShape::new(px_w, px_h);
                    let rows = anchor_rows(width, shape, cell);
                    let boxed = fit_cells(px_w, px_h, Size::new(width, rows), cell);
                    assert_eq!(
                        boxed.height, rows,
                        "{px_w}x{px_h} at width {width} with cell {cell:?}: the \
                         reserved box is {width}x{rows}, the encoder draws {boxed:?}"
                    );
                    assert!(
                        boxed.width <= width,
                        "{px_w}x{px_h} at width {width} with cell {cell:?}: \
                         {boxed:?} is wider than the box"
                    );
                }
            }
        }
    }

    /// A big picture's rows follow the box (both axes), a small picture's rows
    /// follow the picture: the two regimes the old aspect assumption conflated.
    #[test]
    fn anchor_rows_follow_the_box_when_big_and_the_picture_when_small() {
        let big = ImageShape::new(1920, 1080);
        // Wider container, more rows — up to the cap.
        assert!(anchor_rows(40, big, CELL) < anchor_rows(80, big, CELL));
        assert_eq!(anchor_rows(200, big, CELL), MAX_ANCHOR_ROWS);
        // Small pictures are their own size at every width wide enough to hold
        // them (the old formula gave 36 rows for a square one).
        let small = ImageShape::new(512, 512);
        assert_eq!(anchor_rows(140, small, CELL), 26);
        assert_eq!(anchor_rows(500, small, CELL), 26);
        // Narrower than the picture: the width binds, and the rows follow.
        assert_eq!(anchor_rows(40, small, CELL), 20);
        assert_eq!(anchor_rows(20, small, CELL), 10);
        // Both pixel dimensions matter, not their ratio: two 1:1 pictures at
        // the same width and cell reserve five rows and the cap respectively.
        // (A ratio-based row count would answer the same number for both.)
        assert_eq!(anchor_rows(140, ImageShape::new(100, 100), CELL), 5);
        assert_eq!(
            anchor_rows(140, ImageShape::new(1000, 1000), CELL),
            MAX_ANCHOR_ROWS
        );
    }

    #[test]
    fn anchor_rows_is_a_pure_function_of_width_shape_and_cell() {
        let shape = ImageShape::new(800, 600);
        for width in [1u16, 2, 40, 80, 118, 200] {
            let first = anchor_rows(width, shape, CELL);
            for _ in 0..8 {
                assert_eq!(anchor_rows(width, shape, CELL), first, "width {width}");
            }
        }
        // The cell is the unit: taller cells mean fewer rows for the same
        // picture, and it is the same arithmetic the encoder runs.
        assert!(anchor_rows(80, shape, CellPixels::new(10, 40)) < anchor_rows(80, shape, CELL));
        assert_eq!(
            anchor_rows(80, shape, CellPixels::new(10, 20)),
            anchor_rows(80, shape, CELL)
        );
    }

    #[test]
    fn anchor_rows_is_total_for_degenerate_inputs() {
        // A zero width, a degenerate shape and a degenerate cell all answer the
        // minimum instead of panicking or dividing by zero.
        assert_eq!(
            anchor_rows(0, ImageShape::new(800, 600), CELL),
            MIN_ANCHOR_ROWS
        );
        assert_eq!(
            anchor_rows(80, ImageShape::new(0, 100), CELL),
            MIN_ANCHOR_ROWS
        );
        assert_eq!(
            anchor_rows(80, ImageShape::new(100, 0), CELL),
            MIN_ANCHOR_ROWS
        );
        assert_eq!(
            anchor_rows(80, ImageShape::new(0, 0), CELL),
            MIN_ANCHOR_ROWS
        );
        for cell in [
            CellPixels::new(0, 20),
            CellPixels::new(10, 0),
            CellPixels::new(0, 0),
        ] {
            assert_eq!(
                anchor_rows(80, ImageShape::new(800, 600), cell),
                MIN_ANCHOR_ROWS,
                "{cell:?}"
            );
        }
        // Hostile numbers stay inside the bounds.
        assert_eq!(
            anchor_rows(u16::MAX, ImageShape::new(1, u32::MAX), CELL),
            MAX_ANCHOR_ROWS
        );
        assert_eq!(
            anchor_rows(u16::MAX, ImageShape::new(u32::MAX, 1), CELL),
            MIN_ANCHOR_ROWS
        );
    }

    #[test]
    fn options_carry_the_cell_and_degrade_without_one() {
        let off = ImageOpts::off();
        assert!(!off.is_enabled());
        assert_eq!(off.cell_pixels(), CellPixels::new(0, 0));

        let opts = ImageOpts::anchor(
            Some(PathBuf::from("/ws")),
            vec![ImageEntry::new("/ws/a.png", ImageShape::new(800, 600))],
            CELL,
        );
        assert_eq!(opts.mode(), ImageMode::Anchor);
        assert!(opts.is_enabled());
        assert_eq!(opts.cell_pixels(), CELL);
        assert_eq!(
            opts.shape_for(Path::new("/ws/a.png")),
            Some(ImageShape::new(800, 600))
        );
        assert_eq!(opts.shape_for(Path::new("/ws/b.png")), None);
        assert_eq!(opts.workspace(), Some(Path::new("/ws")));
        assert_eq!(
            anchor_rows(80, ImageShape::new(800, 600), opts.cell_pixels()),
            30
        );
        assert_eq!(off.shapes().len(), 0);

        // A degenerate cell is not a layout input: the options fall back to the
        // shared `Off` value (the link path), table and workspace included.
        for cell in [
            CellPixels::new(0, 20),
            CellPixels::new(10, 0),
            CellPixels::new(0, 0),
        ] {
            let degraded = ImageOpts::anchor(
                Some(PathBuf::from("/ws")),
                vec![ImageEntry::new("/ws/a.png", ImageShape::new(800, 600))],
                cell,
            );
            assert_eq!(degraded, *ImageOpts::off(), "{cell:?}");
            assert!(!degraded.is_enabled(), "{cell:?}");
            assert_eq!(degraded.cell_pixels(), CellPixels::new(0, 0), "{cell:?}");
        }
        // An empty table is *not* a reason to degrade: the mode is the caller's
        // decision, the table is just what has been probed so far.
        assert!(
            ImageOpts::anchor(Some(PathBuf::from("/ws")), Vec::new(), CELL).is_enabled(),
            "an empty metadata table still renders anchors once a shape arrives"
        );
    }

    /// The shape gate: a degenerate header (either dimension zero) is not a
    /// shape, and the row count never divides by it.
    #[test]
    fn a_degenerate_shape_is_not_usable() {
        assert!(!ImageShape::new(100, 0).is_usable());
        assert!(!ImageShape::new(0, 100).is_usable());
        assert!(!ImageShape::new(0, 0).is_usable());
        assert!(ImageShape::new(1, 1).is_usable());
        assert!(ImageShape::new(1920, 1080).is_usable());
    }

    // ── Path policy ───────────────────────────────────────────────

    fn ws() -> PathBuf {
        PathBuf::from("/workspace")
    }

    #[test]
    fn relative_paths_resolve_against_the_workspace() {
        let w = Some(ws());
        assert_eq!(
            resolve_image_path(w.as_deref(), "plot.png").unwrap(),
            PathBuf::from("/workspace/plot.png")
        );
        assert_eq!(
            resolve_image_path(w.as_deref(), "./sub/./plot.PNG").unwrap(),
            PathBuf::from("/workspace/sub/plot.PNG")
        );
        assert_eq!(
            resolve_image_path(w.as_deref(), "sub/../plot.jpeg").unwrap(),
            PathBuf::from("/workspace/plot.jpeg")
        );
        assert_eq!(
            resolve_image_path(w.as_deref(), "sub//plot.gif").unwrap(),
            PathBuf::from("/workspace/sub/plot.gif"),
            "duplicate separators collapse"
        );
        // A leading `//` is an absolute path (the link opener reads it the
        // same way: `[x](//sub/a.png)` is not a relative reference here).
        assert_eq!(
            resolve_image_path(w.as_deref(), "//sub/plot.gif").unwrap(),
            PathBuf::from("/sub/plot.gif")
        );
    }

    #[test]
    fn absolute_and_file_paths_are_accepted() {
        assert_eq!(
            resolve_image_path(None, "/tmp/a.png").unwrap(),
            PathBuf::from("/tmp/a.png")
        );
        assert_eq!(
            resolve_image_path(None, "/tmp/./x/../a.webp").unwrap(),
            PathBuf::from("/tmp/a.webp")
        );
        assert_eq!(
            resolve_image_path(None, "file:///tmp/a.png").unwrap(),
            PathBuf::from("/tmp/a.png")
        );
        assert_eq!(
            resolve_image_path(None, "file://localhost/tmp/a.png").unwrap(),
            PathBuf::from("/tmp/a.png")
        );
        assert_eq!(
            resolve_image_path(Some(&ws()), "file:rel/a.png").unwrap(),
            PathBuf::from("/workspace/rel/a.png")
        );
    }

    #[test]
    fn remote_and_exotic_destinations_are_rejected() {
        for raw in [
            "https://example.com/a.png",
            "http://example.com/a.png",
            "data:image/png;base64,AAAA",
            "ftp://host/a.png",
            "file://otherhost/tmp/a.png",
        ] {
            assert_eq!(
                resolve_image_path(None, raw),
                Err(PathReject::RemoteUrl),
                "{raw}"
            );
        }
    }

    #[test]
    fn escaping_paths_are_rejected() {
        let w = Some(ws());
        assert_eq!(
            resolve_image_path(w.as_deref(), "../a.png"),
            Err(PathReject::EscapesWorkspace)
        );
        assert_eq!(
            resolve_image_path(w.as_deref(), "sub/../../a.png"),
            Err(PathReject::EscapesWorkspace)
        );
        assert_eq!(
            resolve_image_path(w.as_deref(), "/workspace/../other/a.png").unwrap(),
            PathBuf::from("/other/a.png"),
            "absolute paths are allowed to leave the workspace"
        );
        // A relative workspace root still catches the escape.
        let rel = Some(PathBuf::from("ws"));
        assert_eq!(
            resolve_image_path(rel.as_deref(), "../a.png"),
            Err(PathReject::EscapesWorkspace)
        );
        assert_eq!(
            resolve_image_path(rel.as_deref(), "sub/a.png").unwrap(),
            PathBuf::from("ws/sub/a.png")
        );
    }

    /// The workspace containment check follows the *platform's* case rule
    /// (review #135, N1): on a case-insensitive filesystem a case-variant
    /// spelling names the same file, and refusing it would cost the user a
    /// picture for nothing. Both rules are asserted here, on every platform, by
    /// injecting the rule — the platform default only picks between them.
    #[test]
    fn containment_folds_case_only_where_the_filesystem_does() {
        let root = Path::new("/Users/me/Project");
        assert!(is_inside_with(
            Path::new("/Users/me/Project/plots/a.png"),
            root,
            true
        ));
        assert!(is_inside_with(
            Path::new("/users/ME/project/plots/a.png"),
            root,
            true
        ));
        assert!(is_inside_with(
            Path::new("/Users/me/Project/plots/a.png"),
            root,
            false
        ));
        assert!(
            !is_inside_with(Path::new("/users/ME/project/plots/a.png"), root, false),
            "a case-sensitive filesystem must keep the byte comparison"
        );
        // The escape itself is refused under both rules: folding case must not
        // turn into a prefix match on a *different* directory.
        for case_insensitive in [true, false] {
            assert!(!is_inside_with(
                Path::new("/Users/me/Project-evil/a.png"),
                root,
                case_insensitive
            ));
            assert!(!is_inside_with(
                Path::new("/Users/me/Other/a.png"),
                root,
                case_insensitive
            ));
            assert!(!is_inside_with(
                Path::new("/Users/me"),
                root,
                case_insensitive
            ));
        }
    }

    #[test]
    fn a_case_variant_spelling_resolves_like_the_platform_allows() {
        // The same rule, seen from the resolver — and the *only* input where it
        // can decide anything is the `..` fold: the containment check runs on
        // the relative branch, whose prefix is the workspace's own spelling
        // (byte for byte), so a variant spelling *below* the root is admitted
        // under both rules. `../WORKSPACE/a.png` under `/workspace` folds back
        // onto a *sibling-looking* path that is the same file on a
        // case-insensitive filesystem and a different one on Linux.
        let w = Some(ws()); // "/workspace"
        let folded = resolve_image_path(w.as_deref(), "../WORKSPACE/a.png");
        if CASE_INSENSITIVE_PATHS {
            assert_eq!(
                folded,
                Ok(PathBuf::from("/WORKSPACE/a.png")),
                "the fold is what admits the case-variant spelling of the root"
            );
        } else {
            assert_eq!(
                folded,
                Err(PathReject::EscapesWorkspace),
                "byte comparison: that spelling is outside the workspace"
            );
        }
        // Below the root the two rules agree, and the spelling is preserved.
        assert_eq!(
            resolve_image_path(w.as_deref(), "SUB/./Plot.PNG"),
            Ok(PathBuf::from("/workspace/SUB/Plot.PNG"))
        );
        // Escaping is rejected either way, whatever the platform.
        assert_eq!(
            resolve_image_path(w.as_deref(), "../../etc/a.png"),
            Err(PathReject::EscapesWorkspace)
        );
        assert_eq!(
            resolve_image_path(w.as_deref(), "../a.png"),
            Err(PathReject::EscapesWorkspace)
        );
    }

    #[test]
    fn non_image_and_malformed_paths_are_rejected() {
        let w = Some(ws());
        assert_eq!(resolve_image_path(w.as_deref(), ""), Err(PathReject::Empty));
        assert_eq!(
            resolve_image_path(w.as_deref(), "   "),
            Err(PathReject::Empty)
        );
        for raw in ["a.txt", "a.md", "plot.png.bak", "noext", "dir/"] {
            assert_eq!(
                resolve_image_path(w.as_deref(), raw),
                Err(PathReject::UnsupportedExtension),
                "{raw}"
            );
        }
        assert_eq!(
            resolve_image_path(w.as_deref(), "a\u{1b}]8;;evil\u{7}.png"),
            Err(PathReject::ControlChar)
        );
        assert_eq!(
            resolve_image_path(w.as_deref(), &"a".repeat(MAX_IMAGE_PATH_CHARS)),
            Err(PathReject::UnsupportedExtension)
        );
        assert_eq!(
            resolve_image_path(
                w.as_deref(),
                &format!("{}.png", "a".repeat(MAX_IMAGE_PATH_CHARS))
            ),
            Err(PathReject::TooLong)
        );
        assert_eq!(
            resolve_image_path(w.as_deref(), "~/plot.png"),
            Err(PathReject::HomeRelative)
        );
        assert_eq!(
            resolve_image_path(None, "plot.png"),
            Err(PathReject::NoWorkspace)
        );
    }

    #[test]
    fn a_short_path_still_has_room_for_its_extension() {
        // The length gate is on characters, not bytes: a CJK directory name
        // must not be rejected early.
        let w = Some(ws());
        let raw = format!("图表/{}.png", "图".repeat(100));
        assert!(resolve_image_path(w.as_deref(), &raw).is_ok());
    }

    // ── Caption ───────────────────────────────────────────────────

    #[test]
    fn caption_uses_alt_then_falls_back_to_the_file_name() {
        let shape = ImageShape::new(800, 600);
        let path = Path::new("/ws/plot.png");
        assert_eq!(
            anchor_caption("Sales by quarter", path, shape, 80),
            "▢ Sales by quarter · 800×600"
        );
        assert_eq!(anchor_caption("", path, shape, 80), "▢ plot.png · 800×600");
        assert_eq!(
            anchor_caption("   ", path, shape, 80),
            "▢ plot.png · 800×600"
        );
    }

    #[test]
    fn caption_is_truncated_to_the_render_width() {
        let shape = ImageShape::new(1920, 1080);
        let path = Path::new("/ws/plot.png");
        let wide_alt = "一二三四五六七八九十一二三四五六七八九十";
        let caption = anchor_caption(wide_alt, path, shape, 20);
        assert!(
            unicode_width::UnicodeWidthStr::width(caption.as_str()) <= 20,
            "{caption:?}"
        );
        assert!(caption.starts_with("▢ 一二三"), "{caption:?}");
        // A zero-width budget degrades to the empty string, never to a panic.
        assert_eq!(anchor_caption(wide_alt, path, shape, 0), "");
    }

    #[test]
    fn caption_falls_back_to_the_full_path_when_there_is_no_file_name() {
        let shape = ImageShape::new(10, 10);
        // The root has no file name — the path itself is the label.
        assert_eq!(anchor_caption("", Path::new("/"), shape, 80), "▢ / · 10×10");
    }
}
