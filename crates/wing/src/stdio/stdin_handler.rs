//! stdin handler for `--input-format stream-json` mode.
//!
//! This module will handle reading prompts from stdin in NDJSON format
//! and performing the initialize handshake (sub-task 4).
//!
//! Currently a stub — implementation deferred to sub-task 4.

// TODO: Implement stdin NDJSON reading and initialize handshake.
// This is sub-task 4 scope.

/// Placeholder for the stdin handler.
/// Will be implemented in sub-task 4.
pub struct StdinHandler {
    _private: (),
}

impl StdinHandler {
    /// Create a new stdin handler.
    pub fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for StdinHandler {
    fn default() -> Self {
        Self::new()
    }
}
