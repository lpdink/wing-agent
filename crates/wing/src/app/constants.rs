//! Protocol constants — magic strings used across the TUI codebase.
//!
//! Centralizing these avoids typos, eases grep-ability, and makes
//! protocol-level changes visible in one place.

// ── Silent request IDs (frontend → gateway) ─────────────────────

/// Request ID for the initial `/info` query on startup.
pub const INIT_INFO_REQUEST_ID: &str = "_init_info";

/// Request ID for fetching the command list via `/help`.
pub const POPUP_HELP_REQUEST_ID: &str = "_popup_help";

// ── Popup dedup request IDs ────────────────────────────────────

/// Dedup key for model list popup.
pub const POPUP_MODEL_REQUEST_ID: &str = "_popup_model";

/// Dedup key for session list popup.
pub const POPUP_SESSION_REQUEST_ID: &str = "_popup_session";

/// Dedup key for branch targets (rewind/fork) popup.
pub const POPUP_REWIND_REQUEST_ID: &str = "_popup_rewind_list";

/// Dedup key for agent list popup.
pub const POPUP_AGENTS_REQUEST_ID: &str = "_popup_agents";

// ── Magic commands (frontend → gateway) ─────────────────────────

/// Request system info.
pub const INFO_COMMAND: &str = "/info";

/// Request available commands list.
pub const HELP_COMMAND: &str = "/help";

/// Interrupt the current agent turn.
pub const INTERRUPT_COMMAND: &str = "/interrupt";

// ── Frontend-only commands (handled locally, never sent to gateway) ─

/// Copy the last assistant message to clipboard.
pub const COPY_COMMAND: &str = "/copy";

/// Clear the chat view.
pub const CLEAR_COMMAND: &str = "/clear";

// ── Session lifecycle commands (handled via HTTP API) ───────────

/// Create a new session.
pub const NEW_COMMAND: &str = "/new";

/// Fork the current session at a branch target.
pub const FORK_COMMAND: &str = "/fork";

/// Resume a session by ID.
pub const SESSION_COMMAND: &str = "/session";

/// Short alias for /session.
pub const SS_COMMAND: &str = "/ss";

// ── Tool names (must match backend tool registry names) ──────────

pub const TOOL_BASH: &str = "Bash";
pub const TOOL_READ: &str = "Read";
pub const TOOL_WRITE: &str = "Write";
pub const TOOL_EDIT: &str = "Edit";
pub const TOOL_GLOB: &str = "Glob";
pub const TOOL_GREP: &str = "Grep";
pub const TOOL_ASK: &str = "AskUserQuestion";
pub const TOOL_TODO: &str = "TodoWrite";
