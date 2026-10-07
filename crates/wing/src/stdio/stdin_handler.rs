//! stdin handler for `--input-format stream-json` mode.
//!
//! stdin 是**常驻控制通道**：SDK（Claude Agent SDK / CloudCLI）在 turn 期间仍会
//! 写 `control_request`（`interrupt` 等）与 `keep_alive`；凡被 SDK await 的控制
//! 请求**必须有应答**，否则编排器的"停止"按钮永远等不到结果。
//!
//! 分工：pump 负责「读一行 → 分类 → 应答 / 投递」，与 turn 驱动（`run_stdio`
//! 的事件循环）通过两条通道协作——`mpsc` 按到达顺序投递 `user` 消息（常驻模式下
//! 由驱动侧逐条转发给网关），`watch<shutdown>` 接收收尾信号。所有 stdout 写入经
//! 共享的 [`StdoutSink`](crate::stdio::stdout::StdoutSink) 串行化。
//!
//! 多轮语义（`--input-format stream-json` + `--output-format stream-json`）：pump
//! 是常驻消息通道，轮间与轮中的每条 `user` 消息都投递给驱动侧（转发
//! `POST /api/session/send`，由网关 inbox 决定 steer / 排队）；进程在 stdin EOF
//! 时收尾，而不是在 `result` 帧后退出（见 `crate::stdio::ExitPolicy`）。
//!
//! 一次性语义（非常驻）：只有首条 `user` 消息有归宿——它要么是 prompt（CLI 未给
//! `-p`），要么被丢弃（CLI 已给 prompt）；其余消息记日志丢弃。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use wing_api_client::GatewayClient as GatewayApiClient;

use crate::stdio::stdout::StdoutSink;

/// interrupt 触发后等网关回话的上限。
///
/// 中断本身在极端形态下要走取消阶梯（后端文档 ~18s），但编排器正在 await 应答
/// ——"中断已触发"必须尽快回话，超时只记日志（见 [`handle_control_request`]）。
const INTERRUPT_ACK_WAIT: Duration = Duration::from_secs(2);

/// 收尾时等 pump 把进行中的工作做完的上限。
///
/// 必须 > [`INTERRUPT_ACK_WAIT`]（答案要落盘），且有界（极端形态也不拖住进程退出）。
const PUMP_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

// ============================================================
// Incoming message types (stdin → wing)
// ============================================================

/// Parsed stdin message from the SDK.
///
/// Uses explicit dispatch via [`StdinMessage::from_value`] rather than
/// `#[serde(untagged)]` on the enum, because `flatten` interacts poorly
/// with untagged enums. Each variant's inner struct uses standard
/// `#[derive(Deserialize)]` with `#[serde(flatten)]` for forward compat.
#[derive(Debug)]
pub enum StdinMessage {
    ControlRequest(ControlRequest),
    /// `type == "control_request"` 但解析失败（缺/错类型的 `request_id` 等）。
    ///
    /// 单独成桶而不是并进 [`StdinMessage::Unknown`]：这类帧**必须被应答**
    /// （SDK 在 await 它），静默丢弃会让编排器挂死且无从审计。能捞到
    /// `request_id`（**原样**，任意 JSON 类型——应答靠它匹配）就回显式 error，
    /// 捞不到只能记日志。
    MalformedControlRequest {
        request_id: Option<serde_json::Value>,
        error: String,
    },
    /// SDK 对「CLI 主动发起的 control_request」的应答。wing 目前不发起任何
    /// control_request，收到即忽略（记 debug：它意味着对端在应答一个并不存在的
    /// 请求）。
    ControlResponse,
    User(UserMessage),
    KeepAlive,
    Unknown,
}

impl StdinMessage {
    /// Parse a JSON value into a [`StdinMessage`].
    ///
    /// Dispatches on the `type` field, falling back to [`StdinMessage::Unknown`]
    /// for unrecognized types or malformed messages.
    pub fn from_value(value: serde_json::Value) -> Self {
        let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match msg_type {
            "control_request" => {
                // 解析失败前先把 request_id 原样捞出来（能捞到就还能应答：
                // 应答的 request_id 必须与对方发出的一致，类型也照抄）。
                let salvage = value.get("request_id").cloned();
                match serde_json::from_value(value) {
                    Ok(req) => Self::ControlRequest(req),
                    Err(e) => Self::MalformedControlRequest {
                        request_id: salvage,
                        error: e.to_string(),
                    },
                }
            }
            "control_response" => Self::ControlResponse,
            "user" => serde_json::from_value(value)
                .map(Self::User)
                .unwrap_or(Self::Unknown),
            "keep_alive" => Self::KeepAlive,
            _ => Self::Unknown,
        }
    }
}

/// SDK → wing: `control_request` message.
///
/// The SDK sends this for the initialize handshake and for operations like
/// `interrupt`. Every subtype gets an explicit reply — the SDK awaits most of
/// them, so silence means a hung orchestrator.
#[derive(Debug, Deserialize)]
pub struct ControlRequest {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub request_id: String,
    /// The inner request payload (subtype: "initialize" / "interrupt" / ...).
    /// Parsed as `Value` to absorb hooks/agents/skills fields we don't use;
    /// defaults to JSON null so a payload-less frame still gets a reply.
    #[serde(default)]
    pub request: serde_json::Value,
    /// Absorb any additional fields the SDK may send.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl ControlRequest {
    /// The `request.subtype` field (empty when absent/malformed).
    fn subtype(&self) -> &str {
        self.request
            .get("subtype")
            .and_then(|s| s.as_str())
            .unwrap_or("")
    }
}

