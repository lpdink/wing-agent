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

/// Initialize the terminal for TUI rendering.
pub fn init_terminal() -> Result<WingTerminal> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseReporting,
        EnableBracketedPaste,
        EnableFocusChange,
        crossterm::cursor::Hide
    )?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
///
/// Mouse reporting is disabled *before* leaving the alternate screen, so the
/// teardown order mirrors the setup order in reverse: an interrupted restore
/// can never leave the terminal forwarding mouse reports to the shell.
pub fn restore_terminal(terminal: &mut WingTerminal) -> Result<()> {
    crossterm::execute!(
        terminal.backend_mut(),
        DisableMouseReporting,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableFocusChange,
        crossterm::cursor::Show
    )?;
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

    fn ansi_of(cmd: impl Command) -> String {
        let mut out = String::new();
        cmd.write_ansi(&mut out).expect("write_ansi into a String");
        out
    }

    /// Parse `\x1b[?<n><h|l>` sequences into `(mode, enabled)` pairs.
    fn parse_modes(seq: &str) -> Vec<(u32, bool)> {
        seq.split("\x1b[?")
            .skip(1)
            .filter_map(|chunk| {
                let (digits, flag) = chunk.split_at(chunk.len().checked_sub(1)?);
                Some((digits.parse().ok()?, flag == "h"))
            })
            .collect()
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
    fn test_mouse_reporting_leaves_out_forbidden_modes() {
        // ?1003 (any-motion) floods the event loop with hover events,
        // ?1015 is RXVT coordinates (we want SGR / 1006 only) and ?1007
        // (alternate scroll) turns the wheel into arrow keys.
        for seq in [
            ansi_of(EnableMouseReporting),
            ansi_of(DisableMouseReporting),
        ] {
            for forbidden in ["?1003", "?1015", "?1007"] {
                assert!(
                    !seq.contains(forbidden),
                    "{seq:?} must not touch {forbidden}"
                );
            }
        }
        assert_eq!(
            parse_modes(&ansi_of(EnableMouseReporting)),
            vec![(1000, true), (1002, true), (1006, true)]
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
        // The event layer must not flatten mouse events into a direction-only
        // enum: selection / scrollbar need kind, modifiers and 0-based coords.
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
