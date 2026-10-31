//! `wing ps` — list sessions, and `wing info` — session runtime info.
//!
//! `wing ps` lists sessions (like `docker ps` or `kubectl get pods`).
//! `wing info <sid>` shows detailed runtime state for a single session.
//!
//! The human-readable session list is rendered through the shared table
//! engine (`render::table`) with the same skin as TUI markdown tables; every
//! measurement is display columns (CJK-safe). `--json` is the machine surface:
//! a flat list in the existing order — the three human modes only shape the
//! table (the lineage feature adds just the `parent_session_id` key to each
//! row).
//!
//! Three human modes ([`TableMode`]), one row set each:
//!
//! - **level** (default): one row per task — level 0 of the lineage forest,
//!   descendants folded into a `CREW` column. `--parent <sid>` drills one
//!   level down, `--tree` draws the whole forest, `--flat` prints the flat
//!   list this command used to print. `--tag` / `--parent` narrow the row set
//!   to a tag subset / a child layer (see `level_rows`), and only the root
//!   level carries the "N crew folded" footer;
//! - **flat**: every session on its own row;
//! - **tree**: the lineage forest with `├─` / `└─` prefixes.
//!
//! Ordering, crew rollup and the liveness closure all come from
//! [`crate::shared::session_tree`] — the CLI consumes them, it does not
//! reimplement them.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use chrono::Datelike;
use chrono::Local;
use chrono::NaiveDateTime;
use ratatui::style::Style;
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::UsageQuery;
use wing_api_client::models::{
    SessionInfo, SessionInfoResponse, SessionUsage, UsageMeasures, UsageResponse,
};

use crate::config::AppConfig;
use crate::config::ColorPreset;
use crate::config::ThemePalette;
use crate::protocol::SessionStatus;
use crate::render::table::ColumnKind;
use crate::render::table::plain::{PlainCell, PlainColumn, PlainTable};
use crate::shared::pinning::{is_pinned, pin_added_at, pin_key};
use crate::shared::session_tree::{Crew, Forest, TreeRow};
use crate::tui::appearance;
use crate::util::format;
use crate::util::format::{NONE_MARK, count, work_time};

use super::common;
use super::common::TableOutput;
use super::usage;

/// Entry point for `wing ps`.
pub async fn run_ps(
    all: bool,
    tags: &[String],
    parent: Option<&str>,
    mode: TableMode,
    json: bool,
    watch: bool,
) -> ExitCode {
    if watch {
        return run_ps_watch(all, tags, parent, mode, json).await;
    }
    match fetch_sessions(parent).await {
        Ok(sessions) => {
            if json {
                emit_json(sessions, all, tags, parent);
                return ExitCode::SUCCESS;
            }
            let out = TableOutput::detect();
            let palette = terminal_palette(&out, json);
            print!(
                "{}",
                sessions_table(sessions, mode, all, tags, parent, &palette, &out)
            );
            ExitCode::SUCCESS
        }
        Err(e) => common::fail("ps", e),
    }
}

/// Watch mode: clear screen and reprint every 2 seconds.
/// Creates the HTTP client once, then polls in a loop. The palette and the
/// output environment are resolved once too — they do not change mid-watch.
async fn run_ps_watch(
    all: bool,
    tags: &[String],
    parent: Option<&str>,
    mode: TableMode,
    json: bool,
) -> ExitCode {
    let (host, port) = match common::ensure_gateway().await {
        Ok(hp) => hp,
        Err(e) => return common::fail("ps", e),
    };
    let http = match common::create_api_client(&host, port) {
        Ok(c) => c,
        Err(e) => return common::fail("ps", e),
    };
    let out = TableOutput::detect();
    let palette = terminal_palette(&out, json);

    loop {
        match http.list_sessions_with_parent(parent).await {
            Ok(resp) => {
                // Clear screen.
                print!("\x1b[2J\x1b[H");
                if json {
                    emit_json(resp.sessions, all, tags, parent);
                } else {
                    let table =
                        sessions_table(resp.sessions, mode, all, tags, parent, &palette, &out);
                    print!("{table}");
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
///
/// The human view appends a `usage:` block read from the audit flow (one
/// `/api/usage` request). It is **optional by design**: an old gateway, a failed
/// request or a session that predates the flow never break the main output —
/// the block is omitted (or degrades to the ledger line), never a row of zeros.
pub async fn run_info(session_id: &str, json: bool) -> ExitCode {
    let http = match common::connect().await {
        Ok(client) => client,
        Err(e) => return common::fail("info", e),
    };
    match fetch_session_info(&http, session_id).await {
        Ok(info) => {
            if json {
                common::print_json_compact(&info);
            } else {
                let usage = fetch_usage_block(&http, session_id).await;
                print_session_info(&info, usage.as_ref());
            }
            ExitCode::SUCCESS
        }
        Err(e) => common::fail("info", e),
    }
}

/// `wing info` 的用量块：自身那一行 + 子孙的合计（一次 `group_by=session&tree`）。
struct InfoUsage {
    own: UsageMeasures,
    /// 子孙（不含自身）：`(会话数, 合计)`；没有子孙时是 `None`。
    tree: Option<(i64, UsageMeasures)>,
}

impl InfoUsage {
    /// 子树汇总走 `totals`（**不被 `limit` 截断**的对账基准）减去自身行，
    /// 而不是把返回的行相加：子孙超过 `limit` 时行是被截断的，加出来的数会静默偏小。
    fn from_report(report: &UsageResponse, session_id: &str) -> InfoUsage {
        let own = report
            .row(session_id)
            .map(|row| row.measures.clone())
            .unwrap_or_default();
        let totals = &report.totals;
        // 子孙有流水才有 `tree:` 行：`totals - 自身行` 里 `sessions_created` 只会来自
        // 目录（回填的存量会话 / 新建的空会话），那不算读数。
        let rest = totals.sub(&own);
        let tree = rest.has_flow().then(|| {
            // `sessions` 是去重计数：作用域内除了自身，剩下的都是子孙。
            let sessions = totals.sessions - i64::from(own.has_flow());
            (sessions, rest)
        });
        InfoUsage { own, tree }
    }
}

/// 一次 `since=all` 的会话作用域查询；取不到就返回 `None`（整块省略）。
async fn fetch_usage_block(http: &GatewayApiClient, session_id: &str) -> Option<InfoUsage> {
    let query = UsageQuery {
        since: Some("all".to_string()),
        until: None,
        group_by: Some("session".to_string()),
        session_id: Some(session_id.to_string()),
        tree: true,
        limit: Some(usage::MAX_LIMIT),
    };
    let report = http.get_usage(&query).await.ok()?;
    Some(InfoUsage::from_report(&report, session_id))
}

/// The table's inks, `auto` preset included.
///
/// `auto` means "ask the terminal"; that question reads stdin and writes stdout,
/// so it is asked **only** when the answer can change what reaches the screen —
/// i.e. when the human table carries colour at all. `--json`, a pipe, `NO_COLOR`
/// or a `dumb` terminal print no ANSI inks, so `auto` resolves to `Wing` unprobed.
fn terminal_palette(out: &TableOutput, json: bool) -> ThemePalette {
    let colors = AppConfig::load().colors;
    let detected = if out.color && !json && colors.preset == ColorPreset::Auto {
        appearance::detect(appearance::DETECT_TIMEOUT)
    } else {
        None
    };
    ThemePalette::from_config_with_appearance(&colors, detected)
}

/// `--json` 的机器面：过滤 + 呈现序 + 紧凑输出。`--tree` / `--flat` 不影响它
/// （呈现方式是表格的事；血缘特性只在每行上新增一个可忽略的 `parent_session_id` 键）。
fn emit_json(sessions: Vec<SessionInfo>, all: bool, tags: &[String], parent: Option<&str>) {
    let mut filtered = filter_sessions(sessions, all, tags, parent);
    order_sessions(&mut filtered);
    common::print_json_compact(&filtered);
}

/// 一次人类输出的完整文本（表格 + 行尾汇总，或「没有会话」）——三种模式的唯一
/// 渲染入口，`-w/--watch` 走同一条路。
fn sessions_table(
    sessions: Vec<SessionInfo>,
    mode: TableMode,
    all: bool,
    tags: &[String],
    parent: Option<&str>,
    palette: &ThemePalette,
    out: &TableOutput,
) -> String {
    let now = Local::now().naive_local();
    match mode {
        // 层视图的森林建在**未按存活过滤**的行集上：`filter_sessions` 只决定
        // 「哪些行进视图」，crew 的三个计数与存活闭包要按全集算。
        TableMode::Level => {
            let view = level_rows(&sessions, all, tags, parent);
            let footer = view.whole_level.then(|| crew_summary(&view.rows)).flatten();
            render_view(&view.rows, palette, out, now, footer)
        }
        TableMode::Flat => {
            let mut filtered = filter_sessions(sessions, all, tags, parent);
            order_sessions(&mut filtered);
            render_view(&flat_rows(&filtered), palette, out, now, None)
        }
        TableMode::Tree => {
            let filtered = filter_sessions(sessions, all, tags, parent);
            render_view(&tree_rows(&filtered), palette, out, now, None)
        }
    }
}

/// 表格 + 行尾（`footer` 由 [`level_rows`] 的 `whole_level` 决定，别的模式没有）。
fn render_view(
    rows: &[SessionRow<'_>],
    palette: &ThemePalette,
    out: &TableOutput,
    now: NaiveDateTime,
    footer: Option<String>,
) -> String {
    if rows.is_empty() {
        return "No sessions found.\n".to_string();
    }
    let mut text = format_sessions_table(rows, palette, out, now);
    if let Some(footer) = footer {
        text.push_str(&footer);
        text.push('\n');
    }
    text
}

/// 人类输出的三种模式（互斥；`--json` 不走这里）。旗标 → 模式的转换只发生在
/// clap 边界（`cmd::mod` 的 dispatch），别处一律按模式分派。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableMode {
    /// 默认：一层一个任务 + `CREW` 列。`--tag` / `--parent` 把行集收窄成标签子集 /
    /// 子层（行尾汇总随之消失，见 `level_rows`）。
    Level,
    /// `--flat`：一行一个会话，没有 `CREW` 列（本命令改动前的样子）。
    Flat,
    /// `--tree`：整棵森林的缩进画法。
    Tree,
}

impl TableMode {
    /// 两个旗标 → 模式。`--flat` 与 `--tree` 由 clap 声明互斥；`--tree` 优先是这里
    /// 的兜底（保持函数全域，不产生「两个都给了会怎样」的未定义行为）。
    pub fn pick(tree: bool, flat: bool) -> Self {
        if tree {
            Self::Tree
        } else if flat {
            Self::Flat
        } else {
            Self::Level
        }
    }
}

/// 表格的一行素材：会话本体 + 树前缀（层视图与平铺模式恒为空串）。
struct SessionRow<'a> {
    session: &'a SessionInfo,
    prefix: String,
    /// 层视图的行：LAST 读子树活跃的**来源行**（叶子行的来源行就是它自己）。平铺与
    /// 树状模式没有这条投影，读会话自己的时间。
    level: bool,
    /// 折进 `CREW` 列的聚合：**只在整层视图**给值。收窄视图（`--tag` / `--parent`）
    /// 的行集是子集，逐行 rollup 会把不在屏上的后代一起数进来（同一批会话可能同屏
    /// 重复计数）——与行尾汇总同一条判据。平铺 / 树状模式没有这一列。
    crew: Option<Crew>,
    /// 层视图里 LAST 读**哪一行的原文**：内核给出的「子树活跃来自哪一行」（叶子 = 自己）。
    /// 平铺与树状模式不用它（读 `session` 自己的时间）。
    activity_source: Option<&'a str>,
}

/// 平铺呈现序（后端基准序 + pin 叠加，由 [`order_sessions`] 定）。
fn flat_rows(sessions: &[SessionInfo]) -> Vec<SessionRow<'_>> {
    sessions
        .iter()
        .map(|session| SessionRow {
            session,
            prefix: String::new(),
            level: false,
            crew: None,
            activity_source: None,
        })
        .collect()
}

/// 树序：交给 [`Forest::build`] 定序（输入序不参与），每个节点带回自己的
/// 渲染前缀。pin 分两层、孤儿升根、环与深链防御都在算法里。
fn tree_rows(sessions: &[SessionInfo]) -> Vec<SessionRow<'_>> {
    let forest = forest_of(sessions);
    forest
        .tree()
        .iter()
        .map(|entry| SessionRow {
            session: &sessions[entry.index],
            prefix: entry.prefix.clone(),
            level: false,
            crew: None,
            activity_source: None,
        })
        .collect()
}

/// 层视图的一次投影：行 + 这份行集是不是**完整的一层**。（同族的 `flat_rows` /
/// `tree_rows` 只给行集；这条通路还要那个判据来决定打不打行尾汇总与 `CREW` 列。）
struct LevelRows<'a> {
    rows: Vec<SessionRow<'a>>,
    /// 根层且没有任何筛（`--all` 不影响这一点）。行尾汇总与 `CREW` 列只描述这里：它是
    /// 「你看到的任务，它们的 crew 折起来了」这句入口提示。`--tag` 的行是标签子集（父与
    /// 子可能同时成行，逐行 rollup 把屏幕上已有的会话又数一遍）、`--parent` 的行是子层
    /// （数字本身没错，但那些行是 crew 不是任务，而且用户已经在往下钻）——两者都不打
    /// 汇总、也不给这一列。
    whole_level: bool,
}

/// 有没有把行集收窄成「子集 / 子层」（`--tag` / `--parent`）。两个消费点共用这一条
/// 判据：`filter_sessions` 决定走筛选分支，[`level_rows`] 取它的反面当 `whole_level`。
fn is_narrowed(parent: Option<&str>, tags: &[String]) -> bool {
    parent.is_some() || !tags.is_empty()
}

/// 层视图的行集：一行一个任务。
///
/// - 没有 `--tag` / `--parent`（`whole_level`）：`level(None, include_dormant = --all)`
///   —— 默认口径是「存活（`status != inactive`）或被 pin」**加上祖先闭包**（一个
///   dormant 的父任务不因为它自己被逐出就消失，否则它的活子会升成假根）。
/// - 有 `--tag` / `--parent`：标签与血缘是另一类筛（都隐含 `--all`），行集是同时满足
///   二者的行（AND），按**森林全序**取——定序契约因此对这两种筛同样成立。
///
/// 两种情形都把森林建在 `sessions`（网关返回的原始列表，**未按存活 / 标签过滤**）上：
/// crew 的后代计数要含 dormant 后代、存活闭包要看到被逐出的父，过滤不能发生在建森林
/// 之前（它只决定「哪些行进视图」）。
fn level_rows<'a>(
    sessions: &'a [SessionInfo],
    all: bool,
    tags: &[String],
    parent: Option<&str>,
) -> LevelRows<'a> {
    let forest = forest_of(sessions);
    let whole_level = !is_narrowed(parent, tags);
    let visible = if whole_level {
        forest.level(None, all).rows
    } else {
        in_total_order(&forest, |index| {
            passes_flag_filters(&sessions[index], parent, tags)
        })
    };
    let rows = visible
        .into_iter()
        .map(|index| SessionRow {
            session: &sessions[index],
            prefix: String::new(),
            level: true,
            // 收窄视图不给 rollup（行集是子集，逐行计数会重复/越界数）。
            crew: whole_level.then(|| forest.crew(index)),
            activity_source: forest
                .activity_of(index)
                .and_then(|source| sessions[source].last_interaction.as_deref()),
        })
        .collect();
    LevelRows { rows, whole_level }
}