/// SDK → wing: `user` message carrying the prompt.
#[derive(Debug, Deserialize)]
pub struct UserMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub message: UserMessageBody,
    /// Absorb `session_id`, `parent_tool_use_id`, and other SDK fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// The body of a user message: role + content.
#[derive(Debug, Deserialize)]
pub struct UserMessageBody {
    pub role: String,
    pub content: MessageContent,
}

/// `content` can be a plain string or an array of content blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A single content block inside a user message.
///
/// Only `type == "text"` blocks are used; other types (image, etc.) are skipped.
#[derive(Debug, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: Option<String>,
}

// ============================================================
// Outgoing message types (wing → stdout)
// ============================================================

/// wing → SDK: `control_response` message.
///
/// Wire shape matches the protocol the SDK parses (`sdk.mjs`
/// `readMessages()`): `response.subtype` + `response.request_id` decide the
/// fate of the SDK-side promise; `response` / `error` carry the payload.
#[derive(Serialize)]
struct ControlResponse {
    #[serde(rename = "type")]
    msg_type: &'static str,
    response: ControlResponseBody,
}

#[derive(Serialize)]
struct ControlResponseBody {
    subtype: &'static str,
    /// 回显对方发来的 `request_id`（对方靠它匹配应答；畸形帧里它可能不是字符串，
    /// 因此原样回传而不是转成 String）。
    request_id: serde_json::Value,
    /// 成功应答的载荷（SDK 只看 subtype / request_id）。
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<serde_json::Value>,
    /// 失败应答的错误文本（SDK 的 `request()` 会把它包进 `Error`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ControlResponse {
    /// 成功应答（载荷为空对象，与既有 initialize 应答形状一致）。
    fn success(request_id: impl Into<serde_json::Value>) -> Self {
        Self {
            msg_type: "control_response",
            response: ControlResponseBody {
                subtype: "success",
                request_id: request_id.into(),
                response: Some(serde_json::json!({})),
                error: None,
            },
        }
    }

    /// 失败应答：显式告诉编排器「这个操作 wing 没做成」，而不是静默 success。
    fn error(request_id: impl Into<serde_json::Value>, error: impl Into<String>) -> Self {
        Self {
            msg_type: "control_response",
            response: ControlResponseBody {
                subtype: "error",
                request_id: request_id.into(),
                response: None,
                error: Some(error.into()),
            },
        }
    }

    /// 一行的 JSON 文本（丢给 [`StdoutSink::line`] 写出去）。
    fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| {
            // 本结构只有 String / &'static str / Value，序列化不会失败。
            unreachable!("control_response serialization failed: {e}")
        })
    }
}

// ============================================================
// Interrupt trigger
// ============================================================

/// 中断动作的 future（trait 要 dyn-safe，不能直接写 `async fn`）。
pub type InterruptFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

/// 中断触发口。
///
/// pump 只负责协议与应答，"让网关停手"这一动作经窄接口注入：单测用假实现
/// 驱动（离线、不需要网关），生产实现是 [`GatewayInterruptor`]。
pub trait Interruptor: Send + Sync + 'static {
    /// 中断当前会话；错误只用于日志——调用方无论如何都回 success。
    fn interrupt(&self) -> InterruptFuture;
}

/// 生产实现：`POST /api/session/interrupt`（wing-api-client）。
pub struct GatewayInterruptor {
    http: GatewayApiClient,
    session_id: String,
}

impl GatewayInterruptor {
    pub fn new(http: GatewayApiClient, session_id: impl Into<String>) -> Self {
        Self {
            http,
            session_id: session_id.into(),
        }
    }
}

impl Interruptor for GatewayInterruptor {
    fn interrupt(&self) -> InterruptFuture {
        let http = self.http.clone();
        let session_id = self.session_id.clone();
        Box::pin(async move {
            http.interrupt_session(&session_id)
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from)
        })
    }
}

// ============================================================
// The pump
// ============================================================

/// stdin pump 的依赖与策略。
pub struct StdinPumpContext {
    /// 中断动作的注入点。
    pub interruptor: Arc<dyn Interruptor>,
    /// 与 renderer 共享的 stdout 出口（串行化，见 [`StdoutSink`]）。
    pub out: Arc<StdoutSink>,
    /// `user` 消息的投递策略（见 [`MessageDelivery`]）。
    pub delivery: MessageDelivery,
}

/// pump 对 stdin `user` 消息的投递策略。
///
/// 驱动侧按模式选一种（`crate::stdio::message_delivery`）：
/// - 常驻多轮：每条消息都要转发 → [`MessageDelivery::All`]；
/// - 一次性且 prompt 来自 stdin → 首条就是 prompt，其余没有归宿
///   （[`MessageDelivery::FirstOnly`]）；
/// - 一次性且 prompt 来自 CLI → stdin 上的消息一条都不要
///   （[`MessageDelivery::Ignore`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDelivery {
    /// 全部投递（常驻多轮）。
    All,
    /// 只投递首条（一次性流程里的 prompt）。
    FirstOnly,
    /// 一条都不投递（一次性流程且 prompt 已由 CLI 参数给出）。
    Ignore,
}

