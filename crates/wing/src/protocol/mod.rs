//! WingEvent protocol types — Rust mirror of wing Python event definitions.
//!
//! Every event type from `wing/event/` is represented here with serde
//! derive for JSON (de)serialization. The `type` field acts as the
//! discriminator via `#[serde(tag = "type")]`.

mod client_request;
mod connect_response;
mod events;

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

pub use client_request::ClientRequest;
pub use connect_response::ConnectResponse;
pub use events::*;

/// Monotonic counter for generating unique request IDs within a process.
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique request ID.
///
/// Format: `wing_{counter}` where counter is a monotonically increasing u64.
/// Guaranteed collision-free within a single process lifetime.
pub fn generate_request_id() -> String {
    let id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("wing_{id}")
}
