//! `wing release` — 逐出（evict）会话的内存态。
//!
//! 逐出只回收网关内存（worker + provider client），**磁盘状态不动**：被逐出的
//! 会话在下一次被使用时按需水合（`wing run -r` / `resume` / `subscribe` /
//! 发消息都算）。
//!
//! 与 TTL 自动逐出的关系：自动逐出等 `sessions.eviction.idle_ttl_seconds`
//! 到期；`release` 是"立即执行一次逐出判定"——忽略空闲时长，
//! **但不忽略钉住条件**，网关对下列会话以 409 拒绝并给出原因：
//!
//! - 忙碌（working / waiting：在飞 turn 或挂着未回答的 Ask）；
//! - 有后台任务（如后台 Explorer）；
//! - 有客户端订阅中（有人正在看它——先断开那个客户端，如退出 TUI）；
//! - 非持久后端（memory：逐出等于数据销毁）。
//!
//! 幂等：本就不在内存的会话返回 `released=false`（not loaded），不是错误。
//!
//! # Example
//!
//! ```sh
//! wing release 20260919-143000-a1b2
//! wing release "$SID1" "$SID2" --json
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use serde::Serialize;

use super::common;

/// 单个会话的逐出结果。
#[derive(Serialize, Clone)]
struct ReleaseResult {
    session_id: String,
    released: bool,
    detail: String,
    is_error: bool,
}

/// `wing release` 的输出。
#[derive(Serialize)]
struct ReleaseOutput {
    results: Vec<ReleaseResult>,
}

/// Entry point for `wing release`.
pub async fn run(session_ids: &[String], json: bool) -> ExitCode {
    match release_inner(session_ids).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_text(&output);
            }
            if output.results.iter().any(|r| r.is_error) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("wing release error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn release_inner(session_ids: &[String]) -> Result<ReleaseOutput> {
    if session_ids.is_empty() {
        anyhow::bail!("no session IDs provided");
    }

    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;

    let mut results: Vec<ReleaseResult> = Vec::with_capacity(session_ids.len());
    for sid in session_ids {
        match http.release_session(sid).await {
            Ok(resp) => results.push(ReleaseResult {
                session_id: sid.clone(),
                released: resp.released,
                detail: resp.detail,
                is_error: false,
            }),
            // 单会话失败（404 不存在 / 409 被钉住）不阻断其余会话。
            Err(e) => results.push(ReleaseResult {
                session_id: sid.clone(),
                released: false,
                detail: e.to_string(),
                is_error: true,
            }),
        }
    }
    Ok(ReleaseOutput { results })
}

fn print_text(output: &ReleaseOutput) {
    let id_w = 30;
    let result_w = 10;
    println!("{:id_w$} {:<result_w$} DETAIL", "SESSION ID", "RESULT");
    println!("{}", "-".repeat(id_w + result_w + 30));
    for r in &output.results {
        let result = if r.is_error {
            "error"
        } else if r.released {
            "released"
        } else {
            "skipped"
        };
        println!(
            "{:id_w$} {:<result_w$} {}",
            common::truncate_chars(&r.session_id, id_w),
            result,
            r.detail
        );
    }
}