/// 满足 `keep` 的行下标，顺序 = 森林全序（树序遍历）在该子集上的限制。
///
/// 只吃下标谓词：森林的下标空间是传给 [`Forest::build`] 的那个切片，把「另一份
/// 数组」和森林配在一起是内核最怕的误用（静默画错行），所以由调用方在自己的
/// 数组上捕获。
fn in_total_order(forest: &Forest<'_>, keep: impl Fn(usize) -> bool) -> Vec<usize> {
    forest
        .tree()
        .iter()
        .map(|entry| entry.index)
        .filter(|&index| keep(index))
        .collect()
}

/// 会话行 → 森林输入（内核要的四件事：拓扑 / pin / 最后交互 / 行状态）。
fn forest_of<'a>(sessions: &'a [SessionInfo]) -> Forest<'a> {
    let input: Vec<TreeRow<'_>> = sessions
        .iter()
        .map(|session| TreeRow {
            id: session.id.as_str(),
            parent: session.parent_session_id.as_deref(),
            pinned: is_pinned(&session.tags),
            pin_added_at: pin_added_at(&session.tag_meta),
            last_interaction: session.last_interaction.as_deref(),
            status: SessionStatus::parse(&session.status),
        })
        .collect();
    Forest::build(&input)
}

/// `--tag` / `--parent` 的**唯一**判定：`parent` 逐字相等（不给则不筛）∧ 所有
/// `tags` 命中（AND）。
///
/// 两个消费点（平铺 / 树状 / `--json` 的 [`filter_sessions`]，层视图的子集分支）
/// 必须都走这里：副本漂移是静默的——同一条命令的不同画法会对同一组旗标给出不同
/// 的行集，编译器和测试都不会说话。
fn passes_flag_filters(session: &SessionInfo, parent: Option<&str>, tags: &[String]) -> bool {
    parent.is_none_or(|parent| session.parent_session_id.as_deref() == Some(parent))
        && tags.iter().all(|tag| session.tags.iter().any(|t| t == tag))
}

/// Filter sessions by status, tags and lineage parent.
///
/// - With `tags` given: keep sessions carrying ALL tags (AND), **inactive
///   included** — tags describe long-term taxonomy (favorites / task crews
///   are often archived), so the inactive-drop does not apply.
/// - With `parent` given: keep the parent's **direct children**, inactive
///   included (same rationale as `tags` — lineage is a long-term fact). The
///   gateway already applied `?parent=`; the re-check covers a gateway that
///   predates the parameter and answered with the full list (a documented
///   superset), so `--parent` means the same thing everywhere.
/// - Without either: `all=true` keeps everything; otherwise drop `inactive`
///   — except **pinned** sessions, which the flat / tree / `--json` row sets
///   keep on purpose (pin means "keep this one in sight", and what you pin is
///   usually a session you stepped away from). The level view does not come
///   through here: its liveness rule is the kernel's closure.
fn filter_sessions(
    sessions: Vec<SessionInfo>,
    all: bool,
    tags: &[String],
    parent: Option<&str>,
) -> Vec<SessionInfo> {
    let mut sessions = sessions;
    if is_narrowed(parent, tags) {
        sessions.retain(|s| passes_flag_filters(s, parent, tags));
    } else if !all {
        sessions.retain(|s| s.status != "inactive" || is_pinned(&s.tags));
    }
    sessions
}

/// 呈现顺序（平铺模式）：后端基准序 + 前端 pin 叠加 —— 被 pin 的置顶，组内后
/// pin 的更靠前（pin 排序键见 [`crate::shared::pinning`]）；其余保持后端下发的
/// 顺序（活跃优先、组内按最后交互时间降序）。
///
/// 只作用于平铺与 `--json` 输出；层视图与 `--tree` 的定序归 [`Forest::build`]
/// （它不读输入顺序），这里的排序对它们没有意义。`/ss` 面板的两条行集同样不读输入序
/// ——层视图是森林全序在某一层的限制，搜索态是树序的子序列；pin 在那里的语义是
/// 「分两层」（见 session_tree）。
fn order_sessions(sessions: &mut [SessionInfo]) {
    sessions.sort_by_cached_key(|s| pin_key(is_pinned(&s.tags), pin_added_at(&s.tag_meta)));
}

async fn fetch_sessions(parent: Option<&str>) -> Result<Vec<SessionInfo>> {
    let http = common::connect().await?;
    let resp = http.list_sessions_with_parent(parent).await?;
    Ok(resp.sessions)
}

async fn fetch_session_info(
    http: &GatewayApiClient,
    session_id: &str,
) -> Result<SessionInfoResponse> {
    // 被逐出（不在内存）的会话 404——先 resume 水合再取（逐出 ≠ 不存在）。
    common::hydrate_on_404(
        http,
        session_id,
        || http.get_session_info(session_id),
        |e| e.into(),
    )
    .await
}

