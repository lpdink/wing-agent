//! Tracing-based logging with file output.
//!
//! Logs are written to `$WING_HOME/tui/logs/wing.log` (default `~/.wing/tui/logs/`).
//! Console output is intentionally disabled — the TUI owns the terminal.

use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

/// Initialize the tracing subscriber.
///
/// Must be called once at startup. The returned guard must be held for the
/// lifetime of the program to ensure log flushing on exit.
pub fn init_logging() -> WorkerGuard {
    let log_dir = log_dir();
    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = tracing_appender::rolling::daily(&log_dir, "wing.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("wing=warn,tokio_tungstenite=warn"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_target(true)
                .with_thread_ids(true)
                .with_ansi(false),
        )
        .init();

    tracing::info!(log_dir = %log_dir.display(), "logging initialized");
    guard
}

/// Determine the log directory.
fn log_dir() -> PathBuf {
    if let Ok(home) = std::env::var("WING_HOME") {
        return PathBuf::from(home).join("tui").join("logs");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".wing")
        .join("tui")
        .join("logs")
}
