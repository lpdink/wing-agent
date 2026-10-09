//! `wing config` — headless 配置入口（Setting API）。
//!
//! 设置面板只服务 TUI；stdio / ACP / CI / 编排器用户没有面板，`wing config` 是他们读写配置的入口，
//! 也是**配置坏掉（网关降级 / setup mode）时唯一还能用的**诊断入口——那正是它最有价值的时刻。
//! 全部读写走 Setting API：校验、写盘、热重载都在后端（与 TUI 面板共用同一条写盘路径），
//! 本模块只做参数解析、值强制转换、稀疏文档上的结构编辑与输出格式化。
//!
//! ```sh
//! wing config doctor [--json]                 # 校验并列出全部问题（setup mode 下也能用）
//! wing config list [--json] [--section S] [--only-overridden]
//! wing config get <path> [--json]
//! wing config set <path> [<value>] [--json-value <raw>] [--force]
//! wing config unset <path> [--force]
//! wing config add <path> [<value>] [--variant simple|object] [--force]
//! wing config remove <path> [--force]
//! wing config move <path> <delta> [--force]
//! wing config path                            # 唯一不联网的子命令
//! ```
//!
//! # 退出码
//!
//! | 码 | 含义 |
//! |---|---|
//! | 0 | 成功（`doctor`：无问题；写命令：`ok=true` 或无操作短路） |
//! | 1 | 有问题（`doctor` 的 problems；写命令 `ok=false`，**未写盘**）或一般失败（5xx / 404 / 反序列化） |
//! | 2 | 网关不可达（`wing config` **不自动拉起网关**：先 `wing start`） |
//! | 3 | 乐观并发冲突（409：磁盘指纹与基线不一致）——重新执行同一条命令即可 |
//! | 4 | 用法错误（路径文法 / 目录寻址 / 值转换 / 边界 / 参数互斥） |
//!
//! clap 自身的解析错误（未知旗标、缺参数）沿用 clap 的退出码 2——它发生在 `dispatch` **之前**，
//! 上表的「4」只覆盖解析之后的失败。传输 / 协议级错误（2 / 3 / 1）一律 stderr 文本，
//! 与 `--json` 无关；`--json` 只影响「命令拿到了结构化结果」的输出路径。
//!
//! # 密文
//!
//! 密文（`kind == secret`）在任何输出路径（`get` / `list` / 写回执 / `--json`）都不回显真实值：
//! 读侧只拿得到后端掩码后的 `null` 与末 4 位 hint；写侧把输入原样交给后端，但从不打印它。
//!
//! # 已知边界（后端契约的推论，不是本模块的取舍）
//!
//! 保存是**全有或全无**（整份配置校验通过才写盘）。因此配置当前非法时，任何单条写命令都会被
//! 拒绝（一个字节都不写）；多问题修复请用 TUI 设置面板（本地累积编辑、一次保存）。
//! 另外 `set` 的 `<value>` 是字符串：`enum` 必须在 `choices` 内，`map` / `object` / `list` 收 JSON，
//! 任何值都能用 `--json-value` 直接给原始 JSON（逃生舱）。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::collections::HashMap;
use std::process::ExitCode;

use clap::Subcommand;
use serde::Serialize;
use serde_json::{Value, json};

use wing_api_client::models::{
    PathStep, SecretPresence, SecretState, SettingKind, SettingNode, SettingProblem,
    SettingsGetResponse, SettingsSchemaResponse, SettingsSetRequest, SettingsSetResponse,
    SettingsStatusResponse, parse_path,
};
use wing_api_client::{ApiClientError, GatewayClient as GatewayApiClient};

use super::{backend_config, common};

// ============================================================
// 子命令
// ============================================================

/// `wing config` 的子命令族（全部走 Setting API，`path` 除外）。
#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Validate the configuration and list every problem (works in setup mode).
    Doctor,

    /// Print the settings tree (path / value / default / apply scope / overridden / problems).
    List {
        /// Only show this top-level section (case-insensitive; e.g. `gateway`).
        #[arg(long)]
        section: Option<String>,
        /// Only show paths that are present in the sparse document (explicitly set).
        #[arg(long = "only-overridden")]
        only_overridden: bool,
    },

    /// Show one setting: value, default, doc, apply scope and constraints.
    Get {
        /// Canonical path (`gateway.port`, `providers[0].api_key`, …).
        path: String,
    },

    /// Set one setting (the value is coerced by the catalog kind).
    Set {
        /// Canonical path (`gateway.port`, `providers[0].api_key`, …).
        path: String,
        /// New value as text (see `--json-value` for raw JSON).
        value: Option<String>,
        /// Raw JSON value, bypassing the local kind coercion entirely.
        #[arg(long = "json-value")]
        json_value: Option<String>,
        /// Skip the fingerprint check (`base: null`): overwrite even if another client changed it.
        #[arg(long)]
        force: bool,
    },

    /// Remove one setting from the document (back to its default).
    Unset {
        /// Canonical path.
        path: String,
        /// Skip the fingerprint check (`base: null`).
        #[arg(long)]
        force: bool,
    },

    /// Append an item to a list (scalar lists need a value; object lists insert a stub).
    Add {
        /// Canonical path of the list (`providers`, `providers[0].models`, …).
        path: String,
        /// Item value: scalar lists require it; object/map elements accept JSON.
        value: Option<String>,
        /// Raw JSON item value, bypassing the local kind coercion entirely.
        #[arg(long = "json-value")]
        json_value: Option<String>,
        /// Element shape for `variants` lists (`models`: simple = bare string, object = full spec).
        #[arg(long)]
        variant: Option<String>,
        /// Skip the fingerprint check (`base: null`).
        #[arg(long)]
        force: bool,
    },

    /// Remove the item at an explicit index from a list.
    Remove {
        /// Canonical path of the item (`providers[1]`, ...).
        path: String,
        /// Skip the fingerprint check (`base: null`).
        #[arg(long)]
        force: bool,
    },

    /// Move a list item by `delta` positions (clamped to the list bounds).
    Move {
        /// Canonical path of the item (`providers[1]`, ...).
        path: String,
        /// Signed number of positions: negative = up, positive = down.
        #[arg(allow_negative_numbers = true)]
        delta: i64,
        /// Skip the fingerprint check (`base: null`).
        #[arg(long)]
        force: bool,
    },

    /// Print the absolute path of `config.yaml` (offline — never talks to the gateway).
    Path,
}

// ============================================================
// 可注入的请求层
// ============================================================

/// `wing config` 依赖的 4 个 Setting API 操作（可注入：单测喂假实现，生产走真网关）。
///
/// 保持**私有**：`async fn in trait` 的 lint 只针对公开 trait，静态分发即可（不用 `dyn`）。
trait SettingsBackend {
    /// `GET /api/settings/schema`
    async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError>;
    /// `GET /api/settings/get`
    async fn get(&self) -> Result<SettingsGetResponse, ApiClientError>;
    /// `GET /api/settings/status`
    async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError>;
    /// `POST /api/settings/set`
    async fn set(&self, req: &SettingsSetRequest) -> Result<SettingsSetResponse, ApiClientError>;
}

impl SettingsBackend for GatewayApiClient {
    async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError> {
        self.settings_schema().await
    }

    async fn get(&self) -> Result<SettingsGetResponse, ApiClientError> {
        self.settings_get().await
    }

    async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError> {
        self.settings_status().await
    }

    async fn set(&self, req: &SettingsSetRequest) -> Result<SettingsSetResponse, ApiClientError> {
        self.settings_set(req).await
    }
}

// ============================================================
// 入口
// ============================================================

/// Entry point for `wing config`.
///
/// **不自动拉起网关**（D26）：`wing config` 常常正是在网关起不来时被用的，自动拉起会掩盖问题。
/// `path` 子命令是唯一不联网的分支。
pub async fn run(command: ConfigCommand, json: bool) -> ExitCode {
    if matches!(command, ConfigCommand::Path) {
        return emit(execute_path(json));
    }
    // 只从后端配置读 host:port —— **不**走 `common::ensure_gateway()`（那个会拉起网关，D26）。
    let gw = backend_config::read_backend_gateway_config();
    let http_base = format!("http://{}:{}", gw.host, gw.port);
    let client = match common::create_api_client(&gw.host, gw.port) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("wing config error: 无法构造 HTTP 客户端：{e}");
            return ExitCode::from(1);
        }
    };
    run_with(command, json, &client, &http_base).await
}

/// 与 [`run`] 相同，但请求层可注入（单测的全部入口）。
async fn run_with<B: SettingsBackend>(
    command: ConfigCommand,
    json: bool,
    backend: &B,
    http_base: &str,
) -> ExitCode {
    emit(execute(command, json, backend, http_base).await)
}

/// 打印结果（stdout / stderr 各一条）并转成退出码——全模块唯一的打印边界。
fn emit(outcome: Outcome) -> ExitCode {
    if let Some(stdout) = outcome.stdout.as_deref() {
        print!("{stdout}");
    }
    if let Some(stderr) = outcome.stderr.as_deref() {
        eprint!("{stderr}");
    }
    ExitCode::from(outcome.code)
}

/// 一条命令的执行结果：退出码 + stdout 载荷 + stderr 文案。
///
/// 打印只发生在 [`run_with`] 这一个边界上，命令本身只产出文本——于是单测能直接断言
/// 「哪条流上出现了什么、退出码是什么」，不需要捕获进程的 stdout/stderr（也因此能证明
/// 密文值在任何一条流上都不出现）。
///
/// 分流规则：命令的**业务结果**（doctor 的问题清单、写命令的 `ok=false` 回执）走 stdout；
/// **执行失败**（网关不可达 / 409 冲突 / 协议级错误）走 stderr。
#[derive(Debug)]
struct Outcome {
    code: u8,
    stdout: Option<String>,
    stderr: Option<String>,
}

impl Outcome {
    /// 成功载荷（文本已含结尾换行）。
    fn ok(stdout: String) -> Self {
        Self {
            code: 0,
            stdout: Some(stdout),
            stderr: None,
        }
    }

    /// 失败（`stderr` 已含结尾换行）。
    fn failure(code: u8, stderr: String) -> Self {
        Self {
            code,
            stdout: None,
            stderr: Some(stderr),
        }
    }

    /// 用法错误（退出码 4）。
    fn usage(message: &str) -> Self {
        Self::failure(4, format!("wing config error: {message}\n"))
    }

    /// 业务结果 + 非零退出码（写命令 `ok=false`；文本仍走 stdout）。
    fn refused(stdout: String) -> Self {
        Self {
            code: 1,
            stdout: Some(stdout),
            stderr: None,
        }
    }
}

/// 执行一条命令（产出文本，不打印）。
async fn execute<B: SettingsBackend>(
    command: ConfigCommand,
    json: bool,
    backend: &B,
    http_base: &str,
) -> Outcome {
    match command {
        ConfigCommand::Path => execute_path(json),
        ConfigCommand::Doctor => execute_doctor(backend, json, http_base).await,
        ConfigCommand::List {
            section,
            only_overridden,
        } => {
            execute_list(
                backend,
                json,
                http_base,
                section.as_deref(),
                only_overridden,
            )
            .await
        }
        ConfigCommand::Get { path } => execute_get(backend, json, http_base, &path).await,
        ConfigCommand::Set {
            path,
            value,
            json_value,
            force,
        } => {
            let op = WriteOp::Set {
                path,
                value,
                json_value,
            };
            execute_write(op, json, backend, http_base, force).await
        }
        ConfigCommand::Unset { path, force } => {
            execute_write(WriteOp::Unset { path }, json, backend, http_base, force).await
        }
        ConfigCommand::Add {
            path,
            value,
            json_value,
            variant,
            force,
        } => {
            let op = WriteOp::Add {
                path,
                value,
                json_value,
                variant,
            };
            execute_write(op, json, backend, http_base, force).await
        }
        ConfigCommand::Remove { path, force } => {
            execute_write(WriteOp::Remove { path }, json, backend, http_base, force).await
        }
        ConfigCommand::Move { path, delta, force } => {
            execute_write(
                WriteOp::Move { path, delta },
                json,
                backend,
                http_base,
                force,
            )
            .await
        }
    }
}

/// `wing config path`：本地推导（与后端 `get_config_path()` 同构），不联网。
fn execute_path(json: bool) -> Outcome {
    let path = config_path().to_string_lossy().to_string();
    if json {
        Outcome::ok(format!(
            "{}\n",
            json_string(&PathOutput { config_path: path })
        ))
    } else {
        Outcome::ok(format!("{path}\n"))
    }
}

/// config.yaml 的绝对路径：`$WING_HOME/core/config.yaml`（`backend_config::wing_root()` 已含 env 覆盖）。
fn config_path() -> std::path::PathBuf {
    backend_config::wing_root().join("core").join("config.yaml")
}

/// `--json` 输出形状：`wing config path`。
#[derive(Debug, Serialize)]
struct PathOutput {
    config_path: String,
}

// ============================================================
// doctor
// ============================================================

/// `wing config doctor`：只打 `status`（最便宜的预检），可用 = 0，有问题 = 1。
async fn execute_doctor<B: SettingsBackend>(backend: &B, json: bool, http_base: &str) -> Outcome {
    let status = match backend.status().await {
        Ok(status) => status,
        Err(e) => {
            let (code, message) = api_failure(&e, http_base, "读取配置状态失败");
            return Outcome::failure(code, format!("{message}\n"));
        }
    };

    let problems = sorted_problems(&status.problems);
    let path = config_path().to_string_lossy().to_string();
    let stdout = if json {
        let output = DoctorOutput {
            valid: status.valid,
            setup_mode: status.setup_mode,
            config_path: path,
            fingerprint: status.fingerprint.clone(),
            problems: problems.into_iter().cloned().collect(),
        };
        format!("{}\n", json_string(&output))
    } else {
        render_doctor(&status, &problems, &path)
    };

    if status.valid {
        Outcome::ok(stdout)
    } else {
        Outcome::refused(stdout)
    }
}

/// `--json` 输出形状：`wing config doctor`。
#[derive(Debug, Serialize)]
struct DoctorOutput {
    valid: bool,
    setup_mode: bool,
    config_path: String,
    fingerprint: Option<String>,
    problems: Vec<SettingProblem>,
}

/// doctor 的人类可读输出（纯函数，便于断言）。
fn render_doctor(
    status: &SettingsStatusResponse,
    problems: &[&SettingProblem],
    config_path: &str,
) -> String {
    if status.valid {
        return format!("✓ 配置可用（{config_path}）\n");
    }
    let mut out = String::new();
    let mode = if status.setup_mode {
        "· 网关处于修复模式"
    } else {
        ""
    };
    out.push_str(&format!("✗ 配置不可用（{config_path}）{mode}\n"));
    for problem in problems {
        let at = problem.path.as_deref().unwrap_or("(文档)");
        out.push_str(&format!("  {at}: {}\n", problem.message));
        if let Some(hint) = problem.hint.as_deref().filter(|h| !h.trim().is_empty()) {
            out.push_str(&format!("      ↳ {hint}\n"));
        }
    }
    out.push_str(&format!(
        "共 {} 个问题 · 运行 wing 打开设置面板，或 wing config set <path> <value>\n",
        problems.len()
    ));
    out
}

/// 问题排序（§14.1 的严重度序，未知种类排最后；同序按 path）。纯函数。
fn sorted_problems(problems: &[SettingProblem]) -> Vec<&SettingProblem> {
    let mut sorted: Vec<&SettingProblem> = problems.iter().collect();
    sorted.sort_by(|a, b| {
        problem_rank(&a.kind)
            .cmp(&problem_rank(&b.kind))
            .then_with(|| {
                a.path
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.path.as_deref().unwrap_or(""))
            })
    });
    sorted
}

/// 已知种类的严重度序；`conflict` 与未知种类一律排在最后（P6：未知必须容忍）。
fn problem_rank(kind: &str) -> usize {
    match kind {
        "missing_required" => 0,
        "unknown_reference" => 1,
        "duplicate" => 2,
        "empty_list" => 3,
        "invalid_value" => 4,
        "unknown_key" => 5,
        _ => 6,
    }
}

// ============================================================
// list / get（读路径）
// ============================================================

