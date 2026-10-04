//! The picture path end to end: markdown anchor → chat view frame → an actual
//! drawn frame in a `TestBackend`.
//!
//! Everything here is headless: the terminal capability is **injected**
//! (`ImageSupport::from_parts`), the pictures are real PNGs in a temp
//! directory, and the worker is driven the way the event loop drives it (draw,
//! sleep, draw) — no TTY, no escape sequence reaching a terminal, no timing
//! dependency on one.
//!
//! What is pinned, in the order the task asks for it:
//!
//! 1. the picture lands in the **right screen rect** and follows the scroll
//!    offset;
//! 2. `off` / incapable / probe-failed / probe-pending frames are **cell for
//!    cell** the images-off baseline (the two-tier rule, mechanically);
//! 3. an overlay (the toast) suppresses a picture wholesale, and a picture
//!    never reaches the gutter, the composer or the status bar;
//! 4. a drag selection keeps the frame text-only and the picture comes back
//!    after the release;
//! 5. a full redraw (`terminal.clear()` / resize / focus regain) invalidates
//!    the store — the old protocol is never reused, and a fresh encode paints;
//! 6. the streaming engine and the reference render lay the same picture out
//!    identically (they share one `CellContext`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::time::Instant;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Style;

use super::support::draw;
use super::support::test_terminal;
use crate::app::App;
use crate::app::image_freshness_tick;
use crate::app::images::FRESHNESS_INTERVAL;
use crate::app::images::Images;
use crate::config::AppConfig;
use crate::config::rendering::ImagesMode;
use crate::render::markdown::CellPixels as LayoutCellPixels;
use crate::render::markdown::ImageShape;
use crate::render::markdown::MAX_ANCHOR_ROWS;
use crate::render::markdown::anchor_rows;
use crate::ui::chat_view::ChatCell;
use crate::ui::chat_view::FrameImage;
use crate::ui::image::CellPixels;
use crate::ui::image::DEFAULT_CACHE_BYTES;
use crate::ui::image::DEFAULT_CACHE_ENTRIES;
use crate::ui::image::DEFAULT_FILE_BYTES;
use crate::ui::image::ImageProtocol;
use crate::ui::image::ImageSupport;
use crate::ui::image::MAX_FAILED_ENTRIES;
use crate::ui::image::MAX_META_ENTRIES;
use crate::ui::scrollbar;
use crate::ui::toast::Toast;

/// The kitty placeholder symbol every painted cell of an image carries.
const PLACEHOLDER: char = '\u{10EEEE}';

const CELL: CellPixels = CellPixels::new(10, 20);

/// The same cell, as the render layer's mirror type — what `anchor_rows`
/// takes (the two are the same numbers by construction: `Images` hands the
/// layout the store's own `ImageSupport` cell).
const LAYOUT_CELL: LayoutCellPixels = LayoutCellPixels::new(10, 20);

fn kitty() -> ImageSupport {
    ImageSupport::from_parts(ImageProtocol::Kitty, CELL, false)
}

// ── fixtures ────────────────────────────────────────────────────

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// Self-cleaning temp directory (the crate has no `tempfile` dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wing-app-images-{}-{tag}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_png(path: &Path, px_w: u32, px_h: u32) {
    let image = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(px_w, px_h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    }));
    image
        .save_with_format(path, image::ImageFormat::Png)
        .expect("write the fixture");
}

/// Write a PNG and cut it short: the header (and so the pixel dimensions) reads
/// fine, the pixels do not — metadata known, picture forever unavailable.
fn write_truncated_png(path: &Path, px_w: u32, px_h: u32) {
    write_png(path, px_w, px_h);
    let mut bytes = fs::read(path).expect("read the fixture");
    bytes.truncate(50);
    fs::write(path, bytes).expect("truncate the fixture");
}

/// Write a flat PNG of the given size and colour.
///
/// Two of these with **different** colours and the same dimensions are the
/// fixture pair of `a_same_size_rewrite_is_seen`; see [`pad_to`] for how their
/// lengths are made to match.
fn write_flat_png(path: &Path, px_w: u32, px_h: u32, rgb: [u8; 3]) {
    let image = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(px_w, px_h, |_, _| {
        image::Rgb(rgb)
    }));
    image
        .save_with_format(path, image::ImageFormat::Png)
        .expect("write the fixture");
}

/// Grow `path` to exactly `len` bytes by appending padding after the PNG's
/// `IEND` chunk.
///
/// Every decoder on this path stops reading at `IEND` — the header probe and
/// the full decode both do (verified: the fixture decodes to the same
/// dimensions and pixels with 4 KiB of trailing bytes), and real-world files
/// carry appended metadata the same way. That is what lets this test build two
/// *different* pictures of the very same byte length, which is the shape of a
/// rewrite the byte count cannot reveal.
fn pad_to(path: &Path, len: u64) {
    let mut bytes = fs::read(path).expect("read the fixture");
    let current = bytes.len() as u64;
    assert!(
        current < len,
        "the fixture already exceeds its target length ({current} >= {len})"
    );
    bytes.resize(len as usize, b'Z');
    fs::write(path, bytes).expect("pad the fixture");
}

/// The kitty transmit sequence the picture's anchor cell carries.
///
/// The payload rides in the box's top-left cell, once, and it embeds the
/// **image id** — which `ui/image/encode.rs` mints fresh for every encoding
/// (a process-local counter, deterministic). A different sequence at the same
/// cell is therefore proof that a *new* image was encoded and sent, rather than
/// the old protocol being painted again.
fn transmit_sequence(buf: &Buffer, area: Rect) -> String {
    buf[(area.x, area.y)].symbol().to_string()
}

/// An app whose picture lane is wired to an injected capability, with no
/// welcome header (so the band's first row is the first content row).
fn app_with_images(mode: ImagesMode, support: ImageSupport, workspace: Option<&Path>) -> App {
    let mut app = App::with_images(
        "test-session".into(),
        AppConfig::default(),
        workspace.map(|path| path.to_string_lossy().into_owned()),
        Images::new(mode, support, None),
    );
    app.clear_welcome();
    app
}

/// Draw once — the frame assertions read `term.backend().buffer()`.
fn frame(app: &mut App, term: &mut Terminal<TestBackend>) -> Buffer {
    draw(app, term);
    term.backend().buffer().clone()
}

