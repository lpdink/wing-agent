//! AppIntent — side-effect declarations from the App state machine.
//!
//! All operations from App to the external world (gateway, clipboard) are
//! expressed as `AppIntent` variants. The runner drains pending intents after
//! each draw and executes them in order.

/// A side-effect intent produced by the App state machine.
///
/// The runner calls `drain_intents()` after each draw cycle and matches on
/// each variant to perform the corresponding I/O operation.
pub enum AppIntent {
    /// Send a user message to the current session via gateway.
    SendMessage { content: String },

    /// Send a silent request (e.g. popup candidate fetching) via gateway.
    SilentRequest { content: String, request_id: String },

    /// Write text to clipboard via OSC52 escape sequence.
    CopyToClipboard(String),

    /// Create a new session via HTTP API.
    CreateSession { workspace: Option<String> },

    /// Resume an existing session via HTTP API.
    ResumeSession { session_id: String },

    /// Fork from a branch target via HTTP API.
    ForkSession { target_uuid: String },

    /// Fetch session list via HTTP API for popup candidates.
    FetchSessionList,
}