/// 读 schema + get（两条 GET）；失败已映射成（退出码 + stderr 文案）。
async fn fetch_catalog<B: SettingsBackend>(
    backend: &B,
    http_base: &str,
) -> Result<(SettingsSchemaResponse, SettingsGetResponse), Outcome> {
    let schema = match backend.schema().await {
        Ok(schema) => schema,
        Err(e) => {
            let (code, message) = api_failure(&e, http_base, "读取设置目录失败");
            return Err(Outcome::failure(code, format!("{message}\n")));
        }
    };
    let values = match backend.get().await {
        Ok(values) => values,
        Err(e) => {
            let (code, message) = api_failure(&e, http_base, "读取配置失败");
            return Err(Outcome::failure(code, format!("{message}\n")));
        }
    };
    Ok((schema, values))
}

/// `wing config list`
async fn execute_list<B: SettingsBackend>(
    backend: &B,
    json: bool,
    http_base: &str,
    section: Option<&str>,
    only_overridden: bool,
) -> Outcome {
    let (schema, current) = match fetch_catalog(backend, http_base).await {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };
    let rows = match build_rows(
        &schema.root,
        &current.values,
        &current.secrets,
        &current.problems,
        section,
        only_overridden,
    ) {
        Ok(rows) => rows,
        Err(message) => return Outcome::usage(&message),
    };

    if json {
        let output = ListOutput {
            config_path: current.config_path.clone(),
            fingerprint: current.fingerprint.clone(),
            setup_mode: current.setup_mode,
            // 文档级问题（path=null，如 YAML 语法错）挂不到任何行上，单列一份全量清单。
            problems: sorted_problems(&current.problems)
                .into_iter()
                .cloned()
                .collect(),
            rows,
        };
        Outcome::ok(format!("{}\n", json_string(&output)))
    } else {
        Outcome::ok(render_list_text(&rows))
    }
}

/// `--json` 输出形状：`wing config list`（树已拍平成行，顺序 = 文本顺序）。
#[derive(Debug, Serialize)]
struct ListOutput {
    config_path: String,
    fingerprint: String,
    setup_mode: bool,
    /// 全量问题清单（按严重度排序；行上另有一个 `problem` 便于就地标红）。
    problems: Vec<SettingProblem>,
    rows: Vec<ListRow>,
}

/// 一行（树被拍平；文本与 JSON 共用同一份构建结果）。
#[derive(Debug, Clone, Serialize)]
struct ListRow {
    /// 规范路径（具体下标；列表元素逐个展开）。
    path: String,
    /// catalog kind（`object` / `list` / `secret` / …）。
    kind: String,
    /// 生效域（`hot` / `next_session` / `restart` / `readonly`）。
    apply: String,
    /// 该路径是否显式写在稀疏文档里。
    overridden: bool,
    /// 标量值（结构行为 `null`；密文恒为 `null`——真实值不出网关）。
    value: Option<Value>,
    has_default: bool,
    default: Option<Value>,
    /// 密文状态（state + 末 4 位 hint）；非密文为 `null`。
    secret: Option<SecretState>,
    /// 该路径上的第一条问题（按严重度排序后取首条）。
    problem: Option<SettingProblem>,
    /// 结构节点（object / list）。
    structural: bool,
    /// 顶层分组（声明序；未声明分组时为 `null`）。
    section: Option<String>,
    /// 树深度（渲染缩进用；`rows` 顺序已是深度优先）。
    depth: usize,
    /// 是否是其兄弟里的最后一个（渲染 `└─` / `├─` 连接符）。
    last: bool,
}

/// `wing config get`
async fn execute_get<B: SettingsBackend>(
    backend: &B,
    json: bool,
    http_base: &str,
    path: &str,
) -> Outcome {
    let (schema, current) = match fetch_catalog(backend, http_base).await {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };

    let Some(steps) = parse_path(path) else {
        return Outcome::usage(&format!(
            "路径文法非法：`{path}`（示例：gateway.port / providers[0].api_key）"
        ));
    };
    let Some(node) = resolve_node(&schema.root, &current.values, &steps) else {
        return Outcome::usage(&format!(
            "路径不在设置目录中：`{path}`（用 `wing config list` 查看全部路径）"
        ));
    };

    let value = get_path(&current.values, &steps).cloned();
    let secret = current.secrets.get(path).cloned();
    let problem = sorted_problems(&current.problems)
        .into_iter()
        .find(|p| p.path.as_deref() == Some(path))
        .cloned();
    let output = GetOutput::new(path, node, value, secret, problem);

    if json {
        Outcome::ok(format!("{}\n", json_string(&output)))
    } else {
        Outcome::ok(output.render_text())
    }
}

/// `--json` 输出形状：`wing config get`。
#[derive(Debug, Serialize)]
struct GetOutput {
    path: String,
    kind: String,
    apply: String,
    /// 生效值：文档里有就是文档值，否则回落声明默认值；都没有是 `null`。
    value: Option<Value>,
    overridden: bool,
    has_default: bool,
    default: Option<Value>,
    secret: Option<SecretState>,
    problem: Option<SettingProblem>,
    /// 目录节点全量（协议类型原样；零映射零漂移）。
    node: SettingNode,
}

impl GetOutput {
    fn new(
        path: &str,
        node: &SettingNode,
        value: Option<Value>,
        secret: Option<SecretState>,
        problem: Option<SettingProblem>,
    ) -> Self {
        let overridden = value.is_some();
        // 防线：即便上游没有掩码，密文也不会经本 CLI 的任一输出流出。
        let value = value.map(|value| redact_secrets(node, &value));
        let default = if node.secret {
            None
        } else {
            node.default.clone()
        };
        let effective = value
            .clone()
            .or_else(|| default.clone().filter(|_| node.has_default));
        Self {
            path: path.to_string(),
            kind: node.kind.as_str().to_string(),
            apply: node.apply.as_str().to_string(),
            value: effective,
            overridden,
            has_default: node.has_default,
            default,
            secret,
            problem,
            node: node.clone(),
        }
    }

    /// 人类可读输出（纯函数，便于断言）。
    fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{} = {}\n", self.path, self.render_value_column()));
        let default_column = match (&self.default, self.has_default) {
            (Some(value), true) => format!("默认 {}", render_json_value(value)),
            _ => "默认 (无)".to_string(),
        };
        let mut flags = format!("{default_column} · 生效域 {}", apply_short(&self.apply));
        if self.overridden {
            flags.push_str(" · 已覆盖");
        }
        if self.node.required {
            flags.push_str(" · 必填");
        }
        out.push_str(&format!("  {flags}\n"));
        out.push_str(&format!("  {}\n", self.node.doc));
        for note in self.node.notes.iter().filter(|n| !n.trim().is_empty()) {
            out.push_str(&format!("  {note}\n"));
        }
        if let Some(example) = self.node.example.as_deref() {
            out.push_str(&format!("  示例：{example}\n"));
        }
        let constraints = render_constraints(&self.node);
        if !constraints.is_empty() {
            out.push_str(&format!("  约束：{}\n", constraints.join(" · ")));
        }
        if !self.node.choices.is_empty() {
            let choices: Vec<String> = self
                .node
                .choices
                .iter()
                .map(
                    |choice| match choice.doc.as_deref().filter(|d| !d.trim().is_empty()) {
                        Some(doc) => format!("{}（{doc}）", choice.value),
                        None => choice.value.clone(),
                    },
                )
                .collect();
            out.push_str(&format!("  可选值：{}\n", choices.join(" / ")));
        }
        if let Some(problem) = &self.problem {
            out.push_str(&format!("  ! {}: {}\n", problem.kind, problem.message));
            if let Some(hint) = problem.hint.as_deref().filter(|h| !h.trim().is_empty()) {
                out.push_str(&format!("    ↳ {hint}\n"));
            }
        }
        out
    }

    /// 值列：密文一律掩码（真实值不出网关，也绝不到终端）。
    fn render_value_column(&self) -> String {
        if let Some(secret) = &self.secret {
            return render_secret(secret);
        }
        if self.node.is_structural() {
            return format!("({})", self.kind);
        }
        match &self.value {
            Some(value) => render_json_value(value),
            None => "(未设置)".to_string(),
        }
    }
}

/// 把目录 + 稀疏文档 + 密文表 + 问题表拍平成 `list` 的行（纯函数）。
fn build_rows(
    root: &SettingNode,
    values: &Value,
    secrets: &HashMap<String, SecretState>,
    problems: &[SettingProblem],
    section: Option<&str>,
    only_overridden: bool,
) -> Result<Vec<ListRow>, String> {
    let sections: Vec<String> = root
        .children
        .iter()
        .filter_map(|child| child.section.clone())
        .collect();
    if let Some(wanted) = section {
        let known = sections.iter().any(|s| s.eq_ignore_ascii_case(wanted));
        if !known {
            return Err(format!(
                "未知分组 `{wanted}`（可选：{}）",
                if sections.is_empty() {
                    "(无)".to_string()
                } else {
                    sections.join(" / ")
                }
            ));
        }
    }

    let ctx = RowCtx {
        secrets,
        problems: sorted_problems(problems),
        only_overridden,
    };
    let mut rows = Vec::new();
    for (index, child) in root.children.iter().enumerate() {
        let child_section = child.section.clone();
        if let Some(wanted) = section {
            let matches = child_section
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(wanted));
            if !matches {
                continue;
            }
        }
        let value = parse_path(&child.path).and_then(|steps| get_path(values, &steps));
        let last = index + 1 == root.children.len();
        walk_node(
            child,
            &child.path,
            value,
            0,
            last,
            child_section,
            &ctx,
            &mut rows,
        );
    }
    Ok(rows)
}

/// `build_rows` 的只读上下文。
struct RowCtx<'a> {
    secrets: &'a HashMap<String, SecretState>,
    problems: Vec<&'a SettingProblem>,
    only_overridden: bool,
}

/// 深度优先（声明序）拍平一个节点；列表按文档实际值展开具体元素。
#[allow(clippy::too_many_arguments)]
fn walk_node(
    node: &SettingNode,
    path: &str,
    value: Option<&Value>,
    depth: usize,
    last: bool,
    section: Option<String>,
    ctx: &RowCtx<'_>,
    out: &mut Vec<ListRow>,
) {
    if !ctx.only_overridden || value.is_some() {
        out.push(make_row(
            node,
            path,
            value,
            depth,
            last,
            section.clone(),
            ctx,
        ));
    }

    // 结构节点下钻：object → children；list → 文档里的具体元素（模板路径无法展开）。
    if !node.children.is_empty() {
        let last_index = node.children.len() - 1;
        for (index, child) in node.children.iter().enumerate() {
            let child_path = format!("{path}.{}", child.key);
            let child_value = value.and_then(|v| v.get(&child.key));
            walk_node(
                child,
                &child_path,
                child_value,
                depth + 1,
                index == last_index,
                section.clone(),
                ctx,
                out,
            );
        }
        return;
    }

    if node.is_structural() {
        let Some(array) = value.and_then(|v| v.as_array()) else {
            return;
        };
        let last_index = array.len().saturating_sub(1);
        for (index, item) in array.iter().enumerate() {
            let item_path = format!("{path}[{index}]");
            let Some(element) = element_node_for(node, item) else {
                continue;
            };
            walk_node(
                element,
                &item_path,
                Some(item),
                depth + 1,
                index == last_index,
                section.clone(),
                ctx,
                out,
            );
        }
    }
}

/// 列表元素节点：单一元素类型走 `element`；union 元素按文档实际值选形态（`select_variant`）。
fn element_node_for<'a>(list: &'a SettingNode, value: &Value) -> Option<&'a SettingNode> {
    if let Some(element) = list.element.as_deref() {
        return Some(element);
    }
    list.select_variant(value)
}

fn make_row(
    node: &SettingNode,
    path: &str,
    value: Option<&Value>,
    depth: usize,
    last: bool,
    section: Option<String>,
    ctx: &RowCtx<'_>,
) -> ListRow {
    let structural = node.is_structural();
    let secret = ctx.secrets.get(path).cloned();
    let problem = ctx
        .problems
        .iter()
        .find(|p| p.path.as_deref() == Some(path))
        .map(|p| (*p).clone());
    // 密文行不给值也不给默认值（真实值不出网关，也绝不到任何输出流）。
    let masked = secret.is_some() || node.secret;
    ListRow {
        path: path.to_string(),
        kind: node.kind.as_str().to_string(),
        apply: node.apply.as_str().to_string(),
        overridden: value.is_some(),
        value: if masked || structural {
            None
        } else {
            value.cloned()
        },
        has_default: node.has_default,
        default: if masked { None } else { node.default.clone() },
        secret,
        problem,
        structural,
        section,
        depth,
        last,
    }
}

/// 按目录把密文叶子替换成 `null`（纵深防御：上游若没掩码，值也不会出现在本模块的任何输出里）。
fn redact_secrets(node: &SettingNode, value: &Value) -> Value {
    if node.secret {
        return Value::Null;
    }
    if !node.children.is_empty() {
        let Value::Object(map) = value else {
            return value.clone();
        };
        let mut redacted = serde_json::Map::new();
        for (key, item) in map {
            let item = match node.children.iter().find(|child| &child.key == key) {
                Some(child) => redact_secrets(child, item),
                None => item.clone(),
            };
            redacted.insert(key.clone(), item);
        }
        return Value::Object(redacted);
    }
    if let Value::Array(array) = value {
        let mut redacted = Vec::with_capacity(array.len());
        for item in array {
            redacted.push(match element_target(node, Some(item)) {
                Some(element) => redact_secrets(element, item),
                None => item.clone(),
            });
        }
        return Value::Array(redacted);
    }
    value.clone()
}

