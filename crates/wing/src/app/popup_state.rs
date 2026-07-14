//! PopupState — aggregated popup state and candidate cache.

use crate::ui::popup::ActivePopup;
use crate::ui::popup::command::CandidateCache;
use crate::ui::popup::command::PopupAction;

/// Aggregated popup state: active popup and candidate cache.
#[derive(Default)]
pub struct PopupState {
    /// Current popup state (None when inactive).
    pub active: ActivePopup,
    /// Cached sub-command candidates.
    pub cache: CandidateCache,
}

impl PopupState {
    /// Update popup state based on input text.
    ///
    /// Returns the popup action if a candidate fetch is needed.
    pub fn update_from_input(&mut self, text: &str) -> Option<PopupAction> {
        self.active.update_from_input(text, &self.cache)
    }

    /// Reset all popup state (e.g., on session switch).
    pub fn reset(&mut self) {
        self.cache.clear();
        self.active = ActivePopup::None;
    }

    /// Get the popup height (0 if no popup).
    pub fn height(&self) -> u16 {
        self.active.height()
    }
}
