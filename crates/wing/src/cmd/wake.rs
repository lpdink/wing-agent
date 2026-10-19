//! `wing wake` — 定时（心跳）唤醒。
//!
//! 注册一条 `kind=timer` 的唤醒触发器：到点往目标会话投一条普通用户消息
//! （`[wing] scheduled wake (<spec>, #<n>)` + 你的 prompt 原文），目标会话照常
//! 跑一轮。取代"每 30 分钟自己起来看一眼"的心跳姿势。
//!
//! ```sh
//! wing wake --in 30m -p "check the build"
//! wing wake --every 30m -p "poll the queue"
//! wing wake --daily 07:00 --times 7 -p "morning triage"
//! wing wake --list            # 只看还活着的
//! wing wake --list --all      # 含已终结的
//! wing wake --cancel 7
//! ```
//!
//! # 时间语义（与网关同一套，见 `docs/dev/wake.md`）
//!
//! - `--in 30m`：一次性，now+30m 触发；`--at "2026-03-01 09:00"`：一次性，该本地时刻；
//! - `--every 30m`：周期，从 now 起算（不按墙钟对齐）；`--daily 07:00`：周期，下一个本地
//!   07:00（已过则明天）；
//! - `--times N` 只对周期型合法（一次性本来就只触发一次）；
//! - 目标会话缺省 = `WING_SESSION_ID`（agent 的 shell 里有），否则必须 `--to <sid>`。
//!
//! 改时间/改 prompt = 取消旧的再建一条（没有"编辑"语义）。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::NaiveDateTime;
use clap::Args;
use serde::Serialize;
use wing_api_client::models::{WakeCreateRequest, WakeInfo};

use crate::config::{AppConfig, ThemePalette};
use crate::render::table::ColumnKind;
use crate::render::table::plain::{PlainCell, PlainColumn, PlainTable};

use super::common::{self, TableOutput};

/// `wing wake` 的参数。
#[derive(Args, Debug, Clone)]
pub struct WakeArgs {
    /// Fire once after this delay (`30s` / `10m` / `2h` / `1d` / `1h30m`).
    #[arg(long = "in", value_name = "DURATION")]
    pub in_: Option<String>,

    /// Fire once at this local time (`"2026-03-01 09:00"` or ISO).
    #[arg(long, value_name = "TIME")]
    pub at: Option<String>,

    /// Fire every this interval, starting one interval from now.
    #[arg(long, value_name = "DURATION")]
    pub every: Option<String>,

    /// Fire every day at this local time (`HH:MM`, 24h; today if not yet passed).
    #[arg(long, value_name = "HH:MM")]
    pub daily: Option<String>,

    /// Message to send on every fire (required when creating).
    #[arg(short = 'p', long = "prompt", value_name = "TEXT")]
    pub prompt: Option<String>,

    /// How many times to fire (recurring specs only; default: forever).
    #[arg(long, value_name = "N")]
    pub times: Option<i64>,

    /// Target session (default: `WING_SESSION_ID`).
    #[arg(long, value_name = "SID")]
    pub to: Option<String>,

    /// List wake triggers instead of creating one.
    #[arg(long)]
    pub list: bool,

    /// With --list: include finished (done / canceled / failed) triggers.
    #[arg(long)]
    pub all: bool,

    /// Cancel the trigger with this id.
    #[arg(long, value_name = "ID")]
    pub cancel: Option<i64>,
}

/// Output of `wing wake` / `wing watch --cancel` (the created / canceled
/// trigger, verbatim from the gateway).
#[derive(Serialize)]
pub(crate) struct WakeOutput {
    pub wake: WakeInfo,
}

/// Output of `wing wake --cancel` when nothing was canceled.
#[derive(Serialize)]
pub(crate) struct WakeCancelOutput {
    pub id: i64,
    pub canceled: bool,
}

// ============================================================
// 纯函数：时间解析与 spec 组装（可单测，不碰网络）
// ============================================================

