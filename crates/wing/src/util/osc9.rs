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
/// The `message` is sanitized to prevent OSC sequence injection:
/// C0 control characters (except newline) and DEL are stripped.
/// The terminal window title (set via OSC 0) is typically used as
/// the notification subtitle.
pub fn send_notification(writer: &mut impl Write, message: &str) -> Result<()> {
    let safe: String = message
        .chars()
        .filter(|&c| c == '\n' || (c >= ' ' && c != '\x7f'))
        .collect();
    // OSC 9 format: ESC ] 9 ; <message> BEL
    write!(writer, "\x1b]9;{safe}\x07").context("failed to send OSC 9 notification")
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

/// Format a TurnResult notification message.
///
/// Stats come first (always visible), result text follows (may be
/// truncated by the terminal emulator's notification display).
///
/// Example outputs:
/// - `"3 turns · 12s · 2.1k tokens\nCreated hello.rs"`
/// - `"1 turn · 5s"` (when result is empty)
pub fn fmt_turn_result(
    result: Option<&str>,
    num_turns: i64,
    duration_ms: i64,
    total_tokens: Option<i64>,
) -> String {
    // Stats line (always visible).
    let mut stats = Vec::new();
    if num_turns > 0 {
        stats.push(if num_turns == 1 {
            "1 turn".into()
        } else {
            format!("{num_turns} turns")
        });
    }
    stats.push(fmt_duration(duration_ms));
    if let Some(tokens) = total_tokens
        && tokens > 0
    {
        stats.push(fmt_tokens_compact(tokens));
    }
    let stats_line = stats.join(" · ");

    // Result text (may be truncated by terminal — we don't truncate ourselves).
    let result_text = result
        .map(|t| {
            t.lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if result_text.is_empty() {
        stats_line
    } else {
        format!("{stats_line}\n{result_text}")
    }
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
    fn send_notification_strips_control_chars() {
        let mut buf = Vec::new();
        send_notification(&mut buf, "hello\x07world\x1b!").unwrap();
        let output = String::from_utf8(buf).unwrap();
        // \x07 and \x1b are stripped, only printable chars remain.
        assert_eq!(output, "\x1b]9;helloworld!\x07");
    }

    #[test]
    fn send_notification_preserves_newlines() {
        let mut buf = Vec::new();
        send_notification(&mut buf, "line1\nline2").unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "\x1b]9;line1\nline2\x07");
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
    fn fmt_tokens_compact_boundaries() {
        assert_eq!(fmt_tokens_compact(999), "999 tokens");
        assert_eq!(fmt_tokens_compact(1000), "1.0k tokens");
        assert_eq!(fmt_tokens_compact(999_999), "1000.0k tokens");
        assert_eq!(fmt_tokens_compact(1_000_000), "1.0M tokens");
    }

    #[test]
    fn fmt_turn_result_success_with_result() {
        let msg = fmt_turn_result(Some("Created hello.rs"), 3, 12000, Some(2100));
        assert_eq!(msg, "3 turns · 12s · 2.1k tokens\nCreated hello.rs");
    }

    #[test]
    fn fmt_turn_result_no_result() {
        let msg = fmt_turn_result(None, 2, 5000, None);
        assert_eq!(msg, "2 turns · 5s");
    }

    #[test]
    fn fmt_turn_result_single_turn() {
        let msg = fmt_turn_result(None, 1, 3000, Some(500));
        assert_eq!(msg, "1 turn · 3s · 500 tokens");
    }

    #[test]
    fn fmt_turn_result_multiline_result() {
        let msg = fmt_turn_result(Some("Line 1\nLine 2\nLine 3"), 1, 1000, None);
        assert_eq!(msg, "1 turn · 1s\nLine 1\nLine 2\nLine 3");
    }
}
