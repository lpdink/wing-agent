//! TurnState — per-turn tracking for agent working state.

use std::time::Instant;

use crate::ui::spinner::SpinnerState;
use crate::ui::status_bar::TurnUsage;

/// Summary of a completed turn result (from TurnResultEvent).
#[derive(Debug, Clone)]
pub struct TurnResultSummary {
    pub subtype: String,
    pub is_error: bool,
    pub duration_ms: i64,
    pub num_turns: i64,
    /// Last assistant text (may be long — truncate for display).
    pub result: Option<String>,
    /// Total tokens (input + output + cached).
    pub total_tokens: Option<i64>,
}

/// Tracks the current agent turn: working flag, timer, spinner, usage.
#[derive(Default)]
pub struct TurnState {
    /// Whether a turn is in progress (TurnStarted received, Done not yet).
    pub working: bool,
    /// When the current turn started (for elapsed time display).
    pub started_at: Option<Instant>,
    /// Spinner animation state.
    pub spinner: SpinnerState,
    /// Per-turn usage (reset on user submit).
    pub usage: TurnUsage,
    /// Last turn result (set on TurnResult, consumed on Done).
    pub last_result: Option<TurnResultSummary>,
    /// Last title string written to terminal — used to deduplicate OSC 0 writes.
    pub last_title: Option<String>,
}

impl TurnState {
    /// Enter working state (turn started).
    /// Guard: only initialize timer if not already working.
    pub fn start(&mut self) {
        if !self.working {
            self.started_at = Some(Instant::now());
            self.spinner.reset();
        }
        self.working = true;
    }

    /// Exit working state (turn done/interrupted/error).
    pub fn finish(&mut self) {
        self.working = false;
        self.started_at = None;
    }

    /// Reset usage counters (new user message).
    pub fn reset_usage(&mut self) {
        self.usage = TurnUsage::default();
    }
}
