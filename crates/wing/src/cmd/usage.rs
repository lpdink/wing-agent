//! `wing usage` — 我的使用史：活动热力图、token、会话、子智能体。
//!
//! 一条命令回答「我最近干得怎么样」：默认出仪表盘（热力图 + 今天 + 我/我的 AI + 模型），
//! `--by <维度>` 出明细表。所有数字都来自 `GET /api/usage` 的 11 个整数度量——派生值
//! （命中率、工时）在这里算，端点不含派生值。
//!
//! ```sh
//! wing usage                          # 仪表盘（最近 12 周）
//! wing usage --since all --metric calls
//! wing usage --by model --limit 5
//! wing usage --by session --tree --session "$SID"
//! wing usage --json                   # 端点响应原样（无 --by 时是 group_by=day）
//! ```
//!
//! 三条口径：
//!
//! - **参数闸门在连网关之前**（非法值不拉起守护进程）；
//! - **没有读数写 `—`，不写 `0`**（`0` 是一个读数）；
//! - 行序、截断、`totals` 全归端点：CLI 不重排、不重算，TOTAL 行永远可对账。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use chrono::Duration as ChronoDuration;
use chrono::Local;
use chrono::NaiveDate;
use ratatui::style::Style;
use wing_api_client::ApiClientError;
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::UsageQuery;
use wing_api_client::models::SessionInfo;
use wing_api_client::models::UsageMeasures;
use wing_api_client::models::UsageResponse;
use wing_api_client::models::UsageRow;

use crate::config::AppConfig;
use crate::config::ThemePalette;
use crate::render::heatmap::Heatmap;
use crate::render::table::ColumnKind;
use crate::render::table::plain::PlainCell;
use crate::render::table::plain::PlainColumn;
use crate::render::table::plain::PlainTable;
use crate::shared::session_tree::TreeRow;
use crate::shared::session_tree::build_forest;
use crate::util::format;
use crate::util::format::{NONE_MARK, count, work_time};

use super::common;
use super::common::TableOutput;

/// `--limit` 的默认值（与端点默认一致）。
pub(crate) const DEFAULT_LIMIT: u32 = 20;
/// `--limit` 的上界（端点契约：1..=200）。
pub(crate) const MAX_LIMIT: u32 = 200;

/// 仪表盘 MODEL 表取前几行（「谁最贵」只需要榜首几名）。
const DASHBOARD_MODEL_ROWS: u32 = 5;
/// `--since` / `--until` 的取值域，用于参数闸门的报错文案。
///
/// `--help` 里有一份独立副本（clap 的 doc 注释），改取值域时两处都要改。
const WHEN_FORMS: &str = "all | today | yesterday | <N>d | <N>w | <N>m | <N>y | YYYY-MM-DD";

/// `-w` 的刷新间隔（与 `wing ps -w` 一致）。
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// 仪表盘与表格正文的左侧缩进（热力图/散行文字用；表格自带画框不缩进）。
const INDENT: &str = "  ";

/// 一次 `wing usage` 的入参（dispatch 从 clap 的字段拼出来）。
pub struct UsageOpts {
    pub since: String,
    pub until: String,
    pub by: Option<String>,
    pub metric: String,
    pub session: Option<String>,
    pub tree: bool,
    pub limit: u32,
}

/// Entry point for `wing usage`.
pub async fn run(opts: UsageOpts, json: bool, watch: bool) -> ExitCode {
    match run_inner(opts, json, watch).await {
        Ok(code) => code,
        Err(e) => common::fail("usage", e),
    }
}

async fn run_inner(opts: UsageOpts, json: bool, watch: bool) -> Result<ExitCode> {
    // 闸门全在连网关之前：非法参数不该拉起守护进程。
    let plan = Plan::resolve(&opts)?;

    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    let palette = ThemePalette::from_config(&AppConfig::load().colors);
    let out = TableOutput::detect();

    loop {
        // 每轮先清屏再画（与 `wing ps -w` 同形）：反过来会把刚画出来的帧立刻抹掉。
        if watch {
            print!("\x1b[2J\x1b[H");
        }
        match emit_once(&http, &plan, &palette, &out, json).await {
            Ok(()) => {}
            Err(e) if watch => eprintln!("wing usage error: {e}"),
            Err(e) => return Err(e),
        }
        if !watch {
            return Ok(ExitCode::SUCCESS);
        }
        tokio::time::sleep(WATCH_INTERVAL).await;
    }
}

/// 一次完整输出（watch 的每一轮就是一次）。
async fn emit_once(
    http: &GatewayApiClient,
    plan: &Plan,
    palette: &ThemePalette,
    out: &TableOutput,
    json: bool,
) -> Result<()> {
    if json {
        // 仪表盘的机器面就是 `group_by=day` 的那一次响应（热力图数据 + totals）。
        let dim = plan.by.unwrap_or(Dim::Day);
        let report = fetch(http, plan, Some(dim), dim.is_ranked().then_some(plan.limit)).await?;
        common::print_json_compact(&report);
        return Ok(());
    }
    let text = match plan.by {
        None => render_dashboard(http, plan, palette, out).await?,
        Some(dim) => render_detail(http, plan, dim, palette, out).await?,
    };
    print!("{text}");
    Ok(())
}

/// 发一次用量查询（`group_by` 由 `dim` 决定；`--by` 缺省时是 `day`）。
async fn fetch(
    http: &GatewayApiClient,
    plan: &Plan,
    dim: Option<Dim>,
    limit: Option<u32>,
) -> Result<UsageResponse> {
    match query(http, plan, dim, limit).await {
        Ok(report) => Ok(report),
        Err(error) => Err(diagnose(http, plan, dim, limit, *error).await),
    }
}

/// 一次纯净的查询（错误原样出来，诊断在 [`diagnose`] 里做）。
///
/// `Err` 装箱：`ApiClientError` 带着整个响应体，原样当返回值会让每个 `Result`
/// 都背上它的大小（判定只需要状态码）。
async fn query(
    http: &GatewayApiClient,
    plan: &Plan,
    dim: Option<Dim>,
    limit: Option<u32>,
) -> std::result::Result<UsageResponse, Box<ApiClientError>> {
    let dim = dim.map(Dim::as_str);
    let query = UsageQuery {
        since: Some(plan.since.clone()),
        until: Some(plan.until.clone()),
        group_by: dim.map(str::to_string),
        session_id: plan.session.clone(),
        tree: plan.tree,
        limit,
    };
    http.get_usage(&query).await.map_err(Box::new)
}

/// 一次失败的查询 → 一句人话。
///
/// 同一个 404 有两条来源：**这个会话不存在**（0 是一个读数，静默给全 0 会让用户以为
/// 它没花钱）与**网关还没有这个路由**。带作用域时才需要区分——再发一次**不带作用域**的
/// 同一查询：它也 404 说明连路由都没有（不嗅探 `detail` 文案，文案会变）。
///
/// 「会话不存在」只在**探测自己回答 200**（端点确实可用）时才给：探测本身失败
/// （网络 / 5xx / 超时）什么也证明不了，原样报原始错误。残余误判仍有一条：作用域那次
/// 查询在服务端读库失败（等锁超时 / 磁盘错）也会落到 404，而 CLI 分辨不了——见
/// `docs/dev/usage.md` §2 的降级口径。
async fn diagnose(
    http: &GatewayApiClient,
    plan: &Plan,
    dim: Option<Dim>,
    limit: Option<u32>,
    error: ApiClientError,
) -> anyhow::Error {
    let Some(session_id) = plan.session.as_deref() else {
        return classify(error, None);
    };
    if !error.is_not_found() {
        return classify(error, Some(session_id));
    }
    // `tree` 必须配 `session_id`：探测请求因此不带它。
    let probe = Plan {
        session: None,
        tree: false,
        ..plan.clone()
    };
    match query(http, &probe, dim, limit).await {
        Ok(_) => classify(error, Some(session_id)),
        Err(probe_error) if is_missing_route(&probe_error) => classify(*probe_error, None),
        Err(_) => error.into(),
    }
}

/// 服务端状态码（网络层 / 反序列化错误没有状态码）。
fn status_of(error: &ApiClientError) -> Option<u16> {
    match error {
        ApiClientError::Api { status, .. } => Some(*status),
        _ => None,
    }
}

