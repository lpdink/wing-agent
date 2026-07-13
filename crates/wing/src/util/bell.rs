//! Terminal bell (BEL) — triggers system-level notification.
//!
//! Writing the BEL character (`\x07`) to stdout causes the terminal emulator
//! to signal attention. On macOS (Ghostty, iTerm2), this produces a system
//! notification when the tab/window is not focused. Works transparently
//! over SSH and WSL.

use std::io::Write;

use anyhow::{Context, Result};

/// Send the BEL character to trigger a terminal notification.
///
/// Only effective when the terminal window/tab is not focused.
/// Most terminal emulators silently ignore BEL when already focused.
pub fn send_bell(writer: &mut impl Write) -> Result<()> {
    writer.write_all(b"\x07").context("failed to send BEL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_bell_writes_bel_character() {
        let mut buf = Vec::new();
        send_bell(&mut buf).unwrap();
        assert_eq!(buf, b"\x07");
    }
}
