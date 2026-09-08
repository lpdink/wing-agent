//! Tracing-based logging with file output.
//!
//! Policy (kept in sync with the Python backend, see AGENTS.md "Logging"):
//! - One file per **local** calendar day: `$WING_HOME/tui/logs/wing_YYYY-MM-DD.log`
//!   (default `~/.wing/tui/logs/`), opened in append mode so TUI restarts
//!   never truncate or fork the log. `tracing-appender`'s built-in daily
//!   rotation is UTC-based, hence this hand-rolled local-date writer.
//! - Files older than [`RETENTION_DAYS`] are pruned at startup, including
//!   legacy `wing.log.YYYY-MM-DD` names.
//! - Console output is intentionally disabled — the TUI owns the terminal.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Datelike, Local, NaiveDate};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*;

/// Days of log files kept on either side of the front/back split.
const RETENTION_DAYS: i64 = 7;

/// Initialize the tracing subscriber.
///
/// Must be called once at startup. The returned guard must be held for the
/// lifetime of the program to ensure log flushing on exit.
pub fn init_logging() -> WorkerGuard {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok();
    prune_old_logs(&dir, Local::now().date_naive());

    let writer = DailyFileWriter::new(dir.clone());
    let (non_blocking, guard) = tracing_appender::non_blocking(writer);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("wing=warn,tokio_tungstenite=warn"));

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_timer(LocalTimer)
                .with_target(true)
                .with_thread_ids(true)
                .with_ansi(false),
        )
        .init();

    tracing::info!(log_dir = %dir.display(), "logging initialized");
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

/// File name for a given local date: `wing_YYYY-MM-DD.log`.
fn log_file_name(day: NaiveDate) -> String {
    format!(
        "wing_{:04}-{:02}-{:02}.log",
        day.year(),
        day.month(),
        day.day()
    )
}

/// Parse the local date out of a log file name.
///
/// Accepts the current `wing_YYYY-MM-DD.log` naming, the legacy
/// `wing.log.YYYY-MM-DD` naming, and the backend's legacy per-process
/// `wing_YYYY-MM-DD-HH-MM-SS.log` naming. Unparseable names yield `None`
/// and are left untouched by pruning.
fn parse_log_date(name: &str) -> Option<NaiveDate> {
    if let Some(rest) = name
        .strip_prefix("wing_")
        .and_then(|r| r.strip_suffix(".log"))
        && let Some(date_part) = rest.get(0..10)
    {
        return NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok();
    }
    if let Some(rest) = name.strip_prefix("wing.log.") {
        return NaiveDate::parse_from_str(rest, "%Y-%m-%d").ok();
    }
    None
}

/// Delete log files whose embedded date is older than the retention window.
fn prune_old_logs(dir: &Path, today: NaiveDate) {
    let cutoff = today - chrono::Duration::days(RETENTION_DAYS - 1);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if let Some(day) = parse_log_date(&name.to_string_lossy())
            && day < cutoff
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

type NowFn = Arc<dyn Fn() -> NaiveDate + Send + Sync>;

/// Append-only daily log writer that rolls at **local** midnight.
///
/// The file is opened lazily on the first write and re-checked on every
/// write, so a long-running TUI crosses midnight without a restart.
struct DailyFileWriter {
    dir: PathBuf,
    now: NowFn,
    file: Option<(NaiveDate, File)>,
}

impl DailyFileWriter {
    fn new(dir: PathBuf) -> Self {
        Self::with_now(dir, Arc::new(|| Local::now().date_naive()))
    }

    fn with_now(dir: PathBuf, now: NowFn) -> Self {
        Self {
            dir,
            now,
            file: None,
        }
    }

    /// Ensure the writer points at today's file, rolling if the date changed.
    fn ensure_current(&mut self) -> io::Result<&mut File> {
        let today = (self.now)();
        if !matches!(&self.file, Some((day, _)) if *day == today) {
            let path = self.dir.join(log_file_name(today));
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            self.file = Some((today, file));
        }
        Ok(&mut self.file.as_mut().expect("file set above").1)
    }
}

impl Write for DailyFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.ensure_current()?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.file {
            Some((_, file)) => file.flush(),
            None => Ok(()),
        }
    }
}

/// Timestamps in local time, matching the backend's `YYYY-MM-DD HH:MM:SS`
/// line prefix so both log families grep the same way.
struct LocalTimer;

impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", Local::now().format("%Y-%m-%d %H:%M:%S%.3f"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wing-log-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_dates_for_all_naming_schemes() {
        assert_eq!(
            parse_log_date("wing_2026-09-08.log"),
            NaiveDate::from_ymd_opt(2026, 9, 8)
        );
        // Legacy tracing-appender naming.
        assert_eq!(
            parse_log_date("wing.log.2026-09-07"),
            NaiveDate::from_ymd_opt(2026, 9, 7)
        );
        // Legacy backend per-process naming.
        assert_eq!(
            parse_log_date("wing_2026-09-07-23-23-26.log"),
            NaiveDate::from_ymd_opt(2026, 9, 7)
        );
        assert_eq!(parse_log_date("new.log"), None);
        assert_eq!(parse_log_date("gateway.log"), None);
        assert_eq!(parse_log_date("wing_not-a-date.log"), None);
    }

    #[test]
    fn prunes_old_files_keeps_recent() {
        let dir = scratch_dir("prune");
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();

        let expired = [
            "wing_2026-08-30.log",          // current naming, too old
            "wing.log.2026-08-20",          // legacy naming, too old
            "wing_2026-08-31-01-02-03.log", // legacy backend naming, too old
        ];
        let kept = [
            "wing_2026-09-02.log",
            "wing.log.2026-09-07",
            "wing_2026-09-08.log",
        ];
        let unrelated = ["gateway.log", "wing_not-a-date.log", "new.log"];

        for name in expired.iter().chain(kept.iter()).chain(unrelated.iter()) {
            std::fs::write(dir.join(name), b"").unwrap();
        }

        prune_old_logs(&dir, today);

        for name in expired {
            assert!(!dir.join(name).exists(), "{name} should have been pruned");
        }
        for name in kept.iter().chain(unrelated.iter()) {
            assert!(dir.join(name).exists(), "{name} should have been kept");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_writer_appends_and_rolls_at_midnight() {
        let dir = scratch_dir("roll");
        let day = Arc::new(Mutex::new(NaiveDate::from_ymd_opt(2026, 9, 8).unwrap()));
        let clock = Arc::clone(&day);
        let mut writer =
            DailyFileWriter::with_now(dir.clone(), Arc::new(move || *clock.lock().unwrap()));

        writer.write_all(b"day one\n").unwrap();
        writer.flush().unwrap();

        *day.lock().unwrap() = NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
        writer.write_all(b"day two\n").unwrap();
        writer.flush().unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("wing_2026-09-08.log")).unwrap(),
            "day one\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("wing_2026-09-09.log")).unwrap(),
            "day two\n"
        );

        // Same-day re-open appends instead of truncating.
        let mut again = DailyFileWriter::with_now(
            dir.clone(),
            Arc::new(|| NaiveDate::from_ymd_opt(2026, 9, 9).unwrap()),
        );
        again.write_all(b"day two more\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("wing_2026-09-09.log")).unwrap(),
            "day two\nday two more\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
