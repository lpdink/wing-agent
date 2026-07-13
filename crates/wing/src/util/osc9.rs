//! Desktop notifications via OSC 9 escape sequence.
//!
//! OSC 9 (`\x1b]9;message\x07`) triggers a desktop notification in terminals
//! that support it (Ghostty, iTerm2, WezTerm, etc.). The terminal emulator
//! displays a system-level notification with the message as body text.
//!
//! This is a best-effort signal: terminals that don't recognize OSC 9 will
//! silently ignore the sequence. Works transparently over SSH.

use std::io::Write;

use anyhow::{Context, Result};

/// Send a desktop notification via OSC 9 escape sequence.
///
/// The `message` is used as the notification body text. The terminal window
/// title (set via OSC 0) is typically used as the notification subtitle.
pub fn send_notification(writer: &mut impl Write, message: &str) -> Result<()> {
    // OSC 9 format: ESC ] 9 ; <message> ST
    // ST (String Terminator) can be either ESC \ or BEL (\x07)
    write!(writer, "\x1b]9;{}\x07", message).context("failed to send OSC 9 notification")
}

/// Format a duration in milliseconds to a compact human-readable string.
pub fn fmt_duration(ms: i64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let remain = secs % 60;
        format!("{mins}m{remain:02}s")
    }
}

/// Truncate text to a maximum number of lines, joining with " · ".
/// Strips leading/trailing whitespace from each line.
fn truncate_lines(text: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .take(max_lines)
        .collect();

    let joined = lines.join(" · ");
    if joined.chars().count() > max_chars {
        let truncated: String = joined.chars().take(max_chars).collect();
        format!("{truncated}…")
    } else {
        joined
    }
}

/// Format a TurnResult notification message.
///
/// Example outputs:
/// - `"I've created the hello world script. · 3 turns · 12s · 2.1k tokens"`
/// - `"3 turns · 12s · 2.1k tokens"` (when result is empty)
pub fn fmt_turn_result(
    result: Option<&str>,
    num_turns: i64,
    duration_ms: i64,
    total_tokens: Option<i64>,
) -> String {
    let mut parts = Vec::new();

    // Result text (truncated).
    if let Some(text) = result {
        let truncated = truncate_lines(text, 2, 80);
        if !truncated.is_empty() {
            parts.push(truncated);
        }
    }

    // Stats summary.
    let mut stats = Vec::new();
    if num_turns > 0 {
        stats.push(format!("{num_turns} turns"));
    }
    stats.push(fmt_duration(duration_ms));
    if let Some(tokens) = total_tokens
        && tokens > 0
    {
        stats.push(fmt_tokens_compact(tokens));
    }
    if !stats.is_empty() {
        parts.push(stats.join(" · "));
    }

    parts.join(" · ")
}

/// Format token count in compact notation.
fn fmt_tokens_compact(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M tokens", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k tokens", n as f64 / 1_000.0)
    } else {
        format!("{n} tokens")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_notification_writes_osc9_sequence() {
        let mut buf = Vec::new();
        send_notification(&mut buf, "hello").unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "\x1b]9;hello\x07");
    }

    #[test]
    fn fmt_duration_seconds() {
        assert_eq!(fmt_duration(5000), "5s");
        assert_eq!(fmt_duration(59000), "59s");
    }

    #[test]
    fn fmt_duration_minutes() {
        assert_eq!(fmt_duration(65000), "1m05s");
        assert_eq!(fmt_duration(125000), "2m05s");
    }

    #[test]
    fn truncate_lines_basic() {
        assert_eq!(truncate_lines("hello\nworld", 2, 80), "hello · world");
    }

    #[test]
    fn truncate_lines_max_chars() {
        let text = "this is a very long line that should be truncated";
        let result = truncate_lines(text, 1, 20);
        assert!(result.ends_with('…'));
        assert!(result.chars().count() <= 21); // 20 chars + …
    }

    #[test]
    fn fmt_turn_result_success_with_result() {
        let msg = fmt_turn_result(Some("Created hello.rs"), 3, 12000, Some(2100));
        assert_eq!(msg, "Created hello.rs · 3 turns · 12s · 2.1k tokens");
    }

    #[test]
    fn fmt_turn_result_no_result() {
        let msg = fmt_turn_result(None, 2, 5000, None);
        assert_eq!(msg, "2 turns · 5s");
    }

    #[test]
    fn fmt_turn_result_empty_result() {
        let msg = fmt_turn_result(Some(""), 1, 3000, Some(500));
        assert_eq!(msg, "1 turns · 3s · 500 tokens");
    }

    #[test]
    fn fmt_turn_result_multiline_result() {
        let msg = fmt_turn_result(Some("Line 1\nLine 2\nLine 3"), 1, 1000, None);
        assert_eq!(msg, "Line 1 · Line 2 · 1 turns · 1s");
    }
}
