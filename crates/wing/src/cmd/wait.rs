//! `wing wait` — block until specified sessions finish.
//!
//! Uses a hybrid approach: subscribes to session events via WebSocket
//! for fast TurnResult notification, and polls HTTP `/api/session/info`
//! every 1 second as a safety net (prevents starvation if the WS event
//! is missed due to subscription timing).
//!
//! # Example (orchestrator pattern)
//!
//! ```sh
//! SID1=$(wing run -p "task 1" --json | jq -r .session_id)
//! SID2=$(wing run -p "task 2" --json | jq -r .session_id)
//! wing wait "$SID1" "$SID2"
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::collections::HashSet;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tokio::time::interval;
use wing_api_client::GatewayClient as GatewayApiClient;

use super::common;
use crate::gateway::GatewayClient;
use crate::protocol::WingEvent;

/// Per-session result tracked during wait.
#[derive(Serialize, Clone)]
struct SessionResult {
    session_id: String,
    status: String,
    subtype: String,
    is_error: bool,
    result: String,
    num_turns: i64,
}

/// Output of `wing wait`.
#[derive(Serialize)]
struct WaitOutput {
    results: Vec<SessionResult>,
}

/// Entry point for `wing wait`.
pub async fn run_wait(session_ids: &[String], timeout_secs: u64, json: bool) -> ExitCode {
    match wait_inner(session_ids, timeout_secs).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_text(&output);
            }
            // Exit code: FAILURE if any session had an error.
            let any_error = output.results.iter().any(|r| r.is_error);
            if any_error {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("wing wait error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn wait_inner(session_ids: &[String], timeout_secs: u64) -> Result<WaitOutput> {
    if session_ids.is_empty() {
        anyhow::bail!("no session IDs provided");
    }

    // 1. Ensure gateway is running.
    let (host, port) = common::ensure_gateway().await?;
    let ws_url = format!("ws://{host}:{port}/ws");
    let http_base = format!("http://{host}:{port}");
    let api_key = common::load_api_key();
    let api_key_ref = api_key.as_deref();

    // 2. WS connect (for TurnResult events).
    let mut gateway = GatewayClient::connect(&ws_url, api_key_ref).await?;
    let client_id = gateway.client_id().to_string();

    // 3. HTTP client.
    let http = GatewayApiClient::new(&http_base, api_key_ref)
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {e}"))?;

    // 4. Subscribe to each session.
    for sid in session_ids {
        http.subscribe(sid, &client_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to subscribe to {sid}: {e}"))?;
    }

    // 5. Initialize tracking state.
    let mut pending: HashSet<String> = session_ids.iter().cloned().collect();
    let mut seen_working: HashSet<String> = HashSet::new();
    let mut results: Vec<SessionResult> = Vec::new();

    // Track sessions that were initially idle (possible race: not started yet).
    let mut idle_since: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();

    // 6. Check initial status.
    for sid in session_ids {
        match http.get_session_info(sid).await {
            Ok(info) => {
                let status = info.status.as_str();
                if status == "working" || status == "waiting" {
                    seen_working.insert(sid.clone());
                } else if status == "idle" || status == "inactive" {
                    // Might be done already, or might not have started yet.
                    // Record the time we first saw idle.
                    idle_since.insert(sid.clone(), std::time::Instant::now());
                }
            }
            Err(e) => {
                // Session not found or error — treat as done with error.
                tracing::warn!("Failed to get info for {sid}: {e}");
                results.push(SessionResult {
                    session_id: sid.clone(),
                    status: "unknown".into(),
                    subtype: "error".into(),
                    is_error: true,
                    result: format!("failed to get session info: {e}"),
                    num_turns: 0,
                });
                pending.remove(sid);
            }
        }
    }

    // 7. Hybrid event loop: WS events + HTTP polling.
    let mut poll_timer = interval(Duration::from_secs(1));
    // First tick fires immediately; skip it.
    poll_timer.tick().await;

    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

    while !pending.is_empty() {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            // Timeout — mark remaining as timed out.
            for sid in pending.drain() {
                results.push(SessionResult {
                    session_id: sid,
                    status: "timeout".into(),
                    subtype: "timeout".into(),
                    is_error: true,
                    result: format!("timed out after {timeout_secs}s"),
                    num_turns: 0,
                });
            }
            break;
        }

        tokio::select! {
            // WS event path: fast TurnResult notification.
            event = gateway.recv_event() => {
                match event {
                    Some(WingEvent::TurnResult {
                        subtype,
                        is_error,
                        result,
                        num_turns,
                        meta,
                        ..
                    }) => {
                        let sid = meta.session_id.unwrap_or_default();
                        if pending.remove(&sid) {
                            results.push(SessionResult {
                                session_id: sid.clone(),
                                status: "idle".into(),
                                subtype,
                                is_error,
                                result: result.unwrap_or_default(),
                                num_turns,
                            });
                        }
                    }
                    Some(_) => { /* ignore other events */ }
                    None => {
                        // WS closed — fall back to pure polling.
                        tracing::warn!("WS connection closed during wait, falling back to polling");
                    }
                }
            }

            // HTTP polling path: safety net (every 1s).
            _ = poll_timer.tick() => {
                let to_check: Vec<String> = pending.iter().cloned().collect();
                for sid in to_check {
                    match http.get_session_info(&sid).await {
                        Ok(info) => {
                            let status = info.status.as_str();
                            if status == "working" || status == "waiting" {
                                seen_working.insert(sid.clone());
                                // Reset idle_since if it was tracking.
                                idle_since.remove(&sid);
                            } else if status == "idle" || status == "inactive" {
                                // Session is idle. Two cases:
                                // a) We've seen it working → done.
                                // b) Never seen working → maybe hasn't started yet.
                                //    Wait 3s grace period before declaring done.
                                let should_finish = if seen_working.contains(&sid) {
                                    true
                                } else {
                                    // Check grace period.
                                    let first_idle = idle_since
                                        .entry(sid.clone())
                                        .or_insert(std::time::Instant::now());
                                    first_idle.elapsed() > Duration::from_secs(3)
                                };

                                if should_finish {
                                    pending.remove(&sid);
                                    idle_since.remove(&sid);
                                    // Try to get last result via session/get.
                                    let (result_text, num_turns) =
                                        fetch_last_result(&http, &sid).await;
                                    results.push(SessionResult {
                                        session_id: sid,
                                        status: status.to_string(),
                                        subtype: "success".into(),
                                        is_error: false,
                                        result: result_text,
                                        num_turns,
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Poll failed for {sid}: {e}");
                        }
                    }
                }
            }
        }
    }

    // 8. Unsubscribe (best effort).
    for sid in session_ids {
        let _ = http.unsubscribe(sid, &client_id).await;
    }

    // Sort results to match input order.
    let mut ordered: Vec<SessionResult> = Vec::new();
    for sid in session_ids {
        if let Some(r) = results.iter().find(|r| &r.session_id == sid) {
            ordered.push(r.clone());
        }
    }
    // Append any results not in the original order (e.g. timeouts).
    for r in results {
        if !ordered.iter().any(|o| o.session_id == r.session_id) {
            ordered.push(r);
        }
    }

    Ok(WaitOutput { results: ordered })
}

/// Fetch the last assistant message from a session as a best-effort result.
async fn fetch_last_result(http: &GatewayApiClient, sid: &str) -> (String, i64) {
    match http.get_session(sid).await {
        Ok(resp) => {
            // Find last assistant message with content.
            for msg in resp.messages.iter().rev() {
                let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role == "assistant" {
                    let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    if !content.is_empty() {
                        let count = resp.messages.len() as i64;
                        return (content.to_string(), count);
                    }
                }
            }
            (String::new(), resp.messages.len() as i64)
        }
        Err(_) => (String::new(), 0),
    }
}

fn print_text(output: &WaitOutput) {
    for r in &output.results {
        println!("session_id: {}", r.session_id);
        println!("  status:     {}", r.status);
        println!("  result:     {}", r.subtype);
        println!("  is_error:   {}", r.is_error);
        println!("  num_turns:  {}", r.num_turns);
        // Truncate long results for display.
        let result_display = common::truncate_chars(&r.result, 200);
        println!("  last_text:  {result_display}");
        println!();
    }
}
