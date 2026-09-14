//! `wing tail` / `wing head` — message filtering (like Unix head/tail).
//!
//! Fetches session messages via `GET /api/session/get` and filters by type.
//! `tail` shows the last N, `head` shows the first N.
//!
//! Decoding lives in the shared typed mirror (`protocol::SessionMessage`);
//! filtering and text printing run on the typed view, while `--json` emits the
//! raw payloads verbatim (typed serialization would drop unknown fields and
//! change key order).
//!
//! # Filter types
//!
//! Role filters select messages and print all their sections; field filters
//! additionally restrict printing to that section only (e.g. `content`
//! prints text without leaking reasoning):
//!
//! | `--type`       | Condition                                  | Printed sections |
//! |----------------|--------------------------------------------|------------------|
//! | `all` (default)| All messages                               | everything       |
//! | `user`         | `role == "user"`                           | everything       |
//! | `assistant`    | `role == "assistant"`                      | everything       |
//! | `tool_call`    | Assistant messages with `tool_calls`       | tool calls only  |
//! | `tool_result`  | `role == "tool"`                           | tool result only |
//! | `reasoning`    | Messages with non-empty `reasoning_content`| reasoning only   |
//! | `content`      | Assistant with text content, no tool_calls | text only        |

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use serde_json::Value;

use crate::protocol::SessionMessage;

use super::common;

/// Entry point for `wing tail`.
pub async fn run_tail(session_id: &str, n: usize, filter: &str, json: bool) -> ExitCode {
    run_messages(session_id, n, filter, json, /* from_head = */ false).await
}

/// Entry point for `wing head`.
pub async fn run_head(session_id: &str, n: usize, filter: &str, json: bool) -> ExitCode {
    run_messages(session_id, n, filter, json, /* from_head = */ true).await
}

/// One fetched history row: the raw payload — printed verbatim by `--json` —
/// plus its typed view.
///
/// `view` is `None` when the payload is not a Message projection (malformed /
/// non-object). Such rows only match `all` (or unknown) filters and print with
/// the `[unknown]` header, mirroring the old field-sniffing behavior.
struct HistoryRow {
    raw: Value,
    view: Option<SessionMessage>,
}

impl HistoryRow {
    fn new(raw: Value) -> Self {
        let view = match SessionMessage::from_json(&raw) {
            Ok(view) => Some(view),
            Err(e) => {
                tracing::debug!(error = %e, "history payload is not a Message projection");
                None
            }
        };
        Self { raw, view }
    }
}

