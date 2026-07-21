//! AppIntent — side-effect declarations from the App state machine.
//!
//! All operations from App to the external world (gateway, clipboard) are
//! expressed as `AppIntent` variants. The runner drains pending intents after
//! each draw and executes them in order.
//!
//! Fetch-type intents (read-only HTTP queries) are executed as background
//! tasks to avoid blocking the main event loop. Results are delivered back
//! via [`FetchResult`] through an mpsc channel.

use wing_api_client::models::{
    AgentsResponse, BranchesResponse, CommandsResponse, ModelsResponse, SessionInfoResponse,
    SessionListResponse,
};

/// Payload of a background fetch result.
pub enum FetchPayload {
    /// Session runtime info (model, tokens, thinking, yolo).
    Info(Box<SessionInfoResponse>),
    /// Available commands list.
    Commands(CommandsResponse),
    /// Available model list.
    Models(ModelsResponse),
    /// Branch targets for fork/rewind.
    Branches(BranchesResponse),
    /// Available agent templates.
    Agents(AgentsResponse),
    /// Session list.
    SessionList(SessionListResponse),
    /// Context stats display text.
    ContextInfo(String),
    /// Skills info display text.
    SkillsInfo(String),
    /// Compact session completed (original_tokens, compressed_tokens).
    CompactDone { original: i64, compressed: i64 },
    /// Show a toast message (for errors or success feedback from background tasks).
    Toast { message: String, is_error: bool },
}

/// Result of a background fetch intent, delivered via mpsc channel.
///
/// Carries the `session_id` that was active when the request was spawned,
/// so the receiver can discard stale results after a session switch.
pub struct FetchResult {
    /// Session ID at spawn time — used to discard stale results.
    pub session_id: String,
    /// The actual payload.
    pub payload: FetchPayload,
}

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

    /// Update session state (model, agent, title, thinking, reasoning_effort, yolo, workspace) via HTTP API.
    UpdateSession {
        model: Option<String>,
        agent: Option<String>,
        title: Option<String>,
        thinking: Option<bool>,
        reasoning_effort: Option<String>,
        yolo: Option<bool>,
        workspace: Option<String>,
    },

    /// Set the terminal title via OSC 0 escape sequence.
    SetTitle(String),

    /// Send a desktop notification via OSC 9 escape sequence.
    Notify(String),

    /// Compact the current session context via HTTP API.
    CompactSession,

    /// Interrupt the current agent turn via HTTP API.
    InterruptSession,

    /// Rewind the session to a specific message via HTTP API.
    RewindSession { target_uuid: String },

    /// Reload system configuration via HTTP API.
    ReloadSystem,

    /// Fetch and display context stats (messages, tokens) via HTTP API.
    ShowContextInfo,

    /// Fetch and display skills info via HTTP API.
    ShowSkillsInfo,

    /// Send a message to a specific session (Goal orchestration).
    GoalSend { session_id: String, content: String },

    /// Create a checker session for Goal mode (with fixed tools + system prompt).
    GoalCreateChecker { system_prompt: String },

    /// Unsubscribe from a Goal session (cleanup on exit).
    GoalUnsubscribe { session_id: String },

    /// Interrupt a Goal session (ESC stops both agents).
    GoalInterrupt { session_id: String },
}

impl AppIntent {
    /// Build an `UpdateSession` intent with all fields `None`.
    fn update_session(f: impl FnOnce(&mut Self)) -> Self {
        let mut intent = Self::UpdateSession {
            model: None,
            agent: None,
            title: None,
            thinking: None,
            reasoning_effort: None,
            yolo: None,
            workspace: None,
        };
        f(&mut intent);
        intent
    }

    /// Update the session model.
    pub fn set_model(model: String) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { model: m, .. } = i {
                *m = Some(model);
            }
        })
    }

    /// Update the session agent template.
    pub fn set_agent(agent: String) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { agent: a, .. } = i {
                *a = Some(agent);
            }
        })
    }

    /// Update the session title.
    pub fn set_title(title: String) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { title: t, .. } = i {
                *t = Some(title);
            }
        })
    }

    /// Update the thinking mode (and optionally reasoning effort).
    pub fn set_thinking(enabled: bool, effort: Option<String>) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession {
                thinking,
                reasoning_effort,
                ..
            } = i
            {
                *thinking = Some(enabled);
                *reasoning_effort = effort;
            }
        })
    }

    /// Update the YOLO mode (auto-approve tool calls).
    pub fn set_yolo(enabled: bool) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { yolo, .. } = i {
                *yolo = Some(enabled);
            }
        })
    }

    /// Update the session working directory.
    pub fn set_workdir(path: String) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { workspace, .. } = i {
                *workspace = Some(path);
            }
        })
    }
}
