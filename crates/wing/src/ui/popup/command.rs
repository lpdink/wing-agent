//! Slash command definitions, filtering, and sub-command candidate management.
//!
//! Dynamic command list from gateway + static TUI-only fallbacks.
//! Handles:
//! - Input text → filter extraction
//! - Prefix matching with sort (exact > prefix)
//! - Sub-command candidates (model list, session list, etc.)

use std::sync::LazyLock;

use super::selection::RichSessionRow;
use super::selection::SelectionRow;
use super::selection::SessionStatus;
use super::selection::plain_row;
use crate::protocol::CommandInfo;

/// Action to take when popup candidates need to be fetched.
///
/// Each variant maps to a specific HTTP API call or intent.
#[derive(Debug, Clone)]
pub enum PopupAction {
    /// Fetch model list via HTTP API for popup candidates.
    FetchModels,
    /// Fetch branch targets via HTTP API for popup candidates.
    FetchBranches,
    /// Fetch agent template list via HTTP API for popup candidates.
    FetchAgents,
    /// Fetch session list via HTTP API (Phase 3c).
    FetchSessionList,
}

/// Commands that have sub-command candidates fetched via HTTP.
/// Maps command name to the corresponding PopupAction.
const CANDIDATE_COMMANDS: &[(&str, PopupAction)] = &[
    ("/model", PopupAction::FetchModels),
    ("/fork", PopupAction::FetchBranches),
    ("/rewind", PopupAction::FetchBranches),
    ("/agents", PopupAction::FetchAgents),
];

/// Local-only candidate commands — candidates populated by App, no fetch needed.
const LOCAL_CANDIDATE_COMMANDS: &[&str] = &["/copy"];

/// Commands whose argument MUST come from candidate selection.
///
/// For these commands Enter means "confirm the highlighted candidate", never
/// "send free text": the popup stays open on exact match (Tab fills the input
/// without dismissing it), and submission is only constructed from a selected
/// candidate. This structurally eliminates undefined resolution — e.g.
/// `/model <name>` where the name is absent from the fetched list previously
/// sent a provider-less update and hit the gateway's symmetric-contract 400.
const MUST_SELECT_COMMANDS: &[&str] = &["/model", "/fork", "/rewind", "/agents", "/session", "/ss"];

/// TUI-only commands (not served by gateway) — always appended as fallback.
///
/// These are commands handled entirely by the TUI (via `try_http_command()` or
/// local logic). The full `CommandInfo` structure allows the popup to display
/// aliases and parameter hints.
static TUI_ONLY_COMMANDS: LazyLock<Vec<CommandInfo>> = LazyLock::new(|| {
    vec![
        CommandInfo {
            name: "clear".into(),
            aliases: vec![],
            description: "Clear chat view".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "copy".into(),
            aliases: vec![],
            description: "Copy assistant message".into(),
            params: "[N]".into(),
        },
        CommandInfo {
            name: "new".into(),
            aliases: vec![],
            description: "Create new session".into(),
            params: "[name]".into(),
        },
        CommandInfo {
            name: "session".into(),
            aliases: vec!["ss".into()],
            description: "Switch or list sessions".into(),
            params: "[session_id]".into(),
        },
        CommandInfo {
            name: "fork".into(),
            aliases: vec![],
            description: "Fork session at message".into(),
            params: "<uuid>".into(),
        },
        CommandInfo {
            name: "model".into(),
            aliases: vec!["m".into()],
            description: "Switch or show model".into(),
            params: "[name]".into(),
        },
        CommandInfo {
            name: "agents".into(),
            aliases: vec![],
            description: "Switch or show agent template".into(),
            params: "[name]".into(),
        },
        CommandInfo {
            name: "title".into(),
            aliases: vec![],
            description: "Show or set session title".into(),
            params: "[name]".into(),
        },
        CommandInfo {
            name: "workdir".into(),
            aliases: vec![],
            description: "Switch working directory".into(),
            params: "<path>".into(),
        },
        CommandInfo {
            name: "think".into(),
            aliases: vec!["t".into()],
            description: "Toggle thinking / set effort".into(),
            params: "on|off|low|medium|high|xhigh|max".into(),
        },
        CommandInfo {
            name: "yolo".into(),
            aliases: vec![],
            description: "Toggle YOLO mode".into(),
            params: "on|off".into(),
        },
        CommandInfo {
            name: "compact".into(),
            aliases: vec![],
            description: "Compress session context".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "context".into(),
            aliases: vec![],
            description: "Show context stats and system prompt".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "skills".into(),
            aliases: vec![],
            description: "Show loaded skills".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "rewind".into(),
            aliases: vec![],
            description: "Rewind to a specific message".into(),
            params: "<uuid>".into(),
        },
        CommandInfo {
            name: "reload".into(),
            aliases: vec![],
            description: "Reload config, hooks, skills".into(),
            params: String::new(),
        },
        CommandInfo {
            name: "goal".into(),
            aliases: vec![],
            description: "Start Goal orchestration (executor + checker loop)".into(),
            params: "<prompt>".into(),
        },
        CommandInfo {
            name: "goal-exit".into(),
            aliases: vec![],
            description: "Exit Goal orchestration mode".into(),
            params: String::new(),
        },
    ]
});