/// 常驻 stdin pump 的把手（turn 驱动侧持有）。
pub struct StdinPump {
    /// pump 投递的 `user` 消息（按 stdin 到达顺序；通道关闭 = stdin 关闭）。
    messages: mpsc::UnboundedReceiver<String>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl StdinPump {
    /// 下一条投递的 `user` 消息。
    ///
    /// `None` = stdin 关闭（EOF / 读错误 / 收尾），且已无缓冲消息——常驻循环
    /// 据此收尾。
    pub async fn next_message(&mut self) -> Option<String> {
        self.messages.recv().await
    }

    /// 等首条 `user` 消息（仅用于 prompt 要从 stdin 来的流程）。
    ///
    /// stdin 在消息到达前关闭 → 明确报错，而不是让 turn 驱动永远等下去。
    pub async fn wait_prompt(&mut self) -> Result<String> {
        match self.next_message().await {
            Some(prompt) => Ok(prompt),
            // 通道关闭且没有缓冲消息 = stdin 在首条消息之前就没了。
            None => anyhow::bail!("stdin closed before receiving user message"),
        }
    }

    /// 收尾：通知 pump 停下，并（有界）等它把进行中的工作做完。
    ///
    /// 存在这样的竞态：pump 正在处理 `interrupt`（HTTP 往返在手）而 turn 恰好
    /// 结束——直接 abort 会丢掉那条应答，SDK 侧的 `interrupt()` 就只能等到进程
    /// 退出时被 reject。这里的等待只在"确有进行中的工作"时发生（pump 停在读上时
    /// 收尾信号立即命中，join 立刻返回），有界保证极端形态也不拖住进程退出。
    pub async fn finish(self) {
        let StdinPump {
            messages,
            shutdown,
            mut task,
        } = self;
        // 先丢掉消息出口：pump 下一次投递会失败并自行结束（不必再读 stdin）。
        drop(messages);
        let _ = shutdown.send(true);
        if tokio::time::timeout(PUMP_SHUTDOWN_GRACE, &mut task)
            .await
            .is_err()
        {
            tracing::warn!("stdin pump did not stop in time; aborting it");
            task.abort();
        }
    }
}

/// 起一个常驻 pump，读真实 stdin。
pub fn spawn(ctx: StdinPumpContext) -> StdinPump {
    spawn_with(spawn_stdin_reader(), ctx)
}

/// stdin 行来源：**独立 std 线程**上的阻塞读 + 通道。
///
/// 不能用 `tokio::io::stdin()`：它把读放在 blocking pool 上且**无法取消**，进程
/// 退出时运行时会等那个未完成的读（tokio 文档原话：「can make shutdown of the
/// runtime hang until the user presses enter」）。编排器（CloudCLI）要到 turn 的
/// result 之后才 release stdin，退出不能依赖它。std 线程不受运行时管理（进程退出
/// 即被杀），pump 侧只从通道收行；EOF / 读错误 / 消费端消失 = 线程结束 → drop
/// 发送端 → 收端看到 `None`。
fn spawn_stdin_reader() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader as StdBufReader};
        let stdin = std::io::stdin();
        for line in StdBufReader::new(stdin.lock()).lines() {
            match line {
                // 发送失败 = pump 已走（收尾/退出），别再读了。
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "reading stdin failed");
                    break;
                }
            }
        }
    });
    rx
}

