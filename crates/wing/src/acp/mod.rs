//! `wing acp` — ACP（Agent Client Protocol）前端：stdio 上的 agent 服务端。
//!
//! 第四个前端形态：把本机 wing 网关桥接给 Zed / omnigent 等 ACP 客户端。与其它
//! 前端的差别只在**协议面**——底下仍是「HTTP 建会话 + 订阅 + WS 事件流」那一套
//! （见 [`crate::stdio`] 的同款骨架）：
//!
//! ```text
//! ACP client ──stdio(JSON-RPC/ACP)── wing acp ──HTTP/WS── wing-gateway ── wing runtime
//!                                        │
//!                                 SessionHub（会话表 / 事件分流 / 出站队列）
//! ```
//!
//! 纪律：
//!
//! - **stdout 只承载 ACP 帧**（SDK 的 `Stdio` 传输独占 stdout）；日志走 tracing
//!   （`$WING_HOME/tui/logs/`），stderr 只留人类可读的致命错误。
//! - 一个进程服务多个会话；同一会话的 `session/prompt` 串行化（见 `session` 模块）。
//! - 事件按 `meta.session_id` 分流，映射规则全在 `translate` 模块（纯函数 + 单测）。
//!
//! 子系统：
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`agent`] | ACP handler 注册（initialize / session/new / session/prompt / session/cancel / list / load / resume / close / set_config_option） |
//! | [`ask`] | Ask 事件 → ACP 交互面（permission / elicitation / 回退，答案格式契约） |
//! | [`model`] | 模型 config option（值 = model_id / 分组 / currentValue）+ 热切换 + 外部变更中继 |
//! | [`session`] | `SessionHub`：会话表、WS 事件泵与分流、出站队列、prompt 串行化 |
//! | [`translate`] | `WingEvent` → ACP `session/update` 的映射（纯函数） |
//!
//! 整机 e2e（`crates/wing/tests/acp_e2e.rs`）从协议面驱动：所有能力都经公开的 ACP
//! 方法与 `/api/*` 观察，模块内部没有为测试留的开关。
// stderr 承载诊断（`wing error: …`），stdout 已被 ACP 协议占用。
#![allow(clippy::print_stderr)]

pub mod agent;
pub mod ask;
pub mod model;
pub mod session;
pub mod translate;

use std::ops::ControlFlow;
use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;

use crate::cmd::common::{HeadlessGateway, ensure_gateway, prepare_headless_gateway};
use crate::gateway::GatewayClient;

use self::session::SessionHub;

/// `wing acp` 的启动参数（进程级默认值，作用于之后创建的每个 ACP 会话）。
#[derive(Debug, Clone, Default)]
pub struct AcpArgs {
    /// 会话模板名（`config.yaml` 的 `agents:` 条目）；None = 网关默认模板。
    pub agent: Option<String>,
    /// 初始模型覆盖；None = 模板模型（客户端仍可经 `/model` config option 改）。
    pub model: Option<String>,
}

/// `wing acp` 入口：连接网关、建立 hub，然后把 stdio 交给 ACP 服务循环。
pub async fn run_acp(args: AcpArgs) -> ExitCode {
    match run_acp_inner(args).await {
        Ok(code) => code,
        Err(e) => {
            // 与 stdio 前端同一口径：致命错误写 stderr（ACP 连接未建立或已结束，
            // stdout 上不会再有帧；客户端会把 stderr 收进自己的日志）。
            // `{e:#}` 打完整因果链（anyhow 的 Display 只给最外层 context）。
            eprintln!("wing error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_acp_inner(args: AcpArgs) -> Result<ExitCode> {
    let (host, port) = ensure_gateway().await?;
    let HeadlessGateway {
        http,
        ws_url,
        api_key,
    } = match prepare_headless_gateway(&host, port, |e| {
        anyhow::Error::new(e).context("failed to create HTTP client")
    })
    .await?
    {
        ControlFlow::Continue(gateway) => gateway,
        ControlFlow::Break(code) => return Ok(code),
    };

    let gateway = GatewayClient::connect(&ws_url, api_key.as_deref())
        .await
        .with_context(|| format!("failed to connect to gateway at {ws_url}"))?;
    let client_id = gateway.client_id().to_string();

    let hub = SessionHub::start(gateway, http, client_id);

    agent::serve(hub, args).await.context("ACP serve failed")?;
    Ok(ExitCode::SUCCESS)
}
