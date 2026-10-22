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

use std::process::ExitCode;

use clap::Subcommand;
use serde::Serialize;

use super::settings_api::SettingsApi;
use super::{backend_config, common};

mod catalog;
mod doctor;
mod error;
mod path;
mod read;
mod render;
#[cfg(test)]
mod testkit;
mod write;

use doctor::execute_doctor;
use error::api_failure;
use read::{execute_get, execute_list};
use render::json_string;
use write::{WriteOp, execute_write};

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
        /// Only show this group (case-insensitive; e.g. `gateway`).
        ///
        /// The group names are the backend's declaration (`config/groups.py`) —
        /// the same anchors the TUI settings panel lists in its left column.
        /// `Advanced` covers hooks / commands / log / user_agent.
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
        ///
        /// Negative numbers are accepted (`-1`, `-2.5`) — without this the value
        /// would be taken for a flag by clap and rejected with exit code 2.
        #[arg(allow_negative_numbers = true)]
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
        ///
        /// Negative numbers are accepted (`-1`, `-2.5`) — see `set` for why.
        #[arg(allow_negative_numbers = true)]
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

// ============================================================
// 入口
// ============================================================

/// Entry point for `wing config`.
///
/// **不自动拉起网关**：`wing config` 常常正是在网关起不来时被用的，自动拉起会掩盖问题。
/// `path` 子命令是唯一不联网的分支。
pub async fn run(command: ConfigCommand, json: bool) -> ExitCode {
    if matches!(command, ConfigCommand::Path) {
        return emit(execute_path(json));
    }
    // 只从后端配置读 host:port —— **不**走 `common::ensure_gateway()`（那个会拉起网关）。
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
async fn run_with<B: SettingsApi>(
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
async fn execute<B: SettingsApi>(
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
// 测试
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::config::testkit::*;
    use std::collections::HashMap;

    use clap::Parser;
    use serde_json::{Value, json};
    use wing_api_client::ApiClientError;
    use wing_api_client::models::{
        SecretPresence, SecretState, SettingsSetResponse, SettingsStatusResponse,
    };

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

    /// 503 + `error=="setup_mode"` 必须给出修复模式提示，而不是裸 503 文本。
    #[test]
    fn setup_mode_503_is_reported_with_a_hint() {
        let error = ApiClientError::Api {
            status: 503,
            detail: "setup mode".to_string(),
            body: Some(wing_api_client::models::ErrorResponse {
                error: "setup_mode".to_string(),
                detail: Some("configuration is invalid".to_string()),
                session_id: None,
                uuid: None,
            }),
        };
        let (code, message) = api_failure(&error, "http://127.0.0.1:32523", "读取配置状态失败");
        assert_eq!(code, 1, "503 是协议级失败，不是「不可达」");
        assert!(message.contains("修复模式"), "{message}");
        assert!(message.contains("loopback"), "{message}");

        // 普通 503（没有 setup_mode 错误码）不带这条提示。
        let plain = ApiClientError::Api {
            status: 503,
            detail: "service unavailable".to_string(),
            body: None,
        };
        let (code, message) = api_failure(&plain, "http://127.0.0.1:32523", "读取配置状态失败");
        assert_eq!(code, 1);
        assert!(!message.contains("修复模式"), "{message}");
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

    /// 六个子命令 × 带 `config.` 前缀 / 不带前缀
    /// → 落到**同一个文档位置**、回显**同一条规范路径**、绝不出现名为 `config` 的伪键。
    #[tokio::test]
    async fn root_prefix_is_normalized_across_every_subcommand() {
        let cases: Vec<(&str, ConfigCommand, ConfigCommand)> = vec![
            (
                "set",
                ConfigCommand::Set {
                    path: "config.gateway.port".to_string(),
                    value: Some("9999".to_string()),
                    json_value: None,
                    force: false,
                },
                ConfigCommand::Set {
                    path: "gateway.port".to_string(),
                    value: Some("9999".to_string()),
                    json_value: None,
                    force: false,
                },
            ),
            (
                "get",
                ConfigCommand::Get {
                    path: "config.gateway.port".to_string(),
                },
                ConfigCommand::Get {
                    path: "gateway.port".to_string(),
                },
            ),
            (
                "unset",
                ConfigCommand::Unset {
                    path: "config.gateway.port".to_string(),
                    force: false,
                },
                ConfigCommand::Unset {
                    path: "gateway.port".to_string(),
                    force: false,
                },
            ),
            (
                "add",
                ConfigCommand::Add {
                    path: "config.tools".to_string(),
                    value: Some("read".to_string()),
                    json_value: None,
                    variant: None,
                    force: false,
                },
                ConfigCommand::Add {
                    path: "tools".to_string(),
                    value: Some("read".to_string()),
                    json_value: None,
                    variant: None,
                    force: false,
                },
            ),
            (
                "remove",
                ConfigCommand::Remove {
                    path: "config.tools[0]".to_string(),
                    force: false,
                },
                ConfigCommand::Remove {
                    path: "tools[0]".to_string(),
                    force: false,
                },
            ),
            (
                "move",
                ConfigCommand::Move {
                    path: "config.providers[0].models[1]".to_string(),
                    delta: -1,
                    force: false,
                },
                ConfigCommand::Move {
                    path: "providers[0].models[1]".to_string(),
                    delta: -1,
                    force: false,
                },
            ),
        ];

        for (name, prefixed, plain) in cases {
            let with_prefix = FakeBackend::healthy(catalog(), document());
            let without = FakeBackend::healthy(catalog(), document());
            let (code_a, stdout_a, stderr_a) = run_cmd(&with_prefix, prefixed, false).await;
            let (code_b, stdout_b, stderr_b) = run_cmd(&without, plain, false).await;

            assert_eq!(code_a, code_b, "{name}: 退出码必须一致");
            assert_eq!(stdout_a, stdout_b, "{name}: 回显必须用规范路径");
            assert_eq!(stderr_a, stderr_b, "{name}: stderr 必须一致");

            let documents_a = with_prefix.requests();
            let documents_b = without.requests();
            assert_eq!(
                documents_a.len(),
                documents_b.len(),
                "{name}: 提交次数必须一致"
            );
            if let (Some(a), Some(b)) = (documents_a.last(), documents_b.last()) {
                assert_eq!(
                    a.document, b.document,
                    "{name}: 两种拼写必须落到同一个文档位置"
                );
                assert!(
                    a.document.get("config").is_none(),
                    "{name}: 不许出现名为 config 的伪键：{}",
                    a.document
                );
            }
            assert!(
                !stdout_a.contains("config.gateway")
                    && !stdout_a.contains("config.tools")
                    && !stdout_a.contains("config.providers"),
                "{name}: 回显里不该出现带前缀的路径：{stdout_a}"
            );
        }
    }

    /// 只敲根名（或路径解析后为空）→ 用法错误 4，并列出可用顶层节名。
    #[tokio::test]
    async fn root_only_path_is_a_usage_error_listing_the_top_level_sections() {
        let backend = FakeBackend::healthy(catalog(), document());
        // `config` 规范化后为空 → 报"路径为空"并列出顶层节名；`config.` 是文法错（空段）→ 同样是 4。
        for (path, needle) in [("config", "路径为空"), ("config.", "路径文法非法")] {
            let (code, stdout, stderr) = run_cmd(
                &backend,
                ConfigCommand::Get {
                    path: path.to_string(),
                },
                false,
            )
            .await;
            assert_eq!(code, 4, "{path}: stderr={stderr}");
            assert!(stdout.is_empty(), "{path}: {stdout}");
            assert!(stderr.contains(needle), "{path}: {stderr}");
            if path == "config" {
                assert!(
                    stderr.contains("providers"),
                    "{path}: 要列出顶层节名 {stderr}"
                );
            }
        }
        // 写命令同理（规范化后为空 → 4，不是静默成功）。
        let (code, _, stderr) = run_cmd(
            &backend,
            ConfigCommand::Set {
                path: "config".to_string(),
                value: Some("1".to_string()),
                json_value: None,
                force: false,
            },
            false,
        )
        .await;
        assert_eq!(code, 4);
        assert!(stderr.contains("路径为空"), "{stderr}");
        assert!(backend.requests().is_empty(), "不许发保存请求");
    }

    /// 带前缀（以及非规范拼写的下标）查密文字段，hint 必须查得到。
    #[tokio::test]
    async fn secrets_lookup_uses_the_canonical_path() {
        let mut backend = FakeBackend::healthy(catalog(), document());
        let mut secrets = HashMap::new();
        secrets.insert(
            "providers[0].api_key".to_string(),
            SecretState {
                state: SecretPresence::Set,
                hint: Some("1234".to_string()),
            },
        );
        if let Ok(response) = backend.get.as_mut() {
            response.secrets = secrets;
        }

        for path in [
            "providers[0].api_key",
            "config.providers[0].api_key",
            "providers[00].api_key",
        ] {
            let (code, stdout, stderr) = run_cmd(
                &backend,
                ConfigCommand::Get {
                    path: path.to_string(),
                },
                false,
            )
            .await;
            assert_eq!(code, 0, "{path}: {stderr}");
            assert!(stdout.contains("•••••••• 1234"), "{path}: {stdout}");
            assert!(
                stdout.starts_with("providers[0].api_key ="),
                "{path}: 回显必须是规范路径：{stdout}"
            );

            let (_, stdout, _) = run_cmd(
                &backend,
                ConfigCommand::Get {
                    path: path.to_string(),
                },
                true,
            )
            .await;
            let parsed: Value = serde_json::from_str(stdout.trim()).expect("json");
            assert_eq!(parsed["path"], json!("providers[0].api_key"), "{path}");
            assert_eq!(parsed["secret"]["state"], json!("set"), "{path}");
            assert_eq!(parsed["secret"]["hint"], json!("1234"), "{path}");
        }
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
            warnings: Vec::new(),
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

    /// `set` / `add` 的位置参数收负数——clap 默认把 `-1` 当旗标（退出码 2，
    /// 与"网关不可达"同码），必须在解析层就放行，让值走到强制转换表。
    #[test]
    fn clap_parses_negative_values_for_set_and_add() {
        let (_, command) = parse(&["wing", "config", "set", "gateway.port", "-1"]);
        match command {
            ConfigCommand::Set { value, force, .. } => {
                assert_eq!(value.as_deref(), Some("-1"));
                assert!(!force);
            }
            other => panic!("expected set, got {other:?}"),
        }
        let (_, command) = parse(&["wing", "config", "set", "images.quality", "-1.5"]);
        match command {
            ConfigCommand::Set { value, .. } => assert_eq!(value.as_deref(), Some("-1.5")),
            other => panic!("expected set, got {other:?}"),
        }
        let (_, command) = parse(&["wing", "config", "add", "tools", "-1"]);
        match command {
            ConfigCommand::Add { value, .. } => assert_eq!(value.as_deref(), Some("-1")),
            other => panic!("expected add, got {other:?}"),
        }
        // 放行负数不能把旗标也吞成值：`--force` / `--json-value` 必须仍是旗标。
        let (_, command) = parse(&["wing", "config", "set", "gateway.port", "--force"]);
        match command {
            ConfigCommand::Set { value, force, .. } => {
                assert_eq!(value, None, "`--force` 不能被当成值");
                assert!(force);
            }
            other => panic!("expected set, got {other:?}"),
        }
        let (_, command) = parse(&["wing", "config", "set", "x.y", "-1", "--force"]);
        match command {
            ConfigCommand::Set { value, force, .. } => {
                assert_eq!(value.as_deref(), Some("-1"));
                assert!(force);
            }
            other => panic!("expected set, got {other:?}"),
        }
    }

    /// `--force` 对五个写命令都要能解析（冲突恢复动作是同构的），`add --json-value` 同理。
    #[test]
    fn clap_parses_force_on_every_writer_and_json_value_on_add() {
        let (_, command) = parse(&["wing", "config", "unset", "gateway.port", "--force"]);
        assert!(matches!(command, ConfigCommand::Unset { force: true, .. }));
        let (_, command) = parse(&["wing", "config", "remove", "tools[0]", "--force"]);
        assert!(matches!(command, ConfigCommand::Remove { force: true, .. }));
        let (_, command) = parse(&["wing", "config", "move", "tools[0]", "1", "--force"]);
        assert!(matches!(command, ConfigCommand::Move { force: true, .. }));
        let (_, command) = parse(&["wing", "config", "add", "tools", "x", "--force"]);
        assert!(matches!(command, ConfigCommand::Add { force: true, .. }));

        let (_, command) = parse(&[
            "wing",
            "config",
            "add",
            "providers[0].models",
            "--json-value",
            r#"{"id":"ds-x"}"#,
            "--variant",
            "object",
        ]);
        match command {
            ConfigCommand::Add {
                value,
                json_value,
                variant,
                ..
            } => {
                assert_eq!(value, None);
                assert_eq!(json_value.as_deref(), Some(r#"{"id":"ds-x"}"#));
                assert_eq!(variant.as_deref(), Some("object"));
            }
            other => panic!("expected add, got {other:?}"),
        }
    }
}
