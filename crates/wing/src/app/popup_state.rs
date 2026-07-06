//! PopupState — aggregated popup state and candidate cache.

use std::collections::HashSet;

use crate::ui::popup::ActivePopup;
use crate::ui::popup::command::CandidateCache;

/// Aggregated popup state: active popup, candidate cache, and dedup set.
#[derive(Default)]
pub struct PopupState {
    /// Current popup state (None when inactive).
    pub active: ActivePopup,
    /// Cached sub-command candidates.
    pub cache: CandidateCache,
    /// Set of popup silent request IDs already sent (dedup).
    pub sent_requests: HashSet<String>,
}

impl PopupState {
    /// Update popup state based on input text.
    ///
    /// Returns the silent request content if a new fetch is needed.
    /// Dedup check is handled by the caller via `should_send_request()`.
    pub fn update_from_input(&mut self, text: &str) -> Option<String> {
        self.active.update_from_input(text, &self.cache)
    }

    /// Check if a popup request ID has already been sent (dedup).
    pub fn should_send_request(&self, req_id: &str) -> bool {
        !self.sent_requests.contains(req_id)
    }

    /// Mark a popup request ID as sent.
    pub fn mark_sent(&mut self, req_id: String) {
        self.sent_requests.insert(req_id);
    }

    /// Clear the dedup entry for a request ID (when response arrives).
    pub fn clear_dedup(&mut self, req_id: &str) {
        self.sent_requests.remove(req_id);
    }

    /// Reset all popup state (e.g., on NewSession).
    pub fn reset(&mut self) {
        self.cache.clear();
        self.sent_requests.clear();
        self.active = ActivePopup::None;
    }

    /// Get the popup height (0 if no popup).
    pub fn height(&self) -> u16 {
        self.active.height()
    }
}
