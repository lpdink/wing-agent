//! stdio frontend — non-interactive mode for external orchestrators and scripts.
//!
//! Triggered by `-p` / `--prompt` or `--input-format stream-json`.
//! Supports three output formats: text, json, stream-json.
#![allow(clippy::print_stdout, clippy::print_stderr)]

pub mod ndjson;
pub mod renderer;
pub mod stdin_handler;

use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cmd::backend_config;
use crate::cmd::start;
use crate::gateway::GatewayClient;
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::models::{AgentOverride, CreateSessionRequest};

use self::renderer::StdioRenderer;

// ============================================================
// Output / Input format enums
// ============================================================

/// Output format for stdio mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Print only the final result text.
    #[default]
    Text,
    /// Print the final result as a single JSON object.
    Json,
    /// Stream NDJSON messages in real-time.
    StreamJson,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "stream-json" => Ok(Self::StreamJson),
            _ => Err(format!(
                "invalid output format: '{s}' (expected text, json, or stream-json)"
            )),
        }
    }
}

/// Input format for stdio mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InputFormat {
    /// Prompt from CLI argument.
    #[default]
    Text,
    /// Prompt from stdin as NDJSON (sub-task 4).
    StreamJson,
}

impl std::str::FromStr for InputFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "stream-json" => Ok(Self::StreamJson),
            _ => Err(format!(
                "invalid input format: '{s}' (expected text or stream-json)"
            )),
        }
    }
}

// ============================================================
// Stdio arguments
// ============================================================

/// Parsed CLI arguments for stdio mode.
#[derive(Debug, Clone)]
pub struct StdioArgs {
    pub prompt: String,
    pub model: Option<String>,
    pub resume: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub effort: Option<String>,
    pub output_format: OutputFormat,
    pub input_format: InputFormat,
    pub yolo: bool,
}

// ============================================================
// Argument filtering (for stdio mode compatibility with SDK)
// ============================================================

use std::collections::HashSet;

use clap::CommandFactory;

use crate::cmd::Cli;

/// Known flags derived from clap's [`Cli`] definition.
///
/// Built once at startup. Eliminates the need for hand-maintained
/// whitelists — adding a new `#[arg]` to [`Cli`] automatically
/// makes the filter aware.
struct KnownFlags {
    /// Long flags that take a value argument (e.g. `--model`, `--max-turns`).
    long_with_value: HashSet<String>,
    /// Long boolean flags (e.g. `--yolo`, `--help`, `--version`).
    long_bool: HashSet<String>,
    /// Short flags that take a value argument (e.g. `-p`, `-m`, `-r`).
    short_with_value: HashSet<char>,
    /// Short boolean flags.
    short_bool: HashSet<char>,
}

impl KnownFlags {
    fn from_cli() -> Self {
        let cmd = Cli::command();
        let mut long_with_value = HashSet::new();
        let mut long_bool = HashSet::new();
        let mut short_with_value = HashSet::new();
        let mut short_bool = HashSet::new();

        for arg in cmd.get_arguments() {
            let takes_value = arg.get_action().takes_values();
            if let Some(long) = arg.get_long() {
                let flag = format!("--{long}");
                if takes_value {
                    long_with_value.insert(flag);
                } else {
                    long_bool.insert(flag);
                }
            }
            if let Some(short) = arg.get_short() {
                if takes_value {
                    short_with_value.insert(short);
                } else {
                    short_bool.insert(short);
                }
            }
        }

        Self {
            long_with_value,
            long_bool,
            short_with_value,
            short_bool,
        }
    }
}

