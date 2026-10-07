//! 官方 ACP 客户端接线：场景运行器 + 记录器 + 脚本化应答。
//!
//! 驱动选型见 06 design D2：`agent-client-protocol`（dev-dependency，`features =
//! ["process"]`）作为 client，`AcpAgent` 拉起真 `wing acp` 子进程；三个 handler：
//!
//! - `SessionNotification` → [`Recorder`]（update 的**原始 JSON**，顺序即到达顺序）；
//! - `RequestPermissionRequest` → 按 [`PermissionScript`] 应答（Bash 确认 / 回退路径）；
//! - `CreateElicitationRequest` → 按 [`ElicitationScript`] 应答（表单路径）。
//!
//! 顺序锚：前台在关键请求 resolve 之后调用 [`Recorder::marker`]——由于 SDK 在一条
//! dispatch 链上顺序处理「通知 → 响应」，marker 之前的记录就是「客户端在收到响应前
//! 已经看到」的帧（`session/load` 的回放帧序断言靠它）。

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use agent_client_protocol::AcpAgent;
use agent_client_protocol::Agent;
use agent_client_protocol::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::Error;
use agent_client_protocol::Responder;
use agent_client_protocol::on_receive_notification;
use agent_client_protocol::on_receive_request;
use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::schema::v1::CreateElicitationRequest;
use agent_client_protocol::schema::v1::CreateElicitationResponse;
use agent_client_protocol::schema::v1::ElicitationAcceptAction;
use agent_client_protocol::schema::v1::ElicitationAction;
use agent_client_protocol::schema::v1::ElicitationContentValue;
use agent_client_protocol::schema::v1::PromptRequest;
use agent_client_protocol::schema::v1::PromptResponse;
use agent_client_protocol::schema::v1::RequestPermissionOutcome;
use agent_client_protocol::schema::v1::RequestPermissionRequest;
use agent_client_protocol::schema::v1::RequestPermissionResponse;
use agent_client_protocol::schema::v1::SelectedPermissionOutcome;
use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::schema::v1::TextContent;
use serde_json::Value;

use super::SCENARIO_TIMEOUT;

// ============================================================
// 记录器
// ============================================================

/// 客户端侧观察到的记录（顺序即到达顺序）。
#[derive(Debug, Clone)]
pub enum Record {
    /// 一条 `session/update` 通知（update 序列化后的原始 JSON）。
    Update { session_id: String, update: Value },
    /// 一条 `session/request_permission` 请求（原始 JSON）。
    Permission(Value),
    /// 一条 `elicitation/create` 请求（原始 JSON）。
    Elicitation(Value),
    /// 前台插入的顺序锚（见模块文档）。
    Marker(&'static str),
}

/// 客户端侧全量记录器（handler 与场景共享）。
#[derive(Default)]
pub struct Recorder {
    entries: Mutex<Vec<Record>>,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn push(&self, record: Record) {
        self.entries.lock().expect("recorder").push(record);
    }

    pub fn marker(&self, name: &'static str) {
        self.push(Record::Marker(name));
    }

    /// 全部记录快照。
    pub fn entries(&self) -> Vec<Record> {
        self.entries.lock().expect("recorder").clone()
    }

    /// 等一条满足谓词的记录（条件轮询 + 有界超时）。
    pub async fn wait_entry(&self, what: &str, predicate: impl Fn(&Record) -> bool) -> Record {
        super::wait_until(what, || {
            self.entries().into_iter().find(|record| predicate(record))
        })
        .await
    }

    /// 该会话的全部 update（按到达顺序）。
    pub fn updates_for(&self, session_id: &str) -> Vec<Value> {
        self.entries()
            .into_iter()
            .filter_map(|record| match record {
                Record::Update {
                    session_id: recorded,
                    update,
                } if recorded == session_id => Some(update),
                _ => None,
            })
            .collect()
    }

    /// 该会话 update 的 `sessionUpdate` 序列（帧序断言的主要形状）。
    pub fn update_kinds(&self, session_id: &str) -> Vec<String> {
        self.updates_for(session_id)
            .iter()
            .map(update_kind)
            .collect()
    }

    /// 第一条 [`Record::Marker`]（`name`）**之前**的该会话 update 序列。
    pub fn updates_before_marker(&self, session_id: &str, name: &str) -> Vec<Value> {
        let mut updates = Vec::new();
        for record in self.entries() {
            match record {
                Record::Marker(marker) if marker == name => break,
                Record::Update {
                    session_id: recorded,
                    update,
                } if recorded == session_id => updates.push(update),
                _ => {}
            }
        }
        updates
    }

