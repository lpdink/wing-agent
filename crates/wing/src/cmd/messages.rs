//! `wing tail` / `wing head` — message filtering (like Unix head/tail).
//!
//! Fetches session messages via `GET /api/session/get` and filters by type.
//! `tail` shows the last N, `head` shows the first N.
//!
//! # Filter types
//!
//! | `--type`       | Condition                                  |
//! |----------------|---------------------------------------------|
//! | `all` (default)| All messages                                |
//! | `user`         | `role == "user"`                            |
//! | `assistant`    | `role == "assistant"`                       |
//! | `tool_call`    | Assistant messages with `tool_calls`        |
//! | `tool_result`  | `role == "tool"`                            |
//! | `reasoning`    | Messages with `reasoning_content`           |
//! | `content`      | Assistant with text content, no tool_calls  |

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
            .filter(|m| m.get("reasoning_content").is_some())
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

fn print_messages(messages: &[&Value], filter: &str) {
    if messages.is_empty() {
        println!("No messages matching filter '{filter}'.");
        return;
    }

    for msg in messages {
        let role = msg
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let uuid = msg.get("uuid").and_then(|v| v.as_str()).unwrap_or("");

        println!("─────────────────────────────────────────────");
        println!("[{role}] {uuid}");

        // Reasoning content (if present).
        if let Some(reasoning) = msg.get("reasoning_content").and_then(|v| v.as_str())
            && !reasoning.is_empty()
        {
            println!();
            println!("{reasoning}");
        }

        // Text content.
        if let Some(content) = msg.get("content").and_then(|v| v.as_str())
            && !content.is_empty()
        {
            println!();
            println!("{content}");
        }

        // Tool calls (if present).
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tool_calls {
                let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                let args = tc
                    .get("arguments")
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                println!();
                println!("  → {name}({args})");
            }
        }

        // Tool result (if role == "tool").
        if role == "tool" {
            let tool_call_id = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
            println!();
            println!("  ← {tool_call_id}");
            // Truncate long tool results.
            let display = common::truncate_chars(content, 500);
            println!("  {display}");
        }
    }
    println!("─────────────────────────────────────────────");
}
