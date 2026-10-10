//! `wing branches` / `wing fork` / `wing rewind` — message-node navigation.
//!
//! A session's chain is a tree of message nodes: `wing branches` lists the
//! nodes a session can be **forked** (a new session keeps everything before
//! the node) or **rewound** to (the current session is cut back to just
//! before the node), and prints the uuids the two commands take. The list
//! also carries a `current` sentinel: forking at `current` copies the whole
//! chain, rewinding to `current` is a documented no-op.
//!
//! Both commands return the target message's text as **draft** — the message
//! itself is left out of the new/cut chain so it can be re-sent (possibly
//! edited) by the caller:
//!
//! ```sh
//! wing branches "$SID"                       # list nodes (uuid / role / preview)
//! wing fork     "$SID" --at <uuid>           # → new session id + draft
//! wing rewind   "$SID" --to <uuid>           # → draft (session keeps the prefix)
//! ```
//!
//! Evicted sessions are hydrated on 404 (the CLI-wide "eviction ≠ missing"
//! convention, same as `wing tail` / `info`): the node list and both
//! operations need the session in gateway memory.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use ratatui::style::Style;
use serde::Serialize;
use wing_api_client::models::BranchTargetInfo;
use wing_api_client::models::BranchesResponse;

use crate::config::AppConfig;
use crate::config::ThemePalette;
use crate::render::table::ColumnKind;
use crate::render::table::plain::{PlainCell, PlainColumn, PlainTable};

use super::common;
use super::common::TableOutput;

/// Longest content preview in the branches table (the backend already caps
/// the stored preview at 100 chars; this only bounds the column).
const CONTENT_CAP: usize = 80;

/// Draft preview cap in text output.
const DRAFT_CAP: usize = 120;

// ============================================================
// wing branches
// ============================================================

