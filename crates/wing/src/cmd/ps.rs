//! `wing ps` — list sessions, and `wing info` — session runtime info.
//!
//! `wing ps` lists all sessions (like `docker ps` or `kubectl get pods`).
//! `wing info <sid>` shows detailed runtime state for a single session.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use wing_api_client::models::{SessionInfo, SessionInfoResponse};

use super::common;

/// Entry point for `wing ps`.
pub async fn run_ps(all: bool, json: bool, watch: bool) -> ExitCode {
    if watch {
        return run_ps_watch(all, json).await;
    }
    match fetch_sessions().await {
        Ok(sessions) => {
            let filtered = if all {
                sessions
            } else {
                sessions
                    .into_iter()
                    .filter(|s| s.status != "inactive")
                    .collect()
            };
            if json {
                common::print_json_compact(&filtered);
            } else {
                print_sessions_table(&filtered);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing ps error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Watch mode: clear screen and reprint every 2 seconds.
async fn run_ps_watch(all: bool, json: bool) -> ExitCode {
    loop {
        match fetch_sessions().await {
            Ok(sessions) => {
                let filtered = if all {
                    sessions
                } else {
                    sessions
                        .into_iter()
                        .filter(|s| s.status != "inactive")
                        .collect()
                };
                // Clear screen.
                print!("\x1b[2J\x1b[H");
                if json {
                    common::print_json_compact(&filtered);
                } else {
                    print_sessions_table(&filtered);
                }
            }
            Err(e) => {
                eprintln!("wing ps error: {e}");
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Entry point for `wing info`.
pub async fn run_info(session_id: &str, json: bool) -> ExitCode {
    match fetch_session_info(session_id).await {
        Ok(info) => {
            if json {
                common::print_json_compact(&info);
            } else {
                print_session_info(&info);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing info error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn fetch_sessions() -> Result<Vec<SessionInfo>> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    let resp = http.list_sessions().await?;
    Ok(resp.sessions)
}

async fn fetch_session_info(session_id: &str) -> Result<SessionInfoResponse> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    Ok(http.get_session_info(session_id).await?)
}

fn print_sessions_table(sessions: &[SessionInfo]) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }

    // Column widths.
    let id_w = 30;
    let status_w = 10;
    let model_w = 20;
    let name_w = 30;

    // Header.
    println!(
        "{:id_w$} {:<status_w$} {:<model_w$} {:<name_w$}",
        "SESSION ID", "STATUS", "LAST INTERACTION", "NAME",
    );
    println!("{}", "-".repeat(id_w + status_w + model_w + name_w + 3));

    for s in sessions {
        let id = truncate_str(&s.id, id_w);
        let status = truncate_str(&s.status, status_w);
        let last = truncate_str(s.last_interaction.as_deref().unwrap_or("-"), model_w);
        let name = truncate_str(s.name.as_deref().unwrap_or("-"), name_w);
        println!(
            "{:id_w$} {:<status_w$} {:<model_w$} {:<name_w$}",
            id, status, last, name
        );
    }
}

fn print_session_info(info: &SessionInfoResponse) {
    println!("model:               {}", info.model);
    println!("api_url:             {}", info.api_url);
    println!("status:              {}", info.status);
    println!("thinking:            {}", info.thinking);
    if let Some(ref effort) = info.reasoning_effort {
        println!("reasoning_effort:    {effort}");
    }
    println!("yolo:                {}", info.yolo);
    if let Some(ref name) = info.session_name {
        println!("session_name:        {name}");
    }
    if let Some(ref wd) = info.workdir {
        println!("workdir:             {wd}");
    }
    println!("tools:               {}", info.tools.join(", "));
    println!();
    println!("context:");
    println!("  messages:           {}", info.context_stats.message_count);
    println!("  total_tokens:       {}", info.context_stats.total_tokens);
    println!("  context_window:     {}", info.context_window_tokens);
}

/// Truncate a string to at most `max` chars (Unicode-safe, delegates to common).
fn truncate_str(s: &str, max: usize) -> String {
    common::truncate_chars(s, max)
}
