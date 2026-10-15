//! Shared utilities for CLI subcommands.
//!
//! Provides gateway discovery, HTTP client construction, and output
//! formatting helpers used by `wing run`, `wing ps`, `wing tail`, etc.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::IsTerminal;

use anyhow::Result;
use wing_api_client::ApiClientError;
use wing_api_client::GatewayClient as GatewayApiClient;

/// Output environment for human-facing tables.
///
/// `width`: a TTY adapts to the terminal; a pipe gets [`PIPE_WIDTH`] —
/// deterministic output for `grep` / `less` / CI (terminal queries through
/// `/dev/tty` must not leak the real width into a redirect).
///
/// `color` is off unless stdout is a TTY, `NO_COLOR` is unset-or-empty (the
/// no-color.org semantics: "present and not an empty string") and the
/// terminal does not declare itself dumb.
pub struct TableOutput {
    pub width: usize,
    pub color: bool,
}

/// Table width for non-TTY output: the width the hand-laid tables used to be
/// cut for, kept so pipes stay deterministic.
pub const PIPE_WIDTH: usize = 120;

impl TableOutput {
    pub fn detect() -> Self {
        let stdout_tty = std::io::stdout().is_terminal();
        Self {
            width: Self::width_for(stdout_tty),
            color: Self::color_for(
                stdout_tty,
                std::env::var_os("NO_COLOR").as_deref(),
                std::env::var_os("TERM").as_deref(),
            ),
        }
    }

    /// Width policy: TTY → the terminal's width (120 when the query fails);
    /// pipe → [`PIPE_WIDTH`].
    fn width_for(stdout_tty: bool) -> usize {
        if stdout_tty {
            crossterm::terminal::size().map_or(PIPE_WIDTH, |(w, _)| w as usize)
        } else {
            PIPE_WIDTH
        }
    }

