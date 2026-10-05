//! Per-frame cost of the picture lane.
//!
//! What the TUI pays for markdown pictures, split into the four numbers the
//! hardening step is judged on. Everything here drives the *production* types
//! through their public API — the chat view widget that records the frame's
//! anchors, the image store that encodes them, and `ui::image::paint`, the one
//! buffer write — with the terminal capability injected, so the numbers are
//! headless, deterministic and about our own code rather than about a real
//! terminal's escape-sequence loop.
//!
//! ```text
//! frame/<n>           one frame's picture half: lay the band out, request the
//!                     visible anchors, paint them (n = 0 is the text baseline,
//!                     the anchor-free cost of the same content)
//! scroll/<n>          the same, one row further down each iteration — a wheel
//!                     notch, the operation a user repeats
//! first_encode/<w×h>  request → Ready: decode + protocol encode of a PNG that
//!                     is not in the cache (what a freshly written picture costs)
//! freshness/<n>       one freshness check over n watched paths: the lane's two
//!                     per-path operations (`ImageStore::meta` — a memo lookup —
//!                     and one `fs::metadata`), i.e. what the timer arm does
//!                     once per `FRESHNESS_INTERVAL`
//! ```
//!
//! Run:  cargo bench --bench image_frame
//! Filter examples:
//!   cargo bench --bench image_frame -- 'frame'
//!   cargo bench --bench image_frame -- 'first_encode'
//!
//! The numbers these produce are recorded in `docs/dev/tui-images.md`; the
//! behavior behind them is pinned by `src/app/tests/images.rs` (there is no
//! timing assertion anywhere — a threshold would be a flaky test, not a budget).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::widgets::Widget;
use wing::config::LayoutConfig;
use wing::config::ThemePalette;
use wing::render::markdown::{ImageEntry, ImageOpts, ImageShape};
use wing::render::renderable::CellContext;
use wing::ui::chat_view::{ChatCell, ChatView, ChatViewWidget};
use wing::ui::image::{CellPixels, ImageProtocol, ImageState, ImageStore, ImageSupport, paint};
use wing::ui::scrollbar;

use criterion::{Criterion, criterion_group, criterion_main};

/// Terminal width — the same column count the acceptance screenshots use.
const WIDTH: u16 = 120;
/// Band height: tall enough for eight four-row boxes and their text.
const HEIGHT: u16 = 60;
/// The terminal's cell size, as `ImageSupport` reports it after detection.
const CELL: CellPixels = CellPixels::new(10, 20);
/// The same cell as the render layer's mirror type — the layout input.
const LAYOUT_CELL: wing::render::markdown::CellPixels =
    wing::render::markdown::CellPixels::new(10, 20);
/// Picture size: a 290×80 PNG lands on a four-row box at `WIDTH`, and the box
/// **is** the picture (the rows reserved are the fitted footprint — see
/// `render::fit`). 80 px is what makes it four rows at a 20 px cell.
const PX: (u32, u32) = (290, 80);
// ── fixtures ────────────────────────────────────────────────────

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// Self-cleaning temp directory (the crate has no `tempfile` dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wing-bench-image-{}-{tag}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
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

/// One pane of the picture pipeline: a chat view, the store behind it, and the
/// options both share (exactly the wiring `App` owns).
struct Fixture {
    /// Kept alive so the pictures stay on disk for the whole measurement.
    _dir: TempDir,
    view: ChatView,
    store: ImageStore,
    opts: ImageOpts,
    palette: ThemePalette,
    layout: LayoutConfig,
}

impl Fixture {
    /// `count` cells at `WIDTH`, each one picture plus a line of text.
    ///
    /// `anchors` false renders the very same cells through the **link path**
    /// (`ImageOpts::off`) — the baseline the picture half of a frame is read
    /// against, content for content.
    fn new(count: usize, anchors: bool) -> Self {
        const TEXT_BASELINE: usize = 8;
        let dir = TempDir::new("frame");
        let mut entries = Vec::new();
        for index in 0..count.max(TEXT_BASELINE) {
            let name = format!("figure-{index}.png");
            let path = dir.file(&name);
            write_png(&path, PX.0, PX.1);
            entries.push(ImageEntry::new(path, ImageShape::new(PX.0, PX.1)));
        }
        let opts = if anchors {
            ImageOpts::anchor(Some(dir.0.clone()), entries, LAYOUT_CELL)
        } else {
            ImageOpts::off().clone()
        };
        let mut view = ChatView::new();
        for index in 0..count {
            view.push(ChatCell::AssistantMessage(format!(
                "![figure {index}](figure-{index}.png)\n\nrow {index} of text"
            )));
        }
        Self {
            store: ImageStore::new(ImageSupport::from_parts(ImageProtocol::Kitty, CELL, false)),
            _dir: dir,
            view,
            opts,
            palette: ThemePalette::from_config(&wing::config::ColorsConfig::default()),
            layout: LayoutConfig::default(),
        }
    }

    /// The rect the chat widget is rendered into: the band minus the
    /// scrollbar gutter — the app's own `scrollbar::content_area`.
    fn content(&self) -> Rect {
        scrollbar::content_area(Rect::new(0, 0, WIDTH, HEIGHT))
    }

    /// One frame: lay the band out (which records this frame's anchors),
    /// request every anchor's encoding and paint what is ready.
    fn draw(&mut self, buf: &mut Buffer) {
        let area = self.content();
        buf.reset();
        let ctx = CellContext {
            palette: &self.palette,
            thinking_mode: wing::config::rendering::ThinkingMode::Visible,
            thinking_expanded: None,
            layout: &self.layout,
            images: &self.opts,
        };
        ChatViewWidget::new(&mut self.view, ctx).render(area, buf);
        self.paint(buf, area);
    }

