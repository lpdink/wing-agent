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
//! # The row count is a pure function
//!
//! ```text
//! rows = clamp(round(W / (CELL_ASPECT × R)), MIN_ANCHOR_ROWS, MAX_ANCHOR_ROWS)
//! R    = px_w / px_h
//! ```
//!
//! `W` is the markdown render width (the cell width minus its 2-column
//! prefix — the anchor's [`ImageSpan::cols`]). The terminal's graphics
//! capabilities, its cell-pixel size and its pixel queries are deliberately
//! **not** inputs: the row count feeds `CachedCell`'s height cache and the
//! streaming engine's "resting state == reference render" invariant, both of
//! which a capability-dependent height would break. In-box scaling (what the
//! picture actually occupies inside the box) is the drawing layer's business.
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

use super::types::truncate_to_display_width;

// ============================================================
// Layout constants (the pure row-count function's inputs)
// ============================================================

/// Character-cell aspect assumption: `cell height / cell width`.
///
/// Terminal fonts are close to 8×16 px, i.e. a cell is twice as tall as it is
/// wide — the same assumption `ratatui-image`'s `CellPixels` defaults to. It
/// only has to be close: the box's row count is what the layout depends on,
/// and in-box scaling (which absorbs any font error) belongs to the drawing
/// layer.
pub const CELL_ASPECT: f64 = 2.0;

/// Fewest rows an anchor may reserve (a single caption row).
pub const MIN_ANCHOR_ROWS: u16 = 1;

/// Most rows an anchor may reserve.
///
/// About one screen on a 40-row terminal: without it an 800×6000 sliver would
/// reserve `W / (2 × 0.133) ≈ 442` rows at 118 columns and swallow the
/// viewport. The cap distorts the box's aspect for very tall images; the
/// drawing layer fits the picture inside the box (letterboxing), so the
/// distortion costs margin, not correctness.
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
/// size and mtime): layout needs the aspect ratio and nothing else. The chat
/// view builds one from a probe result with
/// `ImageShape::new(meta.px_w, meta.px_h)`.
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

    /// `px_w / px_h`, with a finite `0.0` for degenerate headers.
    pub fn aspect_ratio(&self) -> f64 {
        if self.px_h == 0 {
            return 0.0;
        }
        f64::from(self.px_w) / f64::from(self.px_h)
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

/// Image rendering options: the mode, the workspace root, and the metadata.
///
/// Owned (not a borrowed view) because [`super::stream::StreamingRender`]
/// outlives a single render call and has to notice when the metadata changes
/// (a late-arriving probe changes the row count, which must force a rebuild).
/// Equality is structural, which is exactly how that change is detected.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageOpts {
    mode: ImageMode,
    workspace: Option<PathBuf>,
    shapes: Vec<ImageEntry>,
}

/// The shared "images are off" options — what [`super::RenderOpts::new`]
/// borrows, so a render with default options pays nothing for images.
static NO_IMAGES: ImageOpts = ImageOpts {
    mode: ImageMode::Off,
    workspace: None,
    shapes: Vec::new(),
};