/// Check if a bare name (without `/`) matches a TUI-only command (case-insensitive).
pub fn is_tui_only_command(bare_name: &str) -> bool {
    let lower = bare_name.to_lowercase();
    TUI_ONLY_COMMANDS.iter().any(|cmd| {
        cmd.name.to_lowercase() == lower || cmd.aliases.iter().any(|a| a.to_lowercase() == lower)
    })
}

/// Check if a command name has HTTP-fetched sub-command candidates.
pub fn candidate_request_for(name: &str) -> Option<&'static PopupAction> {
    CANDIDATE_COMMANDS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, action)| action)
}

/// Check if a command name has locally-populated sub-command candidates.
pub fn is_local_candidate_command(name: &str) -> bool {
    LOCAL_CANDIDATE_COMMANDS.contains(&name)
}

/// Check if a command requires its argument to be selected from candidates.
pub fn is_must_select_command(name: &str) -> bool {
    MUST_SELECT_COMMANDS.contains(&name)
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
            prefix.push(plain_row(name, desc));
        } else if name_lower == filter_lower {
            exact.push(plain_row(name, desc));
        } else if name_lower.starts_with(&filter_lower) {
            prefix.push(plain_row(name, desc));
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
    for cmd in TUI_ONLY_COMMANDS.iter() {
        if !gateway_names.contains(cmd.name.as_str()) {
            let full_name = format!("/{}", cmd.name);
            let desc = if cmd.params.is_empty() {
                cmd.description.clone()
            } else {
                format!("{} {}", cmd.description, cmd.params)
            };
            try_add(&full_name, &desc);
        }
    }

    exact.extend(prefix);
    exact.sort_by_key(|a| a.name.to_lowercase());
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
        let row = || plain_row(id.clone(), desc.clone());
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

/// Filter session candidates by the given args string, producing rich two-line rows.
///
/// Matching is done on id, title, and workspace (case-insensitive). The order
/// of `candidates` is preserved within each match tier (exact > prefix > contains),
/// so the caller's pre-sort (workdir → status → time) is retained.
pub fn filter_session_candidates(candidates: &[SessionCandidate], args: &str) -> Vec<SelectionRow> {
    let args_lower = args.to_lowercase();

    let mut exact = Vec::new();
    let mut prefix = Vec::new();
    let mut contains = Vec::new();

    for c in candidates {
        let id_lower = c.id.to_lowercase();
        let title_lower = c.title.to_lowercase();
        let ws_lower = c.workspace.to_lowercase();
        let row = || SelectionRow {
            name: c.id.clone(),
            description: String::new(),
            rich: Some(RichSessionRow {
                status: SessionStatus::parse(&c.status),
                workspace: c.workspace.clone(),
                last_active: super::selection::format_last_active(&c.last_interaction),
                title: c.title.clone(),
            }),
        };
        if args.is_empty() {
            prefix.push(row());
        } else if id_lower == args_lower {
            exact.push(row());
        } else if id_lower.starts_with(&args_lower) || title_lower.starts_with(&args_lower) {
            prefix.push(row());
        } else if id_lower.contains(&args_lower)
            || title_lower.contains(&args_lower)
            || ws_lower.contains(&args_lower)
        {
            contains.push(row());
        }
    }

    exact.extend(prefix);
    exact.extend(contains);
    exact
}

/// Returns `true` if `args` exactly matches a session candidate id (case-insensitive).
pub fn is_exact_session_match(candidates: &[SessionCandidate], args: &str) -> bool {
    let args_lower = args.to_lowercase();
    candidates.iter().any(|c| c.id.to_lowercase() == args_lower)
}

/// A session candidate for the `/session`、`/ss` popup.
///
/// Carries everything needed for the two-line render (status icon + workspace
/// on line 1, title on line 2) plus the `id` used as the completion value.
#[derive(Debug, Clone)]
pub struct SessionCandidate {
    /// Session id — inserted on completion (not displayed verbatim).
    pub id: String,
    /// Session title (line 2).
    pub title: String,
    /// Session workspace (line 1, after the status icon).
    pub workspace: String,
    /// Runtime status string from the backend (inactive|idle|working|waiting).
    pub status: String,
    /// Last interaction timestamp (ISO 8601), displayed on line 1.
    pub last_interaction: String,
}

/// Cached sub-command candidate data and dynamic command list.
///
/// Most candidate lists use `(id, description)` tuples. The `id` is the value
/// sent to the gateway on completion; `description` is display-only. Sessions
/// use the richer [`SessionCandidate`] to drive the two-line popup.
#[derive(Debug, Clone, Default)]
pub struct CandidateCache {
    /// Dynamic command list from HTTP GET /api/commands.
    pub commands: Vec<CommandInfo>,
    /// Model list from HTTP GET /api/models. Tuple: (model_name, provider_name).
    pub models: Vec<(String, String)>,
    /// Session list from HTTP GET /api/session/list (rich, two-line render).
    pub sessions: Vec<SessionCandidate>,
    /// Branch targets from HTTP GET /api/session/branches or WS BranchTargetsEvent.
    pub branches: Vec<(String, String)>,
    /// Agent list from HTTP GET /api/agents. Description is empty.
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

    /// Whether session candidates are cached (for `/ss`、`/session`).
    pub fn has_sessions(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Get candidates for a given command name (non-session commands).
    pub fn get_for_command(&self, cmd_name: &str) -> Option<&[(String, String)]> {
        match cmd_name {
            "/model" if !self.models.is_empty() => Some(&self.models),
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
        assert!(matches!(
            candidate_request_for("/model"),
            Some(PopupAction::FetchModels)
        ));
        assert!(matches!(
            candidate_request_for("/fork"),
            Some(PopupAction::FetchBranches)
        ));
        assert!(matches!(
            candidate_request_for("/rewind"),
            Some(PopupAction::FetchBranches)
        ));
        assert!(matches!(
            candidate_request_for("/agents"),
            Some(PopupAction::FetchAgents)
        ));
        // /ss and /session removed from CANDIDATE_COMMANDS (Phase 3c: HTTP-fetched).
        assert!(candidate_request_for("/ss").is_none());
        assert!(candidate_request_for("/session").is_none());
        assert!(candidate_request_for("/help").is_none());
        assert!(candidate_request_for("/nonexistent").is_none());
        // /copy is a local candidate command, not in CANDIDATE_COMMANDS.
        assert!(candidate_request_for("/copy").is_none());
    }

    #[test]
    fn test_is_local_candidate_command() {
        assert!(is_local_candidate_command("/copy"));
        assert!(!is_local_candidate_command("/model"));
        assert!(!is_local_candidate_command("/help"));
    }

    #[test]
    fn test_is_session_command() {
        assert!(is_session_command("/session"));
        assert!(is_session_command("/ss"));
        assert!(!is_session_command("/model"));
        assert!(!is_session_command("/fork"));
    }
}
