//! Popup infrastructure — slash command and sub-command candidate popups.

pub mod command;
pub mod selection;

use command::CandidateCache;
use command::PopupAction;
use command::candidate_request_for;
use command::filter_candidates;
use command::filter_commands;
use command::filter_session_candidates;
use command::is_exact_session_match;
use command::is_local_candidate_command;
use command::is_session_command;
use command::parse_slash_input;
use selection::SelectionRow;
use selection::SelectionState;
use selection::popup_height;
use selection::rich_max_visible;

/// Active popup state.
#[derive(Debug, Clone, Default)]
pub enum ActivePopup {
    /// No popup active.
    #[default]
    None,
    /// Command list popup (showing slash commands).
    Command {
        /// Current filter text (after `/`).
        filter: String,
        /// Filtered command rows.
        rows: Vec<SelectionRow>,
        /// Navigation state.
        state: SelectionState,
    },
    /// Sub-command candidate popup (e.g. model list).
    SubCommand {
        /// The parent command name (e.g. "/model").
        command: String,
        /// Current filter text (after command name + space).
        filter: String,
        /// Filtered candidate rows.
        rows: Vec<SelectionRow>,
        /// Navigation state.
        state: SelectionState,
    },
}

/// Returns `true` if `args` (case-insensitive) exactly matches any candidate ID.
fn is_exact_candidate_match(candidates: &[(String, String)], args: &str) -> bool {
    let args_lower = args.to_lowercase();
    candidates
        .iter()
        .any(|(id, _)| id.to_lowercase() == args_lower)
}

impl ActivePopup {
    /// Update popup state based on current input text.
    ///
    /// Returns an optional action to fetch candidates via HTTP.
    pub fn update_from_input(&mut self, text: &str, cache: &CandidateCache) -> Option<PopupAction> {
        let Some((cmd, args)) = parse_slash_input(text) else {
            *self = Self::None;
            return None;
        };

        // Session commands (`/session`, `/ss`): HTTP-fetched rich candidates.
        if is_session_command(cmd) {
            if cache.has_sessions() {
                let candidates = &cache.sessions;
                // Hide popup if args exactly match a candidate id.
                if !args.is_empty() && is_exact_session_match(candidates, args) {
                    *self = Self::None;
                    return None;
                }
                let rows = filter_session_candidates(candidates, args);
                let count = rows.len();
                let filter = args.to_string();
                *self = Self::SubCommand {
                    command: cmd.to_string(),
                    filter,
                    rows,
                    state: SelectionState::with_max_visible(count, rich_max_visible()),
                };
                return None;
            }
            // Candidates not cached — trigger HTTP fetch.
            // Show command popup while waiting.
            let filter = cmd[1..].to_string();
            let rows = filter_commands(&cache.commands, &filter);
            let count = rows.len();
            *self = Self::Command {
                filter,
                rows,
                state: SelectionState::new(count),
            };
            return Some(PopupAction::FetchSessionList);
        }

        // Check if we're in sub-command mode for local-only candidates (e.g. /copy).
        if is_local_candidate_command(cmd) {
            if let Some(candidates) = cache.get_for_command(cmd) {
                // Hide popup if args exactly match a candidate.
                if !args.is_empty() && is_exact_candidate_match(candidates, args) {
                    *self = Self::None;
                    return None;
                }
                let rows = filter_candidates(candidates, args);
                let count = rows.len();
                let filter = args.to_string();
                // /copy: newest message is the expected default.
                let state = SelectionState::new_selecting_last(count);
                *self = Self::SubCommand {
                    command: cmd.to_string(),
                    filter,
                    rows,
                    state,
                };
                return None;
            }
            *self = Self::None;
            return None;
        }

        // Check if we're in sub-command mode for HTTP-fetched candidates.
        if let Some(action) = candidate_request_for(cmd) {
            // Check if candidates are cached.
            if let Some(candidates) = cache.get_for_command(cmd) {
                // Hide popup if args exactly match a candidate.
                if !args.is_empty() && is_exact_candidate_match(candidates, args) {
                    *self = Self::None;
                    return None;
                }
                let rows = filter_candidates(candidates, args);
                let count = rows.len();
                let filter = args.to_string();
                *self = Self::SubCommand {
                    command: cmd.to_string(),
                    filter,
                    rows,
                    state: SelectionState::new(count),
                };
                return None;
            }

            // Candidates not cached — show command popup while fetching.
            let filter = cmd[1..].to_string();
            let rows = filter_commands(&cache.commands, &filter);
            let count = rows.len();
            *self = Self::Command {
                filter,
                rows,
                state: SelectionState::new(count),
            };
            return Some(action.clone());
        }

        // Default: command list popup.
        let filter = cmd[1..].to_string(); // strip leading `/`
        // Hide popup if filter exactly matches a command name.
        if !filter.is_empty() {
            let filter_lower = filter.to_lowercase();
            let exact_match = cache
                .commands
                .iter()
                .any(|c| c.name.to_lowercase() == filter_lower)
                || command::is_tui_only_command(&filter);
            if exact_match {
                *self = Self::None;
                return None;
            }
        }
        let rows = filter_commands(&cache.commands, &filter);
        let count = rows.len();
        *self = Self::Command {
            filter,
            rows,
            state: SelectionState::new(count),
        };
        None
    }

