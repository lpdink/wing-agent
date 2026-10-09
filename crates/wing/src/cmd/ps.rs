//! `wing ps` — list sessions, and `wing info` — session runtime info.
//!
//! `wing ps` lists all sessions (like `docker ps` or `kubectl get pods`).
//! `wing info <sid>` shows detailed runtime state for a single session.
//!
//! The human-readable session list is rendered through the shared table
//! engine (`render::table`) with the same skin as TUI markdown tables; every
//! measurement is display columns (CJK-safe). `--json` is the machine surface
//! and is left untouched.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use chrono::Local;
use chrono::NaiveDateTime;
use ratatui::style::Style;
use wing_api_client::models::{SessionInfo, SessionInfoResponse};

use crate::config::AppConfig;
use crate::config::ThemePalette;
use crate::render::table::ColumnKind;
use crate::render::table::plain::{PlainCell, PlainColumn, PlainTable};
use crate::shared::pinning::{is_pinned, pin_added_at, pin_key};

use super::common;
use super::common::TableOutput;

/// Entry point for `wing ps`.
pub async fn run_ps(all: bool, tags: &[String], json: bool, watch: bool) -> ExitCode {
    if watch {
        return run_ps_watch(all, tags, json).await;
    }
    match fetch_sessions().await {
        Ok(sessions) => {
            let mut filtered = filter_sessions(sessions, all, tags);
            order_sessions(&mut filtered);
            emit_sessions(&filtered, json);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing ps error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Watch mode: clear screen and reprint every 2 seconds.
/// Creates the HTTP client once, then polls in a loop. The palette and the
/// output environment are resolved once too — they do not change mid-watch.
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
    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = TableOutput::detect();

    loop {
        match http.list_sessions().await {
            Ok(resp) => {
                let mut filtered = filter_sessions(resp.sessions, all, tags);
                order_sessions(&mut filtered);
                // Clear screen.
                print!("\x1b[2J\x1b[H");
                emit_sessions_with(&filtered, json, &palette, &out);
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

/// Human/JSON output for one session snapshot (`--json` stays the machine
/// surface; the table only serves humans).
fn emit_sessions(sessions: &[SessionInfo], json: bool) {
    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = TableOutput::detect();
    emit_sessions_with(sessions, json, &palette, &out);
}

fn emit_sessions_with(
    sessions: &[SessionInfo],
    json: bool,
    palette: &ThemePalette,
    out: &TableOutput,
) {
    if json {
        common::print_json_compact(sessions);
        return;
    }
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }
    let now = Local::now().naive_local();
    print!("{}", format_sessions_table(sessions, palette, out, now));
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

/// Render the sessions table (human surface; `now` is injected so the age
/// column is deterministic in tests).
///
/// Inks: NAME carries the primary text colour (it is what the eye scans for),
/// SESSION ID the secondary one (copied on demand), LAST stays the faintest —
/// and STATUS carries the one semantic colour per state.
fn format_sessions_table(
    sessions: &[SessionInfo],
    palette: &ThemePalette,
    out: &TableOutput,
    now: NaiveDateTime,
) -> String {
    let columns = vec![
        PlainColumn::new("SESSION ID", ColumnKind::Compact),
        PlainColumn::new("STATUS", ColumnKind::Compact),
        PlainColumn::new("LAST", ColumnKind::Compact),
        PlainColumn::capped("NAME", ColumnKind::Narrative, 80),
        PlainColumn::capped("TAGS", ColumnKind::Narrative, 40),
    ];
    let rows = sessions
        .iter()
        .map(|s| {
            vec![
                PlainCell::styled(s.id.clone(), Style::new().fg(palette.tool_result)),
                PlainCell::styled(s.status.clone(), status_style(&s.status, palette)),
                PlainCell::styled(
                    format_last_interaction(s.last_interaction.as_deref(), now),
                    Style::new().fg(palette.dim),
                ),
                PlainCell::styled(
                    common::single_line(s.name.as_deref().unwrap_or("-")),
                    Style::new().fg(palette.text),
                ),
                PlainCell::styled(tags_text(&s.tags), Style::new().fg(palette.tool_result)),
            ]
        })
        .collect();

    let table = PlainTable { columns, rows };
    common::render_table(&table, palette, out)
}

/// The one semantic colour per session state.
fn status_style(status: &str, palette: &ThemePalette) -> Style {
    let color = match status {
        "working" => palette.accent,
        "waiting" => palette.warning,
        "idle" => palette.tool_result,
        "inactive" => palette.dim,
        _ => palette.text,
    };
    Style::new().fg(color)
}

/// Tags cell: comma-separated, `-` when empty.
fn tags_text(tags: &[String]) -> String {
    if tags.is_empty() {
        "-".to_string()
    } else {
        tags.join(", ")
    }
}

/// Render `last_interaction` (a naive local timestamp from the backend, e.g.
/// `2026-10-09T21:10:21.047560`) as a compact age — `now` / `42s` / `12m` /
/// `3h` / `2d`, falling back to the event's own `MM-DD` once a week old.
/// Unparseable values pass through as written (the table still shows
/// *something* meaningful).
fn format_last_interaction(ts: Option<&str>, now: NaiveDateTime) -> String {
    let Some(ts) = ts else {
        return "-".to_string();
    };
    match NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f") {
        Ok(dt) => format_age(now.signed_duration_since(dt), dt),
        Err(_) => ts.to_string(),
    }
}

fn format_age(age: chrono::Duration, at: NaiveDateTime) -> String {
    let secs = age.num_seconds().max(0);
    if secs < 5 {
        "now".to_string()
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{}d", secs / 86_400)
    } else {
        at.format("%m-%d").to_string()
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

    // ── 表格渲染（共享引擎 + 显示宽度记账）────────────────────────

    fn naive(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").expect("fixture timestamp")
    }

    #[test]
    fn format_age_boundaries() {
        let at = naive("2026-10-09T21:00:00");
        assert_eq!(format_age(chrono::Duration::seconds(0), at), "now");
        assert_eq!(format_age(chrono::Duration::seconds(4), at), "now");
        assert_eq!(format_age(chrono::Duration::seconds(5), at), "5s");
        assert_eq!(format_age(chrono::Duration::seconds(59), at), "59s");
        assert_eq!(format_age(chrono::Duration::seconds(60), at), "1m");
        assert_eq!(format_age(chrono::Duration::seconds(3599), at), "59m");
        assert_eq!(format_age(chrono::Duration::seconds(3600), at), "1h");
        assert_eq!(format_age(chrono::Duration::seconds(86_399), at), "23h");
        assert_eq!(format_age(chrono::Duration::seconds(86_400), at), "1d");
        assert_eq!(format_age(chrono::Duration::seconds(6 * 86_400), at), "6d");
        // 一周以上：显示事件当天的日期（比不断变大的天数好读）。
        assert_eq!(
            format_age(chrono::Duration::seconds(7 * 86_400), at),
            "10-09"
        );
        // 时钟偏移导致的"未来"时间戳钳到 now，不出负数。
        assert_eq!(format_age(chrono::Duration::seconds(-5), at), "now");
    }

    #[test]
    fn format_last_interaction_parses_backend_timestamps() {
        let now = naive("2026-10-09T21:11:00");
        assert_eq!(format_last_interaction(None, now), "-");
        // 正常形式（带微秒），38s 前。
        assert_eq!(
            format_last_interaction(Some("2026-10-09T21:10:21.047560"), now),
            "38s"
        );
        // 不带小数部分也能解析。
        assert_eq!(
            format_last_interaction(Some("2026-10-09T21:10:00"), now),
            "1m"
        );
        // 解析不了就原样展示（不装作认识）。
        assert_eq!(format_last_interaction(Some("garbage"), now), "garbage");
    }

    #[test]
    fn status_colors_are_semantic() {
        let p = ThemePalette::default();
        assert_eq!(status_style("working", &p).fg, Some(p.accent));
        assert_eq!(status_style("waiting", &p).fg, Some(p.warning));
        assert_eq!(status_style("idle", &p).fg, Some(p.tool_result));
        assert_eq!(status_style("inactive", &p).fg, Some(p.dim));
    }

    /// 表格渲染的硬不变量：所有行等宽（按显示列记账，CJK 安全）、不超终端宽；
    /// 中文名在窄预算下以 `…` 截断而不是把整行推歪（旧实现按"字符数"补齐，
    /// 中文名会把后续列全顶开）。
    #[test]
    fn sessions_table_is_aligned_by_display_width() {
        let mut long_name = session(
            "20261009-210702-217d85f2",
            "working",
            &["executor", "task=a"],
        );
        long_name.name = Some("我们前端的表格渲染虽然不错吧 但是其实我更喜欢包裹起来的感觉".into());
        long_name.last_interaction = Some("2026-10-09T21:10:21.047560".into());
        let sessions = vec![
            long_name,
            session("20261008-123133-3f465dd1", "inactive", &[]),
        ];

        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 100,
            color: false,
        };
        let table = format_sessions_table(&sessions, &palette, &out, naive("2026-10-09T21:11:00"));

        let lines: Vec<&str> = table.lines().collect();
        // 顶框 / 表头 / 表头分隔 / 行 / 行分隔 / 行 / 底框。
        assert_eq!(lines.len(), 7, "{table}");
        assert!(lines[0].starts_with('┏'), "{table}");
        assert!(lines[6].starts_with('┗'), "{table}");

        let widths: Vec<usize> = lines
            .iter()
            .map(|l| unicode_width::UnicodeWidthStr::width(*l))
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert_eq!(widths[0], 100, "{widths:?}");

        // 第一行数据：id / status / 相对时间 / 截断的中文名 / tags。
        assert!(lines[3].contains("20261009-210702-217d85f2"), "{table}");
        assert!(lines[3].contains("working"), "{table}");
        assert!(lines[3].contains("38s"), "{table}");
        assert!(lines[3].contains('…'), "长名应截断：{table}");
        assert!(lines[3].contains("executor, task=a"), "{table}");
        // 第二行：缺省字段回退 `-`，inactive 是展示值。
        assert!(lines[5].contains("inactive"), "{table}");
        assert!(lines[5].contains('-'), "{table}");
    }

    /// 颜色只在要求时出现，且以显示宽度为锚（ANSI 不计宽）。
    #[test]
    fn sessions_table_colors_only_when_asked() {
        let sessions = vec![session("20261009-210702-217d85f2", "working", &[])];
        let palette = ThemePalette::default();
        let now = naive("2026-10-09T21:10:00");

        let plain_out = TableOutput {
            width: 100,
            color: false,
        };
        let plain = format_sessions_table(&sessions, &palette, &plain_out, now);
        assert!(!plain.contains('\u{1b}'), "no ANSI when colour is off");

        let colored_out = TableOutput {
            width: 100,
            color: true,
        };
        let colored = format_sessions_table(&sessions, &palette, &colored_out, now);
        assert!(
            colored.contains('\u{1b}'),
            "ANSI expected when colour is on"
        );
        // 去 ANSI 后与无色版逐字符相同（颜色不改几何）。
        let stripped: String = {
            let mut out = String::new();
            let mut chars = colored.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch == '\u{1b}' {
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(ch);
                }
            }
            out
        };
        assert_eq!(stripped, plain);
    }

    #[test]
    fn newline_in_a_session_name_does_not_break_the_grid() {
        let mut s = session("20261009-210702-217d85f2", "idle", &[]);
        s.name = Some("第一行\n第二行".into());
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 100,
            color: false,
        };
        let table = format_sessions_table(&[s], &palette, &out, naive("2026-10-09T21:11:00"));
        assert!(!table.contains("第一行\n第二行"), "{table}");
        assert!(table.contains("第一行 第二行"), "{table}");
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