    fn paint(&mut self, buf: &mut Buffer, area: Rect) {
        for frame in self.view.frame_images().to_vec() {
            if !frame.area.intersects(area) {
                continue;
            }
            let ImageState::Ready(image) = self.store.request(&frame.path, frame.target) else {
                continue;
            };
            paint(&image, area, frame.offset, buf);
        }
    }

    /// Drive the store until every visible anchor is encoded, the way the event
    /// loop does (draw, poll, draw): warm-up only, never inside a measurement.
    fn warm_up(&mut self, buf: &mut Buffer) {
        for _ in 0..200 {
            self.draw(buf);
            self.store.poll();
            if self.view.frame_images().iter().all(|frame| {
                matches!(
                    self.store.request(&frame.path, frame.target),
                    ImageState::Ready(_)
                )
            }) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("the fixture never encoded its pictures");
    }
}

// ── benches ─────────────────────────────────────────────────────

/// `frame/…`: one frame's worth of work for the same eight cells — rendered
/// with anchors on (`pictures/<n>`) or on the link path (`text`, the baseline
/// the picture half is read against).
fn frame_cost(c: &mut Criterion, scenario: &str, cells: usize, anchors: bool) {
    let mut fixture = Fixture::new(cells, anchors);
    let mut buf = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    fixture.warm_up(&mut buf);
    let visible = fixture.view.frame_images().len();

    let mut group = c.benchmark_group("frame");
    group.bench_function(scenario, |b| {
        b.iter(|| fixture.draw(&mut buf));
    });
    group.finish();
    // One line per scenario in the report: what the scenario actually did.
    let decoded = visible * PX.0 as usize * PX.1 as usize * 4;
    write_stderr(&format!(
        "frame/{scenario}: {cells} cells, {visible} anchors on screen, \
         {} KiB of decoded pixels\n",
        decoded / 1024
    ));
}

/// `scroll/<n>`: the same frame, one row further down each iteration.
fn scroll_cost(c: &mut Criterion, pictures: usize) {
    let mut fixture = Fixture::new(pictures, true);
    let mut buf = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
    fixture.warm_up(&mut buf);
    let span = fixture.view.content_height().max(1);

    let mut group = c.benchmark_group("scroll");
    group.bench_function(format!("{pictures}"), |b| {
        let mut offset = 0usize;
        b.iter(|| {
            fixture.view.scroll_to(offset, HEIGHT as usize);
            fixture.draw(&mut buf);
            offset = (offset + 1) % span;
        });
    });
    group.finish();
}

/// `first_encode/<w×h>`: request → Ready for a picture that is not cached.
fn encode_cost(c: &mut Criterion) {
    let dir = TempDir::new("encode");
    let mut group = c.benchmark_group("first_encode");
    group.sample_size(10);
    for (px_w, px_h) in [(290u32, 20u32), (800, 600), (1920, 1080)] {
        let path = dir.file(&format!("{px_w}x{px_h}.png"));
        write_png(&path, px_w, px_h);
        let target = Size::new(56, 20);
        let mut store =
            ImageStore::new(ImageSupport::from_parts(ImageProtocol::Kitty, CELL, false));
        // Warm the metadata memo: the measurement is the *encode*, not the probe.
        loop {
            store.poll();
            if matches!(store.meta(&path), wing::ui::image::MetaState::Known(_)) {
                break;
            }
        }
        group.bench_function(format!("{px_w}x{px_h}"), |b| {
            b.iter(|| {
                store.invalidate();
                assert!(matches!(store.request(&path, target), ImageState::Pending));
                loop {
                    store.poll();
                    if matches!(store.request(&path, target), ImageState::Ready(_)) {
                        break;
                    }
                    std::thread::yield_now();
                }
            });
        });
    }
    group.finish();
}

/// `freshness/<n>`: one check over `n` watched paths — the lane's per-path
/// work (`ImageStore::meta` memo lookup + one `fs::metadata`).
fn freshness_cost(c: &mut Criterion) {
    let dir = TempDir::new("freshness");
    let mut group = c.benchmark_group("freshness");
    for count in [1usize, 8] {
        let mut paths = Vec::new();
        for index in 0..count {
            let path = dir.file(&format!("watch-{index}.png"));
            write_png(&path, PX.0, PX.1);
            paths.push(path);
        }
        let mut store =
            ImageStore::new(ImageSupport::from_parts(ImageProtocol::Kitty, CELL, false));
        for path in &paths {
            loop {
                store.poll();
                if matches!(store.meta(path), wing::ui::image::MetaState::Known(_)) {
                    break;
                }
            }
        }
        group.bench_function(format!("{count}"), |b| {
            b.iter(|| {
                let mut seen = 0u64;
                for path in &paths {
                    let known = matches!(store.meta(path), wing::ui::image::MetaState::Known(_));
                    let bytes = fs::metadata(path).map(|stat| stat.len()).unwrap_or(0);
                    seen = seen.wrapping_add(bytes + u64::from(known));
                }
                seen
            });
        });
    }
    group.finish();
}

fn benches(c: &mut Criterion) {
    // The baseline and the picture tiers of the *same* eight cells.
    frame_cost(c, "text", 8, false);
    for pictures in [1usize, 4, 8] {
        frame_cost(c, &format!("pictures/{pictures}"), pictures, true);
    }
    for pictures in [1usize, 4, 8] {
        scroll_cost(c, pictures);
    }
    encode_cost(c);
    freshness_cost(c);
}

/// The crate denies `println!`/`eprintln!`; a bench reports through stderr.
fn write_stderr(text: &str) {
    use std::io::Write as _;
    let _ = std::io::stderr().write_all(text.as_bytes());
}

criterion_group!(benches_group, benches);
criterion_main!(benches_group);