/// 端点错误 → 人类的一句话（`session` 是**已判定成立**的作用域，判定见 [`diagnose`]）。
///
/// 带作用域的 404 = 「这个会话不存在」；不带作用域的 404/405 = 「网关太旧」——
/// 与 `wing cache` 的既有文案同一句。
fn classify(error: ApiClientError, session: Option<&str>) -> anyhow::Error {
    match session {
        Some(session_id) if error.is_not_found() => {
            anyhow::anyhow!("session not found: {session_id}")
        }
        None if is_missing_route(&error) => anyhow::anyhow!(
            "{error} — the running gateway does not serve /api/usage \
             (restart it: wing stop && wing start)"
        ),
        _ => anyhow::anyhow!("{error}"),
    }
}

/// 「网关根本没有这个路由」：404 / 405。与上面那句文案是同一个判据，住在一起。
///
/// 更彻底的解法是让端点给「会话不存在」一个结构化 `error` 码（`gateway/app.py` 的
/// `session_archived` 就是先例）——那要改端点契约，不在本步骤范围内。
fn is_missing_route(error: &ApiClientError) -> bool {
    matches!(status_of(error), Some(404 | 405))
}

// ============================================================
// 参数闸门（纯函数，测试直接打这里）
// ============================================================

/// 明细维度（`--by` 的取值域，顺序即报错文案的顺序）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dim {
    Day,
    Model,
    Session,
    Tool,
    Workspace,
    Agent,
    Origin,
}

impl Dim {
    const ALL: [Dim; 7] = [
        Dim::Day,
        Dim::Model,
        Dim::Session,
        Dim::Tool,
        Dim::Workspace,
        Dim::Agent,
        Dim::Origin,
    ];

    fn parse(raw: &str) -> Option<Dim> {
        Self::ALL.into_iter().find(|dim| dim.as_str() == raw)
    }

    fn as_str(self) -> &'static str {
        match self {
            Dim::Day => "day",
            Dim::Model => "model",
            Dim::Session => "session",
            Dim::Tool => "tool",
            Dim::Workspace => "workspace",
            Dim::Agent => "agent",
            Dim::Origin => "origin",
        }
    }

    /// 排行类维度（`--limit` 只作用于它们；`day` 稠密、`origin` 恰两行）。
    fn is_ranked(self) -> bool {
        matches!(
            self,
            Dim::Model | Dim::Session | Dim::Tool | Dim::Workspace | Dim::Agent
        )
    }
}

/// 热力图的度量轴（`--metric` 的取值域）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Metric {
    Tokens,
    Messages,
    Calls,
    Tools,
    Sessions,
}

impl Metric {
    const ALL: [Metric; 5] = [
        Metric::Tokens,
        Metric::Messages,
        Metric::Calls,
        Metric::Tools,
        Metric::Sessions,
    ];

    fn parse(raw: &str) -> Option<Metric> {
        Self::ALL.into_iter().find(|metric| metric.as_str() == raw)
    }

    fn as_str(self) -> &'static str {
        match self {
            Metric::Tokens => "tokens",
            Metric::Messages => "messages",
            Metric::Calls => "calls",
            Metric::Tools => "tools",
            Metric::Sessions => "sessions",
        }
    }

    /// 一天的格子里画哪个数（`tokens` = `prompt + completion`，含缓存重发）。
    fn value(self, measures: &UsageMeasures) -> i64 {
        match self {
            Metric::Tokens => measures.tokens(),
            Metric::Messages => measures.user_messages,
            Metric::Calls => measures.llm_calls,
            Metric::Tools => measures.tool_calls,
            Metric::Sessions => measures.sessions,
        }
    }
}

/// 校验并归一化后的请求（`--since all` 原样发给端点，由它收敛到第一天）。
#[derive(Clone, Debug)]
struct Plan {
    since: String,
    until: String,
    by: Option<Dim>,
    metric: Metric,
    session: Option<String>,
    tree: bool,
    limit: u32,
}

impl Plan {
    fn resolve(opts: &UsageOpts) -> Result<Plan> {
        if !(1..=MAX_LIMIT).contains(&opts.limit) {
            anyhow::bail!(
                "--limit must be between 1 and {MAX_LIMIT} (got {})",
                opts.limit
            );
        }
        let by = match opts.by.as_deref() {
            None => None,
            Some(raw) => Some(Dim::parse(raw).ok_or_else(|| {
                anyhow::anyhow!(
                    "--by must be one of {} (got {raw:?})",
                    value_list(Dim::ALL.map(Dim::as_str))
                )
            })?),
        };
        let metric = Metric::parse(&opts.metric).ok_or_else(|| {
            anyhow::anyhow!(
                "--metric must be one of {} (got {:?})",
                value_list(Metric::ALL.map(Metric::as_str)),
                opts.metric
            )
        })?;
        if opts.tree && opts.session.is_none() {
            anyhow::bail!("--tree requires --session (a tree without a root is the whole db)");
        }

        let today = Local::now().date_naive();
        let since = parse_when(&opts.since, today, "since")?;
        let until = parse_when(&opts.until, today, "until")?;
        let until = match until {
            When::Day(day) => day,
            When::All => anyhow::bail!("--until cannot be \"all\" (it is a range end)"),
        };
        if let When::Day(start) = since
            && start > until
        {
            anyhow::bail!(
                "--since ({}) is after --until ({})",
                start.format("%Y-%m-%d"),
                until.format("%Y-%m-%d")
            );
        }
        Ok(Plan {
            since: match since {
                When::All => "all".to_string(),
                When::Day(day) => day.format("%Y-%m-%d").to_string(),
            },
            until: until.format("%Y-%m-%d").to_string(),
            by,
            metric,
            session: opts.session.clone(),
            tree: opts.tree,
            limit: opts.limit,
        })
    }
}

/// `a, b, c`（报错文案里的取值域）。
fn value_list<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values.into_iter().collect::<Vec<&str>>().join(", ")
}

/// `--since` / `--until` 的取值：`all` 或一个具体日期。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum When {
    All,
    Day(NaiveDate),
}

/// 人类写法 → 网关日期。时长写法相对**本地今天**往回减（`7d` = 今天往前 7 天）。
fn parse_when(raw: &str, today: NaiveDate, field: &str) -> Result<When> {
    let text = raw.trim();
    let lower = text.to_ascii_lowercase();
    let when = match lower.as_str() {
        "" => anyhow::bail!("--{field} needs a value: {}", WHEN_FORMS),
        "all" => When::All,
        "today" => When::Day(today),
        "yesterday" => When::Day(today - ChronoDuration::days(1)),
        _ => match NaiveDate::parse_from_str(text, "%Y-%m-%d") {
            Ok(day) => When::Day(day),
            Err(_) => match parse_span(&lower) {
                Some(days) => When::Day(today - ChronoDuration::days(days)),
                None => anyhow::bail!("invalid --{field}: {raw:?} (expect {WHEN_FORMS})"),
            },
        },
    };
    Ok(when)
}

/// `7d` / `4w` / `3m` / `1y` → 天数（`m` = 30 天、`y` = 365 天，够用且不随月份漂移）。
fn parse_span(raw: &str) -> Option<i64> {
    let mut chars = raw.chars();
    let unit = chars.next_back()?;
    let count: i64 = chars.as_str().parse().ok()?;
    if count <= 0 {
        return None;
    }
    let days = match unit {
        'd' => count,
        'w' => count.checked_mul(7)?,
        'm' => count.checked_mul(30)?,
        'y' => count.checked_mul(365)?,
        _ => return None,
    };
    Some(days)
}

// ============================================================
// 仪表盘
// ============================================================

async fn render_dashboard(
    http: &GatewayApiClient,
    plan: &Plan,
    palette: &ThemePalette,
    out: &TableOutput,
) -> Result<String> {
    // 三次并发请求：热力图（day）/ 我 vs 我的 AI（origin）/ 谁最贵（model，固定前几行）。
    let (day, origin, model) = tokio::join!(
        query(http, plan, Some(Dim::Day), None),
        query(http, plan, Some(Dim::Origin), None),
        query(http, plan, Some(Dim::Model), Some(DASHBOARD_MODEL_ROWS)),
    );
    // 三条都失败时只诊断**第一条**：诊断会发一次探测请求，三条各诊断一次没有意义。
    let mut reports = Vec::with_capacity(3);
    for (dim, result) in [(Dim::Day, day), (Dim::Origin, origin), (Dim::Model, model)] {
        let limit = matches!(dim, Dim::Model).then_some(DASHBOARD_MODEL_ROWS);
        match result {
            Ok(report) => reports.push(report),
            Err(error) => return Err(diagnose(http, plan, Some(dim), limit, *error).await),
        }
    }
    format_dashboard(
        &reports[0],
        &reports[1],
        &reports[2],
        plan.metric,
        palette,
        out,
    )
}