/// Check if stdio mode should be triggered based on raw args.
///
/// This runs **before** clap parsing to decide whether to filter unknown
/// arguments. Must stay in sync with `Cli::is_stdio_mode()` in `cmd/mod.rs`
/// which runs **after** parsing to decide dispatch. Both check for the same
/// triggers: `-p`/`--prompt` presence or `--input-format stream-json`.
pub fn is_stdio_mode(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-p" || arg == "--prompt" {
            return true;
        }
        if arg.starts_with("--prompt=") || arg.starts_with("-p=") {
            return true;
        }
        if arg == "--input-format" && i + 1 < args.len() && args[i + 1] == "stream-json" {
            return true;
        }
        if arg.starts_with("--input-format=stream-json") {
            return true;
        }
        i += 1;
    }
    false
}

/// Filter unknown arguments in stdio mode.
///
/// Strategy:
/// - **Known flags** (from clap): kept with correct value handling.
/// - **Unknown `--flag=value`**: dropped (single token).
/// - **Unknown `--flag value`**: heuristic — if the next token starts with
///   `-`, the flag is treated as boolean (dropped, next token preserved).
///   Otherwise the flag and its value are both dropped.
/// - **Positional / value arguments**: always kept.
///
/// This lets wing accept arbitrary SDK-injected flags (e.g. `--verbose`,
/// `--permission-mode bypassPermissions`) without maintaining a manual
/// whitelist of external flags.
pub fn filter_unknown_args(args: Vec<String>) -> Vec<String> {
    let known = KnownFlags::from_cli();
    let mut result = Vec::new();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];

        if arg.starts_with("--") {
            let flag_name = if let Some(eq_pos) = arg.find('=') {
                &arg[..eq_pos]
            } else {
                arg.as_str()
            };

            if known.long_with_value.contains(flag_name) {
                // Known value-carrying flag: keep flag + value.
                result.push(arg.clone());
                if !arg.contains('=') {
                    i += 1;
                    if i < args.len() {
                        result.push(args[i].clone());
                    }
                }
            } else if known.long_bool.contains(flag_name) {
                // Known boolean flag: keep flag only.
                result.push(arg.clone());
            } else if !arg.contains('=') {
                // Unknown --flag without =.
                // Heuristic: if next token starts with '-', this flag is
                // likely boolean (e.g. --verbose --system-prompt …).
                // Otherwise it likely takes a value (e.g. --permission-mode bypassPermissions).
                let next_is_flag = args.get(i + 1).is_some_and(|s| s.starts_with('-'));
                if !next_is_flag {
                    // Skip the value token.
                    i += 1;
                }
                // Drop the unknown flag itself.
            }
            // Unknown --flag=value (with =): drop this single token.
        } else if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let flag_char = arg.chars().nth(1).unwrap_or(' ');
            if known.short_with_value.contains(&flag_char) {
                result.push(arg.clone());
                if arg.len() == 2 {
                    i += 1;
                    if i < args.len() {
                        result.push(args[i].clone());
                    }
                }
            } else if known.short_bool.contains(&flag_char) {
                result.push(arg.clone());
            }
            // Unknown short flag: drop.
        } else {
            // Positional or value — keep.
            result.push(arg.clone());
        }

        i += 1;
    }

    result
}

// ============================================================
// Gateway auto-start (extracted from smart_default_tui)
// ============================================================

/// Ensure gateway is running, returning (host, port).
///
/// Checks health endpoint first; if gateway is not reachable, starts it.
pub async fn ensure_gateway_running() -> Result<(String, u16)> {
    let gw_config = backend_config::read_backend_gateway_config();

    // Check if gateway is already running via health check.
    let http_base = format!("http://{}:{}", gw_config.host, gw_config.port);
    let already_running = if let Ok(client) = wing_api_client::GatewayClient::new(&http_base, None)
    {
        matches!(
            client.health().await,
            Ok(h) if h.service == "wing-gateway"
        )
    } else {
        false
    };

    if !already_running {
        start::start_gateway(&gw_config.host, gw_config.port).await?;
    }

    Ok((gw_config.host, gw_config.port))
}

// ============================================================
// stdio entry point
// ============================================================