/// Parse a duration (`30s` / `10m` / `2h` / `1d` / `1h30m`) into seconds.
///
/// Mirrors the gateway's grammar on purpose: the CLI rejects a typo with a
/// local usage error instead of letting a 400 round-trip carry it.
pub(crate) fn parse_duration(text: &str) -> Result<u64> {
    let raw = text.trim();
    if raw.is_empty() {
        bail!("empty duration (expected e.g. 30s / 10m / 2h / 1d)");
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    let mut seen_any = false;
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        let unit = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => bail!("invalid duration '{text}' (expected e.g. 30s / 10m / 2h / 1d / 1h30m)"),
        };
        if digits.is_empty() {
            bail!("invalid duration '{text}' (a unit must follow a number, e.g. 30m)");
        }
        let amount: u64 = digits
            .parse()
            .with_context(|| format!("invalid duration '{text}'"))?;
        total = total
            .checked_add(amount * unit)
            .with_context(|| format!("duration '{text}' is too large"))?;
        digits.clear();
        seen_any = true;
    }
    if !digits.is_empty() {
        bail!("invalid duration '{text}' (missing unit after '{digits}', e.g. 30m)");
    }
    if !seen_any || total == 0 {
        bail!("invalid duration '{text}' (must be > 0)");
    }
    Ok(total)
}

/// Parse a local wall-clock time (`"2026-03-01 09:00"`, `"2026-03-01T09:00:00"`).
///
/// Naive on purpose: the gateway's timezone is the host's, so the CLI must not
/// guess one either (a converted instant would fire at the wrong wall-clock
/// time on a host in another zone).
pub(crate) fn parse_at(text: &str) -> Result<NaiveDateTime> {
    const FORMATS: [&str; 4] = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ];
    let raw = text.trim();
    for format in FORMATS {
        if let Ok(value) = NaiveDateTime::parse_from_str(raw, format) {
            return Ok(value);
        }
    }
    bail!("invalid --at '{text}' (expected e.g. \"2026-03-01 09:00\")")
}

/// Parse `HH:MM` (24h) into `(hour, minute)`.
pub(crate) fn parse_daily(text: &str) -> Result<(u32, u32)> {
    let raw = text.trim();
    let (hour, minute) = raw
        .split_once(':')
        .with_context(|| format!("invalid --daily '{text}' (expected HH:MM, 24h)"))?;
    let hour: u32 = hour
        .parse()
        .with_context(|| format!("invalid --daily '{text}' (expected HH:MM, 24h)"))?;
    let minute: u32 = minute
        .parse()
        .with_context(|| format!("invalid --daily '{text}' (expected HH:MM, 24h)"))?;
    if hour > 23 || minute > 59 {
        bail!("invalid --daily '{text}' (expected HH:MM in 00:00..=23:59)");
    }
    Ok((hour, minute))
}

/// Which schedule the user asked for (exactly one of the four flags).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Schedule {
    In(String),
    At(String),
    Every(String),
    Daily(String),
}

impl Schedule {
    /// The `spec` string on the wire (the gateway's single source of truth).
    pub(crate) fn spec(&self) -> Result<String> {
        match self {
            // 时长原样透传（网关再校验一次并规范化：`90m` 落成 `1h30m`，一段时长
            // 只有一种写法）。
            Schedule::In(raw) => Ok(format!("in:{}", raw.trim())),
            Schedule::Every(raw) => Ok(format!("every:{}", raw.trim())),
            Schedule::At(raw) => Ok(format!("at:{}", parse_at(raw)?.format("%Y-%m-%dT%H:%M:%S"))),
            Schedule::Daily(raw) => {
                let (hour, minute) = parse_daily(raw)?;
                Ok(format!("daily:{hour:02}:{minute:02}"))
            }
        }
    }

    /// One-shot specs fire once (`--times` is meaningless for them).
    pub(crate) fn is_one_shot(&self) -> bool {
        matches!(self, Schedule::In(_) | Schedule::At(_))
    }
}

