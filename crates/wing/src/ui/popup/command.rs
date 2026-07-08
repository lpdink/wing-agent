//! Slash command definitions, filtering, and sub-command candidate management.
//!
//! Dynamic command list from gateway + static TUI-only fallbacks.
//! Handles:
//! - Input text → filter extraction
//! - Prefix matching with sort (exact > prefix)
//! - Sub-command candidates (model list, session list, etc.)

use super::selection::SelectionRow;
use crate::protocol::CommandInfo;

/// Action to take when popup candidates need to be fetched.
///
/// Replaces the old `Option<String>` return from `update_from_input()`.
/// Allows distinguishing between WS silent requests and HTTP fetches.
#[derive(Debug, Clone)]
pub enum PopupAction {
    /// Send a WS silent request to fetch candidates (existing behavior).
    SilentRequest(String),
    /// Fetch session list via HTTP API (Phase 3c).
    FetchSessionList,
}

/// Commands that have sub-command candidates. Maps command name to silent request.
const CANDIDATE_COMMANDS: &[(&str, &str)] = &[
    ("/model", "/model"),
    ("/fork", "/rewind list"),
    ("/rewind", "/rewind list"),
    ("/agents", "/agents"),
    // Local-only: "" = no gateway request, candidates populated by App.
    // If a second local-select-execute command appears, extract a
    // ModalSelect variant from ActivePopup instead of extending this pattern.
    ("/copy", ""),
];

/// TUI-only commands (not served by gateway) — always appended as fallback.
const TUI_ONLY_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "Clear chat view"),
    ("/copy", "Copy assistant message"),
];

/// Check if a bare name (without `/`) matches a TUI-only command (case-insensitive).
pub fn is_tui_only_command(bare_name: &str) -> bool {
    let lower = bare_name.to_lowercase();
    TUI_ONLY_COMMANDS
        .iter()
        .any(|(name, _)| name[1..].to_lowercase() == lower)
}

/// Check if a command name has sub-command candidates.
pub fn candidate_request_for(name: &str) -> Option<&'static str> {
    CANDIDATE_COMMANDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, req)| *req)
}

/// Check if a command is a session command (`/session` or `/ss`).
/// These are handled via HTTP API instead of WS silent requests.
pub fn is_session_command(name: &str) -> bool {
    name == "/session" || name == "/ss"
}

/// Extract the command part (first word) from input starting with `/`.
///
/// Returns `("/model", "gpt-4o")` for `"/model gpt-4o"`.
pub fn parse_slash_input(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    match trimmed.find(' ') {
        Some(pos) => {
            let cmd = &trimmed[..pos];
            let args = trimmed[pos + 1..].trim_start();
            Some((cmd, args))
        }
        None => Some((trimmed, "")),
    }
}

/// Filter and sort commands by the given filter string.
///
/// `commands` is the dynamic list from gateway (may be empty if not yet fetched).
/// TUI-only commands are always appended.
/// Sort order: exact match > prefix match.
pub fn filter_commands(commands: &[CommandInfo], filter: &str) -> Vec<SelectionRow> {
    let filter_lower = filter.to_lowercase();

    let mut exact = Vec::new();
    let mut prefix = Vec::new();

    // Helper to try adding a command row.
    let mut try_add = |name: &str, desc: &str| {
        let name_stripped = &name[1..]; // strip leading `/`
        let name_lower = name_stripped.to_lowercase();

        if filter_lower.is_empty() {
            prefix.push(SelectionRow {
                name: name.to_string(),
                description: desc.to_string(),
            });
        } else if name_lower == filter_lower {
            exact.push(SelectionRow {
                name: name.to_string(),
                description: desc.to_string(),
            });
        } else if name_lower.starts_with(&filter_lower) {
            prefix.push(SelectionRow {
                name: name.to_string(),
                description: desc.to_string(),
            });
        }
    };

    // Dynamic commands from gateway.
    for cmd in commands {
        let full_name = format!("/{}", cmd.name);
        try_add(&full_name, &cmd.description);
    }

    // TUI-only fallbacks (skip if already provided by gateway).
    let gateway_names: std::collections::HashSet<&str> =
        commands.iter().map(|c| c.name.as_str()).collect();
    for (name, desc) in TUI_ONLY_COMMANDS {
        let bare = &name[1..];
        if !gateway_names.contains(bare) {
            try_add(name, desc);
        }
    }

    exact.extend(prefix);
    exact
}

/// Filter sub-command candidates by the given args string.
///
/// Each candidate is `(id, description)`. Matching is done on `id`.
/// `name` in the resulting `SelectionRow` is the `id` (used for completion),
/// `description` is the display label.
pub fn filter_candidates(candidates: &[(String, String)], args: &str) -> Vec<SelectionRow> {
    let args_lower = args.to_lowercase();

    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();

    for (id, desc) in candidates {
        let id_lower = id.to_lowercase();
        let row = || SelectionRow {
            name: id.clone(),
            description: desc.clone(),
        };
        if args.is_empty() {
            prefix.push(row());
        } else if id_lower == args_lower {
            exact.push(row());
        } else if id_lower.starts_with(&args_lower) {
            prefix.push(row());
        } else if id_lower.contains(&args_lower) {
            contains.push(row());
        }
    }

    exact.extend(prefix);
    exact.extend(contains);
    exact
}