async fn run_messages(
    session_id: &str,
    n: usize,
    filter: &str,
    json: bool,
    from_head: bool,
) -> ExitCode {
    match fetch_messages(session_id).await {
        Ok(messages) => {
            let rows: Vec<HistoryRow> = messages.into_iter().map(HistoryRow::new).collect();
            let filtered = filter_messages(&rows, filter);
            let selected = if from_head {
                filtered.into_iter().take(n).collect::<Vec<_>>()
            } else {
                let start = filtered.len().saturating_sub(n);
                filtered.into_iter().skip(start).collect::<Vec<_>>()
            };

            if json {
                let raw: Vec<&Value> = selected.iter().map(|row| &row.raw).collect();
                common::print_json_compact(&raw);
            } else {
                print_messages(&selected, filter);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let cmd = if from_head { "head" } else { "tail" };
            eprintln!("wing {cmd} error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn fetch_messages(session_id: &str) -> Result<Vec<Value>> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;

    // Try GET /api/session/get; if 404, resume the session first.
    match http.get_session(session_id).await {
        Ok(resp) => Ok(resp.messages),
        Err(e) => {
            // Check if it's a 404 (session not in memory).
            let err_str = e.to_string();
            if err_str.contains("404") || err_str.contains("not found") {
                // Try to resume the session from disk.
                http.resume_session(session_id).await?;
                let resp = http.get_session(session_id).await?;
                Ok(resp.messages)
            } else {
                Err(anyhow::anyhow!("{e}"))
            }
        }
    }
}

/// Filter rows by type.
///
/// A row whose payload does not decode matches no named filter (the old
/// sniffing had no field to match either) — only `all` / unknown values.
fn filter_messages<'a>(rows: &'a [HistoryRow], filter: &str) -> Vec<&'a HistoryRow> {
    rows.iter()
        .filter(|row| matches_filter(row, filter))
        .collect()
}

fn matches_filter(row: &HistoryRow, filter: &str) -> bool {
    let Some(msg) = &row.view else {
        return !matches!(
            filter,
            "user" | "assistant" | "tool_call" | "tool_result" | "reasoning" | "content"
        );
    };
    match filter {
        "user" => msg.role == "user",
        "assistant" => msg.role == "assistant",
        // `tool_calls` must be a non-empty array — absent / null / `[]` are
        // not a tool call.
        "tool_call" => msg.role == "assistant" && msg.has_tool_calls(),
        "tool_result" => msg.role == "tool",
        "reasoning" => msg
            .reasoning_content
            .as_deref()
            .is_some_and(|r| !r.is_empty()),
        "content" => msg.role == "assistant" && !msg.content.is_empty() && !msg.has_tool_calls(),
        _ => true, // "all" or unknown
    }
}

/// Which sections of a message `print_messages` renders.
///
/// `--type` has two kinds of filters:
/// - **role filters** (`all`, `user`, `assistant`) select *messages* and print
///   every section of them;
/// - **field filters** (`reasoning`, `content`, `tool_call`, `tool_result`)
///   select messages *and* restrict printing to that section only — filtering
///   `content` must not leak reasoning, which is what this enum enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionFilter {
    All,
    Reasoning,
    Content,
    ToolCall,
    ToolResult,
}

impl SectionFilter {
    fn from_filter(filter: &str) -> Self {
        match filter {
            "reasoning" => Self::Reasoning,
            "content" => Self::Content,
            "tool_call" => Self::ToolCall,
            "tool_result" => Self::ToolResult,
            _ => Self::All,
        }
    }
}

/// Body lines of one message under a section filter (header/separator excluded).
fn message_body_lines(msg: &SessionMessage, section: SectionFilter) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    let reasoning = msg.reasoning_content.as_deref().unwrap_or("");
    if !reasoning.is_empty() && matches!(section, SectionFilter::All | SectionFilter::Reasoning) {
        lines.push(String::new());
        lines.push(reasoning.to_string());
    }

    if !msg.content.is_empty() && matches!(section, SectionFilter::All | SectionFilter::Content) {
        lines.push(String::new());
        lines.push(msg.content.clone());
    }

    if matches!(section, SectionFilter::All | SectionFilter::ToolCall) {
        for tc in msg.tool_calls() {
            // Absent `arguments` prints as `()`, explicit null as `(null)` —
            // the distinction the typed mirror keeps on purpose.
            let args = tc
                .arguments
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();
            lines.push(String::new());
            lines.push(format!("  → {}({args})", tc.name));
        }
    }

    if msg.role == "tool" && matches!(section, SectionFilter::All | SectionFilter::ToolResult) {
        let tool_call_id = msg.tool_call_id.as_deref().unwrap_or("");
        lines.push(String::new());
        lines.push(format!("  ← {tool_call_id}"));
        // Truncate long tool results.
        let display = common::truncate_chars(&msg.content, 500);
        lines.push(format!("  {display}"));
    }

    lines
}

/// Header line for one row: `[role] uuid`.
///
/// An empty/absent role and an undecodable payload both print `[unknown]`
/// (the old sniffing did the same).
fn header_line(row: &HistoryRow) -> String {
    let (role, uuid) = match &row.view {
        Some(msg) => (msg.role.as_str(), msg.uuid.as_deref().unwrap_or("")),
        None => ("unknown", ""),
    };
    let role = if role.is_empty() { "unknown" } else { role };
    format!("[{role}] {uuid}")
}

