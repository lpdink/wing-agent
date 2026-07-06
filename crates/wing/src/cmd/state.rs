//! State file management — `~/.wing/state.json`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Top-level state persisted to `~/.wing/state.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WingState {
    /// Gateway daemon state, `None` when not running.
    pub gateway: Option<GatewayState>,
}

/// Gateway daemon state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayState {
    pub host: String,
    pub port: u16,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
}

impl WingState {
    /// Load state from `~/.wing/state.json`.
    ///
    /// Returns a default (empty) state if the file does not exist or is corrupted.
    pub fn load() -> Self {
        let path = state_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Atomically write state to `~/.wing/state.json`.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = state_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Write to temp file, then rename for atomicity.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Clear only the gateway field, preserving other state fields.
    pub fn clear_gateway() {
        let mut state = Self::load();
        if state.gateway.is_some() {
            state.gateway = None;
            state.save().ok();
        }
    }
}

/// Check if the gateway process is still running.
///
/// Uses `kill(pid, 0)` to verify PID existence. On Linux, additionally
/// checks `/proc/<pid>/cmdline` to detect PID reuse (defense in depth).
pub fn is_gateway_running(gw: &GatewayState) -> bool {
    #[cfg(unix)]
    {
        let pid = gw.pid as i32;
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }

        // Linux-only: verify process name to detect PID reuse.
        verify_process_name(gw.pid)
    }

    #[cfg(not(unix))]
    {
        let _ = gw;
        false
    }
}

/// Verify the process is actually a gateway (not a reused PID).
///
/// On Linux, reads `/proc/<pid>/cmdline`. On other Unix systems (macOS),
/// `kill(pid, 0)` alone is sufficient — PID reuse is rare in practice.
#[cfg(target_os = "linux")]
fn verify_process_name(pid: u32) -> bool {
    let cmdline = std::fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    cmdline.contains("python") || cmdline.contains("wing") || cmdline.contains("gateway")
}

#[cfg(all(unix, not(target_os = "linux")))]
fn verify_process_name(_pid: u32) -> bool {
    // KNOWN LIMITATION: macOS/BSD have no /proc, so we cannot verify the
    // process name to detect PID reuse. `kill(pid, 0)` is used as the sole
    // check. In practice, PID reuse is rare on these systems within the
    // short lifetime of a gateway daemon. A future improvement could use
    // `libproc::proc_name()` on macOS for proper verification.
    true
}

/// Path to the state file: `~/.wing/tui/state.json`.
fn state_path() -> PathBuf {
    tui_home().join("state.json")
}

/// Path to the TUI data directory: `~/.wing/tui/`.
pub fn tui_home() -> PathBuf {
    if let Ok(home) = std::env::var("WING_HOME") {
        return PathBuf::from(home).join("tui");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".wing")
        .join("tui")
}

/// Path to the wing root directory: `~/.wing/`.
///
/// Used for shared resources (e.g. venv) that are not TUI-specific.
pub fn wing_root() -> PathBuf {
    if let Ok(home) = std::env::var("WING_HOME") {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".wing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper: read state from an explicit path (bypasses env var).
    fn load_from_path(path: &std::path::Path) -> WingState {
        match fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => WingState::default(),
        }
    }

    /// Helper: write state to an explicit path (bypasses env var).
    fn save_to_path(state: &WingState, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    #[test]
    fn default_state_has_no_gateway() {
        let state = WingState::default();
        assert!(state.gateway.is_none());
    }

    #[test]
    fn roundtrip_serialization() {
        let state = WingState {
            gateway: Some(GatewayState {
                host: "127.0.0.1".into(),
                port: 32523,
                pid: 12345,
                started_at: Utc::now(),
            }),
        };
        let json = serde_json::to_string(&state).unwrap();
        let loaded: WingState = serde_json::from_str(&json).unwrap();
        assert!(loaded.gateway.is_some());
        let gw = loaded.gateway.unwrap();
        assert_eq!(gw.host, "127.0.0.1");
        assert_eq!(gw.port, 32523);
        assert_eq!(gw.pid, 12345);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = std::env::temp_dir().join("wing-test-missing");
        fs::create_dir_all(&dir).ok();
        let path = dir.join("state.json");
        fs::remove_file(&path).ok();

        let state = load_from_path(&path);
        assert!(state.gateway.is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_corrupted_json_returns_default() {
        let dir = std::env::temp_dir().join("wing-test-corrupt");
        fs::create_dir_all(&dir).ok();
        let path = dir.join("state.json");
        fs::write(&path, "not valid json{{{").unwrap();

        let state = load_from_path(&path);
        assert!(state.gateway.is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("wing-test-roundtrip");
        fs::create_dir_all(&dir).ok();
        let path = dir.join("state.json");

        let state = WingState {
            gateway: Some(GatewayState {
                host: "0.0.0.0".into(),
                port: 9999,
                pid: 42,
                started_at: Utc::now(),
            }),
        };
        save_to_path(&state, &path).unwrap();

        let loaded = load_from_path(&path);
        assert!(loaded.gateway.is_some());
        assert_eq!(loaded.gateway.unwrap().port, 9999);

        fs::remove_file(&path).ok();
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn is_gateway_running_returns_false_for_dead_pid() {
        let gw = GatewayState {
            host: "127.0.0.1".into(),
            port: 32523,
            pid: 999_999_999, // Almost certainly not a real PID.
            started_at: Utc::now(),
        };
        assert!(!is_gateway_running(&gw));
    }
}
