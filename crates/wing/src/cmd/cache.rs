//! `wing cache` — 会话的 KV 前缀缓存账本（理论 vs 实际命中率 + 分叉归因）。
//!
//! 人类视图**只列可行动的分叉**：`cold` / `block_granularity` 这类"前缀其实延续了"
//! 的判定不出现在表里（契约 §7）——它们不是可修的东西，列出来只会稀释真正的行。
//! 每次分叉行点名「第几段、哪个 label、该怪谁、能不能避开、代价多少」，`--why` 再展开
//! 单条的 sha 对照与来源文件。
//!
//! ```sh
//! wing cache                      # 最近活跃会话（表头会打印解析出的 id）
//! wing cache "$SID" --limit 50
//! wing cache "$SID" --why 1       # 展开表格第一行
//! wing cache "$SID" --why 10-18\ 15:41:02
//! wing cache "$SID" --json        # 端点响应 + resolved_session_id
//! ```
//!
//! 会话不存在 / 无账本时网关回 200 + 空账本（契约 §6），所以这里的 404 只有一个含义：
//! 运行中的网关还没有这个路由（见 [`cache_error`]）。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use chrono::NaiveDateTime;
use ratatui::style::Style;
use serde::Serialize;
use wing_api_client::ApiClientError;
use wing_api_client::models::{CacheFork, CacheReportResponse, SessionInfo};

use crate::config::AppConfig;
use crate::config::ThemePalette;
use crate::render::table::ColumnKind;
use crate::render::table::plain::{PlainCell, PlainColumn, PlainTable};
use crate::util::format;

use super::common;
use super::common::TableOutput;

/// `--limit` 的默认值（与端点默认一致）。
pub const DEFAULT_LIMIT: u32 = 20;
/// `--limit` 的上界（端点契约 §6：1..=200）。
pub const MAX_LIMIT: u32 = 200;

/// `ts`（本地 naive ISO，毫秒）的解析与展示形态。
const TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.f";
/// 表格 WHEN 列的形态：`MM-DD HH:MM:SS`（`--why` 接受拷贝这一列）。
const WHEN_FORMAT: &str = "%m-%d %H:%M:%S";
/// 「没有值」的唯一记号（不写 0：0 是一个读数，`—` 是"没有读数"）。
const NONE_MARK: &str = "—";

/// Entry point for `wing cache`.
pub async fn run(session_id: Option<&str>, limit: u32, why: Option<&str>, json: bool) -> ExitCode {
    match run_inner(session_id, limit, why, json).await {
        Ok(code) => code,
        Err(e) => common::fail("cache", e),
    }
}

async fn run_inner(
    session_id: Option<&str>,
    limit: u32,
    why: Option<&str>,
    json: bool,
) -> Result<ExitCode> {
    validate_limit(limit)?;
    validate_why_json(why, json)?;

    let http = common::connect().await?;

    // 省略 session：取最近活跃会话，并把解析结果打进表头（不允许静默猜）。
    let resolved = match session_id {
        Some(id) => id.to_string(),
        None => {
            let resp = http.list_sessions().await?;
            match pick_latest_session(&resp.sessions) {
                Some(session) => session.id.clone(),
                None if resp.sessions.is_empty() => anyhow::bail!(
                    "no sessions found; pass a SESSION_ID (`wing ps --all` lists them)"
                ),
                None => anyhow::bail!(
                    "no session reports a usable last-interaction time; pass a \
                     SESSION_ID (`wing ps --all` lists them)"
                ),
            }
        }
    };

    let report = http
        .get_cache_report(&resolved, limit)
        .await
        .map_err(|e| cache_error(&resolved, &e))?;

    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = TableOutput::detect();

    if json {
        common::print_json_compact(&CacheJson {
            report: &report,
            resolved_session_id: &resolved,
        });
    } else if let Some(selector) = why {
        print!("{}", format_why(&report, selector)?);
    } else {
        print!("{}", format_report(&report, &resolved, &palette, &out));
    }
    Ok(ExitCode::SUCCESS)
}

/// `--json` 的输出：端点响应**原样**（`flatten`，一个字段都不丢）+ 解析出的会话 id。
#[derive(Serialize)]
struct CacheJson<'a> {
    #[serde(flatten)]
    report: &'a CacheReportResponse,
    resolved_session_id: &'a str,
}

/// `--limit` 的闸门：1..=200（端点的契约范围）。
fn validate_limit(limit: u32) -> Result<()> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        anyhow::bail!("--limit must be between 1 and {MAX_LIMIT} (got {limit})");
    }
    Ok(())
}