fn print_messages(rows: &[&HistoryRow], filter: &str) {
    if rows.is_empty() {
        println!("No messages matching filter '{filter}'.");
        return;
    }

    let section = SectionFilter::from_filter(filter);
    for row in rows {
        println!("─────────────────────────────────────────────");
        println!("{}", header_line(row));
        if let Some(msg) = &row.view {
            for line in message_body_lines(msg, section) {
                println!("{line}");
            }
        }
    }
    println!("─────────────────────────────────────────────");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Assistant message carrying reasoning + text + one tool call — the
    /// kitchen-sink case every section filter must slice correctly.
    fn assistant_msg() -> Value {
        json!({
            "role": "assistant",
            "uuid": "u1",
            "reasoning_content": "thinking hard",
            "content": "the answer",
            "tool_calls": [
                {"id": "tc_bash", "name": "bash", "arguments": {"cmd": "ls"}}
            ],
        })
    }

    fn tool_result_msg() -> Value {
        json!({
            "role": "tool",
            "uuid": "u2",
            "tool_call_id": "tc1",
            "content": "file-a\nfile-b",
        })
    }

    fn rows(values: Vec<Value>) -> Vec<HistoryRow> {
        values.into_iter().map(HistoryRow::new).collect()
    }

    fn decoded(value: Value) -> SessionMessage {
        SessionMessage::from_json(&value).expect("test payload must be a Message projection")
    }

    // ── filter_messages ──────────────────────────────

    #[test]
    fn filter_by_role() {
        let msgs = rows(vec![
            assistant_msg(),
            json!({"role": "user", "content": "hi"}),
        ]);
        assert_eq!(filter_messages(&msgs, "user").len(), 1);
        assert_eq!(filter_messages(&msgs, "assistant").len(), 1);
        assert_eq!(filter_messages(&msgs, "all").len(), 2);
    }

    #[test]
    fn filter_reasoning_requires_non_empty() {
        let empty = json!({"role": "assistant", "reasoning_content": ""});
        let msgs = rows(vec![assistant_msg(), empty]);
        // Empty-string reasoning is not a reasoning message.
        assert_eq!(filter_messages(&msgs, "reasoning").len(), 1);
    }

    #[test]
    fn filter_content_excludes_tool_call_messages() {
        let msgs = rows(vec![
            assistant_msg(),
            json!({"role": "assistant", "content": "plain"}),
        ]);
        // The kitchen-sink message has tool_calls → excluded from "content".
        assert_eq!(filter_messages(&msgs, "content").len(), 1);
    }

    #[test]
    fn filter_tool_result_selects_tool_role() {
        let msgs = rows(vec![assistant_msg(), tool_result_msg()]);
        assert_eq!(filter_messages(&msgs, "tool_result").len(), 1);
        assert_eq!(filter_messages(&msgs, "tool_call").len(), 1);
    }

    #[test]
    fn filter_tool_call_requires_non_empty_array() {
        // Absent / null / [] are all "no tool call" — such messages fall to
        // the "content" filter instead.
        let empty = json!({"role": "assistant", "content": "x", "tool_calls": []});
        let null = json!({"role": "assistant", "content": "x", "tool_calls": null});
        let absent = json!({"role": "assistant", "content": "x"});
        let msgs = rows(vec![empty, null, absent, assistant_msg()]);
        assert_eq!(filter_messages(&msgs, "tool_call").len(), 1);
        assert_eq!(filter_messages(&msgs, "content").len(), 3);
    }

    #[test]
    fn undecodable_row_only_matches_all() {
        let msgs = rows(vec![
            json!({"invalid": "message"}),
            json!({"role": "user", "content": "hi"}),
            assistant_msg(),
            tool_result_msg(),
        ]);
        // Named filters keep selecting their real rows, never the undecodable
        // one (the old sniffing had no role to match either).
        assert_eq!(filter_messages(&msgs, "user").len(), 1);
        assert_eq!(filter_messages(&msgs, "assistant").len(), 1);
        assert_eq!(filter_messages(&msgs, "tool_call").len(), 1);
        assert_eq!(filter_messages(&msgs, "tool_result").len(), 1);
        assert_eq!(filter_messages(&msgs, "reasoning").len(), 1);
        assert_eq!(filter_messages(&msgs, "content").len(), 0);
        // `all` (and unknown filters) include it.
        assert_eq!(filter_messages(&msgs, "all").len(), 4);
        assert_eq!(filter_messages(&msgs, "bogus").len(), 4);
    }

    // ── message_body_lines: field filters slice sections ──

    #[test]
    fn body_lines_content_does_not_leak_reasoning() {
        // Regression: `--type content` used to print reasoning too, because
        // the filter only selected messages while print_messages rendered
        // every section.
        let lines = message_body_lines(&decoded(assistant_msg()), SectionFilter::Content);
        let joined = lines.join("\n");
        assert!(joined.contains("the answer"));
        assert!(!joined.contains("thinking hard"));
        assert!(!joined.contains("bash"));
    }

    #[test]
    fn body_lines_reasoning_only() {
        let lines = message_body_lines(&decoded(assistant_msg()), SectionFilter::Reasoning);
        let joined = lines.join("\n");
        assert!(joined.contains("thinking hard"));
        assert!(!joined.contains("the answer"));
        assert!(!joined.contains("bash"));
    }

    #[test]
    fn body_lines_tool_call_only() {
        let lines = message_body_lines(&decoded(assistant_msg()), SectionFilter::ToolCall);
        let joined = lines.join("\n");
        assert!(joined.contains("→ bash"));
        assert!(!joined.contains("thinking hard"));
        assert!(!joined.contains("the answer"));
    }

    #[test]
    fn body_lines_tool_result_only() {
        let lines = message_body_lines(&decoded(tool_result_msg()), SectionFilter::ToolResult);
        let joined = lines.join("\n");
        assert!(joined.contains("← tc1"));
        assert!(joined.contains("file-a"));
    }

    #[test]
    fn body_lines_all_renders_every_section() {
        let lines = message_body_lines(&decoded(assistant_msg()), SectionFilter::All);
        let joined = lines.join("\n");
        assert!(joined.contains("thinking hard"));
        assert!(joined.contains("the answer"));
        assert!(joined.contains("→ bash"));
    }

    #[test]
    fn body_lines_tool_args_absent_vs_null() {
        // Byte-level behavior kept from the sniffing era: absent `arguments`
        // prints `()`, explicit null prints `(null)`.
        let absent = decoded(json!({
            "role": "assistant",
            "tool_calls": [{"id": "a", "name": "Bash"}]
        }));
        let null = decoded(json!({
            "role": "assistant",
            "tool_calls": [{"id": "a", "name": "Bash", "arguments": null}]
        }));
        let lines =
            |msg: &SessionMessage| message_body_lines(msg, SectionFilter::ToolCall).join("\n");
        assert!(lines(&absent).contains("→ Bash()"), "{}", lines(&absent));
        assert!(lines(&null).contains("→ Bash(null)"), "{}", lines(&null));
    }

    #[test]
    fn missing_fields_default() {
        // Only a role: no content / reasoning / tool_calls — decodes cleanly,
        // renders no body.
        let msg = decoded(json!({"role": "assistant"}));
        assert_eq!(msg.content, "");
        assert!(!msg.has_tool_calls());
        assert!(message_body_lines(&msg, SectionFilter::All).is_empty());
    }

    // ── header + raw payload stability ──────────────

    #[test]
    fn header_line_fallbacks() {
        // Missing role (but decodable) → [unknown], like the sniffing era.
        let no_role = rows(vec![json!({"content": "x"})]);
        assert_eq!(header_line(&no_role[0]), "[unknown] ");

        // Undecodable payload → [unknown] header as well.
        let undecodable = rows(vec![json!({"invalid": "message"})]);
        assert_eq!(header_line(&undecodable[0]), "[unknown] ");

        // Normal case keeps role + uuid.
        let normal = rows(vec![assistant_msg()]);
        assert_eq!(header_line(&normal[0]), "[assistant] u1");
        // uuid absent → empty (no "null").
        let no_uuid = rows(vec![json!({"role": "user", "content": "x"})]);
        assert_eq!(header_line(&no_uuid[0]), "[user] ");
    }

    #[test]
    fn raw_payload_is_kept_verbatim_for_json() {
        // `--json` prints the raw payloads, so unknown fields survive and the
        // serialized array matches the fetched one byte for byte.
        let fetched = vec![
            json!({"role": "assistant", "content": "x", "usage": {"in": 1}}),
            json!({"role": "user", "content": "y"}),
        ];
        let fetched_json = serde_json::to_string(&fetched).unwrap();

        let msgs = rows(fetched);
        let selected = filter_messages(&msgs, "all");
        let raw: Vec<&Value> = selected.iter().map(|row| &row.raw).collect();
        assert_eq!(serde_json::to_string(&raw).unwrap(), fetched_json);
        assert!(serde_json::to_string(&raw).unwrap().contains("\"usage\""));
    }

    #[test]
    fn section_filter_from_filter_mapping() {
        assert_eq!(
            SectionFilter::from_filter("reasoning"),
            SectionFilter::Reasoning
        );
        assert_eq!(
            SectionFilter::from_filter("content"),
            SectionFilter::Content
        );
        assert_eq!(
            SectionFilter::from_filter("tool_call"),
            SectionFilter::ToolCall
        );
        assert_eq!(
            SectionFilter::from_filter("tool_result"),
            SectionFilter::ToolResult
        );
        // Role filters (and unknown values) keep full-message rendering.
        assert_eq!(SectionFilter::from_filter("all"), SectionFilter::All);
        assert_eq!(SectionFilter::from_filter("user"), SectionFilter::All);
        assert_eq!(SectionFilter::from_filter("assistant"), SectionFilter::All);
        assert_eq!(SectionFilter::from_filter("bogus"), SectionFilter::All);
    }
}
