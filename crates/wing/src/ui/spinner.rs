//! Spinner state.
//!
//! Ships the MiniDot frame sequence and the elapsed-time formatting the
//! composer's activity rail reads (`ui::input_area::chrome` draws the glyph,
//! the "Working..." label and the timer into the card's top border). Frame
//! timing is driven externally via [`SpinnerState::tick`].

use std::time::Duration;

/// MiniDot braille frames (12 FPS, ~83ms interval).
const MINIDOT_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Interval between frame advances.
const INTERVAL: Duration = Duration::from_millis(83);

/// Mutable spinner state — tracks current frame and accumulated time.
#[derive(Debug, Clone)]
pub struct SpinnerState {
    frame: usize,
    elapsed: Duration,
}

impl SpinnerState {
    pub fn new() -> Self {
        Self {
            frame: 0,
            elapsed: Duration::ZERO,
        }
    }

    /// Advance the spinner by `dt`. May skip multiple frames if `dt` is large.
    ///
    /// Unlike ratatui-cheese, no empty-frames guard is needed here:
    /// `MINIDOT_FRAMES` is a compile-time constant that is always non-empty.
    pub fn tick(&mut self, dt: Duration) {
        self.elapsed += dt;
        while self.elapsed >= INTERVAL {
            self.elapsed -= INTERVAL;
            self.frame = (self.frame + 1) % MINIDOT_FRAMES.len();
        }
    }

    /// Current frame character (single-cell braille glyph).
    pub fn frame_str(&self) -> &'static str {
        MINIDOT_FRAMES[self.frame]
    }

    /// Reset to the first frame.
    pub fn reset(&mut self) {
        self.frame = 0;
        self.elapsed = Duration::ZERO;
    }
}

impl Default for SpinnerState {
    fn default() -> Self {
        Self::new()
    }
}

/// Format elapsed seconds into compact human-friendly form.
///
/// Read by the composer's activity rail, which prints it next to the label:
/// `⠋ Working... (1m 05s · Esc to interrupt)`.
pub(crate) fn fmt_elapsed(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        return format!("{elapsed_secs}s");
    }
    if elapsed_secs < 3600 {
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_frame_is_first() {
        let s = SpinnerState::new();
        assert_eq!(s.frame_str(), "⠋");
    }

    #[test]
    fn tick_advances_frame() {
        let mut s = SpinnerState::new();
        s.tick(INTERVAL);
        assert_eq!(s.frame_str(), "⠙");
    }

    #[test]
    fn tick_wraps_around() {
        let mut s = SpinnerState::new();
        s.tick(INTERVAL * MINIDOT_FRAMES.len() as u32);
        assert_eq!(s.frame_str(), "⠋");
    }

    #[test]
    fn sub_interval_tick_does_not_advance() {
        let mut s = SpinnerState::new();
        s.tick(Duration::from_millis(10));
        assert_eq!(s.frame_str(), "⠋");
    }

    #[test]
    fn large_dt_skips_frames() {
        let mut s = SpinnerState::new();
        s.tick(INTERVAL * 3);
        assert_eq!(s.frame_str(), "⠸");
    }

    #[test]
    fn reset_returns_to_first_frame() {
        let mut s = SpinnerState::new();
        s.tick(INTERVAL * 5);
        s.reset();
        assert_eq!(s.frame_str(), "⠋");
    }

    #[test]
    fn fmt_elapsed_seconds() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(59), "59s");
    }

    #[test]
    fn fmt_elapsed_minutes() {
        assert_eq!(fmt_elapsed(60), "1m 00s");
        assert_eq!(fmt_elapsed(65), "1m 05s");
    }

    #[test]
    fn fmt_elapsed_hours() {
        assert_eq!(fmt_elapsed(3600), "1h 00m 00s");
        assert_eq!(fmt_elapsed(3661), "1h 01m 01s");
    }
}