/// Entry point for `wing branches`.
pub async fn run_branches(session_id: &str, json: bool) -> ExitCode {
    match fetch_branches(session_id).await {
        Ok(targets) => {
            if json {
                common::print_json_compact(&BranchesResponse {
                    targets: targets.clone(),
                });
            } else {
                print_branches(&targets);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing branches error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn fetch_branches(session_id: &str) -> Result<Vec<BranchTargetInfo>> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    let resp = common::hydrate_on_404(&http, session_id, || http.get_branches(session_id)).await?;
    Ok(resp.targets)
}

fn print_branches(targets: &[BranchTargetInfo]) {
    if targets.is_empty() {
        println!("No branch targets (the session has no messages yet).");
        return;
    }
    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = TableOutput::detect();
    print!("{}", format_branches_table(targets, &palette, &out));
}

/// Render the branch-target table. The uuid is the copyable handle `fork` /
/// `rewind` take, so it is never truncated (same rule as the session id in
/// `wing ps`); only the preview yields width.
fn format_branches_table(
    targets: &[BranchTargetInfo],
    palette: &ThemePalette,
    out: &TableOutput,
) -> String {
    let columns = vec![
        PlainColumn::keep_natural("UUID", ColumnKind::Compact),
        PlainColumn::new("ROLE", ColumnKind::Compact),
        PlainColumn::capped("CONTENT", ColumnKind::Narrative, CONTENT_CAP),
    ];
    let rows = targets
        .iter()
        .map(|target| {
            let styled_uuid = if target.uuid == "current" {
                Style::new().fg(palette.warning)
            } else {
                Style::new().fg(palette.tool_result)
            };
            vec![
                PlainCell::styled(target.uuid.clone(), styled_uuid),
                PlainCell::styled(target.role.clone(), Style::new().fg(palette.dim)),
                PlainCell::styled(
                    common::single_line(&target.content),
                    Style::new().fg(palette.text),
                ),
            ]
        })
        .collect();
    let table = PlainTable { columns, rows };
    common::render_table(&table, palette, out)
}

// ============================================================
// wing fork
// ============================================================

/// Output of `wing fork`.
#[derive(Serialize)]
struct ForkOutput {
    /// The new session (the fork's id — what the caller continues with).
    session_id: String,
    /// The session the fork was cut from.
    source_session_id: String,
    /// The node the fork cut at.
    target_uuid: String,
    /// The target message's text, left out of the new session for re-sending
    /// (`""` when forking at the `current` sentinel, `null` if unavailable).
    draft: Option<String>,
}

/// Entry point for `wing fork`.
pub async fn run_fork(session_id: &str, at: &str, json: bool) -> ExitCode {
    match fork_inner(session_id, at).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_fork(&output);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing fork error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn fork_inner(session_id: &str, target_uuid: &str) -> Result<ForkOutput> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    let resp = common::hydrate_on_404(&http, session_id, || {
        http.fork_session(session_id, target_uuid)
    })
    .await?;
    Ok(ForkOutput {
        session_id: resp.session_id,
        source_session_id: session_id.to_string(),
        target_uuid: target_uuid.to_string(),
        draft: resp.draft,
    })
}

fn print_fork(output: &ForkOutput) {
    println!("session_id:  {}", output.session_id);
    println!("source:      {}", output.source_session_id);
    println!("at:          {}", output.target_uuid);
    println!("draft:       {}", draft_preview(output.draft.as_deref()));
}

// ============================================================
// wing rewind
// ============================================================

/// Output of `wing rewind`.
#[derive(Serialize)]
struct RewindOutput {
    session_id: String,
    target_uuid: String,
    /// The target message's text (the caller can re-send it, edited); `null`
    /// for the `current` no-op.
    draft: Option<String>,
}

/// Entry point for `wing rewind`.
pub async fn run_rewind(session_id: &str, to: &str, json: bool) -> ExitCode {
    match rewind_inner(session_id, to).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_rewind(&output);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing rewind error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn rewind_inner(session_id: &str, target_uuid: &str) -> Result<RewindOutput> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    let resp = common::hydrate_on_404(&http, session_id, || {
        http.rewind_session(session_id, target_uuid)
    })
    .await?;
    Ok(RewindOutput {
        session_id: session_id.to_string(),
        target_uuid: target_uuid.to_string(),
        draft: resp.draft,
    })
}

fn print_rewind(output: &RewindOutput) {
    println!("session_id:  {}", output.session_id);
    println!("to:          {}", output.target_uuid);
    println!("draft:       {}", draft_preview(output.draft.as_deref()));
}

/// Draft preview for text output: flattened to one line and truncated
/// (`--json` carries it in full).
fn draft_preview(draft: Option<&str>) -> String {
    match draft {
        None | Some("") => "-".to_string(),
        Some(text) => common::truncate_chars(&common::single_line(text), DRAFT_CAP),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(uuid: &str, role: &str, content: &str) -> BranchTargetInfo {
        BranchTargetInfo {
            uuid: uuid.into(),
            content: content.into(),
            role: role.into(),
        }
    }

    fn out() -> TableOutput {
        TableOutput {
            width: 100,
            color: false,
        }
    }

    /// 表格：uuid 完整在场（fork/rewind 要精确复制）、等宽、多行内容压平。
    #[test]
    fn branches_table_keeps_full_uuids_and_is_aligned() {
        let targets = vec![
            target(
                "3f2a1b4c-5d6e-7f80-9a0b-1c2d3e4f5a6b",
                "user",
                "first question",
            ),
            target(
                "a1b2c3d4-e5f6-7081-9203-a4b5c6d7e8f9",
                "user",
                "第二行\n第三行",
            ),
            target("current", "user", "(current)"),
        ];
        let table = format_branches_table(&targets, &ThemePalette::default(), &out());
        let lines: Vec<&str> = table.lines().collect();
        // 顶框 / 表头 / 表头分隔 / 3 行数据（行间各有分隔）/ 底框 = 9。
        assert_eq!(lines.len(), 9, "{table}");
        assert!(lines[0].starts_with('┏'), "{table}");
        assert!(lines[1].contains("UUID") && lines[1].contains("CONTENT"));

        let widths: Vec<usize> = lines
            .iter()
            .map(|l| unicode_width::UnicodeWidthStr::width(*l))
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert!(widths[0] <= 100, "{widths:?}");

        assert!(
            table.contains("3f2a1b4c-5d6e-7f80-9a0b-1c2d3e4f5a6b"),
            "{table}"
        );
        assert!(table.contains("current"), "{table}");
        // 多行内容单行化，不撕开数据行。
        assert!(table.contains("第二行 第三行"), "{table}");
        assert!(!table.contains("第二行\n第三行"), "{table}");
    }

    #[test]
    fn draft_preview_flattens_truncates_and_falls_back() {
        assert_eq!(draft_preview(None), "-");
        assert_eq!(draft_preview(Some("")), "-");
        assert_eq!(draft_preview(Some("line1\nline2")), "line1 line2");
        let long = "x".repeat(DRAFT_CAP + 10);
        let preview = draft_preview(Some(&long));
        assert_eq!(preview.chars().count(), DRAFT_CAP, "{preview}");
        assert!(preview.ends_with("..."), "{preview}");
    }
}
