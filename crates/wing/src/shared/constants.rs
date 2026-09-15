//! Protocol constants — magic strings used across the TUI codebase.
//!
//! Centralizing these avoids typos, eases grep-ability, and makes
//! protocol-level changes visible in one place.

// ── Frontend-only commands (handled locally, never sent to gateway) ─

/// Copy the last assistant message to clipboard.
pub const COPY_COMMAND: &str = "/copy";

/// Clear the chat view.
pub const CLEAR_COMMAND: &str = "/clear";

// ── Session lifecycle commands (handled via HTTP API) ───────────

/// Create a new session.
pub const NEW_COMMAND: &str = "/new";

/// Activate Goal orchestration mode.
pub const GOAL_COMMAND: &str = "/goal";

/// Exit Goal orchestration mode.
pub const GOAL_EXIT_COMMAND: &str = "/goal-exit";

// ── Tool names (must match backend tool registry names) ──────────

pub const TOOL_BASH: &str = "Bash";
pub const TOOL_READ: &str = "Read";
pub const TOOL_WRITE: &str = "Write";
pub const TOOL_EDIT: &str = "Edit";
pub const TOOL_GLOB: &str = "Glob";
pub const TOOL_GREP: &str = "Grep";
pub const TOOL_ASK: &str = "AskUserQuestion";
pub const TOOL_TODO: &str = "TodoWrite";