/// `list` 的人类可读输出：section 分组头 + `├─` / `└─` 树形（纯函数）。
fn render_list_text(rows: &[ListRow]) -> String {
    if rows.is_empty() {
        return "（没有匹配的设置项）\n".to_string();
    }
    let mut out = String::new();
    let mut previous_section: Option<&str> = None;
    // 深度优先顺序下的祖先栈：`true` = 该祖先是自己兄弟里的最后一个（画空格而不是竖线）。
    let mut ancestors: Vec<bool> = Vec::new();
    for row in rows {
        let section = row.section.as_deref();
        if section != previous_section {
            if let Some(section) = section {
                out.push_str(&format!("── {section} ──\n"));
            }
            previous_section = section;
        }
        ancestors.truncate(row.depth);
        let indent = tree_indent(&ancestors, row.depth, row.last);
        ancestors.push(row.last);
        let bang = if row.problem.is_some() { "! " } else { "" };
        let mut line = format!("{indent}{bang}{}", row.path);
        if row.structural {
            line.push_str(&format!("  ({})", row.kind));
        } else {
            line.push_str(&format!(" = {}", render_row_value(row, row.overridden)));
        }
        if let Some(default) = row
            .default
            .as_ref()
            .filter(|_| row.has_default && !row.structural)
        {
            line.push_str(&format!("  默认 {}", render_json_value(default)));
        }
        line.push_str(&format!("  [{}]", apply_short(&row.apply)));
        if row.overridden {
            line.push_str("  已覆盖");
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// 行值列：密文掩码 / 文档值 / 回落默认值 / 未设置。
fn render_row_value(row: &ListRow, overridden: bool) -> String {
    if let Some(secret) = &row.secret {
        return render_secret(secret);
    }
    if let Some(value) = &row.value {
        return render_json_value(value);
    }
    if let Some(default) = row
        .default
        .as_ref()
        .filter(|_| !overridden && row.has_default)
    {
        return render_json_value(default);
    }
    "(未设置)".to_string()
}

/// 树形前缀：祖先层画 `│  ` / `   `，本层画 `├─ ` / `└─ `（根层无连接符）。
fn tree_indent(ancestors: &[bool], depth: usize, last: bool) -> String {
    if depth == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ancestor_last in ancestors {
        out.push_str(if *ancestor_last { "   " } else { "│  " });
    }
    out.push_str(if last { "└─ " } else { "├─ " });
    out
}

/// 生效域短标记（`next_session` → `session`）。
fn apply_short(apply: &str) -> &str {
    match apply {
        "next_session" => "session",
        other => other,
    }
}

/// 密文渲染：只给掩码 + 末 4 位 hint（state 未知也保守掩码）。
fn render_secret(secret: &SecretState) -> String {
    match &secret.state {
        SecretPresence::Set => match secret.hint.as_deref() {
            Some(hint) if !hint.is_empty() => format!("•••••••• {hint}"),
            _ => "••••••••".to_string(),
        },
        SecretPresence::Empty => "(空)".to_string(),
        SecretPresence::Absent => "(未设置)".to_string(),
        SecretPresence::Unknown(_) => match secret.hint.as_deref() {
            Some(hint) if !hint.is_empty() => format!("•••••••• {hint}"),
            _ => "••••••••".to_string(),
        },
    }
}

/// 值渲染：字符串不带引号，容器序列化成 JSON；一律截断（避免一行吃掉整个终端）。
fn render_json_value(value: &Value) -> String {
    match value {
        Value::String(text) => common::truncate_chars(text, VALUE_MAX_CHARS),
        other => common::truncate_chars(&other.to_string(), VALUE_MAX_CHARS),
    }
}

/// 值列的最大字符数（与 `cmd/query.rs` 的截断风格一致）。
const VALUE_MAX_CHARS: usize = 60;

/// 约束列（只列存在的项）。
fn render_constraints(node: &SettingNode) -> Vec<String> {
    let mut out = Vec::new();
    match (node.min, node.exclusive_min) {
        (Some(min), exclusive) if exclusive => out.push(format!("> {min}")),
        (Some(min), _) => out.push(format!("≥ {min}")),
        (None, _) => {}
    }
    match (node.max, node.exclusive_max) {
        (Some(max), exclusive) if exclusive => out.push(format!("< {max}")),
        (Some(max), _) => out.push(format!("≤ {max}")),
        (None, _) => {}
    }
    if let Some(min_length) = node.min_length {
        out.push(format!("长度 ≥ {min_length}"));
    }
    if let Some(pattern) = node.pattern.as_deref() {
        out.push(format!("匹配 {pattern}"));
    }
    if let Some(min_items) = node.min_items {
        out.push(format!("至少 {min_items} 项"));
    }
    if let Some(max_items) = node.max_items {
        out.push(format!("至多 {max_items} 项"));
    }
    if node.nullable {
        out.push("可置 null".to_string());
    }
    out
}

// ============================================================
// 写路径（set / unset / add / remove / move）
// ============================================================

/// 一条写命令（已解析的参数）。
#[derive(Debug)]
enum WriteOp {
    Set {
        path: String,
        value: Option<String>,
        json_value: Option<String>,
    },
    Unset {
        path: String,
    },
    Add {
        path: String,
        value: Option<String>,
        json_value: Option<String>,
        variant: Option<String>,
    },
    Remove {
        path: String,
    },
    Move {
        path: String,
        delta: i64,
    },
}

/// 本地规划的结论：要么提交一份新文档，要么无操作短路。
#[derive(Debug)]
enum Plan {
    Save {
        document: Value,
        note: Option<String>,
    },
    Noop {
        reason: &'static str,
        message: String,
    },
}

/// 写命令的统一骨架：读 → 本地改 → 保存（`--force` 时 `base: null` 跳过指纹检查）。
async fn execute_write<B: SettingsBackend>(
    op: WriteOp,
    json: bool,
    backend: &B,
    http_base: &str,
    force: bool,
) -> Outcome {
    // 先做不联网的输入校验：语法 / 参数互斥 / --variant 词表 / --json-value 语法。
    // 这样用法错误（4）在网关不可达时也能正确报出，而不是先撞上"网关不可达"。
    if let Err(message) = precheck_write(&op) {
        return Outcome::usage(&message);
    }

    let (schema, current) = match fetch_catalog(backend, http_base).await {
        Ok(pair) => pair,
        Err(outcome) => return outcome,
    };

    let plan = match plan_write(&op, &schema.root, &current.values) {
        Ok(plan) => plan,
        Err(message) => return Outcome::usage(&message),
    };

    let (document, note) = match plan {
        Plan::Save { document, note } => (document, note),
        Plan::Noop { reason, message } => {
            return if json {
                let output = WriteOutput {
                    ok: true,
                    noop: Some(reason.to_string()),
                    note: Some(message),
                    response: None,
                };
                Outcome::ok(format!("{}\n", json_string(&output)))
            } else {
                Outcome::ok(format!("（无操作）{message}\n"))
            };
        }
    };

    // `--force` = 跳过指纹检查（`base: null`；协议增补 P5）。
    let request = SettingsSetRequest {
        base: if force {
            None
        } else {
            Some(current.fingerprint.clone())
        },
        document,
    };

    match backend.set(&request).await {
        Ok(response) => {
            let stdout = if json {
                let output = WriteOutput {
                    ok: response.ok,
                    noop: None,
                    note: note.clone(),
                    response: Some(response.clone()),
                };
                format!("{}\n", json_string(&output))
            } else {
                render_write_text(&response, note.as_deref())
            };
            if response.ok {
                Outcome::ok(stdout)
            } else {
                Outcome::refused(stdout)
            }
        }
        Err(e) => {
            let (code, message) = api_failure(&e, http_base, "保存配置失败");
            let stderr = match &note {
                Some(note) => format!("{message}\n（改动未写盘：{note}）\n"),
                None => format!("{message}\n"),
            };
            Outcome::failure(code, stderr)
        }
    }
}

/// `--json` 输出形状：写命令（`noop` 短路与成功/被拒共用）。
#[derive(Debug, Serialize)]
struct WriteOutput {
    ok: bool,
    noop: Option<String>,
    note: Option<String>,
    response: Option<SettingsSetResponse>,
}

/// 写回执（纯函数，便于断言）：只打印路径与状态，**绝不打印值**（密文自然脱敏）。
fn render_write_text(response: &SettingsSetResponse, note: Option<&str>) -> String {
    let mut out = String::new();
    if !response.ok {
        out.push_str("✗ 保存被拒绝（配置校验未通过，未写盘）\n");
        for problem in sorted_problems(&response.problems) {
            let at = problem.path.as_deref().unwrap_or("(文档)");
            out.push_str(&format!("  {at}: {}\n", problem.message));
            if let Some(hint) = problem.hint.as_deref().filter(|h| !h.trim().is_empty()) {
                out.push_str(&format!("      ↳ {hint}\n"));
            }
        }
        out.push_str(&format!(
            "共 {} 个问题 · 运行 wing 打开设置面板，或 wing config set <path> <value>\n",
            response.problems.len()
        ));
        if let Some(note) = note {
            out.push_str(&format!("（本次改动未写盘：{note}）\n"));
        }
        return out;
    }

    out.push_str(&format!("✓ 已保存（{} 处变更）\n", response.changed.len()));
    for changed in &response.changed {
        out.push_str(&format!("  · {changed}\n"));
    }
    if let Some(note) = note {
        out.push_str(&format!("  {note}\n"));
    }
    if !response.restart_required.is_empty() {
        out.push_str(&format!(
            "⚠ 需重启网关才生效：{}\n",
            response.restart_required.join(", ")
        ));
        out.push_str("  运行 wing stop && wing start（或在 TUI 设置面板里按 r）\n");
    }
    if let Some(reload) = &response.reload {
        if !reload.results.is_empty() {
            let summary: Vec<String> = reload
                .results
                .iter()
                .map(|item| format!("{} {}", item.name, if item.ok { "✓" } else { "✗" }))
                .collect();
            out.push_str(&format!("热重载：{}\n", summary.join(" · ")));
        }
        for item in reload.results.iter().filter(|item| !item.ok) {
            if let Some(detail) = item.detail.as_deref() {
                out.push_str(&format!("  ✗ {}：{detail}\n", item.name));
            }
        }
        if !reload.ok {
            out.push_str("⚠ 热重载未全部成功（配置已写盘且合法）\n");
        }
    }
    if response.setup_mode_exited {
        out.push_str("✓ 网关已转入正常模式\n");
    }
    if let Some(backup) = response.backup_path.as_deref() {
        out.push_str(&format!("备份：{backup}\n"));
    }
    out
}

/// 写命令的本地规划：纯函数（无 I/O、无网络），全部边界在这里裁定。
fn plan_write(op: &WriteOp, root: &SettingNode, values: &Value) -> Result<Plan, String> {
    // 空文档（`values: null`）视作 `{}`：后端在空文件上返回的就是空对象，这里只做防御。
    let mut document = if values.is_null() {
        json!({})
    } else {
        values.clone()
    };
    // 稀疏文档里密文叶是 `null`（= 保留磁盘现值）；原样回传是契约（§7.5）。
    match op {
        WriteOp::Set {
            path,
            value,
            json_value,
        } => {
            let steps = write_steps(path)?;
            let incoming = incoming_value(value.as_deref(), json_value.as_deref())?;
            // union 列表元素的整项替换：形态由**新值**决定（`{` 开头 = 完整形态）；
            // 其余路径的形态由文档现值决定（`resolve_node`）。
            let node = match union_replacement_target(root, &document, &steps, incoming) {
                Some(variant) => variant,
                None => require_node(root, &document, &steps, path)?,
            };
            let coerced = incoming_json(node, incoming)?;
            apply_set(&mut document, &steps, coerced)?;
            Ok(Plan::Save {
                document,
                note: None,
            })
        }
        WriteOp::Unset { path } => {
            let steps = write_steps(path)?;
            require_node(root, &document, &steps, path)?;
            if apply_unset(&mut document, &steps)? {
                Ok(Plan::Save {
                    document,
                    note: None,
                })
            } else {
                Ok(Plan::Noop {
                    reason: "already_default",
                    message: format!("`{path}` 本就不在文档里（已在默认值上）"),
                })
            }
        }
        WriteOp::Add {
            path,
            value,
            json_value,
            variant,
        } => {
            let steps = write_steps(path)?;
            let list = require_node(root, &document, &steps, path)?;
            if list.kind != SettingKind::List {
                return Err(format!("`{path}` 不是列表（kind: {}）", list.kind.as_str()));
            }
            let choice = variant.as_deref().map(parse_variant).transpose()?;
            let element = add_element_node(list, choice)?;
            let item = match (value.as_deref(), json_value.as_deref()) {
                (Some(_), Some(_)) => {
                    return Err("位置参数 <value> 与 --json-value 不能同时给出".to_string());
                }
                (None, Some(raw)) => parse_json(raw)?,
                (Some(raw), None) => coerce_value(element, raw)?,
                (None, None) => match stub_value(element) {
                    Some(stub) => stub,
                    None => {
                        return Err(format!(
                            "`{path}` 的元素是标量（{}），新增时必须给 <value>（或对 object 形态用 --variant object 插骨架）",
                            element.kind.as_str()
                        ));
                    }
                },
            };
            let index = apply_add(&mut document, &steps, list, item)?;
            Ok(Plan::Save {
                document,
                note: Some(format!("新增项：{path}[{index}]")),
            })
        }
        WriteOp::Remove { path } => {
            let steps = write_steps(path)?;
            require_node(root, &document, &steps, path)?;
            // 摘要前先拿到元素节点：脱敏要按目录走（密文叶在输出里一律 null）。
            let element = removal_element(root, &document, &steps);
            let removed = apply_remove(&mut document, &steps)?;
            let summary = match element {
                Some(element) => redact_secrets(element, &removed),
                None => removed,
            };
            Ok(Plan::Save {
                document,
                note: Some(format!("已移除：{path} = {}", render_json_value(&summary))),
            })
        }
        WriteOp::Move { path, delta } => {
            let steps = write_steps(path)?;
            require_node(root, &document, &steps, path)?;
            let outcome = apply_move(&mut document, &steps, *delta)?;
            if !outcome.moved {
                let reason = if *delta == 0 {
                    "delta_zero"
                } else {
                    "already_at_edge"
                };
                let message = format!(
                    "`{path}` 已在边界（第 {} / {} 项），未移动",
                    outcome.from + 1,
                    outcome.length
                );
                return Ok(Plan::Noop { reason, message });
            }
            Ok(Plan::Save {
                document,
                note: Some(format!("已移动：{} → [{}]", path, outcome.to)),
            })
        }
    }
}

/// 写法/输入层面的纯校验（不联网）——见 `execute_write` 的调用点。
fn precheck_write(op: &WriteOp) -> Result<(), String> {
    match op {
        WriteOp::Set {
            path,
            value,
            json_value,
        } => {
            write_steps(path)?;
            let incoming = incoming_value(value.as_deref(), json_value.as_deref())?;
            if let Incoming::RawJson(raw) = incoming {
                parse_json(raw)?;
            }
        }
        WriteOp::Unset { path } | WriteOp::Remove { path } => {
            write_steps(path)?;
        }
        WriteOp::Add {
            path,
            value,
            json_value,
            variant,
        } => {
            write_steps(path)?;
            ensure_exclusive(value.as_deref(), json_value.as_deref())?;
            if let Some(raw) = json_value.as_deref() {
                parse_json(raw)?;
            }
            if let Some(variant) = variant.as_deref() {
                parse_variant(variant)?;
            }
        }
        WriteOp::Move { path, .. } => {
            write_steps(path)?;
        }
    }
    Ok(())
}

/// 位置参数与 `--json-value` 互斥（两处共用：预检 + `incoming_value`）。
fn ensure_exclusive(value: Option<&str>, json_value: Option<&str>) -> Result<(), String> {
    if value.is_some() && json_value.is_some() {
        return Err("位置参数 <value> 与 --json-value 不能同时给出".to_string());
    }
    Ok(())
}

/// 写命令的路径：必须非空、文法合法、且不含元素模板 `[]`。
fn write_steps(path: &str) -> Result<Vec<PathStep>, String> {
    let steps = parse_path(path).ok_or_else(|| {
        format!("路径文法非法：`{path}`（示例：gateway.port / providers[0].api_key）")
    })?;
    if steps.is_empty() {
        return Err("路径不能为空（写操作必须指向具体字段）".to_string());
    }
    if steps.contains(&PathStep::Element) {
        return Err(format!(
            "模板路径（[]）不能用于写操作：`{path}`（请用具体下标，如 providers[0]）"
        ));
    }
    Ok(steps)
}

/// 按路径在目录里寻址；找不到就是用法错误（带统一指引）。
fn require_node<'a>(
    root: &'a SettingNode,
    document: &Value,
    steps: &[PathStep],
    path: &str,
) -> Result<&'a SettingNode, String> {
    resolve_node(root, document, steps).ok_or_else(|| {
        format!("路径不在设置目录中：`{path}`（用 `wing config list` 查看全部路径）")
    })
}

/// `set` 到 union 列表元素（`providers[0].models[0]` 这种整项替换）时，元素形态按**新值**判定：
/// 新值看起来像 JSON 对象（`{` 开头 / `--json-value` 解析出对象）= 完整形态，其余 = 简单形态。
///
/// 理由：整项替换的"目标形态"只能由新值表达——当前文档里的旧值是什么形态，与新值无关。
/// `models: ["ds-flash"] → set providers[0].models[0] '{"id":"x"}'` 因此能就地变成对象形态。
/// 非 union 路径返回 `None`（调用方走常规寻址）。
fn union_replacement_target<'a>(
    root: &'a SettingNode,
    document: &Value,
    steps: &[PathStep],
    incoming: Incoming<'_>,
) -> Option<&'a SettingNode> {
    let (PathStep::Index(_), parents) = steps.split_last()? else {
        return None;
    };
    let wants_object = match incoming {
        Incoming::Positional(raw) => raw.trim_start().starts_with('{'),
        Incoming::RawJson(raw) => serde_json::from_str::<Value>(raw)
            .map(|value| value.is_object())
            .unwrap_or(false),
    };
    let list = resolve_node(root, document, parents)?;
    list.variants
        .as_deref()?
        .iter()
        .find(|variant| matches!(variant.kind, SettingKind::Object) == wants_object)
}

/// `remove` 的目标元素节点（打印摘要前按目录脱敏）。路径形态不对就不脱敏：
/// 值本来就来自后端已掩码的文档，这里只是纵深防御。
fn removal_element<'a>(
    root: &'a SettingNode,
    document: &Value,
    steps: &[PathStep],
) -> Option<&'a SettingNode> {
    let (PathStep::Index(_), parents) = steps.split_last()? else {
        return None;
    };
    let list = resolve_node(root, document, parents)?;
    element_target(list, get_path(document, steps))
}

/// 位置参数与 `--json-value` 二选一的结论。
#[derive(Debug, Clone, Copy)]
enum Incoming<'a> {
    /// 位置参数：按 catalog kind 强制转换。
    Positional(&'a str),
    /// `--json-value`：原始 JSON，不做任何 kind 检查（逃生舱）。
    RawJson(&'a str),
}

/// 位置参数与 `--json-value` 二选一。
fn incoming_value<'a>(
    value: Option<&'a str>,
    json_value: Option<&'a str>,
) -> Result<Incoming<'a>, String> {
    ensure_exclusive(value, json_value)?;
    match (value, json_value) {
        (Some(value), None) => Ok(Incoming::Positional(value)),
        (None, Some(raw)) => Ok(Incoming::RawJson(raw)),
        (None, None) => Err("缺少 <value>（或 --json-value <raw>）".to_string()),
        (Some(_), Some(_)) => unreachable!("exclusivity checked above"),
    }
}

