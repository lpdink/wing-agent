//! The image pipeline as a **consumer** sees it.
//!
//! Everything here goes through the public API only, in the shape step `05_image_anchor` /
//! `06_chatview_integration` will use it. An item that should have been `pub` but was left
//! `pub(crate)` fails this build instead of the next step's.
//!
//! Headless by construction: the terminal capability is injected, so no TTY, no escape
//! sequences and no timing dependency on a real terminal.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::style::Style;
use wing::ui::image::{
    CellPixels, ImageProtocol, ImageState, ImageStore, ImageSupport, MetaState, paint,
};

/// Every item step 05/06 may name through `wing::ui::image::…`.
///
/// This is the compile-time half of the hand-off: a missing re-export (or a `pub(crate)` that
/// should have been `pub`) fails *here*, in CI, instead of in the next step. `use` alone only
/// checks paths, so each item is also *referenced* in the shape the caller will use it.
#[test]
fn the_public_surface_is_reachable_at_the_module_root() {
    use wing::ui::image::{
        CellPixels, DEFAULT_CACHE_BYTES, DEFAULT_CACHE_ENTRIES, DEFAULT_DETECT_TIMEOUT,
        DEFAULT_FILE_BYTES, DEFAULT_PIXELS, ImageMeta, ImageProtocol, ImageState, ImageStore,
        ImageStoreConfig, ImageSupport, Limits, MAX_FAILED_ENTRIES, MAX_META_ENTRIES, MetaState,
        ReadyImage, StoreStats, Unavailable, paint,
    };

    // Constants.
    let budgets = (
        DEFAULT_DETECT_TIMEOUT.as_millis(),
        DEFAULT_CACHE_ENTRIES,
        DEFAULT_CACHE_BYTES,
        DEFAULT_FILE_BYTES,
        DEFAULT_PIXELS,
        MAX_META_ENTRIES,
        MAX_FAILED_ENTRIES,
    );
    assert_eq!(budgets.0, 500);

    // Functions and methods, with the signatures the caller relies on.
    let _: fn() -> ImageStore = || ImageStore::new(ImageSupport::disabled());
    let _: fn(ImageSupport, ImageStoreConfig) -> ImageStore = ImageStore::with_config;
    let _: fn(&ImageStore) -> &ImageSupport = ImageStore::support;
    let _: fn(&mut ImageStore, &Path) -> MetaState = ImageStore::meta;
    let _: fn(&mut ImageStore, &Path, Size) -> ImageState = ImageStore::request;
    let _: fn(&mut ImageStore) -> bool = ImageStore::poll;
    let _: fn(&mut ImageStore) = ImageStore::invalidate;
    let _: fn(&mut ImageStore) = ImageStore::reset;
    let _: fn(&mut ImageStore, &Path) = ImageStore::refresh;
    let _: fn(&ImageStore) -> StoreStats = ImageStore::stats;
    let _: fn(&ReadyImage) -> ImageProtocol = ReadyImage::protocol;
    let _: fn(&ReadyImage) -> Size = ReadyImage::size;
    let _: fn(&ReadyImage) -> bool = ReadyImage::is_current;
    let _: fn(&ReadyImage, Rect, (i16, i16), &mut Buffer) -> Option<Rect> = paint;

    let _: fn(u16, u16) -> CellPixels = CellPixels::new;
    let _: fn(&CellPixels) -> bool = CellPixels::is_valid;
    let _: fn(Duration) -> ImageSupport = ImageSupport::detect;
    let _: fn(ImageProtocol, CellPixels, bool) -> ImageSupport = ImageSupport::from_parts;
    let _: fn() -> ImageSupport = ImageSupport::disabled;
    let _: fn(&ImageSupport) -> bool = ImageSupport::is_enabled;
    let _: fn(&ImageSupport) -> Option<ImageProtocol> = ImageSupport::protocol;
    let _: fn(&ImageSupport) -> Option<CellPixels> = ImageSupport::cell_pixel_size;
    let _: fn(&ImageSupport) -> bool = ImageSupport::is_tmux;
    let _: fn(&ImageProtocol) -> &'static str = ImageProtocol::name;
    let _: fn(&ImageMeta) -> f64 = ImageMeta::aspect_ratio;

    // Data types, constructed/matched in caller shape (fields and variants are part of the
    // contract, and `Unavailable` must stay exhaustively matchable for the fallback).
    let meta = ImageMeta {
        px_w: 4,
        px_h: 3,
        bytes: 16,
        mtime: None,
    };
    assert!(meta.aspect_ratio() > 1.0);
    let _ = Limits::default();
    let _ = ImageStoreConfig::default();
    let _ = ImageStoreConfig {
        limits: Limits::default(),
        waker: None,
    };
    let plan = match store_plan() {
        ImageState::Ready(image) => (image.protocol(), image.size()),
        ImageState::Pending => (ImageProtocol::Kitty, Size::new(0, 0)),
        ImageState::Unavailable(_) => (ImageProtocol::Sixel, Size::new(0, 0)),
    };
    let _ = plan;
    let _ = match MetaState::Unknown {
        MetaState::Known(meta) => meta.px_w,
        MetaState::Unknown => 0,
        MetaState::Unavailable(reason) => reason.to_string().len() as u32,
    };
    let stats = StoreStats {
        cached: 0,
        cached_bytes: 0,
        in_flight: 0,
        memo: 0,
        known_meta: 0,
        failed: 0,
        worker_alive: true,
    };
    assert!(stats.worker_alive);
    for reason in [
        Unavailable::Disabled,
        Unavailable::NoSpace,
        Unavailable::Missing,
        Unavailable::NotAFile,
        Unavailable::Empty,
        Unavailable::TooLarge { bytes: 1 },
        Unavailable::Unreadable,
        Unavailable::NotAnImage,
        Unavailable::TooManyPixels { px_w: 1, px_h: 1 },
        Unavailable::EncodeFailed,
        Unavailable::WorkerFailed,
    ] {
        assert!(!reason.to_string().is_empty());
    }
    for protocol in [
        ImageProtocol::Kitty,
        ImageProtocol::Sixel,
        ImageProtocol::Iterm2,
    ] {
        assert!(!protocol.name().is_empty());
    }
    let _ = CellPixels {
        width: 1,
        height: 2,
    };
}

