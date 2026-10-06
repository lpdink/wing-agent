//! `wing ps` — list sessions, and `wing info` — session runtime info.
//!
//! `wing ps` lists all sessions (like `docker ps` or `kubectl get pods`).
//! `wing info <sid>` shows detailed runtime state for a single session.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use wing_api_client::models::{SessionInfo, SessionInfoResponse};

use crate::shared::pinning::{is_pinned, pin_added_at, pin_key};

use super::common;

/// Entry point for `wing ps`.
pub async fn run_ps(all: bool, tags: &[String], json: bool, watch: bool) -> ExitCode {
    if watch {
        return run_ps_watch(all, tags, json).await;
    }
    match fetch_sessions().await {
        Ok(sessions) => {
            let mut filtered = filter_sessions(sessions, all, tags);
            order_sessions(&mut filtered);
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
/// Creates the HTTP client once, then polls in a loop.
async fn run_ps_watch(all: bool, tags: &[String], json: bool) -> ExitCode {
    let (host, port) = match common::ensure_gateway().await {
        Ok(hp) => hp,
        Err(e) => {
            eprintln!("wing ps error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let http = match common::create_api_client(&host, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wing ps error: {e}");
            return ExitCode::FAILURE;
        }
    };

    loop {
        match http.list_sessions().await {
            Ok(resp) => {
                let mut filtered = filter_sessions(resp.sessions, all, tags);
                order_sessions(&mut filtered);
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

/// Filter sessions by status and tags.
///
/// - With `tags` given: keep sessions carrying ALL tags (AND), **inactive
///   included** — tags describe long-term taxonomy (favorites / task crews
///   are often archived), so the default inactive-drop does not apply.
/// - Without `tags`: `all=true` keeps everything; otherwise drop `inactive`
///   — except **pinned** sessions, which the default view keeps on purpose
///   (pin means "keep this one in sight", and what you pin is usually a
///   session you stepped away from).
fn filter_sessions(sessions: Vec<SessionInfo>, all: bool, tags: &[String]) -> Vec<SessionInfo> {
    if !tags.is_empty() {
        return sessions
            .into_iter()
            .filter(|s| tags.iter().all(|t| s.tags.iter().any(|st| st == t)))
            .collect();
    }
    if all {
        sessions
    } else {
        sessions
            .into_iter()
            .filter(|s| s.status != "inactive" || is_pinned(&s.tags))
            .collect()
    }
}

/// 呈现顺序：后端基准序 + 前端 pin 叠加 —— 被 pin 的置顶，组内后 pin 的更靠前
/// （唯一实现在 [`crate::shared::pinning`]，与 `/ss` 面板共用）；其余保持
/// 后端下发的顺序（活跃优先、组内按最后交互时间降序）。
fn order_sessions(sessions: &mut [SessionInfo]) {
    sessions.sort_by_cached_key(|s| pin_key(is_pinned(&s.tags), pin_added_at(&s.tag_meta)));
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
    // 被逐出（不在内存）的会话 404——先 resume 水合再取，与 `wing tail`
    // 的 404→resume 是同一条惯例（逐出 ≠ 不存在）。
    match http.get_session_info(session_id).await {
        Ok(info) => Ok(info),
        Err(e) if e.is_not_found() => {
            http.resume_session(session_id).await?;
            Ok(http.get_session_info(session_id).await?)
        }
        Err(e) => Err(e.into()),
    }
}

fn print_sessions_table(sessions: &[SessionInfo]) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }

    // Column widths. TAGS is appended last (existing columns keep their
    // position); NAME was trimmed 30 → 24 to keep the row within 120 cols.
    let id_w = 30;
    let status_w = 10;
    let last_w = 20;
    let name_w = 24;
    let tags_w = 32;

    // Header.
    println!(
        "{:id_w$} {:<status_w$} {:<last_w$} {:<name_w$} {:<tags_w$}",
        "SESSION ID", "STATUS", "LAST INTERACTION", "NAME", "TAGS",
    );
    println!(
        "{}",
        "-".repeat(id_w + status_w + last_w + name_w + tags_w + 4)
    );

    for s in sessions {
        let id = truncate_str(&s.id, id_w);
        let status = truncate_str(&s.status, status_w);
        let last = truncate_str(s.last_interaction.as_deref().unwrap_or("-"), last_w);
        let name = truncate_str(s.name.as_deref().unwrap_or("-"), name_w);
        let tags = if s.tags.is_empty() {
            "-".to_string()
        } else {
            truncate_str(&s.tags.join(","), tags_w)
        };
        println!(
            "{:id_w$} {:<status_w$} {:<last_w$} {:<name_w$} {:<tags_w$}",
            id, status, last, name, tags
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
    if !info.tags.is_empty() {
        println!("tags:                {}", info.tags.join(", "));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, status: &str, tags: &[&str]) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: Some(id.into()),
            created_at: None,
            template_name: None,
            workspace: None,
            last_interaction: None,
            status: status.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            tag_meta: Default::default(),
        }
    }

    /// 一个 pin 过的会话（带打标时间）。
    fn pinned(id: &str, status: &str, added_at: Option<&str>) -> SessionInfo {
        let mut info = session(id, status, &["pin"]);
        if let Some(stamp) = added_at {
            info.tag_meta.insert(
                "pin".into(),
                wing_api_client::models::TagMeta {
                    added_at: Some(stamp.into()),
                },
            );
        }
        info
    }

    fn ids(sessions: &[SessionInfo]) -> Vec<String> {
        sessions.iter().map(|s| s.id.clone()).collect()
    }

    #[test]
    fn default_filter_drops_inactive_but_all_keeps() {
        let sessions = vec![session("a", "working", &[]), session("b", "inactive", &[])];
        assert_eq!(ids(&filter_sessions(sessions.clone(), false, &[])), ["a"]);
        assert_eq!(ids(&filter_sessions(sessions, true, &[])), ["a", "b"]);
    }

    #[test]
    fn default_filter_keeps_pinned_inactive_sessions() {
        // pin 的语义是"我要一直看到它"——inactive 也不例外；未 pin 的
        // inactive 照旧被默认视图丢掉。
        let sessions = vec![
            session("a", "working", &[]),
            session("b", "inactive", &[]),
            pinned("c", "inactive", Some("2026-10-05T21:30:12")),
        ];
        assert_eq!(
            ids(&filter_sessions(sessions.clone(), false, &[])),
            ["a", "c"]
        );
        assert_eq!(ids(&filter_sessions(sessions, true, &[])), ["a", "b", "c"]);
    }

    #[test]
    fn order_puts_pinned_first_by_pin_time() {
        // 后端基准序（活跃在前、组内时间降序）之上叠加 pin：置顶组内后 pin
        // 的更靠前；无记录 / 不可解析的排在有时间者之后；未 pin 的原序。
        let mut sessions = vec![
            session("active-new", "idle", &[]),
            pinned("old-pin", "inactive", Some("2026-10-01T09:00:00")),
            session("active-old", "waiting", &[]),
            pinned("new-pin", "inactive", Some("2026-10-06T09:00:00")),
            pinned("bare-pin", "inactive", None),
        ];
        order_sessions(&mut sessions);
        assert_eq!(
            ids(&sessions),
            ["new-pin", "old-pin", "bare-pin", "active-new", "active-old"]
        );
    }

    #[test]
    fn order_is_stable_when_nothing_is_pinned() {
        let mut sessions = vec![
            session("first", "idle", &[]),
            session("second", "inactive", &[]),
            session("third", "working", &["executor"]),
        ];
        order_sessions(&mut sessions);
        assert_eq!(ids(&sessions), ["first", "second", "third"]);
    }

    #[test]
    fn tag_filter_is_and_and_includes_inactive() {
        let sessions = vec![
            session("s1", "working", &["executor", "task=a"]),
            session("s2", "inactive", &["executor", "task=a"]),
            session("s3", "inactive", &["executor"]),
            session("s4", "inactive", &[]),
        ];
        // 单标签：命中即保留（含 inactive）。
        let out = filter_sessions(sessions.clone(), false, &["executor".into()]);
        assert_eq!(ids(&out), ["s1", "s2", "s3"]);
        // 多标签：AND；不因 inactive 被默认丢弃。
        let out = filter_sessions(sessions, false, &["executor".into(), "task=a".into()]);
        assert_eq!(ids(&out), ["s1", "s2"]);
    }

    #[test]
    fn tag_filter_no_match_returns_empty() {
        let sessions = vec![session("a", "working", &["x"])];
        assert!(filter_sessions(sessions, true, &["nope".into()]).is_empty());
    }
}