fn format_dashboard(
    day: &UsageResponse,
    origin: &UsageResponse,
    model: &UsageResponse,
    metric: Metric,
    palette: &ThemePalette,
    out: &TableOutput,
) -> Result<String> {
    let since = parse_day(&day.range.since)?;
    let until = parse_day(&day.range.until)?;
    let width = out.width;
    let heat_width = width.saturating_sub(INDENT.len());

    let values: Vec<(NaiveDate, i64)> = day
        .rows
        .iter()
        .filter_map(|row| {
            let date = parse_day(&row.key).ok()?;
            Some((date, metric.value(&row.measures)))
        })
        .collect();
    let heat = Heatmap {
        since,
        until,
        values: &values,
        width: heat_width,
        ink: out.color.then_some(palette),
    }
    .render();

    let mut out_text = String::new();
    // 周数说窗口横跨多少周，裁掉时再补一句实际画了几列（两个数字相同就不补）。
    let shown = if heat.truncated() {
        format!(" (last {} shown)", heat.columns)
    } else {
        String::new()
    };
    let weeks = weeks_text(heat.total);
    // 窄终端从宽到窄逐级退让：整段范围 → 只留终点 → 只留周数，保证标题不折行。
    let rights = [
        format!(
            "{} → {} · {weeks}{shown}",
            since.format("%Y-%m-%d"),
            until.format("%Y-%m-%d")
        ),
        format!("→ {} · {weeks}{shown}", until.format("%Y-%m-%d")),
        format!("{weeks}{shown}"),
    ];
    out_text.push_str(&title_line("WING USAGE", &rights, width));
    out_text.push('\n');

    if let Some(note) = empty_note(day) {
        out_text.push('\n');
        out_text.push_str(INDENT);
        out_text.push_str(note);
        out_text.push('\n');
        return Ok(out_text);
    }

    out_text.push('\n');
    for line in &heat.lines {
        out_text.push_str(INDENT);
        out_text.push_str(line);
        out_text.push('\n');
    }

    // 活跃天数 / 连续活跃 / 最长连续（都在窗口上算）。
    let mut flags: Vec<(NaiveDate, bool)> = day
        .rows
        .iter()
        .filter_map(|row| {
            let date = parse_day(&row.key).ok()?;
            Some((date, row.measures.has_flow()))
        })
        .collect();
    flags.sort_by_key(|(date, _)| *date);
    let (active_days, streak, longest) = streak_stats(&flags, Local::now().date_naive());
    out_text.push('\n');
    out_text.push_str(&format!(
        "{INDENT}{} active {} · streak {streak} · longest {longest}\n",
        count(active_days as i64),
        if active_days == 1 { "day" } else { "days" },
    ));

    // 审计起点提示：早于流水的日子只有「建了多少会话」这类计数，必须说明，
    // 否则用户会以为之前的月份没干活。
    if let Some(data_since) = day.range.data_since.as_deref()
        && let Ok(data_since) = parse_day(data_since)
        && data_since > since
    {
        out_text.push_str(&format!(
            "{INDENT}token / call / message audit starts {}; earlier days only have session counts\n",
            data_since.format("%Y-%m-%d")
        ));
    }

    // TODAY：只打今天真的干活了的那一天（全 0 的一天不打一排 0）。
    let today = Local::now().date_naive();
    if today >= since
        && today <= until
        && let Some(row) = day.row(&today.format("%Y-%m-%d").to_string())
        && row.measures.has_flow()
    {
        out_text.push('\n');
        out_text.push_str(&format!(
            "{INDENT}{:<8}{}\n",
            "TODAY",
            today_text(&row.measures)
        ));
    }

    out_text.push('\n');
    out_text.push_str(&format_who_table(origin, palette, out));
    out_text.push('\n');
    out_text.push_str(&format_model_table(model, palette, out));
    Ok(out_text)
}

/// `TODAY   34 messages · 6 sessions · ↑ 1.4G in (99% cached) · ↓ 4.8M out · work 20h 00m`
fn today_text(measures: &UsageMeasures) -> String {
    format!(
        "{} {} · {} {} · ↑ {} in{} · ↓ {} out · work {}",
        count(measures.user_messages),
        if measures.user_messages == 1 {
            "message"
        } else {
            "messages"
        },
        count(measures.sessions),
        if measures.sessions == 1 {
            "session"
        } else {
            "sessions"
        },
        format::tokens(measures.prompt_tokens),
        match format::hit_rate(measures.cached_tokens, measures.prompt_tokens) {
            Some(rate) => format!(" ({rate:.0}% cached)"),
            None => String::new(),
        },
        format::tokens(measures.completion_tokens),
        work_time(measures.work_time_ms()),
    )
}

/// 我 vs 我的 AI：`origin` 的两行 + 一行合计（`totals` 是端点给的对账基准）。
fn format_who_table(origin: &UsageResponse, palette: &ThemePalette, out: &TableOutput) -> String {
    let columns = vec![
        PlainColumn::new("WHO", ColumnKind::Compact),
        PlainColumn::new("CREATED", ColumnKind::Compact),
        PlainColumn::new("ACTIVE", ColumnKind::Compact),
        PlainColumn::new("MSGS", ColumnKind::Compact),
        PlainColumn::new("LLM CALLS", ColumnKind::Compact),
        PlainColumn::new("TOOL CALLS", ColumnKind::Compact),
        PlainColumn::new("IN", ColumnKind::Compact),
        PlainColumn::new("CACHE", ColumnKind::Compact),
        PlainColumn::new("OUT", ColumnKind::Compact),
    ];
    let mut rows: Vec<Vec<PlainCell>> = origin
        .rows
        .iter()
        .map(|row| {
            let mut cells = origin_cells(&row.measures);
            cells.insert(0, text_cell(row.key.clone(), palette.text));
            cells
        })
        .collect();
    let mut total = origin_cells(&origin.totals);
    total.insert(
        0,
        PlainCell::styled("total".to_string(), Style::new().fg(palette.accent)),
    );
    // 合计行整行用同一支强调墨色（它是这张表唯一的对账基准）。
    rows.push(
        total
            .into_iter()
            .map(|cell| PlainCell {
                text: cell.text,
                style: Some(Style::new().fg(palette.accent)),
            })
            .collect(),
    );

    let table = PlainTable { columns, rows };
    common::render_table(&table, palette, out)
}

/// `origin` 两行与合计共用的度量列（`CREATED` 起的 8 格）。
fn origin_cells(measures: &UsageMeasures) -> Vec<PlainCell> {
    vec![
        PlainCell::plain(count(measures.sessions_created)),
        PlainCell::plain(count(measures.sessions)),
        PlainCell::plain(count(measures.user_messages)),
        PlainCell::plain(count(measures.llm_calls)),
        PlainCell::plain(count(measures.tool_calls)),
        PlainCell::plain(format::tokens(measures.prompt_tokens)),
        PlainCell::plain(cache_text(measures)),
        PlainCell::plain(format::tokens(measures.completion_tokens)),
    ]
}

fn format_model_table(model: &UsageResponse, palette: &ThemePalette, out: &TableOutput) -> String {
    let columns = vec![
        PlainColumn::capped("MODEL", ColumnKind::Narrative, 48),
        PlainColumn::new("CALLS", ColumnKind::Compact),
        PlainColumn::new("IN", ColumnKind::Compact),
        PlainColumn::new("CACHE", ColumnKind::Compact),
        PlainColumn::new("OUT", ColumnKind::Compact),
    ];
    let rows = model
        .rows
        .iter()
        .map(|row| {
            vec![
                text_cell(model_name(row), palette.text),
                PlainCell::plain(count(row.measures.llm_calls)),
                PlainCell::plain(format::tokens(row.measures.prompt_tokens)),
                PlainCell::plain(cache_text(&row.measures)),
                PlainCell::plain(format::tokens(row.measures.completion_tokens)),
            ]
        })
        .collect();
    let table = PlainTable { columns, rows };
    common::render_table(&table, palette, out)
}

// ============================================================
// 明细（--by）
// ============================================================

async fn render_detail(
    http: &GatewayApiClient,
    plan: &Plan,
    dim: Dim,
    palette: &ThemePalette,
    out: &TableOutput,
) -> Result<String> {
    let limit = dim.is_ranked().then_some(plan.limit);
    let report = fetch(http, plan, Some(dim), limit).await?;
    // 标题/归属是会话维度的展示素材：取不到就退化成 `—`，不拖垮报表。
    let sessions = match dim {
        Dim::Session => http.list_sessions().await.ok(),
        _ => None,
    };
    Ok(format_detail(
        &report,
        dim,
        plan,
        sessions.as_ref().map(|list| list.sessions.as_slice()),
        palette,
        out,
    ))
}

