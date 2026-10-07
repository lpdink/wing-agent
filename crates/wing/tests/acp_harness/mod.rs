//! `acp_e2e` 的测试基建（见 `06_e2e/design.md`）。
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`gateway`] | 假网关（进程内 HTTP + WS）：记录请求、按脚本推事件 |
//! | [`client`] | 官方 ACP 客户端：`run_client` + 记录器 + 脚本化应答 |
//! | 本模块 | 临时 `WING_HOME`、`AcpAgent` 启动配置、条件轮询、帧 fixture |
//!
//! 纪律：不联网、不碰用户网关与 `~/.wing`；端口临时分配；每测试独立 WING_HOME 与子进程。

pub mod client;
pub mod gateway;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use agent_client_protocol::AcpAgent;
use agent_client_protocol::AcpAgentConfig;
use serde_json::Value;
use serde_json::json;

pub use client::*;
pub use gateway::*;

/// 等跨进程信号的单次超时上限（宽松但必须有界）。
pub const WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// 整个场景（`connect_with` 前台）的总超时：任何挂死都以可诊断的失败收口。
pub const SCENARIO_TIMEOUT: Duration = Duration::from_secs(30);

/// 负断言（「从未发生」）的观察窗口：断言前等这么久。
pub const QUIET_WINDOW: Duration = Duration::from_millis(200);

static HOME_SEQ: AtomicUsize = AtomicUsize::new(0);

// ============================================================
// 装置
// ============================================================

/// 一次 e2e 的完整装置：临时 `WING_HOME` + 指向它的假网关。
///
/// `wing acp` 的启动配置由 [`Harness::agent`] 给出（真二进制 + WING_HOME 环境变量）。
/// `gateway` 是 `Arc`，场景闭包可以持一份（harness 本体要活到场景结束——它持有临时
/// WING_HOME 的清理责任）。
pub struct Harness {
    pub gateway: Arc<FakeGateway>,
    home: TempHome,
}

impl Harness {
    pub async fn start() -> Self {
        let gateway = FakeGateway::start();
        let home = TempHome::new(gateway.port());
        Self {
            gateway: Arc::new(gateway),
            home,
        }
    }

    /// 会话的 `cwd` / workspace（绝对路径且存在——`session/new` 会校验）。
    pub fn workspace(&self) -> String {
        self.home.path().to_string_lossy().to_string()
    }

    /// 被测进程的启动配置：`env!("CARGO_BIN_EXE_wing") acp`，`WING_HOME` 指向本装置。
    ///
    /// `WING_ACP_E2E_TRACE=1` 时把全部 stdio 帧打到 stderr（失败时随测试输出可见）。
    pub fn agent(&self) -> AcpAgent {
        let agent = AcpAgent::new(
            AcpAgentConfig::new(env!("CARGO_BIN_EXE_wing"))
                .arg("acp")
                .env("WING_HOME", self.home.path().to_string_lossy().to_string()),
        );
        if std::env::var_os("WING_ACP_E2E_TRACE").is_some() {
            agent.with_debug(|line, direction| eprintln!("[wing acp {direction:?}] {line}"))
        } else {
            agent
        }
    }
}

/// 临时 `WING_HOME`（Drop 时 best-effort 清理）。
///
/// 公开给不走 [`Harness`] 的用例（如冷启动回归：网关由 `wing acp` 自己拉起，
/// 端口与 `WING_GATEWAY_CMD` 都要测试自己安排）。
pub struct TempHome {
    path: PathBuf,
}