/// [`spawn`] 的注入版：行从通道来（单测直接喂行 / drop 发送端模拟 stdin 关闭）。
fn spawn_with(rx: mpsc::UnboundedReceiver<String>, ctx: StdinPumpContext) -> StdinPump {
    let (messages_tx, messages_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let task = tokio::spawn(async move {
        if let Err(e) = pump_loop(rx, &ctx, messages_tx, shutdown_rx).await {
            tracing::warn!(error = %e, "stdin pump stopped on stdin read error");
        }
    });

    StdinPump {
        messages: messages_rx,
        shutdown: shutdown_tx,
        task,
    }
}

/// pump 主循环：逐行消费 stdin，直到 EOF 或收尾信号。
async fn pump_loop(
    mut lines: mpsc::UnboundedReceiver<String>,
    ctx: &StdinPumpContext,
    messages: mpsc::UnboundedSender<String>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    // 是否已投递过 `user` 消息（`MessageDelivery::FirstOnly` 的判据）。
    let mut delivered_first = false;
    loop {
        // 收尾信号与下一行二选一。`biased` + 读优先：已经到达的控制帧先处理完，
        // 再执行收尾（`interrupt` 的应答可能正好排在这一行）。`changed()` 只看
        // 版本号——信号即便在上一轮处理中到达也不会丢（下一次循环立刻命中）；
        // 万一 stdin 一直有数据、收尾信号迟迟读不到，`finish()` 的有界等待兜底。
        let line = tokio::select! {
            biased;
            line = lines.recv() => match line {
                Some(line) => line,
                None => break, // stdin EOF（读线程结束）
            },
            _ = shutdown.changed() => break,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, line = %trimmed, "skipping non-JSON stdin line");
                continue;
            }
        };

        match StdinMessage::from_value(value) {
            StdinMessage::ControlRequest(req) => handle_control_request(&req, ctx).await,
            StdinMessage::MalformedControlRequest { request_id, error } => {
                // 「必有应答」不变量的兜底：解析失败的控制请求同样要有出口，
                // 且必须留下可审计的 warn（静默丢弃会让编排器挂死且查不到原因）。
                match request_id {
                    Some(request_id) => {
                        tracing::warn!(
                            request_id = %request_id,
                            error = %error,
                            "malformed control_request; answering error"
                        );
                        ctx.out.line(
                            &ControlResponse::error(
                                request_id,
                                format!("malformed control request: {error}"),
                            )
                            .to_line(),
                        );
                    }
                    None => tracing::warn!(
                        error = %error,
                        "malformed control_request without request_id; cannot answer"
                    ),
                }
            }
            StdinMessage::User(user) => {
                let prompt = extract_prompt_text(&user.message.content);
                let deliver = match ctx.delivery {
                    MessageDelivery::All => true,
                    MessageDelivery::FirstOnly => !delivered_first,
                    MessageDelivery::Ignore => false,
                };
                if deliver {
                    delivered_first = true;
                    tracing::info!(
                        prompt_len = prompt.len(),
                        "forwarding user message from stdin"
                    );
                    // 投递失败 = 驱动侧已走（收尾/退出）：别再读了。
                    if messages.send(prompt).is_err() {
                        break;
                    }
                } else {
                    tracing::warn!(
                        prompt_len = prompt.len(),
                        "ignoring user message (one-shot stdio mode: only the first message \
                         becomes the prompt)"
                    );
                }
            }
            StdinMessage::ControlResponse => {
                tracing::debug!("ignoring control_response from orchestrator");
            }
            StdinMessage::KeepAlive => {
                tracing::debug!("ignoring keep_alive");
            }
            StdinMessage::Unknown => {
                tracing::debug!("ignoring unknown stdin message type");
            }
        }
    }

    // EOF / 收尾：返回即 drop 消息出口 → 驱动侧 `next_message()` 得到 `None`
    // （若首条 prompt 还没投递，`wait_prompt` 在那里明确报错）。
    Ok(())
}

/// 处理一条 `control_request`：**凡是被 await 的请求都要有应答**。
async fn handle_control_request(req: &ControlRequest, ctx: &StdinPumpContext) {
    match req.subtype() {
        "initialize" => {
            tracing::info!(request_id = %req.request_id, "received initialize control_request");
            ctx.out
                .line(&ControlResponse::success(req.request_id.as_str()).to_line());
        }

        "interrupt" => {
            tracing::info!(
                request_id = %req.request_id,
                "received interrupt control_request: interrupting session"
            );
            // 有界等待：中断本身可能拖很久（取消阶梯），但编排器在 await 应答
            // ——超时 / 失败 / 无进行中的 turn 一律照回 success，只记日志。
            match tokio::time::timeout(INTERRUPT_ACK_WAIT, ctx.interruptor.interrupt()).await {
                Ok(Ok(())) => tracing::info!("session interrupted"),
                Ok(Err(e)) => tracing::warn!(
                    error = %e,
                    "interrupt request failed; answering success anyway"
                ),
                Err(_) => tracing::warn!(
                    wait_secs = INTERRUPT_ACK_WAIT.as_secs(),
                    "interrupt request still running; answering success anyway"
                ),
            }
            ctx.out
                .line(&ControlResponse::success(req.request_id.as_str()).to_line());
        }

        other => {
            let subtype = if other.is_empty() { "unknown" } else { other };
            tracing::warn!(
                subtype = subtype,
                request_id = %req.request_id,
                "unsupported control_request subtype; answering error"
            );
            ctx.out.line(
                &ControlResponse::error(
                    req.request_id.as_str(),
                    format!("unsupported control request subtype: {subtype}"),
                )
                .to_line(),
            );
        }
    }
}