    /// Colour policy (see the struct docs). Pure, so the rule is testable
    /// without a terminal.
    fn color_for(
        stdout_tty: bool,
        no_color: Option<&std::ffi::OsStr>,
        term: Option<&std::ffi::OsStr>,
    ) -> bool {
        stdout_tty
            && no_color.is_none_or(|v| v.is_empty())
            && term.is_none_or(|t| t.to_string_lossy() != "dumb")
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

/// session id 的字节上限——与后端闸门 `common.utils.SESSION_ID_MAX_BYTES` 同值。
const SESSION_ID_MAX_BYTES: usize = 128;

/// The lineage parent for a session being created right now, read from the
/// environment: `WING_SESSION_ID` is injected into every agent shell by the
/// Bash tool, so a CLI invocation that runs inside a session can record *who
/// created it* (`docs/dev/session-lineage.md` §1/§3).
///
/// The env is a **soft signal**: absent (`None`) means "the user's own shell"
/// → a root session; a malformed value is dropped with a warning and never
/// blocks creation. The hygiene gate mirrors the backend's
/// `is_valid_session_id` (a bad value would be a 400 there anyway) — this
/// side just fails earlier and leaves a trace.
pub fn parent_session_id_from_env() -> Option<String> {
    match std::env::var_os("WING_SESSION_ID") {
        Some(raw) => match raw.into_string() {
            Ok(value) => parent_session_id(Some(value)),
            Err(raw) => {
                tracing::warn!(
                    value = ?raw,
                    "WING_SESSION_ID is not valid UTF-8; ignoring it (lineage is a soft signal)"
                );
                None
            }
        },
        None => parent_session_id(None),
    }
}

/// [`parent_session_id_from_env`]'s pure core (the env is process-global and
/// therefore untestable): `None` = variable absent, `Some` = its value.
///
/// Hygiene rules (same gate as the backend): non-empty, ≤ 128 **bytes**,
/// no leading `.`, no path separators or `..`, no ASCII control characters.
/// (`String` is UTF-8 by construction, so the encodability rule coincides.)
fn parent_session_id(raw: Option<String>) -> Option<String> {
    let value = raw?;
    if !value.is_empty()
        && value.len() <= SESSION_ID_MAX_BYTES
        && !value.starts_with('.')
        && !value.contains(['/', '\\'])
        && !value.contains("..")
        && !value.bytes().any(|b| b < 0x20 || b == 0x7f)
    {
        return Some(value);
    }
    // `?value` (Debug) escapes control characters: a malformed value must not
    // be able to inject newlines into the log.
    tracing::warn!(
        value = ?value,
        "WING_SESSION_ID is not a well-formed session id; ignoring it (lineage is a soft signal)"
    );
    None
}

/// Log-only reconciliation for the create read-back: the caller requested a
/// parent (the env had one) but the gateway did not record it — an older
/// gateway silently ignores the field. Soft mechanism: the session was still
/// created, so this is a warning, never an error.
pub fn warn_if_parent_dropped(requested: Option<&str>, read_back: Option<&str>) {
    if requested.is_some() && read_back.is_none() {
        tracing::warn!(
            parent = ?requested,
            "gateway did not record the session's parent (does the running gateway predate \
             session lineage? restart it: wing stop && wing start)"
        );
    }
}

/// Verify the requested tags actually landed on `session_id`.
///
/// Guards against a gateway that predates session tags: the create endpoint
/// there silently ignores the `tags` field (pydantic `extra="ignore"`), which
/// would recreate the "dispatched but untagged" window this feature exists to
/// close. The read (no ops) doubles as the compatibility probe — an older
/// gateway has no `/api/session/tag` route at all and fails loudly here,
/// before the prompt is sent.
///
/// Returns the session's **resulting** tags (existing + newly added), which
/// callers report as the outcome.
pub async fn ensure_tags_applied(
    http: &GatewayApiClient,
    session_id: &str,
    tags: &[String],
) -> Result<Vec<String>> {
    if tags.is_empty() {
        return Ok(Vec::new());
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
    Ok(resp.tags)
}

/// Run a session-scoped call with the CLI's **404 → resume → retry** convention.
///
/// A 404 from a session endpoint means "not loaded **right now**" (evicted, or
/// never hydrated since the gateway started) — not "does not exist": eviction
/// only reclaims memory, the disk state is kept. `wing tail` / `head` / `info`
/// already hydrate on 404, and the control-plane commands follow the same
/// rule so an operator never has to know whether a session was evicted before
/// running `wing branches|fork|rewind|compact|update`.
///
/// The retry happens **once**: if the resumed call 404s again, or the resume
/// itself 404s (the session is not on disk either), that error is the honest
/// answer. The returned error is already the user-facing one
/// ([`session_error`]), so callers just `?` it.
///
/// `wing tail` / `head` predate this helper and keep their own inline version
/// (`cmd::messages::fetch_messages`, which sniffs "404" / "not found" out of
/// the error text); folding them in would change an existing command's
/// behavior, so that is left as a follow-up.
pub async fn hydrate_on_404<T, F, Fut>(
    http: &GatewayApiClient,
    session_id: &str,
    call: F,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, ApiClientError>>,
{
    match call().await {
        Ok(value) => Ok(value),
        Err(first) if first.is_not_found() => {
            // 404 = 不在内存 ⇒ 水合一次；磁盘上也没有才算真的不存在。
            http.resume_session(session_id)
                .await
                .map_err(|e| session_error(session_id, &e))?;
            // 重试：会话此时已知在内存，再来 404 是**另一个 404**（fork / rewind
            // 的 uuid 未命中就是这一种）——网关的 detail 才是答案，原样带出，
            // 不再改写成"会话不存在"。
            call().await.map_err(|e| session_failed(session_id, &e))
        }
        Err(e) => Err(session_error(session_id, &e)),
    }
}

/// `session <id>: <gateway error>` — status code and detail verbatim.
pub fn session_failed(session_id: &str, error: &ApiClientError) -> anyhow::Error {
    anyhow::anyhow!("session {session_id}: {error}")
}

/// One friendly line for a session-scoped API failure.
///
/// 404 gets the "evicted ≠ missing" wording (with the way out: `wing resume`
/// or `wing ps --all`); 409 passes through the gateway's own reason (busy /
/// subscribed / not durable) — the status code and the reason are the useful
/// part, so both stay in the message. Everything else keeps the transport
/// error verbatim (never re-classify a network failure as "not found").
pub fn session_error(session_id: &str, error: &ApiClientError) -> anyhow::Error {
    if error.is_not_found() {
        anyhow::anyhow!(
            "session {session_id} not found (not loaded and not on disk); \
             `wing ps --all` lists every known session"
        )
    } else {
        session_failed(session_id, error)
    }
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

    /// 合法 env 值原样带上；缺席（`None`）静默——两类合法形态。
    /// 卫生判据与后端闸门同口径：只防穿越与卫生，不限定 id 形态。
    #[test]
    fn a_well_formed_parent_passes_through_verbatim() {
        for ok in [
            "20261015-211837-87e3fac2",
            "team-A_1.2", // 内部点、任意 Unicode 都放行（只防穿越与卫生）
            "会话-一",
            &"x".repeat(SESSION_ID_MAX_BYTES), // 上限本身是合法值（字节）
        ] {
            assert_eq!(
                parent_session_id(Some(ok.to_string())).as_deref(),
                Some(ok),
                "{ok:?} should pass"
            );
        }
        assert_eq!(parent_session_id(None), None, "absent is silent");
    }

    #[test]
    fn malformed_parent_values_are_dropped() {
        let too_long = "x".repeat(SESSION_ID_MAX_BYTES + 1);
        for bad in [
            "",        // 空串
            ".hidden", // 点开头（存储的点命名空间）
            "a/b",     // 路径分隔符
            "a\\b",    // Windows 分隔符也防（id 是单层路径组件）
            "a..b",    // 父目录引用
            "..",
            "line\nbreak", // 控制字符（也防日志注入）
            "tab\there",
            "del\u{7f}",
            too_long.as_str(),
        ] {
            assert_eq!(
                parent_session_id(Some(bad.to_string())),
                None,
                "{bad:?} must be dropped"
            );
        }
    }

    #[test]
    fn single_line_flattens_control_whitespace() {
        // 硬换行 / tab / CR 都是行结构，进表格前必须压成空格。
        assert_eq!(single_line("first\nsecond"), "first second");
        assert_eq!(single_line("a\tb\rc"), "a b c");
        // 普通文本原样（不折叠已有空格）。
        assert_eq!(single_line("keep  the  spaces"), "keep  the  spaces");
    }

    #[test]
    fn table_output_policy_is_testable_without_a_terminal() {
        use std::ffi::OsStr;
        // 管道：固定宽度（确定性）、永不着色。
        assert_eq!(TableOutput::width_for(false), PIPE_WIDTH);
        assert!(!TableOutput::color_for(false, None, None));
        // TTY：默认着色；NO_COLOR 非空即退出（空串不算 opt-out，no-color.org 口径）。
        assert!(TableOutput::color_for(true, None, None));
        assert!(!TableOutput::color_for(true, Some(OsStr::new("1")), None));
        assert!(TableOutput::color_for(true, Some(OsStr::new("")), None));
        // TERM=dumb 同样退出。
        assert!(!TableOutput::color_for(
            true,
            None,
            Some(OsStr::new("dumb"))
        ));
        assert!(TableOutput::color_for(
            true,
            None,
            Some(OsStr::new("xterm-256color"))
        ));
    }
}