fn format_detail(
    report: &UsageResponse,
    dim: Dim,
    plan: &Plan,
    sessions: Option<&[SessionInfo]>,
    palette: &ThemePalette,
    out: &TableOutput,
) -> String {
    let mut text = String::new();
    // 空态 = 窗口里一件事实都没有：排行类维度靠 `rows` 就能看出来，稠密维度（`day`
    // 每天一行、`origin` 恒两行）的零行永远非空——只看 `rows` 会把首跑打成一整窗
    // 全零的表，还附一句「建过会话」的假话。
    let nothing = !report.totals.has_flow() && report.totals.sessions_created == 0;
    if (report.rows.is_empty() || nothing)
        && let Some(note) = empty_note(report)
    {
        text.push_str(note);
        text.push('\n');
        return text;
    }

    let columns = detail_columns(dim);
    // 行序（`--tree` 时是血缘树序）+ 每行的渲染前缀，只算一次。
    let order = ordered_rows(report, plan.tree);
    let mut rows: Vec<Vec<PlainCell>> = Vec::with_capacity(order.len() + 1);
    for (index, prefix) in &order {
        let row = &report.rows[*index];
        let joined = sessions.and_then(|list| list.iter().find(|info| info.id == row.key));
        rows.push(detail_row(dim, row, prefix, joined, palette));
    }
    rows.push(detail_total(dim, report, palette));
    debug_assert!(
        rows.iter().all(|row| row.len() == columns.len()),
        "每行的格子数必须等于列数（TOTAL 行与数据行同一套列）"
    );

    let table = PlainTable { columns, rows };
    text.push_str(&common::render_table(&table, palette, out));
    if report.truncated {
        text.push_str(&format!("… more {} (raise --limit)\n", dim.as_str()));
    }
    // 有行、但十个流水度量全 0：这一窗只有目录事实（换窗口也补不出流水）。
    if !report.totals.has_flow() && report.totals.sessions_created > 0 {
        text.push_str("only sessions created in this range (no flow yet)\n");
    }
    text
}

/// 一行明细：键列 + 该维度的独有列 + 三列 token 度量。
fn detail_row(
    dim: Dim,
    row: &UsageRow,
    prefix: &str,
    session: Option<&SessionInfo>,
    palette: &ThemePalette,
) -> Vec<PlainCell> {
    let m = &row.measures;
    match dim {
        Dim::Day => vec![
            text_cell(row.key.clone(), palette.text),
            PlainCell::plain(count(m.sessions)),
            PlainCell::plain(count(m.user_messages)),
            PlainCell::plain(count(m.llm_calls)),
            PlainCell::plain(count(m.tool_calls)),
            PlainCell::plain(format::tokens(m.prompt_tokens)),
            PlainCell::plain(cache_text(m)),
            PlainCell::plain(format::tokens(m.completion_tokens)),
            PlainCell::plain(work_time(m.work_time_ms())),
        ],
        Dim::Model => {
            let (provider, _) = split_model_key(&row.key);
            vec![
                text_cell(model_name(row), palette.text),
                PlainCell::plain(provider.to_string()),
                PlainCell::plain(count(m.llm_calls)),
                PlainCell::plain(format::tokens(m.prompt_tokens)),
                PlainCell::plain(cache_text(m)),
                PlainCell::plain(format::tokens(m.completion_tokens)),
            ]
        }
        Dim::Session => vec![
            text_cell(format!("{prefix}{}", row.key), palette.tool_result),
            PlainCell::plain(
                session
                    .and_then(|info| info.name.as_deref())
                    .map(common::single_line)
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| NONE_MARK.to_string()),
            ),
            PlainCell::plain(session.map_or_else(
                || NONE_MARK.to_string(),
                |info| {
                    if info.parent_session_id.is_some() {
                        "agents".to_string()
                    } else {
                        "me".to_string()
                    }
                },
            )),
            PlainCell::plain(count(m.user_messages)),
            PlainCell::plain(count(m.llm_calls)),
            PlainCell::plain(format::tokens(m.prompt_tokens)),
            PlainCell::plain(cache_text(m)),
            PlainCell::plain(format::tokens(m.completion_tokens)),
        ],
        Dim::Tool => vec![
            text_cell(row.key.clone(), palette.text),
            PlainCell::plain(count(m.tool_calls)),
            PlainCell::plain(count(m.tool_errors)),
            PlainCell::plain(avg_time(m.tool_time_ms, m.tool_calls)),
        ],
        Dim::Workspace => vec![
            text_cell(namespace_text(&row.key), palette.text),
            PlainCell::plain(count(m.sessions)),
            PlainCell::plain(count(m.llm_calls)),
            PlainCell::plain(format::tokens(m.prompt_tokens)),
            PlainCell::plain(format::tokens(m.completion_tokens)),
        ],
        Dim::Agent => vec![
            text_cell(namespace_text(&row.key), palette.text),
            PlainCell::plain(count(m.sessions)),
            PlainCell::plain(count(m.llm_calls)),
            PlainCell::plain(format::tokens(m.prompt_tokens)),
            PlainCell::plain(format::tokens(m.completion_tokens)),
        ],
        Dim::Origin => {
            let mut cells = origin_cells(m);
            cells.insert(0, text_cell(row.key.clone(), palette.text));
            cells
        }
    }
}

/// 表尾 TOTAL 行（= 端点 `totals`；永远可对账）。
fn detail_total(dim: Dim, report: &UsageResponse, palette: &ThemePalette) -> Vec<PlainCell> {
    let m = &report.totals;
    let cells = match dim {
        Dim::Day => vec![
            count(m.sessions),
            count(m.user_messages),
            count(m.llm_calls),
            count(m.tool_calls),
            format::tokens(m.prompt_tokens),
            cache_text(m),
            format::tokens(m.completion_tokens),
            work_time(m.work_time_ms()),
        ],
        Dim::Model => vec![
            String::new(),
            count(m.llm_calls),
            format::tokens(m.prompt_tokens),
            cache_text(m),
            format::tokens(m.completion_tokens),
        ],
        Dim::Session => vec![
            String::new(),
            String::new(),
            count(m.user_messages),
            count(m.llm_calls),
            format::tokens(m.prompt_tokens),
            cache_text(m),
            format::tokens(m.completion_tokens),
        ],
        Dim::Tool => vec![
            count(m.tool_calls),
            count(m.tool_errors),
            avg_time(m.tool_time_ms, m.tool_calls),
        ],
        Dim::Workspace | Dim::Agent => vec![
            count(m.sessions),
            count(m.llm_calls),
            format::tokens(m.prompt_tokens),
            format::tokens(m.completion_tokens),
        ],
        // `origin` 的合计就是「我 + 我的 AI」那一行本身，键写 `total`。
        Dim::Origin => Vec::new(),
    };
    let ink = Style::new().fg(palette.accent);
    let mut row = vec![PlainCell::styled("TOTAL".to_string(), ink)];
    row.extend(cells.into_iter().map(|text| PlainCell::styled(text, ink)));
    if dim == Dim::Origin {
        row.extend(origin_cells(m).into_iter().map(|cell| PlainCell {
            text: cell.text,
            style: Some(ink),
        }));
    }
    row
}

fn detail_columns(dim: Dim) -> Vec<PlainColumn> {
    let headers: &[&str] = match dim {
        Dim::Day => &[
            "DATE", "ACTIVE", "MSGS", "LLM", "TOOLS", "IN", "CACHE", "OUT", "TIME",
        ],
        Dim::Model => &["MODEL", "PROVIDER", "CALLS", "IN", "CACHE", "OUT"],
        Dim::Session => &[
            "SESSION", "TITLE", "WHO", "MSGS", "CALLS", "IN", "CACHE", "OUT",
        ],
        Dim::Tool => &["TOOL", "CALLS", "ERR", "AVG"],
        Dim::Workspace => &["WORKSPACE", "SESSIONS", "CALLS", "IN", "OUT"],
        Dim::Agent => &["AGENT", "SESSIONS", "CALLS", "IN", "OUT"],
        Dim::Origin => &[
            "WHO",
            "CREATED",
            "ACTIVE",
            "MSGS",
            "LLM CALLS",
            "TOOL CALLS",
            "IN",
            "CACHE",
            "OUT",
        ],
    };
    headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            let kind = match *header {
                "TITLE" => ColumnKind::Narrative,
                "WORKSPACE" => ColumnKind::TokenHeavy,
                _ => ColumnKind::Compact,
            };
            if index == 0 && dim == Dim::Session {
                // 会话 id 是可拷贝句柄：表格引擎绝不截断它（窄终端里由 TITLE 先让宽）。
                PlainColumn::keep_natural(*header, kind)
            } else {
                PlainColumn::capped(*header, kind, 96)
            }
        })
        .collect()
}