/// 输入 → 待写入的 JSON 值：位置参数按 kind 转换，`--json-value` 原样解析。
fn incoming_json(node: &SettingNode, incoming: Incoming<'_>) -> Result<Value, String> {
    match incoming {
        Incoming::Positional(raw) => coerce_value(node, raw),
        Incoming::RawJson(raw) => parse_json(raw),
    }
}

// ============================================================
// 路径与目录
// ============================================================

/// 规范路径 → 文本（`Key` 用 `.` 连接，`Index` / `Element` 用方括号）。
fn format_path(steps: &[PathStep]) -> String {
    let mut out = String::new();
    for step in steps {
        match step {
            PathStep::Key(name) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(name);
            }
            PathStep::Index(index) => out.push_str(&format!("[{index}]")),
            PathStep::Element => out.push_str("[]"),
        }
    }
    out
}

/// 按路径取文档里的值（`None` = 路径缺席或含模板步）。
fn get_path<'a>(document: &'a Value, steps: &[PathStep]) -> Option<&'a Value> {
    let mut current = document;
    for step in steps {
        current = match step {
            PathStep::Key(name) => current.get(name)?,
            PathStep::Index(index) => current.get(*index)?,
            PathStep::Element => return None,
        };
    }
    Some(current)
}

/// 目录寻址：`Key` 走 `children`；`Index`/`Element` 走 `element`，
/// union 元素（`variants`）用**文档实际值** + [`SettingNode::select_variant`] 选形态。
///
/// 与 [`SettingNode::node_at`] 的分工：`node_at` 在 `variants` 场景按契约返回 `None`（catalog
/// 无法单独判定元素形态），而 CLI 手里有文档，所以这里补上"用值选形态"这一段；
/// 路径解析仍走 `parse_path`，选形态仍走 `select_variant`——没有第二份实现。
fn resolve_node<'a>(
    root: &'a SettingNode,
    document: &Value,
    steps: &[PathStep],
) -> Option<&'a SettingNode> {
    // 根名（`config`）是可选前缀：仅当首段不命中任何子节点时才吃掉它（与 `node_at` 同口径）。
    let steps: &[PathStep] = match steps {
        [PathStep::Key(first), rest @ ..]
            if !first.is_empty()
                && first == &root.key
                && !root.children.iter().any(|child| &child.key == first) =>
        {
            rest
        }
        all => all,
    };

    let mut node = root;
    let mut value: Option<&Value> = Some(document);
    for step in steps {
        match step {
            PathStep::Key(name) => {
                node = node.children.iter().find(|child| &child.key == name)?;
                value = value.and_then(|current| current.get(name));
            }
            PathStep::Index(index) => {
                let item = value.and_then(|current| current.get(*index));
                node = element_target(node, item)?;
                value = item;
            }
            PathStep::Element => {
                node = element_target(node, None)?;
                value = None;
            }
        }
    }
    Some(node)
}

/// 列表节点 → 元素节点：单一元素类型直接给 `element`；union 元素要一个值来选形态。
fn element_target<'a>(list: &'a SettingNode, item: Option<&Value>) -> Option<&'a SettingNode> {
    if let Some(element) = list.element.as_deref() {
        return Some(element);
    }
    list.select_variant(item?)
}

// ============================================================
// 值强制转换
// ============================================================

/// 值强制转换表（design.md D4）：把命令行文本按 catalog `kind` 转成 JSON。
///
/// - `nullable` 先行：`null` / `~` / 空串 → JSON `null`（任何 kind）；
/// - 容器（`map` / `object` / `list`）收 JSON 文本；
/// - 未知 kind 拒绝：让调用方改用 `--json-value`（逃生舱），而不是猜一种转换。
fn coerce_value(node: &SettingNode, raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if node.nullable && matches!(raw, "null" | "~" | "") {
        return Ok(Value::Null);
    }
    match &node.kind {
        SettingKind::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(json!(true)),
            "false" | "0" | "no" | "off" => Ok(json!(false)),
            _ => Err("需要一个布尔值（true/false）".to_string()),
        },
        SettingKind::Int => raw
            .parse::<i64>()
            .map(|number| json!(number))
            .map_err(|_| "需要一个整数".to_string()),
        SettingKind::Float => raw
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .map(|number| json!(number))
            .ok_or_else(|| "需要一个数字".to_string()),
        SettingKind::Str | SettingKind::Secret => Ok(json!(raw)),
        SettingKind::Enum => {
            if node.choices.iter().any(|choice| choice.value == raw) {
                Ok(json!(raw))
            } else {
                Err(format!("可选值：{}", render_choices(node)))
            }
        }
        SettingKind::Map | SettingKind::Object => match parse_json(raw)? {
            Value::Object(map) => Ok(Value::Object(map)),
            _ => Err("需要一个 JSON 对象".to_string()),
        },
        SettingKind::List => match parse_json(raw)? {
            Value::Array(array) => Ok(Value::Array(array)),
            _ => Err("需要一个 JSON 数组".to_string()),
        },
        SettingKind::Unknown(_) => Err(
            "无法把字符串转换成该类型（网关比 CLI 新？），请用 --json-value 指定原始 JSON"
                .to_string(),
        ),
    }
}

/// 枚举可选值的展示（`a` / `b`）。
fn render_choices(node: &SettingNode) -> String {
    node.choices
        .iter()
        .map(|choice| choice.value.clone())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// `--json-value`：原始 JSON，不做任何 kind 检查（调用方负责形状；不符由后端 problem 报告）。
fn parse_json(raw: &str) -> Result<Value, String> {
    serde_json::from_str(raw).map_err(|e| format!("不是合法 JSON：{e}"))
}

/// 容器元素的骨架：object / map → `{}`；list → `[]`；标量 → `None`（必须给值）。
///
/// 骨架**不发明假值**（不填 name / base_url 之类）：插进去的 problems 由后端如实报告，
/// 用户按提示用 `wing config set` 补齐，或者直接 `add <path> '{"name":…}'` 一次给全。
fn stub_value(node: &SettingNode) -> Option<Value> {
    match node.kind {
        SettingKind::Object | SettingKind::Map => Some(json!({})),
        SettingKind::List => Some(json!([])),
        _ => None,
    }
}

// ============================================================
// 文档编辑（纯函数）
// ============================================================

/// `set`：沿路径写值；中间缺失 / 为 `null` 的 object 自动创建。
///
/// 中间层是标量、或列表下标越界 → 用法错误（不猜、不扩张列表——扩张用 `add`）。
fn apply_set(document: &mut Value, steps: &[PathStep], value: Value) -> Result<(), String> {
    apply_set_at(document, steps, 0, value)
}

fn apply_set_at(
    current: &mut Value,
    steps: &[PathStep],
    at: usize,
    value: Value,
) -> Result<(), String> {
    if at == steps.len() {
        *current = value;
        return Ok(());
    }
    match &steps[at] {
        PathStep::Key(name) => {
            let map = current.as_object_mut().ok_or_else(|| {
                format!(
                    "路径 {} 的中间层 {} 不是对象",
                    format_path(steps),
                    format_path(&steps[..at])
                )
            })?;
            let next = map.entry(name.clone()).or_insert(Value::Null);
            if at + 1 == steps.len() {
                *next = value;
                return Ok(());
            }
            if next.is_null() {
                *next = json!({});
            }
            apply_set_at(next, steps, at + 1, value)
        }
        PathStep::Index(index) => {
            let array = current.as_array_mut().ok_or_else(|| {
                format!(
                    "路径 {} 的中间层 {} 不是列表",
                    format_path(steps),
                    format_path(&steps[..at])
                )
            })?;
            let length = array.len();
            let item = array.get_mut(*index).ok_or_else(|| {
                format!(
                    "列表下标越界：{} 只有 {} 项（下标 {}）",
                    format_path(&steps[..at]),
                    length,
                    index
                )
            })?;
            apply_set_at(item, steps, at + 1, value)
        }
        PathStep::Element => Err("模板路径（[]）不能用于写操作".to_string()),
    }
}

/// `unset`：从稀疏文档移除该路径；`Ok(false)` = 本就不存在（幂等无操作）。
///
/// 中间层类型不符同样视作"缺席"：unset 的语义是"确保它不在"，不是类型检查。
fn apply_unset(document: &mut Value, steps: &[PathStep]) -> Result<bool, String> {
    let Some((last, parents)) = steps.split_last() else {
        return Err("路径不能为空".to_string());
    };
    let mut current = document;
    for step in parents {
        current = match step {
            PathStep::Key(name) => match current.get_mut(name) {
                Some(next) => next,
                None => return Ok(false),
            },
            PathStep::Index(index) => match current.get_mut(*index) {
                Some(next) => next,
                None => return Ok(false),
            },
            PathStep::Element => return Err("模板路径（[]）不能用于写操作".to_string()),
        };
    }
    match last {
        PathStep::Key(name) => Ok(current
            .as_object_mut()
            .map(|map| map.remove(name).is_some())
            .unwrap_or(false)),
        PathStep::Index(index) => {
            let Some(array) = current.as_array_mut() else {
                return Ok(false);
            };
            if *index < array.len() {
                array.remove(*index);
                return Ok(true);
            }
            Ok(false)
        }
        PathStep::Element => Err("模板路径（[]）不能用于写操作".to_string()),
    }
}

/// `remove`：删除列表里的具体下标；数组缺席 / 下标越界 → 用法错误（带当前长度）。
fn apply_remove(document: &mut Value, steps: &[PathStep]) -> Result<Value, String> {
    let (array, index) = list_slot(document, steps)?;
    let length = array.len();
    if index >= length {
        return Err(format!(
            "列表下标越界：{} 只有 {} 项（下标 {}）",
            format_path(&steps[..steps.len() - 1]),
            length,
            index
        ));
    }
    Ok(array.remove(index))
}

/// `move` 的结论。
#[derive(Debug, PartialEq, Eq)]
struct MoveOutcome {
    moved: bool,
    from: usize,
    to: usize,
    length: usize,
}

/// `move`：把元素挪到 `i + delta`，**越界钳制**（`clamp(i + delta, 0, len-1)`），
/// 位移为 0（含 `delta == 0`）时 `moved = false`（调用方短路，不写盘）。
fn apply_move(document: &mut Value, steps: &[PathStep], delta: i64) -> Result<MoveOutcome, String> {
    let (array, index) = list_slot(document, steps)?;
    let length = array.len();
    if index >= length {
        return Err(format!(
            "列表下标越界：{} 只有 {} 项（下标 {}）",
            format_path(&steps[..steps.len() - 1]),
            length,
            index
        ));
    }
    // i128：`delta` 取 i64 极值时 `index + delta` 也不能溢出。
    let target = (index as i128 + delta as i128).clamp(0, length as i128 - 1) as usize;
    if target == index {
        return Ok(MoveOutcome {
            moved: false,
            from: index,
            to: index,
            length,
        });
    }
    let item = array.remove(index);
    array.insert(target, item);
    Ok(MoveOutcome {
        moved: true,
        from: index,
        to: target,
        length,
    })
}

/// `add`：在列表末尾追加一项，返回新下标。
///
/// 列表在稀疏文档里缺席时，用 catalog 的**声明默认值**物化（没有默认则空列表）——
/// 否则 `add tools bash` 会在"没显式写过 tools"时无处可落。
fn apply_add(
    document: &mut Value,
    steps: &[PathStep],
    list: &SettingNode,
    item: Value,
) -> Result<usize, String> {
    let existing = get_path(document, steps);
    let mut array = match existing {
        Some(Value::Array(array)) => array.clone(),
        Some(Value::Null) | None => match list.default.clone() {
            Some(Value::Array(default)) if list.has_default => default,
            _ => Vec::new(),
        },
        Some(_) => {
            return Err(format!(
                "{} 不是列表（文档里的值不是 JSON 数组）",
                format_path(steps)
            ));
        }
    };
    array.push(item);
    let index = array.len() - 1;
    apply_set(document, steps, Value::Array(array))?;
    Ok(index)
}

/// 定位"列表 + 具体下标"：路径必须以 `Index` 结尾、父层必须存在且是数组。
fn list_slot<'a>(
    document: &'a mut Value,
    steps: &[PathStep],
) -> Result<(&'a mut Vec<Value>, usize), String> {
    let Some((PathStep::Index(index), parents)) = steps.split_last() else {
        return Err(format!(
            "{} 必须以具体下标结尾（如 providers[1]）",
            format_path(steps)
        ));
    };
    let mut current = document;
    for step in parents {
        current = match step {
            PathStep::Key(name) => current.get_mut(name).ok_or_else(|| {
                format!(
                    "路径不在当前文档中：{}（先 set / add 建出来）",
                    format_path(parents)
                )
            })?,
            PathStep::Index(inner) => current.get_mut(*inner).ok_or_else(|| {
                format!(
                    "路径不在当前文档中：{}（先 set / add 建出来）",
                    format_path(parents)
                )
            })?,
            PathStep::Element => return Err("模板路径（[]）不能用于写操作".to_string()),
        };
    }
    let Some(array) = current.as_array_mut() else {
        return Err(format!(
            "{} 不是列表（文档里的值不是 JSON 数组）",
            format_path(parents)
        ));
    };
    Ok((array, *index))
}

// ============================================================
// `--variant` 与元素形态
// ============================================================

/// `--variant` 的形态词表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VariantChoice {
    /// 简单形态（裸字符串等标量）。
    Simple,
    /// 完整形态（对象 / map）。
    Object,
}

impl VariantChoice {
    fn word(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Object => "object",
        }
    }
}

/// 解析 `--variant` 的取值（只接受 `simple` / `object`）。
fn parse_variant(raw: &str) -> Result<VariantChoice, String> {
    match raw {
        "simple" => Ok(VariantChoice::Simple),
        "object" => Ok(VariantChoice::Object),
        other => Err(format!("--variant 只接受 simple|object，收到 `{other}`")),
    }
}

/// 节点在 `--variant` 语义下的形态：object / map = 完整形态，其余 = 简单形态。
fn variant_class(node: &SettingNode) -> VariantChoice {
    match node.kind {
        SettingKind::Object | SettingKind::Map => VariantChoice::Object,
        _ => VariantChoice::Simple,
    }
}

/// 选新增项的元素节点：`--variant` 缺省时取声明序第一个（variants 列表的"简单形态"在前）。
fn add_element_node(
    list: &SettingNode,
    choice: Option<VariantChoice>,
) -> Result<&SettingNode, String> {
    let candidates: Vec<&SettingNode> = match (list.element.as_deref(), list.variants.as_deref()) {
        (Some(element), _) => vec![element],
        (None, Some(variants)) if !variants.is_empty() => variants.iter().collect(),
        _ => {
            return Err(format!(
                "{} 没有声明元素形态（退化列表），无法新增",
                list.path
            ));
        }
    };
    match choice {
        None => Ok(candidates[0]),
        Some(wanted) => candidates
            .iter()
            .copied()
            .find(|node| variant_class(node) == wanted)
            .ok_or_else(|| {
                let available: Vec<&str> =
                    candidates.iter().map(|n| variant_class(n).word()).collect();
                format!(
                    "{} 没有 {} 形态（可用：{}）",
                    list.path,
                    wanted.word(),
                    available.join(" / ")
                )
            }),
    }
}

// ============================================================
// 错误映射
// ============================================================

/// API 错误 →（退出码，stderr 文案；文案不含结尾换行）。纯函数：调用方负责打印，
/// 单测因此可以直接断言（不需要捕获进程流）。
fn api_failure(error: &ApiClientError, http_base: &str, what: &str) -> (u8, String) {
    if is_unreachable(error) {
        return (
            2,
            format!("wing config error: 网关不可达（{http_base}）：先运行 wing start"),
        );
    }
    if error.is_conflict() {
        return (
            3,
            "wing config error: 配置已被其它客户端修改，请重试（或 --force 跳过指纹检查）"
                .to_string(),
        );
    }
    if let ApiClientError::Api { status: 404, .. } = error {
        return (
            1,
            format!(
                "wing config error: {what}：{error}\n  （该网关可能早于 Setting API；重启网关：wing stop && wing start）"
            ),
        );
    }
    (1, format!("wing config error: {what}：{error}"))
}

