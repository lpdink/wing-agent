//! Popup infrastructure — slash command and sub-command candidate popups.

pub mod command;
pub mod selection;

use command::CandidateCache;
use command::candidate_request_for;
use command::filter_candidates;
use command::filter_commands;
use command::parse_slash_input;
use selection::SelectionRow;
use selection::SelectionState;
use selection::popup_height;

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

impl ActivePopup {
    /// Update popup state based on current input text.
    ///
    /// Returns an optional silent request to send for fetching candidates.
    pub fn update_from_input(&mut self, text: &str, cache: &CandidateCache) -> Option<String> {
        let Some((cmd, args)) = parse_slash_input(text) else {
            *self = Self::None;
            return None;
        };

        // Check if we're in sub-command mode (command + space + args).
        if let Some(request) = candidate_request_for(cmd) {
            // Check if candidates are cached.
            if let Some(candidates) = cache.get_for_command(cmd) {
                // Hide popup if args exactly match a candidate.
                if !args.is_empty() {
                    let args_lower = args.to_lowercase();
                    if candidates
                        .iter()
                        .any(|(id, _)| id.to_lowercase() == args_lower)
                    {
                        *self = Self::None;
                        return None;
                    }
                }
                let rows = filter_candidates(candidates, args);
                let count = rows.len();
                let filter = args.to_string();
                let state = if cmd == "/copy" {
                    SelectionState::new_selecting_last(count)
                } else {
                    SelectionState::new(count)
                };
                *self = Self::SubCommand {
                    command: cmd.to_string(),
                    filter,
                    rows,
                    state,
                };
                return None;
            }

            // Candidates not cached.
            // Local-only commands (empty request): nothing to fetch, no popup.
            if request.is_empty() {
                *self = Self::None;
                return None;
            }

            // Show command popup while waiting for gateway response.
            let filter = cmd[1..].to_string();
            let rows = filter_commands(&cache.commands, &filter);
            let count = rows.len();
            *self = Self::Command {
                filter,
                rows,
                state: SelectionState::new(count),
            };
            return Some(request.to_string());
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
            Self::Command { rows, .. } => popup_height(rows.len()),
            Self::SubCommand { rows, .. } => popup_height(rows.len()),
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
                if candidate_request_for(&row.name).is_some() {
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
}