    /// Get the popup height (0 if no popup).
    pub fn height(&self) -> u16 {
        match self {
            Self::None => 0,
            Self::Command { rows, state, .. } => popup_height(rows, state.max_visible),
            Self::SubCommand { rows, state, .. } => popup_height(rows, state.max_visible),
        }
    }

    /// Whether a popup is active.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether the popup has any matching items to select from.
    pub fn has_items(&self) -> bool {
        match self {
            Self::None => false,
            Self::Command { rows, .. } | Self::SubCommand { rows, .. } => !rows.is_empty(),
        }
    }

    /// Move selection up.
    pub fn move_up(&mut self) {
        match self {
            Self::Command { state, .. } | Self::SubCommand { state, .. } => state.move_up(),
            Self::None => {}
        }
    }

    /// Move selection down.
    pub fn move_down(&mut self) {
        match self {
            Self::Command { state, .. } | Self::SubCommand { state, .. } => state.move_down(),
            Self::None => {}
        }
    }

    /// Get the selected item's name (for completion).
    pub fn selected_name(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Command { rows, state, .. } => rows.get(state.selected).map(|r| r.name.as_str()),
            Self::SubCommand { rows, state, .. } => {
                rows.get(state.selected).map(|r| r.name.as_str())
            }
        }
    }

    /// Build the completion text for the selected item.
    ///
    /// For commands with candidates: returns command + space (e.g. "/model ").
    /// For commands without: returns the command name.
    /// For sub-commands: returns the full input (e.g. "/model gpt-4o").
    pub fn completion_text(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Command { rows, state, .. } => {
                let row = rows.get(state.selected)?;
                let has_candidates = candidate_request_for(&row.name).is_some()
                    || is_local_candidate_command(&row.name);
                if has_candidates {
                    Some(format!("{} ", row.name))
                } else {
                    Some(row.name.to_string())
                }
            }
            Self::SubCommand {
                command,
                rows,
                state,
                ..
            } => {
                let row = rows.get(state.selected)?;
                Some(format!("{command} {}", row.name))
            }
        }
    }

    /// Whether the selected item should be submitted immediately.
    ///
    /// Commands without candidates are submitted on Enter.
    /// Commands with candidates or sub-commands need more input.
    pub fn should_submit(&self) -> bool {
        match self {
            Self::None => false,
            Self::Command { rows, state, .. } => {
                if let Some(row) = rows.get(state.selected) {
                    candidate_request_for(&row.name).is_none()
                        && !is_local_candidate_command(&row.name)
                } else {
                    false
                }
            }
            Self::SubCommand { rows, state, .. } => {
                // Only submit if a valid candidate is selected.
                rows.get(state.selected).is_some()
            }
        }
    }

    /// Access the popup for rendering.
    pub fn render_data(&self) -> Option<(&[SelectionRow], &SelectionState, &str)> {
        match self {
            Self::None => None,
            Self::Command {
                rows,
                state,
                filter,
            } => Some((rows, state, filter)),
            Self::SubCommand {
                rows,
                state,
                filter,
                ..
            } => Some((rows, state, filter)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CommandInfo;

    /// Build a SessionCandidate for tests.
    fn sess(id: &str, title: &str) -> command::SessionCandidate {
        command::SessionCandidate {
            id: id.into(),
            title: title.into(),
            workspace: "/tmp/ws".into(),
            status: "idle".into(),
            last_interaction: "2025-07-22T21:41:00".into(),
        }
    }

    fn sample_cache() -> CandidateCache {
        CandidateCache {
            commands: vec![
                CommandInfo {
                    name: "help".into(),
                    aliases: vec![],
                    description: "Show commands".into(),
                    params: String::new(),
                },
                CommandInfo {
                    name: "model".into(),
                    aliases: vec![],
                    description: "Switch model".into(),
                    params: String::new(),
                },
            ],
            ..CandidateCache::default()
        }
    }

    #[test]
    fn test_active_popup_none_by_default() {
        let popup = ActivePopup::default();
        assert!(!popup.is_active());
        assert_eq!(popup.height(), 0);
    }

    #[test]
    fn test_active_popup_command_mode() {
        let mut popup = ActivePopup::default();
        let cache = sample_cache();
        popup.update_from_input("/mo", &cache);
        assert!(popup.is_active());
        assert!(popup.height() > 0);
    }

    #[test]
    fn test_active_popup_sub_command_mode() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        cache.models = vec![
            ("gpt-4o".into(), String::new()),
            ("gpt-4".into(), String::new()),
        ];
        popup.update_from_input("/model ", &cache);
        assert!(popup.is_active());
        assert!(matches!(popup, ActivePopup::SubCommand { .. }));
    }

    #[test]
    fn test_active_popup_completion() {
        let mut popup = ActivePopup::default();
        let cache = sample_cache();
        popup.update_from_input("/he", &cache);
        let completion = popup.completion_text();
        assert!(completion.is_some());
        assert_eq!(completion.unwrap(), "/help");
    }

    #[test]
    fn test_active_popup_sub_completion() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        cache.models = vec![("gpt-4o".into(), String::new())];
        popup.update_from_input("/model gp", &cache);
        let completion = popup.completion_text();
        assert!(completion.is_some());
        assert_eq!(completion.unwrap(), "/model gpt-4o");
    }

    #[test]
    fn test_active_popup_navigation() {
        let mut popup = ActivePopup::default();
        let cache = sample_cache();
        popup.update_from_input("/", &cache);
        popup.move_down();
        let name1 = popup.selected_name().map(String::from);
        popup.move_down();
        let name2 = popup.selected_name().map(String::from);
        assert_ne!(name1, name2);
    }

    #[test]
    fn test_active_popup_should_submit() {
        let mut popup = ActivePopup::default();
        let cache = sample_cache();
        // /help exact match → popup hidden (None).
        popup.update_from_input("/help", &cache);
        assert!(!popup.is_active());
        // /hel partial match → popup shows, should submit (no candidates).
        popup.update_from_input("/hel", &cache);
        assert!(popup.is_active());
        assert!(popup.should_submit());
        // /mod partial match → popup shows, should NOT submit (has candidates).
        popup.update_from_input("/mod", &cache);
        assert!(popup.is_active());
        assert!(!popup.should_submit());
    }

    #[test]
    fn test_active_popup_exact_match_hides_popup() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        // Command exact match hides popup.
        popup.update_from_input("/clear", &cache);
        assert!(!popup.is_active());
        // SubCommand exact match hides popup.
        cache.models = vec![("gpt-4o".into(), String::new())];
        popup.update_from_input("/model gpt-4o", &cache);
        assert!(!popup.is_active());
        // SubCommand partial match shows popup.
        popup.update_from_input("/model gpt", &cache);
        assert!(popup.is_active());
    }

    #[test]
    fn test_active_popup_sub_should_submit_requires_selection() {
        let mut popup = ActivePopup::default();
        let mut cache = sample_cache();
        cache.models = vec![("gpt-4o".into(), String::new())];
        popup.update_from_input("/model ", &cache);
        // Has a selection → should submit.
        assert!(popup.should_submit());
    }

    #[test]
    fn test_session_empty_cache_returns_fetch_action() {
        use command::PopupAction;

        let mut popup = ActivePopup::default();
        let cache = CandidateCache::default(); // sessions empty
        let action = popup.update_from_input("/session ", &cache);
        assert!(matches!(action, Some(PopupAction::FetchSessionList)));
    }

    #[test]
    fn test_ss_empty_cache_returns_fetch_action() {
        use command::PopupAction;

        let mut popup = ActivePopup::default();
        let cache = CandidateCache::default();
        let action = popup.update_from_input("/ss ", &cache);
        assert!(matches!(action, Some(PopupAction::FetchSessionList)));
    }

    #[test]
    fn test_session_cached_shows_popup_no_action() {
        let mut popup = ActivePopup::default();
        let mut cache = CandidateCache::default();
        cache.sessions = vec![sess("sess-1", "My Session"), sess("sess-2", "Other")];
        let action = popup.update_from_input("/session ", &cache);
        assert!(action.is_none()); // No fetch needed
        assert!(popup.is_active());
        assert!(matches!(popup, ActivePopup::SubCommand { .. }));
    }

    #[test]
    fn test_session_exact_match_hides_popup() {
        let mut popup = ActivePopup::default();
        let mut cache = CandidateCache::default();
        cache.sessions = vec![sess("sess-1", "My Session")];
        let action = popup.update_from_input("/session sess-1", &cache);
        assert!(action.is_none());
        assert!(!popup.is_active());
    }

    #[test]
    fn test_session_popup_rows_are_rich() {
        let mut popup = ActivePopup::default();
        let mut cache = CandidateCache::default();
        cache.sessions = vec![sess("sess-1", "My Session")];
        popup.update_from_input("/session ", &cache);
        if let ActivePopup::SubCommand { rows, state, .. } = &popup {
            assert_eq!(rows.len(), 1);
            assert!(rows[0].rich.is_some());
            // Rich popup uses the smaller visible-item window.
            assert_eq!(state.max_visible, rich_max_visible());
        } else {
            panic!("expected SubCommand popup");
        }
    }

    #[test]
    fn test_fork_triggers_fetch_branches() {
        use command::PopupAction;

        let mut popup = ActivePopup::default();
        let cache = CandidateCache::default();
        let action = popup.update_from_input("/fork ", &cache);
        assert!(matches!(action, Some(PopupAction::FetchBranches)));
    }
}
