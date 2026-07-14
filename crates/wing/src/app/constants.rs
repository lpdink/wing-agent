//! Protocol constants — magic strings used across the TUI codebase.
//!
//! Centralizing these avoids typos, eases grep-ability, and makes
//! protocol-level changes visible in one place.

// ── Magic commands (frontend → gateway) ─────────────────────────

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