/// `--why` 是人类的单条展开视图，`--json` 是整份机器面：同时给等于让一边静默失效。
fn validate_why_json(why: Option<&str>, json: bool) -> Result<()> {
    if why.is_some() && json {
        anyhow::bail!(
            "--why expands one fork for humans; it cannot be combined with --json \
             (the JSON report already carries every fork field)"
        );
    }
    Ok(())
}

/// 最近活跃的会话：`last_interaction` 最大者。
///
/// 只让**能解析**的时间戳参与竞争——未知时间不冒充"最新"（宁可报错让用户显式给 id，
/// 也不替他猜一个会话去读）。时间并列时保留列表序靠前的那个（`/api/session/list`
/// 本身按活跃优先下发）。一个能解析的都没有 → `None`。
fn pick_latest_session(sessions: &[SessionInfo]) -> Option<&SessionInfo> {
    let mut best: Option<(NaiveDateTime, &SessionInfo)> = None;
    for session in sessions {
        let Some(at) = session.last_interaction.as_deref().and_then(parse_ts) else {
            continue;
        };
        if best.is_none_or(|(current, _)| at > current) {
            best = Some((at, session));
        }
    }
    best.map(|(_, session)| session)
}

fn parse_ts(raw: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(raw, TS_FORMAT).ok()
}

/// `session <id>: <gateway error>`，404/405 额外说明"网关太旧"（契约 §6 之后 404 只剩
/// 这一个含义：会话不存在 / 无账本都是 200 + 空账本）。
fn cache_error(session_id: &str, error: &ApiClientError) -> anyhow::Error {
    match error {
        ApiClientError::Api {
            status: 404 | 405, ..
        } => anyhow::anyhow!(
            "session {session_id}: {error} — the running gateway does not serve \
             /api/session/cache (does it predate the cache ledger? restart it: \
             wing stop && wing start)"
        ),
        _ => common::session_failed(session_id, error),
    }
}

// ============================================================
// 人类视图：头两行 + 分叉表
// ============================================================

/// 渲染整份人类视图（纯函数：给定报告与输出环境 → 文本，便于快照式断言）。
fn format_report(
    report: &CacheReportResponse,
    session_id: &str,
    palette: &ThemePalette,
    out: &TableOutput,
) -> String {
    let forks = human_forks(&report.forks);
    let latest = report.calls.first();
    let mut text = format!(
        "CACHE  {session_id} · {} · {}\n  {}\n",
        header_field(latest.map(|call| call.model.as_str())),
        header_field(latest.map(|call| call.purpose.as_str())),
        summary_line(report),
    );

    if forks.is_empty() {
        let hidden = report.forks.len();
        text.push('\n');
        text.push_str(&empty_line(report, hidden));
        text.push('\n');
        return text;
    }

    let columns = vec![
        PlainColumn::new("WHEN", ColumnKind::Compact),
        PlainColumn::capped("WHAT", ColumnKind::Narrative, 80),
        PlainColumn::new("BLAME", ColumnKind::Compact),
        PlainColumn::new("AVOIDABLE", ColumnKind::Compact),
        PlainColumn::new("COST", ColumnKind::Compact),
    ];
    let rows = forks
        .iter()
        .map(|fork| {
            vec![
                PlainCell::styled(when_text(&fork.ts), Style::new().fg(palette.dim)),
                PlainCell::styled(what_text(fork), Style::new().fg(palette.text)),
                PlainCell::styled(blame_text(fork), Style::new().fg(palette.tool_result)),
                PlainCell::styled(avoidable_text(fork), avoidable_style(fork, palette)),
                PlainCell::styled(cost_text(fork), Style::new().fg(palette.tool_result)),
            ]
        })
        .collect();
    let table = PlainTable { columns, rows };
    text.push('\n');
    text.push_str(&common::render_table(&table, palette, out));
    text
}

/// 人类视图只列「前缀真的断了」的行。
///
/// 契约 §7 的硬要求（`cold` / `block_granularity` 绝不出现）。它们按契约 §4 本来
/// 就不会带上 `fork`，`verdict == ""`（契约 §9：未计算）也不该给结论，这道过滤是
/// **兜底**：网关若把非分叉判定也塞进 `forks`，稀释掉的正是用户要读的那几行。
fn human_forks(forks: &[CacheFork]) -> Vec<&CacheFork> {
    forks
        .iter()
        .filter(|fork| {
            !matches!(
                fork.verdict.as_str(),
                "" | "cold" | "cold_expired" | "block_granularity" | "hit"
            )
        })
        .collect()
}

