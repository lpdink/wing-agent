//! Clipboard via OSC52 — works over SSH and terminal multiplexers.

use std::io::Write;

use anyhow::{Context, Result};
use crossterm::clipboard::{ClipboardSelection, ClipboardType, CopyToClipboard};
use crossterm::execute;

/// Write `text` to the system clipboard via OSC52.
///
/// Requires a terminal that supports the OSC 52 escape sequence.
/// Works transparently over SSH and tmux (with `set-clipboard on`).
pub fn copy_to_clipboard(writer: &mut impl Write, text: &str) -> Result<()> {
    execute!(
        writer,
        CopyToClipboard {
            content: text.to_owned(),
            destination: ClipboardSelection(vec![ClipboardType::Clipboard]),
        }
    )
    .context("OSC52 clipboard write failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_writes_osc52_sequence() {
        let mut buf = Vec::new();
        copy_to_clipboard(&mut buf, "hello").unwrap();
        let output = String::from_utf8(buf).unwrap();
        // OSC52 format: ESC ] 52 ; c ; <base64> ST
        assert!(output.contains("52;c;"));
        // "hello" in base64 is "aGVsbG8="
        assert!(output.contains("aGVsbG8="));
    }

    #[test]
    fn copy_empty_string_does_not_panic() {
        let mut buf = Vec::new();
        copy_to_clipboard(&mut buf, "").unwrap();
    }
}
