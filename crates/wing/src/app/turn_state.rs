//! TurnState — per-turn tracking for agent working state.

use std::time::Instant;

use crate::ui::spinner::SpinnerState;
use crate::ui::status_bar::TurnUsage;

/// Parse a UTC ISO-8601 turn-start timestamp into a local monotonic `Instant`
/// representing "when the turn started".
///
/// Used on resume: the backend sends `turn_started_at` (UTC) so the TUI can
/// restore the *real* elapsed time instead of recounting from the resume
/// moment. Clock skew is saturated — a timestamp in the future (negative
/// elapsed) clamps to zero (i.e. `Instant::now()`) rather than panicking or
/// wrapping to a huge value. Returns `None` only when the string is unparseable
/// or the elapsed exceeds the monotonic clock's range (caller keeps its
/// existing `started_at`).
pub fn instant_from_utc_iso(turn_started_at: &str) -> Option<Instant> {
    let started = chrono::DateTime::parse_from_rfc3339(turn_started_at).ok()?;
    let elapsed = chrono::Utc::now().signed_duration_since(started);
    // `to_std()` errors on a negative duration → clamp to zero (clock skew /
    // cross-machine resume).
    let elapsed = elapsed.to_std().unwrap_or_default();
    Instant::now().checked_sub(elapsed)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_past_timestamp_to_elapsed_instant() {
        let ts = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        let instant = instant_from_utc_iso(&ts).expect("parseable");
        let elapsed = instant.elapsed().as_secs();
        assert!(
            (3..=7).contains(&elapsed),
            "elapsed should be ~5s, got {elapsed}s"
        );
    }

    #[test]
    fn future_timestamp_clamps_to_now() {
        // Clock skew / cross-machine resume: a timestamp in the future yields a
        // negative elapsed → clamp to zero (Instant::now()), never panic or wrap
        // to a huge value.
        let ts = (chrono::Utc::now() + chrono::Duration::seconds(3600)).to_rfc3339();
        let instant = instant_from_utc_iso(&ts).expect("parseable");
        assert!(
            instant.elapsed().as_secs() < 2,
            "future timestamp must clamp elapsed to ~0"
        );
    }

    #[test]
    fn unparseable_returns_none() {
        assert!(instant_from_utc_iso("not-a-timestamp").is_none());
        assert!(instant_from_utc_iso("").is_none());
    }
}
