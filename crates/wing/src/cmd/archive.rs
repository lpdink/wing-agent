//! `wing archive` — 归档 / 恢复 / 列出归档区。
//!
//! 一个子命令三种用法（刻意不堆子命令）：
//!
//! - `wing archive <session-id>...` —— 归档：会话移入网关存储根的
//!   `.archive/`，从列表消失并冻结（resume / send / tag 等按其 id 的调用
//!   报「已归档」，提示先恢复）；
//! - `wing archive --restore <session-id>...` —— 恢复：移回 live 区；
//! - `wing archive --list` —— 列出归档区条目（读网关侧索引，不扫会话目录）。
//!
//! 归档是**纯移动**（字节不变）；钉住条件（忙碌 / 排队输入 / 被订阅 /
//! 非归档后端）由网关以 409 拒绝，本命令如实带出原因。多 id 时单会话失败
//! 不阻断其余（与 `wing release` 同款）。
//!
//! ```sh
//! wing archive 20260101-120000-abcd
//! wing archive --restore "$SID"
//! wing archive --list
//! wing archive --list --json
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use serde::Serialize;
use wing_api_client::models::ArchivedSession;

use super::common;

/// 单个会话的归档 / 恢复结果。
#[derive(Serialize, Clone)]
struct ArchiveResult {
    session_id: String,
    /// 结果状态是否处于目标状态（true = 现在在归档区 / 已恢复）。
    archived: bool,
    /// 本次调用是否真的移动了会话（false = 幂等 no-op）。
    changed: bool,
    detail: String,
    is_error: bool,
}

/// `wing archive`（归档 / 恢复方向）的输出。
#[derive(Serialize)]
struct ArchiveOutput {
    results: Vec<ArchiveResult>,
}

/// Validate the mutually exclusive shapes of the one subcommand.
///
/// Kept pure so the three-way contract is unit-testable without a gateway.
pub(crate) fn validate(session_ids: &[String], restore: bool, list: bool) -> Result<()> {
    if list {
        if restore || !session_ids.is_empty() {
            anyhow::bail!(
                "--list cannot be combined with session IDs or --restore \
                 (it lists the archive; restore with: wing archive --restore <session-id>)"
            );
        }
        return Ok(());
    }
    if session_ids.is_empty() {
        anyhow::bail!(
            "no session IDs provided; archive with: wing archive <session-id>, \
             restore with: wing archive --restore <session-id>, \
             list with: wing archive --list"
        );
    }
    Ok(())
}