/// Pick the schedule out of the parsed flags and validate the combinations
/// clap cannot express.
pub(crate) fn schedule_from_flags(
    in_: Option<&str>,
    at: Option<&str>,
    every: Option<&str>,
    daily: Option<&str>,
) -> Result<Schedule> {
    let chosen: Vec<Schedule> = [
        in_.map(|raw| Schedule::In(raw.to_string())),
        at.map(|raw| Schedule::At(raw.to_string())),
        every.map(|raw| Schedule::Every(raw.to_string())),
        daily.map(|raw| Schedule::Daily(raw.to_string())),
    ]
    .into_iter()
    .flatten()
    .collect();
    match chosen.len() {
        1 => Ok(chosen.into_iter().next().unwrap()),
        0 => bail!(
            "no schedule given; pick one of: --in 30m | --at \"2026-03-01 09:00\" | \
             --every 30m | --daily 07:00"
        ),
        _ => bail!("pick exactly one of --in / --at / --every / --daily"),
    }
}

/// Normalize a `--on` value (`done,ask`).
pub(crate) fn parse_on_events(raw: &str) -> Result<String> {
    let mut kinds: Vec<&str> = Vec::new();
    for part in raw.split(',') {
        let name = part.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        if name != "done" && name != "ask" {
            bail!("invalid --on value '{name}' (expected a comma list of: done,ask)");
        }
        if !kinds.contains(&name.as_str()) {
            kinds.push(if name == "done" { "done" } else { "ask" });
        }
    }
    if kinds.is_empty() {
        bail!("--on must not be empty (expected a comma list of: done,ask)");
    }
    Ok(kinds.join(","))
}

/// The actionable message an explicit command (`watch` / `wake`) gets when it
/// has no target: `WING_SESSION_ID` is absent and `--to` was not given.
pub(crate) const NO_TARGET: &str = "no target session: set WING_SESSION_ID or pass --to <sid>";

/// Explicit-command target resolution: `--to`, else `WING_SESSION_ID`.
pub(crate) fn resolve_target(explicit: Option<&str>) -> Result<String> {
    if let Some(value) = explicit.map(str::trim).filter(|v| !v.is_empty()) {
        return Ok(value.to_string());
    }
    match common::parent_session_id_from_env() {
        Some(session_id) => Ok(session_id),
        None => bail!(NO_TARGET),
    }
}

/// The `POST /api/wake/create` body for a timer wake.
pub(crate) fn timer_request(
    spec: &str,
    prompt: &str,
    times: Option<i64>,
    target: &str,
) -> WakeCreateRequest {
    WakeCreateRequest {
        kind: "timer".into(),
        target_session_id: target.to_string(),
        source_session_id: None,
        on_events: None,
        spec: Some(spec.to_string()),
        prompt: Some(prompt.to_string()),
        times,
    }
}

/// Validate the timer triple (spec / prompt / times) before spending a request.
pub(crate) fn validate_timer(
    schedule: &Schedule,
    prompt: Option<&str>,
    times: Option<i64>,
) -> Result<()> {
    if prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        bail!("a timer wake needs a message: pass -p <TEXT>");
    }
    if let Some(value) = times {
        if value < 1 {
            bail!("--times must be >= 1 (got {value})");
        }
        if schedule.is_one_shot() {
            bail!(
                "--times only applies to --every / --daily; {} fires once",
                schedule.spec().unwrap_or_default()
            );
        }
    }
    // 语法在本机先过一遍（含时长）：用户不必为一个手误等一次网关往返。
    match schedule {
        Schedule::In(raw) | Schedule::Every(raw) => {
            parse_duration(raw)?;
        }
        Schedule::At(_) | Schedule::Daily(_) => {}
    }
    schedule.spec()?;
    Ok(())
}

/// 本次调用有没有给时间参数（四种之一）。
fn has_schedule(args: &WakeArgs) -> bool {
    args.in_.is_some() || args.at.is_some() || args.every.is_some() || args.daily.is_some()
}

// ============================================================
// 入口
// ============================================================

