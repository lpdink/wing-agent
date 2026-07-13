//! Terminal lifecycle management — init, restore, and crossterm event stream.
//!
//! Borrowed concepts from codex-rs/tui/src/tui.rs (Apache 2.0).

use std::io;
use std::io::Stdout;

use anyhow::Result;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableFocusChange;
use crossterm::event::EnableMouseCapture;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::MouseEventKind;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

pub type WingTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Events from the terminal (keyboard, mouse, resize, ticks, paste, focus).
#[derive(Debug)]
pub enum TermEvent {
    Key(KeyEvent),
    Mouse(MouseAction),
    Paste(String),
    Resize(u16, u16),
    Focus(bool),
    Tick,
}

/// Simplified mouse actions we care about.
#[derive(Debug)]
pub enum MouseAction {
    ScrollUp,
    ScrollDown,
}

/// Initialize the terminal for TUI rendering.
pub fn init_terminal() -> Result<WingTerminal> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
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
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture,
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
                        let action = match mouse.kind {
                            MouseEventKind::ScrollUp => Some(MouseAction::ScrollUp),
                            MouseEventKind::ScrollDown => Some(MouseAction::ScrollDown),
                            _ => None,
                        };
                        if let Some(action) = action
                            && tx.send(TermEvent::Mouse(action)).await.is_err()
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