fn empty_line(report: &CacheReportResponse, hidden: usize) -> String {
    if report.calls.is_empty() && report.forks.is_empty() && report.summary.calls == 0 {
        "No cache data yet (the ledger starts with the session's first LLM call).".to_string()
    } else if hidden > 0 {
        // 有分叉但都不该展示：说清是「没有可行动的」，而不是「账本说没分叉」。
        // 文案里不点名判定（`cold` / `block_granularity` 这些词在人类视图里一律不出现）。
        format!("No actionable cache forks ({hidden} row(s) hidden: none names a broken prefix).")
    } else {
        "No cache forks (no call diverged from the previous prefix).".to_string()
    }
}

/// 头两行的第二行：三率 + 调用条数（契约 §7 的样列）。
fn summary_line(report: &CacheReportResponse) -> String {
    let summary = &report.summary;
    format!(
        "理论命中率 {}   实际 {}   provider 欠账 {}   （近 {} 次调用）",
        percent_text(summary.theory_rate),
        percent_text(summary.actual_rate),
        percent_text(summary.provider_debt_rate),
        summary.calls,
    )
}

/// 头一行里的 model / purpose 取**最近一次调用**（账本新→旧）；无调用 → `-`。
fn header_field(value: Option<&str>) -> String {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => common::single_line(value),
        None => "-".to_string(),
    }
}

/// 分数 → `88.1%`；`None` / 非有限值 → `—`（不显示 0%，契约 §9）。
fn percent_text(rate: Option<f64>) -> String {
    match rate {
        Some(rate) if rate.is_finite() => {
            format!("{:.1}%", (rate * 100.0).clamp(0.0, 100.0))
        }
        _ => NONE_MARK.to_string(),
    }
}

/// `ts` → `MM-DD HH:MM:SS`；不可解析时原样带出（压成一行，别撕开表格）。
fn when_text(ts: &str) -> String {
    match parse_ts(ts) {
        Some(at) => at.format(WHEN_FORMAT).to_string(),
        None if ts.is_empty() => NONE_MARK.to_string(),
        None => common::single_line(ts),
    }
}

/// WHAT 列：分叉符号 + label（`+` 插入 / `-` 移除 / `~` 修改）。
fn what_text(fork: &CacheFork) -> String {
    let sigil = match fork.kind.as_str() {
        "insert" => '+',
        "remove" => '-',
        "modify" => '~',
        _ => '?',
    };
    let label = if fork.label.is_empty() {
        NONE_MARK.to_string()
    } else {
        common::single_line(&fork.label)
    };
    format!("{sigil} {label}")
}

fn blame_text(fork: &CacheFork) -> String {
    match fork
        .blame
        .as_deref()
        .map(str::trim)
        .filter(|blame| !blame.is_empty())
    {
        Some(blame) => common::single_line(blame),
        None => NONE_MARK.to_string(),
    }
}

/// `avoidable` 三态：`true` = YES（用户能修）、`false` = no、`null` = `—`（该归因不适用）。
fn avoidable_text(fork: &CacheFork) -> String {
    match fork.avoidable {
        Some(true) => "YES".to_string(),
        Some(false) => "no".to_string(),
        None => NONE_MARK.to_string(),
    }
}

fn avoidable_style(fork: &CacheFork, palette: &ThemePalette) -> Style {
    let color = match fork.avoidable {
        Some(true) => palette.warning,
        _ => palette.dim,
    };
    Style::new().fg(color)
}

/// COST 列：`≈-` + 既有 token 记法（`tokens_lost` 是**丢失量**，所以符号由这一列
/// 的语义给出；`format::tokens` 对负数会退化成裸数字，因此取绝对值自己拼符号）。
/// 无参照（`tokens_lost == null`）→ `—`。
fn cost_text(fork: &CacheFork) -> String {
    match fork.tokens_lost {
        Some(lost) => format!("≈-{}", format::tokens(lost.abs())),
        None => NONE_MARK.to_string(),
    }
}

// ============================================================
// --why：单条分叉的展开
// ============================================================

/// `--why` 的选择子：纯数字 = 人类表格里的 **1-based 行序**，其余按 `ts` 匹配。
enum WhySelector<'a> {
    Ordinal(usize),
    Ts(&'a str),
}

