//! The neutral layer: state machines and vocabulary shared by the App and the UI.
//!
//! `app/` orchestrates and `ui/` renders; whatever **both** need lives here,
//! because neither side owns it. The layer is *neutral*: it holds zero I/O,
//! names nothing above it (`app`, `ui`, `cmd`, `stdio`, `gateway`, `tui`) and
//! reaches for no render library — `tests/layer_guard.rs` pins all of those
//! directions on every `cargo test`.
//!
//! Two kinds of content live here:
//!
//! * [`panels`] — the interactive selection state machines (kernel + adapters):
//!   the App drives them (keys, refresh, lifetime) and the UI renders them;
//! * vocabulary — the shared magic strings ([`constants`]) and the tip pool
//!   ([`tips`]), plus the pin convention ([`pinning`]: which tag means
//!   "pinned" and how pinned sessions sort first — one implementation shared
//!   by the session panel, the status bar and `wing ps`), the session-lineage
//!   forest ([`session_tree`]: flat session rows → tree order + drawing
//!   prefixes — one implementation shared by the session panel and
//!   `wing ps --tree`), and the sparse settings document primitives
//!   ([`doc_edit`]: canonical-path algebra, the stub built from the catalog,
//!   list add/remove/move — one implementation shared by the settings panel
//!   and `wing config`, with their two UX policies spelled out per entry
//!   point).
//!
//! `text_edit` (single-line editing: char ↔ byte plus insert / backspace /
//! delete / move) sits here for the same neutral-layer reason: the Ask panel,
//! the settings editor and the composer all need it, and `shared` cannot reach
//! up into the UI.
//!
//! **Admission rule.** Only state or vocabulary that *both* sides need, that is
//! free of I/O and that knows neither the App nor the UI may live here; anything
//! with a single consumer stays on the side that consumes it (`app/popup_state.rs`
//! is the current example). The dependency direction is pinned by
//! `tests/layer_guard.rs`.

pub mod constants;
pub mod doc_edit;
pub mod panels;
pub mod pinning;
pub mod session_tree;
pub(crate) mod text_edit;
pub mod tips;
