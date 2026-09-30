//! The image store: a non-blocking, three-state cache in front of a decode/encode worker.
//!
//! # Threading
//!
//! ```text
//! UI thread ──Job { generation }──► mpsc ──► worker thread (decode + encode)
//! UI thread ◄──────Done { generation }───── std mpsc ◄──┘
//!                                                  └── waker() ⟶ wake the host's event loop
//! ```
//!
//! The UI thread does nothing but hash lookups: [`ImageStore::request`] /
//! [`ImageStore::meta`] return immediately, and [`ImageStore::poll`] moves finished work into
//! the cache. Every file read, decode and protocol encode happens on the worker.
//!
//! The store is UI-thread state with a single owner and no locking (the cache holds
//! `Arc`-shared protocols that the same thread paints). The worker never touches it — it sees
//! only jobs, results and an `AtomicU64` generation. The whole pipeline is nevertheless `Send`
//! (asserted in the tests), so the App can hold the store without any thread-affinity dance.
//!
//! # Lifecycle
//!
//! Dropping the store drops the job sender, so the worker's `recv()` fails and the thread
//! exits after finishing whatever it was doing. There is no daemon and nothing to join.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::SystemTime;

use ratatui::layout::Size;
use ratatui_image::sliced::SlicedProtocol;

use super::encode;
use super::meta::{self, ImageMeta, Probed, Unavailable};
use super::place::ReadyImage;
use super::probe::{CellPixels, ImageProtocol, ImageSupport};

/// Default number of encoded images kept in the LRU.
///
/// A viewport shows one to three images at a time; eight covers a screenful of scrolling
/// back and forth, which is as far as "still on screen soon" goes.
pub const DEFAULT_CACHE_ENTRIES: usize = 8;

/// Default approximate memory budget for the LRU.
///
/// ~4 images of 1200×800 at 4 bytes per pixel (~3.8 MiB each). The estimate is the decoded
/// footprint (`cells × cell pixels × 4`), which is the peak allocation; the encoded payload
/// (base64 / sixel text) is the same order of magnitude and not counted separately.
pub const DEFAULT_CACHE_BYTES: usize = 24 * 1024 * 1024;

/// Default file size ceiling — beyond this the file is not an image we want to decode.
pub const DEFAULT_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Default pixel ceiling (≈ 4000×4000 at 4 bytes per pixel ≈ 64 MiB of decode buffer).
pub const DEFAULT_PIXELS: u64 = 16_000_000;

/// Upper bound on memoised metadata entries.
///
/// Metadata entries are tiny (~100 bytes); rather than run a second LRU for them, the memo is
/// dropped wholesale when it grows past this. Re-probing is cheap and only costs a frame.
pub const MAX_META_ENTRIES: usize = 256;

/// Store policy: what to cache and what to refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum number of encoded images held in the LRU.
    pub cache_entries: usize,
    /// Approximate maximum bytes of decoded image data held in the LRU.
    pub cache_bytes: usize,
    /// Files larger than this are refused without reading them.
    pub file_bytes: u64,
    /// Images with more pixels than this are refused from their header alone.
    pub pixels: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cache_entries: DEFAULT_CACHE_ENTRIES,
            cache_bytes: DEFAULT_CACHE_BYTES,
            file_bytes: DEFAULT_FILE_BYTES,
            pixels: DEFAULT_PIXELS,
        }
    }
}

/// Construction options for [`ImageStore`].
#[derive(Default)]
pub struct ImageStoreConfig {
    /// Cache and file policy.
    pub limits: Limits,
    /// Called from the worker thread after each finished job.
    ///
    /// The host uses it to wake its event loop (e.g. by poking a tokio channel) instead of
    /// polling on a timer. `None` means "poll [`ImageStore::poll`] every frame", which is
    /// always correct, just up to one frame later.
    pub waker: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// What we know about a file's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaState {
    /// The header is known.
    Known(ImageMeta),
    /// A probe is queued or in flight; ask again after [`ImageStore::poll`] reports a change.
    Unknown,
    /// Permanently unavailable; the reason is memoised, no further I/O happens.
    Unavailable(Unavailable),
}

/// The three states of [`ImageStore::request`].
#[derive(Debug, Clone)]
pub enum ImageState {
    /// Encoding is queued or in flight — draw the fallback for now.
    Pending,
    /// Encoded and cached; paint it with [`super::place::paint`].
    Ready(ReadyImage),
    /// Nothing to draw, ever (for this target size); keep the existing rendering.
    Unavailable(Unavailable),
}