/// Cached sub-command candidate data and dynamic command list.
///
/// All candidate lists use `(id, description)` tuples. The `id` is the value
/// sent to the gateway on completion; `description` is display-only.
#[derive(Debug, Clone, Default)]
pub struct CandidateCache {
    /// Dynamic command list from gateway (CommandListEvent).
    pub commands: Vec<CommandInfo>,
    /// Model list (from ModelListEvent). Description is empty.
    pub models: Vec<(String, String)>,
    /// Session list (from SessionListEvent). Description is session name.
    pub sessions: Vec<(String, String)>,
    /// Branch targets (from BranchTargetsEvent). Description is content preview.
    pub branches: Vec<(String, String)>,
    /// Agent list (from AgentListEvent). Description is empty.
    pub agents: Vec<(String, String)>,
    /// Copy candidates (local-only). `(1-based index, first-line preview)`.
    pub copies: Vec<(String, String)>,
}

impl CandidateCache {
    pub fn clear(&mut self) {
        // commands 不清空——来自 gateway 的全局命令列表，不随 session 变化
        self.models.clear();
        self.sessions.clear();
        self.branches.clear();
        self.agents.clear();
        self.copies.clear();
    }

    /// Get candidates for a given command name.
    pub fn get_for_command(&self, cmd_name: &str) -> Option<&[(String, String)]> {
        match cmd_name {
            "/model" if !self.models.is_empty() => Some(&self.models),
            "/ss" | "/session" if !self.sessions.is_empty() => Some(&self.sessions),
            "/fork" | "/rewind" if !self.branches.is_empty() => Some(&self.branches),
            "/agents" if !self.agents.is_empty() => Some(&self.agents),
            "/copy" if !self.copies.is_empty() => Some(&self.copies),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_commands() -> Vec<CommandInfo> {
        vec![
            CommandInfo {
                name: "help".into(),
                aliases: vec!["h".into(), "?".into()],
                description: "Show available commands".into(),
                params: String::new(),
            },
            CommandInfo {
                name: "model".into(),
                aliases: vec!["m".into()],
                description: "Switch model".into(),
                params: String::new(),
            },
            CommandInfo {
                name: "compact".into(),
                aliases: vec![],
                description: "Trigger context compaction".into(),
                params: String::new(),
            },
        ]
    }

    #[test]
    fn test_parse_slash_input() {
        assert_eq!(parse_slash_input("/model"), Some(("/model", "")));
        assert_eq!(parse_slash_input("/model gpt"), Some(("/model", "gpt")));
        assert_eq!(parse_slash_input("/"), Some(("/", "")));
        assert_eq!(parse_slash_input("hello"), None);
        assert_eq!(parse_slash_input(""), None);
    }

    #[test]
    fn test_filter_commands_with_cache() {
        let cmds = sample_commands();
        let rows = filter_commands(&cmds, "");
        // Dynamic commands (primary names only, no aliases) + TUI-only fallbacks.
        assert!(rows.iter().any(|r| r.name == "/help"));
        assert!(!rows.iter().any(|r| r.name == "/h")); // alias NOT shown
        assert!(rows.iter().any(|r| r.name == "/model"));
        assert!(rows.iter().any(|r| r.name == "/clear")); // TUI-only
    }

    #[test]
    fn test_filter_commands_empty_cache() {
        // No gateway commands yet — only TUI-only fallbacks.
        let rows = filter_commands(&[], "");
        assert!(rows.iter().any(|r| r.name == "/clear"));
    }

    #[test]
    fn test_filter_commands_prefix() {
        let cmds = sample_commands();
        let rows = filter_commands(&cmds, "mo");
        // Only /model matches "mo" prefix — alias /m is too short.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "/model");
    }

    #[test]
    fn test_filter_commands_exact() {
        let cmds = sample_commands();
        let rows = filter_commands(&cmds, "help");
        assert!(!rows.is_empty());
        assert_eq!(rows[0].name, "/help");
    }

    #[test]
    fn test_filter_commands_no_match() {
        let cmds = sample_commands();
        let rows = filter_commands(&cmds, "zzz");
        assert!(rows.is_empty());
    }

    #[test]
    fn test_filter_candidates() {
        let candidates = vec![
            ("gpt-4o".into(), String::new()),
            ("gpt-4".into(), String::new()),
            ("claude-3-opus".into(), String::new()),
        ];
        let rows = filter_candidates(&candidates, "gpt");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "gpt-4o");
    }

    #[test]
    fn test_filter_candidates_empty() {
        let candidates = vec![("a".into(), String::new()), ("b".into(), String::new())];
        let rows = filter_candidates(&candidates, "");
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_filter_candidates_with_description() {
        let candidates = vec![
            ("abc123".into(), "my-session".into()),
            ("def456".into(), "other".into()),
        ];
        let rows = filter_candidates(&candidates, "abc");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "abc123"); // id used for completion
        assert_eq!(rows[0].description, "my-session"); // display only
    }

    #[test]
    fn test_candidate_cache() {
        let mut cache = CandidateCache::default();
        assert!(cache.get_for_command("/model").is_none());
        cache.models = vec![("gpt-4o".into(), String::new())];
        assert_eq!(cache.get_for_command("/model").unwrap().len(), 1);
        cache.clear();
        assert!(cache.get_for_command("/model").is_none());
    }

    #[test]
    fn test_candidate_request_for() {
        assert_eq!(candidate_request_for("/model"), Some("/model"));
        // /ss and /session removed from CANDIDATE_COMMANDS (Phase 3c: HTTP-fetched).
        assert_eq!(candidate_request_for("/ss"), None);
        assert_eq!(candidate_request_for("/session"), None);
        assert_eq!(candidate_request_for("/fork"), Some("/rewind list"));
        assert_eq!(candidate_request_for("/help"), None);
        assert_eq!(candidate_request_for("/nonexistent"), None);
    }

    #[test]
    fn test_is_session_command() {
        assert!(is_session_command("/session"));
        assert!(is_session_command("/ss"));
        assert!(!is_session_command("/model"));
        assert!(!is_session_command("/fork"));
    }
}