/// `--tree` 的行序与前缀：血缘交给 `shared::session_tree`（孤儿升根、环防御都在里面）。
fn ordered_rows(report: &UsageResponse, tree: bool) -> Vec<(usize, String)> {
    if !tree {
        return (0..report.rows.len())
            .map(|index| (index, String::new()))
            .collect();
    }
    let input: Vec<TreeRow<'_>> = report
        .rows
        .iter()
        .map(|row| TreeRow {
            id: row.key.as_str(),
            parent: row.parent_session_id.as_deref(),
            pinned: false,
            pin_added_at: None,
            last_interaction: None,
            // 用量报告里没有会话状态：`None` = 未知（不参与注意力、不计 working、
            // 按存活处理）。
            status: None,
        })
        .collect();
    build_forest(&input)
        .into_iter()
        .map(|entry| (entry.index, entry.prefix))
        .collect()
}

// ============================================================
// 数字记法
// ============================================================

/// 命中率（`cached / prompt`）整数百分比；没有输入 → `—`（不写 `0%`）。
fn cache_text(measures: &UsageMeasures) -> String {
    match format::hit_rate(measures.cached_tokens, measures.prompt_tokens) {
        Some(rate) => format!("{rate:.0}%"),
        None => NONE_MARK.to_string(),
    }
}

/// 一次调用的平均耗时：`840ms` / `6.8s` / `2m`；没有调用 → `—`。
///
/// 秒档保留一位小数：整数除法会把 6.8s 印成 `6s`，而「平均耗时」正是要比较的量。
fn avg_time(total_ms: i64, calls: i64) -> String {
    if calls <= 0 {
        return NONE_MARK.to_string();
    }
    let average = total_ms.max(0) / calls;
    if average < 1_000 {
        format!("{average}ms")
    } else if average < 60_000 {
        format!("{:.1}s", average as f64 / 1_000.0)
    } else {
        work_time(average)
    }
}

/// `12 weeks` / `1 week`（热力图的列数就是周数）。
fn weeks_text(weeks: usize) -> String {
    format!("{weeks} week{}", if weeks == 1 { "" } else { "s" })
}

/// `provider/model` → 两半（`provider` 名不含 `/`；没有 `/` 时全归 provider 侧）。
fn split_model_key(key: &str) -> (&str, &str) {
    match key.split_once('/') {
        Some((provider, model)) => (provider, model),
        None => ("", key),
    }
}

/// MODEL 列：展示名优先，回落 `provider/model` 的调用名，再回落整个键。
fn model_name(row: &UsageRow) -> String {
    row.label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(common::single_line)
        .unwrap_or_else(|| {
            let (_, model) = split_model_key(&row.key);
            if model.is_empty() {
                NONE_MARK.to_string()
            } else {
                model.to_string()
            }
        })
}

/// 空键（没声明 workspace / 模板）在表里显示 `(none)`。
fn namespace_text(key: &str) -> String {
    if key.trim().is_empty() {
        "(none)".to_string()
    } else {
        common::single_line(key)
    }
}

/// 活跃天数 / 连续活跃 / 最长连续（`days` 是按日期升序的 `(日期, 有没有活动)`）。
fn streak_stats(days: &[(NaiveDate, bool)], today: NaiveDate) -> (usize, usize, usize) {
    let active_days = days.iter().filter(|(_, active)| *active).count();
    let mut longest = 0usize;
    let mut run = 0usize;
    for (_, active) in days {
        if *active {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    // 窗口没延伸到「昨天」→ 这是一段历史窗口，不存在「当前连续」（否则会把早已中断的
    // 一段旧连续报成正在进行中）。
    let historical = days
        .last()
        .is_some_and(|(day, _)| (today - *day).num_days() > 1);
    if historical {
        return (active_days, 0, longest);
    }
    // 连续活跃：从今天往回数；今天还没干活就从昨天起（今天的格子没填是常态）。
    let mut streak = 0usize;
    for (day, active) in days.iter().rev() {
        if *day > today {
            continue;
        }
        if *active {
            streak += 1;
        } else if *day == today {
            continue;
        } else {
            break;
        }
    }
    (active_days, streak, longest)
}

// ============================================================
// 小工具
// ============================================================

/// 空态的一句话：完全没有数据 vs 窗口内没活动，是两种不同的结论。
fn empty_note(report: &UsageResponse) -> Option<&'static str> {
    // 只看流水：窗口内建过会话（`sessions_created`）不算「有数据」。
    if report.totals.has_flow() {
        return None;
    }
    Some(if report.range.data_since.is_some() {
        "No activity in this range (try --since all)."
    } else {
        "No usage data yet — your first conversation fills the first cell."
    })
}

fn parse_day(raw: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").map_err(|_| {
        anyhow::anyhow!("the gateway reported an unparsable date: {raw:?} (expect YYYY-MM-DD)")
    })
}

/// 标题行：左标题、右范围（宽度够就右对齐，不够就一个空格隔开）。
fn title_line(left: &str, rights: &[String], width: usize) -> String {
    for right in rights {
        let used = left.chars().count() + right.chars().count();
        if let Some(pad) = width.checked_sub(used)
            && pad >= 2
        {
            return format!("{left}{}{right}", " ".repeat(pad));
        }
    }
    // 一种都装不下：留左标题 + 最短的那种，裁到预算内（绝不越过终端宽度）。
    let right = rights.last().map(String::as_str).unwrap_or_default();
    let budget = width.saturating_sub(left.chars().count() + 2);
    format!("{left}  {}", right.chars().take(budget).collect::<String>())
}