    /// 第一条 [`Record::Marker`]（`name`）**之后**的该会话 update 序列。
    pub fn updates_after_marker(&self, session_id: &str, name: &str) -> Vec<Value> {
        let mut seen = false;
        let mut updates = Vec::new();
        for record in self.entries() {
            match record {
                Record::Marker(marker) if marker == name => seen = true,
                Record::Update {
                    session_id: recorded,
                    update,
                } if recorded == session_id && seen => updates.push(update),
                _ => {}
            }
        }
        updates
    }

    /// 等该会话收到一条满足谓词的 update。
    pub async fn wait_update(
        &self,
        what: &str,
        session_id: &str,
        predicate: impl Fn(&Value) -> bool,
    ) -> Value {
        self.wait_entry(what, |record| match record {
            Record::Update {
                session_id: recorded,
                update,
            } => recorded == session_id && predicate(update),
            _ => false,
        })
        .await
        .into_update()
    }

    /// permission 请求（原始 JSON）列表。
    pub fn permissions(&self) -> Vec<Value> {
        self.entries()
            .into_iter()
            .filter_map(|record| match record {
                Record::Permission(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    /// 等第 `n`（1-based）条 permission 请求。
    pub async fn wait_permission(&self, what: &str, n: usize) -> Value {
        super::wait_until(what, || {
            let requests = self.permissions();
            requests.get(n - 1).cloned()
        })
        .await
    }

    /// elicitation 请求（原始 JSON）列表。
    pub fn elicitations(&self) -> Vec<Value> {
        self.entries()
            .into_iter()
            .filter_map(|record| match record {
                Record::Elicitation(request) => Some(request),
                _ => None,
            })
            .collect()
    }

    /// 等第 `n`（1-based）条 elicitation 请求。
    pub async fn wait_elicitation(&self, what: &str, n: usize) -> Value {
        super::wait_until(what, || {
            let requests = self.elicitations();
            requests.get(n - 1).cloned()
        })
        .await
    }
}

impl Record {
    fn into_update(self) -> Value {
        match self {
            Record::Update { update, .. } => update,
            other => panic!("期望一条 update 记录，得到 {other:?}"),
        }
    }
}

/// update 的 `sessionUpdate` 取值（未知形状 → `<unknown>`，便于断言失败时定位）。
pub fn update_kind(update: &Value) -> String {
    update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>")
        .to_string()
}

/// 渲染帧序，供断言失败信息（人读）。
pub fn describe_kinds(kinds: &[String]) -> String {
    kinds.join(" → ")
}

// ============================================================
// prompt 派生
// ============================================================

/// 在连接上派生一个 `session/prompt`，返回其结果通道。
///
/// 场景在等待结果期间可以继续推帧 / 发 `session/cancel`（`block_task` 直接等会阻塞
/// 前台脚本的其他动作）。
pub fn spawn_prompt(
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    text: &str,
) -> Result<tokio::sync::oneshot::Receiver<Result<PromptResponse, Error>>, Error> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let inner = connection.clone();
    let prompt = PromptRequest::new(
        session_id,
        vec![ContentBlock::Text(TextContent::new(text.to_string()))],
    );
    connection.spawn(async move {
        let result = inner.send_request(prompt).block_task().await;
        let _ = tx.send(result);
        Ok(())
    })?;
    Ok(rx)
}

// ============================================================
// 脚本化应答
// ============================================================

/// 一条 permission 应答（按到达顺序消费）。
#[derive(Debug, Clone)]
pub enum PermissionReply {
    /// 选中该 `optionId`（客户端点了某个按钮）。
    Select(String),
    /// 回 `cancelled` outcome（轮次被取消；也是脚本耗尽时的兜底）。
    Cancel,
    /// 延迟 `delay` 之后再选中该 `optionId`：模拟「轮次已被取消、用户才点按钮」的
    /// 迟到应答。发出前会往 [`Recorder`] 记一条 [`Record::Marker`]（名字见下），
    /// 供用例断言「应答确实已经发出」。
    SelectAfter { option_id: String, delay: Duration },
}

/// [`PermissionReply::SelectAfter`] 发出迟到应答时打的顺序锚。
pub const LATE_PERMISSION_MARKER: &str = "permission-answered-late";

/// 脚本化的 permission 应答队列。
#[derive(Default)]
pub struct PermissionScript {
    replies: Mutex<VecDeque<PermissionReply>>,
}

impl PermissionScript {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn push(&self, reply: PermissionReply) {
        self.replies
            .lock()
            .expect("permission script")
            .push_back(reply);
    }

    fn next(&self) -> Option<PermissionReply> {
        self.replies.lock().expect("permission script").pop_front()
    }
}

/// 一条 elicitation 应答。
#[derive(Debug, Clone)]
pub enum ElicitationReply {
    /// 表单提交（accept + content）。
    Accept(BTreeMap<String, ElicitationContentValue>),
    /// 客户端回 `-32601`（未实现该方法）——触发进程级降级。
    MethodNotFound,
}

/// 脚本化的 elicitation 应答队列。
#[derive(Default)]
pub struct ElicitationScript {
    replies: Mutex<VecDeque<ElicitationReply>>,
}

impl ElicitationScript {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn push(&self, reply: ElicitationReply) {
        self.replies
            .lock()
            .expect("elicitation script")
            .push_back(reply);
    }

    fn next(&self) -> Option<ElicitationReply> {
        self.replies.lock().expect("elicitation script").pop_front()
    }
}

// ============================================================
// 场景运行器
// ============================================================

/// 用官方 SDK 客户端跑一幕场景：三个 handler 已注册，`scenario` 在 `connect_with`
/// 前台里执行（`block_task` 可用；整体套 [`SCENARIO_TIMEOUT`]）。
pub async fn run_client(
    agent: AcpAgent,
    recorder: Arc<Recorder>,
    permissions: Arc<PermissionScript>,
    elicitations: Arc<ElicitationScript>,
    scenario: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), Error>,
) -> Result<(), Error> {
    let notifications = Arc::clone(&recorder);
    let permission_recorder = Arc::clone(&recorder);
    let elicitation_recorder = Arc::clone(&recorder);
    Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx: ConnectionTo<Agent>| {
                notifications.push(Record::Update {
                    session_id: notification.session_id.to_string(),
                    update: serde_json::to_value(&notification.update).unwrap_or(Value::Null),
                });
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest,
                        responder: Responder<RequestPermissionResponse>,
                        _cx: ConnectionTo<Agent>| {
                let raw = serde_json::to_value(&request).unwrap_or(Value::Null);
                permission_recorder.push(Record::Permission(raw.clone()));
                let response = match permissions.next() {
                    Some(PermissionReply::Select(option_id)) => {
                        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(option_id),
                        ))
                    }
                    Some(PermissionReply::SelectAfter { option_id, delay }) => {
                        tokio::time::sleep(delay).await;
                        permission_recorder.marker(LATE_PERMISSION_MARKER);
                        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(option_id),
                        ))
                    }
                    // 脚本耗尽 / 明确取消：回 cancelled outcome（最保守）。
                    Some(PermissionReply::Cancel) | None => {
                        RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                    }
                };
                responder.respond(response)
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateElicitationRequest,
                        responder: Responder<CreateElicitationResponse>,
                        _cx: ConnectionTo<Agent>| {
                elicitation_recorder.push(Record::Elicitation(
                    serde_json::to_value(&request).unwrap_or(Value::Null),
                ));
                let response = match elicitations.next() {
                    Some(ElicitationReply::Accept(content)) => CreateElicitationResponse::new(
                        ElicitationAction::Accept(ElicitationAcceptAction::new().content(content)),
                    ),
                    Some(ElicitationReply::MethodNotFound) => {
                        return responder.respond_with_error(Error::method_not_found());
                    }
                    // 脚本耗尽：拒绝（最保守，且断言会因帧内容和预期不符而失败）。
                    None => CreateElicitationResponse::new(ElicitationAction::Decline),
                };
                responder.respond(response)
            },
            on_receive_request!(),
        )
        .connect_with(agent, async move |connection: ConnectionTo<Agent>| {
            match tokio::time::timeout(SCENARIO_TIMEOUT, scenario(connection)).await {
                Ok(result) => result,
                Err(_) => Err(Error::internal_error()
                    .data(format!("e2e 场景超时（{}s）", SCENARIO_TIMEOUT.as_secs()))),
            }
        })
        .await
}
