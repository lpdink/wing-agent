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
//! | [`agent`] | ACP handler 注册（initialize / session/new / session/prompt / session/cancel） |
//! | [`session`] | `SessionHub`：会话表、WS 事件泵与分流、出站队列、prompt 串行化 |
//! | [`translate`] | `WingEvent` → ACP `session/update` 的映射（纯函数） |
//!
//! 后续步骤（03 ask / 04 model / 05 sessions）在本模块的接口上追加：
//! handler 追加点见 [`agent::serve`]，事件映射追加点见 [`translate::ToolCards::updates_for`]
//! 与 [`translate::turn_end`]，会话操作追加点见 [`session::SessionHub`]。
// stderr 承载诊断（`wing error: …`），stdout 已被 ACP 协议占用。
#![allow(clippy::print_stderr)]

pub mod agent;
pub mod session;
pub mod translate;

use std::process::ExitCode;

use anyhow::Context;
use anyhow::Result;

use crate::gateway::GatewayClient;
use wing_api_client::GatewayClient as GatewayApiClient;

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
    // 1. 网关（复用 stdio 前端的 ensure 流程：健康检查 → 必要时拉起守护进程）。
    let (host, port) = crate::stdio::ensure_gateway_running().await?;
    let ws_url = format!("ws://{host}:{port}/ws");
    let http_base = format!("http://{host}:{port}");

    // 2. API key 与 TUI 同源（`$WING_HOME/tui/config.yaml`）。
    let api_key = crate::config::AppConfig::load()
        .api_key
        .filter(|k| !k.is_empty());

    // 3. WS 连接：拿 client_id；连接本体移交给 hub 的事件泵（见 session.rs 的
    //    模块文档：WS 客户端不可 Clone，收/发共用一条连接）。
    let gateway = GatewayClient::connect(&ws_url, api_key.as_deref())
        .await
        .with_context(|| format!("failed to connect to gateway at {ws_url}"))?;
    let client_id = gateway.client_id().to_string();

    // 4. HTTP client（建会话 / 订阅 / interrupt / 命令列表）。
    let http = GatewayApiClient::new(http_base, api_key.as_deref())
        .context("failed to create HTTP client")?;

    // 5. hub：会话表 + 事件分流 + 出站队列（内部 spawn WS 事件泵）。
    let hub = SessionHub::start(gateway, http, client_id);

    // 6. ACP 服务循环（stdout 归它）。
    agent::serve(hub, args).await.context("ACP serve failed")?;
    Ok(ExitCode::SUCCESS)
}