/// Cache/worker counters. Diagnostics and tests only — the UI has no use for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreStats {
    /// Encoded images currently cached.
    pub cached: usize,
    /// Approximate bytes those images occupy.
    pub cached_bytes: usize,
    /// Encode jobs queued or running.
    pub in_flight: usize,
    /// Memoised metadata entries (known or unavailable).
    pub memo: usize,
    /// Memoised metadata entries whose header parsed successfully.
    pub known_meta: usize,
}

/// Identity of an encoded image: the file (by canonical path and mtime, so a rewrite is a
/// different key) plus what it was encoded *for* (target cells and the terminal's cell size,
/// so a resize, a re-layout or a font change is a different key too).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EncodeKey {
    canonical: PathBuf,
    mtime: Option<SystemTime>,
    target: Size,
    cell: CellPixels,
}

/// Per-path metadata bookkeeping.
struct MetaSlot {
    /// Bumped on every probe request; a result whose sequence is stale is dropped, which is
    /// what makes [`ImageStore::refresh`] authoritative over an in-flight probe.
    seq: u64,
    canonical: PathBuf,
    state: MetaSlotState,
}

enum MetaSlotState {
    Pending,
    Known(ImageMeta),
    Unavailable(Unavailable),
}

/// One cached encoded image, with its approximate size for the byte budget.
struct Cached {
    key: EncodeKey,
    image: ReadyImage,
    bytes: usize,
}

/// LRU over encoded images. Capacity is single digits by design, so a `Vec` scan beats
/// pulling in a cache crate; index 0 is the least recently used.
#[derive(Default)]
struct Cache {
    entries: Vec<Cached>,
    bytes: usize,
}

impl Cache {
    fn get(&mut self, key: &EncodeKey) -> Option<ReadyImage> {
        let index = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(index);
        let image = entry.image.clone();
        self.entries.push(entry);
        Some(image)
    }