fn parse_selector(raw: &str) -> WhySelector<'_> {
    let trimmed = raw.trim();
    if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
        // 全数字串：`u64` 解析失败（超长）也不会误判成 ts。
        if let Ok(value) = trimmed.parse::<usize>() {
            return WhySelector::Ordinal(value);
        }
    }
    WhySelector::Ts(trimmed)
}

/// 渲染一条分叉的展开视图（第一行带头部，其后是逐字面对照）。
fn format_why(report: &CacheReportResponse, raw: &str) -> Result<String> {
    let forks = human_forks(&report.forks);
    let (ordinal, fork) = select_fork(&forks, raw)?;
    let detail = &fork.detail;

    // 参照那次调用的段数：契约 §4 里 `segments_affected = len(C) - k`，
    // 故 `len(C) = index + segments_affected`。
    let total_segments = fork.index.max(0) + fork.segments_affected.max(0);
    let sha = match (detail.sha_before.as_deref(), detail.sha_after.as_deref()) {
        (None, None) => NONE_MARK.to_string(),
        (before, after) => format!(
            "{} → {}",
            before.unwrap_or(NONE_MARK),
            after.unwrap_or(NONE_MARK)
        ),
    };
    let mtime = match (detail.mtime.as_deref(), detail.size) {
        (None, None) => NONE_MARK.to_string(),
        (mtime, size) => {
            let mtime = mtime.unwrap_or(NONE_MARK);
            match size {
                Some(size) => format!("{mtime} ({size} bytes)"),
                None => mtime.to_string(),
            }
        }
    };

    let mut out = String::new();
    out.push_str(&format!(
        "FORK  #{ordinal}  {} ({})\n",
        what_text(fork),
        header_field(Some(fork.kind.as_str())),
    ));
    let mut fields = vec![
        ("segment", format!("{} / {total_segments}", fork.index)),
        ("sha", sha),
        (
            "source",
            detail.path.clone().unwrap_or_else(|| NONE_MARK.to_string()),
        ),
        ("mtime", mtime),
        ("cost", cost_text(fork)),
        (
            "verdict",
            if fork.verdict.is_empty() {
                NONE_MARK.to_string()
            } else {
                fork.verdict.clone()
            },
        ),
        (
            "blame",
            format!("{} (avoidable: {})", blame_text(fork), avoidable_text(fork)),
        ),
        ("when", when_text(&fork.ts)),
    ];
    // 对照侧的调用已经滚出账本（后端给 null）时整行不显示：写成 `—` 会被读成
    // 「sha 就是这一刻变的」，而那正是后端拒绝猜的那件事。
    if let Some(ref_ts) = detail.ref_ts.as_deref() {
        fields.push(("ref", when_text(ref_ts)));
    }
    for (label, value) in fields {
        out.push_str(&format!("  {label:<9}{value}\n"));
    }
    Ok(out)
}

