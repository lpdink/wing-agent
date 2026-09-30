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
use crate::app::images::Images;
use crate::config::AppConfig;
use crate::config::rendering::ImagesMode;
use crate::render::markdown::ImageShape;
use crate::render::markdown::anchor_rows;
use crate::ui::chat_view::ChatCell;
use crate::ui::chat_view::FrameImage;
use crate::ui::image::CellPixels;
use crate::ui::image::ImageProtocol;
use crate::ui::image::ImageSupport;
use crate::ui::toast::Toast;

/// The kitty placeholder symbol every painted cell of an image carries.
const PLACEHOLDER: char = '\u{10EEEE}';

const CELL: CellPixels = CellPixels::new(10, 20);

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

/// Draw until `done` holds on the frame just drawn.
///
/// The store's worker is a real thread and its results only become visible
/// through `poll()` at the start of a draw, so waiting *is* drawing in a loop
/// (the same shape the event loop has, with the waker replaced by this sleep).
fn draw_until(
    app: &mut App,
    term: &mut Terminal<TestBackend>,
    what: &str,
    mut done: impl FnMut(&App, &Buffer) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let buf = frame(app, term);
        if done(app, &buf) {
            return;
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
    let rows = anchor_rows(content.width - 2, ImageShape::new(800, 600));

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
            anchor_rows(content.width - 2, ImageShape::new(800, 600))
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
    write_png(&plot, 800, 600);
    let mut app = app_with_images(ImagesMode::Auto, kitty(), Some(dir.path()));
    app.chat
        .push(ChatCell::AssistantMessage("![plot](plot.png)".into()));
    let mut term = test_terminal(122, 48);
    draw_until(&mut app, &mut term, "the picture", |_, buf| {
        has_placeholder(buf)
    });
    let tall = app.chat.frame_images().first().cloned().expect("recorded");
    assert_eq!(tall.target.height, 36, "the 800×600 cap");

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
            anchor_rows(content.width - 2, ImageShape::new(2000, 20))
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
    let rows = anchor_rows(content.width - 2, ImageShape::new(800, 600));
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
        anchor_rows(narrowed - 2, ImageShape::new(800, 600)),
        "the box (and so the encode target) is a function of the width"
    );
    assert_eq!(
        app.chat.cells[0].compute_height(narrowed, &ctx),
        usize::from(anchor.rows) + 1,
        "the layout sums the box the drawing pass was handed"
    );
}
