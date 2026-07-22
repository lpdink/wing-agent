//! Terminal title management via OSC 0 escape sequence.
//!
//! Sets the terminal tab/window title to reflect the current agent state:
//! - **Idle**: `☾ wing` (waning crescent moon, U+263E)
//! - **Working**: `<spinner_frame> wing` (braille animation, synced with UI spinner)
//! - **Attention**: `❓ wing` (ask) / `⚠ wing` (error) / `✓ wing` (done)
//!
//! When a workdir is known, its last path component is appended as a suffix
//! (e.g. `☾ wing [myproject]`) so multiple `wing` tabs are distinguishable.

use std::io::Write;

use anyhow::{Context, Result};
use crossterm::execute;
use crossterm::terminal::SetTitle;

/// Attention kind for title display when the terminal is not focused.
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

/// Append a ` [dir]` suffix to a base title when a directory label is present.
fn apply_dir(base: &str, dir: Option<&str>) -> String {
    match dir {
        Some(d) if !d.is_empty() => format!("{base} [{d}]"),
        _ => base.to_string(),
    }
}

/// Extract the last path component of a workdir for use as a title suffix.
///
/// Returns `None` for empty paths or roots with no meaningful directory name.
pub fn dir_label(workdir: Option<&str>) -> Option<String> {
    let wd = workdir?.trim();
    if wd.is_empty() {
        return None;
    }
    let name = std::path::Path::new(wd).file_name()?;
    let s = name.to_string_lossy().into_owned();
    if s.is_empty() { None } else { Some(s) }
}

/// Format the title for working state with the given spinner frame.
pub fn title_working(frame: &str, dir: Option<&str>) -> String {
    let base = format!("{frame} wing");
    apply_dir(&base, dir)
}

/// Return the idle title.
pub fn title_idle(dir: Option<&str>) -> String {
    apply_dir(IDLE_TITLE, dir)
}

/// Format the title for an attention event.
pub fn title_attention(kind: AttentionKind, dir: Option<&str>) -> String {
    let base = match kind {
        AttentionKind::Ask => "❓ wing",
        AttentionKind::Error => "⚠ wing",
        AttentionKind::Done => "✓ wing",
    };
    apply_dir(base, dir)
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
        assert_eq!(title_working("⠋", None), "⠋ wing");
        assert_eq!(title_working("⠙", Some("myproject")), "⠙ wing [myproject]");
    }

    #[test]
    fn title_idle_returns_moon() {
        assert_eq!(title_idle(None), "☾ wing");
        assert_eq!(title_idle(Some("myproject")), "☾ wing [myproject]");
    }

    #[test]
    fn title_attention_variants() {
        assert_eq!(title_attention(AttentionKind::Ask, None), "❓ wing");
        assert_eq!(title_attention(AttentionKind::Error, None), "⚠ wing");
        assert_eq!(title_attention(AttentionKind::Done, None), "✓ wing");
        assert_eq!(
            title_attention(AttentionKind::Ask, Some("proj")),
            "❓ wing [proj]"
        );
    }

    #[test]
    fn set_title_empty_string() {
        let mut buf = Vec::new();
        set_title(&mut buf, "").unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("\x1b]0;\x07"));
    }

    #[test]
    fn dir_label_extracts_last_component() {
        assert_eq!(
            dir_label(Some("/home/user/myproject")),
            Some("myproject".into())
        );
        assert_eq!(
            dir_label(Some("/home/user/myproject/")),
            Some("myproject".into())
        );
        assert_eq!(dir_label(Some("relative/dir")), Some("dir".into()));
    }

    #[test]
    fn dir_label_none_for_empty_or_root() {
        assert_eq!(dir_label(None), None);
        assert_eq!(dir_label(Some("")), None);
        assert_eq!(dir_label(Some("   ")), None);
        assert_eq!(dir_label(Some("/")), None);
    }
}