/// A disabled store always answers `Unavailable`, which is all this helper needs.
fn store_plan() -> ImageState {
    let mut store = ImageStore::new(ImageSupport::disabled());
    store.request(Path::new("/nonexistent.png"), Size::new(1, 1))
}

const CELL: CellPixels = CellPixels::new(10, 20);

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

/// Self-cleaning temp directory (the crate has no `tempfile` dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let unique = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wing-image-it-{}-{tag}-{unique}",
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

/// Drive the store the way the event loop does: poll, re-plan, sleep.
fn poll_until(store: &mut ImageStore, what: &str, mut done: impl FnMut(&mut ImageStore) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        store.poll();
        if done(store) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_disabled_terminal_keeps_the_legacy_rendering_bit_for_bit() {
    let dir = TempDir::new("disabled");
    let path = dir.file("plot.png");
    write_png(&path, 300, 100);

    let mut store = ImageStore::new(ImageSupport::disabled());
    let area = Rect::new(0, 0, 30, 4);
    let mut buf = Buffer::empty(area);
    // Whatever the markdown renderer draws for `![plot](plot.png)` today.
    buf.set_string(0, 0, "![plot](plot.png)", Style::default());
    let legacy = buf.clone();

    // This is the *exact* branch step 06 writes around the image pipeline.
    if let ImageState::Ready(image) = store.request(&path, Size::new(20, 4)) {
        paint(&image, area, (0, 0), &mut buf);
    }

    assert_eq!(buf, legacy, "a disabled terminal must not touch the buffer");
    assert!(!store.poll());
    let stats = store.stats();
    assert_eq!(stats.cached, 0);
    assert_eq!(stats.in_flight, 0, "a disabled terminal must queue no work");
    assert_eq!(stats.memo, 0, "a disabled terminal must read no files");
    assert!(matches!(
        store.request(&path, Size::new(20, 4)),
        ImageState::Unavailable(_)
    ));
}

#[test]
fn an_enabled_terminal_reports_metadata_then_paints_the_image() {
    let dir = TempDir::new("enabled");
    let path = dir.file("plot.png");
    write_png(&path, 300, 100);

    let support = ImageSupport::from_parts(ImageProtocol::Kitty, CELL, false);
    assert!(support.is_enabled());
    assert_eq!(support.protocol(), Some(ImageProtocol::Kitty));
    assert_eq!(support.cell_pixel_size(), Some(CELL));
    let mut store = ImageStore::new(support);

    // Frame 1: the header probe has not answered yet, so the caller draws the fallback.
    assert_eq!(store.meta(&path), MetaState::Unknown);
    assert!(matches!(
        store.request(&path, Size::new(30, 5)),
        ImageState::Pending
    ));

    poll_until(&mut store, "metadata", |store| {
        matches!(store.meta(&path), MetaState::Known(_))
    });
    let MetaState::Known(meta) = store.meta(&path) else {
        panic!("expected known metadata");
    };
    assert_eq!((meta.px_w, meta.px_h), (300, 100));
    assert!(meta.bytes > 0);

    // Frame 2: the encode lands and the image can be painted.
    let target = Size::new(30, 5);
    assert!(matches!(store.request(&path, target), ImageState::Pending));
    poll_until(&mut store, "the image", |store| {
        matches!(store.request(&path, target), ImageState::Ready(_))
    });
    let ImageState::Ready(image) = store.request(&path, target) else {
        panic!("expected a ready image");
    };
    assert_eq!(image.protocol(), ImageProtocol::Kitty);
    assert_eq!(image.size(), Size::new(30, 5));

    let area = Rect::new(0, 0, 40, 10);
    let mut buf = Buffer::empty(area);
    let covered = paint(&image, area, (0, 0), &mut buf).expect("the image fits");
    assert_eq!(covered, Rect::new(0, 0, 30, 5));
    assert!(buf[(0, 0)].symbol().contains('\u{10EEEE}'));

    // Scrolled half out of the viewport: still painted, but only over what is visible.
    let mut offset_buf = Buffer::empty(area);
    let covered = paint(&image, area, (2, -3), &mut offset_buf).expect("partially visible");
    assert_eq!(covered, Rect::new(2, 0, 30, 2));

    // A second lookup must not queue anything: the cache answers.
    assert!(matches!(store.request(&path, target), ImageState::Ready(_)));
    assert_eq!(store.stats().in_flight, 0);
    assert_eq!(store.stats().cached, 1);
    assert!(store.stats().worker_alive);

    // Invalidation is the caller's answer to `terminal.clear()` / resize / font change: it
    // drops the encodings *and* revokes the handles the caller is still holding, because the
    // transmit sequence is one-shot and the terminal may have thrown the image away.
    store.invalidate();
    assert!(!image.is_current(), "the handed-out image must be revoked");
    let mut revoked = Buffer::empty(area);
    assert_eq!(paint(&image, area, (0, 0), &mut revoked), None);
    assert!(matches!(store.request(&path, target), ImageState::Pending));

    poll_until(&mut store, "the re-encode", |store| {
        matches!(store.request(&path, target), ImageState::Ready(_))
    });
    let ImageState::Ready(fresh) = store.request(&path, target) else {
        panic!("expected a fresh image");
    };
    assert!(fresh.is_current());
    assert!(paint(&fresh, area, (0, 0), &mut revoked).is_some());
}

#[test]
fn missing_files_degrade_to_a_reason_instead_of_an_error() {
    let dir = TempDir::new("missing");
    let mut store = ImageStore::new(ImageSupport::from_parts(ImageProtocol::Sixel, CELL, false));
    let path = dir.file("nope.png");

    assert_eq!(store.meta(&path), MetaState::Unknown);
    poll_until(&mut store, "the failure", |store| {
        store.meta(&path) != MetaState::Unknown
    });
    assert!(matches!(store.meta(&path), MetaState::Unavailable(_)));
    match store.request(&path, Size::new(10, 5)) {
        ImageState::Unavailable(reason) => assert!(!reason.to_string().is_empty()),
        other => panic!("expected an unavailable image, got {other:?}"),
    }
}
