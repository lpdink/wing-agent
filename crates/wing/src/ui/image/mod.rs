//! Terminal graphics: capability detection, an image cache, and the buffer placement
//! primitive.
//!
//! This is the crate's only door to `ratatui-image` and `image`; everything else works with
//! the narrow types re-exported here, so a future protocol swap stays inside this module.
//!
//! # The shape of it
//!
//! ```text
//!  ImageSupport::detect()          ─ what can this terminal draw? (injectable: from_parts)
//!      │
//!  ImageStore::meta(path)          ─ header-only facts (size/aspect, no decode)
//!  ImageStore::request(path, size) ─ Pending | Ready | Unavailable, never blocking
//!      │  ▲ poll()                   worker thread: decode + encode
//!      │  └──── waker() ──────────────┘
//!  place::paint(&image, area, offset, buf) ─ the only buffer write
//! ```
//!
//! # The two-tier rule, enforced mechanically
//!
//! `ImageSupport::is_enabled() == false` (no graphics protocol, or images turned off) means
//! [`ImageStore::request`] and [`ImageStore::meta`] answer `Unavailable(Disabled)`, **no job
//! is queued, no file is read**, and the caller's existing text rendering — the link path for
//! a markdown image — runs unchanged. There is no half-way mode: no half-blocks, no mosaics.
//!
//! # Threading
//!
//! The store has a single owner (the UI thread) and holds `Arc`-shared encoded protocols
//! that get painted by that same owner. Detection, decoding and encoding happen on one worker
//! thread owned by the store (see [`store`]). Nothing here blocks the render path on I/O.
//!
//! # Failure is a state, not an exception
//!
//! Missing files, directories, empty files, oversized files, text re-named `.png`, corrupt
//! headers: all of them are [`Unavailable`] values, memoised so the store does not keep
//! re-reading a file it already gave up on. Nothing in this module panics because of a file.

pub mod place;
pub mod probe;
pub mod store;

mod encode;
mod meta;

#[cfg(test)]
mod test_support;

pub use meta::{ImageMeta, Unavailable};
pub use place::{ReadyImage, paint};
pub use probe::{CellPixels, DEFAULT_DETECT_TIMEOUT, ImageProtocol, ImageSupport};
pub use store::{
    DEFAULT_CACHE_BYTES, DEFAULT_CACHE_ENTRIES, DEFAULT_FILE_BYTES, DEFAULT_PIXELS, ImageState,
    ImageStore, ImageStoreConfig, Limits, MAX_META_ENTRIES, MetaState, StoreStats,
};
