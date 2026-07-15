//! Backend configuration reader — reads gateway settings from
//! `$WING_HOME/core/config.yaml` (default: `~/.wing/core/config.yaml`).
//!
//! The backend (Python) config is the single source of truth for
//! gateway host:port. The Rust frontend reads these values instead
//! of maintaining its own duplicate config.
//!
//! Also provides `tui_home()` and `wing_root()` path helpers used
//! across the CLI for log files, venv discovery, etc.

use std::path::PathBuf;

use serde::Deserialize;

/// Default gateway host.
const DEFAULT_HOST: &str = "127.0.0.1";
/// Default gateway port.
const DEFAULT_PORT: u16 = 32523;

/// Minimal view of the backend config — only the `gateway` section.
///
/// All other fields in `core/config.yaml` are ignored via
/// `#[serde(default)]` on the outer struct.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BackendConfigFile {
    gateway: GatewaySection,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct GatewaySection {
    host: String,
    port: u16,
}

impl Default for GatewaySection {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
        }
    }
}

/// Gateway endpoint read from the backend config file.
#[derive(Debug, Clone)]
pub struct BackendGatewayConfig {
    pub host: String,
    pub port: u16,
}

impl BackendGatewayConfig {
    /// WebSocket URL: `ws://{host}:{port}/ws`.
    #[allow(dead_code)]
    pub fn ws_url(&self) -> String {
        format!("ws://{}:{}/ws", self.host, self.port)
    }

    /// HTTP base URL: `http://{host}:{port}`.
    #[allow(dead_code)]
    pub fn http_base(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

/// Path to the wing root directory: `~/.wing/`.
///
/// Used for shared resources (e.g. venv, core config).
/// `WING_HOME` env var overrides the default `~/.wing`.
pub fn wing_root() -> PathBuf {
    if let Ok(home) = std::env::var("WING_HOME") {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".wing")
}

/// Path to the TUI data directory: `~/.wing/tui/`.
///
/// Used for TUI logs and config.
pub fn tui_home() -> PathBuf {
    if let Ok(home) = std::env::var("WING_HOME") {
        return PathBuf::from(home).join("tui");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".wing")
        .join("tui")
}

/// Read gateway settings from the backend config file.
///
/// Falls back to defaults (`127.0.0.1:32523`) if the file is missing
/// or unparseable. Missing file is logged at debug level; parse errors
/// at warn level.
pub fn read_backend_gateway_config() -> BackendGatewayConfig {
    let path = wing_root().join("core").join("config.yaml");

    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_yaml::from_str::<BackendConfigFile>(&content) {
            Ok(cfg) => {
                tracing::debug!(?path, host = %cfg.gateway.host, port = cfg.gateway.port,
                    "loaded gateway config from backend");
                BackendGatewayConfig {
                    host: cfg.gateway.host,
                    port: cfg.gateway.port,
                }
            }
            Err(e) => {
                tracing::warn!(?path, %e, "failed to parse backend config, using defaults");
                BackendGatewayConfig {
                    host: DEFAULT_HOST.to_string(),
                    port: DEFAULT_PORT,
                }
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(?path, "backend config not found, using defaults");
            BackendGatewayConfig {
                host: DEFAULT_HOST.to_string(),
                port: DEFAULT_PORT,
            }
        }
        Err(e) => {
            tracing::warn!(?path, %e, "failed to read backend config, using defaults");
            BackendGatewayConfig {
                host: DEFAULT_HOST.to_string(),
                port: DEFAULT_PORT,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_url() {
        let cfg = BackendGatewayConfig {
            host: "127.0.0.1".into(),
            port: 32523,
        };
        assert_eq!(cfg.ws_url(), "ws://127.0.0.1:32523/ws");
    }

    #[test]
    fn test_http_base() {
        let cfg = BackendGatewayConfig {
            host: "127.0.0.1".into(),
            port: 32523,
        };
        assert_eq!(cfg.http_base(), "http://127.0.0.1:32523");
    }

    #[test]
    fn test_parse_partial_yaml() {
        // Backend config has many fields; we only care about gateway.
        let yaml = r#"
openai:
  base_url: https://api.example.com
  api_key: sk-xxx
agents:
  - name: default
    model: gpt-4
gateway:
  host: 0.0.0.0
  port: 9000
"#;
        let cfg: BackendConfigFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.gateway.host, "0.0.0.0");
        assert_eq!(cfg.gateway.port, 9000);
    }

    #[test]
    fn test_parse_missing_gateway_section() {
        // No gateway section → defaults.
        let yaml = r#"
openai:
  base_url: https://api.example.com
  api_key: sk-xxx
"#;
        let cfg: BackendConfigFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.gateway.host, DEFAULT_HOST);
        assert_eq!(cfg.gateway.port, DEFAULT_PORT);
    }
}
