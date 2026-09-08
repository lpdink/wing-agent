//! `wing tail` / `wing head` — message filtering (like Unix head/tail).
//!
//! Fetches session messages via `GET /api/session/get` and filters by type.
//! `tail` shows the last N, `head` shows the first N.
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

use super::common;

/// Entry point for `wing tail`.
pub async fn run_tail(session_id: &str, n: usize, filter: &str, json: bool) -> ExitCode {
    run_messages(session_id, n, filter, json, /* from_head = */ false).await
}

/// Entry point for `wing head`.
pub async fn run_head(session_id: &str, n: usize, filter: &str, json: bool) -> ExitCode {
    run_messages(session_id, n, filter, json, /* from_head = */ true).await
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
            let filtered = filter_messages(&messages, filter);
            let selected = if from_head {
                filtered.into_iter().take(n).collect::<Vec<_>>()
            } else {
                let start = filtered.len().saturating_sub(n);
                filtered.into_iter().skip(start).collect::<Vec<_>>()
            };

            if json {
                common::print_json_compact(&selected);
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

/// Filter messages by type.
fn filter_messages<'a>(messages: &'a [Value], filter: &str) -> Vec<&'a Value> {
    match filter {
        "user" => messages.iter().filter(|m| role_is(m, "user")).collect(),
        "assistant" => messages
            .iter()
            .filter(|m| role_is(m, "assistant"))
            .collect(),
        "tool_call" => messages
            .iter()
            .filter(|m| role_is(m, "assistant") && has_tool_calls(m))
            .collect(),
        "tool_result" => messages.iter().filter(|m| role_is(m, "tool")).collect(),
        "reasoning" => messages
            .iter()
            .filter(|m| {
                m.get("reasoning_content")
                    .and_then(|v| v.as_str())
                    .is_some_and(|r| !r.is_empty())
            })
            .collect(),
        "content" => messages
            .iter()
            .filter(|m| {
                role_is(m, "assistant")
                    && m.get("content")
                        .map(|c| !c.as_str().unwrap_or("").is_empty())
                        .unwrap_or(false)
                    && !has_tool_calls(m)
            })
            .collect(),
        _ => messages.iter().collect(), // "all" or unknown
    }
}

fn role_is(msg: &Value, role: &str) -> bool {
    msg.get("role")
        .and_then(|v| v.as_str())
        .map(|r| r == role)
        .unwrap_or(false)
}

fn has_tool_calls(msg: &Value) -> bool {
    msg.get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
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
fn message_body_lines(msg: &Value, section: SectionFilter) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    let reasoning = msg
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !reasoning.is_empty() && matches!(section, SectionFilter::All | SectionFilter::Reasoning) {
        lines.push(String::new());
        lines.push(reasoning.to_string());
    }

    let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
    if !content.is_empty() && matches!(section, SectionFilter::All | SectionFilter::Content) {
        lines.push(String::new());
        lines.push(content.to_string());
    }

    if matches!(section, SectionFilter::All | SectionFilter::ToolCall)
        && let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array())
    {
        for tc in tool_calls {
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
            let args = tc
                .get("arguments")
                .map(|v| v.to_string())
                .unwrap_or_default();
            lines.push(String::new());
            lines.push(format!("  → {name}({args})"));
        }
    }

    if msg.get("role").and_then(|v| v.as_str()) == Some("tool")
        && matches!(section, SectionFilter::All | SectionFilter::ToolResult)
    {
        let tool_call_id = msg
            .get("tool_call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        lines.push(String::new());
        lines.push(format!("  ← {tool_call_id}"));
        // Truncate long tool results.
        let display = common::truncate_chars(content, 500);
        lines.push(format!("  {display}"));
    }

    lines
}

fn print_messages(messages: &[&Value], filter: &str) {
    if messages.is_empty() {
        println!("No messages matching filter '{filter}'.");
        return;
    }

    let section = SectionFilter::from_filter(filter);
    for msg in messages {
        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let uuid = msg.get("uuid").and_then(|v| v.as_str()).unwrap_or("");

        println!("─────────────────────────────────────────────");
        println!("[{role}] {uuid}");
        for line in message_body_lines(msg, section) {
            println!("{line}");
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
                {"name": "bash", "arguments": {"cmd": "ls"}}
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

    // ── filter_messages ──────────────────────────────

    #[test]
    fn filter_by_role() {
        let msgs = vec![assistant_msg(), json!({"role": "user", "content": "hi"})];
        assert_eq!(filter_messages(&msgs, "user").len(), 1);
        assert_eq!(filter_messages(&msgs, "assistant").len(), 1);
        assert_eq!(filter_messages(&msgs, "all").len(), 2);
    }

    #[test]
    fn filter_reasoning_requires_non_empty() {
        let empty = json!({"role": "assistant", "reasoning_content": ""});
        let msgs = vec![assistant_msg(), empty];
        // Empty-string reasoning is not a reasoning message.
        assert_eq!(filter_messages(&msgs, "reasoning").len(), 1);
    }

    #[test]
    fn filter_content_excludes_tool_call_messages() {
        let msgs = vec![
            assistant_msg(),
            json!({"role": "assistant", "content": "plain"}),
        ];
        // The kitchen-sink message has tool_calls → excluded from "content".
        assert_eq!(filter_messages(&msgs, "content").len(), 1);
    }

    #[test]
    fn filter_tool_result_selects_tool_role() {
        let msgs = vec![assistant_msg(), tool_result_msg()];
        assert_eq!(filter_messages(&msgs, "tool_result").len(), 1);
        assert_eq!(filter_messages(&msgs, "tool_call").len(), 1);
    }

    // ── message_body_lines: field filters slice sections ──

    #[test]
    fn body_lines_content_does_not_leak_reasoning() {
        // Regression: `--type content` used to print reasoning too, because
        // the filter only selected messages while print_messages rendered
        // every section.
        let lines = message_body_lines(&assistant_msg(), SectionFilter::Content);
        let joined = lines.join("\n");
        assert!(joined.contains("the answer"));
        assert!(!joined.contains("thinking hard"));
        assert!(!joined.contains("bash"));
    }

    #[test]
    fn body_lines_reasoning_only() {
        let lines = message_body_lines(&assistant_msg(), SectionFilter::Reasoning);
        let joined = lines.join("\n");
        assert!(joined.contains("thinking hard"));
        assert!(!joined.contains("the answer"));
        assert!(!joined.contains("bash"));
    }

    #[test]
    fn body_lines_tool_call_only() {
        let lines = message_body_lines(&assistant_msg(), SectionFilter::ToolCall);
        let joined = lines.join("\n");
        assert!(joined.contains("→ bash"));
        assert!(!joined.contains("thinking hard"));
        assert!(!joined.contains("the answer"));
    }

    #[test]
    fn body_lines_tool_result_only() {
        let lines = message_body_lines(&tool_result_msg(), SectionFilter::ToolResult);
        let joined = lines.join("\n");
        assert!(joined.contains("← tc1"));
        assert!(joined.contains("file-a"));
    }

    #[test]
    fn body_lines_all_renders_every_section() {
        let lines = message_body_lines(&assistant_msg(), SectionFilter::All);
        let joined = lines.join("\n");
        assert!(joined.contains("thinking hard"));
        assert!(joined.contains("the answer"));
        assert!(joined.contains("→ bash"));
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