/// 传输层错误 = 连不上（`Connection` 是假后端 / tool host 用的同义变体）。
fn is_unreachable(error: &ApiClientError) -> bool {
    matches!(
        error,
        ApiClientError::Transport(_) | ApiClientError::Connection(_)
    )
}

/// `--json` 序列化（本模块输出全是自己的 struct，序列化不会失败）。
fn json_string<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use clap::Parser;
    use wing_api_client::models::{ApplyScope, ReloadResponse, ReloadResultItem, SettingChoice};

    // ------------------------------------------------------------
    // fixture：目录
    // ------------------------------------------------------------

    /// 一个节点（全部约束先取缺省；各 fixture 自行覆盖需要的字段）。
    fn node(key: &str, path: &str, kind: SettingKind) -> SettingNode {
        SettingNode {
            key: key.to_string(),
            path: path.to_string(),
            title: String::new(),
            doc: format!("{key} 的一行说明"),
            notes: Vec::new(),
            example: None,
            order: 0,
            kind,
            required: false,
            nullable: false,
            default: None,
            has_default: false,
            min: None,
            max: None,
            exclusive_min: false,
            exclusive_max: false,
            min_length: None,
            pattern: None,
            choices: Vec::new(),
            min_items: None,
            max_items: None,
            secret: false,
            apply: ApplyScope::Hot,
            editable: true,
            deprecated: None,
            section: None,
            section_doc: None,
            children: Vec::new(),
            element: None,
            variants: None,
            summary_fields: Vec::new(),
            value_hint: None,
        }
    }

    fn choice(value: &str, doc: &str) -> SettingChoice {
        SettingChoice {
            value: value.to_string(),
            doc: Some(doc.to_string()),
        }
    }

    /// 覆盖关键形态的目录：对象 / 嵌套对象 / 列表（单一元素）/ 列表（union）/ map /
    /// secret / enum / 声明默认值 / section 分组 / restart 生效域。
    fn catalog() -> SettingNode {
        // providers[].*
        let mut name = node("name", "providers[].name", SettingKind::Str);
        name.required = true;
        name.pattern = Some("^[a-zA-Z0-9_-]+$".to_string());
        let mut protocol = node("protocol", "providers[].protocol", SettingKind::Enum);
        protocol.choices = vec![
            choice("openai", "OpenAI 兼容"),
            choice("anthropic", "Anthropic"),
        ];
        protocol.default = Some(json!("openai"));
        protocol.has_default = true;
        let mut base_url = node("base_url", "providers[].base_url", SettingKind::Str);
        base_url.required = true;
        let mut api_key = node("api_key", "providers[].api_key", SettingKind::Secret);
        api_key.required = true;
        api_key.secret = true;
        let model_id = node("id", "providers[].models[].id", SettingKind::Str);
        let mut model_spec = node("[]", "providers[].models[]", SettingKind::Object);
        model_spec.children = vec![model_id];
        let model_str = node("[]", "providers[].models[]", SettingKind::Str);
        let mut models = node("models", "providers[].models", SettingKind::List);
        models.variants = Some(vec![model_str, model_spec]);
        let extra_body = node("extra_body", "providers[].extra_body", SettingKind::Map);
        let mut provider = node("[]", "providers[]", SettingKind::Object);
        provider.children = vec![name, protocol, base_url, api_key, models, extra_body];
        let mut providers = node("providers", "providers", SettingKind::List);
        providers.element = Some(Box::new(provider));
        providers.min_items = Some(1);
        providers.section = Some("Providers".to_string());

        // agents[].*
        let mut agent_name = node("name", "agents[].name", SettingKind::Str);
        agent_name.required = true;
        let mut agent_model = node("model", "agents[].model", SettingKind::Str);
        agent_model.required = true;
        let mut agent = node("[]", "agents[]", SettingKind::Object);
        agent.children = vec![agent_name, agent_model];
        let mut agents = node("agents", "agents", SettingKind::List);
        agents.element = Some(Box::new(agent));
        agents.min_items = Some(1);
        agents.section = Some("Agents".to_string());

        // gateway.*（restart 生效域 + 嵌套对象）
        let mut host = node("host", "gateway.host", SettingKind::Str);
        host.default = Some(json!("127.0.0.1"));
        host.has_default = true;
        let mut port = node("port", "gateway.port", SettingKind::Int);
        port.default = Some(json!(32523));
        port.has_default = true;
        port.min = Some(1.0);
        let mut auth_enabled = node("enabled", "gateway.auth.enabled", SettingKind::Bool);
        auth_enabled.default = Some(json!(false));
        auth_enabled.has_default = true;
        let mut auth = node("auth", "gateway.auth", SettingKind::Object);
        auth.children = vec![auth_enabled];
        let mut gateway = node("gateway", "gateway", SettingKind::Object);
        gateway.children = vec![host, port, auth];
        gateway.section = Some("Gateway".to_string());
        gateway.apply = ApplyScope::Restart;

        // images.max_bytes
        let mut max_bytes = node("max_bytes", "images.max_bytes", SettingKind::Int);
        max_bytes.default = Some(json!(1048576));
        max_bytes.has_default = true;
        max_bytes.min = Some(1.0);
        max_bytes.section = Some("Images".to_string());
        let mut images = node("images", "images", SettingKind::Object);
        images.children = vec![max_bytes];
        images.section = Some("Images".to_string());

        // tools（标量列表 + 声明默认值：add 的物化路径）
        let mut tools = node("tools", "tools", SettingKind::List);
        tools.element = Some(Box::new(node("[]", "tools[]", SettingKind::Str)));
        tools.default = Some(json!(["bash"]));
        tools.has_default = true;

        // log.level（enum 默认值）
        let mut level = node("level", "log.level", SettingKind::Enum);
        level.choices = vec![choice("info", "常规"), choice("debug", "调试")];
        level.default = Some(json!("info"));
        level.has_default = true;
        let mut log = node("log", "log", SettingKind::Object);
        log.children = vec![level];

        let mut root = node("config", "config", SettingKind::Object);
        root.children = vec![providers, agents, gateway, images, tools, log];
        root
    }

    /// 一份合法文档（与 [`catalog`] 对齐的最小形态）。
    fn document() -> Value {
        json!({
            "providers": [
                {
                    "name": "qoder",
                    "protocol": "openai",
                    "base_url": "https://example.com/v1",
                    "api_key": null,
                    "models": ["ds-flash", {"id": "ds-pro"}]
                }
            ],
            "agents": [{"name": "default", "model": "ds-flash"}],
            "gateway": {"port": 32523},
            "tools": ["bash"]
        })
    }

    fn get_response(values: Value) -> SettingsGetResponse {
        SettingsGetResponse {
            values,
            secrets: HashMap::new(),
            fingerprint: "fp-1".to_string(),
            problems: Vec::new(),
            setup_mode: false,
            config_path: "/tmp/wing/config.yaml".to_string(),
        }
    }

    fn ok_set_response() -> SettingsSetResponse {
        SettingsSetResponse {
            ok: true,
            fingerprint: "fp-2".to_string(),
            problems: Vec::new(),
            changed: vec!["gateway.port".to_string()],
            restart_required: Vec::new(),
            reload: None,
            setup_mode_exited: false,
            backup_path: Some("/tmp/wing/config.yaml.bak".to_string()),
        }
    }

    fn problem(
        path: Option<&str>,
        kind: &str,
        message: &str,
        hint: Option<&str>,
    ) -> SettingProblem {
        SettingProblem {
            path: path.map(str::to_string),
            kind: kind.to_string(),
            message: message.to_string(),
            hint: hint.map(str::to_string),
        }
    }

    // ------------------------------------------------------------
    // 假后端（可注入请求层）
    // ------------------------------------------------------------

    /// 假后端能制造的错误（`ApiClientError` 不可 Clone，所以存构造参数）。
    #[derive(Debug, Clone, Copy)]
    enum FakeError {
        Unreachable,
        Conflict,
        NotFound,
        Server,
    }

    impl FakeError {
        fn to_api_error(self) -> ApiClientError {
            match self {
                FakeError::Unreachable => {
                    ApiClientError::Connection("connection refused".to_string())
                }
                FakeError::Conflict => ApiClientError::Api {
                    status: 409,
                    detail: "fingerprint mismatch".to_string(),
                    body: None,
                },
                FakeError::NotFound => ApiClientError::Api {
                    status: 404,
                    detail: "Not Found".to_string(),
                    body: None,
                },
                FakeError::Server => ApiClientError::Api {
                    status: 500,
                    detail: "boom".to_string(),
                    body: None,
                },
            }
        }
    }

    /// 四路应答可独立指定的假后端；`set` 请求全部被捕获供断言。
    struct FakeBackend {
        schema: Result<SettingsSchemaResponse, FakeError>,
        get: Result<SettingsGetResponse, FakeError>,
        status: Result<SettingsStatusResponse, FakeError>,
        set: Result<SettingsSetResponse, FakeError>,
        captured: Mutex<Vec<SettingsSetRequest>>,
    }

    impl FakeBackend {
        fn healthy(root: SettingNode, values: Value) -> Self {
            Self {
                schema: Ok(SettingsSchemaResponse {
                    version: "0.0.0".to_string(),
                    root,
                    config_path: "/tmp/wing/config.yaml".to_string(),
                }),
                get: Ok(get_response(values)),
                status: Ok(SettingsStatusResponse {
                    valid: true,
                    setup_mode: false,
                    problems: Vec::new(),
                    fingerprint: Some("fp-1".to_string()),
                }),
                set: Ok(ok_set_response()),
                captured: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<SettingsSetRequest> {
            self.captured.lock().unwrap().clone()
        }

        fn last_document(&self) -> Value {
            let requests = self.requests();
            requests
                .last()
                .expect("expected one settings_set call")
                .document
                .clone()
        }
    }

    impl SettingsBackend for FakeBackend {
        async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError> {
            self.schema.clone().map_err(FakeError::to_api_error)
        }

        async fn get(&self) -> Result<SettingsGetResponse, ApiClientError> {
            self.get.clone().map_err(FakeError::to_api_error)
        }

        async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError> {
            self.status.clone().map_err(FakeError::to_api_error)
        }

        async fn set(
            &self,
            req: &SettingsSetRequest,
        ) -> Result<SettingsSetResponse, ApiClientError> {
            self.captured.lock().unwrap().push(req.clone());
            self.set.clone().map_err(FakeError::to_api_error)
        }
    }

    /// 跑一条命令（测试入口）：返回（退出码, stdout, stderr）三元组。
    async fn run_cmd(
        backend: &FakeBackend,
        command: ConfigCommand,
        json: bool,
    ) -> (u8, String, String) {
        let outcome = execute(command, json, backend, "http://127.0.0.1:32523").await;
        (
            outcome.code,
            outcome.stdout.unwrap_or_default(),
            outcome.stderr.unwrap_or_default(),
        )
    }

    fn steps(path: &str) -> Vec<PathStep> {
        parse_path(path).expect("fixture path must parse")
    }

    // ------------------------------------------------------------
    // path
    // ------------------------------------------------------------

    #[test]
    fn path_command_is_local_and_renders_both_modes() {
        let text = execute_path(false);
        assert_eq!(text.code, 0);
        let rendered = text.stdout.expect("stdout");
        assert!(rendered.ends_with('\n'), "{rendered}");
        assert!(
            rendered.trim_end().ends_with("core/config.yaml"),
            "本地推导必须指向 $WING_HOME/core/config.yaml，got {rendered}"
        );

        let json_out = execute_path(true);
        let parsed: Value =
            serde_json::from_str(json_out.stdout.as_deref().expect("stdout").trim()).expect("json");
        assert_eq!(
            parsed["config_path"].as_str().expect("config_path"),
            rendered.trim_end()
        );
    }

    // ------------------------------------------------------------
    // 值强制转换表（design.md D4 全表）
    // ------------------------------------------------------------

    fn scalar(kind: SettingKind) -> SettingNode {
        node("x", "x", kind)
    }

    #[test]
    fn coercion_bool_accepts_the_documented_spellings() {
        let target = scalar(SettingKind::Bool);
        for raw in ["true", "TRUE", "1", "yes", "On"] {
            assert_eq!(coerce_value(&target, raw).unwrap(), json!(true), "{raw}");
        }
        for raw in ["false", "False", "0", "no", "off"] {
            assert_eq!(coerce_value(&target, raw).unwrap(), json!(false), "{raw}");
        }
        assert_eq!(
            coerce_value(&target, "maybe").unwrap_err(),
            "需要一个布尔值（true/false）"
        );
    }

    #[test]
    fn coercion_int_parses_and_rejects() {
        let target = scalar(SettingKind::Int);
        assert_eq!(coerce_value(&target, " 300 ").unwrap(), json!(300));
        assert_eq!(coerce_value(&target, "-7").unwrap(), json!(-7));
        assert_eq!(coerce_value(&target, "3.5").unwrap_err(), "需要一个整数");
        assert_eq!(coerce_value(&target, "abc").unwrap_err(), "需要一个整数");
    }

    #[test]
    fn coercion_float_parses_integer_text_as_float() {
        let target = scalar(SettingKind::Float);
        // "300" → 300.0（不是整数 300）：否则写进 float 字段的是 string-or-int 形状。
        assert_eq!(coerce_value(&target, "300").unwrap(), json!(300.0));
        assert_ne!(coerce_value(&target, "300").unwrap(), json!(300));
        assert_eq!(coerce_value(&target, "0.5").unwrap(), json!(0.5));
        assert_eq!(coerce_value(&target, "1e3").unwrap(), json!(1000.0));
        assert_eq!(coerce_value(&target, "NaN").unwrap_err(), "需要一个数字");
        assert_eq!(coerce_value(&target, "inf").unwrap_err(), "需要一个数字");
        assert_eq!(coerce_value(&target, "x").unwrap_err(), "需要一个数字");
    }

    #[test]
    fn coercion_str_and_secret_pass_through_trimmed() {
        let text = scalar(SettingKind::Str);
        assert_eq!(coerce_value(&text, "  hello  ").unwrap(), json!("hello"));
        let mut secret = scalar(SettingKind::Secret);
        secret.secret = true;
        assert_eq!(coerce_value(&secret, " sk-1 ").unwrap(), json!("sk-1"));
    }

    #[test]
    fn coercion_enum_requires_a_declared_choice() {
        let mut target = scalar(SettingKind::Enum);
        target.choices = vec![choice("low", "低"), choice("high", "高")];
        assert_eq!(coerce_value(&target, "low").unwrap(), json!("low"));
        assert_eq!(
            coerce_value(&target, "LOW").unwrap_err(),
            "可选值：low / high"
        );
    }

    #[test]
    fn coercion_containers_accept_json_of_matching_shape() {
        let map = scalar(SettingKind::Map);
        assert_eq!(
            coerce_value(&map, r#"{"thinking":{"type":"enabled"}}"#).unwrap(),
            json!({"thinking": {"type": "enabled"}})
        );
        assert_eq!(coerce_value(&map, "[1]").unwrap_err(), "需要一个 JSON 对象");
        assert!(
            coerce_value(&map, "{oops")
                .unwrap_err()
                .starts_with("不是合法 JSON")
        );

        let object = scalar(SettingKind::Object);
        assert_eq!(coerce_value(&object, "{}").unwrap(), json!({}));

        let list = scalar(SettingKind::List);
        assert_eq!(
            coerce_value(&list, r#"["a","b"]"#).unwrap(),
            json!(["a", "b"])
        );
        assert_eq!(coerce_value(&list, "{}").unwrap_err(), "需要一个 JSON 数组");
    }

    #[test]
    fn coercion_nullable_accepts_null_tilde_and_empty_string() {
        let mut nullable = scalar(SettingKind::Str);
        nullable.nullable = true;
        for raw in ["null", "~", "", "  "] {
            assert_eq!(
                coerce_value(&nullable, raw).unwrap(),
                Value::Null,
                "{raw:?}"
            );
        }
        // 空串要有办法表达"空字符串本身"：--json-value '""'。
        assert_eq!(
            parse_json(r#""""#).unwrap(),
            json!(""),
            "逃生舱必须能表达空字符串"
        );
    }

    #[test]
    fn coercion_null_is_not_special_on_non_nullable_fields() {
        let text = scalar(SettingKind::Str);
        assert_eq!(coerce_value(&text, "null").unwrap(), json!("null"));
        let number = scalar(SettingKind::Int);
        assert_eq!(coerce_value(&number, "null").unwrap_err(), "需要一个整数");
    }

    #[test]
    fn coercion_unknown_kind_points_at_json_value() {
        let unknown = scalar(SettingKind::Unknown("future".to_string()));
        let message = coerce_value(&unknown, "x").unwrap_err();
        assert!(message.contains("--json-value"), "{message}");
    }

    #[test]
    fn parse_json_reports_syntax_errors_without_echoing_content() {
        let message = parse_json("{nope}").unwrap_err();
        assert!(message.starts_with("不是合法 JSON"), "{message}");
        assert!(
            !message.contains("nope"),
            "解析错误不该回显原始内容：{message}"
        );
    }

    // ------------------------------------------------------------
    // 路径与目录寻址
    // ------------------------------------------------------------

    #[test]
    fn format_path_renders_canonical_paths() {
        assert_eq!(
            format_path(&steps("providers[0].models[2].id")),
            "providers[0].models[2].id"
        );
        assert_eq!(format_path(&steps("providers[]")), "providers[]");
        assert_eq!(format_path(&[]), "");
    }

    #[test]
    fn resolve_node_finds_concrete_and_template_paths() {
        let root = catalog();
        let doc = document();
        let concrete = resolve_node(&root, &doc, &steps("providers[0].api_key")).expect("concrete");
        assert_eq!(concrete.path, "providers[].api_key");
        assert_eq!(concrete.kind, SettingKind::Secret);
        // 模板路径（[]）在 catalog 上永远可寻址（读命令用来学约束）。
        let template = resolve_node(&root, &doc, &steps("providers[].api_key")).expect("template");
        assert_eq!(template.path, "providers[].api_key");
        assert!(resolve_node(&root, &doc, &steps("providers[0].nope")).is_none());
    }

    #[test]
    fn resolve_node_selects_union_variant_by_document_value() {
        let root = catalog();
        let doc = document();
        // 裸字符串元素 → str 形态；对象元素 → object 形态。
        let simple = resolve_node(&root, &doc, &steps("providers[0].models[0]")).expect("simple");
        assert_eq!(simple.kind, SettingKind::Str);
        let rich = resolve_node(&root, &doc, &steps("providers[0].models[1]")).expect("object");
        assert_eq!(rich.kind, SettingKind::Object);
        let rich_child =
            resolve_node(&root, &doc, &steps("providers[0].models[1].id")).expect("id");
        assert_eq!(rich_child.path, "providers[].models[].id");
        // 文档里没有的元素：union 形态无法判定（不猜）。
        assert!(resolve_node(&root, &doc, &steps("providers[0].models[9]")).is_none());
    }

    #[test]
    fn resolve_node_honours_the_optional_root_prefix() {
        let root = catalog();
        let doc = document();
        let plain = resolve_node(&root, &doc, &steps("gateway.port")).expect("plain");
        let prefixed = resolve_node(&root, &doc, &steps("config.gateway.port")).expect("prefixed");
        assert_eq!(plain.path, prefixed.path);
    }

    // ------------------------------------------------------------
    // 文档编辑
    // ------------------------------------------------------------

    #[test]
    fn apply_set_creates_missing_intermediate_objects() {
        let mut doc = json!({});
        apply_set(&mut doc, &steps("gateway.auth.enabled"), json!(true)).unwrap();
        assert_eq!(doc, json!({"gateway": {"auth": {"enabled": true}}}));
        // null 中间层同样被物化成对象（nullable object 字段）。
        let mut doc = json!({"gateway": {"auth": null}});
        apply_set(&mut doc, &steps("gateway.auth.enabled"), json!(true)).unwrap();
        assert_eq!(doc, json!({"gateway": {"auth": {"enabled": true}}}));
    }

    #[test]
    fn apply_set_rejects_scalar_intermediate_and_out_of_range_index() {
        let mut doc = json!({"gateway": {"port": 8080}});
        let message = apply_set(&mut doc, &steps("gateway.port.sub"), json!(1)).unwrap_err();
        assert!(message.contains("不是对象"), "{message}");
        let message = apply_set(&mut doc, &steps("gateway.port[0]"), json!(1)).unwrap_err();
        assert!(message.contains("不是列表"), "{message}");

        let mut doc = document();
        let message = apply_set(&mut doc, &steps("providers[9].name"), json!("x")).unwrap_err();
        assert!(message.contains("列表下标越界"), "{message}");
        assert!(
            message.contains("列表下标越界：providers 只有 1 项"),
            "{message}"
        );
    }

    #[test]
    fn apply_unset_removes_and_reports_absent() {
        let mut doc = document();
        assert!(apply_unset(&mut doc, &steps("gateway.port")).unwrap());
        assert!(doc["gateway"].get("port").is_none());
        // 幂等：第二次就是"缺席"。
        assert!(!apply_unset(&mut doc, &steps("gateway.port")).unwrap());
        assert!(!apply_unset(&mut doc, &steps("nothing.here")).unwrap());
        // 整个列表元素也可移除。
        assert!(apply_unset(&mut doc, &steps("providers[0]")).unwrap());
        assert_eq!(doc["providers"], json!([]));
    }

    #[test]
    fn apply_remove_reports_bounds_with_length() {
        let mut doc = document();
        let removed = apply_remove(&mut doc, &steps("tools[0]")).unwrap();
        assert_eq!(removed, json!("bash"));
        assert_eq!(doc["tools"], json!([]));
        let message = apply_remove(&mut doc, &steps("tools[5]")).unwrap_err();
        assert!(message.contains("列表下标越界"), "{message}");
        assert!(message.contains("tools 只有 0 项"), "{message}");
    }

    #[test]
    fn apply_move_swaps_in_both_directions() {
        let mut doc = json!({"tools": ["a", "b", "c"]});
        let outcome = apply_move(&mut doc, &steps("tools[2]"), -1).unwrap();
        assert_eq!(
            outcome,
            MoveOutcome {
                moved: true,
                from: 2,
                to: 1,
                length: 3
            }
        );
        assert_eq!(doc["tools"], json!(["a", "c", "b"]));
        let outcome = apply_move(&mut doc, &steps("tools[0]"), 2).unwrap();
        assert_eq!(outcome.to, 2);
        assert_eq!(doc["tools"], json!(["c", "b", "a"]));
    }

    #[test]
    fn apply_move_clamps_at_edges_and_tolerates_i64_extremes() {
        let mut doc = json!({"tools": ["a", "b"]});
        // 边界钳制：向上越界 = 原地不动（moved=false 由调用方短路）。
        let outcome = apply_move(&mut doc, &steps("tools[0]"), -5).unwrap();
        assert!(!outcome.moved);
        assert_eq!(doc["tools"], json!(["a", "b"]));
        // 向下越界钳到末位。
        let outcome = apply_move(&mut doc, &steps("tools[0]"), i64::MAX).unwrap();
        assert_eq!(outcome.to, 1);
        assert_eq!(doc["tools"], json!(["b", "a"]));
        // 负向极值也不能溢出。
        let outcome = apply_move(&mut doc, &steps("tools[1]"), i64::MIN).unwrap();
        assert_eq!(outcome.to, 0);
        // delta = 0 → 无位移。
        let outcome = apply_move(&mut doc, &steps("tools[1]"), 0).unwrap();
        assert!(!outcome.moved);
    }

    #[test]
    fn apply_add_appends_and_materializes_declared_defaults() {
        let root = catalog();
        let providers = resolve_node(&root, &document(), &steps("providers")).unwrap();

        // 标量列表：文档里有 → 直接追加。
        let mut doc = document();
        let index = apply_add(&mut doc, &steps("tools"), providers, json!("read")).unwrap();
        assert_eq!(index, 1);
        assert_eq!(doc["tools"], json!(["bash", "read"]));

        // 列表缺席 → 用声明默认值物化后追加（否则 "add 到默认值" 无处可落）。
        let mut doc = json!({});
        let tools_node = resolve_node(&root, &doc, &steps("tools")).unwrap();
        let index = apply_add(&mut doc, &steps("tools"), tools_node, json!("read")).unwrap();
        assert_eq!(index, 1);
        assert_eq!(doc["tools"], json!(["bash", "read"]));

        // 没有默认值且缺席 → 从空列表开始。
        let mut doc = json!({});
        let agents_node = resolve_node(&root, &doc, &steps("agents")).unwrap();
        let index = apply_add(
            &mut doc,
            &steps("agents"),
            agents_node,
            json!({"name": "a"}),
        )
        .unwrap();
        assert_eq!(index, 0);
        assert_eq!(doc["agents"], json!([{"name": "a"}]));

        // 文档里的值不是数组 → 用法错误。
        let mut doc = json!({"tools": "oops"});
        let message = apply_add(&mut doc, &steps("tools"), tools_node, json!("x")).unwrap_err();
        assert!(message.contains("不是列表"), "{message}");
    }

    // ------------------------------------------------------------
    // plan_write
    // ------------------------------------------------------------

    fn plan(op: &WriteOp) -> Result<Plan, String> {
        plan_write(op, &catalog(), &document())
    }

    fn saved_document(plan: Result<Plan, String>) -> Value {
        match plan.expect("plan must succeed") {
            Plan::Save { document, .. } => document,
            Plan::Noop { message, .. } => panic!("expected a save, got noop: {message}"),
        }
    }

    #[test]
    fn plan_set_coerces_by_catalog_kind() {
        let doc = saved_document(plan(&WriteOp::Set {
            path: "images.max_bytes".to_string(),
            value: Some("1000000".to_string()),
            json_value: None,
        }));
        assert_eq!(doc["images"]["max_bytes"], json!(1000000));

        let doc = saved_document(plan(&WriteOp::Set {
            path: "providers[0].models[0]".to_string(),
            value: Some("ds-pro".to_string()),
            json_value: None,
        }));
        assert_eq!(doc["providers"][0]["models"][0], json!("ds-pro"));
    }

    #[test]
    fn plan_set_replaces_a_union_element_in_the_new_values_shape() {
        // 对象形态 → 简单形态：裸字符串就地替换（不用 --json-value 也能表达）。
        let doc = saved_document(plan(&WriteOp::Set {
            path: "providers[0].models[1]".to_string(),
            value: Some("ds-pro-lite".to_string()),
            json_value: None,
        }));
        assert_eq!(doc["providers"][0]["models"][1], json!("ds-pro-lite"));

        // 简单形态 → 对象形态：JSON 对象就地替换。
        let doc = saved_document(plan(&WriteOp::Set {
            path: "providers[0].models[0]".to_string(),
            value: Some(r#"{"id":"ds-flash-2"}"#.to_string()),
            json_value: None,
        }));
        assert_eq!(
            doc["providers"][0]["models"][0],
            json!({"id": "ds-flash-2"})
        );

        // --json-value 同样按新值判形态。
        let doc = saved_document(plan(&WriteOp::Set {
            path: "providers[0].models[1]".to_string(),
            value: None,
            json_value: Some("\"ds-plain\"".to_string()),
        }));
        assert_eq!(doc["providers"][0]["models"][1], json!("ds-plain"));
    }

    #[test]
    fn plan_set_missing_value_is_usage_error() {
        let message = plan(&WriteOp::Set {
            path: "gateway.port".to_string(),
            value: None,
            json_value: None,
        })
        .unwrap_err();
        assert!(message.contains("缺少 <value>"), "{message}");
    }

    #[test]
    fn plan_set_json_value_bypasses_kind_checks() {
        // 故意给 int 字段一个字符串：本地不拦（后端才是校验器）。
        let doc = saved_document(plan(&WriteOp::Set {
            path: "gateway.port".to_string(),
            value: None,
            json_value: Some("\"not-a-number\"".to_string()),
        }));
        assert_eq!(doc["gateway"]["port"], json!("not-a-number"));
        // 非法 JSON → 用法错误。
        let message = plan(&WriteOp::Set {
            path: "gateway.port".to_string(),
            value: None,
            json_value: Some("{oops".to_string()),
        })
        .unwrap_err();
        assert!(message.starts_with("不是合法 JSON"), "{message}");
        // 位置参数与 --json-value 互斥。
        let message = plan(&WriteOp::Set {
            path: "gateway.port".to_string(),
            value: Some("1".to_string()),
            json_value: Some("2".to_string()),
        })
        .unwrap_err();
        assert!(message.contains("不能同时给出"), "{message}");
    }

    #[test]
    fn plan_write_rejects_template_empty_and_unknown_paths() {
        for path in ["providers[]", "providers[].api_key"] {
            let message = plan(&WriteOp::Unset {
                path: path.to_string(),
            })
            .unwrap_err();
            assert!(message.contains("模板路径"), "{path}: {message}");
        }
        let message = plan(&WriteOp::Unset {
            path: String::new(),
        })
        .unwrap_err();
        assert!(message.contains("路径不能为空"), "{message}");
        let message = plan(&WriteOp::Unset {
            path: "nope.nope".to_string(),
        })
        .unwrap_err();
        assert!(message.contains("路径不在设置目录中"), "{message}");
        // 自由 map 的内部不可寻址（catalog 只声明到 map 这一层）。
        let message = plan(&WriteOp::Unset {
            path: "providers[0].extra_body.thinking".to_string(),
        })
        .unwrap_err();
        assert!(message.contains("路径不在设置目录中"), "{message}");
    }

    #[test]
    fn plan_unset_absent_is_a_noop() {
        let outcome = plan(&WriteOp::Unset {
            path: "gateway.host".to_string(),
        })
        .expect("absent key is not an error");
        match outcome {
            Plan::Noop { reason, message } => {
                assert_eq!(reason, "already_default");
                assert!(message.contains("gateway.host"), "{message}");
            }
            Plan::Save { .. } => panic!("unsetting an absent key must not save"),
        }
    }

    #[test]
    fn plan_move_zero_delta_and_edge_are_noops() {
        for (path, delta, reason) in [
            ("providers[0]", 0, "delta_zero"),
            ("providers[0]", -1, "already_at_edge"),
        ] {
            match plan(&WriteOp::Move {
                path: path.to_string(),
                delta,
            })
            .expect("no-op is fine")
            {
                Plan::Noop {
                    reason: got,
                    message,
                } => {
                    assert_eq!(got, reason, "{message}");
                }
                Plan::Save { .. } => panic!("expected a noop for ({path}, {delta})"),
            }
        }
        // 有意义的一次移动会产出保存。
        let doc = saved_document(plan(&WriteOp::Move {
            path: "providers[0].models[1]".to_string(),
            delta: -1,
        }));
        assert_eq!(
            doc["providers"][0]["models"],
            json!([{"id": "ds-pro"}, "ds-flash"])
        );
    }

    #[test]
    fn plan_move_requires_an_explicit_index() {
        let message = plan(&WriteOp::Move {
            path: "providers[0].models".to_string(),
            delta: 1,
        })
        .unwrap_err();
        assert!(message.contains("必须以具体下标结尾"), "{message}");
    }

    #[test]
    fn plan_add_requires_a_value_for_scalar_elements() {
        let message = plan(&WriteOp::Add {
            path: "tools".to_string(),
            value: None,
            json_value: None,
            variant: None,
        })
        .unwrap_err();
        assert!(message.contains("必须给 <value>"), "{message}");

        let doc = saved_document(plan(&WriteOp::Add {
            path: "tools".to_string(),
            value: Some("read".to_string()),
            json_value: None,
            variant: None,
        }));
        assert_eq!(doc["tools"], json!(["bash", "read"]));
    }

    #[test]
    fn plan_add_object_stub_and_variant_choice() {
        // 对象列表（单一 element）：缺省插 {}。
        let doc = saved_document(plan(&WriteOp::Add {
            path: "agents".to_string(),
            value: None,
            json_value: None,
            variant: None,
        }));
        assert_eq!(doc["agents"][1], json!({}));

        // variants 列表：缺省 simple → 裸字符串，必须给值。
        let message = plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: None,
            json_value: None,
            variant: None,
        })
        .unwrap_err();
        assert!(message.contains("必须给 <value>"), "{message}");

        // --variant simple + 值 → 追加裸字符串。
        let doc = saved_document(plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: Some("ds-max".to_string()),
            json_value: None,
            variant: Some("simple".to_string()),
        }));
        assert_eq!(doc["providers"][0]["models"][2], json!("ds-max"));

        // --variant object → 插 {} 骨架。
        let doc = saved_document(plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: None,
            json_value: None,
            variant: Some("object".to_string()),
        }));
        assert_eq!(doc["providers"][0]["models"][2], json!({}));

        // 对象元素也能一次给全（JSON）。
        let doc = saved_document(plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: Some(r#"{"id":"ds-pro-2"}"#.to_string()),
            json_value: None,
            variant: Some("object".to_string()),
        }));
        assert_eq!(doc["providers"][0]["models"][2], json!({"id": "ds-pro-2"}));

        // 非法 variant 词 / 不适用 variant → 用法错误。
        let message = plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: Some("x".to_string()),
            json_value: None,
            variant: Some("rich".to_string()),
        })
        .unwrap_err();
        assert!(
            message.contains("--variant 只接受 simple|object"),
            "{message}"
        );
        // 没有该形态的列表（tools 只有 simple）→ 用法错误。
        let message = plan(&WriteOp::Add {
            path: "tools".to_string(),
            value: Some("x".to_string()),
            json_value: None,
            variant: Some("object".to_string()),
        })
        .unwrap_err();
        assert!(message.contains("没有 object 形态"), "{message}");

        let message = plan(&WriteOp::Add {
            path: "providers[0].protocol".to_string(),
            value: Some("x".to_string()),
            json_value: None,
            variant: None,
        })
        .unwrap_err();
        assert!(message.contains("不是列表"), "{message}");
    }

    #[test]
    fn plan_add_derives_the_new_item_path_in_the_note() {
        match plan(&WriteOp::Add {
            path: "providers[0].models".to_string(),
            value: Some("ds-max".to_string()),
            json_value: None,
            variant: Some("simple".to_string()),
        })
        .unwrap()
        {
            Plan::Save { note, .. } => {
                assert_eq!(note.as_deref(), Some("新增项：providers[0].models[2]"));
            }
            Plan::Noop { .. } => panic!("expected a save"),
        }
    }

    #[test]
    fn plan_remove_redacts_secrets_in_the_note() {
        // 对抗性输入：文档里的 api_key 是明文（后端本该掩码，这里验证纵深防御）。
        let mut values = document();
        values["providers"][0]["api_key"] = json!("sk-live-plaintext-0001");
        let plan = plan_write(
            &WriteOp::Remove {
                path: "providers[0]".to_string(),
            },
            &catalog(),
            &values,
        )
        .expect("remove plans");
        match plan {
            Plan::Save { note, .. } => {
                let note = note.expect("note");
                assert!(
                    !note.contains("sk-live-plaintext-0001"),
                    "摘要不能带出密文：{note}"
                );
                // 摘要会被截断，所以单独证明"脱敏真的发生"（而不是只有截断在挡）。
                let root = catalog();
                let element =
                    resolve_node(&root, &values, &steps("providers[0]")).expect("element");
                let redacted = redact_secrets(element, &values["providers"][0]);
                assert_eq!(redacted["api_key"], Value::Null);
                assert_eq!(redacted["name"], json!("qoder"));
            }
            Plan::Noop { .. } => panic!("expected a save"),
        }
    }

    // ------------------------------------------------------------
    // 渲染
    // ------------------------------------------------------------

    #[test]
    fn sorted_problems_orders_by_severity_then_path_and_unknown_last() {
        let problems = vec![
            problem(Some("b"), "unknown_key", "unknown", None),
            problem(Some("z"), "invalid_value", "bad", None),
            problem(None, "missing_required", "gone", None),
            problem(Some("a"), "invalid_value", "bad", None),
            problem(Some("c"), "weird_future_kind", "?", None),
        ];
        let sorted: Vec<&str> = sorted_problems(&problems)
            .into_iter()
            .map(|p| p.path.as_deref().unwrap_or("(doc)"))
            .collect();
        assert_eq!(sorted, vec!["(doc)", "a", "z", "b", "c"]);
    }

    #[test]
    fn render_doctor_renders_ok_and_problem_lines() {
        let ok = SettingsStatusResponse {
            valid: true,
            setup_mode: false,
            problems: Vec::new(),
            fingerprint: Some("fp-1".to_string()),
        };
        assert_eq!(
            render_doctor(&ok, &[], "/tmp/config.yaml"),
            "✓ 配置可用（/tmp/config.yaml）\n"
        );

        let status = SettingsStatusResponse {
            valid: false,
            setup_mode: true,
            problems: vec![
                problem(
                    Some("providers"),
                    "empty_list",
                    "providers 不得为空",
                    Some("新增一个 provider"),
                ),
                problem(None, "invalid_value", "文档无法解析", None),
            ],
            fingerprint: None,
        };
        let problems = sorted_problems(&status.problems);
        let rendered = render_doctor(&status, &problems, "/tmp/config.yaml");
        assert!(
            rendered.starts_with("✗ 配置不可用（/tmp/config.yaml）· 网关处于修复模式"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  providers: providers 不得为空"),
            "{rendered}"
        );
        assert!(rendered.contains("      ↳ 新增一个 provider"), "{rendered}");
        assert!(rendered.contains("  (文档): 文档无法解析"), "{rendered}");
        assert!(rendered.contains("共 2 个问题"), "{rendered}");
        assert!(
            rendered.contains("wing config set <path> <value>"),
            "{rendered}"
        );
    }

    #[test]
    fn render_list_text_shows_sections_tree_flags_and_problem_marker() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "providers[0].api_key".to_string(),
            SecretState {
                state: SecretPresence::Set,
                hint: Some("0001".to_string()),
            },
        );
        let problems = vec![problem(
            Some("providers[0].protocol"),
            "invalid_value",
            "protocol 不认识",
            None,
        )];
        let rows = build_rows(&catalog(), &document(), &secrets, &problems, None, false).unwrap();
        let text = render_list_text(&rows);

        assert!(text.contains("── Providers ──"), "{text}");
        assert!(text.contains("── Gateway ──"), "{text}");
        assert!(text.contains("providers  (list)"), "{text}");
        // ├─ / └─ 连接符 + 祖先竖线（深度优先顺序下的树形）。
        assert!(text.contains("│  └─ providers[0]  (object)"), "{text}");
        assert!(
            text.contains("│     ├─ providers[0].name = qoder"),
            "{text}"
        );
        assert!(
            text.contains("│     └─ providers[0].extra_body = (未设置)"),
            "{text}"
        );
        assert!(
            text.contains("│     ├─ providers[0].models  (list)"),
            "{text}"
        );
        assert!(text.contains("providers[0].name = qoder"), "{text}");
        assert!(
            text.contains("providers[0].api_key = •••••••• 0001"),
            "{text}"
        );
        assert!(text.contains("gateway.port = 32523  默认 32523"), "{text}");
        assert!(text.contains("! providers[0].protocol = openai"), "{text}");
        assert!(text.contains("[hot]"), "{text}");
        assert!(text.contains("[restart]"), "{text}");
        assert!(text.contains("已覆盖"), "{text}");
        // 列表元素逐个展开。
        assert!(text.contains("providers[0].models[0] = ds-flash"), "{text}");
        assert!(
            text.contains("providers[0].models[1].id = ds-pro"),
            "{text}"
        );
        assert!(text.contains("agents[0].name = default"), "{text}");
        assert!(text.contains("agents[0].model = ds-flash"), "{text}");
    }

    #[test]
    fn build_rows_only_overridden_keeps_document_paths_only() {
        let rows = build_rows(&catalog(), &document(), &HashMap::new(), &[], None, true).unwrap();
        let paths: Vec<&str> = rows.iter().map(|row| row.path.as_str()).collect();
        assert!(paths.contains(&"providers[0].base_url"), "{paths:?}");
        assert!(paths.contains(&"gateway.port"), "{paths:?}");
        assert!(
            !paths.contains(&"gateway.host"),
            "默认值路径必须隐藏：{paths:?}"
        );
        assert!(
            !paths.contains(&"images"),
            "无覆盖的分组必须隐藏：{paths:?}"
        );
    }

    #[test]
    fn build_rows_section_filter_matches_case_insensitively() {
        let rows = build_rows(
            &catalog(),
            &document(),
            &HashMap::new(),
            &[],
            Some("gateway"),
            false,
        )
        .unwrap();
        let paths: Vec<&str> = rows.iter().map(|row| row.path.as_str()).collect();
        assert!(
            paths.iter().all(|path| path.starts_with("gateway")),
            "{paths:?}"
        );
        assert!(paths.contains(&"gateway.auth.enabled"), "{paths:?}");

        let message = build_rows(
            &catalog(),
            &document(),
            &HashMap::new(),
            &[],
            Some("nope"),
            false,
        )
        .unwrap_err();
        assert!(message.contains("未知分组"), "{message}");
        assert!(
            message.contains("Providers"),
            "错误里要列出可选分组：{message}"
        );
    }

    #[test]
    fn render_list_text_says_so_when_nothing_matches() {
        assert_eq!(render_list_text(&[]), "（没有匹配的设置项）\n");
        // --only-overridden 在空文档上就是这一行。
        let rows = build_rows(&catalog(), &json!({}), &HashMap::new(), &[], None, true).unwrap();
        assert!(rows.is_empty());
        assert_eq!(render_list_text(&rows), "（没有匹配的设置项）\n");
    }

    #[test]
    fn list_json_rows_carry_snake_case_fields_and_null_secret_values() {
        let mut secrets = HashMap::new();
        secrets.insert(
            "providers[0].api_key".to_string(),
            SecretState {
                state: SecretPresence::Set,
                hint: Some("0001".to_string()),
            },
        );
        let mut values = document();
        values["providers"][0]["api_key"] = json!("sk-plain-0001");
        let rows = build_rows(&catalog(), &values, &secrets, &[], None, false).unwrap();
        let api_key_row = rows
            .iter()
            .find(|row| row.path == "providers[0].api_key")
            .expect("secret row");
        assert_eq!(api_key_row.value, None, "密文行不能带值");
        assert_eq!(api_key_row.default, None);
        assert_eq!(api_key_row.kind, "secret");
        assert_eq!(
            api_key_row.secret.as_ref().unwrap().hint.as_deref(),
            Some("0001")
        );
    }

    #[test]
    fn render_secret_covers_all_three_states() {
        let state = |presence| SecretState {
            state: presence,
            hint: Some("abcd".to_string()),
        };
        assert_eq!(render_secret(&state(SecretPresence::Set)), "•••••••• abcd");
        assert_eq!(render_secret(&state(SecretPresence::Empty)), "(空)");
        assert_eq!(render_secret(&state(SecretPresence::Absent)), "(未设置)");
        // 未知状态（更新的网关）也必须保守掩码。
        let unknown = render_secret(&state(SecretPresence::Unknown("future".to_string())));
        assert!(unknown.starts_with("••••••••"), "{unknown}");
        // 无 hint 的 set 状态。
        let no_hint = render_secret(&SecretState {
            state: SecretPresence::Set,
            hint: None,
        });
        assert_eq!(no_hint, "••••••••");
    }

    #[test]
    fn get_output_renders_effective_default_and_overridden_flag() {
        let root = catalog();
        let doc = json!({"gateway": {"port": 8080}});

        let node = resolve_node(&root, &doc, &steps("gateway.port")).unwrap();
        let output = GetOutput::new("gateway.port", node, Some(json!(8080)), None, None);
        assert!(output.overridden);
        assert_eq!(output.value, Some(json!(8080)));
        let text = output.render_text();
        assert!(text.starts_with("gateway.port = 8080\n"), "{text}");
        assert!(text.contains("默认 32523"), "{text}");
        assert!(text.contains("已覆盖"), "{text}");
        assert!(text.contains("约束：≥ 1"), "{text}");

        // 未覆盖 → 值回落默认。
        let node = resolve_node(&root, &doc, &steps("gateway.host")).unwrap();
        let output = GetOutput::new("gateway.host", node, None, None, None);
        assert!(!output.overridden);
        assert_eq!(output.value, Some(json!("127.0.0.1")));
        assert!(
            output.render_text().contains("gateway.host = 127.0.0.1"),
            "{}",
            output.render_text()
        );

        // 必填但没有默认值 → 未设置。
        let node = resolve_node(&root, &doc, &steps("providers[0].base_url")).unwrap();
        let output = GetOutput::new("providers[0].base_url", node, None, None, None);
        assert_eq!(output.value, None);
        let text = output.render_text();
        assert!(text.contains("(未设置)"), "{text}");
        assert!(text.contains("必填"), "{text}");
    }

    #[test]
    fn render_write_text_ok_includes_changed_restart_reload_and_backup() {
        let response = SettingsSetResponse {
            ok: true,
            fingerprint: "fp-2".to_string(),
            problems: Vec::new(),
            changed: vec![
                "gateway.port".to_string(),
                "providers[0].base_url".to_string(),
            ],
            restart_required: vec!["gateway.port".to_string()],
            reload: Some(ReloadResponse {
                ok: false,
                results: vec![
                    ReloadResultItem {
                        name: "config.yaml".to_string(),
                        ok: true,
                        detail: None,
                    },
                    ReloadResultItem {
                        name: "hooks".to_string(),
                        ok: false,
                        detail: Some("boom".to_string()),
                    },
                ],
            }),
            setup_mode_exited: true,
            backup_path: Some("/tmp/config.yaml.bak".to_string()),
        };
        let text = render_write_text(&response, Some("新增项：providers[1]"));
        assert!(text.contains("✓ 已保存（2 处变更）"), "{text}");
        assert!(text.contains("  · gateway.port"), "{text}");
        assert!(text.contains("新增项：providers[1]"), "{text}");
        assert!(text.contains("⚠ 需重启网关才生效：gateway.port"), "{text}");
        assert!(text.contains("热重载：config.yaml ✓ · hooks ✗"), "{text}");
        assert!(text.contains("  ✗ hooks：boom"), "{text}");
        assert!(text.contains("⚠ 热重载未全部成功"), "{text}");
        assert!(text.contains("✓ 网关已转入正常模式"), "{text}");
        assert!(text.contains("备份：/tmp/config.yaml.bak"), "{text}");
    }

    #[test]
    fn render_write_text_refused_lists_problems_and_never_writes() {
        let response = SettingsSetResponse {
            ok: false,
            fingerprint: "fp-1".to_string(),
            problems: vec![problem(
                Some("providers[0].models"),
                "empty_list",
                "至少声明一个模型",
                Some("加一个模型"),
            )],
            changed: Vec::new(),
            restart_required: Vec::new(),
            reload: None,
            setup_mode_exited: false,
            backup_path: None,
        };
        let text = render_write_text(&response, None);
        assert!(text.contains("✗ 保存被拒绝"), "{text}");
        assert!(
            text.contains("providers[0].models: 至少声明一个模型"),
            "{text}"
        );
        assert!(text.contains("↳ 加一个模型"), "{text}");
        assert!(text.contains("共 1 个问题"), "{text}");
    }

    // ------------------------------------------------------------
    // 执行层（假后端）：退出码 / 请求体 / 无操作短路 / 密文不回显
    // ------------------------------------------------------------

    #[tokio::test]
    async fn doctor_exit_codes_and_json_shape() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, stdout, stderr) = run_cmd(&backend, ConfigCommand::Doctor, false).await;
        assert_eq!(code, 0);
        assert!(stdout.starts_with("✓ 配置可用（"), "{stdout}");
        assert!(stderr.is_empty());

        let (code, stdout, _) = run_cmd(&backend, ConfigCommand::Doctor, true).await;
        assert_eq!(code, 0);
        let parsed: Value = serde_json::from_str(stdout.trim()).expect("doctor --json parses");
        assert_eq!(parsed["valid"], json!(true));
        assert_eq!(parsed["setup_mode"], json!(false));
        assert!(
            parsed["config_path"]
                .as_str()
                .unwrap()
                .ends_with("core/config.yaml")
        );

        // invalid → 1（setup mode 时文案带修复模式）。
        let mut invalid = FakeBackend::healthy(catalog(), document());
        invalid.status = Ok(SettingsStatusResponse {
            valid: false,
            setup_mode: true,
            problems: vec![problem(
                Some("providers"),
                "empty_list",
                "providers 不得为空",
                None,
            )],
            fingerprint: None,
        });
        let (code, stdout, _) = run_cmd(&invalid, ConfigCommand::Doctor, false).await;
        assert_eq!(code, 1);
        assert!(stdout.contains("修复模式"), "{stdout}");
        assert!(stdout.contains("providers 不得为空"), "{stdout}");

        // 网关不可达 → 2 + 可操作提示。
        let mut down = FakeBackend::healthy(catalog(), document());
        down.status = Err(FakeError::Unreachable);
        let (code, stdout, stderr) = run_cmd(&down, ConfigCommand::Doctor, false).await;
        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert!(stderr.contains("网关不可达"), "{stderr}");
        assert!(stderr.contains("wing start"), "{stderr}");

        // 旧网关（404）→ 1 + "早于 Setting API" 提示。
        let mut old = FakeBackend::healthy(catalog(), document());
        old.status = Err(FakeError::NotFound);
        let (code, _, stderr) = run_cmd(&old, ConfigCommand::Doctor, false).await;
        assert_eq!(code, 1);
        assert!(stderr.contains("早于 Setting API"), "{stderr}");
    }

    #[tokio::test]
    async fn list_renders_tree_and_json_round_trips() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::List {
                section: None,
                only_overridden: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert!(stdout.contains("── Providers ──"), "{stdout}");

        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::List {
                section: None,
                only_overridden: false,
            },
            true,
        )
        .await;
        assert_eq!(code, 0);
        let parsed: Value = serde_json::from_str(stdout.trim()).expect("list --json parses");
        assert_eq!(parsed["fingerprint"], json!("fp-1"));
        assert_eq!(parsed["setup_mode"], json!(false));
        assert!(
            parsed["problems"].is_array(),
            "顶层问题清单必须存在：{parsed}"
        );
        let rows = parsed["rows"].as_array().expect("rows");
        assert!(rows.iter().any(|row| row["path"] == json!("gateway.port")));
        assert_eq!(backend.requests().len(), 0, "读命令不允许写");

        // 未知分组 → 用法错误 4。
        let (code, _, stderr) = run_cmd(
            &backend,
            ConfigCommand::List {
                section: Some("nope".to_string()),
                only_overridden: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 4);
        assert!(stderr.contains("未知分组"), "{stderr}");
    }

    #[tokio::test]
    async fn get_resolves_unknown_and_malformed_paths_as_usage_errors() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Get {
                path: "images.max_bytes".to_string(),
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert!(stdout.contains("images.max_bytes = 1048576"), "{stdout}");

        let (code, _, stderr) = run_cmd(
            &backend,
            ConfigCommand::Get {
                path: "images[".to_string(),
            },
            false,
        )
        .await;
        assert_eq!(code, 4);
        assert!(stderr.contains("路径文法非法"), "{stderr}");

        let (code, _, stderr) = run_cmd(
            &backend,
            ConfigCommand::Get {
                path: "nope.nope".to_string(),
            },
            false,
        )
        .await;
        assert_eq!(code, 4);
        assert!(stderr.contains("路径不在设置目录中"), "{stderr}");
    }

    #[tokio::test]
    async fn set_posts_the_mutated_document_with_the_fingerprint() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, stdout, stderr) = run_cmd(
            &backend,
            ConfigCommand::Set {
                path: "images.max_bytes".to_string(),
                value: Some("1000000".to_string()),
                json_value: None,
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
        assert!(stdout.contains("✓ 已保存"), "{stdout}");

        let requests = backend.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].base.as_deref(), Some("fp-1"), "默认带指纹");
        assert_eq!(requests[0].document["images"]["max_bytes"], json!(1000000));
        // 原样回传的密文 null 必须还在（丢键 = 清空密钥）。
        assert_eq!(requests[0].document["providers"][0]["api_key"], Value::Null);
    }

    #[tokio::test]
    async fn set_force_sends_a_null_base() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, _, _) = run_cmd(
            &backend,
            ConfigCommand::Set {
                path: "gateway.port".to_string(),
                value: Some("8080".to_string()),
                json_value: None,
                force: true,
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        let requests = backend.requests();
        assert_eq!(
            requests[0].base, None,
            "--force = base:null（跳过指纹检查）"
        );
    }

    #[tokio::test]
    async fn write_failures_map_to_the_documented_exit_codes() {
        // 409 → 3。
        let mut conflict = FakeBackend::healthy(catalog(), document());
        conflict.set = Err(FakeError::Conflict);
        let (code, _, stderr) = run_cmd(
            &conflict,
            ConfigCommand::Set {
                path: "gateway.port".to_string(),
                value: Some("8080".to_string()),
                json_value: None,
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 3);
        assert!(stderr.contains("配置已被其它客户端修改"), "{stderr}");

        // 连不上 → 2。
        let mut down = FakeBackend::healthy(catalog(), document());
        down.set = Err(FakeError::Unreachable);
        let (code, _, stderr) = run_cmd(
            &down,
            ConfigCommand::Set {
                path: "gateway.port".to_string(),
                value: Some("8080".to_string()),
                json_value: None,
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 2);
        assert!(stderr.contains("wing start"), "{stderr}");

        // 500 → 1（原文）。
        let mut broken = FakeBackend::healthy(catalog(), document());
        broken.set = Err(FakeError::Server);
        let (code, _, stderr) = run_cmd(
            &broken,
            ConfigCommand::Unset {
                path: "gateway.port".to_string(),
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 1);
        assert!(stderr.contains("boom"), "{stderr}");

        // ok=false → 1，且回执（problems）走 stdout。
        let mut refused = FakeBackend::healthy(catalog(), document());
        refused.set = Ok(SettingsSetResponse {
            ok: false,
            fingerprint: "fp-1".to_string(),
            problems: vec![problem(
                Some("images.max_bytes"),
                "invalid_value",
                "必须 ≥ 1",
                None,
            )],
            changed: Vec::new(),
            restart_required: Vec::new(),
            reload: None,
            setup_mode_exited: false,
            backup_path: None,
        });
        let (code, stdout, stderr) = run_cmd(
            &refused,
            ConfigCommand::Set {
                path: "images.max_bytes".to_string(),
                value: Some("0".to_string()),
                json_value: None,
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 1);
        assert!(stdout.contains("✗ 保存被拒绝"), "{stdout}");
        assert!(stdout.contains("images.max_bytes: 必须 ≥ 1"), "{stdout}");
        assert!(stderr.is_empty(), "{stderr}");
    }

    #[tokio::test]
    async fn usage_errors_do_not_need_a_reachable_gateway() {
        // 网关整个不可达：语法 / 互斥 / --variant / --json-value 仍报 4（不是 2）。
        let mut backend = FakeBackend::healthy(catalog(), document());
        backend.schema = Err(FakeError::Unreachable);
        backend.get = Err(FakeError::Unreachable);

        for (command, needle) in [
            (
                ConfigCommand::Set {
                    path: "providers[]".to_string(),
                    value: Some("1".to_string()),
                    json_value: None,
                    force: false,
                },
                "模板路径",
            ),
            (
                ConfigCommand::Set {
                    path: "gateway.port".to_string(),
                    value: None,
                    json_value: None,
                    force: false,
                },
                "缺少 <value>",
            ),
            (
                ConfigCommand::Set {
                    path: "gateway.port".to_string(),
                    value: Some("1".to_string()),
                    json_value: Some("2".to_string()),
                    force: false,
                },
                "不能同时给出",
            ),
            (
                ConfigCommand::Add {
                    path: "providers[0].models".to_string(),
                    value: Some("x".to_string()),
                    json_value: None,
                    variant: Some("rich".to_string()),
                    force: false,
                },
                "--variant",
            ),
            (
                ConfigCommand::Set {
                    path: "providers[0].extra_body".to_string(),
                    value: None,
                    json_value: Some("{oops".to_string()),
                    force: false,
                },
                "不是合法 JSON",
            ),
        ] {
            let (code, stdout, stderr) = run_cmd(&backend, command, false).await;
            assert_eq!(code, 4, "{stderr}");
            assert!(stdout.is_empty(), "{stdout}");
            assert!(stderr.contains(needle), "{stderr}");
        }
    }

    #[tokio::test]
    async fn noop_writes_short_circuit_without_posting() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Unset {
                path: "gateway.host".to_string(),
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert!(stdout.contains("（无操作）"), "{stdout}");
        assert!(backend.requests().is_empty(), "无操作不得发保存请求");

        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Move {
                path: "providers[0]".to_string(),
                delta: -1,
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert!(stdout.contains("已在边界"), "{stdout}");
        assert!(backend.requests().is_empty());

        // --json 下的无操作形状。
        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Unset {
                path: "gateway.host".to_string(),
                force: false,
            },
            true,
        )
        .await;
        assert_eq!(code, 0);
        let parsed: Value = serde_json::from_str(stdout.trim()).expect("noop json parses");
        assert_eq!(parsed["ok"], json!(true));
        assert_eq!(parsed["noop"], json!("already_default"));
        assert_eq!(parsed["response"], Value::Null);
    }

    #[tokio::test]
    async fn add_posts_the_appended_document_and_reports_the_new_path() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Add {
                path: "providers[0].models".to_string(),
                value: Some("ds-max".to_string()),
                json_value: None,
                variant: Some("simple".to_string()),
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert!(
            stdout.contains("新增项：providers[0].models[2]"),
            "{stdout}"
        );
        assert_eq!(
            backend.last_document()["providers"][0]["models"][2],
            json!("ds-max")
        );
    }

    #[tokio::test]
    async fn remove_and_move_post_the_expected_documents() {
        let backend = FakeBackend::healthy(catalog(), document());
        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Remove {
                path: "providers[0].models[0]".to_string(),
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert!(
            stdout.contains("已移除：providers[0].models[0] = ds-flash"),
            "{stdout}"
        );
        assert_eq!(
            backend.last_document()["providers"][0]["models"],
            json!([{"id": "ds-pro"}])
        );

        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Move {
                path: "providers[0].models[0]".to_string(),
                delta: 1,
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert!(
            stdout.contains("已移动：providers[0].models[0] → [1]"),
            "{stdout}"
        );
    }

    /// 密文契约的**专门**单测：任何输出路径（get / list / set 回执 / --json）都不出现真实值。
    #[tokio::test]
    async fn secret_value_is_never_echoed_on_any_output_path() {
        const SECRET: &str = "sk-live-supersecret-123456";

        // ① set：值必须发给网关，但绝不能出现在任何输出流上。
        for json in [false, true] {
            let mut backend = FakeBackend::healthy(catalog(), document());
            let mut response = ok_set_response();
            response.changed = vec!["providers[0].api_key".to_string()];
            backend.set = Ok(response);
            let (code, stdout, stderr) = run_cmd(
                &backend,
                ConfigCommand::Set {
                    path: "providers[0].api_key".to_string(),
                    value: Some(SECRET.to_string()),
                    json_value: None,
                    force: false,
                },
                json,
            )
            .await;
            assert_eq!(code, 0, "stdout={stdout} stderr={stderr}");
            assert_eq!(
                backend.last_document()["providers"][0]["api_key"],
                json!(SECRET),
                "值必须原样发给网关"
            );
            assert!(
                !stdout.contains(SECRET),
                "set --json={json} 泄露到 stdout：{stdout}"
            );
            assert!(
                !stderr.contains(SECRET),
                "set --json={json} 泄露到 stderr：{stderr}"
            );
            assert!(
                stdout.contains("providers[0].api_key"),
                "回执要给出路径：{stdout}"
            );
        }

        // ② get / list：对抗性文档（后端没掩码）+ hint → 只许出现掩码与末 4 位。
        let mut values = document();
        values["providers"][0]["api_key"] = json!(SECRET);
        let mut backend = FakeBackend::healthy(catalog(), values);
        let mut secrets = HashMap::new();
        secrets.insert(
            "providers[0].api_key".to_string(),
            SecretState {
                state: SecretPresence::Set,
                hint: Some("3456".to_string()),
            },
        );
        if let Ok(response) = backend.get.as_mut() {
            response.secrets = secrets;
        }

        for json in [false, true] {
            let (code, stdout, stderr) = run_cmd(
                &backend,
                ConfigCommand::Get {
                    path: "providers[0].api_key".to_string(),
                },
                json,
            )
            .await;
            assert_eq!(code, 0);
            assert!(!stdout.contains(SECRET), "get --json={json} 泄露：{stdout}");
            assert!(!stderr.contains(SECRET));
            assert!(
                !stdout.contains("supersecret"),
                "只允许末 4 位提示：{stdout}"
            );
            assert!(stdout.contains("3456"), "hint 必须给出：{stdout}");
        }

        for json in [false, true] {
            let (code, stdout, stderr) = run_cmd(
                &backend,
                ConfigCommand::List {
                    section: None,
                    only_overridden: false,
                },
                json,
            )
            .await;
            assert_eq!(code, 0);
            assert!(
                !stdout.contains(SECRET),
                "list --json={json} 泄露：{stdout}"
            );
            assert!(!stderr.contains(SECRET));
        }

        // ③ remove：摘要里同样不许带出密文（文档里是明文时也一样）。
        let (code, stdout, _) = run_cmd(
            &backend,
            ConfigCommand::Remove {
                path: "providers[0]".to_string(),
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert!(!stdout.contains(SECRET), "remove 摘要泄露：{stdout}");
    }

    // ------------------------------------------------------------
    // clap 解析
    // ------------------------------------------------------------

    fn parse(argv: &[&str]) -> (bool, ConfigCommand) {
        let cli = crate::cmd::Cli::try_parse_from(argv).expect("expected the CLI to parse");
        let Some(crate::cmd::Command::Config { command }) = cli.command else {
            panic!("expected a config subcommand, got {:?}", cli.command);
        };
        (cli.json, command)
    }

    #[test]
    fn clap_parses_every_config_subcommand() {
        let (json, command) = parse(&["wing", "config", "doctor"]);
        assert!(!json);
        assert!(matches!(command, ConfigCommand::Doctor));

        let (json, command) = parse(&[
            "wing",
            "config",
            "list",
            "--section",
            "gateway",
            "--only-overridden",
            "--json",
        ]);
        assert!(json);
        match command {
            ConfigCommand::List {
                section,
                only_overridden,
            } => {
                assert_eq!(section.as_deref(), Some("gateway"));
                assert!(only_overridden);
            }
            other => panic!("expected list, got {other:?}"),
        }

        let (_, command) = parse(&["wing", "config", "get", "gateway.port"]);
        match command {
            ConfigCommand::Get { path } => assert_eq!(path, "gateway.port"),
            other => panic!("expected get, got {other:?}"),
        }

        let (_, command) = parse(&["wing", "config", "set", "gateway.port", "8080", "--force"]);
        match command {
            ConfigCommand::Set {
                path,
                value,
                json_value,
                force,
            } => {
                assert_eq!(path, "gateway.port");
                assert_eq!(value.as_deref(), Some("8080"));
                assert_eq!(json_value, None);
                assert!(force);
            }
            other => panic!("expected set, got {other:?}"),
        }

        let (_, command) = parse(&[
            "wing",
            "config",
            "set",
            "providers[0].extra_body",
            "--json-value",
            r#"{"a":1}"#,
        ]);
        match command {
            ConfigCommand::Set {
                value, json_value, ..
            } => {
                assert_eq!(value, None);
                assert_eq!(json_value.as_deref(), Some(r#"{"a":1}"#));
            }
            other => panic!("expected set, got {other:?}"),
        }

        let (_, command) = parse(&["wing", "config", "unset", "gateway.port"]);
        assert!(matches!(command, ConfigCommand::Unset { .. }));

        let (_, command) = parse(&["wing", "config", "add", "providers", "--variant", "object"]);
        match command {
            ConfigCommand::Add {
                path,
                value,
                variant,
                ..
            } => {
                assert_eq!(path, "providers");
                assert_eq!(value, None);
                assert_eq!(variant.as_deref(), Some("object"));
            }
            other => panic!("expected add, got {other:?}"),
        }

        let (_, command) = parse(&["wing", "config", "remove", "providers[1]"]);
        assert!(matches!(command, ConfigCommand::Remove { .. }));

        // 带符号 delta：负值不能被当成旗标（allow_negative_numbers）。
        let (_, command) = parse(&["wing", "config", "move", "providers[1]", "-1"]);
        match command {
            ConfigCommand::Move { path, delta, .. } => {
                assert_eq!(path, "providers[1]");
                assert_eq!(delta, -1);
            }
            other => panic!("expected move, got {other:?}"),
        }
        let (_, command) = parse(&["wing", "config", "move", "providers[0]", "2"]);
        match command {
            ConfigCommand::Move { delta, .. } => assert_eq!(delta, 2),
            other => panic!("expected move, got {other:?}"),
        }

        let (_, command) = parse(&["wing", "config", "path"]);
        assert!(matches!(command, ConfigCommand::Path));
    }
}