/// Run stdio mode: create/resume session, send prompt, output results.
pub async fn run_stdio(args: StdioArgs) -> ExitCode {
    match run_stdio_inner(args).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wing error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_stdio_inner(args: StdioArgs) -> Result<ExitCode> {
    let start_time = Instant::now();

    // 1. Ensure gateway is running.
    let (host, port) = ensure_gateway_running().await?;

    let ws_url = format!("ws://{host}:{port}/ws");
    let http_base = format!("http://{host}:{port}");

    tracing::info!("stdio mode: gateway at {ws_url}");

    // Load API key from TUI config.
    let api_key = crate::config::AppConfig::load()
        .api_key
        .filter(|k| !k.is_empty());
    let api_key_ref = api_key.as_deref();

    // 2. WS connect.
    let mut gateway = GatewayClient::connect(&ws_url, api_key_ref)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to connect to gateway at {ws_url}: {e}\n\
             Make sure the gateway is running: wing start"
            )
        })?;

    let client_id = gateway.client_id().to_string();
    tracing::info!(client_id = %client_id, "WS connected");

    // 3. HTTP client.
    let http = GatewayApiClient::new(http_base, api_key_ref)
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;

    // 4. Create or resume session.
    let session_id = if let Some(ref resume_id) = args.resume {
        let resp = http
            .resume_session(resume_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to resume session: {e}"))?;
        tracing::info!(session_id = %resp.session_id, "session resumed");
        resp.session_id
    } else {
        let workspace = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string());

        let override_ = AgentOverride {
            model: args.model.clone(),
            system_prompt: args.system_prompt.clone(),
            append_system_prompt: args.append_system_prompt.clone(),
            tools: Some(vec![
                "Read".into(),
                "Write".into(),
                "Edit".into(),
                "Bash".into(),
            ]),
            max_turns: args.max_turns,
            effort: args.effort.clone(),
            yolo: Some(true),
        };

        let create_req = CreateSessionRequest {
            template_name: None,
            workspace,
            agent: Some(override_),
        };

        let resp = http
            .create_session(&create_req)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create session: {e}"))?;
        tracing::info!(session_id = %resp.session_id, "session created");
        resp.session_id
    };

    // 5. HTTP subscribe.
    http.subscribe(&session_id, &client_id)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to subscribe to session: {e}"))?;

    tracing::info!("subscribed to session events");

    // 6. Build renderer.
    let mut renderer =
        StdioRenderer::new(args.output_format.clone(), start_time, session_id.clone());

    // 7. Resolve prompt: CLI arg > stdin (stream-json) > error.
    let prompt = if !args.prompt.is_empty() {
        args.prompt
    } else if args.input_format == InputFormat::StreamJson {
        stdin_handler::handle_stdin_stream().await?
    } else {
        anyhow::bail!("no prompt provided");
    };

    // 8. Send prompt.
    http.send_message(&session_id, &prompt, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send message: {e}"))?;

    tracing::info!("prompt sent, entering event loop");

    // 9. Event loop: receive events from WS, render, exit on TurnResult.
    loop {
        match gateway.recv_event().await {
            Some(event) => {
                let should_exit = renderer.handle_event(&event);
                if should_exit {
                    break;
                }
            }
            None => {
                tracing::warn!("WS connection closed before TurnResult");
                return Ok(ExitCode::FAILURE);
            }
        }
    }

    Ok(renderer.exit_code())
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_stdio_mode ----

    #[test]
    fn stdio_mode_triggered_by_short_prompt() {
        let args: Vec<String> = vec!["-p", "hello"].into_iter().map(String::from).collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_triggered_by_long_prompt() {
        let args: Vec<String> = vec!["--prompt", "hello"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_triggered_by_prompt_equals() {
        let args: Vec<String> = vec!["--prompt=hello"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_triggered_by_input_format_stream_json() {
        let args: Vec<String> = vec!["--input-format", "stream-json"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_triggered_by_input_format_equals_stream_json() {
        let args: Vec<String> = vec!["--input-format=stream-json"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_not_triggered_by_input_format_text() {
        let args: Vec<String> = vec!["--input-format", "text"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(!is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_not_triggered_by_empty_args() {
        let args: Vec<String> = vec![];
        assert!(!is_stdio_mode(&args));
    }

    #[test]
    fn stdio_mode_not_triggered_by_tui_subcommand() {
        let args: Vec<String> = vec!["tui"].into_iter().map(String::from).collect();
        assert!(!is_stdio_mode(&args));
    }

    // ---- filter_unknown_args ----

    #[test]
    fn filter_keeps_known_flags() {
        let args: Vec<String> = vec!["-p", "hello", "--output-format", "json"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "--output-format", "json"]);
    }

    #[test]
    fn filter_drops_unknown_value_flags() {
        let args: Vec<String> = vec![
            "-p",
            "hello",
            "--permission-mode",
            "bypassPermissions",
            "--verbose",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello"]);
    }

    #[test]
    fn filter_unknown_bool_before_known_flag() {
        // --verbose is unknown boolean; next token --system-prompt starts with '-'
        // so the heuristic treats --verbose as boolean and preserves --system-prompt.
        let args: Vec<String> = vec!["-p", "hello", "--verbose", "--system-prompt", ""]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "--system-prompt", ""]);
    }

    #[test]
    fn filter_multiple_unknown_bools_before_known_flag() {
        let args: Vec<String> = vec![
            "-p",
            "hello",
            "--verbose",
            "--include-partial-messages",
            "--max-turns",
            "5",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "--max-turns", "5"]);
    }

    #[test]
    fn filter_keeps_yolo_flag() {
        let args: Vec<String> = vec!["-p", "hello", "--yolo"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "--yolo"]);
    }

    #[test]
    fn filter_keeps_equals_style_flags() {
        let args: Vec<String> = vec!["--prompt=hello", "--output-format=json"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["--prompt=hello", "--output-format=json"]);
    }

    #[test]
    fn filter_drops_unknown_equals_style_flags() {
        let args: Vec<String> = vec!["-p", "hello", "--unknown-flag=value"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello"]);
    }

    #[test]
    fn filter_handles_model_flag() {
        let args: Vec<String> = vec!["-p", "hello", "-m", "gpt-4o"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "-m", "gpt-4o"]);
    }

    #[test]
    fn filter_handles_resume_flag() {
        let args: Vec<String> = vec!["-p", "hello", "-r", "abc123"]
            .into_iter()
            .map(String::from)
            .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(filtered, vec!["-p", "hello", "-r", "abc123"]);
    }

    #[test]
    fn filter_sdk_command_line() {
        // Full command line as constructed by claude-agent-sdk-python's
        // SubprocessCLITransport._build_command().
        let args: Vec<String> = vec![
            "--output-format",
            "stream-json",
            "--verbose",
            "--system-prompt",
            "",
            "--max-turns",
            "5",
            "--permission-mode",
            "bypassPermissions",
            "--input-format",
            "stream-json",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(
            filtered,
            vec![
                "--output-format",
                "stream-json",
                "--system-prompt",
                "",
                "--max-turns",
                "5",
                "--input-format",
                "stream-json",
            ]
        );
    }

    #[test]
    fn filter_complex_mixed() {
        let args: Vec<String> = vec![
            "-p",
            "list files",
            "--output-format",
            "stream-json",
            "--permission-mode",
            "bypassPermissions",
            "--max-turns",
            "5",
            "--yolo",
            "--some-other-flag",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let filtered = filter_unknown_args(args);
        assert_eq!(
            filtered,
            vec![
                "-p",
                "list files",
                "--output-format",
                "stream-json",
                "--max-turns",
                "5",
                "--yolo",
            ]
        );
    }
}