impl ImageOpts {
    /// The shared `Off` options (no anchors, no metadata lookups).
    pub fn off() -> &'static ImageOpts {
        &NO_IMAGES
    }

    /// `Anchor` options over `shapes`, resolving relative paths against
    /// `workspace`.
    ///
    /// `workspace = None` means "relative paths are rejected" — the table's
    /// keys are then absolute paths only.
    ///
    /// Table contract: every key is the output of [`resolve_image_path`] for
    /// the destination as written in the markdown, and **a path appears at
    /// most once**. A duplicate is a caller bug (a directory scan that
    /// re-registers the same file): the lookup answers with the first row, so
    /// the result stays deterministic, but the contract is "one row per path".
    /// Keep the table scoped to what the view is about to draw rather than to
    /// "every image in the workspace" — see [`shape_for`](Self::shape_for).
    pub fn anchor(workspace: Option<PathBuf>, shapes: Vec<ImageEntry>) -> Self {
        Self {
            mode: ImageMode::Anchor,
            workspace,
            shapes,
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
// Row count — the pure function
// ============================================================

/// Rows an image anchor reserves at markdown width `width`.
///
/// Pure: the inputs are the container width, the pixel aspect ratio and the
/// constants above. Nothing about the terminal, the file or the moment in
/// time is an input, so the value is stable across frames, capabilities and
/// profiling, and repeated calls with the same inputs are identical.
///
/// Total: a zero width, a degenerate shape (`px_h == 0`) or a non-finite
/// computation answers [`MIN_ANCHOR_ROWS`] rather than panicking. The parse
/// layer rejects those cases before they can become an anchor.
pub fn anchor_rows(width: u16, shape: ImageShape) -> u16 {
    if width == 0 || !shape.is_usable() {
        return MIN_ANCHOR_ROWS;
    }
    let raw = (f64::from(width) / (CELL_ASPECT * shape.aspect_ratio())).round();
    if !raw.is_finite() {
        return MIN_ANCHOR_ROWS;
    }
    raw.clamp(f64::from(MIN_ANCHOR_ROWS), f64::from(MAX_ANCHOR_ROWS)) as u16
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
/// macOS and Windows default to case-insensitive filesystems; Linux does not.
/// The path itself is never rewritten — this only decides which spellings count
/// as *the same* path, and only here: a case-variant spelling still names the
/// file the filesystem resolves it to, so refusing it would be a false
/// "escapes the workspace" that costs the user a picture.
const CASE_INSENSITIVE_PATHS: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// [`Path::starts_with`] under the platform's case rule (see
/// [`CASE_INSENSITIVE_PATHS`]) — still purely lexical, no `stat` and no
/// `canonicalize`, so the module's zero-I/O contract is untouched.
///
/// Only ASCII case is folded (Unicode folding would need a table this layer has
/// no business carrying), and symlinks are still compared by their own path:
/// the boundary here is lexical by design.
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

    // ── Row count: the pure function ──────────────────────────────

    /// The table from the design doc, asserted row by row.
    #[test]
    fn anchor_rows_table() {
        let cases: &[(u32, u32, u16)] = &[
            // (px_w, px_h, expected rows at width 80)
            (100, 100, 36),  // R=1.0   raw 40    → MAX
            (800, 500, 25),  // R=1.6   raw 25
            (800, 600, 30),  // R=1.333 raw 30
            (1600, 900, 23), // R=1.778 raw 22.5  → half away from zero
            (100, 200, 36),  // R=0.5   raw 80    → MAX
            (50, 1000, 36),  // R=0.05  raw 800   → MAX
            (1000, 20, 1),   // R=50    raw 0.8   → MIN
            (128, 256, 36),  // R=0.5   raw 80    → MAX
            (800, 6000, 36), // R=0.133 raw 300   → MAX
            (2000, 100, 2),  // R=20    raw 2
            (400, 100, 10),  // R=4     raw 10
        ];
        for &(px_w, px_h, want) in cases {
            assert_eq!(
                anchor_rows(80, ImageShape::new(px_w, px_h)),
                want,
                "{px_w}x{px_h} at width 80"
            );
        }
    }

    #[test]
    fn anchor_rows_is_a_pure_function_of_width_and_shape() {
        let shape = ImageShape::new(800, 600);
        for width in [1u16, 2, 40, 80, 118, 200] {
            let first = anchor_rows(width, shape);
            for _ in 0..8 {
                assert_eq!(anchor_rows(width, shape), first, "width {width}");
            }
        }
        // Same shape, different container: the row count follows the width.
        assert!(anchor_rows(40, shape) < anchor_rows(80, shape));
        assert_eq!(anchor_rows(0, shape), MIN_ANCHOR_ROWS);
    }

    #[test]
    fn anchor_rows_clamps_at_both_ends_and_stays_total() {
        // Extremes: 1:50 and 50:1 both land inside the bounds.
        assert_eq!(anchor_rows(80, ImageShape::new(1, 50)), MAX_ANCHOR_ROWS);
        assert_eq!(anchor_rows(80, ImageShape::new(50, 1)), MIN_ANCHOR_ROWS);
        assert_eq!(anchor_rows(80, ImageShape::new(4, 4)), MAX_ANCHOR_ROWS);
        // Width 1 with a square image rounds to a single row.
        assert_eq!(anchor_rows(1, ImageShape::new(100, 100)), 1);
        // Degenerate and hostile shapes stay inside the bounds.
        assert_eq!(anchor_rows(80, ImageShape::new(0, 100)), MIN_ANCHOR_ROWS);
        assert_eq!(anchor_rows(80, ImageShape::new(100, 0)), MIN_ANCHOR_ROWS);
        assert_eq!(anchor_rows(80, ImageShape::new(0, 0)), MIN_ANCHOR_ROWS);
        assert_eq!(
            anchor_rows(u16::MAX, ImageShape::new(1, u32::MAX)),
            MAX_ANCHOR_ROWS
        );
    }

    #[test]
    fn anchor_rows_only_depends_on_width_and_shape() {
        // The same (width, shape) pair yields the same rows whatever else is
        // configured: the row count is not a function of the mode, the
        // workspace, or the metadata table's other entries.
        let off = ImageOpts::off();
        assert!(!off.is_enabled());
        let opts = ImageOpts::anchor(
            Some(PathBuf::from("/ws")),
            vec![ImageEntry::new("/ws/a.png", ImageShape::new(800, 600))],
        );
        assert_eq!(opts.mode(), ImageMode::Anchor);
        assert_eq!(
            opts.shape_for(Path::new("/ws/a.png")),
            Some(ImageShape::new(800, 600))
        );
        assert_eq!(opts.shape_for(Path::new("/ws/b.png")), None);
        assert_eq!(opts.workspace(), Some(Path::new("/ws")));
        assert_eq!(anchor_rows(80, ImageShape::new(800, 600)), 30);
        assert_eq!(off.shapes().len(), 0);
    }

    #[test]
    fn shape_aspect_ratio_is_finite_for_degenerate_headers() {
        assert!((ImageShape::new(320, 200).aspect_ratio() - 1.6).abs() < 1e-9);
        assert_eq!(ImageShape::new(100, 0).aspect_ratio(), 0.0);
        assert!(ImageShape::new(100, 0).aspect_ratio().is_finite());
        assert!(ImageShape::new(0, 100).aspect_ratio().is_finite());
        assert!(!ImageShape::new(100, 0).is_usable());
        assert!(!ImageShape::new(0, 100).is_usable());
        assert!(ImageShape::new(1, 1).is_usable());
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
        // The same rule, seen from the resolver: the path is never rewritten,
        // only admitted.
        let w = Some(ws()); // "/workspace"
        let variant = resolve_image_path(w.as_deref(), "SUB/./Plot.PNG");
        match variant {
            Ok(path) => {
                assert!(CASE_INSENSITIVE_PATHS, "only folded on those platforms");
                assert_eq!(path, PathBuf::from("/workspace/SUB/Plot.PNG"));
            }
            Err(reason) => {
                assert_eq!(reason, PathReject::EscapesWorkspace);
                assert!(!CASE_INSENSITIVE_PATHS, "byte comparison on Linux");
            }
        }
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