/// Entry point for `wing archive`.
pub async fn run(session_ids: &[String], restore: bool, list: bool, json: bool) -> ExitCode {
    match run_inner(session_ids, restore, list, json).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wing archive error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_inner(
    session_ids: &[String],
    restore: bool,
    list: bool,
    json: bool,
) -> Result<ExitCode> {
    validate(session_ids, restore, list)?;

    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;

    if list {
        let resp = http.list_archived().await?;
        if json {
            common::print_json_compact(&resp.sessions);
        } else {
            print_archive_list(&resp.sessions);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let mut results: Vec<ArchiveResult> = Vec::with_capacity(session_ids.len());
    for sid in session_ids {
        // The wire direction is a state to set: `archived: !restore`.
        match http.archive_session(sid, !restore).await {
            Ok(resp) => results.push(ArchiveResult {
                session_id: sid.clone(),
                archived: resp.archived,
                changed: resp.changed,
                detail: resp.detail,
                is_error: false,
            }),
            // 单会话失败（404 不存在 / 409 被钉住）不阻断其余会话；文案走
            // 共享的会话错误（普通 404 有"怎么找"，归档 404 有"怎么恢复"）。
            Err(e) => results.push(ArchiveResult {
                session_id: sid.clone(),
                archived: restore,
                changed: false,
                detail: common::session_error(sid, &e).to_string(),
                is_error: true,
            }),
        }
    }

    if json {
        common::print_json_compact(&ArchiveOutput {
            results: results.clone(),
        });
    } else {
        print_results(&results, restore);
    }
    if results.iter().any(|r| r.is_error) {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

/// The RESULT column: the action that actually happened.
fn result_label(result: &ArchiveResult, restore: bool) -> &'static str {
    if result.is_error {
        return "error";
    }
    if !result.changed {
        return "skipped";
    }
    if restore { "restored" } else { "archived" }
}

/// The archive / restore result table (pure so the output contract is
/// unit-testable without a gateway).
fn format_results(results: &[ArchiveResult], restore: bool) -> String {
    let id_w = 30;
    let result_w = 10;
    let mut out = String::new();
    out.push_str(&format!(
        "{:id_w$} {:<result_w$} DETAIL\n",
        "SESSION ID", "RESULT"
    ));
    out.push_str(&"-".repeat(id_w + result_w + 30));
    out.push('\n');
    for r in results {
        out.push_str(&format!(
            "{:id_w$} {:<result_w$} {}\n",
            common::truncate_chars(&r.session_id, id_w),
            result_label(r, restore),
            r.detail
        ));
    }
    out
}

fn print_results(results: &[ArchiveResult], restore: bool) {
    print!("{}", format_results(results, restore));
}

/// The model cell: display name → call name → id (`-` when none — the entry
/// predates the model record or the gateway resolved nothing).
fn model_text(entry: &ArchivedSession) -> String {
    match [
        entry.model_display_name.as_deref(),
        entry.model_name.as_deref(),
        entry.model_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|value| !value.is_empty())
    {
        Some(value) => common::single_line(value),
        None => "-".to_string(),
    }
}

/// `archived_at`（本地 naive ISO）压缩成 `YYYY-MM-DD HH:MM`；不可解析时原样
/// 带出（时间未知的重建条目为 None → `-`）。
///
/// 按**字符**截断（不是字节切片）：`archived_at` 是索引文件里的任意字符串
/// （手改 / 旧版本写坏），按字节切会在多字节字符中间 panic。
fn format_archived_at(value: Option<&str>) -> String {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return "-".to_string();
    };
    raw.chars().take(16).collect::<String>().replace('T', " ")
}

fn print_archive_list(entries: &[ArchivedSession]) {
    use crate::config::{AppConfig, ThemePalette};

    use super::common::TableOutput;

    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = TableOutput::detect();
    print!("{}", format_archive_list(entries, &palette, &out));
}

/// The archive listing (pure: palette + output environment injected, so the
/// render contract — including multibyte timestamps — is unit-testable).
fn format_archive_list(
    entries: &[ArchivedSession],
    palette: &crate::config::ThemePalette,
    out: &common::TableOutput,
) -> String {
    use crate::render::table::ColumnKind;
    use crate::render::table::plain::{PlainCell, PlainColumn, PlainTable};
    use ratatui::style::Style;

    if entries.is_empty() {
        return "No archived sessions.\n".to_string();
    }

    let columns = vec![
        // 归档条目是对照 ps 找回来的：id 要能原样复制（不截断）。
        PlainColumn::keep_natural("SESSION ID", ColumnKind::Compact),
        PlainColumn::capped("NAME", ColumnKind::Narrative, 60),
        PlainColumn::capped("MODEL", ColumnKind::Compact, 32),
        PlainColumn::capped("TAGS", ColumnKind::Narrative, 40),
        PlainColumn::new("ARCHIVED", ColumnKind::Compact),
    ];
    let rows = entries
        .iter()
        .map(|entry| {
            vec![
                PlainCell::styled(entry.id.clone(), Style::new().fg(palette.tool_result)),
                PlainCell::styled(
                    common::single_line(entry.name.as_deref().unwrap_or("-")),
                    Style::new().fg(palette.text),
                ),
                PlainCell::styled(model_text(entry), Style::new().fg(palette.tool_result)),
                PlainCell::styled(
                    common::single_line(&entry.tags.join(", ")),
                    Style::new().fg(palette.tool_result),
                ),
                PlainCell::styled(
                    format_archived_at(entry.archived_at.as_deref()),
                    Style::new().fg(palette.dim),
                ),
            ]
        })
        .collect();

    let table = PlainTable { columns, rows };
    format!(
        "{}\n",
        common::render_table(&table, palette, out).trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn validate_enforces_the_three_shapes() {
        assert!(validate(&ids(&["s1"]), false, false).is_ok());
        assert!(validate(&ids(&["s1"]), true, false).is_ok());
        assert!(validate(&[], false, true).is_ok());

        let conflict = validate(&ids(&["s1"]), false, true).unwrap_err();
        assert!(conflict.to_string().contains("--list cannot be combined"));
        assert!(validate(&[], true, true).is_err());
        let missing = validate(&[], false, false).unwrap_err();
        assert!(missing.to_string().contains("no session IDs"));
    }

    #[test]
    fn result_label_reports_the_action() {
        let archived = ArchiveResult {
            session_id: "s1".into(),
            archived: true,
            changed: true,
            detail: "archived".into(),
            is_error: false,
        };
        assert_eq!(result_label(&archived, false), "archived");
        assert_eq!(result_label(&archived, true), "restored");

        let skipped = ArchiveResult {
            changed: false,
            ..archived.clone()
        };
        assert_eq!(result_label(&skipped, false), "skipped");

        let failed = ArchiveResult {
            is_error: true,
            ..archived
        };
        assert_eq!(result_label(&failed, false), "error");
    }

    #[test]
    fn model_text_falls_back_then_dash() {
        let mut entry = ArchivedSession::default();
        assert_eq!(model_text(&entry), "-");
        entry.model_id = Some("ds-flash".into());
        assert_eq!(model_text(&entry), "ds-flash");
        entry.model_name = Some("deepseek-flash".into());
        entry.model_display_name = Some("DeepSeek Flash".into());
        assert_eq!(model_text(&entry), "DeepSeek Flash");
    }

    #[test]
    fn archived_at_is_compacted() {
        assert_eq!(format_archived_at(None), "-");
        assert_eq!(format_archived_at(Some("")), "-");
        assert_eq!(
            format_archived_at(Some("2026-10-01T10:20:30.123456")),
            "2026-10-01 10:20"
        );
        assert_eq!(format_archived_at(Some("boom")), "boom");
    }

    /// `archived_at` 是索引文件里的任意字符串：按字节切会在多字节字符中间
    /// panic（16 字节落在 `語` 的中间那一位）。
    #[test]
    fn archived_at_truncates_on_char_boundaries() {
        let long = "日本語".repeat(7); // 21 字符 / 63 字节
        assert_eq!(
            format_archived_at(Some(&long)),
            "日本語日本語日本語日本語日本語日" // 前 16 个字符
        );
        // 不足 16 字符的短串原样带出、不 panic。
        assert_eq!(format_archived_at(Some("日本語日本語日")), "日本語日本語日");
        assert_eq!(format_archived_at(Some("abcdefghijklm")), "abcdefghijklm");
    }

    fn entry(id: &str, name: Option<&str>, archived_at: Option<&str>) -> ArchivedSession {
        ArchivedSession {
            id: id.to_string(),
            name: name.map(str::to_string),
            archived_at: archived_at.map(str::to_string),
            tags: vec!["smoke".to_string()],
            model_display_name: Some("DeepSeek Flash".to_string()),
            ..ArchivedSession::default()
        }
    }

    fn plain_out() -> crate::cmd::common::TableOutput {
        crate::cmd::common::TableOutput {
            width: 120,
            color: false,
        }
    }

    #[test]
    fn archive_list_renders_entries_and_never_panics_on_weird_timestamps() {
        let palette = crate::config::ThemePalette::default();
        let rendered = format_archive_list(
            &[
                entry("old-1", Some("old chat"), Some("2026-10-01T10:20:30.5")),
                entry("old-2", None, Some("日本語日本語日本語日")),
            ],
            &palette,
            &plain_out(),
        );
        assert!(rendered.contains("old-1"), "{rendered}");
        assert!(rendered.contains("old chat"), "{rendered}");
        assert!(rendered.contains("2026-10-01 10:20"), "{rendered}");
        assert!(rendered.contains("DeepSeek Flash"), "{rendered}");
        assert!(rendered.contains("smoke"), "{rendered}");
        assert!(rendered.contains("old-2"), "{rendered}");
        assert!(!rendered.contains('\u{1b}'), "colour off must stay plain");

        // 空列表是一个稳定的句子（不是空输出）。
        assert_eq!(
            format_archive_list(&[], &palette, &plain_out()),
            "No archived sessions.\n"
        );
    }

    #[test]
    fn results_table_carries_action_and_detail() {
        let results = vec![
            ArchiveResult {
                session_id: "s1".into(),
                archived: true,
                changed: true,
                detail: "archived".into(),
                is_error: false,
            },
            ArchiveResult {
                session_id: "s2".into(),
                archived: true,
                changed: false,
                detail: "already archived".into(),
                is_error: false,
            },
        ];
        let rendered = format_results(&results, false);
        assert!(rendered.starts_with("SESSION ID"), "{rendered}");
        assert!(
            rendered.contains("archived") && rendered.contains("skipped"),
            "{rendered}"
        );
        assert!(rendered.contains("already archived"), "{rendered}");
        assert!(
            format_results(&results, true).contains("restored"),
            "restore 方向结果是 restored"
        );
    }
}
