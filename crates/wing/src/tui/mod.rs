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

/// Events from the terminal (keyboard, resize, paste, focus).
#[derive(Debug)]
pub enum TermEvent {
    Key(KeyEvent),
    Paste(String),
    Resize(u16, u16),
    Focus(bool),
    Tick,
}

// ---------------------------------------------------------------------------
// Alternate Scroll (DECSET 1007)
//
// Tells the terminal to translate scroll-wheel / trackpad gestures into
// Up/Down arrow key sequences while in the alternate screen. This gives us
// scroll support *without* enabling full mouse capture, so native text
// selection (click-drag to copy) still works without holding Shift.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableAlternateScroll;

impl Command for EnableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "EnableAlternateScroll: WinAPI not supported, use ANSI",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[?1007l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "DisableAlternateScroll: WinAPI not supported, use ANSI",
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
        EnableAlternateScroll,
        EnableBracketedPaste,
        EnableFocusChange,
        crossterm::cursor::Hide
    )?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

/// Restore the terminal to its original state.
pub fn restore_terminal(terminal: &mut WingTerminal) -> Result<()> {
    crossterm::execute!(
        terminal.backend_mut(),
        DisableAlternateScroll,
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
                    // Mouse events are not captured (no EnableMouseCapture),
                    // so they won't arrive here. Ignore anything else.
                    Ok(_) => {}
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
