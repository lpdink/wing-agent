//! Shared utilities for CLI subcommands.
//!
//! Provides gateway discovery, HTTP client construction, and output
//! formatting helpers used by `wing run`, `wing ps`, `wing tail`, etc.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::IsTerminal;

use anyhow::Result;
use wing_api_client::GatewayClient as GatewayApiClient;

/// Output environment for human-facing tables.
///
/// `width` is the terminal width when stdout is a TTY, 120 otherwise (the
/// width the hand-laid tables used to be cut for, kept for pipes so output
/// stays deterministic); `color` is off when stdout is not a TTY, when
/// `NO_COLOR` is set (the standard opt-out) or when the terminal declares
/// itself dumb.
pub struct TableOutput {
    pub width: usize,
    pub color: bool,
}

impl TableOutput {
    pub fn detect() -> Self {
        let dumb = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
        Self {
            width: crossterm::terminal::size().map_or(120, |(w, _)| w as usize),
            color: std::io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && !dumb,
        }
    }
}

/// Flatten a display string to a single line: control whitespace (hard
/// newlines included) becomes a space.
///
/// Free text (session names, tool descriptions) may carry newlines; a raw one
/// breaks a tabular row in two.
pub fn single_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Render a plain table through the shared engine with the standard CLI inks
/// (dim frame, bold header), sized to `out`. Trailing newline included.
pub fn render_table(
    table: &crate::render::table::plain::PlainTable,
    palette: &crate::config::ThemePalette,
    out: &TableOutput,
) -> String {
    use ratatui::style::Style;

    let opts = crate::render::table::plain::PlainOpts {
        width: out.width,
        color: out.color,
        frame: Style::new().fg(palette.dim),
        header: Style::new().fg(palette.text).bold(),
    };
    let mut rendered = crate::render::table::plain::render(table, &opts).join("\n");
    rendered.push('\n');
    rendered
}

/// Ensure the gateway is running, returning `(host, port)`.
///
/// Checks the health endpoint first; if the gateway is not reachable,
/// starts it automatically. Delegates to `stdio::ensure_gateway_running()`
/// which already implements the full discovery + auto-start logic.
pub async fn ensure_gateway() -> Result<(String, u16)> {
    crate::stdio::ensure_gateway_running().await
}

/// Load the API key from TUI config (if any).
pub fn load_api_key() -> Option<String> {
    crate::config::AppConfig::load()
        .api_key
        .filter(|k| !k.is_empty())
}

/// Create a `GatewayApiClient` connected to the running gateway.
///
/// Loads the API key from TUI config and constructs the HTTP client.
pub fn create_api_client(host: &str, port: u16) -> Result<GatewayApiClient> {
    let api_key = load_api_key();
    let http_base = format!("http://{host}:{port}");
    GatewayApiClient::new(&http_base, api_key.as_deref())
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))
}

/// Print a value as JSON to stdout.
pub fn print_json<T: serde::Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize JSON: {e}"),
    }
}

/// Print a value as compact JSON to stdout (single line, for agent `jq` piping).
pub fn print_json_compact<T: serde::Serialize + ?Sized>(value: &T) {
    match serde_json::to_string(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("error: failed to serialize JSON: {e}"),
    }
}

/// Truncate a string to at most `max` characters (Unicode-safe).
/// Appends "..." if truncated. Avoids splitting multi-byte characters.
pub fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    // Not enough room for ellipsis — just take what fits.
    if max <= 3 {
        return chars.into_iter().take(max).collect();
    }
    let keep = max - 3;
    let truncated: String = chars.into_iter().take(keep).collect();
    format!("{truncated}...")
}

/// Verify the requested tags actually landed on `session_id`.
///
/// Guards against a gateway that predates session tags: the create endpoint
/// there silently ignores the `tags` field (pydantic `extra="ignore"`), which
/// would recreate the "dispatched but untagged" window this feature exists to
/// close. The read (no ops) doubles as the compatibility probe — an older
/// gateway has no `/api/session/tag` route at all and fails loudly here,
/// before the prompt is sent.
pub async fn ensure_tags_applied(
    http: &GatewayApiClient,
    session_id: &str,
    tags: &[String],
) -> Result<()> {
    if tags.is_empty() {
        return Ok(());
    }
    let resp = http
        .tag_session(session_id, None, None)
        .await
        .map_err(|e| {
            // 404/405 = 运行中的网关早于 tags 能力（重启即可）；其余保持
            // 传输错误的原文——不要把网络抖动归因成"旧网关"。
            if e.is_not_found() {
                anyhow::anyhow!(
                    "failed to verify --tag on session {session_id}: {e} \
                     (does the running gateway predate session tags? restart it: wing stop && wing start)"
                )
            } else {
                anyhow::anyhow!("failed to verify --tag on session {session_id}: {e}")
            }
        })?;
    let missing = missing_tags(tags, &resp.tags);
    if !missing.is_empty() {
        anyhow::bail!(
            "gateway did not apply tags {missing:?} to session {session_id} \
             (older gateways ignore the create `tags` field; restart it: wing stop && wing start)"
        );
    }
    Ok(())
}

/// Requested tags not present in `actual` (pure; unit-tested).
fn missing_tags<'a>(requested: &'a [String], actual: &[String]) -> Vec<&'a str> {
    requested
        .iter()
        .filter(|tag| !actual.iter().any(|applied| applied == *tag))
        .map(String::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tags_reports_only_absent() {
        let requested = vec!["executor".to_string(), "task=x".to_string()];
        let actual = vec!["task=x".to_string(), "favorite".to_string()];
        assert_eq!(missing_tags(&requested, &actual), vec!["executor"]);
        assert!(missing_tags(&requested, &requested).is_empty());
        assert_eq!(missing_tags(&requested, &[]).len(), 2);
    }

    #[test]
    fn single_line_flattens_control_whitespace() {
        // 硬换行 / tab / CR 都是行结构，进表格前必须压成空格。
        assert_eq!(single_line("first\nsecond"), "first second");
        assert_eq!(single_line("a\tb\rc"), "a b c");
        // 普通文本原样（不折叠已有空格）。
        assert_eq!(single_line("keep  the  spaces"), "keep  the  spaces");
    }
}
