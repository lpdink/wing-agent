//! Terminal lifecycle management — init, restore, and crossterm event stream.
//!
//! Borrowed concepts from codex-rs/tui/src/tui.rs (Apache 2.0).

use std::fmt;
use std::io;
use std::io::Stdout;

use anyhow::Result;
use crossterm::Command;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableFocusChange;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub type WingTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Events from the terminal (keyboard, mouse, resize, paste, focus).
#[derive(Debug)]
pub enum TermEvent {
    Key(KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Paste(String),
    Resize(u16, u16),
    Focus(bool),
    Tick,
}

// ---------------------------------------------------------------------------
// Mouse reporting (DECSET 1000 + 1002 + 1006)
//
// Written by hand instead of using `crossterm::event::EnableMouseCapture`:
// the crossterm helper also enables any-motion reporting (`?1003`) and RXVT
// coordinates (`?1015`). We only want key press/release plus button-motion
// (drag) events in SGR encoding — no hover event flood, and the full
// `MouseEvent` (kind / modifiers / 0-based column & row) reaches the app,
// which is what later changes (text selection, scrollbar) build on.
// `?1003` (hover / any-motion) is to be turned on here and nowhere else —
// the scrollbar change is the one that needs it.
//
// Alternate scroll (DECSET 1007) is deliberately NOT used: it makes the
// terminal translate the wheel into plain Up/Down keys, which are
// indistinguishable from real arrow keys and therefore get swallowed by any
// panel that navigates with Up/Down.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableMouseReporting;

impl Command for EnableMouseReporting {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "EnableMouseReporting: WinAPI not supported, use ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Disable mouse reporting. The modes are turned off high-bit first, so the
/// terminal never ends up in a state where motion events are still enabled
/// while SGR encoding is already off.
///
/// `pub` so the panic hook (`cmd`) can share the one teardown sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisableMouseReporting;

impl Command for DisableMouseReporting {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1006l\x1b[?1002l\x1b[?1000l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "DisableMouseReporting: WinAPI not supported, use ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

/// Write the sequence that enters TUI mode: alternate screen, mouse
/// reporting, bracketed paste, focus reporting, hide cursor.
///
/// This is the *only* place that decides the setup order — `init_terminal`
/// must not write these commands ad hoc. The exact bytes are asserted by
/// `test_enter_sequence_is_exact`, so reordering the arguments here is a
/// test failure, not a silent behavior change.
pub fn enter_sequence(w: &mut impl io::Write) -> io::Result<()> {
    crossterm::execute!(
        w,
        EnterAlternateScreen,
        EnableMouseReporting,
        EnableBracketedPaste,
        EnableFocusChange,
        crossterm::cursor::Hide
    )?;
    Ok(())
}

/// Write the sequence that leaves TUI mode: mouse reporting off *first*, then
/// the alternate screen, bracketed paste, focus reporting, show cursor.
///
/// Ordering is load-bearing: an interrupted teardown must never leave the
/// terminal forwarding mouse reports to the shell. Shared by the clean-exit
/// path and the panic hook (both are byte-asserted by
/// `test_leave_sequence_is_exact`).
pub fn leave_sequence(w: &mut impl io::Write) -> io::Result<()> {
    crossterm::execute!(
        w,
        DisableMouseReporting,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableFocusChange,
        crossterm::cursor::Show
    )?;
    Ok(())
}

/// Initialize the terminal for TUI rendering.
pub fn init_terminal() -> Result<WingTerminal> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    enter_sequence(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
pub fn restore_terminal(terminal: &mut WingTerminal) -> Result<()> {
    leave_sequence(terminal.backend_mut())?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

/// Spawn a task that reads crossterm events and sends them over a channel.
///
/// Returns the receiver end. The task runs until the sender is dropped or
/// a fatal error occurs.
pub fn spawn_event_stream() -> mpsc::Receiver<TermEvent> {
    let (tx, rx) = mpsc::channel(128);
    tokio::spawn(async move {
        loop {
            // Poll crossterm with a timeout so we can emit ticks.
            let timeout = std::time::Duration::from_millis(100);
            match crossterm::event::poll(timeout) {
                Ok(true) => match crossterm::event::read() {
                    Ok(crossterm::event::Event::Key(key)) => {
                        if key.kind == KeyEventKind::Press
                            && tx.send(TermEvent::Key(key)).await.is_err()
                        {
                            break;
                        }
                    }
                    Ok(crossterm::event::Event::Mouse(mouse)) => {
                        if tx.send(TermEvent::Mouse(mouse)).await.is_err() {
                            break;
                        }
                    }
                    Ok(crossterm::event::Event::Resize(w, h)) => {
                        if tx.send(TermEvent::Resize(w, h)).await.is_err() {
                            break;
                        }
                    }
                    Ok(crossterm::event::Event::Paste(text)) => {
                        if tx.send(TermEvent::Paste(text)).await.is_err() {
                            break;
                        }
                    }
                    Ok(crossterm::event::Event::FocusGained) => {
                        let _ = tx.send(TermEvent::Focus(true)).await;
                    }
                    Ok(crossterm::event::Event::FocusLost) => {
                        let _ = tx.send(TermEvent::Focus(false)).await;
                    }
                    Err(e) => {
                        tracing::error!("crossterm read error: {e}");
                        break;
                    }
                },
                Ok(false) => {
                    if tx.send(TermEvent::Tick).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("crossterm poll error: {e}");
                    break;
                }
            }
        }
    });
    rx
}

/// Check if a key event is the quit shortcut (Ctrl+C).
pub fn is_quit_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact bytes of each path, spelled out once so that reordering or
    /// extending a sequence is a test failure rather than a silent change.
    const ENTER_BYTES: &str =
        "\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[?25l";
    const LEAVE_BYTES: &str =
        "\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1049l\x1b[?2004l\x1b[?1004l\x1b[?25h";

    fn ansi_of(cmd: impl Command) -> String {
        let mut out = String::new();
        cmd.write_ansi(&mut out).expect("write_ansi into a String");
        out
    }

    /// Parse `\x1b[?<n><h|l>` sequences into `(mode, enabled)` pairs. Panics on
    /// a segment that does not parse, so a truncated or unexpected sequence
    /// cannot pass by being silently dropped.
    fn parse_modes(seq: &str) -> Vec<(u32, bool)> {
        seq.split("\x1b[?")
            .skip(1)
            .map(|chunk| {
                let (digits, flag) =
                    chunk.split_at(chunk.len().checked_sub(1).expect("empty mode segment"));
                let mode = digits.parse().expect("mode number");
                let enabled = match flag {
                    "h" => true,
                    "l" => false,
                    other => panic!("unexpected mode flag {other:?} in {chunk:?}"),
                };
                (mode, enabled)
            })
            .collect()
    }

    #[test]
    fn test_enter_sequence_is_exact() {
        // `init_terminal` consumes this function, so the assertion covers the
        // real setup path: alt screen → mouse → paste → focus → hide cursor.
        let mut out = Vec::new();
        enter_sequence(&mut out).expect("write enter sequence");
        assert_eq!(String::from_utf8(out).unwrap(), ENTER_BYTES);
    }

    #[test]
    fn test_leave_sequence_is_exact() {
        // Both `restore_terminal` and the panic hook consume this function.
        // Mouse reporting must be off *before* `?1049l` leaves the alternate
        // screen — that ordering lives here and nowhere else.
        let mut out = Vec::new();
        leave_sequence(&mut out).expect("write leave sequence");
        assert_eq!(String::from_utf8_lossy(&out), LEAVE_BYTES);

        // Restoring twice just writes the same constant sequence again
        // (idempotent teardown; `disable_raw_mode` is a no-op when off).
        leave_sequence(&mut out).expect("write leave sequence again");
        assert_eq!(
            String::from_utf8_lossy(&out),
            format!("{LEAVE_BYTES}{LEAVE_BYTES}")
        );
    }

    #[test]
    fn test_sequences_leave_out_forbidden_modes() {
        // ?1003 (any-motion) floods the event loop with hover events,
        // ?1015 is RXVT coordinates (we want SGR / 1006 only) and ?1007
        // (alternate scroll) turns the wheel into arrow keys.
        for seq in [ENTER_BYTES, LEAVE_BYTES] {
            for forbidden in ["?1003", "?1015", "?1007"] {
                assert!(
                    !seq.contains(forbidden),
                    "{seq:?} must not touch {forbidden}"
                );
            }
        }
        assert_eq!(
            parse_modes(ENTER_BYTES),
            vec![
                (1049, true),
                (1000, true),
                (1002, true),
                (1006, true),
                (2004, true),
                (1004, true),
                (25, false),
            ]
        );
        assert_eq!(
            parse_modes(LEAVE_BYTES),
            vec![
                (1006, false),
                (1002, false),
                (1000, false),
                (1049, false),
                (2004, false),
                (1004, false),
                (25, true),
            ]
        );
    }

    #[test]
    fn test_mouse_reporting_sequences_are_exact() {
        assert_eq!(
            ansi_of(EnableMouseReporting),
            "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );
        assert_eq!(
            ansi_of(DisableMouseReporting),
            "\x1b[?1006l\x1b[?1002l\x1b[?1000l"
        );
    }

    #[test]
    fn test_disable_order_reverses_enable_order() {
        // High-bit-first teardown: the terminal must never sit in a state
        // where motion tracking is still on while SGR encoding is off.
        let mut expected: Vec<(u32, bool)> = parse_modes(&ansi_of(EnableMouseReporting))
            .into_iter()
            .map(|(mode, _)| (mode, false))
            .collect();
        expected.reverse();
        assert_eq!(parse_modes(&ansi_of(DisableMouseReporting)), expected);
    }

    #[test]
    fn test_term_event_mouse_keeps_coordinates_and_modifiers() {
        // Type guard only: it fails to compile if the payload stops carrying
        // the full `MouseEvent` (kind / modifiers / 0-based coords), which is
        // what selection & scrollbar build on. The actual forwarding in
        // `spawn_event_stream` is not unit-testable without a pty — it was
        // verified by a pty probe recorded in the commit that added it.
        let event = TermEvent::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 42,
            row: 7,
            modifiers: crossterm::event::KeyModifiers::SHIFT,
        });
        match event {
            TermEvent::Mouse(mouse) => {
                assert_eq!(mouse.kind, crossterm::event::MouseEventKind::ScrollUp);
                assert_eq!(mouse.column, 42);
                assert_eq!(mouse.row, 7);
                assert_eq!(mouse.modifiers, crossterm::event::KeyModifiers::SHIFT);
            }
            other => panic!("expected a mouse event, got {other:?}"),
        }
    }
}