    fn put(&mut self, key: EncodeKey, image: ReadyImage, bytes: usize, limits: &Limits) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.bytes = self.bytes.saturating_sub(self.entries.remove(index).bytes);
        }
        self.entries.push(Cached { key, image, bytes });
        self.bytes = self.bytes.saturating_add(bytes);
        // Always keep the entry we just inserted: a single image larger than the whole budget
        // must still be cached, or every frame would re-encode it.
        let max_entries = limits.cache_entries.max(1);
        while self.entries.len() > max_entries
            || (self.bytes > limits.cache_bytes && self.entries.len() > 1)
        {
            let evicted = self.entries.remove(0);
            self.bytes = self.bytes.saturating_sub(evicted.bytes);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// A unit of work for the worker thread.
struct Job {
    generation: u64,
    kind: JobKind,
}

enum JobKind {
    Probe {
        path: PathBuf,
        seq: u64,
    },
    Encode {
        path: PathBuf,
        target: Size,
        key: EncodeKey,
    },
}

/// A finished unit of work, handed back to the UI thread.
struct Done {
    generation: u64,
    outcome: Outcome,
}

enum Outcome {
    Probed {
        path: PathBuf,
        seq: u64,
        result: Result<Probed, Unavailable>,
    },
    Encoded {
        key: EncodeKey,
        result: Result<SlicedProtocol, Unavailable>,
    },
}

/// What the UI thread should do next for a path.
enum Step {
    Probe,
    Waiting,
    Unavailable(Unavailable),
    Known { canonical: PathBuf, meta: ImageMeta },
}

/// The image store. Lives on the UI thread, owns the worker thread.
pub struct ImageStore {
    support: ImageSupport,
    limits: Limits,
    metas: HashMap<PathBuf, MetaSlot>,
    cache: Cache,
    in_flight: HashSet<EncodeKey>,
    failed: HashMap<EncodeKey, Unavailable>,
    /// Bumped by every invalidation; results tagged with an older value are dropped.
    generation: u64,
    /// Same counter, shared with the worker so stale queued jobs can be skipped outright.
    shared_generation: Arc<AtomicU64>,
    seq: u64,
    /// `None` when there is no worker (disabled terminal, or the thread could not start).
    jobs: Option<Sender<Job>>,
    done: Receiver<Done>,
}

impl ImageStore {
    /// A store with default limits and no waker.
    pub fn new(support: ImageSupport) -> Self {
        Self::with_config(support, ImageStoreConfig::default())
    }

    /// A store with explicit limits and an optional wake callback.
    pub fn with_config(support: ImageSupport, config: ImageStoreConfig) -> Self {
        let (done_tx, done) = mpsc::channel();
        let shared_generation = Arc::new(AtomicU64::new(0));
        let jobs = if support.is_enabled() {
            let (job_tx, job_rx) = mpsc::channel::<Job>();
            let worker = Worker {
                done: done_tx,
                generation: Arc::clone(&shared_generation),
                support: support.clone(),
                limits: config.limits,
                waker: config.waker,
            };
            thread::Builder::new()
                .name("wing-image".to_string())
                .spawn(move || worker.run(&job_rx))
                .ok()
                .map(|_| job_tx)
        } else {
            None
        };
        Self {
            support,
            limits: config.limits,
            metas: HashMap::new(),
            cache: Cache::default(),
            in_flight: HashSet::new(),
            failed: HashMap::new(),
            generation: 0,
            shared_generation,
            seq: 0,
            jobs,
            done,
        }
    }

    /// The capability this store was built for.
    pub fn support(&self) -> &ImageSupport {
        &self.support
    }

    /// Whether this store can do anything at all.
    ///
    /// False for a disabled terminal *and* for the (pathological) case where the worker
    /// thread could not be started — in both cases no work is queued and no file is read.
    fn can_work(&self) -> bool {
        self.support.is_enabled() && self.jobs.is_some()
    }

    /// Non-blocking metadata lookup. The first call queues a header probe and answers
    /// [`MetaState::Unknown`]; call again once [`ImageStore::poll`] reports a change.
    ///
    /// When the terminal cannot show images this is [`Unavailable::Disabled`] without I/O —
    /// callers keep their existing rendering and never pay for a probe they cannot use.
    pub fn meta(&mut self, path: &Path) -> MetaState {
        if !self.can_work() {
            return MetaState::Unavailable(Unavailable::Disabled);
        }
        match self.step(path) {
            Step::Probe => {
                self.enqueue_probe(path);
                MetaState::Unknown
            }
            Step::Waiting => MetaState::Unknown,
            Step::Unavailable(reason) => MetaState::Unavailable(reason),
            Step::Known { meta, .. } => MetaState::Known(meta),
        }
    }

    /// Non-blocking lookup of an encoded image sized for `target` cells.
    ///
    /// Repeat calls within the same frame are free: a path already being probed or encoded
    /// answers [`ImageState::Pending`] without queueing a second job.
    pub fn request(&mut self, path: &Path, target: Size) -> ImageState {
        if !self.can_work() {
            return ImageState::Unavailable(Unavailable::Disabled);
        }
        if target.width == 0 || target.height == 0 {
            return ImageState::Unavailable(Unavailable::NoSpace);
        }
        match self.step(path) {
            Step::Probe => {
                self.enqueue_probe(path);
                ImageState::Pending
            }
            Step::Waiting => ImageState::Pending,
            Step::Unavailable(reason) => ImageState::Unavailable(reason),
            Step::Known { canonical, meta } => {
                let key = self.encode_key(canonical, meta, target);
                if let Some(image) = self.cache.get(&key) {
                    return ImageState::Ready(image);
                }
                if let Some(reason) = self.failed.get(&key) {
                    return ImageState::Unavailable(reason.clone());
                }
                if self.in_flight.contains(&key) {
                    return ImageState::Pending;
                }
                self.enqueue_encode(path, target, key);
                ImageState::Pending
            }
        }
    }

    /// Move finished worker results into the cache.
    ///
    /// Returns `true` when something changed (metadata arrived, an image finished encoding,
    /// or a failure was recorded) — the host should redraw. Cheap and safe to call every
    /// frame, whether or not a [`ImageStoreConfig::waker`] is installed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(done) = self.done.try_recv() {
            changed |= self.absorb(done);
        }
        changed
    }

    /// Drop every encoded image and invalidate in-flight encodes.
    ///
    /// Call this whenever the terminal can no longer be trusted to still show what we sent:
    /// `terminal.clear()`, a window resize, a font-size (cell pixel) change, or a tmux
    /// passthrough change. Encoded protocols are *stateful* — `ratatui-image` remembers that
    /// a kitty image has been transmitted and would never send it again — so the only correct
    /// answer is to throw the protocols away and re-encode on the next request.
    ///
    /// Metadata is kept: a resize does not change what is on disk.
    pub fn invalidate(&mut self) {
        self.cache.clear();
        self.failed.clear();
        self.bump_generation();
    }

    /// [`ImageStore::invalidate`] plus dropping the metadata memo, so the next probe re-reads
    /// the file. Use when the caller knows the files on disk may have been replaced.
    pub fn reset(&mut self) {
        self.invalidate();
        self.metas.clear();
    }

    /// Forget what we know about one path, so the next [`ImageStore::meta`] re-reads it.
    ///
    /// This is how an mtime change is noticed: the store never re-`stat`s a path on its own
    /// (that would be I/O on the render path), so the caller announces the change.
    pub fn refresh(&mut self, path: &Path) {
        self.metas.remove(path);
        self.bump_generation();
    }

    /// Cache/worker counters.
    pub fn stats(&self) -> StoreStats {
        StoreStats {
            cached: self.cache.entries.len(),
            cached_bytes: self.cache.bytes,
            in_flight: self.in_flight.len(),
            memo: self.metas.len(),
            known_meta: self
                .metas
                .values()
                .filter(|slot| matches!(slot.state, MetaSlotState::Known(_)))
                .count(),
        }
    }

    fn step(&self, path: &Path) -> Step {
        match self.metas.get(path) {
            None => Step::Probe,
            Some(slot) => match &slot.state {
                MetaSlotState::Pending => Step::Waiting,
                MetaSlotState::Unavailable(reason) => Step::Unavailable(reason.clone()),
                MetaSlotState::Known(meta) => Step::Known {
                    canonical: slot.canonical.clone(),
                    meta: *meta,
                },
            },
        }
    }

    fn encode_key(&self, canonical: PathBuf, meta: ImageMeta, target: Size) -> EncodeKey {
        EncodeKey {
            canonical,
            mtime: meta.mtime,
            target,
            cell: self
                .support
                .cell_pixel_size()
                .unwrap_or_else(|| CellPixels::new(1, 1)),
        }
    }

    fn enqueue_probe(&mut self, path: &Path) {
        let Some(jobs) = self.jobs.as_ref() else {
            return;
        };
        self.seq += 1;
        let seq = self.seq;
        if self.metas.len() >= MAX_META_ENTRIES {
            // Tiny entries, huge session: start the memo over instead of growing forever.
            // In-flight probes for the dropped slots are discarded by their sequence check.
            self.metas.clear();
        }
        self.metas.insert(
            path.to_path_buf(),
            MetaSlot {
                seq,
                canonical: path.to_path_buf(),
                state: MetaSlotState::Pending,
            },
        );
        let _ = jobs.send(Job {
            generation: self.generation,
            kind: JobKind::Probe {
                path: path.to_path_buf(),
                seq,
            },
        });
    }

    fn enqueue_encode(&mut self, path: &Path, target: Size, key: EncodeKey) {
        let Some(jobs) = self.jobs.as_ref() else {
            return;
        };
        self.in_flight.insert(key.clone());
        let _ = jobs.send(Job {
            generation: self.generation,
            kind: JobKind::Encode {
                path: path.to_path_buf(),
                target,
                key,
            },
        });
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.in_flight.clear();
        self.shared_generation
            .store(self.generation, Ordering::Relaxed);
    }

    /// Apply one finished job. Returns whether the store changed.
    fn absorb(&mut self, done: Done) -> bool {
        match done.outcome {
            Outcome::Probed { path, seq, result } => {
                // A probe is a pure read, so it is only rejected when the caller has since
                // asked the same path again (`refresh` / a dropped memo): then the slot
                // carries a different sequence and this answer is obsolete.
                if self.metas.get(&path).map(|slot| slot.seq) != Some(seq) {
                    return false;
                }
                let (canonical, state) = match result {
                    Ok(probed) => (probed.canonical, MetaSlotState::Known(probed.meta)),
                    Err(reason) => (path.clone(), MetaSlotState::Unavailable(reason)),
                };
                self.metas.insert(
                    path,
                    MetaSlot {
                        seq,
                        canonical,
                        state,
                    },
                );
                true
            }
            Outcome::Encoded { key, result } => {
                if done.generation != self.generation {
                    // The terminal was invalidated while this was encoding; the protocol is
                    // no longer allowed to be shown (it would never be re-transmitted).
                    return false;
                }
                self.in_flight.remove(&key);
                match result {
                    Ok(protocol) => {
                        let kind = self.support.protocol().unwrap_or(ImageProtocol::Kitty);
                        let image = ReadyImage::new(protocol, kind);
                        let bytes = self.footprint_bytes(image.size());
                        self.cache.put(key, image, bytes, &self.limits);
                        true
                    }
                    Err(reason) => {
                        // Keyed by target size: retrying the same size would fail the same
                        // way, but a re-layout (or an invalidation) may succeed.
                        self.failed.insert(key, reason);
                        true
                    }
                }
            }
        }
    }

    /// Approximate decoded footprint of a rendered image, in bytes.
    fn footprint_bytes(&self, size: Size) -> usize {
        let cell = self
            .support
            .cell_pixel_size()
            .unwrap_or_else(|| CellPixels::new(1, 1));
        let pixels = u64::from(size.width)
            .saturating_mul(u64::from(cell.width))
            .saturating_mul(u64::from(size.height))
            .saturating_mul(u64::from(cell.height));
        usize::try_from(pixels.saturating_mul(4)).unwrap_or(usize::MAX)
    }
}

