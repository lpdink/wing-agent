//! Terminal title management via OSC 0 escape sequence.
//!
//! Sets the terminal tab/window title to reflect the current agent state:
//! - **Idle**: `☾ wing` (waning crescent moon, U+263E)
//! - **Working**: `<spinner_frame> wing` (braille animation, synced with UI spinner)
//! - **Attention**: `❓ wing` (ask) / `⚠ wing` (error) / `✓ wing` (done)

use std::io::Write;

use anyhow::{Context, Result};
use crossterm::execute;
use crossterm::terminal::SetTitle;

/// Attention kind for title display when terminal is not focused.
#[derive(Debug, Clone, Copy)]
pub enum AttentionKind {
    /// Ask event — agent needs user input.
    Ask,
    /// Error event — something went wrong.
    Error,
    /// Turn completed successfully.
    Done,
}

/// Idle title: waning crescent moon + wing.
const IDLE_TITLE: &str = "☾ wing";

/// Format the title for working state with the given spinner frame.
pub fn title_working(frame: &str) -> String {
    format!("{frame} wing")
}

/// Return the idle title.
pub fn title_idle() -> &'static str {
    IDLE_TITLE
}

/// Format the title for an attention event.
pub fn title_attention(kind: AttentionKind) -> &'static str {
    match kind {
        AttentionKind::Ask => "❓ wing",
        AttentionKind::Error => "⚠ wing",
        AttentionKind::Done => "✓ wing",
    }
}

/// Write the terminal title via OSC 0 escape sequence.
pub fn set_title(writer: &mut impl Write, title: &str) -> Result<()> {
    execute!(writer, SetTitle(title)).context("failed to set terminal title")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_title_writes_osc0_sequence() {
        let mut buf = Vec::new();
        set_title(&mut buf, "hello").unwrap();
        let output = String::from_utf8(buf).unwrap();
        // OSC 0 format: ESC ] 0 ; <title> BEL
        assert!(output.contains("\x1b]0;hello\x07"), "got: {output:?}");
    }

    #[test]
    fn title_working_formats_correctly() {
        assert_eq!(title_working("⠋"), "⠋ wing");
        assert_eq!(title_working("⠙"), "⠙ wing");
    }

    #[test]
    fn title_idle_returns_moon() {
        assert_eq!(title_idle(), "☾ wing");
    }

    #[test]
    fn title_attention_variants() {
        assert_eq!(title_attention(AttentionKind::Ask), "❓ wing");
        assert_eq!(title_attention(AttentionKind::Error), "⚠ wing");
        assert_eq!(title_attention(AttentionKind::Done), "✓ wing");
    }

    #[test]
    fn set_title_empty_string() {
        let mut buf = Vec::new();
        set_title(&mut buf, "").unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\x1b]0;\x07"));
    }
}