/// Render the sessions table (human surface; `now` is injected so the age
/// column is deterministic in tests).
///
/// Inks: NAME carries the primary text colour (it is what the eye scans for),
/// SESSION ID the secondary one (copied on demand), LAST stays the faintest —
/// and STATUS carries the one semantic colour per state.
///
/// The tree prefix (empty in every mode but `--tree`) is part of the SESSION ID
/// cell: the column is `keep_natural`, so the widest `prefix + id` sets the
/// column and the engine keeps every row at the same display width.
///
/// MODEL is the model's display name (gateway config) with the call name / id
/// as fallback; it is a short label rather than prose, so it classifies as
/// `Compact` (shrinks after NAME/TAGS) and is capped like the other columns.
///
/// CREW (whole-level view only) appears last and only when some row actually
/// has a rollup — a list without a crew is not made wider for a column of
/// zeros. The narrowed views (`--tag` / `--parent`) carry no rollup at all
/// (their row set is a subset; see [`level_rows`]), so the column never shows
/// there. It is `Compact` (a count is one short token) and its width is the
/// engine's business like every other column.
fn format_sessions_table(
    rows: &[SessionRow<'_>],
    palette: &ThemePalette,
    out: &TableOutput,
    now: NaiveDateTime,
) -> String {
    let show_crew = folded_crew(rows) > 0;
    let mut columns = vec![
        // The id is the copyable handle and only matches exactly — it must
        // never be truncated, even on a narrow terminal (the table overflows
        // instead; NAME/TAGS give up their width first).
        PlainColumn::keep_natural("SESSION ID", ColumnKind::Compact),
        PlainColumn::new("STATUS", ColumnKind::Compact),
        PlainColumn::new("LAST", ColumnKind::Compact),
        PlainColumn::capped("NAME", ColumnKind::Narrative, 80),
        PlainColumn::capped("MODEL", ColumnKind::Compact, 32),
        PlainColumn::capped("TAGS", ColumnKind::Narrative, 40),
    ];
    if show_crew {
        columns.push(PlainColumn::new("CREW", ColumnKind::Compact));
    }
    let table_rows = rows
        .iter()
        .map(|row| {
            let session = row.session;
            let mut cells = vec![
                PlainCell::styled(
                    format!("{}{}", row.prefix, session.id),
                    Style::new().fg(palette.tool_result),
                ),
                PlainCell::styled(
                    session.status.clone(),
                    status_style(&session.status, palette),
                ),
                PlainCell::styled(last_text(row, now), Style::new().fg(palette.dim)),
                PlainCell::styled(
                    common::single_line(session.name.as_deref().unwrap_or("-")),
                    Style::new().fg(palette.text),
                ),
                PlainCell::styled(model_text(session), Style::new().fg(palette.tool_result)),
                PlainCell::styled(
                    tags_text(&session.tags),
                    Style::new().fg(palette.tool_result),
                ),
            ];
            if show_crew {
                cells.push(PlainCell::styled(
                    // `None`（这一行没有聚合）留空，`0` 才是「这个任务没有后代」。
                    row.crew.map(crew_text).unwrap_or_default(),
                    Style::new().fg(palette.tool_result),
                ));
            }
            cells
        })
        .collect();

    let table = PlainTable {
        columns,
        rows: table_rows,
    };
    common::render_table(&table, palette, out)
}

/// 折进 `CREW` 列的后代数（= 该列之和）：列出现与否读这一个数。收窄视图（`--tag` /
/// `--parent`）的行没有聚合，所以这个数在那里恒为 0 —— 列与行尾汇总同一条判据
/// （见 [`level_rows`]）。
fn folded_crew(rows: &[SessionRow<'_>]) -> usize {
    rows.iter()
        .filter_map(|row| row.crew)
        .map(|crew| crew.descendants)
        .sum()
}

/// 行尾汇总：说明折了多少 crew、以及两条展开出口。没有 crew 时返回 `None`（不打空话）。
fn crew_summary(rows: &[SessionRow<'_>]) -> Option<String> {
    let folded = folded_crew(rows);
    (folded > 0).then(|| {
        let tasks = if rows.len() == 1 { "task" } else { "tasks" };
        format!(
            "{} {tasks} · {} crew folded — `--parent <sid>` drills in, `--tree` shows all",
            rows.len(),
            folded
        )
    })
}

/// `CREW` 列的读法：后代总数，再按非零追加 `?k`（后代在等你）与 `●k`（后代在干活）。
fn crew_text(crew: Crew) -> String {
    let mut text = crew.descendants.to_string();
    if crew.waiting > 0 {
        text.push_str(&format!(" ?{}", crew.waiting));
    }
    if crew.working > 0 {
        text.push_str(&format!(" ●{}", crew.working));
    }
    text
}

/// LAST 列：层视图的行读内核指出的**来源行**原文（叶子行的来源行就是它自己），其余模式
/// 读会话自己的时间。两侧都走 [`format_last_interaction`]，所以「原样展示 / naive 当本地
/// 墙钟 / 缺失给 `-`」这三条降级对父行与叶子行是同一条。
fn last_text(row: &SessionRow<'_>, now: NaiveDateTime) -> String {
    let stamp = if row.level {
        row.activity_source
    } else {
        row.session.last_interaction.as_deref()
    };
    format_last_interaction(stamp, now)
}

/// The model cell: display name first, then the call name, then the id
/// (`-` when the gateway sent none of them — an old gateway omits all four).
///
/// Blank values are treated as absent (a gateway that sends `""` does not
/// mean "the model is called nothing"), and the winner is flattened to one
/// line: the value ends up in a fixed-width cell where a stray control
/// character would tear the grid apart (same guard as NAME).
fn model_text(session: &SessionInfo) -> String {
    match [
        session.model_display_name.as_deref(),
        session.model_name.as_deref(),
        session.model_id.as_deref(),
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
/// `3h` / `2d`. Unparseable values pass through as written (flattened to one
/// line first: the table still shows *something* meaningful, and a control
/// character cannot tear the row apart).
fn format_last_interaction(ts: Option<&str>, now: NaiveDateTime) -> String {
    let Some(ts) = ts else {
        return "-".to_string();
    };
    match NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f") {
        Ok(dt) => format_age(dt, now),
        Err(_) => common::single_line(ts),
    }
}

/// Compact age of `at` seen from `now`: `now` / `42s` / `12m` / `3h` / `2d`;
/// once past a week the event's own date reads better than a growing day
/// count (with the year when it is not the current one — a session list
/// outlives new year's eve).
fn format_age(at: NaiveDateTime, now: NaiveDateTime) -> String {
    let secs = now.signed_duration_since(at).num_seconds().max(0);
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
    } else if at.year() == now.year() {
        at.format("%m-%d").to_string()
    } else {
        at.format("%Y-%m-%d").to_string()
    }
}

fn print_session_info(info: &SessionInfoResponse, usage: Option<&InfoUsage>) {
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
    if let Some(ref parent) = info.parent_session_id {
        println!("parent:              {parent}");
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
    if let Some(block) = usage {
        print!("{}", format_usage_block(block, info.usage.as_ref()));
    }
}

/// `usage:` 块（与 `wing usage --session <sid>` 同源：都是同一个端点的同一份度量）。
///
/// 流水全 0 的会话（老会话 / 审计上线前）**不写一排 0**：改写一行 `—` 并回落到会话
/// 账本（`SessionInfoResponse.usage`），否则「没有读数」会被读成「一次都没用过」。
fn format_usage_block(block: &InfoUsage, ledger: Option<&SessionUsage>) -> String {
    let own = &block.own;
    if !own.has_flow() {
        // 自身没有流水（老会话 / 审计上线前建的）：写一排 0 会被读成「一次都没用过」。
        let note = ledger
            .filter(|ledger| ledger.turns > 0 || ledger.llm_calls > 0 || ledger.tool_calls > 0)
            .map(|ledger| {
                format!(
                    " (no audit flow; ledger: {} turns · ↑{} ↓{})",
                    ledger.turns,
                    format::tokens(ledger.prompt_tokens),
                    format::tokens(ledger.completion_tokens),
                )
            })
            .unwrap_or_else(|| " (no audit flow)".to_string());
        let mut text = format!("usage:  {NONE_MARK}{note}\n");
        if let Some((sessions, tree)) = &block.tree {
            text.push_str(&tree_line(*sessions, tree));
        }
        return text;
    }
    let mut text = String::from("usage:\n");
    text.push_str(&format!(
        "  {:<20}{}\n",
        "messages:",
        messages_text(own, ledger)
    ));
    text.push_str(&format!(
        "  {:<20}{} llm · {} tools\n",
        "calls:",
        count(own.llm_calls),
        count(own.tool_calls)
    ));
    text.push_str(&format!(
        "  {:<20}↑ {} in{} · ↓ {} out\n",
        "tokens:",
        format::tokens(own.prompt_tokens),
        match format::hit_rate(own.cached_tokens, own.prompt_tokens) {
            Some(rate) => format!(" ({rate:.0}% cached)"),
            None => String::new(),
        },
        format::tokens(own.completion_tokens)
    ));
    text.push_str(&format!(
        "  {:<20}llm {} · tools {} · work {}\n",
        "time:",
        work_time(own.llm_time_ms),
        work_time(own.tool_time_ms),
        work_time(own.work_time_ms())
    ));
    if let Some((sessions, tree)) = &block.tree {
        text.push_str(&tree_line(*sessions, tree));
    }
    text
}

/// `messages:` 行：**会话账本优先**，流水回退，两者都没有写 `—`。
///
/// 审计的 `audit_user_message` 是后加的表：存量会话的流水是 0，而账本
/// （`metadata.usage.user_messages`）从第一天就在记。只读流水会让老会话显示成
/// 「一条消息都没说过」。反过来账本也可能缺（memory 后端 / 建账前的历史），
/// 所以两者都没有时才写 `—`（不写 0）。
fn messages_text(own: &UsageMeasures, ledger: Option<&SessionUsage>) -> String {
    let from_ledger = ledger.map(|ledger| ledger.user_messages).unwrap_or(0);
    if from_ledger > 0 {
        return count(from_ledger);
    }
    if own.user_messages > 0 {
        return count(own.user_messages);
    }
    NONE_MARK.to_string()
}

/// `tree:` 行：子树（自身之外的会话）的会话数与 token 和。
fn tree_line(sessions: i64, tree: &UsageMeasures) -> String {
    format!(
        "  {:<20}{} sessions · ↑ {} · ↓ {}\n",
        "tree:",
        count(sessions),
        format::tokens(tree.prompt_tokens),
        format::tokens(tree.completion_tokens)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::UsageRow;

    fn session(id: &str, status: &str, tags: &[&str]) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: Some(id.into()),
            created_at: None,
            template_name: None,
            workspace: None,
            parent_session_id: None,
            last_interaction: None,
            status: status.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            tag_meta: Default::default(),
            model_id: None,
            model_name: None,
            provider_name: None,
            model_display_name: None,
        }
    }

    /// 带模型四件套的会话（列表条目的模型列素材）；`provider` 显式给出，
    /// 与 `model_name` 独立（网关的降级形态里两者可以只剩其一）。
    fn with_model(
        mut info: SessionInfo,
        model_id: Option<&str>,
        model_name: Option<&str>,
        provider: Option<&str>,
        display_name: Option<&str>,
    ) -> SessionInfo {
        info.model_id = model_id.map(str::to_string);
        info.model_name = model_name.map(str::to_string);
        info.provider_name = provider.map(str::to_string);
        info.model_display_name = display_name.map(str::to_string);
        info
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

    /// 有父会话的行（血缘边）。
    fn child_of(mut info: SessionInfo, parent: &str) -> SessionInfo {
        info.parent_session_id = Some(parent.into());
        info
    }

    /// 平铺表格（多数渲染用例的形态）。
    fn flat_table(
        sessions: &[SessionInfo],
        palette: &ThemePalette,
        out: &TableOutput,
        now: NaiveDateTime,
    ) -> String {
        format_sessions_table(&flat_rows(sessions), palette, out, now)
    }

    /// 树表格（`--tree` 的渲染面）。
    fn tree_table(
        sessions: &[SessionInfo],
        palette: &ThemePalette,
        out: &TableOutput,
        now: NaiveDateTime,
    ) -> String {
        format_sessions_table(&tree_rows(sessions), palette, out, now)
    }

    // ── 表格渲染（共享引擎 + 显示宽度记账）────────────────────────

    fn naive(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").expect("fixture timestamp")
    }

    #[test]
    fn format_age_boundaries() {
        let at = naive("2026-10-09T21:00:00");
        let after = |secs: i64| at + chrono::Duration::seconds(secs);
        assert_eq!(format_age(at, after(0)), "now");
        assert_eq!(format_age(at, after(4)), "now");
        assert_eq!(format_age(at, after(5)), "5s");
        assert_eq!(format_age(at, after(59)), "59s");
        assert_eq!(format_age(at, after(60)), "1m");
        assert_eq!(format_age(at, after(3599)), "59m");
        assert_eq!(format_age(at, after(3600)), "1h");
        assert_eq!(format_age(at, after(86_399)), "23h");
        assert_eq!(format_age(at, after(86_400)), "1d");
        assert_eq!(format_age(at, after(6 * 86_400)), "6d");
        // 一周以上：显示事件当天的日期（同年只给 MM-DD）。
        assert_eq!(format_age(at, after(7 * 86_400)), "10-09");
        // 跨年保留年份——会话列表会跨年，`12-28` 的年份不能靠猜。
        assert_eq!(
            format_age(naive("2025-12-28T10:00:00"), naive("2026-01-05T10:00:00")),
            "2025-12-28"
        );
        // 时钟偏移导致的"未来"时间戳钳到 now，不出负数。
        assert_eq!(format_age(at, after(-5)), "now");
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
        // 解析不了就原样展示（不装作认识）——但先压平控制字符，别撕开数据行。
        assert_eq!(format_last_interaction(Some("garbage"), now), "garbage");
        assert_eq!(
            format_last_interaction(Some("bad\nts"), now),
            "bad ts",
            "回退路径也必须单行化"
        );
    }

    #[test]
    fn status_colors_are_semantic() {
        let p = ThemePalette::default();
        assert_eq!(status_style("working", &p).fg, Some(p.accent));
        assert_eq!(status_style("waiting", &p).fg, Some(p.warning));
        assert_eq!(status_style("idle", &p).fg, Some(p.tool_result));
        assert_eq!(status_style("inactive", &p).fg, Some(p.dim));
    }

    /// 模型列的回落链：展示名 → 调用名 → 引用词 → `-`。
    ///
    /// 展示名是展示素材（未声明时网关发 null），调用名是"实际跑的是什么"，
    /// 引用词是身份——三者都没有（旧网关）才回落到 `-`。
    #[test]
    fn model_text_falls_back_display_name_then_name_then_id() {
        let full = with_model(
            session("a", "idle", &[]),
            Some("ds-flash"),
            Some("deepseek-flash-2026"),
            Some("probe"),
            Some("DeepSeek Flash"),
        );
        assert_eq!(model_text(&full), "DeepSeek Flash");

        // 未声明展示名 → 调用名。
        let no_display = with_model(
            full.clone(),
            Some("ds-flash"),
            Some("deepseek-flash-2026"),
            Some("probe"),
            None,
        );
        assert_eq!(model_text(&no_display), "deepseek-flash-2026");

        // 只有引用词（降级路径）→ id。
        let id_only = with_model(full.clone(), Some("ds-flash"), None, None, None);
        assert_eq!(model_text(&id_only), "ds-flash");

        // 旧网关 / 全缺 → `-`。
        assert_eq!(model_text(&session("b", "inactive", &[])), "-");

        // 空串 / 纯空白不算"有值"（网关发的空展示名不能吃掉调用名）。
        let blank = with_model(
            full.clone(),
            Some("ds-flash"),
            Some("deepseek-flash-2026"),
            Some("probe"),
            Some("   "),
        );
        assert_eq!(model_text(&blank), "deepseek-flash-2026");
        let all_blank = with_model(full, Some("ds-flash"), Some(""), Some("probe"), Some(" "));
        assert_eq!(model_text(&all_blank), "ds-flash");
    }

    /// 模型值里的控制字符不能撕开数据行（与 NAME 同一条纪律）。
    #[test]
    fn model_text_flattens_control_characters() {
        let messy = with_model(
            session("a", "idle", &[]),
            Some("ds-flash"),
            None,
            Some("probe"),
            Some("Deep\nSeek\tFlash"),
        );
        assert_eq!(model_text(&messy), "Deep Seek Flash");
    }

    /// 模型列出现在表格里（表头 + 值），且不破坏等宽契约。
    #[test]
    fn sessions_table_carries_a_model_column() {
        let sessions = vec![
            with_model(
                session("20261009-210702-217d85f2", "working", &["task=a"]),
                Some("ds-flash"),
                Some("deepseek-flash-2026"),
                Some("probe"),
                Some("DeepSeek Flash"),
            ),
            with_model(
                session("20261008-123133-3f465dd1", "inactive", &[]),
                Some("probe/model"),
                Some("probe/model"),
                Some("probe"),
                None,
            ),
        ];
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 100,
            color: false,
        };
        let table = flat_table(&sessions, &palette, &out, naive("2026-10-09T21:11:00"));

        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[1].contains("MODEL"), "{table}");
        // 展示名优先；无展示名的行回落到调用名。
        assert!(lines[3].contains("DeepSeek Flash"), "{table}");
        assert!(lines[5].contains("probe/model"), "{table}");

        // 等宽的格子（引擎的契约），且不超终端宽。
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| unicode_width::UnicodeWidthStr::width(*l))
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert!(widths[0] <= 100, "{widths:?}");
    }

    /// 没有模型素材的会话（旧网关 / 未解析）在模型列显示 `-`，而不是空单元格。
    #[test]
    fn sessions_table_shows_a_dash_without_model_material() {
        let sessions = vec![session("20261009-210702-217d85f2", "idle", &[])];
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 100,
            color: false,
        };
        let table = flat_table(&sessions, &palette, &out, naive("2026-10-09T21:11:00"));
        let row: Vec<&str> = table.lines().filter(|l| l.contains("idle")).collect();
        assert_eq!(row.len(), 1, "{table}");
        // 单元格按体行分隔符切：SESSION ID / STATUS / LAST / NAME / MODEL / TAGS。
        // （前提：本用例的值里不含框线字符——`single_line` 只压平控制字符。）
        let cells: Vec<&str> = row[0].split('│').collect();
        assert_eq!(cells.len(), 6, "{table}");
        assert_eq!(cells[2].trim(), "-", "LAST：{table}");
        assert_eq!(cells[4].trim(), "-", "MODEL：{table}");
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
        let table = flat_table(&sessions, &palette, &out, naive("2026-10-09T21:11:00"));

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

    /// 会话 id 是**精确匹配**的可复制句柄（`wing tail/info/wait` 都要完整 id），
    /// 窄终端下必须在场：收缩全部发生在 NAME/TAGS/STATUS 上，80 列（最常见
    /// 的默认宽度）尤其要保住。
    #[test]
    fn session_id_survives_a_narrow_terminal() {
        let mut long_name = session(
            "20261009-210702-217d85f2",
            "working",
            &["executor", "task=a"],
        );
        long_name.name = Some("我们前端的表格渲染虽然不错吧 但是其实我更喜欢包裹起来的感觉".into());
        long_name.last_interaction = Some("2026-10-09T21:10:21.047560".into());
        let sessions = vec![long_name];

        let palette = ThemePalette::default();
        let now = naive("2026-10-09T21:11:00");
        for width in [80usize, 100, 120] {
            let out = TableOutput {
                width,
                color: false,
            };
            let table = flat_table(&sessions, &palette, &out, now);
            assert!(
                table.contains("20261009-210702-217d85f2"),
                "width={width}: id 不能被截断\n{table}"
            );
        }
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
        let plain = flat_table(&sessions, &palette, &plain_out, now);
        assert!(!plain.contains('\u{1b}'), "no ANSI when colour is off");

        let colored_out = TableOutput {
            width: 100,
            color: true,
        };
        let colored = flat_table(&sessions, &palette, &colored_out, now);
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
        let table = flat_table(&[s], &palette, &out, naive("2026-10-09T21:11:00"));
        assert!(!table.contains("第一行\n第二行"), "{table}");
        assert!(table.contains("第一行 第二行"), "{table}");
    }

    #[test]
    fn default_filter_drops_inactive_but_all_keeps() {
        let sessions = vec![session("a", "working", &[]), session("b", "inactive", &[])];
        assert_eq!(
            ids(&filter_sessions(sessions.clone(), false, &[], None)),
            ["a"]
        );
        assert_eq!(ids(&filter_sessions(sessions, true, &[], None)), ["a", "b"]);
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
            ids(&filter_sessions(sessions.clone(), false, &[], None)),
            ["a", "c"]
        );
        assert_eq!(
            ids(&filter_sessions(sessions, true, &[], None)),
            ["a", "b", "c"]
        );
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
        let out = filter_sessions(sessions.clone(), false, &["executor".into()], None);
        assert_eq!(ids(&out), ["s1", "s2", "s3"]);
        // 多标签：AND；不因 inactive 被默认丢弃。
        let out = filter_sessions(sessions, false, &["executor".into(), "task=a".into()], None);
        assert_eq!(ids(&out), ["s1", "s2"]);
    }

    #[test]
    fn tag_filter_no_match_returns_empty() {
        let sessions = vec![session("a", "working", &["x"])];
        assert!(filter_sessions(sessions, true, &["nope".into()], None).is_empty());
    }

    // ── 血缘：--parent 过滤与 --tree 渲染 ─────────────────────────

    #[test]
    fn parent_filter_keeps_direct_children_including_inactive() {
        let sessions = vec![
            session("parent", "working", &[]),
            session("child-live", "idle", &[]),
            session("child-idle", "inactive", &[]),
            session("grandchild", "inactive", &[]),
            session("stranger", "inactive", &[]),
        ];
        let sessions = sessions
            .into_iter()
            .map(|s| match s.id.as_str() {
                "child-live" | "child-idle" => child_of(s, "parent"),
                "grandchild" => child_of(s, "child-live"),
                _ => s,
            })
            .collect();
        // `--parent` 隐含 `--all`：inactive 的直接子照样列出；孙不在（只列直接子）；
        // 父自己不在。
        let out = filter_sessions(sessions, false, &[], Some("parent"));
        assert_eq!(ids(&out), ["child-live", "child-idle"]);
    }

    #[test]
    fn parent_filter_ands_with_tags() {
        let sessions = vec![
            child_of(session("s1", "inactive", &["task=a"]), "P"),
            child_of(session("s2", "inactive", &["task=b"]), "P"),
            child_of(session("s3", "inactive", &["task=a"]), "Q"),
        ];
        let out = filter_sessions(sessions, false, &["task=a".into()], Some("P"));
        assert_eq!(ids(&out), ["s1"]);
    }

    /// 孤儿：父不在展示集合（被过滤 / 幽灵引用）→ 升为根（无前缀）。
    #[test]
    fn orphan_child_renders_as_a_root() {
        let sessions = vec![
            child_of(session("lost", "idle", &[]), "gone"),
            session("root", "idle", &[]),
        ];
        let rows = tree_rows(&sessions);
        assert_eq!(
            rows.iter()
                .map(|r| (r.session.id.as_str(), r.prefix.as_str()))
                .collect::<Vec<_>>(),
            [("lost", ""), ("root", "")]
        );
    }

    /// 子会话挂在父之后，带连接前缀；兄弟组内 `├─` / `└─` 收尾正确。
    #[test]
    fn tree_rows_nest_children_with_drawing_prefixes() {
        let sessions = vec![
            session("A", "working", &[]),
            child_of(session("c1", "idle", &[]), "A"),
            child_of(session("c2", "idle", &[]), "A"),
            session("B", "idle", &[]),
        ];
        let rows = tree_rows(&sessions);
        assert_eq!(
            rows.iter()
                .map(|r| (r.session.id.as_str(), r.prefix.as_str()))
                .collect::<Vec<_>>(),
            [
                ("A", ""),
                ("c1", "├─ "),
                ("c2", "└─ "),
                // 树与树之间不连线（每棵树独立成画）。
                ("B", ""),
            ]
        );
    }

    /// 孙辈要延续祖先层的 `│`（非末位祖先的竖线不能断）。
    #[test]
    fn tree_rows_keep_the_trunk_line_for_grandchildren() {
        let sessions = vec![
            session("A", "idle", &[]),
            child_of(session("B", "idle", &[]), "A"),
            child_of(session("C", "idle", &[]), "A"),
            child_of(session("G", "idle", &[]), "B"),
        ];
        let rows = tree_rows(&sessions);
        assert_eq!(
            rows.iter()
                .map(|r| (r.session.id.as_str(), r.prefix.as_str()))
                .collect::<Vec<_>>(),
            [("A", ""), ("B", "├─ "), ("G", "│  └─ "), ("C", "└─ "),]
        );
    }

    /// pin 分层（文档 §4）：pin 的**根**整棵上浮；pin 的**子**只在兄弟组里
    /// 排前，不提升父节点。
    #[test]
    fn tree_rows_float_a_pinned_root_with_its_subtree() {
        let mut sessions = vec![
            session("r1", "idle", &[]),
            pinned("r2", "idle", Some("2026-10-01T09:00:00")),
            child_of(session("r2child", "idle", &[]), "r2"),
        ];
        // 输入顺序不参与定序：倒着给也一样。
        sessions.reverse();
        let rows = tree_rows(&sessions);
        assert_eq!(
            rows.iter()
                .map(|r| (r.session.id.as_str(), r.prefix.as_str()))
                .collect::<Vec<_>>(),
            [("r2", ""), ("r2child", "└─ "), ("r1", "")]
        );
    }

    #[test]
    fn tree_rows_order_a_pinned_child_first_under_its_parent() {
        let sessions = vec![
            child_of(session("P", "idle", &[]), "root"),
            session("root", "idle", &[]),
            child_of(pinned("pinned-child", "idle", None), "P"),
            child_of(session("plain-child", "idle", &[]), "P"),
        ];
        let rows = tree_rows(&sessions);
        assert_eq!(
            rows.iter()
                .map(|r| (r.session.id.as_str(), r.prefix.as_str()))
                .collect::<Vec<_>>(),
            [
                ("root", ""),
                ("P", "└─ "),
                ("pinned-child", "   ├─ "),
                ("plain-child", "   └─ "),
            ]
        );
    }

    /// 子树活跃度：子树里有新动静的根上浮（排序语义来自 `shared::session_tree`，
    /// 这里锚定 CLI 侧确实消费了它）。
    #[test]
    fn tree_rows_rank_roots_by_subtree_activity() {
        let mut quiet = session("quiet", "idle", &[]);
        quiet.last_interaction = Some("2026-10-01T00:00:00".into());
        let mut busy = session("busy", "idle", &[]);
        busy.last_interaction = Some("2026-10-02T00:00:00".into());
        let mut busy_child = child_of(session("busy-child", "idle", &[]), "busy");
        busy_child.last_interaction = Some("2026-10-03T00:00:00".into());

        let input = [quiet, busy, busy_child];
        let rows = tree_rows(&input);
        assert_eq!(
            rows.iter()
                .map(|r| r.session.id.as_str())
                .collect::<Vec<_>>(),
            ["busy", "busy-child", "quiet"]
        );
    }

    /// 注意力：子树里有人等你 → 更旧的根上浮。锚定 CLI 侧确实把行状态喂进了
    /// `TreeRow`（`working` 不享有这个待遇）。
    #[test]
    fn tree_rows_rank_roots_by_subtree_attention() {
        let mut fresh = session("fresh", "idle", &[]);
        fresh.last_interaction = Some("2026-10-05T00:00:00".into());
        let mut quiet = session("quiet", "idle", &[]);
        quiet.last_interaction = Some("2026-10-01T00:00:00".into());

        let asker = child_of(session("asker", "waiting", &[]), "quiet");
        let input = [fresh.clone(), quiet.clone(), asker];
        let rows = tree_rows(&input);
        assert_eq!(
            rows.iter()
                .map(|r| r.session.id.as_str())
                .collect::<Vec<_>>(),
            ["quiet", "asker", "fresh"]
        );

        let busy = child_of(session("busy", "working", &[]), "quiet");
        let input = [fresh, quiet, busy];
        let rows = tree_rows(&input);
        assert_eq!(
            rows.iter()
                .map(|r| r.session.id.as_str())
                .collect::<Vec<_>>(),
            ["fresh", "quiet", "busy"],
            "working 不是注意力：顺序回到活跃度降序"
        );
    }

    /// 树表格：前缀进 SESSION ID 列，行等宽（显示列记账）、不超终端宽；
    /// 无父子关系的会话在树模式下与平铺逐行一致（退化面）。
    #[test]
    fn tree_table_indents_the_session_id_column_and_stays_aligned() {
        let mut parent = session("20261015-211837-87e3fac2", "working", &[]);
        parent.name = Some("调度者".into());
        let mut child = child_of(
            session("20261015-215945-bf5b1d00", "idle", &[]),
            parent.id.as_str(),
        );
        child.name = Some("执行者一号".into());
        let sessions = vec![parent, child];

        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 100,
            color: false,
        };
        let table = tree_table(&sessions, &palette, &out, naive("2026-10-15T22:00:00"));

        // 子行：前缀进第一列（`┃ ` 是外框，后跟前缀 + id）。
        assert!(table.contains("┃ └─ 20261015-215945-bf5b1d00"), "{table}");
        // 根行没有前缀（第一列紧接着就是 id）。
        assert!(table.contains("┃ 20261015-211837-87e3fac2"), "{table}");
        let widths: Vec<usize> = table
            .lines()
            .map(unicode_width::UnicodeWidthStr::width)
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert!(widths[0] <= 100, "{widths:?}");
        // CJK 名字不把行推歪（引擎按显示宽度截断/补齐）。
        assert!(table.contains("调度者"), "{table}");
        assert!(table.contains("执行者一号"), "{table}");
    }

    /// 空前缀 = 平滑退化：没有任何血缘时，树模式的表格与平铺一模一样。
    #[test]
    fn tree_table_without_lineage_matches_the_flat_table() {
        let sessions = vec![
            session("a", "working", &[]),
            session("b", "idle", &["task=a"]),
        ];
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 100,
            color: false,
        };
        let now = naive("2026-10-09T21:11:00");
        assert_eq!(
            tree_table(&sessions, &palette, &out, now),
            flat_table(&sessions, &palette, &out, now)
        );
    }

    /// 深链防御：前缀宽度封顶（`shared::session_tree::MAX_INDENT_DEPTH`），
    /// 渲染不会把行推到爆。
    #[test]
    fn tree_table_caps_the_indent_of_a_deep_chain() {
        let n = 40;
        let mut sessions: Vec<SessionInfo> = Vec::new();
        for i in 0..n {
            let mut s = session(&format!("n{i}"), "idle", &[]);
            if i > 0 {
                s.parent_session_id = Some(format!("n{}", i - 1));
            }
            sessions.push(s);
        }
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 200,
            color: false,
        };
        let table = tree_table(&sessions, &palette, &out, naive("2026-10-15T22:00:00"));
        // 最深一行仍存在（被放置），但前缀长度封顶（不随链深无限加宽）。
        let deepest = table
            .lines()
            .find(|l| l.contains("n39"))
            .expect("deepest row rendered");
        assert!(deepest.contains("└─ n39"), "{deepest}");
        // 去掉外框（`┃ `）后的前缀宽度必须封顶。
        let cell = deepest.strip_prefix("┃ ").expect("data row border");
        let prefix_width = unicode_width::UnicodeWidthStr::width(cell.split("n39").next().unwrap());
        assert!(
            prefix_width <= crate::shared::session_tree::MAX_INDENT_DEPTH * 3 + 3,
            "prefix not capped: {prefix_width}\n{table}"
        );
    }

    // ---- 层视图（默认渲染）：一行一个任务 + CREW ----

    /// 层视图的行（这些用例只看行与 crew；`whole_level` 有自己的用例）。
    fn level<'a>(
        sessions: &'a [SessionInfo],
        all: bool,
        tags: &[String],
        parent: Option<&str>,
    ) -> Vec<SessionRow<'a>> {
        level_rows(sessions, all, tags, parent).rows
    }

    /// 一条数据行的各单元格文本（去外框与内边距；表头带的 `┃` 与数据行的 `│`
    /// 都算分隔符）。
    fn row_cells(line: &str) -> Vec<String> {
        line.trim()
            .trim_start_matches('┃')
            .trim_end_matches('┃')
            .split(['│', '┃'])
            .map(|cell| cell.trim().to_string())
            .collect()
    }

    fn row_ids<'a>(rows: &'a [SessionRow<'a>]) -> Vec<&'a str> {
        rows.iter().map(|row| row.session.id.as_str()).collect()
    }

    /// 带最后交互时间的会话（层的定序与 LAST 列都要它）。
    fn last_at(mut info: SessionInfo, ts: &str) -> SessionInfo {
        info.last_interaction = Some(ts.into());
        info
    }

    /// 层视图的完整输出（默认口径：`--all` / `--tag` / `--parent` 都不给 —— 整层，
    /// 所以带行尾汇总）。
    fn level_table(sessions: &[SessionInfo], width: usize) -> String {
        let palette = ThemePalette::default();
        let out = TableOutput {
            width,
            color: false,
        };
        sessions_table(
            sessions.to_vec(),
            TableMode::Level,
            false,
            &[],
            None,
            &palette,
            &out,
        )
    }

    /// 一个 pin 住的调度者 + `kids` 个执行者（状态按 4 取模轮转）+ 一个陌生人。
    fn crew_fixture(kids: usize) -> Vec<SessionInfo> {
        let mut sessions = vec![pinned("sched", "working", Some("2026-10-15T20:00:00"))];
        for i in 0..kids {
            let status = match i % 4 {
                0 => "waiting",
                1 => "working",
                _ => "idle",
            };
            sessions.push(child_of(
                session(&format!("kid{i:02}"), status, &["executor"]),
                "sched",
            ));
        }
        sessions.push(session("stranger", "idle", &[]));
        sessions
    }

    #[test]
    fn table_mode_prefers_tree_then_flat() {
        assert_eq!(TableMode::pick(false, false), TableMode::Level);
        assert_eq!(TableMode::pick(false, true), TableMode::Flat);
        assert_eq!(TableMode::pick(true, false), TableMode::Tree);
        // clap 声明二者互斥；全序兜底让 tree 赢（不产生未定义行为）。
        assert_eq!(TableMode::pick(true, true), TableMode::Tree);
    }

    #[test]
    fn crew_text_counts_then_flags_only_the_nonzero_parts() {
        let crew = |descendants, waiting, working| Crew {
            descendants,
            waiting,
            working,
            activity: None,
        };
        assert_eq!(crew_text(crew(40, 0, 0)), "40");
        assert_eq!(crew_text(crew(40, 1, 3)), "40 ?1 ●3");
        assert_eq!(crew_text(crew(1, 1, 0)), "1 ?1");
        assert_eq!(crew_text(crew(2, 0, 3)), "2 ●3");
        assert_eq!(crew_text(Crew::default()), "0");
    }

    /// 默认视图一行一个任务：40 个执行者只让调度者多一列，不多 40 行。
    #[test]
    fn level_view_folds_a_crew_into_one_row_and_one_column() {
        let sessions = crew_fixture(40);
        let table = level_table(&sessions, 120);
        let data: Vec<&str> = table.lines().filter(|l| l.starts_with('┃')).collect();
        // 表头 + 2 个任务（调度者 / 陌生人）。
        assert_eq!(data.len(), 3, "{table}");
        assert!(data[0].contains("CREW"), "{table}");
        let sched = row_cells(data[1]);
        assert_eq!(sched[0], "sched");
        // 40 个后代：i%4==0 的 10 个在等、i%4==1 的 10 个在干活。
        assert_eq!(sched[6], "40 ?10 ●10", "{table}");
        // 没有后代的行在列存在时显示 0。
        assert_eq!(row_cells(data[2])[6], "0");
        assert!(!table.contains("kid00"), "crew 不逐行枚举：{table}");
    }

    /// 没有 crew 时不打扰：`CREW` 列与汇总行都不出现。
    #[test]
    fn level_view_omits_the_crew_column_without_descendants() {
        let sessions = vec![session("a", "idle", &[]), session("b", "working", &[])];
        let table = level_table(&sessions, 120);
        assert!(!table.contains("CREW"), "{table}");
        assert!(!table.contains("folded"), "{table}");
        for line in table.lines().filter(|l| l.starts_with('┃')) {
            assert_eq!(row_cells(line).len(), 6, "{line}");
        }
    }

    /// 存活闭包：dormant 的父任务只要还有活着的子就仍在默认视图里（否则它的活子
    /// 会升成假根，crew 也数不全）；整棵都 dormant 的任务不列。
    #[test]
    fn level_view_keeps_a_dormant_parent_of_a_live_child() {
        let sessions = vec![
            session("live", "idle", &[]),
            session("parked", "inactive", &[]),
            child_of(
                last_at(session("parked-kid", "idle", &[]), "2026-10-15T21:00:00"),
                "parked",
            ),
        ];
        let rows = level(&sessions, false, &[], None);
        assert_eq!(row_ids(&rows), ["parked", "live"]);
        assert_eq!(
            rows[0].crew.map(|crew| crew.descendants),
            Some(1),
            "活子是 dormant 父的 crew"
        );

        let sessions = vec![
            session("live", "idle", &[]),
            session("gone", "inactive", &[]),
            child_of(session("gone-kid", "inactive", &[]), "gone"),
        ];
        assert_eq!(row_ids(&level(&sessions, false, &[], None)), ["live"]);
    }

    /// `--all` 不过滤：彻底 dormant 的任务也在（仍是一层一行）。
    #[test]
    fn all_lists_every_root_and_still_folds_the_crew() {
        let sessions = vec![
            session("live", "idle", &[]),
            session("gone", "inactive", &[]),
            child_of(session("gone-kid", "inactive", &[]), "gone"),
        ];
        let rows = level(&sessions, true, &[], None);
        assert_eq!(row_ids(&rows), ["gone", "live"]);
        let gone = rows
            .iter()
            .find(|row| row.session.id == "gone")
            .expect("row");
        assert_eq!(gone.crew.map(|crew| crew.descendants), Some(1));
    }

    /// `--tag` 不是「一层」而是标签子集：行按森林全序取，**不带 rollup**（行集是子集，
    /// 逐行计数会把不在屏上的后代也数进来）。
    #[test]
    fn tag_view_takes_the_subset_in_forest_order_without_a_rollup() {
        let sessions = vec![
            session("tagged-quiet", "idle", &["task=a"]),
            last_at(
                session("tagged-busy", "idle", &["task=a"]),
                "2026-10-15T21:30:00",
            ),
            // 未打标签、但在等人类回答的子：不列，但它让父任务有注意力（排在前面）。
            child_of(session("kid", "waiting", &[]), "tagged-quiet"),
        ];
        let rows = level(&sessions, true, &["task=a".to_string()], None);
        assert_eq!(row_ids(&rows), ["tagged-quiet", "tagged-busy"]);
        assert!(
            rows.iter().all(|row| row.crew.is_none()),
            "收窄视图不给 rollup"
        );
        assert!(
            rows.iter().all(|row| row.level),
            "但仍是层视图（LAST 读子树活跃）"
        );
    }

    /// `--parent` = 一层往下：只列直接子（父自己不在、孙不在），**不带 rollup**。
    #[test]
    fn parent_view_lists_the_direct_children_without_a_rollup() {
        let sessions = vec![
            session("P", "working", &[]),
            child_of(session("c1", "idle", &[]), "P"),
            child_of(session("c2", "idle", &[]), "P"),
            child_of(session("g", "waiting", &[]), "c1"),
        ];
        let rows = level(&sessions, false, &[], Some("P"));
        assert_eq!(row_ids(&rows), ["c1", "c2"], "注意力让带人的子排在前面");
        assert!(
            rows.iter().all(|row| row.crew.is_none()),
            "子层的行是 crew 不是任务：数字留给 --tree"
        );
        // 未知 / 已归档的父：空行集（诚实降级，不猜）。
        assert!(level(&sessions, false, &[], Some("gone")).is_empty());
    }

    /// `--tag` 与 `--parent` 是 AND（与平铺模式的 `filter_sessions` 同一条规则）。
    #[test]
    fn parent_and_tag_filters_are_anded() {
        let sessions = vec![
            child_of(session("s1", "inactive", &["task=a"]), "P"),
            child_of(session("s2", "inactive", &["task=b"]), "P"),
            child_of(session("s3", "inactive", &["task=a"]), "Q"),
        ];
        let rows = level(&sessions, true, &["task=a".to_string()], Some("P"));
        assert_eq!(row_ids(&rows), ["s1"]);
    }

    /// 层视图的 LAST 对父行是子树活跃（与面板同口径）；叶子行仍是自己的时间
    /// ——与平铺逐字一致。
    #[test]
    fn level_view_last_is_the_subtree_activity_for_a_parent() {
        let sessions = vec![
            last_at(session("old-parent", "idle", &[]), "2026-10-15T10:00:00"),
            child_of(
                last_at(session("fresh-kid", "idle", &[]), "2026-10-15T21:59:22"),
                "old-parent",
            ),
        ];
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 120,
            color: false,
        };
        let now = naive("2026-10-15T22:00:00");

        let level = format_sessions_table(&level(&sessions, false, &[], None), &palette, &out, now);
        let row = level
            .lines()
            .find(|l| l.contains("old-parent"))
            .expect("row");
        assert_eq!(row_cells(row)[2], "38s", "父行取子树最近动静：{level}");

        let flat = format_sessions_table(&flat_rows(&sessions[..1]), &palette, &out, now);
        let row = flat
            .lines()
            .find(|l| l.contains("old-parent"))
            .expect("row");
        assert_eq!(row_cells(row)[2], "12h", "平铺仍是自己的时间：{flat}");
    }

    /// 父行的 LAST 与叶子行**同一条降级**（同一个格式化函数：原样展示 / 空串留空 /
    /// 缺失给 `-`）。
    #[test]
    fn level_view_last_degrades_like_a_leaf_row() {
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 120,
            color: false,
        };
        let now = naive("2026-10-15T22:00:00");
        let last_of = |sessions: &[SessionInfo], id: &str| {
            let table =
                format_sessions_table(&level(sessions, false, &[], None), &palette, &out, now);
            let row = table
                .lines()
                .find(|l| l.contains(id))
                .expect("row")
                .to_string();
            (row_cells(&row)[2].clone(), table)
        };

        // 解析不了：父行的子树活跃只有这一个值 → 原样展示（不是 `-`）。
        let garbage = vec![
            last_at(session("garbage-parent", "idle", &[]), "not-a-time"),
            child_of(session("quiet-kid", "idle", &[]), "garbage-parent"),
        ];
        let (cell, table) = last_of(&garbage, "garbage-parent");
        assert_eq!(cell, "not-a-time", "父行与叶子行同一条降级：{table}");

        // 带时区偏移：原样展示，不再拿「UTC 墙钟 vs 本地 now」算出一个 8h 的年龄。
        let offset = vec![
            session("offset-parent", "idle", &[]),
            child_of(
                last_at(
                    session("offset-kid", "idle", &[]),
                    "2026-10-15T21:59:22+08:00",
                ),
                "offset-parent",
            ),
        ];
        let (cell, table) = last_of(&offset, "offset-parent");
        assert_eq!(cell, "2026-10-15T21:59:22+08:00", "原样展示：{table}");
    }

    /// `--flat` / `--tree` 里没有 `CREW` 列：老布局逐字保留（层视图才是新面）。
    #[test]
    fn flat_and_tree_tables_have_no_crew_column() {
        let sessions = crew_fixture(4);
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 120,
            color: false,
        };
        let now = naive("2026-10-15T22:00:00");
        for table in [
            format_sessions_table(&flat_rows(&sessions), &palette, &out, now),
            format_sessions_table(&tree_rows(&sessions), &palette, &out, now),
        ] {
            assert!(!table.contains("CREW"), "{table}");
            assert!(!table.contains("folded"), "{table}");
            for line in table.lines().filter(|l| l.starts_with('┃')) {
                assert_eq!(row_cells(line).len(), 6, "{line}");
            }
        }
    }

    /// 行尾汇总只在有 crew 被折起时出现，且必须说明两条出口；单数形态不写
    /// `1 tasks`，整行不超过 80 列（80 列终端是最常见的窄宽）。
    #[test]
    fn the_summary_line_names_both_ways_out() {
        let sessions = crew_fixture(3);
        let text = level_table(&sessions, 120);
        let last = text.lines().last().expect("summary line");
        assert!(last.starts_with("2 tasks · 3 crew folded"), "{text}");
        assert!(last.contains("`--parent <sid>`"), "{text}");
        assert!(last.contains("`--tree`"), "{text}");
        assert!(
            unicode_width::UnicodeWidthStr::width(last) <= 80,
            "汇总行要能塞进 80 列终端：{last}"
        );

        let single = crew_summary(&level(&sessions[..4], false, &[], None)).expect("crew");
        assert!(single.starts_with("1 task · 3 crew folded"), "{single}");
        // 没有 crew 被折起 → 不打。
        assert_eq!(crew_summary(&level(&sessions[4..], false, &[], None)), None);
    }

    /// 行尾汇总只描述**根层**：`--tag`（标签子集）与 `--parent`（子层）不打。这里走
    /// `sessions_table`（判据本身落在这里），不是给 `render_view` 一个现成的 bool。
    #[test]
    fn only_the_root_level_view_carries_the_summary() {
        let sessions = vec![
            session("P", "working", &["task=a"]),
            child_of(session("c1", "idle", &["task=a"]), "P"),
            child_of(session("c2", "idle", &["task=a"]), "P"),
            child_of(session("g", "waiting", &[]), "c1"),
        ];
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 120,
            color: false,
        };
        let table = |tags: &[String], parent: Option<&str>| {
            sessions_table(
                sessions.clone(),
                TableMode::Level,
                true,
                tags,
                parent,
                &palette,
                &out,
            )
        };

        // 根层（默认）：P 折起 c1 / c2 / g → 有汇总行。
        let root = table(&[], None);
        assert!(root.contains("1 task · 3 crew folded"), "{root}");
        assert!(root.contains("CREW"), "{root}");

        // 子层（`--parent`）：行集是子集 ⇒ 既没有汇总行，也没有 `CREW` 列（同一条判据：
        // 逐行 rollup 会把不在这一层的孙辈也数进来，同屏重复计数）。
        let children = table(&[], Some("P"));
        assert!(!children.contains("CREW"), "{children}");
        assert!(!children.contains("folded"), "{children}");

        // 标签子集：同上（命中的行逐行列出，行集不是一层）。
        let tagged = table(&["task=a".to_string()], None);
        assert!(!tagged.contains("CREW"), "{tagged}");
        assert!(!tagged.contains("folded"), "{tagged}");
    }

    /// 收窄视图（`--tag` / `--parent`）没有 `CREW` 列，但 LAST 仍是子树活跃
    /// ——列消失不改投影口径。
    #[test]
    fn narrowed_level_views_carry_no_crew_column_but_keep_the_subtree_last() {
        let sessions = vec![
            last_at(
                session("tagged-parent", "idle", &["task=a"]),
                "2026-10-15T10:00:00",
            ),
            child_of(
                last_at(
                    session("tagged-kid", "idle", &["task=a"]),
                    "2026-10-15T20:00:00",
                ),
                "tagged-parent",
            ),
            // 未打标签的孙：进父行 / 子行的子树活跃，但不进任何一行。
            child_of(
                last_at(session("plain-grand", "idle", &[]), "2026-10-15T21:59:22"),
                "tagged-kid",
            ),
        ];
        let palette = ThemePalette::default();
        let out = TableOutput {
            width: 120,
            color: false,
        };
        let now = naive("2026-10-15T22:00:00");
        let rows = level(&sessions, true, &["task=a".to_string()], None);
        let table = format_sessions_table(&rows, &palette, &out, now);
        assert!(!table.contains("CREW"), "{table}");
        assert!(!table.contains("▸"), "没有徽章列：{table}");
        // 六列（老的布局）：CREW 不在其中。
        for line in table.lines().filter(|l| l.starts_with('┃')) {
            assert_eq!(row_cells(line).len(), 6, "{line}");
        }
        // LAST 依旧取子树活跃的来源行（孙那一行），与整层视图同一条投影。
        let kid = table
            .lines()
            .find(|l| l.contains("tagged-kid"))
            .expect("row");
        assert_eq!(row_cells(kid)[2], "38s", "{table}");
    }

    /// `CREW` 列在窄终端（80 列）下参与同一套让位：7 列都在、行等宽、id 完整；
    /// `?k` / `●k` 会被截掉（列宽交给引擎，不为它开 `keep_natural` —— 那会让整表
    /// 溢出终端）。要读注意力标记就加宽终端或看 `--tree`。
    #[test]
    fn the_crew_column_yields_with_the_rest_on_a_narrow_terminal() {
        // 真实形态的宽度：22 列 id + CJK 名字，自然宽度明显超过 80。
        let mut sessions = vec![pinned(
            "20261015-211837-sched",
            "working",
            Some("2026-10-15T20:00:00"),
        )];
        sessions[0].name = Some("调度者：会话列表改版".into());
        for i in 0..12 {
            let status = if i % 3 == 0 { "waiting" } else { "working" };
            let mut kid = child_of(
                session(
                    &format!("20261015-2200{i:02}-kid{i:02}"),
                    status,
                    &["executor"],
                ),
                "20261015-211837-sched",
            );
            kid.name = Some(format!("执行者 {i}：表格渲染"));
            sessions.push(kid);
        }

        let table = level_table(&sessions, 80);
        let lines: Vec<&str> = table.lines().filter(|l| l.starts_with('┃')).collect();
        assert!(lines[0].contains("CREW"), "{table}");
        for line in &lines {
            assert_eq!(row_cells(line).len(), 7, "{line}");
            assert_eq!(
                unicode_width::UnicodeWidthStr::width(*line),
                80,
                "行宽 = 终端宽：{line}"
            );
        }
        // id 是精确句柄：窄终端下仍完整。
        assert!(table.contains("20261015-211837-sched"), "{table}");
        // CREW 的第一段（后代总数）还在；`?k` / `●k` 已让位让掉了（12 个后代里 4 个
        // 在等、8 个在干活，完整形态是 `12 ?4 ●8`）。
        let row = lines
            .iter()
            .find(|l| l.contains("20261015-211837-sched"))
            .expect("scheduler row");
        let crew = &row_cells(row)[6];
        assert!(crew.starts_with("12"), "CREW：{crew}\n{table}");
        assert!(
            !table.contains("12 ?4 ●8"),
            "80 列下注意力标记应被让位：{table}"
        );
    }

    /// 一行都没有（全是 dormant）时与平铺同一个出口。
    #[test]
    fn an_empty_level_view_says_there_are_no_sessions() {
        let sessions = vec![session("gone", "inactive", &[])];
        assert_eq!(level_table(&sessions, 120), "No sessions found.\n");
    }

    // ---- `wing info` 的用量块 ----

    fn usage_row(key: &str, parent: Option<&str>, measures: UsageMeasures) -> UsageRow {
        UsageRow {
            key: key.into(),
            label: None,
            parent_session_id: parent.map(str::to_string),
            measures,
        }
    }

    fn usage_report(rows: Vec<UsageRow>, totals: UsageMeasures) -> UsageResponse {
        UsageResponse {
            rows,
            totals,
            ..UsageResponse::default()
        }
    }

    fn own_measures() -> UsageMeasures {
        UsageMeasures {
            sessions: 1,
            user_messages: 34,
            llm_calls: 156,
            tool_calls: 210,
            prompt_tokens: 1_200_000,
            completion_tokens: 45_600,
            cached_tokens: 936_000,
            llm_time_ms: 6_000_000,
            tool_time_ms: 2_040_000,
            ..UsageMeasures::default()
        }
    }

    fn child_measures() -> UsageMeasures {
        UsageMeasures {
            sessions: 1,
            prompt_tokens: 4_600_000,
            completion_tokens: 180_000,
            ..UsageMeasures::default()
        }
    }

    /// `own + child` 的合计（端点的 `totals` 就是这个形状；生产侧从不由行相加得出）。
    fn subtree_measures() -> UsageMeasures {
        UsageMeasures {
            sessions: 2,
            user_messages: 34,
            llm_calls: 156,
            tool_calls: 210,
            prompt_tokens: 5_800_000,
            completion_tokens: 225_600,
            cached_tokens: 936_000,
            llm_time_ms: 6_000_000,
            tool_time_ms: 2_040_000,
            ..UsageMeasures::default()
        }
    }

    #[test]
    fn the_usage_block_reads_the_own_row_and_the_subtree_total() {
        let report = usage_report(
            vec![
                usage_row("sid", None, own_measures()),
                usage_row("child", Some("sid"), child_measures()),
            ],
            subtree_measures(),
        );
        let block = InfoUsage::from_report(&report, "sid");
        let text = format_usage_block(&block, None);
        assert!(text.starts_with("usage:\n"), "{text}");
        assert!(text.contains("  messages:           34"), "{text}");
        assert!(
            text.contains("  calls:              156 llm · 210 tools"),
            "{text}"
        );
        assert!(
            text.contains("  tokens:             ↑ 1.2M in (78% cached) · ↓ 45.6k out"),
            "{text}"
        );
        assert!(
            text.contains("  time:               llm 1h 40m · tools 34m · work 2h 14m"),
            "{text}"
        );
        assert!(
            text.contains("  tree:               1 sessions · ↑ 4.6M · ↓ 180.0k"),
            "{text}"
        );
    }

    #[test]
    fn a_session_without_descendants_omits_the_tree_line() {
        let own = UsageMeasures {
            sessions: 1,
            llm_calls: 1,
            prompt_tokens: 10,
            ..UsageMeasures::default()
        };
        let report = usage_report(vec![usage_row("sid", None, own.clone())], own);
        let block = InfoUsage::from_report(&report, "sid");
        assert!(block.tree.is_none());
        let text = format_usage_block(&block, None);
        assert!(!text.contains("tree:"), "{text}");
        assert!(
            text.contains("calls:              1 llm · 0 tools"),
            "{text}"
        );
    }

    #[test]
    fn a_session_without_flow_degrades_to_the_ledger_instead_of_zeros() {
        let report = usage_report(
            vec![usage_row("sid", None, UsageMeasures::default())],
            UsageMeasures::default(),
        );
        let block = InfoUsage::from_report(&report, "sid");
        let ledger = SessionUsage {
            turns: 12,
            llm_calls: 34,
            prompt_tokens: 1_200_000,
            completion_tokens: 45_000,
            ..SessionUsage::default()
        };
        assert_eq!(
            format_usage_block(&block, Some(&ledger)),
            format!("usage:  {NONE_MARK} (no audit flow; ledger: 12 turns · ↑1.2M ↓45.0k)\n")
        );
        // 账本也是空的：只说「没有流水」，不编造账本读数。
        assert_eq!(
            format_usage_block(&block, None),
            format!("usage:  {NONE_MARK} (no audit flow)\n")
        );
        assert!(!format_usage_block(&block, None).contains('0'));
    }

    /// `messages:` 行优先读会话账本：审计的 `audit_user_message` 是后加的表，存量会话的
    /// 流水是 0 而账本有真实值——只读流水会把「说过 7 条」显示成 0。
    #[test]
    fn the_messages_line_prefers_the_ledger_over_the_flow() {
        let own = UsageMeasures {
            sessions: 1,
            user_messages: 0,
            llm_calls: 12,
            prompt_tokens: 900,
            ..UsageMeasures::default()
        };
        let report = usage_report(vec![usage_row("sid", None, own.clone())], own.clone());
        let block = InfoUsage::from_report(&report, "sid");
        let ledger = SessionUsage {
            user_messages: 7,
            turns: 3,
            ..SessionUsage::default()
        };
        let text = format_usage_block(&block, Some(&ledger));
        assert!(text.contains("  messages:           7"), "{text}");
        assert!(!text.contains("messages:           0"), "{text}");

        // 账本没有（或为 0）且流水也是 0 → `—`（不写 0）。
        assert!(
            format_usage_block(&block, None).contains(&format!("messages:           {NONE_MARK}")),
            "两者都没有读数时不写 0"
        );
        let flow = UsageMeasures {
            user_messages: 5,
            ..own.clone()
        };
        let report = usage_report(vec![usage_row("sid", None, flow.clone())], flow);
        let block = InfoUsage::from_report(&report, "sid");
        assert!(
            format_usage_block(&block, None).contains("  messages:           5"),
            "回退到流水"
        );

        // 两者都没有 → `—`（不写 0）。
        let quiet = UsageMeasures {
            llm_calls: 1,
            ..UsageMeasures::default()
        };
        let report = usage_report(vec![usage_row("sid", None, quiet.clone())], quiet);
        let block = InfoUsage::from_report(&report, "sid");
        let text = format_usage_block(&block, Some(&SessionUsage::default()));
        assert!(
            text.contains(&format!("  messages:           {NONE_MARK}")),
            "{text}"
        );
    }

    /// 只有目录事实（`sessions_created`）的会话不是「有读数」：`—` 降级必须可达，
    /// 子树的 `tree:` 行也不能因为「目录里多了一行」就冒出来。
    #[test]
    fn a_catalog_only_row_is_not_a_reading() {
        let catalog_only = UsageMeasures {
            sessions_created: 1,
            ..UsageMeasures::default()
        };
        let ledger = SessionUsage {
            turns: 12,
            user_messages: 7,
            prompt_tokens: 1_200_000,
            completion_tokens: 45_000,
            ..SessionUsage::default()
        };
        let report = usage_report(
            vec![usage_row("sid", None, catalog_only.clone())],
            catalog_only,
        );
        let block = InfoUsage::from_report(&report, "sid");
        assert!(block.tree.is_none(), "目录行不是子孙流水");
        let text = format_usage_block(&block, Some(&ledger));
        assert!(
            text.starts_with(&format!("usage:  {NONE_MARK}")),
            "只建过会话 → 走 `—` 降级：\n{text}"
        );
        assert!(!text.contains("calls:"), "不打一排 0：\n{text}");
        assert!(text.contains("ledger: 12 turns"), "回落到账本：\n{text}");
    }

    /// 子树汇总取 `totals`（不被 `limit` 截断），不靠把返回的行加起来：
    /// 子孙超过 200 时「行和」会静默偏小。
    #[test]
    fn the_tree_line_survives_a_truncated_row_set() {
        let report = UsageResponse {
            rows: vec![
                usage_row("sid", None, own_measures()),
                usage_row("child", Some("sid"), child_measures()),
            ],
            // 250 个子孙的完整汇总（行只回来了 1 个）。
            totals: UsageMeasures {
                sessions: 251,
                user_messages: 34,
                llm_calls: 156,
                tool_calls: 210,
                prompt_tokens: 4_601_200_000,
                completion_tokens: 180_045_600,
                cached_tokens: 936_000,
                llm_time_ms: 6_000_000,
                tool_time_ms: 2_040_000,
                ..UsageMeasures::default()
            },
            truncated: true,
            ..UsageResponse::default()
        };
        let block = InfoUsage::from_report(&report, "sid");
        let text = format_usage_block(&block, None);
        assert!(text.contains("250 sessions"), "不靠行数：\n{text}");
        assert!(text.contains("↑ 4.6G"), "{text}");
    }

    /// 自身没有流水、但子孙有：自身那一小节写 `—`（不写一排 0），`tree:` 照常。
    #[test]
    fn a_quiet_own_row_degrades_alone_while_the_tree_line_stays() {
        let report = usage_report(
            vec![
                usage_row("sid", None, UsageMeasures::default()),
                usage_row("child", Some("sid"), child_measures()),
            ],
            child_measures(),
        );
        let block = InfoUsage::from_report(&report, "sid");
        let text = format_usage_block(&block, None);
        assert!(text.starts_with(&format!("usage:  {NONE_MARK}")), "{text}");
        assert!(!text.contains("messages:           0"), "{text}");
        assert!(text.contains("tree:"), "{text}");
        assert!(text.contains("1 sessions"), "{text}");
    }
}
