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
//! * vocabulary — the shared magic strings ([`constants`]) and the goal display
//!   role ([`goal_role`]).
//!
//! **Admission rule.** Only state or vocabulary that *both* sides need, that is
//! free of I/O and that knows neither the App nor the UI may live here; anything
//! with a single consumer stays on the side that consumes it (`app/popup_state.rs`
//! is the current example). The dependency direction is pinned by
//! `tests/layer_guard.rs`.

pub mod constants;
pub mod goal_role;
pub mod panels;