/// 选择子 → (`1-based` 行序, fork)。
///
/// 三种匹配口径（顺序即优先级）：行序、`ts` 精确、表格里显示的 `MM-DD HH:MM:SS`、
/// `ts` 的唯一前缀。多义时报错而不替用户挑一条。
fn select_fork<'a>(forks: &[&'a CacheFork], raw: &str) -> Result<(usize, &'a CacheFork)> {
    if raw.is_empty() {
        anyhow::bail!("--why needs a value: a fork's row ordinal (1 = newest) or its timestamp");
    }
    match parse_selector(raw) {
        WhySelector::Ordinal(ordinal) => {
            if ordinal == 0 || ordinal > forks.len() {
                anyhow::bail!(
                    "--why {ordinal} is out of range: the table lists {} fork(s) \
                     (ordinals are 1-based row positions, newest first)",
                    forks.len()
                );
            }
            Ok((ordinal, forks[ordinal - 1]))
        }
        WhySelector::Ts(needle) => {
            let exact: Vec<usize> = forks
                .iter()
                .enumerate()
                .filter(|(_, fork)| fork.ts == needle || when_text(&fork.ts) == needle)
                .map(|(index, _)| index)
                .collect();
            let hits = if exact.is_empty() {
                forks
                    .iter()
                    .enumerate()
                    .filter(|(_, fork)| !needle.is_empty() && fork.ts.starts_with(needle))
                    .map(|(index, _)| index)
                    .collect()
            } else {
                exact
            };
            match hits.as_slice() {
                [] => anyhow::bail!(
                    "no fork matches --why {needle} (the table lists {} fork(s); \
                     `wing cache --json` shows the raw timestamps of every fork)",
                    forks.len()
                ),
                [index] => Ok((index + 1, forks[*index])),
                _ => anyhow::bail!(
                    "--why {needle} is ambiguous: {} table rows match; use a longer \
                     timestamp prefix or the row ordinal",
                    hits.len()
                ),
            }
        }
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wing_api_client::models::CacheCall;
    use wing_api_client::models::CacheForkDetail;
    use wing_api_client::models::CacheSummary;

    fn fork_by(
        kind: &str,
        index: i64,
        label: &str,
        verdict: &str,
        tokens_lost: Option<i64>,
    ) -> CacheFork {
        CacheFork {
            ts: "2026-10-18T15:41:02.123".into(),
            index,
            label: label.into(),
            kind: kind.into(),
            blame: Some("external".into()),
            avoidable: Some(true),
            tokens_lost,
            segments_affected: 12,
            verdict: verdict.into(),
            detail: CacheForkDetail {
                path: Some("/home/u/.wing/core/skills/pdf-tools".into()),
                mtime: Some("2026-10-18T15:40:58.010".into()),
                size: Some(1234),
                ref_ts: Some("2026-10-18T15:39:00.500".into()),
                sha_before: Some("9f2c1a3b4d5e6f70".into()),
                sha_after: Some("8e1b2c3d4e5f6071".into()),
            },
        }
    }

    fn call(ts: &str, model: &str, verdict: &str, expected: Option<i64>) -> CacheCall {
        CacheCall {
            ts: ts.into(),
            model: model.into(),
            purpose: "main".into(),
            prompt: 14_200,
            cached: 0,
            expected,
            verdict: verdict.into(),
            fp: "ab12cd34ef567890".into(),
        }
    }

    /// 一份贴近真实的报告：两条可行动分叉 + `calls` 里混着 `cold` / `hit` /
    /// `block_granularity`（它们绝不能变成人类视图的行）。
    fn report() -> CacheReportResponse {
        CacheReportResponse {
            session_id: "20261018-154102-9f3a4b2c".into(),
            summary: CacheSummary {
                calls: 42,
                prompt_tokens: 590_000,
                cached_tokens: 484_000,
                expected_tokens: 520_000,
                theory_rate: Some(0.881),
                actual_rate: Some(0.820),
                provider_debt_rate: Some(0.061),
            },
            forks: vec![
                fork_by(
                    "insert",
                    3,
                    "skills:pdf-tools",
                    "prefix_broken",
                    Some(14_200),
                ),
                CacheFork {
                    // 修改型：`history` 段被压缩改写，归因到 compact（不可避开）。
                    ts: "2026-10-18T14:58:03.900".into(),
                    index: 7,
                    label: "history:9f3a4b2c-1111-2222-3333-444455556666:tool".into(),
                    kind: "modify".into(),
                    blame: Some("compact".into()),
                    avoidable: Some(false),
                    tokens_lost: Some(9_800),
                    segments_affected: 5,
                    verdict: "prefix_broken".into(),
                    detail: CacheForkDetail {
                        path: None,
                        mtime: None,
                        size: None,
                        ref_ts: Some("2026-10-18T14:57:00.000".into()),
                        sha_before: Some("1111222233334444".into()),
                        sha_after: Some("5555666677778888".into()),
                    },
                },
            ],
            calls: vec![
                call(
                    "2026-10-18T15:41:02.123",
                    "deepseek-v3",
                    "prefix_broken",
                    None,
                ),
                call(
                    "2026-10-18T15:21:00.000",
                    "deepseek-v3",
                    "block_granularity",
                    Some(13_800),
                ),
                call("2026-10-18T09:00:00.000", "deepseek-v3", "cold", None),
            ],
        }
    }

    fn table_output() -> TableOutput {
        TableOutput {
            width: common::PIPE_WIDTH,
            color: false,
        }
    }

    fn render(report: &CacheReportResponse) -> String {
        format_report(
            report,
            "20261018-154102-9f3a4b2c",
            &ThemePalette::default(),
            &table_output(),
        )
    }

    /// 一条「脏」分叉：网关若把非分叉判定也塞进 `forks`，它必须被挡在人类视图之外。
    fn dirty_fork(label: &str, verdict: &str) -> CacheFork {
        CacheFork {
            label: label.into(),
            kind: "insert".into(),
            blame: Some("composition".into()),
            avoidable: Some(false),
            ..fork_by("insert", 1, label, verdict, None)
        }
    }

    /// 表体的数据行数（框线表格：`┃` 行减去表头那一行）。
    fn body_rows(text: &str) -> usize {
        text.lines()
            .filter(|line| line.starts_with('┃'))
            .count()
            .saturating_sub(1)
    }

    /// 契约 §7 的硬要求：`cold` / `block_granularity` 绝不出现在人类视图。
    ///
    /// fixture 的 `forks` 里直接塞进这三类脏行——渲染只读 `forks`，所以把
    /// `human_forks` 的过滤删掉，这条测试必红。
    #[test]
    fn the_human_view_never_surfaces_cold_or_block_granularity() {
        let mut report = report();
        let actionable = report.forks.len();
        report.forks.push(dirty_fork("rules:/etc/rules.md", "cold"));
        report
            .forks
            .push(dirty_fork("tools:Bash", "block_granularity"));
        report.forks.push(dirty_fork("system:wire", "hit"));
        report.forks.push(dirty_fork("system:base", "cold_expired"));
        report.forks.push(dirty_fork("tools:Grep", ""));

        let text = render(&report);
        for verdict in [
            "cold",
            "cold_expired",
            "block_granularity",
            "hit",
            "rules:/etc/rules.md",
            "tools:Bash",
            "system:wire",
            "system:base",
        ] {
            assert!(!text.contains(verdict), "人类视图不列 {verdict}：\n{text}");
        }
        // 脏行不占行：表里恰好是 fixture 的可行动分叉数（Grep 那条空判定也被挡掉）。
        assert_eq!(body_rows(&text), actionable, "{text}");
        // 可行动的照常在（过滤没有把整张表吃掉）。
        assert!(text.contains("+ skills:pdf-tools"), "{text}");
        assert!(text.contains("~ history:9f3a4b2c"), "{text}");

        // `--why` 的序位同样只数用户看到的行：第 2 行仍是 compact 那条。
        let why = format_why(&report, "2").unwrap();
        assert!(why.contains("~ history:"), "{why}");
        assert!(format_why(&report, "3").is_err(), "脏行不占序位");
    }

    /// 全部被过滤时说实话：是「没有可行动的」，不是「账本说没分叉」。
    #[test]
    fn a_report_with_only_non_actionable_forks_says_so() {
        let mut report = report();
        report.forks = vec![
            dirty_fork("rules:/etc/rules.md", "cold"),
            dirty_fork("tools:Bash", "block_granularity"),
        ];
        let text = render(&report);
        assert!(
            text.contains("No actionable cache forks (2 row(s) hidden"),
            "{text}"
        );
        assert!(!text.contains("block_granularity"), "{text}");
        assert!(!text.contains("cold"), "{text}");
    }

    #[test]
    fn the_human_view_prints_the_resolved_session_and_the_three_rates() {
        let text = render(&report());
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("CACHE  20261018-154102-9f3a4b2c · deepseek-v3 · main"),
            "表头第一行：解析出的 id · model · purpose"
        );
        assert_eq!(
            lines.next(),
            Some("  理论命中率 88.1%   实际 82.0%   provider 欠账 6.1%   （近 42 次调用）"),
            "{text}"
        );
        // 分叉表紧随其后：表头列名齐全。
        for column in ["WHEN", "WHAT", "BLAME", "AVOIDABLE", "COST"] {
            assert!(text.contains(column), "缺列 {column}：\n{text}");
        }
    }

    /// COST = `≈-` + 既有 token 记法（契约 §7 样列的 `≈-14.2k`）；无参照 → `—`。
    #[test]
    fn the_cost_column_uses_the_shared_token_notation() {
        let mut report = report();
        report.forks[0].tokens_lost = Some(14_200);
        report.forks[1].tokens_lost = None;
        let text = render(&report);
        assert!(text.contains("≈-14.2k"), "{text}");
        assert!(text.contains(NONE_MARK), "{text}");
        assert!(
            !text.contains("≈-0") && !text.contains("≈0"),
            "没有参照时不许编 0：\n{text}"
        );
        // 负的 tokens_lost（不该发生）也不许退化成裸数字 `-14200`。
        report.forks[0].tokens_lost = Some(-14_200);
        assert!(render(&report).contains("≈-14.2k"), "{text}");
    }

    /// 窄终端不撕行：表体每一行等宽（`render/table/plain` 的截断不越预算）。
    #[test]
    fn a_narrow_terminal_still_renders_a_closed_grid() {
        let out = TableOutput {
            width: 60,
            color: false,
        };
        let text = format_report(
            &report(),
            "20261018-154102-9f3a4b2c",
            &ThemePalette::default(),
            &out,
        );
        let grid: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with(['┏', '┃', '┣', '┠', '┗']))
            .collect();
        assert!(grid.len() >= 5, "{text}");
        let widths: Vec<usize> = grid
            .iter()
            .map(|line| unicode_width::UnicodeWidthStr::width(*line))
            .collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "{widths:?}"
        );
        assert!(widths[0] <= 60, "不超预算：{widths:?}");
    }

    /// 三率缺失（分母为 0）时显示 `—`，不显示 `0.0%`。
    #[test]
    fn absent_rates_render_as_a_dash_not_zero() {
        let mut report = report();
        report.summary.theory_rate = None;
        report.summary.actual_rate = None;
        report.summary.provider_debt_rate = None;
        report.summary.calls = 0;
        let text = render(&report);
        assert!(
            text.contains("理论命中率 —   实际 —   provider 欠账 —   （近 0 次调用）"),
            "{text}"
        );
        assert!(!text.contains("0.0%"), "{text}");
    }

    /// 空账本：说明"还没有数据"，而不是打一张只有表头的空表。
    #[test]
    fn an_empty_ledger_explains_itself() {
        let empty = CacheReportResponse {
            session_id: "sid".into(),
            ..CacheReportResponse::default()
        };
        let text = render(&empty);
        assert!(text.contains("CACHE  "), "{text}");
        assert!(text.contains("No cache data yet"), "{text}");
        assert!(!text.contains("WHEN"), "空账本不打表：\n{text}");
    }

    #[test]
    fn fork_rows_only_without_forks_explain_themselves() {
        let mut report = report();
        report.forks.clear();
        let text = render(&report);
        assert!(text.contains("No cache forks"), "{text}");
    }

    /// `--why` 展开：`index / 总段数`、label、`sha_before → sha_after`、来源文件、mtime、
    /// tokens_lost 全在（契约 §7 的逐项）。
    #[test]
    fn why_expands_one_fork_with_its_segment_comparison() {
        let text = format_why(&report(), "1").unwrap();
        assert!(text.starts_with("FORK  #1  + skills:pdf-tools"), "{text}");
        // 3 / (3 + 12) —— 参照那次调用的第 3 段 / 共 15 段。
        assert!(text.contains("3 / 15"), "{text}");
        assert!(
            text.contains("9f2c1a3b4d5e6f70 → 8e1b2c3d4e5f6071"),
            "sha 对照：\n{text}"
        );
        assert!(
            text.contains("/home/u/.wing/core/skills/pdf-tools"),
            "{text}"
        );
        assert!(text.contains("2026-10-18T15:40:58.010"), "{text}");
        assert!(text.contains("≈-14.2k"), "{text}");
        assert!(text.contains("external"), "{text}");
        assert!(text.contains("avoidable: YES"), "{text}");
        // 人类的 WHEN 形态（表格里也是这一列）。
        assert!(text.contains("10-18 15:41:02"), "{text}");
        // 有对照侧 → `ref` 行在。
        assert!(text.contains("\n  ref      "), "{text}");
    }

    /// 对照侧已滚出账本（后端给 `ref_ts: null`）→ **整行不显示**（不拿 `—` 冒充）。
    #[test]
    fn why_hides_the_reference_row_when_the_comparison_side_is_gone() {
        let mut report = report();
        report.forks[0].detail.ref_ts = None;
        let text = format_why(&report, "1").unwrap();
        assert!(
            !text.contains("\n  ref "),
            "null 的 ref 不该占一行：\n{text}"
        );
        // 其余行照旧
        assert!(text.contains("\n  sha      "), "{text}");
        assert!(text.contains("\n  when     "), "{text}");
    }

    #[test]
    fn why_accepts_the_when_column_and_timestamp_prefixes() {
        // 表格里拷贝出来的 WHEN 列。
        let by_when = format_why(&report(), "10-18 14:58:03").unwrap();
        assert!(by_when.contains("~ history:"), "{by_when}");
        // `ts` 前缀（唯一）。
        let by_prefix = format_why(&report(), "2026-10-18T14:58").unwrap();
        assert!(by_prefix.contains("~ history:"), "{by_prefix}");
        // 原始 `ts` 全量。
        let by_ts = format_why(&report(), "2026-10-18T14:58:03.900").unwrap();
        assert!(by_ts.contains("FORK  #2"), "{by_ts}");
    }

    #[test]
    fn why_rejects_out_of_range_and_unknown_selectors() {
        let out_of_range = format_why(&report(), "3").unwrap_err().to_string();
        assert!(
            out_of_range.contains("out of range") && out_of_range.contains("2 fork(s)"),
            "{out_of_range}"
        );
        // 0 不是合法行序（1-based：不替用户猜"那就是第一条"）。
        assert!(format_why(&report(), "0").is_err());
        let unknown = format_why(&report(), "10-19").unwrap_err().to_string();
        assert!(unknown.contains("no fork matches"), "{unknown}");
    }

    /// 多义前缀不替用户选：报 ambiguous。
    #[test]
    fn why_refuses_to_guess_between_two_matches() {
        let mut report = report();
        report.forks[1].ts = "2026-10-18T15:41:02.999".into();
        let err = format_why(&report, "2026-10-18T15:41")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ambiguous"), "{err}");
    }

    /// `--why` 只看见人类视图里的行：被过滤掉的行不占行序。
    #[test]
    fn why_ordinals_count_the_rows_the_user_sees() {
        let mut report = report();
        report
            .forks
            .insert(0, fork_by("insert", 1, "tools:Bash", "cold", None));
        let text = format_why(&report, "1").unwrap();
        assert!(
            text.contains("+ skills:pdf-tools"),
            "第 1 行是用户看到的第一条：\n{text}"
        );
    }

    #[test]
    fn json_passthrough_keeps_every_field_and_the_resolved_id() {
        let report = report();
        let payload = CacheJson {
            report: &report,
            resolved_session_id: "20261018-154102-9f3a4b2c",
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&payload).unwrap()).unwrap();
        assert_eq!(value["resolved_session_id"], "20261018-154102-9f3a4b2c");
        assert_eq!(value["session_id"], "20261018-154102-9f3a4b2c");
        assert_eq!(value["summary"]["theory_rate"], json!(0.881));
        assert_eq!(value["summary"]["expected_tokens"], json!(520_000));
        assert_eq!(
            value["forks"][0]["detail"]["sha_before"],
            "9f2c1a3b4d5e6f70"
        );
        assert_eq!(value["forks"][0]["tokens_lost"], json!(14_200));
        assert_eq!(value["forks"][1]["avoidable"], json!(false));
        assert_eq!(value["calls"][0]["verdict"], "prefix_broken");
        assert_eq!(value["calls"][1]["expected"], json!(13_800));
    }

    // ---- 最近活跃会话 ----

    fn session(id: &str, last: Option<&str>) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: None,
            created_at: None,
            template_name: None,
            workspace: None,
            parent_session_id: None,
            last_interaction: last.map(str::to_string),
            status: "idle".into(),
            tags: vec![],
            tag_meta: Default::default(),
            model_id: None,
            model_name: None,
            provider_name: None,
            model_display_name: None,
        }
    }

    #[test]
    fn the_latest_session_wins_and_unknown_times_do_not_pretend_to_be_newest() {
        let sessions = vec![
            session("old", Some("2026-10-17T09:00:00.000")),
            session("unknown", None),
            session("newest", Some("2026-10-18T15:41:02.123")),
            session("middle", Some("2026-10-18T10:00:00")),
        ];
        assert_eq!(
            pick_latest_session(&sessions).map(|s| s.id.as_str()),
            Some("newest")
        );
        // 时间全部未知 → 不猜（None 让调用方报错）。
        let unknown_only = vec![session("a", None), session("b", Some("not-a-timestamp"))];
        assert!(pick_latest_session(&unknown_only).is_none());
        assert!(pick_latest_session(&[]).is_none());
    }

    #[test]
    fn ties_keep_the_list_order() {
        let sessions = vec![
            session("first", Some("2026-10-18T15:41:02.123")),
            session("second", Some("2026-10-18T15:41:02.123")),
        ];
        assert_eq!(
            pick_latest_session(&sessions).map(|s| s.id.as_str()),
            Some("first")
        );
    }

    #[test]
    fn limit_and_why_json_gates_are_explicit() {
        assert!(validate_limit(1).is_ok());
        assert!(validate_limit(MAX_LIMIT).is_ok());
        for bad in [0, MAX_LIMIT + 1] {
            let err = validate_limit(bad).unwrap_err().to_string();
            assert!(err.contains("between 1 and 200"), "{err}");
        }
        let err = validate_why_json(Some("1"), true).unwrap_err().to_string();
        assert!(err.contains("--json"), "{err}");
        assert!(validate_why_json(None, true).is_ok());
        assert!(validate_why_json(Some("1"), false).is_ok());
    }
}