/// The worker thread's state. Holds no reference to the store.
struct Worker {
    done: Sender<Done>,
    generation: Arc<AtomicU64>,
    support: ImageSupport,
    limits: Limits,
    waker: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Worker {
    fn run(self, jobs: &Receiver<Job>) {
        while let Ok(job) = jobs.recv() {
            if job.generation != self.generation.load(Ordering::Relaxed) {
                // Invalidated while queued: the UI thread has already re-requested whatever
                // it still needs, so encoding this would be wasted work.
                continue;
            }
            let generation = job.generation;
            let outcome = self.execute(job.kind);
            if self
                .done
                .send(Done {
                    generation,
                    outcome,
                })
                .is_err()
            {
                // The store is gone; nothing left to report to.
                break;
            }
            if let Some(waker) = &self.waker {
                waker();
            }
        }
    }

    fn execute(&self, kind: JobKind) -> Outcome {
        match kind {
            JobKind::Probe { path, seq } => {
                let result = meta::probe(&path, &self.limits);
                Outcome::Probed { path, seq, result }
            }
            JobKind::Encode { path, target, key } => {
                let result = self.encode(&path, target);
                Outcome::Encoded { key, result }
            }
        }
    }

    fn encode(&self, path: &Path, target: Size) -> Result<SlicedProtocol, Unavailable> {
        let Some(protocol) = self.support.protocol() else {
            return Err(Unavailable::Disabled);
        };
        let Some(cell) = self.support.cell_pixel_size() else {
            return Err(Unavailable::Disabled);
        };
        let image = encode::decode(path)?;
        encode::encode(image, target, protocol, cell, self.support.is_tmux())
    }
}

// `ImageProtocol` is used in `absorb`; keep the import local to that path.
#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::ui::image::meta::write_png_fixture;
    use crate::ui::image::probe::{CellPixels, ImageProtocol};
    use crate::ui::image::test_support::TempDir;