impl TempHome {
    /// 建一个临时 `WING_HOME`，其 `core/config.yaml` 指向 `port`。
    pub fn new(port: u16) -> Self {
        let seq = HOME_SEQ.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("wing-acp-e2e-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(path.join("core")).expect("create temp WING_HOME");
        std::fs::write(
            path.join("core").join("config.yaml"),
            format!("gateway:\n  host: 127.0.0.1\n  port: {port}\n"),
        )
        .expect("write gateway config into the temp WING_HOME");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ============================================================
// 等待
// ============================================================

/// 条件轮询：等到 `probe` 返回 `Some`，或到 [`WAIT_TIMEOUT`] 时以 `what` 为线索 panic。
pub async fn wait_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + WAIT_TIMEOUT;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "等待超时（{} ms）：{what}",
            WAIT_TIMEOUT.as_millis(),
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// 有界静默：负断言（「此后不再发生」）的观察窗口。
pub async fn settle() {
    tokio::time::sleep(QUIET_WINDOW).await;
}

// ============================================================
// 帧 fixture
// ============================================================

/// 给一条 WingEvent 帧补公共 meta（`created_at` / `request_id` / `session_id`）。
///
/// `session_id` 已存在的帧（如 `sync_session` 自带具名字段）原样保留语义。
pub fn wing_event(session_id: &str, mut frame: Value) -> Value {
    frame["session_id"] = json!(session_id);
    frame["created_at"] = json!("2026-01-01T00:00:00+00:00");
    frame["request_id"] = json!("req-e2e");
    frame
}

pub fn text(content: &str) -> Value {
    json!({"type": "text", "content": content})
}

pub fn reasoning(content: &str) -> Value {
    json!({"type": "reasoning", "content": content})
}

pub fn tool_call_stream(tool_call_id: &str, tool_name: &str) -> Value {
    json!({
        "type": "tool_call_stream",
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
        "args_fragment": "{\"path\":",
        "is_final": false,
    })
}

pub fn tool_call(tool_call_id: &str, tool_name: &str, args: Value) -> Value {
    json!({
        "type": "tool_call",
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
        "tool_args": args,
    })
}

pub fn tool_call_result(tool_call_id: &str, tool_name: &str, result: &str, success: bool) -> Value {
    json!({
        "type": "tool_call_result",
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
        "tool_args": {},
        "tool_result": result,
        "tool_success": success,
    })
}

pub fn diff_content(
    tool_call_id: &str,
    path: &str,
    old_text: Option<&str>,
    new_text: &str,
) -> Value {
    json!({
        "type": "diff_content",
        "tool_call_id": tool_call_id,
        "path": path,
        "old_text": old_text,
        "new_text": new_text,
    })
}

pub fn session_title(title: &str) -> Value {
    json!({"type": "session_state_changed", "title": title})
}

pub fn context_stats(total_tokens: i64, context_window_tokens: i64) -> Value {
    json!({
        "type": "context_stats",
        "message_count": 4,
        "total_tokens": total_tokens,
        "context_window_tokens": context_window_tokens,
    })
}

pub fn turn_result() -> Value {
    json!({
        "type": "turn_result",
        "subtype": "success",
        "result": null,
        "num_turns": 1,
        "duration_ms": 7,
    })
}

pub fn done() -> Value {
    json!({"type": "done"})
}

pub fn interrupted() -> Value {
    json!({"type": "interrupted"})
}

/// Bash 危险命令确认形态的 `ask`（`choices` = y/n/yolo，`required`）。
pub fn bash_ask(tool_call_id: &str, question: &str) -> Value {
    json!({
        "type": "ask",
        "tool_call_id": tool_call_id,
        "question": question,
        "choices": ["y", "n", "yolo"],
        "required": true,
    })
}

/// AskUserQuestion 形态的 `ask`（`questions` 非空即走问答路径）。
pub fn questions_ask(tool_call_id: &str, questions: Value) -> Value {
    json!({
        "type": "ask",
        "tool_call_id": tool_call_id,
        "questions": questions,
        "choices": [],
        "required": false,
    })
}

// ---- 网关响应 fixture ----

/// `/api/session/list` 的一行（`SessionInfo`）。
pub fn session_row(
    id: &str,
    name: Option<&str>,
    workspace: Option<&str>,
    last_interaction: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "created_at": "2026-01-02T03:04:05+00:00",
        "template_name": "default",
        "workspace": workspace,
        "last_interaction": last_interaction,
        "status": "inactive",
        "tags": [],
        "tag_meta": {},
    })
}

/// `/api/models` 目录：`[(provider, [(model, display_name)])]`。
pub fn models_catalog(providers: &[(&str, &[(&str, &str)])]) -> Value {
    let providers: Vec<Value> = providers
        .iter()
        .map(|(provider, models)| {
            json!({
                "provider": provider,
                "models": models.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
                "model_details": models
                    .iter()
                    .map(|(name, display)| json!({"name": name, "display_name": display}))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({"providers": providers})
}

/// `/api/commands`：`[(name, description, params)]`。
pub fn commands_catalog(commands: &[(&str, &str, &str)]) -> Value {
    json!({
        "commands": commands
            .iter()
            .map(|(name, description, params)| {
                json!({"name": name, "aliases": [], "description": description, "params": params})
            })
            .collect::<Vec<_>>(),
    })
}

/// 一条 `sync_session` 快照（回放素材）。
pub fn sync_session(session_id: &str, messages: Value, events: Value) -> Value {
    wing_event(
        session_id,
        json!({
            "type": "sync_session",
            "session_id": session_id,
            "status": "idle",
            "messages": messages,
            "events": events,
        }),
    )
}