/// Entry point for `wing wake`.
pub async fn run(args: WakeArgs, json: bool) -> ExitCode {
    match run_inner(args, json).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wing wake error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_inner(args: WakeArgs, json: bool) -> Result<()> {
    // 校验全部前置：纯参数错误不该先把网关拉起来（`ensure_gateway` 会启守护进程）。
    let mode = validate_mode(&args)?;
    let create = match mode {
        Mode::Create => Some(create_plan(&args)?),
        Mode::List | Mode::Cancel(_) => None,
    };
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;

    match mode {
        Mode::Cancel(id) => {
            let resp = http.cancel_wake(id).await?;
            if json {
                common::print_json_compact(&WakeCancelOutput {
                    id,
                    canceled: resp.canceled,
                });
            } else if resp.canceled {
                println!("canceled {id}");
            } else {
                println!("{id} was already finished (nothing to cancel)");
            }
        }
        Mode::List => {
            let resp = http.list_wakes(args.all).await?;
            if json {
                common::print_json_compact(&resp.wakes);
            } else {
                print!("{}", render_wakes(&resp.wakes, args.all));
            }
        }
        Mode::Create => {
            let plan = create.expect("Mode::Create 已经在起网关之前算好了计划");
            let resp = http
                .create_wake(&timer_request(
                    &plan.spec,
                    &plan.prompt,
                    plan.times,
                    &plan.target,
                ))
                .await?;
            if json {
                common::print_json_compact(&WakeOutput { wake: resp.wake });
            } else {
                print!("{}", render_wake(&resp.wake));
            }
        }
    }
    Ok(())
}

/// 创建路径的**纯**计划（spec / prompt / 目标 / 次数），全部可在起网关之前算完。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CreatePlan {
    pub spec: String,
    pub prompt: String,
    pub target: String,
    pub times: Option<i64>,
}

/// 纯校验 + 组装：spec 语法、`--times` 组合、目标会话都不过网络。
pub(crate) fn create_plan(args: &WakeArgs) -> Result<CreatePlan> {
    let schedule = schedule_from_flags(
        args.in_.as_deref(),
        args.at.as_deref(),
        args.every.as_deref(),
        args.daily.as_deref(),
    )?;
    validate_timer(&schedule, args.prompt.as_deref(), args.times)?;
    Ok(CreatePlan {
        spec: schedule.spec()?,
        prompt: args.prompt.clone().unwrap_or_default(),
        target: resolve_target(args.to.as_deref())?,
        times: args.times,
    })
}

/// 本次调用是哪一种形态（三种形态的参数集合互斥）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Create,
    List,
    Cancel(i64),
}

/// 判定形态并拒绝**放错位置的**参数。
///
/// 静默吃掉一个参数等于把这次调用变成另一次调用（用户以为 schedule 生效了，
/// 实际只打印了一张表）——与仓库对"放错位置的 flag"的既有口径一致：响亮报错。
pub(crate) fn validate_mode(args: &WakeArgs) -> Result<Mode> {
    if let Some(id) = args.cancel {
        if args.list {
            bail!("--cancel cannot be combined with --list");
        }
        if has_schedule(args) {
            bail!("--cancel does not take a schedule (--in / --at / --every / --daily)");
        }
        if args.times.is_some() || args.prompt.is_some() || args.to.is_some() || args.all {
            bail!("--cancel only takes --json");
        }
        return Ok(Mode::Cancel(id));
    }
    if args.list {
        if has_schedule(args) || args.times.is_some() || args.prompt.is_some() {
            bail!(
                "--list cannot be combined with a schedule, --times or --prompt \
                 (to create one: wing wake --in 30m -p \"...\")"
            );
        }
        if args.to.is_some() {
            bail!("--list does not take --to");
        }
        return Ok(Mode::List);
    }
    if args.all {
        bail!("--all only applies to --list");
    }
    if !has_schedule(args) {
        bail!(
            "nothing to do: pass a schedule (--in 30m | --at \"...\" | --every 30m | --daily 07:00), \
             or --list / --cancel <ID>"
        );
    }
    Ok(Mode::Create)
}

// ============================================================
// 输出（纯函数：palette / 输出环境注入，可单测）
// ============================================================