    const CELL: CellPixels = CellPixels::new(10, 20);

    fn enabled(protocol: ImageProtocol) -> ImageSupport {
        ImageSupport::from_parts(protocol, CELL, false)
    }

    /// A waker that counts calls, so tests can tell "the worker finished something" from
    /// "the store happened to answer from its cache".
    fn counting_waker() -> (Arc<AtomicUsize>, Arc<dyn Fn() + Send + Sync>) {
        let counter = Arc::new(AtomicUsize::new(0));
        let handle: Arc<dyn Fn() + Send + Sync> = {
            let counter = Arc::clone(&counter);
            Arc::new(move || {
                counter.fetch_add(1, AtomicOrdering::SeqCst);
            })
        };
        (counter, handle)
    }

    fn store_with(
        protocol: ImageProtocol,
        limits: Limits,
        waker: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> ImageStore {
        ImageStore::with_config(enabled(protocol), ImageStoreConfig { limits, waker })
    }

    /// Poll until `check` holds or the deadline passes.
    fn pump_until(
        store: &mut ImageStore,
        what: &str,
        mut check: impl FnMut(&mut ImageStore) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            store.poll();
            if check(store) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn ready(store: &mut ImageStore, path: &Path, target: Size) -> ReadyImage {
        pump_until(store, "a ready image", |store| {
            matches!(store.request(path, target), ImageState::Ready(_))
        });
        match store.request(path, target) {
            ImageState::Ready(image) => image,
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    fn fixture(dir: &TempDir, name: &str, px_w: u32, px_h: u32) -> PathBuf {
        let path = dir.path().join(name);
        write_png_fixture(&path, px_w, px_h);
        path
    }

    #[test]
    fn disabled_store_answers_without_touching_anything() {
        let dir = TempDir::new("store-disabled");
        let path = fixture(&dir, "plot.png", 40, 30);
        let mut store = ImageStore::new(ImageSupport::disabled());

        assert_eq!(
            store.meta(&path),
            MetaState::Unavailable(Unavailable::Disabled)
        );
        assert!(matches!(
            store.request(&path, Size::new(10, 5)),
            ImageState::Unavailable(Unavailable::Disabled)
        ));
        assert!(!store.poll());
        let stats = store.stats();
        assert_eq!(stats.memo, 0, "a disabled store must not probe");
        assert_eq!(stats.in_flight, 0, "a disabled store must not queue work");
        assert_eq!(stats.cached, 0);
    }

    #[test]
    fn metadata_arrives_asynchronously_and_is_then_memoised() {
        let dir = TempDir::new("store-meta");
        let path = fixture(&dir, "plot.png", 320, 200);
        let (waker_calls, waker) = counting_waker();
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), Some(waker));

        assert_eq!(store.meta(&path), MetaState::Unknown);
        pump_until(&mut store, "metadata", |store| {
            matches!(store.meta(&path), MetaState::Known(_))
        });
        let MetaState::Known(meta) = store.meta(&path) else {
            panic!("expected known metadata");
        };
        assert_eq!((meta.px_w, meta.px_h), (320, 200));
        assert!(waker_calls.load(AtomicOrdering::SeqCst) >= 1);

        // Deleting the file proves the answer came from the memo, not from disk.
        fs::remove_file(&path).expect("remove fixture");
        assert_eq!(store.meta(&path), MetaState::Known(meta));
    }

    #[test]
    fn unavailable_metadata_is_memoised_too() {
        let dir = TempDir::new("store-meta-missing");
        let path = dir.path().join("gone.png");
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);

        assert_eq!(store.meta(&path), MetaState::Unknown);
        pump_until(&mut store, "the failure", |store| {
            matches!(store.meta(&path), MetaState::Unavailable(_))
        });
        assert_eq!(
            store.meta(&path),
            MetaState::Unavailable(Unavailable::Missing)
        );
        // Creating the file afterwards does not change the memoised answer: the store never
        // re-stats on its own (that would be I/O on the render path).
        fs::write(&path, b"now it exists").expect("write");
        assert_eq!(
            store.meta(&path),
            MetaState::Unavailable(Unavailable::Missing)
        );
        // ... until the caller says so.
        store.refresh(&path);
        assert_eq!(store.meta(&path), MetaState::Unknown);
    }

    #[test]
    fn request_round_trip_then_cache_hit() {
        let dir = TempDir::new("store-roundtrip");
        let path = fixture(&dir, "plot.png", 300, 100);
        let (waker_calls, waker) = counting_waker();
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), Some(waker));
        let target = Size::new(20, 5);