/// Extract prompt text from a [`MessageContent`].
///
/// - `Text(s)` → returns `s` directly
/// - `Blocks(blocks)` → filters `type == "text"`, joins `text` fields with spaces
fn extract_prompt_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stdio::stdout::CaptureSink;
    use std::sync::Mutex;

    // ---- 测试夹具 ----

    /// 假中断触发器：记录调用次数，可配置为失败。
    #[derive(Default)]
    struct FakeInterruptor {
        calls: Mutex<usize>,
        fail: bool,
    }

    impl FakeInterruptor {
        fn failing() -> Self {
            Self {
                fail: true,
                ..Default::default()
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl Interruptor for FakeInterruptor {
        fn interrupt(&self) -> InterruptFuture {
            *self.calls.lock().unwrap() += 1;
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(anyhow::anyhow!("gateway said no"))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn context(
        interruptor: Arc<FakeInterruptor>,
        out: &CaptureSink,
        delivery: MessageDelivery,
    ) -> StdinPumpContext {
        StdinPumpContext {
            interruptor: interruptor as Arc<dyn Interruptor>,
            out: out.sink(),
            delivery,
        }
    }

    /// 等 stdout 攒够 `n` 行（pump 在别的 task 里，不能同步断言）。
    async fn wait_lines(capture: &CaptureSink, n: usize) -> Vec<serde_json::Value> {
        for _ in 0..400 {
            let text = capture.text();
            let lines: Vec<serde_json::Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
                .collect();
            if lines.len() >= n {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("expected {n} stdout lines, got: {:?}", capture.text());
    }

    const INITIALIZE: &str = r#"{"type":"control_request","request_id":"init-1","request":{"subtype":"initialize","hooks":null,"agents":null}}"#;
    const INTERRUPT: &str =
        r#"{"type":"control_request","request_id":"int-1","request":{"subtype":"interrupt"}}"#;

    // ---- StdinMessage::from_value ----

    #[test]
    fn parse_control_request_initialize() {
        let json = serde_json::json!({
            "type": "control_request",
            "request_id": "req-1",
            "request": {
                "subtype": "initialize",
                "hooks": null,
                "agents": null
            }
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::ControlRequest(req) => {
                assert_eq!(req.request_id, "req-1");
                assert_eq!(req.subtype(), "initialize");
            }
            _ => panic!("expected ControlRequest, got {msg:?}"),
        }
    }

    #[test]
    fn parse_control_request_with_extra_fields() {
        let json = serde_json::json!({
            "type": "control_request",
            "request_id": "req-2",
            "request": { "subtype": "initialize" },
            "extra_field": "should be absorbed"
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::ControlRequest(req) => {
                assert_eq!(req.request_id, "req-2");
                assert!(req.extra.contains_key("extra_field"));
            }
            _ => panic!("expected ControlRequest"),
        }
    }

    #[test]
    fn parse_control_request_without_payload_still_answers_as_unknown_subtype() {
        // 没有 request 字段的畸形帧不能落进 Unknown（那意味着永远不会应答），
        // 而应归到"未知 subtype" → 显式 error 应答。
        let json = serde_json::json!({
            "type": "control_request",
            "request_id": "req-3"
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::ControlRequest(req) => assert_eq!(req.subtype(), ""),
            _ => panic!("expected ControlRequest"),
        }
    }

    #[test]
    fn parse_user_message_string_content() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "hello world"
            },
            "session_id": "sess-123",
            "parent_tool_use_id": null
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::User(user) => {
                assert_eq!(user.message.role, "user");
                match &user.message.content {
                    MessageContent::Text(s) => assert_eq!(s, "hello world"),
                    _ => panic!("expected Text content"),
                }
                // Extra fields absorbed
                assert!(user.extra.contains_key("session_id"));
                assert!(user.extra.contains_key("parent_tool_use_id"));
            }
            _ => panic!("expected User, got {msg:?}"),
        }
    }

    #[test]
    fn parse_user_message_content_blocks() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "part1" },
                    { "type": "text", "text": "part2" },
                    { "type": "image", "source": "..." }
                ]
            }
        });

        let msg = StdinMessage::from_value(json);
        match msg {
            StdinMessage::User(user) => {
                let prompt = extract_prompt_text(&user.message.content);
                assert_eq!(prompt, "part1 part2");
            }
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn parse_control_response() {
        let json = serde_json::json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": "x", "response": {} }
        });
        assert!(matches!(
            StdinMessage::from_value(json),
            StdinMessage::ControlResponse
        ));
    }

    #[test]
    fn parse_keep_alive() {
        let json = serde_json::json!({ "type": "keep_alive" });
        let msg = StdinMessage::from_value(json);
        assert!(matches!(msg, StdinMessage::KeepAlive));
    }

    #[test]
    fn parse_unknown_type() {
        let json = serde_json::json!({
            "type": "some_future_type",
            "data": "whatever"
        });
        let msg = StdinMessage::from_value(json);
        assert!(matches!(msg, StdinMessage::Unknown));
    }

    #[test]
    fn parse_no_type_field() {
        let json = serde_json::json!({ "foo": "bar" });
        let msg = StdinMessage::from_value(json);
        assert!(matches!(msg, StdinMessage::Unknown));
    }

    #[test]
    fn extract_prompt_from_string() {
        let content = MessageContent::Text("do something".into());
        assert_eq!(extract_prompt_text(&content), "do something");
    }

    #[test]
    fn extract_prompt_from_blocks_text_only() {
        let content = MessageContent::Blocks(vec![
            ContentBlock {
                block_type: "text".into(),
                text: Some("hello".into()),
            },
            ContentBlock {
                block_type: "text".into(),
                text: Some("world".into()),
            },
        ]);
        assert_eq!(extract_prompt_text(&content), "hello world");
    }

    #[test]
    fn extract_prompt_from_blocks_skips_non_text() {
        let content = MessageContent::Blocks(vec![
            ContentBlock {
                block_type: "text".into(),
                text: Some("visible".into()),
            },
            ContentBlock {
                block_type: "image".into(),
                text: None,
            },
            ContentBlock {
                block_type: "text".into(),
                text: Some("also visible".into()),
            },
        ]);
        assert_eq!(extract_prompt_text(&content), "visible also visible");
    }

    #[test]
    fn extract_prompt_from_empty_blocks() {
        let content = MessageContent::Blocks(vec![]);
        assert_eq!(extract_prompt_text(&content), "");
    }

    // ---- control_response 序列化 ----

    #[test]
    fn success_response_matches_the_sdk_shape() {
        let parsed: serde_json::Value =
            serde_json::from_str(&ControlResponse::success("req-1").to_line()).unwrap();

        assert_eq!(parsed["type"], "control_response");
        assert_eq!(parsed["response"]["subtype"], "success");
        assert_eq!(parsed["response"]["request_id"], "req-1");
        assert_eq!(parsed["response"]["response"], serde_json::json!({}));
        assert!(parsed["response"].get("error").is_none());
    }

    #[test]
    fn error_response_carries_the_reason() {
        let parsed: serde_json::Value = serde_json::from_str(
            &ControlResponse::error("req-2", "unsupported control request subtype: set_model")
                .to_line(),
        )
        .unwrap();

        assert_eq!(parsed["response"]["subtype"], "error");
        assert_eq!(parsed["response"]["request_id"], "req-2");
        assert_eq!(
            parsed["response"]["error"],
            "unsupported control request subtype: set_model"
        );
        assert!(parsed["response"].get("response").is_none());
    }

    // ---- pump：静态 fixture（预置整个 stdin 内容再关掉 stdin） ----

    fn spawn_static(
        input: &str,
        interruptor: &Arc<FakeInterruptor>,
        capture: &CaptureSink,
        delivery: MessageDelivery,
    ) -> StdinPump {
        let (tx, rx) = mpsc::unbounded_channel();
        // 逐行送（空行也送：真实 stdin 的空行语义要在 fixture 里保留），
        // 送完 drop 发送端 = EOF。
        for line in input.split('\n') {
            tx.send(line.to_string()).unwrap();
        }
        drop(tx);
        spawn_with(rx, context(Arc::clone(interruptor), capture, delivery))
    }

    #[tokio::test]
    async fn pump_answers_initialize_and_delivers_prompt() {
        let input = format!(
            "{INITIALIZE}\n\n{{\"type\":\"keep_alive\"}}\n{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"say hello\"}}]}}}}\n"
        );
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(&input, &interruptor, &capture, MessageDelivery::FirstOnly);

        assert_eq!(pump.wait_prompt().await.unwrap(), "say hello");

        let lines = wait_lines(&capture, 1).await;
        assert_eq!(
            lines.len(),
            1,
            "只有 initialize 需要应答：{:?}",
            capture.text()
        );
        assert_eq!(lines[0]["type"], "control_response");
        assert_eq!(lines[0]["response"]["subtype"], "success");
        assert_eq!(lines[0]["response"]["request_id"], "init-1");
        assert_eq!(interruptor.calls(), 0);

        pump.finish().await;
    }

    #[tokio::test]
    async fn pump_reports_stdin_closed_before_prompt() {
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(
            INITIALIZE,
            &interruptor,
            &capture,
            MessageDelivery::FirstOnly,
        );

        let err = pump.wait_prompt().await.unwrap_err();
        assert!(
            err.to_string()
                .contains("stdin closed before receiving user message"),
            "{err}"
        );

        // initialize 仍然被应答了。
        let lines = wait_lines(&capture, 1).await;
        assert_eq!(lines[0]["response"]["request_id"], "init-1");

        pump.finish().await;
    }

    #[tokio::test]
    async fn pump_handles_interrupt_and_answers_success() {
        let input = format!(
            "{INITIALIZE}\n{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"go\"}}}}\n{INTERRUPT}\n"
        );
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(&input, &interruptor, &capture, MessageDelivery::FirstOnly);

        assert_eq!(pump.wait_prompt().await.unwrap(), "go");

        let lines = wait_lines(&capture, 2).await;
        assert_eq!(lines[1]["response"]["subtype"], "success");
        assert_eq!(lines[1]["response"]["request_id"], "int-1");
        assert_eq!(interruptor.calls(), 1, "interrupt 必须触发网关中断");

        pump.finish().await;
    }

    #[tokio::test]
    async fn pump_answers_success_even_when_the_interrupt_fails() {
        let input = format!("{INITIALIZE}\n{INTERRUPT}\n");
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::failing());
        let pump = spawn_static(&input, &interruptor, &capture, MessageDelivery::FirstOnly);

        let lines = wait_lines(&capture, 2).await;
        assert_eq!(lines[1]["response"]["subtype"], "success");
        assert_eq!(lines[1]["response"]["request_id"], "int-1");
        assert_eq!(interruptor.calls(), 1);

        pump.finish().await;
    }

    #[tokio::test]
    async fn pump_answers_error_for_unsupported_control_request() {
        let input = concat!(
            r#"{"type":"control_request","request_id":"m-1","request":{"subtype":"set_model","model":"x"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            "\n",
        );
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(input, &interruptor, &capture, MessageDelivery::FirstOnly);

        assert_eq!(pump.wait_prompt().await.unwrap(), "go");

        let lines = wait_lines(&capture, 1).await;
        assert_eq!(lines[0]["response"]["subtype"], "error");
        assert_eq!(lines[0]["response"]["request_id"], "m-1");
        assert!(
            lines[0]["response"]["error"]
                .as_str()
                .unwrap()
                .contains("set_model"),
            "{:?}",
            lines[0]
        );
        assert_eq!(interruptor.calls(), 0);

        pump.finish().await;
    }

    // ---- 畸形 control_request（N2）：仍要有出口 ----

    #[test]
    fn parse_malformed_control_request_keeps_the_request_id() {
        // request_id 不是字符串 → 整帧解析失败，但 id 仍要能捞出来（用于应答）。
        let json = serde_json::json!({
            "type": "control_request",
            "request_id": 42,
            "request": {"subtype": "interrupt"}
        });

        match StdinMessage::from_value(json) {
            StdinMessage::MalformedControlRequest { request_id, error } => {
                assert_eq!(request_id, Some(serde_json::json!(42)), "{error}");
            }
            other => panic!("expected MalformedControlRequest, got {other:?}"),
        }
    }

    #[test]
    fn parse_malformed_control_request_without_id() {
        let json = serde_json::json!({"type": "control_request"});
        match StdinMessage::from_value(json) {
            StdinMessage::MalformedControlRequest { request_id, .. } => {
                assert_eq!(request_id, None);
            }
            other => panic!("expected MalformedControlRequest, got {other:?}"),
        }
    }

    /// 解析失败但能捞到 id → 回显式 error（「必有应答」不变量可审计）。
    #[tokio::test]
    async fn pump_answers_error_for_a_malformed_control_request() {
        let input = concat!(
            r#"{"type":"control_request","request_id":42,"request":{"subtype":"interrupt"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            "\n",
        );
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(input, &interruptor, &capture, MessageDelivery::FirstOnly);

        assert_eq!(pump.wait_prompt().await.unwrap(), "go");

        let lines = wait_lines(&capture, 1).await;
        assert_eq!(lines[0]["response"]["subtype"], "error");
        // 原样回显：对方发的是数字 42，应答里也是数字（不是 "42"）。
        assert_eq!(lines[0]["response"]["request_id"], serde_json::json!(42));
        assert!(
            lines[0]["response"]["error"]
                .as_str()
                .unwrap()
                .contains("malformed"),
            "{:?}",
            lines[0]
        );
        // 畸形帧不触发任何中断动作。
        assert_eq!(interruptor.calls(), 0);

        pump.finish().await;
    }

    /// 连 id 都没有：只能记 warn，不产生输出、不 panic。
    #[tokio::test]
    async fn pump_logs_a_malformed_control_request_without_id() {
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let pump = spawn_static(
            r#"{"type":"control_request"}"#,
            &interruptor,
            &capture,
            MessageDelivery::Ignore,
        );

        pump.finish().await;
        assert_eq!(capture.text(), "");
        assert_eq!(interruptor.calls(), 0);
    }

    #[tokio::test]
    async fn pump_ignores_junk_keep_alive_and_control_response() {
        let input = concat!(
            "not valid json at all\n",
            "\n",
            r#"{"type":"some_future_type","data":"ignored"}"#,
            "\n",
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"x"}}"#,
            "\n",
            r#"{"type":"keep_alive"}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
        );
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(input, &interruptor, &capture, MessageDelivery::FirstOnly);

        assert_eq!(pump.wait_prompt().await.unwrap(), "hi");
        pump.finish().await;
        assert_eq!(capture.text(), "", "这些帧都不产生 stdout 输出");
    }

    #[tokio::test]
    async fn first_only_delivery_drops_everything_after_the_prompt() {
        let input = concat!(
            r#"{"type":"user","message":{"role":"user","content":"first"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"second"}}"#,
            "\n",
        );
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(input, &interruptor, &capture, MessageDelivery::FirstOnly);

        assert_eq!(pump.wait_prompt().await.unwrap(), "first");
        // 第二条既不投递也不产生输出：通道里没有第二条（EOF 已到 → None）。
        assert_eq!(pump.next_message().await, None, "第二条消息不得被投递");
        pump.finish().await;
        assert_eq!(capture.text(), "");
    }

    #[tokio::test]
    async fn all_delivery_forwards_every_message_in_order() {
        let input = concat!(
            r#"{"type":"user","message":{"role":"user","content":"one"}}"#,
            "\n",
            r#"{"type":"keep_alive"}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"two"},{"type":"text","text":"and a half"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"three"}}"#,
            "\n",
        );
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(input, &interruptor, &capture, MessageDelivery::All);

        // 常驻：每条都投递（首条也照投——由驱动侧决定它是不是 prompt）。
        assert_eq!(pump.next_message().await.as_deref(), Some("one"));
        assert_eq!(pump.next_message().await.as_deref(), Some("two and a half"));
        assert_eq!(pump.next_message().await.as_deref(), Some("three"));
        assert_eq!(pump.next_message().await, None, "EOF 收尾");
        pump.finish().await;
        assert_eq!(capture.text(), "", "user 消息不产生 stdout 输出");
    }

    /// 常驻通道是**活流**：stdin 不关，消息陆续到达即投递（轮间/轮中投递的
    /// 关键行为）。
    #[tokio::test]
    async fn all_delivery_streams_messages_while_stdin_stays_open() {
        let (sdk, rx) = mpsc::unbounded_channel();
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_with(
            rx,
            context(Arc::clone(&interruptor), &capture, MessageDelivery::All),
        );

        sdk.send(r#"{"type":"user","message":{"role":"user","content":"one"}}"#.to_string())
            .unwrap();
        assert_eq!(pump.next_message().await.as_deref(), Some("one"));

        sdk.send(r#"{"type":"user","message":{"role":"user","content":"two"}}"#.to_string())
            .unwrap();
        assert_eq!(pump.next_message().await.as_deref(), Some("two"));

        // stdin 保持张开：下一次读还没结果（不是 EOF）。
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pump.next_message())
                .await
                .is_err(),
            "stdin 未关时不得以 None 收尾"
        );

        drop(sdk);
        assert_eq!(pump.next_message().await, None);
        pump.finish().await;
    }

    #[tokio::test]
    async fn ignore_delivery_keeps_control_requests_working() {
        let input = concat!(
            r#"{"type":"user","message":{"role":"user","content":"ignored"}}"#,
            "\n",
            r#"{"type":"control_request","request_id":"init-9","request":{"subtype":"initialize"}}"#,
            "\n",
        );
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_static(input, &interruptor, &capture, MessageDelivery::Ignore);

        // 消息一条都不投递（通道立刻 EOF）……
        assert_eq!(pump.next_message().await, None);
        // ……但 control_request 的应答纪律不受影响。
        pump.finish().await;
        let lines = wait_lines(&capture, 1).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["response"]["request_id"], "init-9");
    }

    // ---- pump：活流（通道持续开着：turn 期间仍能消费 stdin） ----

    /// 一个"turn 还在跑"的场景：pump 已经在等下一行（stdin 没关），此时
    /// interrupt 到达 —— 必须被消费、触发中断、写回应答。
    #[tokio::test]
    async fn pump_serves_interrupt_while_the_turn_is_running() {
        let (sdk, rx) = mpsc::unbounded_channel();
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let mut pump = spawn_with(
            rx,
            context(
                Arc::clone(&interruptor),
                &capture,
                MessageDelivery::FirstOnly,
            ),
        );

        sdk.send(INITIALIZE.to_string()).unwrap();
        wait_lines(&capture, 1).await; // initialize 已应答

        sdk.send(r#"{"type":"user","message":{"role":"user","content":"do it"}}"#.to_string())
            .unwrap();
        assert_eq!(pump.wait_prompt().await.unwrap(), "do it");

        // turn 进行中：stdin 保持张开，interrupt 随时可能到。
        sdk.send(INTERRUPT.to_string()).unwrap();

        let lines = wait_lines(&capture, 2).await;
        assert_eq!(lines[1]["response"]["subtype"], "success");
        assert_eq!(lines[1]["response"]["request_id"], "int-1");
        assert_eq!(interruptor.calls(), 1);

        // 收尾不依赖 stdin 关闭：finish 立刻让 pump 退出。
        tokio::time::timeout(Duration::from_secs(2), pump.finish())
            .await
            .expect("finish 不能干等 stdin 关闭");
        assert_eq!(capture.text().lines().count(), 2);
    }

    /// turn 结束时 SDK 的 stdin 仍然张着（CloudCLI 会 hold 住进程）：
    /// finish 必须立即停掉 pump，而不是等到 EOF。
    #[tokio::test]
    async fn finish_stops_the_pump_without_stdin_eof() {
        let (_sdk, rx) = mpsc::unbounded_channel();
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let pump = spawn_with(
            rx,
            context(
                Arc::clone(&interruptor),
                &capture,
                MessageDelivery::FirstOnly,
            ),
        );

        tokio::time::timeout(Duration::from_secs(2), pump.finish())
            .await
            .expect("finish 不能干等 stdin 关闭");
        assert_eq!(capture.text(), "");
    }

    /// 编排器关掉 stdin（EOF）：pump 自行结束，finish 幂等收尾。
    #[tokio::test]
    async fn pump_ends_on_stdin_eof_without_prompt() {
        let (sdk, rx) = mpsc::unbounded_channel();
        let capture = CaptureSink::default();
        let interruptor = Arc::new(FakeInterruptor::default());
        let pump = spawn_with(
            rx,
            context(
                Arc::clone(&interruptor),
                &capture,
                MessageDelivery::FirstOnly,
            ),
        );

        drop(sdk); // EOF

        tokio::time::timeout(Duration::from_secs(2), pump.finish())
            .await
            .expect("EOF 后 pump 必须已结束（finish 立即返回）");
        assert_eq!(capture.text(), "");
    }

    #[tokio::test]
    async fn finish_waits_for_an_in_flight_response() {
        // interrupt 处理中（HTTP 往返挂在 fake 里）就收尾：应答必须写完才退出。
        struct SlowInterruptor;

        impl Interruptor for SlowInterruptor {
            fn interrupt(&self) -> InterruptFuture {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(())
                })
            }
        }

        let (sdk, rx) = mpsc::unbounded_channel();
        let capture = CaptureSink::default();
        let out = capture.sink();
        let pump = spawn_with(
            rx,
            StdinPumpContext {
                interruptor: Arc::new(SlowInterruptor),
                out,
                delivery: MessageDelivery::Ignore,
            },
        );

        sdk.send(INTERRUPT.to_string()).unwrap();
        // 不 sleep：立刻收尾，此时 interrupt 正在处理（100ms）。
        pump.finish().await;

        let lines = wait_lines(&capture, 1).await;
        assert_eq!(lines[0]["response"]["subtype"], "success");
        assert_eq!(lines[0]["response"]["request_id"], "int-1");
    }
}