fn render_wakes(wakes: &[WakeInfo], all: bool) -> String {
    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = TableOutput::detect();
    format_wakes(wakes, all, &palette, &out)
}

fn render_wake(wake: &WakeInfo) -> String {
    format_wake_detail(wake)
}

/// 一条触发器的详情（`key: value` 竖排，与 `wing run` 的输出同款）。
pub(crate) fn format_wake_detail(wake: &WakeInfo) -> String {
    let mut lines = vec![format!("wake:    #{}", wake.id)];
    lines.push(format!("kind:    {}", wake.kind));
    if let Some(source) = wake.source_session_id.as_deref() {
        lines.push(format!("source:  {source}"));
        lines.push(format!("on:      {}", wake.on_events));
    }
    lines.push(format!("target:  {}", wake.target_session_id));
    if let Some(spec) = wake.spec.as_deref() {
        lines.push(format!("spec:    {spec}"));
    }
    if let Some(next) = wake.next_fire_at.as_deref() {
        lines.push(format!("next:    {next}"));
    }
    if let Some(prompt) = wake.prompt.as_deref() {
        lines.push(format!("prompt:  {}", common::single_line(prompt)));
    }
    lines.push(format!("state:   {}", wake.state));
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// 触发器列表（人类视图）。空列表给出"下一步做什么"，而不是一张空表。
pub(crate) fn format_wakes(
    wakes: &[WakeInfo],
    all: bool,
    palette: &ThemePalette,
    out: &TableOutput,
) -> String {
    use ratatui::style::Style;

    if wakes.is_empty() {
        return if all {
            "No wake triggers.\n".to_string()
        } else {
            "No active wake triggers (finished ones: add --all).\n".to_string()
        };
    }

    let columns = vec![
        // id 是取消时要原样复制的句柄：不截断。
        PlainColumn::keep_natural("ID", ColumnKind::Compact),
        PlainColumn::new("KIND", ColumnKind::Compact),
        // 两个 id 列是本行的定位信息（复制句柄在 `--json` 与创建回执里是全的）：
        // 截短它们，换取时刻列能完整显示（`2026-03-01T09:30:00` 一列放得下）。
        PlainColumn::capped("SOURCE", ColumnKind::Compact, 16),
        PlainColumn::capped("TARGET", ColumnKind::Compact, 16),
        PlainColumn::capped("SPEC", ColumnKind::Compact, 20),
        PlainColumn::new("ON", ColumnKind::Compact),
        PlainColumn::new("STATE", ColumnKind::Compact),
        PlainColumn::new("FIRES", ColumnKind::Compact),
        PlainColumn::keep_natural("NEXT", ColumnKind::Compact),
    ];
    let rows = wakes
        .iter()
        .map(|wake| {
            vec![
                PlainCell::styled(
                    format!("#{}", wake.id),
                    Style::new().fg(palette.tool_result),
                ),
                PlainCell::styled(wake.kind.clone(), Style::new().fg(palette.text)),
                PlainCell::styled(
                    common::single_line(wake.source_session_id.as_deref().unwrap_or("-")),
                    Style::new().fg(palette.dim),
                ),
                PlainCell::styled(
                    common::single_line(&wake.target_session_id),
                    Style::new().fg(palette.tool_result),
                ),
                PlainCell::styled(
                    wake.spec.as_deref().unwrap_or("-").to_string(),
                    Style::new().fg(palette.text),
                ),
                PlainCell::styled(wake.on_events.clone(), Style::new().fg(palette.dim)),
                PlainCell::styled(wake.state.clone(), Style::new().fg(palette.text)),
                PlainCell::styled(wake.fire_count.to_string(), Style::new().fg(palette.dim)),
                PlainCell::styled(
                    // 终结的触发器没有"下一次"：别拿上次触发时刻冒充（列名就是 NEXT）。
                    short_timestamp(wake.next_fire_at.as_deref()),
                    Style::new().fg(palette.dim),
                ),
            ]
        })
        .collect();
    let table = PlainTable { columns, rows };
    common::render_table(&table, palette, out)
}

/// 表格里的时刻：截到秒（网关的 ISO 带微秒，一列放不下也没人读）。
pub(crate) fn short_timestamp(value: Option<&str>) -> String {
    match value {
        None => "-".to_string(),
        Some(text) if text.len() > 19 && text.as_bytes().get(10) == Some(&b'T') => {
            text[..19].to_string()
        }
        Some(text) => text.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // ── 时长 ──────────────────────────────────────────────

    #[test]
    fn cli_parses_the_in_flag_into_the_schedule() {
        use clap::Parser;

        let cli = crate::cmd::Cli::try_parse_from([
            "wing", "wake", "--in", "1m", "-p", "ping", "--to", "p1",
        ])
        .expect("parses");
        match cli.command {
            Some(crate::cmd::Command::Wake(args)) => {
                assert_eq!(args.in_.as_deref(), Some("1m"));
                assert_eq!(args.prompt.as_deref(), Some("ping"));
                assert_eq!(args.to.as_deref(), Some("p1"));
            }
            other => panic!("expected wake, got {other:?}"),
        }
    }

    #[test]
    fn durations_parse_in_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("10m").unwrap(), 600);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86400);
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("  5m  ").unwrap(), 300);
    }

    #[test]
    fn invalid_durations_are_rejected_locally() {
        for text in ["", "  ", "10", "10x", "5m!", "0s", "m", "10m5", "1m2x"] {
            assert!(
                parse_duration(text).is_err(),
                "{text:?} should not parse as a duration"
            );
        }
    }

    // ── 时刻 / 每日 ────────────────────────────────────────

    #[test]
    fn at_accepts_space_and_iso_forms() {
        let expected = "2026-03-01T09:00:00";
        assert_eq!(
            parse_at("2026-03-01 09:00")
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
            expected
        );
        assert_eq!(
            parse_at("2026-03-01T09:00:00")
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
            expected
        );
        assert!(parse_at("2026-03-01").is_err(), "日期单独出现缺时刻");
        assert!(parse_at("tomorrow 9am").is_err());
    }

    #[test]
    fn daily_accepts_hh_mm() {
        assert_eq!(parse_daily("07:00").unwrap(), (7, 0));
        assert_eq!(parse_daily("7:05").unwrap(), (7, 5));
        assert_eq!(parse_daily("23:59").unwrap(), (23, 59));
        for bad in ["24:00", "7:60", "0700", "seven", ""] {
            assert!(parse_daily(bad).is_err(), "{bad:?} should not parse");
        }
    }

    // ── schedule / spec ───────────────────────────────────

    #[test]
    fn schedule_maps_to_the_gateway_spec() {
        let cases = [
            (Schedule::In("30m".into()), "in:30m"),
            (Schedule::Every("1h30m".into()), "every:1h30m"),
            (
                Schedule::At("2026-03-01 09:00".into()),
                "at:2026-03-01T09:00:00",
            ),
            (Schedule::Daily("7:05".into()), "daily:07:05"),
        ];
        for (schedule, expected) in cases {
            assert_eq!(schedule.spec().unwrap(), expected);
        }
    }

    #[test]
    fn exactly_one_schedule_is_required() {
        assert!(schedule_from_flags(None, None, None, None).is_err());
        assert!(schedule_from_flags(Some("30m"), Some("2026-03-01 09:00"), None, None).is_err());
        assert_eq!(
            schedule_from_flags(Some("30m"), None, None, None).unwrap(),
            Schedule::In("30m".into())
        );
    }

    #[test]
    fn one_shot_specs_reject_times() {
        assert!(Schedule::In("5m".into()).is_one_shot());
        assert!(Schedule::At("2026-03-01 09:00".into()).is_one_shot());
        assert!(!Schedule::Every("5m".into()).is_one_shot());
        assert!(!Schedule::Daily("07:00".into()).is_one_shot());

        let once = Schedule::In("5m".into());
        let error = validate_timer(&once, Some("ping"), Some(3)).unwrap_err();
        assert!(error.to_string().contains("--times"), "{error}");
        let recurring = Schedule::Every("5m".into());
        assert!(validate_timer(&recurring, Some("ping"), Some(3)).is_ok());
    }

    #[test]
    fn timer_requires_a_prompt() {
        let every = Schedule::Every("5m".into());
        assert!(validate_timer(&every, None, None).is_err());
        assert!(validate_timer(&every, Some("   "), None).is_err());
        assert!(validate_timer(&every, Some("ping"), None).is_ok());
        assert!(validate_timer(&every, Some("ping"), Some(0)).is_err());
    }

    #[test]
    fn on_events_is_normalized_and_validated() {
        assert_eq!(parse_on_events("done,ask").unwrap(), "done,ask");
        assert_eq!(parse_on_events(" ask , done ").unwrap(), "ask,done");
        assert_eq!(parse_on_events("done,done").unwrap(), "done");
        assert!(parse_on_events("done,error").is_err());
        assert!(parse_on_events("  ").is_err());
    }

    #[test]
    fn timer_request_carries_the_spec_and_prompt() {
        let request = timer_request("every:30m", "ping", Some(2), "parent");
        assert_eq!(request.kind, "timer");
        assert_eq!(request.target_session_id, "parent");
        assert_eq!(request.spec.as_deref(), Some("every:30m"));
        assert_eq!(request.prompt.as_deref(), Some("ping"));
        assert_eq!(request.times, Some(2));
        assert!(request.source_session_id.is_none());
    }

    // ── 输出 ──────────────────────────────────────────────

    fn wake_info(kind: &str) -> WakeInfo {
        WakeInfo {
            id: 7,
            kind: kind.into(),
            target_session_id: "parent".into(),
            source_session_id: (kind == "session").then(|| "child".into()),
            on_events: "done,ask".into(),
            spec: (kind == "timer").then(|| "every:30m".into()),
            prompt: (kind == "timer").then(|| "ping".into()),
            state: "active".into(),
            fire_count: 0,
            last_fire_at: None,
            last_error: None,
            created_at: Some("2026-03-01T09:00:00".into()),
            next_fire_at: (kind == "timer").then(|| "2026-03-01T09:30:00".into()),
        }
    }

    #[test]
    fn timer_detail_lists_spec_next_and_prompt() {
        let text = format_wake_detail(&wake_info("timer"));
        assert!(text.contains("wake:    #7"), "{text}");
        assert!(text.contains("spec:    every:30m"), "{text}");
        assert!(text.contains("next:    2026-03-01T09:30:00"), "{text}");
        assert!(text.contains("prompt:  ping"), "{text}");
        assert!(!text.contains("source:"), "{text}");
    }

    #[test]
    fn session_detail_lists_source_and_events() {
        let text = format_wake_detail(&wake_info("session"));
        assert!(text.contains("source:  child"), "{text}");
        assert!(text.contains("on:      done,ask"), "{text}");
        assert!(text.contains("target:  parent"), "{text}");
        assert!(!text.contains("spec:"), "{text}");
    }

    fn base_args() -> WakeArgs {
        WakeArgs {
            in_: None,
            at: None,
            every: None,
            daily: None,
            prompt: None,
            times: None,
            to: None,
            list: false,
            all: false,
            cancel: None,
        }
    }

    #[test]
    fn validate_mode_rejects_misplaced_flags() {
        let mut args = base_args();
        args.cancel = Some(7);
        assert_eq!(validate_mode(&args).unwrap(), Mode::Cancel(7));

        // --cancel 只吃 id
        for mutate in [
            |a: &mut WakeArgs| a.list = true,
            |a: &mut WakeArgs| a.in_ = Some("5m".into()),
            |a: &mut WakeArgs| a.daily = Some("07:00".into()),
            |a: &mut WakeArgs| a.times = Some(2),
            |a: &mut WakeArgs| a.prompt = Some("x".into()),
            |a: &mut WakeArgs| a.to = Some("sid".into()),
            |a: &mut WakeArgs| a.all = true,
        ] {
            let mut args = base_args();
            args.cancel = Some(7);
            mutate(&mut args);
            assert!(validate_mode(&args).is_err(), "{args:?} 应当被拒");
        }

        // --list 只吃 --all
        let mut args = base_args();
        args.list = true;
        args.all = true;
        assert_eq!(validate_mode(&args).unwrap(), Mode::List);
        for mutate in [
            |a: &mut WakeArgs| a.in_ = Some("5m".into()),
            |a: &mut WakeArgs| a.times = Some(2),
            |a: &mut WakeArgs| a.prompt = Some("x".into()),
            |a: &mut WakeArgs| a.to = Some("sid".into()),
        ] {
            let mut args = base_args();
            args.list = true;
            mutate(&mut args);
            assert!(validate_mode(&args).is_err(), "{args:?} 应当被拒");
        }

        // 创建：必须有 schedule；--all 只配 --list
        assert!(validate_mode(&base_args()).is_err());
        let mut args = base_args();
        args.all = true;
        assert!(validate_mode(&args).is_err());
        let mut args = base_args();
        args.every = Some("30m".into());
        args.prompt = Some("ping".into());
        args.to = Some("sid".into());
        assert_eq!(validate_mode(&args).unwrap(), Mode::Create);
    }

    #[test]
    fn has_schedule_sees_every_form() {
        let mut args = WakeArgs {
            in_: None,
            at: None,
            every: None,
            daily: None,
            prompt: None,
            times: None,
            to: None,
            list: false,
            all: false,
            cancel: None,
        };
        assert!(!has_schedule(&args));
        args.daily = Some("07:00".into());
        assert!(
            has_schedule(&args),
            "--daily 也算 schedule（曾经在 --cancel 里被漏掉）"
        );
        args.daily = None;
        args.at = Some("2026-03-01 09:00".into());
        assert!(has_schedule(&args));
    }

    #[test]
    fn the_next_column_never_shows_the_last_fire_time() {
        let palette = ThemePalette::from_config(&AppConfig::load().colors);
        let out = TableOutput {
            width: common::PIPE_WIDTH,
            color: false,
        };
        let mut finished = wake_info("timer");
        finished.state = "done".into();
        finished.next_fire_at = None;
        finished.last_fire_at = Some("2026-03-01T09:30:00.5".into());
        let text = format_wakes(&[finished], true, &palette, &out);
        assert!(
            !text.contains("2026-03-01T09:30:00"),
            "终态行的 NEXT 必须是 '-'：{text}"
        );
    }

    #[test]
    fn timestamps_in_the_table_stop_at_seconds() {
        assert_eq!(
            short_timestamp(Some("2026-03-01T09:30:00.123456")),
            "2026-03-01T09:30:00"
        );
        assert_eq!(
            short_timestamp(Some("2026-03-01T09:30:00")),
            "2026-03-01T09:30:00"
        );
        assert_eq!(short_timestamp(None), "-");
    }

    #[test]
    fn empty_list_says_what_to_do_next() {
        let palette = ThemePalette::from_config(&AppConfig::load().colors);
        let out = TableOutput {
            width: common::PIPE_WIDTH,
            color: false,
        };
        let active = format_wakes(&[], false, &palette, &out);
        assert!(active.contains("--all"), "{active}");
        let everything = format_wakes(&[], true, &palette, &out);
        assert!(everything.contains("No wake triggers"), "{everything}");
    }

    #[test]
    fn list_renders_every_row() {
        let palette = ThemePalette::from_config(&AppConfig::load().colors);
        let out = TableOutput {
            width: common::PIPE_WIDTH,
            color: false,
        };
        let text = format_wakes(
            &[wake_info("timer"), wake_info("session")],
            true,
            &palette,
            &out,
        );
        assert!(text.contains("#7"), "{text}");
        assert!(text.contains("every:30m"), "{text}");
        assert!(text.contains("done,ask"), "{text}");
        assert!(text.contains("2026-03-01T09:30:00"), "{text}");
    }
}
