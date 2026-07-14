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

    /// Fetch session runtime info (model, tokens, thinking, yolo) via HTTP API.
    FetchInfo,

    /// Fetch available commands list via HTTP API.
    FetchCommands,

    /// Fetch available model list via HTTP API for popup candidates.
    FetchModels,

    /// Fetch branch targets via HTTP API for popup candidates.
    FetchBranches,

    /// Fetch available agent template list via HTTP API for popup candidates.
    FetchAgents,

    /// Update session state (model, agent, title, thinking, yolo) via HTTP API.
    UpdateSession {
        model: Option<String>,
        agent: Option<String>,
        title: Option<String>,
        thinking: Option<bool>,
        yolo: Option<bool>,
    },

    /// Set the terminal title via OSC 0 escape sequence.
    SetTitle(String),

    /// Send a desktop notification via OSC 9 escape sequence.
    Notify(String),
}