        assert!(matches!(store.request(&path, target), ImageState::Pending));
        let image = ready(&mut store, &path, target);
        assert_eq!(image.protocol(), ImageProtocol::Kitty);
        assert!(image.size().width <= target.width && image.size().height <= target.height);
        assert_eq!(store.stats().cached, 1);
        assert!(
            waker_calls.load(AtomicOrdering::SeqCst) >= 2,
            "probe + encode"
        );

        // Second visit: cache hit, no new job, no new wake-up.
        let calls = waker_calls.load(AtomicOrdering::SeqCst);
        let again = ready(&mut store, &path, target);
        assert_eq!(again.size(), image.size());
        assert_eq!(waker_calls.load(AtomicOrdering::SeqCst), calls);
        assert_eq!(store.stats().in_flight, 0);
    }

    #[test]
    fn repeated_requests_inside_one_frame_queue_a_single_job() {
        let dir = TempDir::new("store-dedup");
        let path = fixture(&dir, "plot.png", 300, 100);
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);
        let target = Size::new(20, 5);

        assert!(matches!(store.request(&path, target), ImageState::Pending));
        // The probe is queued; the second call sees the pending slot and enqueues nothing.
        pump_until(&mut store, "metadata", |store| {
            matches!(store.meta(&path), MetaState::Known(_))
        });
        assert!(matches!(store.request(&path, target), ImageState::Pending));
        assert!(matches!(store.request(&path, target), ImageState::Pending));
        assert!(matches!(store.request(&path, target), ImageState::Pending));
        assert_eq!(store.stats().in_flight, 1);
    }

    #[test]
    fn a_different_target_size_is_encoded_separately() {
        let dir = TempDir::new("store-retarget");
        let path = fixture(&dir, "plot.png", 300, 100);
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);

        let small = ready(&mut store, &path, Size::new(10, 3));
        assert_eq!(store.stats().cached, 1);
        assert!(matches!(
            store.request(&path, Size::new(40, 12)),
            ImageState::Pending
        ));
        let large = ready(&mut store, &path, Size::new(40, 12));
        assert!(large.size().width > small.size().width);
        assert_eq!(store.stats().cached, 2);
    }

    #[test]
    fn a_rewritten_file_re_encodes_after_refresh() {
        let dir = TempDir::new("store-mtime");
        let path = fixture(&dir, "plot.png", 300, 100);
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);
        let target = Size::new(20, 5);

        ready(&mut store, &path, target);
        let MetaState::Known(before) = store.meta(&path) else {
            panic!("expected known metadata");
        };

        // Rewrite with different dimensions, retrying until the filesystem reports a new
        // mtime (the cache identity includes it, so this is what makes the entry stale).
        let mut after = before;
        for _ in 0..50 {
            write_png_fixture(&path, 600, 200);
            let probed = meta::probe(&path, &Limits::default()).expect("probe");
            if probed.meta.mtime != before.mtime {
                after = probed.meta;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_ne!(after.mtime, before.mtime, "the fixture mtime never changed");

        // Without a refresh the store keeps answering from its memo...
        assert_eq!(store.meta(&path), MetaState::Known(before));
        store.refresh(&path);
        assert_eq!(store.meta(&path), MetaState::Unknown);
        pump_until(&mut store, "the new metadata", |store| {
            store.meta(&path) == MetaState::Known(after)
        });

        // ...and the new mtime is a different cache key, so the image re-encodes.
        assert!(matches!(store.request(&path, target), ImageState::Pending));
        let image = ready(&mut store, &path, target);
        assert_eq!(image.size().width, target.width);
    }

    #[test]
    fn invalidate_never_hands_back_the_old_protocol() {
        let dir = TempDir::new("store-invalidate");
        let path = fixture(&dir, "plot.png", 300, 100);
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);
        let target = Size::new(20, 5);

        let first = ready(&mut store, &path, target);
        store.invalidate();
        assert_eq!(store.stats().cached, 0);
        // The very next request must not see the encodings from before the clear: a kitty
        // protocol that was already transmitted would never be sent again.
        assert!(matches!(store.request(&path, target), ImageState::Pending));
        assert_eq!(store.stats().in_flight, 1);

        let second = ready(&mut store, &path, target);
        assert!(
            !second.same_encoding(&first),
            "the invalidated protocol was reused"
        );
    }

    #[test]
    fn reset_forgets_metadata_but_invalidate_keeps_it() {
        let dir = TempDir::new("store-reset");
        let path = fixture(&dir, "plot.png", 300, 100);
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);
        ready(&mut store, &path, Size::new(20, 5));
        assert_eq!(store.stats().memo, 1);

        store.invalidate();
        assert_eq!(store.stats().memo, 1, "metadata survives a resize/clear");

        store.reset();
        assert_eq!(store.stats().memo, 0);
        assert_eq!(store.meta(&path), MetaState::Unknown);
    }

    #[test]
    fn the_lru_evicts_the_least_recently_used_entry() {
        let dir = TempDir::new("store-lru");
        let first = fixture(&dir, "a.png", 100, 100);
        let second = fixture(&dir, "b.png", 100, 100);
        let third = fixture(&dir, "c.png", 100, 100);
        let limits = Limits {
            cache_entries: 2,
            ..Limits::default()
        };
        let mut store = store_with(ImageProtocol::Kitty, limits, None);
        let target = Size::new(10, 5);

        for path in [&first, &second, &third] {
            ready(&mut store, path, target);
        }
        assert_eq!(store.stats().cached, 2, "the entry count is capped");
        // `first` fell out of the cache, so it must be re-encoded...
        assert!(matches!(store.request(&first, target), ImageState::Pending));
        ready(&mut store, &first, target);
        // ...while the most recent one is still a hit.
        assert!(matches!(
            store.request(&third, target),
            ImageState::Ready(_)
        ));
    }

    #[test]
    fn the_byte_budget_keeps_at_least_one_entry() {
        let dir = TempDir::new("store-bytes");
        let path = fixture(&dir, "big.png", 400, 200);
        let limits = Limits {
            cache_bytes: 1,
            ..Limits::default()
        };
        let mut store = store_with(ImageProtocol::Kitty, limits, None);
        let target = Size::new(40, 20);

        ready(&mut store, &path, target);
        assert_eq!(store.stats().cached, 1);
        assert!(store.stats().cached_bytes > limits.cache_bytes);
        // The oversized entry is still cached: otherwise every frame would re-encode it.
        assert!(matches!(store.request(&path, target), ImageState::Ready(_)));
    }

    #[test]
    fn the_metadata_memo_is_bounded() {
        let dir = TempDir::new("store-memo");
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);
        for index in 0..MAX_META_ENTRIES + 1 {
            let path = dir.path().join(format!("absent-{index}.png"));
            assert_eq!(
                store.meta(&path),
                MetaState::Unknown,
                "the first probe is asynchronous"
            );
            pump_until(&mut store, "the probe", |store| {
                store.meta(&path) != MetaState::Unknown
            });
        }
        assert!(
            store.stats().memo <= MAX_META_ENTRIES,
            "the memo grew past its bound: {}",
            store.stats().memo
        );
    }

    #[test]
    fn a_zero_sized_target_is_reported_as_no_space() {
        let dir = TempDir::new("store-nospace");
        let path = fixture(&dir, "plot.png", 300, 100);
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);
        assert!(matches!(
            store.request(&path, Size::new(0, 5)),
            ImageState::Unavailable(Unavailable::NoSpace)
        ));
        assert!(matches!(
            store.request(&path, Size::new(10, 0)),
            ImageState::Unavailable(Unavailable::NoSpace)
        ));
        assert_eq!(store.stats().in_flight, 0);
    }

    #[test]
    fn a_file_that_is_not_an_image_fails_once_and_stays_failed() {
        let dir = TempDir::new("store-notimage");
        let path = dir.path().join("liar.png");
        fs::write(&path, b"definitely not a png").expect("write");
        let mut store = store_with(ImageProtocol::Kitty, Limits::default(), None);

        assert_eq!(store.meta(&path), MetaState::Unknown);
        pump_until(&mut store, "the failure", |store| {
            matches!(store.meta(&path), MetaState::Unavailable(_))
        });
        assert_eq!(
            store.meta(&path),
            MetaState::Unavailable(Unavailable::NotAnImage)
        );
        assert!(matches!(
            store.request(&path, Size::new(10, 5)),
            ImageState::Unavailable(Unavailable::NotAnImage)
        ));
        assert_eq!(store.stats().in_flight, 0);
    }

    #[test]
    fn oversized_files_never_reach_the_decoder() {
        let dir = TempDir::new("store-toolarge");
        let path = fixture(&dir, "plot.png", 300, 100);
        let limits = Limits {
            file_bytes: 16,
            ..Limits::default()
        };
        let mut store = store_with(ImageProtocol::Kitty, limits, None);
        assert!(matches!(
            store.request(&path, Size::new(10, 5)),
            ImageState::Pending
        ));
        pump_until(&mut store, "the refusal", |store| {
            matches!(
                store.request(&path, Size::new(10, 5)),
                ImageState::Unavailable(_)
            )
        });
        let ImageState::Unavailable(reason) = store.request(&path, Size::new(10, 5)) else {
            panic!("expected a refusal");
        };
        assert!(
            matches!(reason, Unavailable::TooLarge { .. }),
            "unexpected reason: {reason:?}"
        );
    }

    #[test]
    fn the_store_can_be_moved_by_the_app_loop() {
        // Not a requirement of this layer (it is designed as single-owner UI state), but the
        // whole pipeline — protocols, worker channels, waker — is `Send`, so the App can hold
        // the store across an `await` without a refactor.
        fn assert_send<T: Send>() {}
        assert_send::<ImageStore>();
        assert_send::<ReadyImage>();
    }

    #[test]
    fn every_image_protocol_round_trips_through_the_store() {
        let dir = TempDir::new("store-protocols");
        for protocol in [
            ImageProtocol::Kitty,
            ImageProtocol::Sixel,
            ImageProtocol::Iterm2,
        ] {
            let path = fixture(&dir, "plot.png", 200, 60);
            let mut store = store_with(protocol, Limits::default(), None);
            let image = ready(&mut store, &path, Size::new(20, 6));
            assert_eq!(image.protocol(), protocol);
            assert_eq!(image.size(), Size::new(20, 3));
        }
    }

    /// A worker with its own channels, for tests that drive the loop by hand.
    fn spawn_worker(generation: Arc<AtomicU64>) -> (Sender<Job>, Receiver<Done>, JoinHandle<()>) {
        let (job_tx, job_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = Worker {
            done: done_tx,
            generation,
            support: enabled(ImageProtocol::Kitty),
            limits: Limits::default(),
            waker: None,
        };
        let handle = thread::spawn(move || worker.run(&job_rx));
        (job_tx, done_rx, handle)
    }

    #[test]
    fn the_worker_thread_exits_when_its_channel_closes() {
        // No daemon, no join needed on the store side: dropping the sender ends the loop.
        let dir = TempDir::new("store-worker-exit");
        let path = fixture(&dir, "plot.png", 40, 30);
        let (jobs, done, handle) = spawn_worker(Arc::new(AtomicU64::new(0)));
        jobs.send(Job {
            generation: 0,
            kind: JobKind::Probe { path, seq: 1 },
        })
        .expect("send");
        drop(jobs);
        handle
            .join()
            .expect("the worker must exit once the job channel closes");
        assert!(matches!(
            done.recv().expect("one result").outcome,
            Outcome::Probed { .. }
        ));
    }

    #[test]
    fn the_worker_skips_jobs_from_an_invalidated_generation() {
        let dir = TempDir::new("store-worker-stale");
        let path = fixture(&dir, "plot.png", 40, 30);
        let generation = Arc::new(AtomicU64::new(0));
        let (jobs, done, handle) = spawn_worker(Arc::clone(&generation));

        // The store was invalidated (generation 1) while this job was still queued.
        generation.store(1, Ordering::Relaxed);
        jobs.send(Job {
            generation: 0,
            kind: JobKind::Probe { path, seq: 1 },
        })
        .expect("send");
        drop(jobs);
        handle.join().expect("worker exit");
        assert!(
            done.try_recv().is_err(),
            "an invalidated job must not be executed"
        );
    }
}
