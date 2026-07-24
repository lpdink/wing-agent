//! AskSelection — state for required Ask event selection menus.
//!
//! When the agent sends an Ask event with `required: true`, the TUI
//! activates a selection cursor within the AskMessage cell in the chat
//! view. This module holds the navigation state; rendering is done by
//! `AskMessage::to_lines()` in `cells/ask_msg.rs`.

/// Mutable state for an active ask selection.
#[derive(Debug, Clone)]
pub struct AskSelection {
    /// Correlation id of the Ask event — echoed back when sending the
    /// chosen answer so the gateway resolves the right feedback waiter.
    pub tool_call_id: String,
    /// Available choices.
    pub choices: Vec<String>,
    /// Index of the currently highlighted choice (wraps around).
    pub selected: usize,
}

impl AskSelection {
    pub fn new(tool_call_id: String, choices: Vec<String>) -> Self {
        Self {
            tool_call_id,
            choices,
            selected: 0,
        }
    }

    /// Move selection up (wraps to last item).
    pub fn move_up(&mut self) {
        if self.choices.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.choices.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Move selection down (wraps to first item).
    pub fn move_down(&mut self) {
        if self.choices.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.choices.len();
    }

    /// Get the currently selected choice value.
    pub fn current(&self) -> Option<&str> {
        self.choices.get(self.selected).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_selection() {
        let sel = AskSelection::new("tc".into(), vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(sel.selected, 0);
        assert_eq!(sel.current(), Some("a"));
    }

    #[test]
    fn test_move_down_wraps() {
        let mut sel = AskSelection::new("tc".into(), vec!["a".into(), "b".into()]);
        sel.move_down();
        assert_eq!(sel.selected, 1);
        sel.move_down();
        assert_eq!(sel.selected, 0);
    }

    #[test]
    fn test_move_up_wraps() {
        let mut sel = AskSelection::new("tc".into(), vec!["a".into(), "b".into()]);
        sel.move_up();
        assert_eq!(sel.selected, 1);
        sel.move_up();
        assert_eq!(sel.selected, 0);
    }

    #[test]
    fn test_empty_selection() {
        let sel = AskSelection::new("tc".into(), vec![]);
        assert_eq!(sel.current(), None);
    }
}