/// Draw until `done` holds on the frame just drawn, and hand that frame's
/// buffer back.
///
/// The store's worker is a real thread and its results only become visible
/// through `poll()` at the start of a draw, so waiting *is* drawing in a loop
/// (the same shape the event loop has, with the waker replaced by this sleep).
///
/// **Assert on the returned buffer, never on a frame drawn afterwards.** The
/// predicate and the buffer describe the same frame; a later frame re-plans
/// this frame's requests (`Images::paint` → `request`), so a picture that was
/// drawn a moment ago can be `Pending` in it again — an encode that just landed
/// can evict the cache entry, and a probe that just landed can add an anchor
/// whose encode has not started. That is not the thing under test, and it is
/// exactly what a loaded CI machine makes visible.
fn draw_until(
    app: &mut App,
    term: &mut Terminal<TestBackend>,
    what: &str,
    mut done: impl FnMut(&App, &Buffer) -> bool,
) -> Buffer {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let buf = frame(app, term);
        if done(app, &buf) {
            return buf;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn placeholder_cells(buf: &Buffer) -> Vec<(u16, u16)> {
    let mut cells = Vec::new();
    for y in buf.area.y..buf.area.bottom() {
        for x in buf.area.x..buf.area.right() {
            if buf[(x, y)].symbol().contains(PLACEHOLDER) {
                cells.push((x, y));
            }
        }
    }
    cells
}

fn has_placeholder(buf: &Buffer) -> bool {
    !placeholder_cells(buf).is_empty()
}

/// The bounding box of the painted cells — the rect the picture actually
/// covered.
fn placeholder_rect(buf: &Buffer) -> Option<Rect> {
    let cells = placeholder_cells(buf);
    let (first_x, first_y) = *cells.first()?;
    let mut rect = Rect::new(first_x, first_y, 1, 1);
    for (x, y) in cells {
        let right = rect.right().max(x + 1);
        let bottom = rect.bottom().max(y + 1);
        rect.x = rect.x.min(x);
        rect.y = rect.y.min(y);
        rect.width = right - rect.x;
        rect.height = bottom - rect.y;
    }
    Some(rect)
}

/// First screen row whose text contains `needle`.
fn row_with(buf: &Buffer, needle: &str) -> Option<u16> {
    (buf.area.y..buf.area.bottom()).find(|&y| {
        (buf.area.x..buf.area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .contains(needle)
    })
}

fn buffer_text(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in buf.area.y..buf.area.bottom() {
        for x in buf.area.x..buf.area.right() {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

// ── 1. the paintable path ───────────────────────────────────────

#[test]
fn a_ready_image_is_painted_into_its_anchor_box() {
    let dir = TempDir::new("ready");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    let content = app.chat.geometry().area;
    let band = app.geometry.chat_band();
    let frame = app.chat.frame_images().first().cloned().expect("recorded");
    let rows = anchor_rows(content.width - 2, ImageShape::new(800, 600), LAYOUT_CELL);

    // The request: the cell's prefix column, the box at the markdown width,
    // and the whole box as the encode target.
    assert_eq!(frame.path, plot);
    assert_eq!(frame.offset, (2, 0));
    assert_eq!(frame.target, Size::new(content.width - 2, rows));
    assert_eq!(frame.area.x, content.x + 2);
    assert_eq!(frame.area.width, content.width - 2);
    assert_eq!(
        frame.area.y, content.y,
        "the anchor opens the first content row"
    );

    // The paint: exactly that rect, and nothing anywhere else.
    let painted = placeholder_rect(term.backend().buffer()).expect("placeholders");
    assert_eq!(painted, frame.area);
    assert_eq!(app.images.stats().expect("lane").cached, 1);
    for (x, _) in placeholder_cells(term.backend().buffer()) {
        assert!(
            x < band.right() - 1,
            "the picture reached the scrollbar gutter at column {x}"
        );
    }
}

/// The acceptance case behind this contract, at the frame level: a 512×512
/// picture at a 10×20 cell is **26** rows (52×26 cells), not the 36 rows an
/// aspect assumption reserved — the box *is* the picture, so the markdown line
/// after it starts right below it instead of ten rows lower.
#[test]
fn a_small_picture_reserves_its_own_rows_and_leaves_no_blank_band() {
    let dir = TempDir::new("small-box");
    let plot = dir.file("plot.png");
    write_png(&plot, 512, 512);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(
        "![plot](plot.png)\n\nrow after the picture".into(),
    ));
    let mut term = test_terminal(142, 40);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    let buf = term.backend().buffer().clone();
    let content = app.chat.geometry().area;
    let cols = content.width - 2;
    let frame = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(
        frame.target,
        Size::new(cols, 26),
        "512x512 at a 10x20 cell is 52x26 cells, not a 36-row reservation"
    );
    assert_eq!(
        frame.target.height,
        anchor_rows(cols, ImageShape::new(512, 512), LAYOUT_CELL),
        "the reserved rows are the shared fit"
    );
    assert!(
        frame.target.height < MAX_ANCHOR_ROWS,
        "a picture smaller than the box must not reserve the cap"
    );

    // The picture covers the box's rows, top to bottom — the reservation is
    // the drawing. (The box is the full render width; the picture is 52 of
    // those columns, left-aligned — the column axis is not part of this
    // contract.)
    let painted = placeholder_rect(&buf).expect("placeholders");
    assert_eq!(
        painted.height, frame.area.height,
        "the picture fills the box rows"
    );
    assert_eq!(painted.height, frame.target.height);
    assert_eq!(painted.width, 52, "512 px at a 10 px cell is 52 columns");
    assert_eq!((painted.x, painted.y), (frame.area.x, frame.area.y));

    // …and the next markdown line follows within the renderer's own block
    // separator (one blank row). Under the old contract this gap was the ten
    // rows the picture did not fill.
    let text_row = row_with(&buf, "row after the picture").expect("the following text");
    let gap = text_row.saturating_sub(painted.bottom());
    assert!(gap <= 2, "blank band under the picture: {gap} rows");
}

#[test]
fn scrolling_moves_the_picture_with_its_box() {
    let dir = TempDir::new("scroll");

    let plot = dir.file("plot.png");
    write_png(&plot, 600, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    // A tall trailing message makes the band scrollable, then read five rows
    // down from the top of the content.
    app.chat.push(ChatCell::AssistantMessage(
        (0..30)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    frame(&mut app, &mut term);
    let band = app.geometry.chat_band();
    app.chat.scroll_to(5, band.height as usize);
    let buf = frame(&mut app, &mut term);
    assert_eq!(app.chat.scroll_position(), 5);

    let frame_image = app.chat.frame_images().first().cloned().expect("recorded");
    let rows = frame_image.target.height;
    assert_eq!(
        frame_image.offset,
        (2, -5),
        "the box moved up with the text"
    );
    assert_eq!(frame_image.area.y, band.y, "clipped to the band's top");
    assert_eq!(
        frame_image.area.height,
        rows - 5,
        "the visible part is the box minus the rows scrolled away"
    );
    assert_eq!(
        placeholder_rect(&buf).expect("placeholders"),
        frame_image.area
    );
}

/// A streaming cell anchors with the app's options — the same table the
/// non-streaming path gets (see `CachedCell::sync_image_opts`).
#[test]
fn a_streaming_cell_anchors_with_the_app_options() {
    let dir = TempDir::new("streaming");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(String::new()));
    app.chat.append_to_last_assistant("![plot](plot.png)");
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    let content = app.chat.geometry().area;
    let frame = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(
        frame.target,
        Size::new(
            content.width - 2,
            anchor_rows(content.width - 2, ImageShape::new(800, 600), LAYOUT_CELL)
        ),
        "the streaming engine must lay out the same box as the reference render"
    );
    assert!(placeholder_rect(term.backend().buffer()).is_some());
}

/// Two cells with the same picture: two boxes, one encode.
#[test]
fn two_cells_with_the_same_image_encode_once() {
    let dir = TempDir::new("same-image");
    let plot = dir.file("plot.png");
    write_png(&plot, 400, 200);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(
        "first\n\n![plot](plot.png)".into(),
    ));
    app.chat.push(ChatCell::AssistantMessage(
        "second\n\n![plot](plot.png)".into(),
    ));
    let mut term = test_terminal(60, 30);
    draw_until(&mut app, &mut term, "both pictures", |_, buf| {
        placeholder_cells(buf).len() > 2
    });
    let frames = app.chat.frame_images().to_vec();
    assert_eq!(frames.len(), 2, "one request per box");
    assert!(frames[0].area.y < frames[1].area.y, "in anchor order");
    let stats = app.images.stats().expect("lane");
    assert_eq!(stats.cached, 1, "the same (path, target) encodes once");
    assert_eq!(stats.in_flight, 0, "and nothing is queued twice");
}

/// A picture narrower than its box (the row cap makes `Resize::Fit` letterbox
/// it) owns the whole box: the part of the caption the picture does not cover
/// is cleared, so no half-caption peeks out beside the image.
#[test]
fn the_caption_tail_outside_the_picture_is_cleared() {
    let dir = TempDir::new("caption-tail");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    // A caption long enough to run past the letterboxed picture.
    let alt = "x".repeat(100);
    app.chat
        .push(ChatCell::AssistantMessage(format!("![{alt}](plot.png)")));
    let mut term = test_terminal(122, 48);

    // The anchor renders before the picture does (the encode is requested by
    // the very paint pass that finds it missing): the first frame with a
    // recorded box and no placeholder shows the caption in full.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (caption, frame) = loop {
        let buf = frame(&mut app, &mut term);
        if let Some(recorded) = app.chat.frame_images().first().cloned()
            && !has_placeholder(&buf)
        {
            let row = recorded.area.y;
            let text: String = (recorded.area.x..recorded.area.right())
                .map(|x| buf[(x, row)].symbol())
                .collect();
            break (text, recorded);
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the anchor"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert!(caption.contains('▢'), "no caption: {caption:?}");
    assert!(
        caption.trim_end().chars().count() > 96,
        "the fixture's caption must run past the picture: {caption:?}"
    );

    // Once the picture lands, it owns the whole box: the tail is blanked.
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    let buf = term.backend().buffer();
    let covered = placeholder_rect(buf).expect("placeholders");
    assert_eq!(covered.x, frame.area.x);
    assert!(
        covered.right() < frame.area.right(),
        "the fixture must letterbox the picture ({} of {} columns)",
        covered.width,
        frame.area.width
    );
    for x in covered.right()..frame.area.right() {
        assert_eq!(
            buf[(x, frame.area.y)].symbol(),
            " ",
            "the caption's tail still shows at column {x}"
        );
    }
}

/// A cell that also holds an **over-wide line** still draws its picture.
///
/// The widget renders such a cell through a wrapping `Paragraph` whose scroll is
/// measured in rows, so the anchor's screen row is the *wrapped* one — the
/// cell-level row-arithmetic flag (`rows_exact`) is the link layer's
/// precondition, not the picture's. The picture must land exactly on the caption
/// row, which the paint pass re-checks before covering it.
#[test]
fn a_picture_after_an_over_wide_line_lands_on_its_wrapped_row() {
    let dir = TempDir::new("wrapped-row");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(format!(
        "```\n{}\n```\n\n![plot](plot.png)",
        "a".repeat(200)
    )));
    // Tall enough that the whole cell (code line + anchor box) is on screen.
    let mut term = test_terminal(60, 120);

    // The caption renders before the picture does (the encode is requested by the
    // paint pass that finds it missing): record where it really is.
    let deadline = Instant::now() + Duration::from_secs(10);
    let caption_row = loop {
        let buf = frame(&mut app, &mut term);
        if let Some(row) = row_with(&buf, "▢")
            && !has_placeholder(&buf)
        {
            break row;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the caption"
        );
        std::thread::sleep(Duration::from_millis(2));
    };

    // The fixture is the inexact shape: the code line wraps into extra rows, so
    // the cell's line index is not the screen row — and the anchor's own row is
    // still known (`CellFrame::image_row`).
    let (anchor_line, anchor_row) = {
        let (palette, layout) = (app.palette(), app.config.layout.clone());
        let width = scrollbar::content_area(app.geometry.chat_band()).width;
        let ctx = crate::render::renderable::CellContext {
            palette: &palette,
            thinking_mode: app.config.rendering.thinking,
            thinking_expanded: app.chat.thinking_expansion(),
            layout: &layout,
            images: app.images.opts(),
        };
        let frame = app.chat.cells[0].compute_cell_frame(width, &ctx);
        assert!(!frame.rows_exact, "the fixture must be the inexact shape");
        let span = frame
            .images
            .iter()
            .flatten()
            .next()
            .cloned()
            .expect("the anchor exists");
        (span.line, frame.image_row(span.line))
    };
    assert!(
        anchor_row > anchor_line,
        "the over-wide line above the anchor must push it down: {anchor_row} vs {anchor_line}"
    );
    let content = app.chat.geometry().area;
    assert_eq!(
        i32::from(content.y) + i32::try_from(anchor_row).expect("fits"),
        i32::from(caption_row),
        "the anchor's wrapped row is where the caption renders"
    );

    // …and the picture covers exactly that row — not the line index, which would
    // be inside the code block above.
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    let recorded = app.chat.frame_images().first().cloned().expect("recorded");
    let painted = placeholder_rect(term.backend().buffer()).expect("placeholders");
    assert_eq!(i32::from(painted.y), i32::from(caption_row));
    assert_eq!(painted.x, content.x + 2);
    assert_eq!(
        recorded.offset,
        (2, i16::try_from(anchor_row).expect("fits"))
    );
}

/// The same cell shape with the over-wide line *below* the anchor: the anchor's
/// own rows are exact, so nothing may suppress it either.
#[test]
fn a_picture_before_an_over_wide_line_is_drawn_too() {
    let dir = TempDir::new("wrapped-row-after");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(format!(
        "![plot](plot.png)\n\n```\n{}\n```",
        "a".repeat(200)
    )));
    let mut term = test_terminal(60, 120);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    let content = app.chat.geometry().area;
    let recorded = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(recorded.offset, (2, 0), "the anchor opens the cell");
    assert_eq!(
        placeholder_rect(term.backend().buffer()).expect("placeholders"),
        recorded.area
    );
    assert_eq!(recorded.area.x, content.x + 2);
}

/// The paint pass covers a box only when its top-left cell really holds the
/// caption: the check that keeps a drifted row metric from painting a picture
/// over someone else's text.
#[test]
fn a_box_that_does_not_start_on_its_caption_is_not_painted() {
    let dir = TempDir::new("caption-check");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut images = Images::new(ImagesMode::Auto, kitty(), None);
    assert!(images.set_workspace(Some(dir.path().to_str().expect("utf-8 temp path"))));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !images.sync(std::slice::from_ref(&plot)) {
        assert!(Instant::now() < deadline, "timed out waiting for the probe");
        std::thread::sleep(Duration::from_millis(2));
    }
    let clip = Rect::new(0, 0, 40, 10);
    let image = FrameImage {
        area: Rect::new(2, 0, 38, 5),
        offset: (2, 0),
        target: Size::new(38, 5),
        path: plot.clone(),
    };

    // Over the caption, once the encode lands, the picture is painted.
    let mut right = Buffer::empty(clip);
    right.set_string(2, 0, "▢ plot · 800×600", Style::default());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        images.sync(std::slice::from_ref(&plot));
        let mut attempt = right.clone();
        images.paint(std::slice::from_ref(&image), clip, None, &mut attempt);
        if has_placeholder(&attempt) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the encode"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // The very same request — the encode is warm now — over a box whose origin
    // is ordinary text leaves the buffer alone.
    let mut wrong = Buffer::empty(clip);
    wrong.set_string(2, 0, "not a caption at all", Style::default());
    let untouched = wrong.clone();
    images.paint(std::slice::from_ref(&image), clip, None, &mut wrong);
    assert_eq!(wrong, untouched, "the picture must not cover plain text");

    // …and neither does a box one row off (its origin is a cover row, not the
    // caption): a drifted row metric must not turn into a picture over the
    // caption's neighbours.
    let mut shifted = right.clone();
    images.paint(
        std::slice::from_ref(&FrameImage {
            // The box the widget would record if it thought the anchor sat one
            // row lower: area and offset move together.
            area: Rect::new(2, 1, 38, 4),
            offset: (2, 1),
            ..image.clone()
        }),
        clip,
        None,
        &mut shifted,
    );
    assert_eq!(
        shifted, right,
        "a box that starts below its caption must not be painted"
    );
}

/// A `--dump`-style config keeps its meaning across a parse: `Off` is the only
/// way to turn pictures off, and a value that fell back to `auto` would turn
/// them back on silently.
#[test]
fn a_rendering_config_round_trips_through_its_dump() {
    let config = crate::config::AppConfig {
        rendering: crate::config::rendering::RenderingConfig {
            thinking: crate::config::rendering::ThinkingMode::Hidden,
            math: crate::config::rendering::MathMode::Off,
            images: ImagesMode::Off,
        },
        ..crate::config::AppConfig::default()
    };
    let dumped = config.to_yaml();
    assert!(dumped.contains("Off"), "{dumped}");
    let parsed: crate::config::AppConfig =
        serde_yaml::from_str(&dumped).expect("the dump parses back");
    assert_eq!(parsed.rendering.images, ImagesMode::Off, "{dumped}");
    assert_eq!(
        parsed.rendering.math,
        crate::config::rendering::MathMode::Off,
        "{dumped}"
    );
    assert_eq!(
        parsed.rendering.thinking,
        crate::config::rendering::ThinkingMode::Hidden,
        "{dumped}"
    );
}

// ── 2. the degradation ladder ───────────────────────────────────

/// The two-tier rule, mechanically: every "cannot draw" state renders the
/// legacy link path, **cell for cell**.
#[test]
fn a_lane_that_cannot_draw_is_cell_for_cell_the_today_rendering() {
    let dir = TempDir::new("degrade");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let other = TempDir::new("degrade-missing");
    let text = "图片与文字同行：inline ![plot](plot.png) 后面还有字。\n\n![plot](plot.png)\n";

    // The baseline: mode off, on a terminal that could draw.
    let mut off = app_with_images(ImagesMode::Off, kitty(), Some(dir.path()));
    off.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut off_term = test_terminal(50, 14);
    let baseline = frame(&mut off, &mut off_term);
    assert!(!has_placeholder(&baseline));

    // (a) auto, terminal without a graphics protocol.
    let mut incapable =
        app_with_images(ImagesMode::Auto, ImageSupport::disabled(), Some(dir.path()));
    incapable.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut incapable_term = test_terminal(50, 14);
    let incapable_frame = frame(&mut incapable, &mut incapable_term);
    assert_eq!(
        incapable_frame, baseline,
        "an incapable terminal must not differ"
    );
    assert!(
        incapable.images.stats().is_none(),
        "an incapable terminal builds no store at all"
    );

    // (b) auto, capable, but the file does not exist: once the probe has
    // answered `Unavailable`, the path is still not in the metadata table.
    let mut missing = app_with_images(ImagesMode::Auto, kitty(), Some(other.path()));
    missing.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut missing_term = test_terminal(50, 14);
    draw_until(
        &mut missing,
        &mut missing_term,
        "the failed probe",
        |app, _| app.images.stats().expect("lane").memo > 0,
    );
    let missing_frame = frame(&mut missing, &mut missing_term);
    assert_eq!(missing_frame, baseline, "a missing file is the link path");
    assert!(!has_placeholder(&missing_frame));
}

/// The probe is asynchronous: the frame before its answer is the baseline
/// too, and the anchor appears only once the shape is known.
#[test]
fn the_frame_before_the_probe_answers_is_the_baseline() {
    let dir = TempDir::new("pending-probe");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let text = "![plot](plot.png)";

    let mut off = app_with_images(ImagesMode::Off, kitty(), Some(dir.path()));
    off.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut off_term = test_terminal(60, 48);
    let baseline = frame(&mut off, &mut off_term);

    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut term = test_terminal(60, 48);
    // The very first frame: the probe has just been queued (in the *post*-render
    // sync), so nothing is known yet.
    let first = frame(&mut app, &mut term);
    assert_eq!(first, baseline, "an unknown image is the link path");
    assert!(!buffer_text(&first).contains('▢'));

    // The answer lands, and the anchor takes over.
    draw_until(&mut app, &mut term, "the anchor", |_, buf| {
        buffer_text(buf).contains('▢')
    });
}

/// A picture whose shape is known but which can never be drawn (a truncated
/// file) keeps its caption — the box is never blank.
#[test]
fn an_unencodable_picture_keeps_its_caption() {
    let dir = TempDir::new("unencodable");
    let plot = dir.file("plot.png");
    write_truncated_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    let buf = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let buf = frame(&mut app, &mut term);
            if buffer_text(&buf).contains('▢') {
                break buf;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the anchor"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    };
    // The anchor exists (the header read fine) and nothing was painted over it.
    let recorded = app.chat.frame_images().first().cloned().expect("recorded");
    assert!(
        !has_placeholder(&buf),
        "an unavailable picture draws nothing"
    );
    assert!(buffer_text(&buf).contains('▢'));
    assert_eq!(recorded.target.width, app.chat.geometry().area.width - 2);

    // The failure is memoised: later frames do not change their mind.
    let later = frame(&mut app, &mut term);
    assert!(!has_placeholder(&later));
}

/// The `Pending` step of the ladder, pinned where it is deterministic: the
/// encode is queued by this very call, so the picture cannot have arrived yet —
/// and the caption must survive untouched.
#[test]
fn a_pending_encode_leaves_the_caption_untouched() {
    let dir = TempDir::new("encode-pending");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut images = Images::new(ImagesMode::Auto, kitty(), None);
    assert!(images.set_workspace(Some(dir.path().to_str().expect("utf-8 temp path"))));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !images.sync(std::slice::from_ref(&plot)) {
        assert!(Instant::now() < deadline, "timed out waiting for the probe");
        std::thread::sleep(Duration::from_millis(2));
    }

    let clip = Rect::new(0, 0, 40, 10);
    let mut caption = Buffer::empty(clip);
    caption.set_string(2, 0, "▢ plot · 800×600", Style::default());
    let requested = FrameImage {
        area: Rect::new(2, 0, 38, 5),
        offset: (2, 0),
        target: Size::new(38, 5),
        path: plot.clone(),
    };
    let mut buf = caption.clone();
    images.paint(std::slice::from_ref(&requested), clip, None, &mut buf);
    assert_eq!(
        buf, caption,
        "a queued encode must leave the caption exactly as it was"
    );

    // Once the encode lands, the same call paints the picture.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut painted = false;
    while !painted {
        images.sync(std::slice::from_ref(&plot));
        let mut attempt = caption.clone();
        images.paint(std::slice::from_ref(&requested), clip, None, &mut attempt);
        painted = has_placeholder(&attempt);
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the encode"
        );
        if !painted {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// A content rebuild (session switch / compaction / rewind) re-reads the
/// files: the lane has no file watcher, so the rebuild is the moment a
/// rewritten picture is noticed.
#[test]
fn a_content_rebuild_re_reads_a_replaced_picture() {
    let dir = TempDir::new("replaced");
    let plot = dir.file("plot.png");
    // Tall enough that the 36-row box caps it: the first generation's box is
    // the maximum, the second's is a single row.
    write_png(&plot, 800, 6000);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(122, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    let tall = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(tall.target.height, 36, "the tall fixture's cap");

    // The file is replaced by a wide sliver and the session is rebuilt (the
    // epoch `ChatView::clear` bumps — the same one every `/new` / replay does).
    write_png(&plot, 2000, 20);
    app.chat.clear();
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    draw_until(&mut app, &mut term, "the new shape", |_, buf| {
        has_placeholder(buf)
    });
    let sliver = app.chat.frame_images().first().cloned().expect("recorded");
    let content = app.chat.geometry().area;
    assert_eq!(
        sliver.target,
        Size::new(
            content.width - 2,
            anchor_rows(content.width - 2, ImageShape::new(2000, 20), LAYOUT_CELL)
        ),
        "the rebuilt content must lay the picture out from its new header"
    );
    assert_eq!(sliver.target.height, 1);
}

/// …and a picture that vanished behind the rebuild stops being an anchor: the
/// rebuilt content falls back to the link path instead of drawing a stale box.
#[test]
fn a_content_rebuild_forgets_a_deleted_picture() {
    let dir = TempDir::new("deleted");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let text = "![plot](plot.png)";

    let mut off = app_with_images(ImagesMode::Off, kitty(), Some(dir.path()));
    off.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut off_term = test_terminal(60, 48);
    let baseline = frame(&mut off, &mut off_term);

    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    // The file goes away between two sessions; the rebuilt view must not keep
    // the shape the old session probed.
    fs::remove_file(&plot).expect("remove the fixture");
    app.chat.clear();
    app.chat.push(ChatCell::AssistantMessage(text.into()));
    draw_until(&mut app, &mut term, "the failed re-probe", |app, _| {
        app.images.stats().expect("lane").memo > 0
    });
    let buf = frame(&mut app, &mut term);
    assert_eq!(buf, baseline, "a deleted picture is the link path again");
    assert!(!buffer_text(&buf).contains('▢'));
}

/// A workspace change (`/workdir`, a session switch) restarts the metadata
/// table: its keys were resolved against the old root, so a relative path may
/// now name a different file — or no file at all.
#[test]
fn a_workspace_change_restarts_the_metadata_table() {
    let first = TempDir::new("ws-first");
    let plot = first.file("plot.png");
    write_png(&plot, 800, 600);
    let second = TempDir::new("ws-second");
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(first.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    assert_eq!(app.images.opts().shapes().len(), 1);

    // The session now works somewhere else — where `plot.png` does not exist.
    app.status.workdir = Some(second.path().to_string_lossy().into_owned());
    draw_until(&mut app, &mut term, "the table restart", |app, _| {
        app.images.opts().shapes().is_empty()
    });
    let buf = frame(&mut app, &mut term);
    assert!(!has_placeholder(&buf), "the old picture must not survive");
    assert!(
        !buffer_text(&buf).contains('▢'),
        "and neither must its box: the path resolves to a missing file now"
    );
}

// ── 3. overlay masking ──────────────────────────────────────────

#[test]
fn a_toast_over_a_picture_suppresses_it_whole() {
    let dir = TempDir::new("toast");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    // The toast paints over the band's top-right — the box's first rows.
    app.show_toast(Toast::info("hello", Duration::from_secs(30)));
    let buf = frame(&mut app, &mut term);
    assert!(
        !has_placeholder(&buf),
        "a picture the toast covers must not be drawn"
    );
    assert!(
        buffer_text(&buf).contains('▢'),
        "the caption stays as the fallback"
    );

    // …and it comes back once the overlay is gone.
    app.clear_toast();
    let buf = frame(&mut app, &mut term);
    assert!(has_placeholder(&buf));
}

#[test]
fn a_picture_never_leaves_the_chat_band() {
    let dir = TempDir::new("band");
    let plot = dir.file("plot.png");
    write_png(&plot, 1200, 400);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(70, 24);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    let band = app.geometry.chat_band();
    let composer = app.geometry.composer_rect();
    for (x, y) in placeholder_cells(term.backend().buffer()) {
        assert!(
            y >= band.y && y < band.bottom(),
            "a picture cell landed outside the chat band at ({x},{y})"
        );
        assert!(
            x < band.right() - 1,
            "a picture cell landed on the scrollbar gutter at ({x},{y})"
        );
        assert!(
            y < composer.y,
            "a picture cell landed on the composer at ({x},{y})"
        );
        assert!(
            y > 0,
            "a picture cell landed on the status bar at ({x},{y})"
        );
    }
}

// ── 4. selection ────────────────────────────────────────────────

#[test]
fn a_drag_selection_keeps_the_frame_text_only() {
    let dir = TempDir::new("selection");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    let band = app.geometry.chat_band();
    app.handle_mouse(super::support::press((band.x + 3, band.y + 1)));
    app.handle_mouse(super::support::drag((band.x + 20, band.y + 4)));
    let buf = frame(&mut app, &mut term);
    assert!(
        !has_placeholder(&buf),
        "a drag captures text: the frame must not draw pictures"
    );
    assert!(
        buffer_text(&buf).contains('▢'),
        "the anchor's caption is what gets copied"
    );

    app.handle_mouse(super::support::release((band.x + 20, band.y + 4)));
    let buf = frame(&mut app, &mut term);
    assert!(
        has_placeholder(&buf),
        "the picture comes back after the release"
    );
}

// ── 5. invalidation ─────────────────────────────────────────────

/// `needs_full_redraw` is the one flag `terminal.clear()`, a resize and a
/// focus regain all set: it must drop the store's encodings (the terminal no
/// longer holds them) and the very next encode must paint again.
#[test]
fn a_full_redraw_invalidates_the_pictures() {
    let dir = TempDir::new("invalidate");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    assert_eq!(app.images.stats().expect("lane").cached, 1);

    // Exactly what the resize / focus arms do.
    app.needs_full_redraw = true;
    let buf = frame(&mut app, &mut term);
    assert!(
        !has_placeholder(&buf),
        "an invalidated protocol must not be repainted"
    );
    assert_eq!(
        app.images.stats().expect("lane").cached,
        0,
        "the encodings were dropped"
    );

    // The re-encode lands and the picture is back.
    draw_until(&mut app, &mut term, "the re-encode", |_, buf| {
        has_placeholder(buf)
    });
}

// ── 6. one set of options ───────────────────────────────────────

/// The app hands the *same* `ImageOpts` to the heights (`update_heights`) and
/// to the drawing pass: a frame that grows an anchor must report the height
/// the layout used. The streaming/non-streaming agreement itself is pinned in
/// `ui::cached_cell`, this is the app-level wiring.
#[test]
fn the_layout_height_matches_what_is_drawn() {
    let dir = TempDir::new("geometry");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    let content = app.chat.geometry().area;
    let frame = app.chat.frame_images().first().cloned().expect("recorded");
    // The box the drawing pass got is the box the height reserved: the cell is
    // the anchor's rows plus the blank line every assistant message ends with,
    // and the picture fills the box from the content's very first row.
    let rows = anchor_rows(content.width - 2, ImageShape::new(800, 600), LAYOUT_CELL);
    assert_eq!(frame.target.height, rows);
    assert_eq!(app.chat.content_height(), usize::from(rows) + 1);
    assert_eq!(frame.area.y, content.y);
    assert_eq!(frame.area.height, rows);
    // A narrower frame re-lays the box (and so re-encodes): the box — and with
    // it the encode target `Images::paint` requests — follows the width.
    let (palette, layout) = (app.palette(), app.config.layout.clone());
    let ctx = crate::render::renderable::CellContext {
        palette: &palette,
        thinking_mode: app.config.rendering.thinking,
        thinking_expanded: app.chat.thinking_expansion(),
        layout: &layout,
        images: app.images.opts(),
    };
    let narrowed = 40u16;
    let resized = app.chat.cells[0].compute_cell_frame(narrowed, &ctx);
    let anchor = resized
        .images
        .iter()
        .flatten()
        .next()
        .cloned()
        .expect("the anchor survives the resize");
    // The box is the markdown render width — the cell width minus its prefix.
    assert_eq!(anchor.cols, narrowed - 2);
    assert_eq!(
        anchor.rows,
        anchor_rows(narrowed - 2, ImageShape::new(800, 600), LAYOUT_CELL),
        "the box (and so the encode target) is a function of the width"
    );
    assert_eq!(
        app.chat.cells[0].compute_height(narrowed, &ctx),
        usize::from(anchor.rows) + 1,
        "the layout sums the box the drawing pass was handed"
    );
}

// ── 7. freshness: a rewritten picture must come back ────────────

/// One freshness tick — the event loop's timer arm, with the clock handed in.
///
/// The lane takes `now` as an argument precisely so this can be a synthetic
/// instant: the interval is exercised without a single `sleep`.
fn tick(app: &mut App, at: Instant) -> bool {
    app.poll_image_freshness(at)
}

/// Whether `(x, y)` carries a painted part of a picture.
fn painted_at(buf: &Buffer, x: u16, y: u16) -> bool {
    x < buf.area.right() && y < buf.area.bottom() && buf[(x, y)].symbol().contains(PLACEHOLDER)
}

/// The whole rewrite story: the model regenerates `plot.png` in place, the lane
/// notices within its window, and the *new* header re-lays the box out.
#[test]
fn a_rewritten_picture_is_re_read_on_the_next_freshness_tick() {
    let dir = TempDir::new("rewrite");
    let plot = dir.file("plot.png");
    // Tall enough that the 36-row box caps the first generation's rows.
    write_png(&plot, 800, 6000);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(122, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    let content = app.chat.geometry().area;
    let cols = content.width - 2;
    let before = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(
        before.target,
        Size::new(
            cols,
            anchor_rows(cols, ImageShape::new(800, 6000), LAYOUT_CELL)
        )
    );
    assert_eq!(before.target.height, MAX_ANCHOR_ROWS, "the tall fixture");

    // The first check has nothing to report: it records what the files look
    // like (the store's own version, which is what the picture was made from).
    let now = Instant::now();
    assert!(!tick(&mut app, now), "an unchanged file is not a change");
    let stats = app.images.stats().expect("lane");
    assert_eq!(stats.cached, 1, "and nothing was dropped");
    assert_eq!(stats.in_flight, 0, "and nothing was queued");

    // The second generation of the same chart, written to the same path.
    write_png(&plot, 2000, 20);
    assert!(
        tick(&mut app, now + FRESHNESS_INTERVAL),
        "the rewrite is seen"
    );

    // The very next frame has no picture (the memo is gone, the probe is in
    // flight) but the box is still there — a caption, never a blank hole.
    let buf = frame(&mut app, &mut term);
    assert!(!has_placeholder(&buf), "the old encoding must not survive");
    assert!(buffer_text(&buf).contains('▢'), "the caption holds the box");

    draw_until(&mut app, &mut term, "the new picture", |_, buf| {
        has_placeholder(buf)
    });
    let after = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(
        after.target,
        Size::new(
            cols,
            anchor_rows(cols, ImageShape::new(2000, 20), LAYOUT_CELL)
        ),
        "the new header re-laid the box out (a pure function of the shape)"
    );
    assert_eq!(after.target.height, 1, "the wide sliver");
    assert_ne!(after.target.height, before.target.height);
    let painted = placeholder_rect(term.backend().buffer()).expect("placeholders");
    assert_eq!(painted, after.area, "drawn where the new box says");
}

/// The other half of the contract: an unchanged file is not a change — no
/// re-probe, no re-encode, not one cell of the frame moves.
#[test]
fn an_unchanged_picture_is_not_re_encoded() {
    let dir = TempDir::new("no-change");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    let painted = frame(&mut app, &mut term);

    let now = Instant::now();
    assert!(!tick(&mut app, now), "the baseline check");
    assert!(
        !tick(&mut app, now + FRESHNESS_INTERVAL * 4),
        "four windows later the file still says the same thing"
    );

    let after = frame(&mut app, &mut term);
    assert_eq!(after, painted, "the frame is byte for byte unchanged");
    let stats = app.images.stats().expect("lane");
    assert_eq!(stats.cached, 1, "the encoding was never dropped");
    assert_eq!(stats.in_flight, 0, "and never re-queued");
}

/// The throttle: a check costs a `stat` per watched picture, so it happens at
/// most once per window — a rewrite inside the window waits for the next one.
#[test]
fn the_freshness_check_is_throttled_to_its_interval() {
    let dir = TempDir::new("throttle");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    let now = Instant::now();
    assert!(
        !tick(&mut app, now),
        "the first window only records the baseline"
    );

    write_png(&plot, 2000, 20);
    assert!(
        !tick(&mut app, now + FRESHNESS_INTERVAL / 2),
        "inside the window the rewrite is not looked for"
    );
    // …and the picture is still the old one, which is the point of the throttle
    // being a *bound* and not a promise of instant freshness.
    let buf = frame(&mut app, &mut term);
    assert!(has_placeholder(&buf), "the old encoding is still on screen");

    assert!(
        tick(&mut app, now + FRESHNESS_INTERVAL),
        "past the window the check runs and sees the rewrite"
    );
}

/// The target set is what is **on screen**: a picture scrolled out of the
/// viewport is not watched (and parks the timer arm), and scrolling back brings
/// it into the next window.
#[test]
fn off_screen_pictures_are_not_watched() {
    let dir = TempDir::new("off-screen");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(122, 30);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    // A tall trailing message makes the band scrollable; five rows down is past
    // the 36-row box's top but still inside it, so scroll well past the box.
    app.chat.push(ChatCell::AssistantMessage(
        (0..60)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    frame(&mut app, &mut term);
    let band = app.geometry.chat_band();
    app.chat.scroll_to(40, band.height as usize);
    frame(&mut app, &mut term);
    assert!(
        app.chat.frame_images().is_empty(),
        "the box is off screen, so no anchor is recorded"
    );
    assert_eq!(
        app.images.freshness_deadline(Instant::now()),
        None,
        "nothing on screen is nothing to watch: the arm parks"
    );

    // A rewrite while it is off screen is deliberately not looked for …
    let now = Instant::now();
    write_png(&plot, 2000, 20);
    assert!(!tick(&mut app, now + FRESHNESS_INTERVAL * 2));

    // … and scrolling back puts it in the target set again: the next window
    // re-reads it, and the box follows the new header.
    app.chat.scroll_to(0, band.height as usize);
    frame(&mut app, &mut term);
    assert!(
        app.images
            .freshness_deadline(now + FRESHNESS_INTERVAL * 2)
            .is_some(),
        "a visible anchor schedules the check"
    );
    assert!(
        tick(&mut app, now + FRESHNESS_INTERVAL * 3),
        "the rewrite is seen"
    );
    draw_until(&mut app, &mut term, "the new shape", |app, _| {
        app.chat
            .frame_images()
            .first()
            .is_some_and(|frame| frame.target.height == 1)
    });
}

/// A rewrite that lands mid-write (here: a truncated file, the shape a
/// non-atomic save has for a few milliseconds) does not strand the picture: the
/// box keeps the shape the last good probe read — a caption, exactly like the
/// encode-failure case — and the next window picks the finished file up.
#[test]
fn a_degraded_picture_keeps_its_box_and_recovers() {
    let dir = TempDir::new("degraded");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    let before = app.chat.frame_images().first().cloned().expect("recorded");

    // The file is emptied (the truncate half of a rewrite).
    fs::write(&plot, b"").expect("truncate the fixture");
    let now = Instant::now();
    assert!(tick(&mut app, now + FRESHNESS_INTERVAL));
    draw_until(&mut app, &mut term, "the failed re-probe", |app, _| {
        let stats = app.images.stats().expect("lane");
        stats.memo > 0 && stats.known_meta == 0
    });
    let buf = frame(&mut app, &mut term);
    assert!(!has_placeholder(&buf), "an empty file has nothing to draw");
    let after = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(
        after.target, before.target,
        "the box keeps the shape of the last good probe"
    );
    assert!(
        buffer_text(&buf).contains('▢'),
        "the caption is what explains the box"
    );
    assert!(
        app.images
            .opts()
            .shapes()
            .iter()
            .any(|entry| entry.path == plot),
        "and the path stays in the table, which is what keeps it watched"
    );

    // The write finishes: the next window sees a version that differs from the
    // one we last observed, and the picture comes back.
    write_png(&plot, 800, 600);
    assert!(tick(&mut app, now + FRESHNESS_INTERVAL * 2));
    draw_until(&mut app, &mut term, "the recovered picture", |_, buf| {
        has_placeholder(buf)
    });
    let recovered = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(recovered.target, before.target, "same picture, same box");
}

/// The end-to-end limits: everything the store refuses at the *metadata* stage
/// renders exactly like the picture lane never existed — the link path, cell
/// for cell, no reserved rows, no half-drawn box.
#[test]
fn pathological_files_are_cell_for_cell_the_baseline() {
    let dir = TempDir::new("pathological");
    // Empty, not an image, larger than the file ceiling (a sparse file: the
    // probe must refuse it from the stat alone, never read a byte of it), and
    // simply missing.
    fs::write(dir.file("empty.png"), b"").expect("empty fixture");
    fs::write(dir.file("liar.png"), b"this is not a png\n").expect("liar fixture");
    let huge = std::fs::File::create(dir.file("huge.png")).expect("huge fixture");
    huge.set_len(DEFAULT_FILE_BYTES + 1).expect("sparse length");
    let text =
        "![empty](empty.png)\n\n![liar](liar.png)\n\n![huge](huge.png)\n\n![missing](missing.png)";

    let mut off = app_with_images(ImagesMode::Off, kitty(), Some(dir.path()));
    off.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut off_term = test_terminal(60, 40);
    let baseline = frame(&mut off, &mut off_term);

    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(text.into()));
    let mut term = test_terminal(60, 40);
    draw_until(&mut app, &mut term, "every probe to fail", |app, _| {
        app.images.stats().expect("lane").memo >= 4
    });
    let buf = frame(&mut app, &mut term);
    assert_eq!(buf, baseline, "a refused picture is the link path again");
    assert!(!has_placeholder(&buf));
    assert!(!buffer_text(&buf).contains('▢'), "no reserved box either");
    assert!(
        app.images.opts().shapes().is_empty(),
        "a refused path never enters the metadata table"
    );
    assert_eq!(
        app.chat.content_height(),
        off.chat.content_height(),
        "and it costs no rows"
    );
}

/// The two extreme shapes: a 1×5000 sliver (the row cap) and an 8000×100 strip
/// (a single row). Both are *valid* pictures, so both are drawn — inside the
/// bounds the pure row function gives, without a panic anywhere.
#[test]
fn extreme_aspect_ratios_stay_inside_the_bounds() {
    let dir = TempDir::new("extreme");
    write_png(&dir.file("sliver.png"), 1, 5000);
    write_png(&dir.file("strip.png"), 8000, 100);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(
        "![sliver](sliver.png)\n\n![strip](strip.png)".into(),
    ));
    let mut term = test_terminal(60, 48);
    // Both boxes are on screen, and both encodings have landed (the first
    // placeholder to appear is the *fast* one; the 8000-pixel-wide strip takes
    // longer to decode and encode).
    draw_until(&mut app, &mut term, "both pictures", |app, buf| {
        let frames = app.chat.frame_images();
        frames.len() == 2
            && frames
                .iter()
                .all(|frame| painted_at(buf, frame.area.x, frame.area.y))
    });

    let cols = app.chat.geometry().area.width - 2;
    let frames = app.chat.frame_images().to_vec();
    assert_eq!(frames.len(), 2, "one box per anchor");
    assert_eq!(
        frames[0].target,
        Size::new(
            cols,
            anchor_rows(cols, ImageShape::new(1, 5000), LAYOUT_CELL)
        )
    );
    assert_eq!(
        frames[0].target.height, MAX_ANCHOR_ROWS,
        "the sliver is capped, not unbounded"
    );
    assert_eq!(
        frames[1].target,
        Size::new(
            cols,
            anchor_rows(cols, ImageShape::new(8000, 100), LAYOUT_CELL)
        )
    );
    assert_eq!(frames[1].target.height, 1, "the strip is one row");
    let buf = term.backend().buffer();
    assert!(
        painted_at(buf, frames[0].area.x, frames[0].area.y),
        "the sliver's box is drawn"
    );
    assert!(
        painted_at(buf, frames[1].area.x, frames[1].area.y),
        "the strip's box is drawn"
    );
}

/// A long session: a hundred-odd distinct pictures, scrolled through. The
/// caches stay inside their published budgets at every step, the picture on
/// screen is drawn, and nothing panics or leaks a worker.
#[test]
fn a_hundred_pictures_stay_within_the_cache_budget() {
    const COUNT: u32 = 120;
    let dir = TempDir::new("pressure");
    // Wide, short pictures: one-row boxes at this width. Each cell is a
    // picture plus two text rows, so a 30-row band holds a handful of anchors
    // at a time — the shape the LRU (eight entries) is sized for.
    let body = (0..COUNT)
        .map(|index| {
            let name = format!("figure-{index:03}.png");
            write_png(&dir.file(&name), 1400 + index * 10, 20);
            format!("![figure {index}]({name})\n\ntext {index} line one\ntext {index} line two")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat.push(ChatCell::AssistantMessage(body));
    let mut term = test_terminal(60, 30);
    // The band is only known once a frame has been drawn, and the content
    // height only once the cells have been laid out.
    frame(&mut app, &mut term);
    let band = app.geometry.chat_band();
    let height = app.chat.content_height();
    let budget = (DEFAULT_CACHE_ENTRIES, DEFAULT_CACHE_BYTES);

    let mut steps = 0;
    let mut painted_any = false;
    for offset in (0..height).step_by(15) {
        app.chat.scroll_to(offset, band.height as usize);
        // Every anchor the frame recorded must end up drawn: the whole point of
        // scrolling through a long session is that the picture that is visible
        // is the picture that gets encoded, whatever the cache did before.
        //
        // The step's assertions below read **this** frame (see `draw_until`):
        // taking one more frame here would re-plan the step's requests, and a
        // picture that is `Pending` in it — an encode that landed and evicted
        // the cache entry in between, an anchor whose probe only just answered —
        // says nothing about the budget under test. The wait demands a
        // non-empty set as well: `all()` over nothing would return the first
        // frame and make the step vacuous.
        let buf = draw_until(&mut app, &mut term, "the step's pictures", |app, buf| {
            let frames = app.chat.frame_images();
            !frames.is_empty()
                && frames
                    .iter()
                    .all(|frame| painted_at(buf, frame.area.x, frame.area.y))
        });
        let stats = app.images.stats().expect("lane");
        assert!(
            stats.cached <= budget.0,
            "the LRU grew past its entry budget at row {offset}: {stats:?}"
        );
        assert!(
            stats.cached_bytes <= budget.1,
            "the LRU grew past its byte budget at row {offset}: {stats:?}"
        );
        assert!(
            stats.memo <= MAX_META_ENTRIES,
            "the metadata memo grew past its bound at row {offset}: {stats:?}"
        );
        assert!(
            stats.failed <= MAX_FAILED_ENTRIES,
            "the failure memo grew past its bound at row {offset}: {stats:?}"
        );
        assert!(stats.worker_alive, "the worker died at row {offset}");
        let frames = app.chat.frame_images();
        for anchor in frames {
            assert!(
                painted_at(&buf, anchor.area.x, anchor.area.y),
                "the picture at row {offset} was not drawn"
            );
            painted_any = true;
        }
        steps += 1;
    }
    assert!(steps > 10, "the fixture must scroll ({steps} steps)");
    assert!(painted_any, "the fixture must show pictures");
    assert_eq!(
        app.images.opts().shapes().len(),
        COUNT as usize,
        "every picture the session names has a shape once the view has been laid out"
    );
}

/// The **same-size** rewrite: new pixels, identical byte length.
///
/// This is the case that makes the timestamp half of the freshness comparison
/// load-bearing — the byte count cannot tell the two versions apart — and it is
/// what "the model redrew the same chart" looks like whenever the encoder lands
/// on the same size. The fixture pair is built by [`pad_to`] (two different
/// pictures, padded to the same length) and the test asserts its own
/// preconditions, so it cannot silently degrade into the size-changed case that
/// `a_rewritten_picture_is_re_read_on_the_next_freshness_tick` already covers.
#[test]
fn a_same_size_rewrite_is_seen() {
    const PIXELS: (u32, u32) = (40, 12);
    let dir = TempDir::new("same-size");
    let plot = dir.file("plot.png");
    write_flat_png(&plot, PIXELS.0, PIXELS.1, [10, 20, 30]);
    let base = fs::metadata(&plot).expect("stat the fixture").len();
    pad_to(&plot, base + 4096);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(60, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });

    // The baseline check, before anything moves: it records what the store read.
    let now = Instant::now();
    assert!(!tick(&mut app, now), "the baseline check");
    let before = app.chat.frame_images().first().cloned().expect("recorded");
    let before_bytes = fs::read(&plot).expect("read the fixture");
    let before_len = before_bytes.len() as u64;
    let before_mtime = fs::metadata(&plot)
        .expect("stat the fixture")
        .modified()
        .expect("mtime");
    let before_transmission = transmit_sequence(term.backend().buffer(), before.area);

    // Different pixels, same dimensions, padded to the very same length, one
    // minute later.
    let replacement = dir.file("replacement.png");
    write_flat_png(&replacement, PIXELS.0, PIXELS.1, [200, 100, 50]);
    pad_to(&replacement, before_len);
    fs::copy(&replacement, &plot).expect("replace the picture");
    let handle = fs::OpenOptions::new()
        .write(true)
        .open(&plot)
        .expect("open the fixture to set its timestamp");
    handle
        .set_times(fs::FileTimes::new().set_modified(before_mtime + Duration::from_secs(60)))
        .expect("set the mtime");

    // The fixture's own guards: the content moved, the size did not.
    let after_bytes = fs::read(&plot).expect("read the fixture");
    assert_ne!(after_bytes, before_bytes, "the pixels must change");
    assert_eq!(
        after_bytes.len() as u64,
        before_len,
        "the fixture must keep the byte length — that is the whole point"
    );
    assert_ne!(
        fs::metadata(&plot)
            .expect("stat the fixture")
            .modified()
            .expect("mtime"),
        before_mtime,
        "and move the timestamp instead"
    );

    assert!(
        tick(&mut app, now + FRESHNESS_INTERVAL),
        "a rewrite that only the timestamp can reveal must still be seen"
    );

    // The stale transmission is dropped (not repainted) …
    let buf = frame(&mut app, &mut term);
    assert!(
        !has_placeholder(&buf),
        "the old encoding must not be repainted"
    );

    // … and the picture comes back as a *new* transmission: same box, same
    // cells, different image id — so the file really was read again.
    draw_until(&mut app, &mut term, "the new picture", |_, buf| {
        has_placeholder(buf)
    });
    let recovered = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(
        recovered.area, before.area,
        "the same dimensions must lay out the same box"
    );
    assert_ne!(
        transmit_sequence(term.backend().buffer(), recovered.area),
        before_transmission,
        "the picture must be a fresh encoding, not the old protocol reused"
    );
}

/// The freshness timer arm itself: armed with a deadline it fires, parked on
/// `None` it never wakes the loop.
///
/// Same contract (and same shape) as the selection auto-scroll arm's test: the
/// `None` half is what makes "a session with nothing anchored costs this lane
/// nothing" true, and an arm that returned immediately from `None` would spin
/// the event loop at the frame rate.
#[tokio::test]
async fn test_image_freshness_timer_fires_only_when_armed() {
    // Armed with a deadline already reached: the arm completes on its own.
    tokio::select! {
        () = image_freshness_tick(Some(std::time::Instant::now())) => {}
        () = tokio::time::sleep(FRESHNESS_INTERVAL * 20) => {
            panic!("an armed timer must fire");
        }
    }

    // Parked: it never completes — the loop stays idle instead of polling.
    let parked = tokio::time::timeout(FRESHNESS_INTERVAL * 3, image_freshness_tick(None)).await;
    assert!(
        parked.is_err(),
        "a parked timer must not wake the event loop"
    );
}

/// `select!` rebuilds the arm on every loop iteration, so the deadline has to be
/// absolute: a relative sleep would be pushed back by every key press and stream
/// event and, during a busy turn, would never fire.
#[tokio::test]
async fn test_image_freshness_deadline_survives_busy_iterations() {
    let step = std::time::Duration::from_millis(30);
    let stop = std::time::Instant::now() + step * 10;
    let mut deadline = Some(std::time::Instant::now());
    let mut fires = 0;
    while std::time::Instant::now() < stop {
        tokio::select! {
            () = image_freshness_tick(deadline) => {
                fires += 1;
                deadline = Some(std::time::Instant::now() + step);
            }
            // An event far more frequent than the interval: the arm is dropped
            // and rebuilt with the *same* deadline.
            () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
        }
    }
    assert!(
        fires >= 3,
        "the absolute deadline must keep firing despite frequent iterations, got {fires}"
    );
}

/// A window resize (`Resize` sets `needs_full_redraw`) re-encodes for the new
/// box: the terminal lost the picture, and the layout it lost it in is gone too.
#[test]
fn a_resize_re_encodes_for_the_new_box() {
    let dir = TempDir::new("resize");
    let plot = dir.file("plot.png");
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(122, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    let wide = app.chat.frame_images().first().cloned().expect("recorded");
    let wide_cols = app.chat.geometry().area.width - 2;
    assert_eq!(
        wide.target,
        Size::new(
            wide_cols,
            anchor_rows(wide_cols, ImageShape::new(800, 600), LAYOUT_CELL)
        )
    );

    // Exactly what `TermEvent::Resize` does: a new window size and a full
    // redraw. The backend is the source of truth for the size (`autoresize`
    // picks it up on the next draw), which is also why the app cannot miss it.
    term.backend_mut().resize(60, 48);
    app.needs_full_redraw = true;
    let buf = frame(&mut app, &mut term);
    assert!(!has_placeholder(&buf), "the invalidated protocol is gone");
    draw_until(&mut app, &mut term, "the re-encode", |_, buf| {
        has_placeholder(buf)
    });

    let narrow = app.chat.frame_images().first().cloned().expect("recorded");
    let cols = app.chat.geometry().area.width - 2;
    assert!(cols < wide_cols, "the fixture must narrow the content");
    assert_eq!(
        narrow.target,
        Size::new(
            cols,
            anchor_rows(cols, ImageShape::new(800, 600), LAYOUT_CELL)
        ),
        "the box — and so the encode target — follows the new width"
    );
    assert_ne!(narrow.target, wide.target);
    let painted = placeholder_rect(term.backend().buffer()).expect("placeholders");
    assert_eq!(painted, narrow.area, "drawn into the new box");
    assert_eq!(
        app.images.stats().expect("lane").cached,
        1,
        "one encoding, for the new target"
    );
}