fn text_cell(text: String, color: ratatui::style::Color) -> PlainCell {
    PlainCell::styled(text, Style::new().fg(color))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::UsageRange;

    fn session(id: &str, name: Option<&str>, parent: Option<&str>) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: name.map(str::to_string),
            created_at: None,
            template_name: None,
            workspace: None,
            parent_session_id: parent.map(str::to_string),
            last_interaction: None,
            status: "idle".into(),
            tags: Vec::new(),
            tag_meta: Default::default(),
            model_id: None,
            model_name: None,
            provider_name: None,
            model_display_name: None,
        }
    }

    fn date(raw: &str) -> NaiveDate {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d").unwrap()
    }

    fn out() -> TableOutput {
        TableOutput {
            width: common::PIPE_WIDTH,
            color: false,
        }
    }

    fn opts() -> UsageOpts {
        UsageOpts {
            since: "12w".into(),
            until: "today".into(),
            by: None,
            metric: "tokens".into(),
            session: None,
            tree: false,
            limit: DEFAULT_LIMIT,
        }
    }

    fn measures(prompt: i64, completion: i64, cached: i64) -> UsageMeasures {
        UsageMeasures {
            sessions: 1,
            user_messages: 2,
            llm_calls: 3,
            tool_calls: 4,
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
            llm_time_ms: 3_600_000,
            tool_time_ms: 600_000,
            ..UsageMeasures::default()
        }
    }

    fn row(key: &str, measures: UsageMeasures) -> UsageRow {
        UsageRow {
            key: key.to_string(),
            label: None,
            parent_session_id: None,
            measures,
        }
    }

    fn report(group_by: &str, rows: Vec<UsageRow>, totals: UsageMeasures) -> UsageResponse {
        UsageResponse {
            range: UsageRange {
                since: "2026-08-03".into(),
                until: "2026-10-25".into(),
                data_since: Some("2026-08-03".into()),
            },
            group_by: group_by.into(),
            totals,
            rows,
            truncated: false,
        }
    }

    // ---- 参数闸门 ----

    #[test]
    fn the_plan_translates_human_dates_into_gateway_days() {
        let today = date("2026-10-23");
        assert_eq!(
            parse_when("today", today, "since").unwrap(),
            When::Day(today)
        );
        assert_eq!(
            parse_when("yesterday", today, "since").unwrap(),
            When::Day(date("2026-10-22"))
        );
        assert_eq!(
            parse_when("7d", today, "since").unwrap(),
            When::Day(date("2026-10-16"))
        );
        assert_eq!(
            parse_when("4w", today, "since").unwrap(),
            When::Day(date("2026-09-25"))
        );
        assert_eq!(
            parse_when("3m", today, "since").unwrap(),
            When::Day(date("2026-07-25"))
        );
        assert_eq!(
            parse_when("1y", today, "since").unwrap(),
            When::Day(date("2025-10-23"))
        );
        assert_eq!(
            parse_when("12w", today, "since").unwrap(),
            When::Day(date("2026-07-31"))
        );
        assert_eq!(
            parse_when("2026-10-01", today, "since").unwrap(),
            When::Day(date("2026-10-01"))
        );
        assert_eq!(parse_when("all", today, "since").unwrap(), When::All);
        // 大小写不敏感（`ALL` / `Today` 都是人类会敲的）。
        assert_eq!(parse_when("ALL", today, "since").unwrap(), When::All);
        assert_eq!(
            parse_when("Today", today, "since").unwrap(),
            When::Day(today)
        );
    }

    #[test]
    fn malformed_dates_and_spans_are_rejected_before_the_gateway() {
        let today = date("2026-10-23");
        for bad in ["", "0d", "d", "-3d", "3x", "2026-13-01", "next week", "3d5"] {
            let error = parse_when(bad, today, "since").unwrap_err().to_string();
            assert!(error.contains("--since"), "{bad:?} → {error}");
        }
        // `--until all` 不是一个合法的范围终点。
        let error = Plan::resolve(&UsageOpts {
            until: "all".into(),
            ..opts()
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("--until"), "{error}");
    }

    #[test]
    fn the_plan_gates_limit_by_and_tree_before_connecting() {
        let error = Plan::resolve(&UsageOpts { limit: 0, ..opts() })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("--limit must be between 1 and 200"),
            "{error}"
        );
        assert!(
            Plan::resolve(&UsageOpts {
                limit: 201,
                ..opts()
            })
            .is_err()
        );
        assert!(Plan::resolve(&UsageOpts { limit: 1, ..opts() }).is_ok());

        let error = Plan::resolve(&UsageOpts {
            by: Some("days".into()),
            ..opts()
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("--by must be one of"), "{error}");
        assert!(error.contains("workspace"), "取值域全列：{error}");

        let error = Plan::resolve(&UsageOpts {
            metric: "tokens!".into(),
            ..opts()
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("--metric must be one of"), "{error}");

        let error = Plan::resolve(&UsageOpts {
            tree: true,
            ..opts()
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("--tree requires --session"), "{error}");
    }

    #[test]
    fn a_range_that_ends_before_it_starts_is_rejected() {
        let error = Plan::resolve(&UsageOpts {
            since: "2026-10-20".into(),
            until: "2026-10-01".into(),
            ..opts()
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("is after --until"), "{error}");
    }

    #[test]
    fn since_all_is_forwarded_verbatim() {
        let plan = Plan::resolve(&UsageOpts {
            since: "all".into(),
            ..opts()
        })
        .unwrap();
        assert_eq!(plan.since, "all");
        let plan = Plan::resolve(&UsageOpts {
            since: "7d".into(),
            until: "2026-10-25".into(),
            ..opts()
        })
        .unwrap();
        let expected = (Local::now().date_naive() - ChronoDuration::days(7))
            .format("%Y-%m-%d")
            .to_string();
        assert_eq!(plan.since, expected);
        assert_eq!(plan.until, "2026-10-25");
    }

    // ---- --metric 映射 ----

    #[test]
    fn the_metric_picks_one_axis_of_the_same_row() {
        let m = measures(1_200_000, 45_600, 936_000);
        assert_eq!(Metric::Tokens.value(&m), 1_245_600);
        assert_eq!(Metric::Messages.value(&m), 2);
        assert_eq!(Metric::Calls.value(&m), 3);
        assert_eq!(Metric::Tools.value(&m), 4);
        assert_eq!(Metric::Sessions.value(&m), 1);
        assert!(Metric::parse("calls").is_some());
        assert!(Metric::parse("call").is_none());
    }

    // ---- 数字记法 ----

    /// 记法本身（千分位 / 工时）在 `util::format` 的单测里；这里只钉用量视图的两处派生值。
    #[test]
    fn derived_readings_use_a_dash_instead_of_zero() {
        assert_eq!(cache_text(&measures(1_000, 10, 990)), "99%");
        assert_eq!(cache_text(&UsageMeasures::default()), NONE_MARK);
        assert_eq!(avg_time(0, 0), NONE_MARK);
        assert_eq!(avg_time(1_680, 2), "840ms");
        assert_eq!(avg_time(6_800, 1), "6.8s", "秒档保留一位，不截成 6s");
        assert_eq!(avg_time(130_000, 1), "2m");
    }

    // ---- 连续与活跃 ----

    #[test]
    fn streaks_count_back_from_today_and_fall_back_to_yesterday() {
        let today = date("2026-10-23");
        let days = vec![
            (date("2026-10-18"), true),
            (date("2026-10-19"), true),
            (date("2026-10-20"), false),
            (date("2026-10-21"), true),
            (date("2026-10-22"), true),
            (date("2026-10-23"), true),
        ];
        assert_eq!(streak_stats(&days, today), (5, 3, 3));

        // 今天还没活动 → 从昨天往回数。
        let trailing = vec![
            (date("2026-10-21"), true),
            (date("2026-10-22"), true),
            (date("2026-10-23"), false),
        ];
        assert_eq!(streak_stats(&trailing, today), (2, 2, 2));

        // 整天没活动 → 0 连续，0 活跃。
        let quiet = vec![(date("2026-10-23"), false)];
        assert_eq!(streak_stats(&quiet, today), (0, 0, 0));
    }

    #[test]
    fn a_window_that_ends_in_the_past_does_not_borrow_todays_streak() {
        let today = date("2026-10-23");
        let days = vec![
            (date("2026-10-20"), true),
            (date("2026-10-21"), true),
            (date("2026-10-22"), false),
        ];
        // 窗口最后一天是 10-22（没活动）→ 连续为 0；活跃天数与最长连续照算。
        assert_eq!(streak_stats(&days, today), (2, 0, 2));
    }

    // ---- 仪表盘 ----

    fn dashboard() -> String {
        let day = report(
            "day",
            vec![
                row("2026-10-22", measures(1_100_000, 40_000, 1_000_000)),
                row("2026-10-23", measures(1_200_000, 45_600, 936_000)),
            ],
            measures(2_300_000, 85_600, 1_936_000),
        );
        let origin = report(
            "origin",
            vec![
                row("me", measures(2_000_000, 80_000, 1_900_000)),
                row("agents", measures(300_000, 5_600, 36_000)),
            ],
            measures(2_300_000, 85_600, 1_936_000),
        );
        let model = report(
            "model",
            vec![UsageRow {
                key: "gateway/deepseek-flash".into(),
                label: Some("DeepSeek-Flash".into()),
                parent_session_id: None,
                measures: measures(2_300_000, 85_600, 1_936_000),
            }],
            measures(2_300_000, 85_600, 1_936_000),
        );
        format_dashboard(
            &day,
            &origin,
            &model,
            Metric::Tokens,
            &ThemePalette::default(),
            &out(),
        )
        .unwrap()
    }

    #[test]
    fn the_dashboard_speaks_in_five_sections() {
        let text = dashboard();
        // 1) 标题行 + 范围 + 周数（右对齐到 120 列）。
        let title = text.lines().next().unwrap();
        assert!(title.starts_with("WING USAGE"), "{title}");
        assert!(title.contains("2026-08-03 → 2026-10-25"), "{title}");
        assert!(title.ends_with("12 weeks"), "{title}");
        assert_eq!(title.chars().count(), common::PIPE_WIDTH, "右对齐");
        // 2) 热力图（含图例）与 3) 统计行。
        assert!(text.contains("less · ░ ▒ ▓ █ more"), "{text}");
        assert!(text.contains("2 active days · streak"), "{text}");
        // 4) 审计起点提示（data_since == since → 不打印）。
        assert!(!text.contains("audit starts"), "{text}");
        // 5) 两张表：WHO 有 total 行，MODEL 用展示名。
        for header in [
            "WHO",
            "CREATED",
            "ACTIVE",
            "MSGS",
            "LLM CALLS",
            "TOOL CALLS",
        ] {
            assert!(text.contains(header), "缺列 {header}：\n{text}");
        }
        assert!(text.contains("total"), "{text}");
        assert!(text.contains("DeepSeek-Flash"), "{text}");
        assert!(text.contains("2.3M"), "tokens 用紧凑记法：\n{text}");
    }

    #[test]
    fn the_audit_start_hint_only_shows_when_the_flow_starts_after_the_range() {
        let mut day = report("day", vec![], measures(0, 0, 0));
        day.range.data_since = Some("2026-10-20".into());
        let origin = report("origin", vec![], measures(0, 0, 0));
        let model = report("model", vec![], measures(0, 0, 0));
        let text = format_dashboard(
            &day,
            &origin,
            &model,
            Metric::Tokens,
            &ThemePalette::default(),
            &out(),
        )
        .unwrap();
        assert!(
            text.contains(
                "token / call / message audit starts 2026-10-20; earlier days only have session counts"
            ),
            "{text}"
        );
        // 老网关不给 data_since → 不打印这一行。
        day.range.data_since = None;
        let text = format_dashboard(
            &day,
            &origin,
            &model,
            Metric::Tokens,
            &ThemePalette::default(),
            &out(),
        )
        .unwrap();
        assert!(!text.contains("audit starts"), "{text}");
    }

    #[test]
    fn an_empty_report_says_which_kind_of_empty_it_is() {
        let empty = report("day", vec![], UsageMeasures::default());
        assert_eq!(
            empty_note(&empty),
            Some("No activity in this range (try --since all).")
        );
        let mut virgin = report("day", vec![], UsageMeasures::default());
        virgin.range.data_since = None;
        assert_eq!(
            empty_note(&virgin),
            Some("No usage data yet — your first conversation fills the first cell.")
        );
        let busy = report("day", vec![], measures(1, 1, 0));
        assert_eq!(empty_note(&busy), None);

        // 只有目录事实（窗口内建过会话、没有流水）→ 仍然是「没有数据」。
        let catalog_only = report(
            "day",
            vec![],
            UsageMeasures {
                sessions_created: 3,
                ..UsageMeasures::default()
            },
        );
        assert!(
            empty_note(&catalog_only).is_some(),
            "`sessions_created` 是目录事实，不是读数"
        );

        // 空态只打标题 + 一句话，不打空表。
        let text = format_dashboard(
            &virgin,
            &virgin,
            &virgin,
            Metric::Tokens,
            &ThemePalette::default(),
            &out(),
        )
        .unwrap();
        assert!(text.contains("No usage data yet"), "{text}");
        assert!(!text.contains("WHO"), "空态不打表：\n{text}");
    }

    #[test]
    fn today_is_printed_only_when_it_has_something_to_report() {
        let today = Local::now().date_naive();
        let key = today.format("%Y-%m-%d").to_string();
        let mut day = report(
            "day",
            vec![row(&key, measures(1_000, 10, 990))],
            measures(1_000, 10, 990),
        );
        day.range.since = key.clone();
        day.range.until = key.clone();
        let origin = report("origin", vec![], measures(1_000, 10, 990));
        let model = report("model", vec![], measures(1_000, 10, 990));
        let text = format_dashboard(
            &day,
            &origin,
            &model,
            Metric::Tokens,
            &ThemePalette::default(),
            &out(),
        )
        .unwrap();
        assert!(text.contains("TODAY"), "{text}");
        assert!(text.contains("2 messages"), "{text}");
        assert!(text.contains("(99% cached)"), "{text}");

        // 今天在窗口内但只建过会话（`sessions_created`）→ 不打印这一行。
        day.rows = vec![row(
            &key,
            UsageMeasures {
                sessions_created: 2,
                ..UsageMeasures::default()
            },
        )];
        day.totals = measures(1_000, 10, 990);
        let text = format_dashboard(
            &day,
            &origin,
            &model,
            Metric::Tokens,
            &ThemePalette::default(),
            &out(),
        )
        .unwrap();
        assert!(!text.contains("TODAY"), "{text}");
    }

    // ---- 明细 ----

    fn detail(dim: Dim, rows: Vec<UsageRow>, totals: UsageMeasures) -> String {
        let report = report(dim.as_str(), rows, totals);
        format_detail(
            &report,
            dim,
            &Plan::resolve(&opts()).unwrap(),
            None,
            &ThemePalette::default(),
            &out(),
        )
    }

    #[test]
    fn a_catalog_only_window_draws_its_rows_with_a_footnote() {
        // 只建过会话、还没有流水：后端给的行照常出表（吞掉它们会让 `--since all`
        // 变成一句空话——换窗口也补不出流水）。
        let catalog_only = UsageMeasures {
            sessions_created: 2,
            ..UsageMeasures::default()
        };
        let text = detail(
            Dim::Day,
            vec![row("2026-08-03", catalog_only.clone())],
            catalog_only,
        );
        assert!(!text.contains("No activity in this range"), "{text}");
        assert!(text.contains("2026-08-03"), "{text}");
        assert!(text.contains("TOTAL"), "{text}");
        assert!(
            text.contains("only sessions created in this range (no flow yet)"),
            "{text}"
        );

        // 一行都没有 → 仍是空态文案，且不带那句表尾。
        assert_eq!(
            detail(Dim::Session, vec![], UsageMeasures::default()),
            "No activity in this range (try --since all).\n"
        );

        // 稠密维度的零行不是「有数据」（`day` 每天一行、`origin` 恒两行）：
        // 首跑不该被画成一整窗全零的表，更不该附「建过会话」那句话。
        let zero = row("2026-08-03", UsageMeasures::default());
        assert_eq!(
            detail(Dim::Day, vec![zero.clone()], UsageMeasures::default()),
            "No activity in this range (try --since all).\n"
        );
        assert_eq!(
            detail(
                Dim::Origin,
                vec![zero.clone(), zero],
                UsageMeasures::default()
            ),
            "No activity in this range (try --since all).\n"
        );

        // 有流水 → 表尾那句不出现。
        let busy = detail(
            Dim::Day,
            vec![row("2026-08-03", measures(1_000, 10, 990))],
            measures(1_000, 10, 990),
        );
        assert!(!busy.contains("only sessions created"), "{busy}");
    }

    #[test]
    fn every_dimension_shows_its_own_columns_and_a_total_row() {
        // 表头唯一来源是 `detail_columns`：加一列只改生产代码，测试跟着走。
        for dim in Dim::ALL {
            let columns = detail_columns(dim);
            let headers: Vec<&str> = columns.iter().map(|c| c.header.as_str()).collect();
            assert!(!headers.is_empty(), "{dim:?} 没有列");

            let report = report(
                dim.as_str(),
                vec![row("k", measures(1_000, 10, 990))],
                measures(1_000, 10, 990),
            );
            let plan = Plan::resolve(&opts()).unwrap();
            let text = format_detail(&report, dim, &plan, None, &ThemePalette::default(), &out());
            for header in &headers {
                assert!(text.contains(header), "{dim:?} 缺列 {header}：\n{text}");
            }
            assert!(text.contains("TOTAL"), "{dim:?} 缺 TOTAL：\n{text}");

            // 数据行与 TOTAL 行的格子数必须等于列数（`debug_assert` 只在 debug 生效）。
            let mut cells = detail_row(dim, &report.rows[0], "", None, &ThemePalette::default());
            assert_eq!(cells.len(), columns.len(), "{dim:?} 数据行");
            cells = detail_total(dim, &report, &ThemePalette::default());
            assert_eq!(cells.len(), columns.len(), "{dim:?} TOTAL 行");
        }
    }

    /// 窄终端裁列时标题必须自己说清楚画了几列（`total` 已加、除法已删，这条钉文案）。
    #[test]
    fn a_narrow_dashboard_says_how_many_weeks_are_shown() {
        // 42 周窗口 + 60 列：热力图装不下，标题退到「→ until · N weeks (last N shown)」。
        let mut day = report(
            "day",
            vec![row("2026-01-05", measures(1_100_000, 40_000, 1_000_000))],
            measures(1_100_000, 40_000, 1_000_000),
        );
        day.range.since = "2026-01-05".into();
        day.range.until = "2026-10-25".into();
        day.range.data_since = None;
        let origin = report("origin", vec![], measures(1_100_000, 40_000, 1_000_000));
        let model = report("model", vec![], measures(1_100_000, 40_000, 1_000_000));
        let narrow = TableOutput {
            width: 60,
            color: false,
        };
        let text = format_dashboard(
            &day,
            &origin,
            &model,
            Metric::Tokens,
            &ThemePalette::default(),
            &narrow,
        )
        .unwrap();
        let title = text.lines().next().unwrap();
        assert!(title.chars().count() <= 60, "标题不越宽度：{title:?}");
        assert!(title.contains("42 weeks"), "窗口跨 42 周：{title}");
        let shown: usize = title
            .split("(last ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|digits| digits.parse().ok())
            .unwrap_or_else(|| panic!("缺 (last N shown)：{title:?}"));
        // 与真正画出来的格子数对账（第一行格子：去掉 gutter 后的非空格字符数）。
        let first_grid_row = text
            .lines()
            .find(|line| line.starts_with("  ") && line.contains(['·', '░', '▒', '▓', '█']))
            .expect("a grid row");
        // 只数档位字形：裁剪时最左那一格是 `…` 标记，不是一列。
        let cells = first_grid_row
            .chars()
            .skip(INDENT.len() + 6)
            .filter(|ch| matches!(ch, '·' | '░' | '▒' | '▓' | '█'))
            .count();
        assert_eq!(shown, cells, "标题里的列数必须等于画出来的列数：{title:?}");
    }

    #[test]
    fn the_model_row_splits_the_composite_key_and_prefers_the_display_name() {
        let labelled = UsageRow {
            key: "gateway/deepseek-flash".into(),
            label: Some("DeepSeek-Flash".into()),
            parent_session_id: None,
            measures: measures(1_000, 10, 990),
        };
        let text = detail(Dim::Model, vec![labelled], measures(1_000, 10, 990));
        assert!(text.contains("DeepSeek-Flash"), "{text}");
        assert!(text.contains("gateway"), "{text}");

        // 没有展示名 → 回落 key 的调用名；没有 provider 前缀也不炸。
        let bare = UsageRow {
            key: "deepseek-flash".into(),
            label: None,
            parent_session_id: None,
            measures: measures(1_000, 10, 990),
        };
        assert_eq!(model_name(&bare), "deepseek-flash");
        assert_eq!(split_model_key(&bare.key), ("", "deepseek-flash"));
    }

    #[test]
    fn the_session_row_joins_the_title_and_the_origin_and_shows_dashes_without_them() {
        let rows = vec![
            UsageRow {
                key: "sid-root".into(),
                label: None,
                parent_session_id: None,
                measures: measures(1_000, 10, 990),
            },
            UsageRow {
                key: "sid-child".into(),
                label: None,
                parent_session_id: Some("sid-root".into()),
                measures: measures(1_000, 10, 990),
            },
        ];
        let report = report("session", rows, measures(2_000, 20, 1_980));
        let plan = Plan::resolve(&opts()).unwrap();
        let sessions = vec![
            session("sid-root", Some("the root"), None),
            session("sid-child", None, Some("sid-root")),
        ];
        let text = format_detail(
            &report,
            Dim::Session,
            &plan,
            Some(&sessions),
            &ThemePalette::default(),
            &out(),
        );
        assert!(text.contains("the root"), "{text}");
        assert!(text.contains("me"), "{text}");
        assert!(text.contains("agents"), "{text}");
        // 没有名字的子会话显示 `—`（不重复 id、不猜标题）。
        assert!(text.contains(NONE_MARK), "{text}");

        // 列表里查不到（已归档 / 已删除）→ TITLE 与 WHO 都是 `—`，不猜。
        let text = format_detail(
            &report,
            Dim::Session,
            &plan,
            None,
            &ThemePalette::default(),
            &out(),
        );
        assert!(text.contains(NONE_MARK), "{text}");
        assert!(!text.contains("agents"), "{text}");
    }

    #[test]
    fn tree_mode_nests_the_rows_under_the_scoped_session() {
        let rows = vec![
            UsageRow {
                key: "root".into(),
                label: None,
                parent_session_id: None,
                measures: measures(1_000, 10, 990),
            },
            UsageRow {
                key: "child".into(),
                label: None,
                parent_session_id: Some("root".into()),
                measures: measures(500, 5, 400),
            },
            UsageRow {
                key: "grandchild".into(),
                label: None,
                parent_session_id: Some("child".into()),
                measures: measures(100, 1, 90),
            },
        ];
        let report = report("session", rows, measures(1_600, 16, 1_480));
        let plan = Plan::resolve(&UsageOpts {
            by: Some("session".into()),
            session: Some("root".into()),
            tree: true,
            ..opts()
        })
        .unwrap();
        let text = format_detail(
            &report,
            Dim::Session,
            &plan,
            None,
            &ThemePalette::default(),
            &out(),
        );
        assert!(text.contains("└─ child"), "{text}");
        assert!(
            text.contains("   └─ grandchild"),
            "孙辈再缩进一层：\n{text}"
        );

        // 平铺模式：不画任何前缀。
        let flat = format_detail(
            &report,
            Dim::Session,
            &Plan::resolve(&opts()).unwrap(),
            None,
            &ThemePalette::default(),
            &out(),
        );
        assert!(!flat.contains("├─"), "{flat}");
    }

    #[test]
    fn a_truncated_breakdown_says_so_without_inventing_a_count() {
        let mut report = report(
            "model",
            vec![row("a/b", measures(1, 1, 0))],
            measures(1, 1, 0),
        );
        report.truncated = true;
        let text = format_detail(
            &report,
            Dim::Model,
            &Plan::resolve(&opts()).unwrap(),
            None,
            &ThemePalette::default(),
            &out(),
        );
        assert!(text.contains("… more model (raise --limit)"), "{text}");
        assert!(!text.contains("0 more"), "不假装知道还剩几行：{text}");
    }

    #[test]
    fn a_tool_row_averages_the_time_per_call() {
        let row = UsageRow {
            key: "Bash".into(),
            label: None,
            parent_session_id: None,
            measures: UsageMeasures {
                tool_calls: 4,
                tool_errors: 1,
                tool_time_ms: 3_360,
                ..UsageMeasures::default()
            },
        };
        let totals = UsageMeasures {
            tool_calls: 4,
            tool_errors: 1,
            tool_time_ms: 3_360,
            ..UsageMeasures::default()
        };
        let text = detail(Dim::Tool, vec![row], totals);
        assert!(
            text.contains("840ms"),
            "平均耗时 = tool_time/calls：\n{text}"
        );
        assert!(text.contains("TOTAL"), "{text}");
    }

    #[test]
    fn empty_keys_read_as_none_rather_than_a_blank_cell() {
        assert_eq!(namespace_text(""), "(none)");
        assert_eq!(namespace_text("   "), "(none)");
        assert_eq!(namespace_text("/Users/me/proj"), "/Users/me/proj");
    }

    #[test]
    fn the_title_line_right_aligns_and_falls_back_to_a_shorter_variant() {
        let rights = ["AAAA BBBB".to_string(), "BBBB".to_string()];
        assert_eq!(title_line("A", &rights, 12), "A  AAAA BBBB");
        // 装不下时退到下一级，仍然右对齐。
        assert_eq!(title_line("A", &rights, 8), "A   BBBB");
        // 一种都装不下：裁到宽度内，绝不越界。
        let narrow = title_line("A", &rights, 4);
        assert_eq!(narrow, "A  B");
        assert!(narrow.chars().count() <= 4, "{narrow:?}");
    }

    #[test]
    fn errors_name_the_session_question_or_the_stale_gateway() {
        let not_found = || ApiClientError::Api {
            status: 404,
            detail: "session not found: 'ghost'".into(),
            body: None,
        };
        assert_eq!(
            classify(not_found(), Some("ghost")).to_string(),
            "session not found: ghost"
        );
        let stale = classify(not_found(), None).to_string();
        assert!(stale.contains("does not serve /api/usage"), "{stale}");
        assert!(stale.contains("wing stop && wing start"), "{stale}");
        let method = classify(
            ApiClientError::Api {
                status: 405,
                detail: "Method Not Allowed".into(),
                body: None,
            },
            None,
        )
        .to_string();
        assert!(method.contains("does not serve /api/usage"), "{method}");
        // 5xx / 传输错误原样带出。
        let boom = classify(
            ApiClientError::Api {
                status: 500,
                detail: "boom".into(),
                body: None,
            },
            Some("sid"),
        )
        .to_string();
        assert!(boom.contains("boom"), "{boom}");
    }

    /// 一个只回 500 的最小 HTTP 桩（探测请求自己失败的一种），返回它的 base URL。
    ///
    /// 用桩而不是「bind 后 drop」：后者靠端口不会被别人瞬占，是概率性假设。
    fn spawn_500_stub() -> String {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\
                      Connection: close\r\n\r\n",
                );
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_failed_probe_is_not_read_as_a_missing_session() {
        // 探测请求自己失败（这里是 5xx）什么也证明不了：原样报原始错误，
        // 不能反过来断言「这个会话不存在」。
        let http = GatewayApiClient::new(spawn_500_stub(), None).unwrap();
        let mut opts = opts();
        opts.session = Some("ghost".into());
        let plan = Plan::resolve(&opts).unwrap();
        let not_found = ApiClientError::Api {
            status: 404,
            detail: "Not Found".into(),
            body: None,
        };

        let message = diagnose(&http, &plan, None, None, not_found)
            .await
            .to_string();
        // 原样带出端点的错误（探测失败之后什么也不能断言）。
        assert!(message.starts_with("API error (404)"), "{message}");
        assert!(!message.contains("session not found"), "{message}");
    }
}
