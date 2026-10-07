//! `SessionHub` — ACP 会话表 + 网关事件泵/分流 + prompt 串行化。
//!
//! # 为什么 WS 客户端住在事件泵任务里
//!
//! [`GatewayClient`] 不可 Clone，且收事件要 `&mut self`（`recv_event`）、发消息只要
//! `&self`（`send_message`）——把引用散给多处会立刻撞上借用冲突。方案：**连接本体
//! move 进泵任务**，泵里 `tokio::select!` 同时等「WS 事件」与「出站命令」；hub 其余
//! 部分只拿一个 `mpsc::Sender<Outbound>`。
//!
//! （`select!` 在进入分支体前会 drop 其余分支的 future，所以分支体里可以对同一连接
//! 做共享借用；这是本设计的编译期前提，改动泵结构时注意别破坏它。）
//!
//! # 事件分流
//!
//! 单条 WS 连接承载所有会话的事件，按 `meta.session_id` 分流到各会话的
//! `mpsc::Sender<WingEvent>`（容量 [`EVENT_BUFFER`]，**`try_send`**——投递阻塞会
//! 反过来卡死出站队列，见 `dispatch`）。没有在途 prompt 的会话直接丢弃（ACP 的
//! `session/update` 只在轮次内有意义），事件流随 WS 断开而终止。
//!
//! # prompt 串行化
//!
//! 每个会话一把 `tokio::sync::Mutex`（gate）。[`SessionHub::begin_turn`] 先排队拿
//! gate（等前一轮终态），再装事件通道、投递用户消息；[`Turn`] 持有 gate guard 与事件
//! 接收端，Drop 时一并释放。同会话第二个 prompt 因此天然排队（不拒绝、不丢弃）。

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use agent_client_protocol::schema::v1::ClientCapabilities;
use agent_client_protocol::schema::v1::Implementation;
use agent_client_protocol::schema::v1::SessionUpdate;
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::gateway::GatewayClient;
use crate::protocol::WingEvent;
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::models::AgentOverride;
use wing_api_client::models::CreateSessionRequest;

use super::translate::ToolCards;

/// 每会话事件通道容量。
///
/// 有界是硬要求（不许无界队列）：消费端（prompt 循环）逐事件翻译并立刻发通知，正常
/// 情况下通道几乎是空的；满 = 消费端异常，丢弃并记 warn 比阻塞事件泵安全。
const EVENT_BUFFER: usize = 256;

/// 出站命令队列容量（prompt / Ask 应答 / 03 步的模型切换共用）。
const OUTBOUND_BUFFER: usize = 64;

/// 终态之后的**尾帧消化**窗口（有界、写死）。
///
/// 后端的帧序：正常 `turn_result → done`；异常 `turn_result → error → done`
/// （`react_loop.py` 的 except 分支，三帧同步连发）。前端若只消费第一个终态就把 gate
/// 还回去，排队中的下一个 prompt 会把那条尾随 `error` 当成自己的终态——故终态后必须
/// 把尾帧读干净再放 gate。正常路径只需多读一帧 `done`（微秒级）；没有尾帧的路径
/// （如 cancel）最多多等这一个窗口。
const TRAILING_FRAME_GRACE: Duration = Duration::from_millis(50);

/// 一次尾帧消化最多收集的事件数（防「停不下来的流」把内存拖走）。
const TRAILING_FRAME_MAX: usize = 64;

/// WS 断开后的收尾窗口：等客户端关连接（或超时）再退进程。
///
/// 三段收尾的顺序是语义的一部分：
///
/// 1. `fail_in_flight`：让在途 prompt 的接收端立刻结束（它们随即以 JSON-RPC error 收口）；
/// 2. `drain_pending_replies`：等每个在途 prompt 的错误**入出站队列**；
/// 3. `wait_client_closed`：等客户端关连接（或本窗口耗尽）再退出。
///
/// 第 3 步不是凑数：SDK 的「有限前台」收尾（`connect_with` 的前台 future 返回）不等
/// 物理写出——它的 `drain_outgoing` 只在成功路径/被动 EOF 路径等传输完成，进程随即
/// 退出会把刚入队的 error 帧一起带走（实测：不等就只剩客户端那边的「连接消失」，
/// 而本步验收要求「WS 断开时所有在途 prompt 回 JSON-RPC error（可诊断）」）。
/// 等客户端自己关连接是最干净的放下时机：它读到错误后大约就是这个时刻；真机上
/// 客户端每次都关，最坏情况才走满窗口。
const DRAIN_GRACE: Duration = Duration::from_secs(3);

// ============================================================
// 错误面
// ============================================================

/// hub 层的失败（调用方负责翻译成 ACP JSON-RPC error）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubError {
    /// 未知会话 id（客户端传来的 sessionId 本进程没建过）。
    UnknownSession(String),
    /// 网关拒绝了请求（HTTP 4xx/5xx、连接失败……）。
    Gateway(String),
    /// WS 事件流已断（网关进程没了 / 连接被回收）。
    Disconnected,
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSession(id) => write!(f, "unknown session: {id}"),
            Self::Gateway(detail) => write!(f, "gateway request failed: {detail}"),
            Self::Disconnected => write!(f, "gateway connection lost"),
        }
    }
}

impl std::error::Error for HubError {}

fn gateway_error(err: wing_api_client::ApiClientError) -> HubError {
    HubError::Gateway(err.to_string())
}

// ============================================================
// 出站命令
// ============================================================

/// 经 WS 发往网关的命令（泵任务消费）。
#[derive(Debug, Clone)]
enum Outbound {
    /// 投递一条用户消息：`session/prompt` 的拍平文本，或 Ask 的应答。
    ///
    /// 两条路径共用同一条 WS 出站通道（`tool_call_id` 非空 = 定向 resolve 某个
    /// feedback waiter；见 `wing.gateway.routes.ws`）。
    Message {
        session_id: String,
        content: String,
        tool_call_id: Option<String>,
    },
}

// ============================================================
// 会话条目
// ============================================================

/// 一个 wing 会话在 ACP 侧的运行态。
struct SessionEntry {
    id: String,
    /// prompt 串行化闸门（同会话第二个 prompt 排队等它）。
    gate: Arc<tokio::sync::Mutex<()>>,
    inner: Mutex<EntryState>,
}

#[derive(Default)]
struct EntryState {
    /// 当前在途 prompt 的事件接收口；`None` = 没有消费者（事件丢弃）。
    events: Option<mpsc::Sender<WingEvent>>,
    /// 会话级工具卡片记忆（跨轮次）。
    tools: ToolCards,
    /// 是否有一轮 prompt 已 armed（`request_cancel` 据此决定发不发 interrupt）。
    active: bool,
}

impl SessionEntry {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            gate: Arc::new(tokio::sync::Mutex::new(())),
            inner: Mutex::new(EntryState::default()),
        }
    }

    /// 装上本轮的事件通道并置位 active（调用方已持有 gate）。
    fn arm(&self) -> mpsc::Receiver<WingEvent> {
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        state.events = Some(tx);
        state.active = true;
        rx
    }

    /// 卸下通道、清 active 标记（轮次收口；Drop 也走这里）。
    fn disarm(&self) {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        state.events = None;
        state.active = false;
    }

    /// 事件投递：没有消费者 → 丢弃（debug）；通道满 → 丢弃 + warn。
    fn deliver(&self, event: WingEvent) -> bool {
        let sender = {
            let state = self.inner.lock().expect("entry mutex poisoned");
            state.events.clone()
        };
        let Some(sender) = sender else {
            tracing::debug!(
                session_id = %self.id,
                event_type = event.event_type(),
                "acp: event dropped (no prompt in flight on this session)"
            );
            return false;
        };
        match sender.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(event)) => {
                tracing::warn!(
                    session_id = %self.id,
                    event_type = event.event_type(),
                    "acp: event dropped (session event buffer full)"
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    fn is_active(&self) -> bool {
        self.inner.lock().expect("entry mutex poisoned").active
    }

    /// 会话级工具卡片映射（持锁期间不许 await）。
    fn updates_for(&self, event: &WingEvent) -> Vec<SessionUpdate> {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        state.tools.apply(event)
    }
}

// ============================================================
// 会话 hub
// ============================================================

/// 会话表 + 出站队列 + WS 事件泵的持有者。
///
/// 公开面是后续步骤（04 model / 05 sessions）的接入点：
///
/// | 步骤 | 用到的入口 |
/// |------|-----------|
/// | 03 | [`SessionHub::elicitation_form_supported`] / [`SessionHub::downgrade_elicitation`]（Ask 能力门控）、[`SessionHub::answer_ask`]（应答 Ask） |
/// | 04 | [`SessionHub::client_capabilities`]、[`Turn::updates_for`]，模型切换本身走 `http.update_session`（05 步的会话操作同址） |
/// | 05 | [`SessionHub::session_ids`] / [`SessionHub::knows`] / [`SessionHub::http`-级操作由 handler 直接持有 hub 完成] |
pub struct SessionHub {
    /// 网关 HTTP 客户端（建会话 / 订阅 / interrupt；后续步骤的 update/list/load 也用它）。
    pub(crate) http: GatewayApiClient,
    /// 本进程在网关侧的 client_id（订阅与事件路由的钥匙）。
    client_id: String,
    /// 出站命令队列（唯一写口是 WS 泵）。
    outbound: mpsc::Sender<Outbound>,
    /// WS 事件流终止信号（`true` = 收尾已完成，前台 future 可以退出了）。
    closed: watch::Sender<bool>,
    closed_rx: watch::Receiver<bool>,
    /// 在途 prompt 响应计数（WS 断开的收尾窗口用）。
    pending: Arc<PendingReplies>,
    /// 客户端关连接信号（收尾窗口的提前退出条件）。
    client_closed: Arc<ClientClosed>,
    /// 事件流已死（粘性）：WS 泵退出前置位，此后任何新 turn 一律立刻失败
    /// （见 [`SessionHub::begin_turn`]——绝不 arm 一个「没人投递」的 turn）。
    stream_dead: AtomicBool,
    /// 「事件流已死」的唤醒信号（与 `stream_dead` 配对：先登记再复查，见
    /// [`SessionHub::stream_dead`]）。
    stream_dead_notify: Notify,
    state: Mutex<HubState>,
}

#[derive(Default)]
struct HubState {
    sessions: HashMap<String, Arc<SessionEntry>>,
    /// `initialize` 声明的客户端能力（03 步的 elicitation / fs / terminal 门控）。
    client_capabilities: ClientCapabilities,
    /// `initialize` 声明的客户端实现（日志用）。
    client_info: Option<Implementation>,
    /// `elicitation/create` 被客户端回 `-32601`（粘性）：本进程内不再发 elicitation
    /// （见 [`SessionHub::downgrade_elicitation`]）。
    elicitation_unsupported: bool,
}

impl SessionHub {
    /// 建立 hub 并启动 WS 事件泵。
    ///
    /// 泵持有连接本体的**弱引用**（防循环引用）：hub 被丢弃 → 出站 channel 关闭 →
    /// 泵退出。
    pub fn start(ws: GatewayClient, http: GatewayApiClient, client_id: String) -> Arc<Self> {
        let (hub, outbound_rx) = Self::new_unstarted(http, client_id);
        tokio::spawn(pump(ws, Arc::downgrade(&hub), outbound_rx));
        hub
    }

    /// 建 hub 但**不**启动 WS 泵，返回（hub, 出站接收端）。
    ///
    /// 生产路径由 [`SessionHub::start`] 立刻把接收端交给泵；单测直接建 hub 时会自己
    /// 持有它（保持存活 = 出站发送仍会成功，从而把「因队列关闭而失败」与「因事件流已死
    /// 而失败」两条路径区分开）。
    fn new_unstarted(
        http: GatewayApiClient,
        client_id: String,
    ) -> (Arc<Self>, mpsc::Receiver<Outbound>) {
        let (outbound, outbound_rx) = mpsc::channel(OUTBOUND_BUFFER);
        let (closed, closed_rx) = watch::channel(false);
        let hub = Arc::new(Self {
            http,
            client_id,
            outbound,
            closed,
            closed_rx,
            pending: Arc::new(PendingReplies::default()),
            client_closed: Arc::new(ClientClosed::default()),
            stream_dead: AtomicBool::new(false),
            stream_dead_notify: Notify::new(),
            state: Mutex::new(HubState::default()),
        });
        (hub, outbound_rx)
    }

    /// 记录 `initialize` 的客户端能力与身份（03 步据此决定 elicitation 走不走）。
    pub fn register_client(&self, capabilities: ClientCapabilities, info: Option<Implementation>) {
        let mut state = self.state.lock().expect("hub mutex poisoned");
        tracing::debug!(
            client = info.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
            version = info.as_ref().map(|i| i.version.clone()).unwrap_or_default(),
            "acp: client registered"
        );
        state.client_capabilities = capabilities;
        state.client_info = info;
    }

    /// 客户端声明的能力（未 initialize 时是默认值 = 全不支持）。
    pub fn client_capabilities(&self) -> ClientCapabilities {
        self.state
            .lock()
            .expect("hub mutex poisoned")
            .client_capabilities
            .clone()
    }

    /// 客户端声明的实现信息（未 initialize 时为 None）。
    pub fn client_info(&self) -> Option<Implementation> {
        self.state
            .lock()
            .expect("hub mutex poisoned")
            .client_info
            .clone()
    }

    /// 客户端是否可用 `elicitation/create`（form 模式）—— 03 步 Ask 的门控。
    ///
    /// 判据 = `initialize` 声明了 `clientCapabilities.elicitation.form`，且本进程尚未
    /// 因 `-32601` 降级（[`SessionHub::downgrade_elicitation`]）。
    pub fn elicitation_form_supported(&self) -> bool {
        let state = self.state.lock().expect("hub mutex poisoned");
        !state.elicitation_unsupported
            && state
                .client_capabilities
                .elicitation
                .as_ref()
                .is_some_and(|elicitation| elicitation.form.is_some())
    }

    /// `elicitation/create` 被客户端回 `-32601`：**粘性**降级（本进程内不再发 elicitation，
    /// Ask 一律走回退路径）。
    ///
    /// 一个进程只服务一个 ACP 连接，所以「进程级 = 连接级 = 客户端级」；粘性是刻意的——
    /// 客户端不实现该方法就不会中途学会（omnigent 即此类）。
    pub fn downgrade_elicitation(&self) {
        let mut state = self.state.lock().expect("hub mutex poisoned");
        if !state.elicitation_unsupported {
            state.elicitation_unsupported = true;
            tracing::info!(
                "acp: client replied method-not-found to elicitation/create; falling back to per-question permissions"
            );
        }
    }

    /// 建会话：HTTP create + subscribe + 入表；返回 wing 会话 id（原样作 ACP sessionId）。
    pub async fn new_session(&self, params: NewSessionParams) -> Result<String, HubError> {
        // 事件流已死 = 这个进程再也递不出 `session/update`：不签发病入膏肓的会话，
        // 直接以可诊断错误回绝（进程本身正在收尾）。
        if self.stream_dead.load(Ordering::SeqCst) {
            return Err(HubError::Disconnected);
        }
        let request = CreateSessionRequest {
            template_name: params.template,
            workspace: Some(params.workspace.to_string_lossy().to_string()),
            // 明确不设 yolo：危险操作要在 ACP 客户端里可见（03 步映射为权限询问）。
            agent: Some(AgentOverride {
                model: params.model,
                ..Default::default()
            }),
            backend: None,
            tags: None,
        };
        let created = self
            .http
            .create_session(&request)
            .await
            .map_err(gateway_error)?;
        let session_id = created.session_id;
        self.http
            .subscribe(&session_id, &self.client_id)
            .await
            .map_err(gateway_error)?;
        self.register(&session_id);
        Ok(session_id)
    }

    /// 会话入表（返回条目）。会话 id 由网关发放，正常不会重复。
    fn register(&self, session_id: &str) -> Arc<SessionEntry> {
        let entry = Arc::new(SessionEntry::new(session_id));
        self.state
            .lock()
            .expect("hub mutex poisoned")
            .sessions
            .insert(session_id.to_string(), Arc::clone(&entry));
        entry
    }

    /// 会话条目（未知 id → [`HubError::UnknownSession`]）。
    fn entry(&self, session_id: &str) -> Result<Arc<SessionEntry>, HubError> {
        self.state
            .lock()
            .expect("hub mutex poisoned")
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| HubError::UnknownSession(session_id.to_string()))
    }

    /// 已知会话 id 快照（05 步的 list / 回放选材用）。
    pub fn session_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("hub mutex poisoned")
            .sessions
            .keys()
            .cloned()
            .collect()
    }

    /// 是否认识该会话（`session/cancel` 等通知的静默忽略判据）。
    pub fn knows(&self, session_id: &str) -> bool {
        self.state
            .lock()
            .expect("hub mutex poisoned")
            .sessions
            .contains_key(session_id)
    }

    /// 取得会话的 prompt 执行权并投递用户消息。
    ///
    /// 同会话串行化：拿 gate 会排队等前一轮终态（这是「每会话至多一个在途 prompt」的
    /// 实现）；排队期间客户端断开 / 进程收尾时任务被 drop，guard 随 [`Turn`] 一起释放，
    /// 不会有人卡死。
    pub async fn begin_turn(&self, session_id: &str, text: &str) -> Result<Turn, HubError> {
        let entry = self.entry(session_id)?;
        let gate = Arc::clone(&entry.gate).lock_owned().await;
        // 拿到 gate 之后、arm 之前检查：WS 事件流是否已经死了。
        //
        // 这一步专门收「同会话排队中的 prompt」——`fail_in_flight` 只收得掉已经 armed
        // 的条目，排队者会在前一轮还回 gate 之后重新 arm：此时泵还没退出、出站队列
        // 还在，send 会成功，于是它永远等不到事件（客户端只看到连接消失）。粘性标记
        // 让这种 turn 根本不会被 arm。
        if self.stream_dead.load(Ordering::SeqCst) {
            return Err(HubError::Disconnected);
        }
        let rx = entry.arm();
        let turn = Turn {
            session_id: session_id.to_string(),
            entry: Arc::clone(&entry),
            outbound: self.outbound.clone(),
            rx,
            _gate: gate,
        };
        if self
            .outbound
            .send(Outbound::Message {
                session_id: session_id.to_string(),
                content: text.to_string(),
                tool_call_id: None,
            })
            .await
            .is_err()
        {
            // 出站队列关闭 = 泵已退出：drop turn 收刀（卸下事件通道、放 gate）。
            return Err(HubError::Disconnected);
        }
        Ok(turn)
    }

    /// 请求取消当前轮次：异步触发网关 interrupt。
    ///
    /// 只有会话确实有在途 prompt 时才发起 HTTP interrupt——空闲期发会把网关的
    /// `interrupted` 广播留给下一个 prompt，等于凭空把它打回 cancelled。
    /// 网关随后广播 `interrupted`，在途 [`Turn`] 收到即以 `cancelled` 收口
    /// （`session/cancel` 必回 `stopReason: "cancelled"`）。
    pub fn request_cancel(self: &Arc<Self>, session_id: &str) {
        let Ok(entry) = self.entry(session_id) else {
            tracing::debug!(session_id, "acp: cancel for an unknown session; ignored");
            return;
        };
        if !entry.is_active() {
            tracing::debug!(session_id, "acp: cancel with no prompt in flight; ignored");
            return;
        }
        let hub = Arc::clone(self);
        let sid = session_id.to_string();
        tokio::spawn(async move {
            if let Err(err) = hub.http.interrupt_session(&sid).await {
                tracing::warn!(session_id = %sid, error = %err, "acp: interrupt failed");
            }
        });
    }

    /// 应答 Ask（经 WS 出站队列，定向 resolve feedback waiter）。
    ///
    /// 03 步的正式实现也从这里走（答案格式：`header: answer` 逐行 / 多选逗号 /
    /// 未答占位 / 取消哨兵——见 `shared::panels::ask` 的契约）。
    pub async fn answer_ask(
        &self,
        session_id: &str,
        tool_call_id: &str,
        content: &str,
    ) -> Result<(), HubError> {
        if !self.knows(session_id) {
            return Err(HubError::UnknownSession(session_id.to_string()));
        }
        self.outbound
            .send(Outbound::Message {
                session_id: session_id.to_string(),
                content: content.to_string(),
                tool_call_id: Some(tool_call_id.to_string()),
            })
            .await
            .map_err(|_| HubError::Disconnected)
    }

    /// 事件分流（WS 泵调用）：按 `meta.session_id` 投递到对应会话。
    fn dispatch(&self, event: WingEvent) {
        let Some(session_id) = event.session_id().map(str::to_string) else {
            // 全局事件（无 session_id）：ACP 侧没有对应物。
            tracing::debug!(event_type = event.event_type(), "acp: global event dropped");
            return;
        };
        let entry = self
            .entry(&session_id)
            .inspect_err(|_| {
                tracing::debug!(
                    session_id,
                    event_type = event.event_type(),
                    "acp: event for an unknown session dropped"
                );
            })
            .ok();
        if let Some(entry) = entry {
            entry.deliver(event);
        }
    }

    /// WS 事件流结束的第一步：让所有在途 prompt 的接收端立刻结束
    /// （它们随即以 JSON-RPC error 收口，见 `agent::run_turn`）。
    fn fail_in_flight(&self) {
        let sessions: Vec<Arc<SessionEntry>> = self
            .state
            .lock()
            .expect("hub mutex poisoned")
            .sessions
            .values()
            .cloned()
            .collect();
        for entry in sessions {
            entry.disarm();
        }
    }

    /// 等在途 prompt 的响应入队（有界）；返回是否已收干净。
    async fn drain_pending_replies(&self, timeout: Duration) -> bool {
        self.pending.wait_idle(timeout).await
    }

    /// 客户端关连接（`agent::serve` 的 watcher 任务调用；幂等）。
    pub fn note_client_closed(&self) {
        self.client_closed.note();
    }

    /// 等客户端关连接，或等到窗口耗尽（收尾用，见 [`DRAIN_GRACE`]）。
    async fn wait_client_closed(&self, timeout: Duration) {
        self.client_closed.wait(timeout).await;
    }

    /// 取得在途响应凭据（prompt handler 用；应答之后释放，见 [`PendingReply`]）。
    pub fn pending_reply(&self) -> PendingReply {
        PendingReply {
            _guard: self.pending.acquire(),
        }
    }

    /// WS 事件流结束的最后一步：广播 closed（前台 future 据此退出进程）。
    fn mark_closed(&self) {
        // `send_replace`：即使没有等待者也要把状态留在 watch 里（`closed()` 先查状态）。
        self.closed.send_replace(true);
    }

    /// WS 事件流终止（`SessionHub::start` 之后随时可能触发）。
    pub async fn closed(&self) {
        let mut rx = self.closed_rx.clone();
        if *rx.borrow() {
            return;
        }
        let _ = rx.wait_for(|closed| *closed).await;
    }

    /// 置「事件流已死」并唤醒等待者（泵进入收尾的第一步）。
    fn mark_stream_dead(&self) {
        self.stream_dead.store(true, Ordering::SeqCst);
        self.stream_dead_notify.notify_waiters();
    }

    /// 等「事件流已死」（粘性；已死则立刻返回）。
    ///
    /// 在途 ask 等待客户端作答时用它做 `select!` 的另一臂（见 `agent::run_turn`）：
    /// WS 一断就（用默认答案）收口，让轮次走到既有的「gateway event stream ended」错误
    /// 分支——否则 `session/prompt` 会一直卡在客户端请求上，只以连接消失告终
    /// （review r1 N-1）。
    pub async fn stream_dead(&self) {
        loop {
            // 先登记再复查：漏掉「登记与检查之间刚置位」的窗口。
            let notified = self.stream_dead_notify.notified();
            if self.stream_dead.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

// ============================================================
// WS 事件泵
// ============================================================

/// WS 收事件 + 出站命令的唯一执行者（见模块文档：连接不可 Clone）。
async fn pump(
    mut ws: GatewayClient,
    hub: Weak<SessionHub>,
    mut outbound: mpsc::Receiver<Outbound>,
) {
    loop {
        tokio::select! {
            event = ws.recv_event() => {
                match event {
                    Some(event) => match hub.upgrade() {
                        Some(hub) => hub.dispatch(event),
                        // hub 已释放（进程正在收尾）：停止消费。
                        None => break,
                    },
                    None => {
                        tracing::warn!("acp: gateway event stream ended");
                        break;
                    }
                }
            }
            command = outbound.recv() => {
                match command {
                    Some(Outbound::Message { session_id, content, tool_call_id }) => {
                        let request_id = crate::protocol::generate_request_id();
                        if let Err(err) = ws
                            .send_message(&session_id, &content, tool_call_id, request_id)
                            .await
                        {
                            tracing::warn!(session_id = %session_id, error = %err, "acp: send_message failed");
                        }
                    }
                    // hub 被丢弃 = 进程收尾。
                    None => break,
                }
            }
        }
    }
    if let Some(hub) = hub.upgrade() {
        // 收尾四步（顺序是语义的一部分，见 DRAIN_GRACE 与 stream_dead）：
        // 0. 置「事件流已死」（粘性）：此后新 turn 一律立刻失败，不再 arm；
        //    在途 ask 的等待也随之醒来（见 [`SessionHub::stream_dead`]）；
        // 1. 让在途 prompt 立刻失败（接收端关闭）；
        // 2. 等它们把 JSON-RPC error 交给出站队列（有界）；
        // 3. 等客户端关连接（有界），再广播 closed 让前台 future 退出（退出码 1）。
        hub.mark_stream_dead();
        hub.fail_in_flight();
        if !hub.drain_pending_replies(DRAIN_GRACE).await {
            tracing::warn!(
                timeout_s = DRAIN_GRACE.as_secs(),
                "acp: in-flight prompts did not drain in time; exiting anyway"
            );
        }
        // 让错误帧真的写到客户端再退：SDK 的「有限前台」收尾不等物理写出
        // （见 DRAIN_GRACE 的说明），等客户端关连接（或最坏再等 DRAIN_GRACE）。
        hub.wait_client_closed(DRAIN_GRACE).await;
        hub.mark_closed();
    }
}

// ============================================================
// 一轮 prompt
// ============================================================

/// `session/new` 的入参（进程级默认值 + 客户端给的 cwd）。
#[derive(Debug, Clone)]
pub struct NewSessionParams {
    /// 工作目录（会话 workspace；必须绝对路径且存在）。
    pub workspace: PathBuf,
    /// 会话模板名（`--agent`）。
    pub template: Option<String>,
    /// 初始模型覆盖（`--model`）。
    pub model: Option<String>,
}

// ============================================================
// 在途响应凭据
// ============================================================

/// 在途 prompt 响应的计数器：WS 断开的收尾窗口等它归零，才广播 `closed` 退进程。
///
/// 计的是**响应凭据**而不是 turn：turn 在 `run_turn` 返回时就 drop 了，而响应还要
/// 多走一步（`responder.respond_with_error` 把帧送进出站队列）。凭据由 prompt
/// handler 在应答之后释放，于是「归零」= 每个在途 prompt 的错误都已入队；收尾再
/// 等客户端关连接（见 [`DRAIN_GRACE`]）才会退出——SDK 的有限前台收尾不等物理写出，
/// 进程退得太早会把这些帧一起带走。
#[derive(Default)]
struct PendingReplies {
    count: AtomicUsize,
    idle: Notify,
}

impl PendingReplies {
    fn acquire(self: &Arc<Self>) -> PendingReplyGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        PendingReplyGuard(Arc::clone(self))
    }

    fn is_idle(&self) -> bool {
        self.count.load(Ordering::SeqCst) == 0
    }

    /// 等到没有在途响应（或有界超时）；返回是否已收干净。
    async fn wait_idle(&self, timeout: Duration) -> bool {
        tracing::debug!(
            pending = self.count.load(Ordering::SeqCst),
            "acp: waiting for in-flight prompt replies"
        );
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // 先注册再复查：漏掉「注册与检查之间刚递减」的窗口。
            let notified = self.idle.notified();
            if self.is_idle() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.is_idle();
            }
        }
    }
}

struct PendingReplyGuard(Arc<PendingReplies>);

impl Drop for PendingReplyGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// 在途 prompt 的响应凭据（见 [`PendingReplies`]）：handler 取得，**应答之后**释放。
#[must_use]
pub struct PendingReply {
    _guard: PendingReplyGuard,
}

/// 客户端（ACP 连接）关连接的通知：收尾窗口据此尽早退出（见 [`DRAIN_GRACE`]）。
#[derive(Default)]
struct ClientClosed {
    closed: AtomicBool,
    notify: Notify,
}

impl ClientClosed {
    fn note(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn wait(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.notify.notified();
            if self.is_closed() {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }
}

/// 一轮在途 prompt：事件接收端 + gate guard + 会话级工具卡片状态。
///
/// Drop = 收口（卸下事件通道、释放 gate，让排队的下一个 prompt 继续；在途计数递减）。
pub struct Turn {
    session_id: String,
    entry: Arc<SessionEntry>,
    outbound: mpsc::Sender<Outbound>,
    rx: mpsc::Receiver<WingEvent>,
    /// 持有到轮次结束：同会话第二个 prompt 在 `begin_turn` 里排队等它。
    _gate: OwnedMutexGuard<()>,
}

impl Turn {
    /// 本轮的 ACP sessionId（= wing 会话 id）。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 下一条事件；`None` = 事件流已终止（WS 断开 / hub 收尾）。
    pub async fn next_event(&mut self) -> Option<WingEvent> {
        self.rx.recv().await
    }

    /// 事件 → ACP update 列表（会话级工具卡片状态在内部落地）。
    pub fn updates_for(&self, event: &WingEvent) -> Vec<SessionUpdate> {
        self.entry.updates_for(event)
    }

    /// 消化本轮**终态之后**的尾帧（有界，见 [`TRAILING_FRAME_GRACE`]）。
    ///
    /// 必须在 `Turn` drop（= gate 释放）之前调用：后端的异常帧序
    /// `turn_result → error → done` 里，那条 `error` 若是留给下一个排队 prompt 去读，
    /// 它会被当成新轮次的终态（review_r1 S3 实测复现）。
    ///
    /// 返回窗口内收到的尾帧（已从通道取走）；`done`（正常路径的尾帧）到达即停，
    /// 通道关闭或预算耗尽同样停——总耗时不超过 [`TRAILING_FRAME_GRACE`]。
    pub async fn drain_trailing(&mut self) -> Vec<WingEvent> {
        let deadline = tokio::time::Instant::now() + TRAILING_FRAME_GRACE;
        let mut trailing = Vec::new();
        while trailing.len() < TRAILING_FRAME_MAX {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(event)) => {
                    let is_done = matches!(event, WingEvent::Done { .. });
                    trailing.push(event);
                    if is_done {
                        break;
                    }
                }
                // 通道关闭（WS 断开）或窗口耗尽：本轮不会再有意义事件。
                Ok(None) | Err(_) => break,
            }
        }
        if !trailing.is_empty() {
            tracing::debug!(
                count = trailing.len(),
                kinds = ?trailing.iter().map(WingEvent::event_type).collect::<Vec<_>>(),
                "acp: drained trailing frames after the terminal event"
            );
        }
        trailing
    }

    /// 应答一个 Ask（占位实现与 03 步的正式实现共用这条路径）。
    pub async fn answer_ask(&self, tool_call_id: &str, content: &str) -> Result<(), HubError> {
        self.outbound
            .send(Outbound::Message {
                session_id: self.session_id.clone(),
                content: content.to_string(),
                tool_call_id: Some(tool_call_id.to_string()),
            })
            .await
            .map_err(|_| HubError::Disconnected)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        self.entry.disarm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ElicitationCapabilities;
    use agent_client_protocol::schema::v1::ElicitationFormCapabilities;
    use serde_json::json;

    fn event(session_id: &str) -> WingEvent {
        serde_json::from_value(json!({
            "type": "text",
            "content": "hi",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": session_id,
            "request_id": "req-1",
        }))
        .expect("fixture decodes")
    }

    /// 任意事件类型的最小 fixture（只补公共字段）。
    fn event_of(kind: &str, session_id: &str) -> WingEvent {
        serde_json::from_value(json!({
            "type": kind,
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": session_id,
            "request_id": "req-1",
        }))
        .unwrap_or_else(|err| panic!("fixture {kind} must decode: {err}"))
    }

    /// 测试用 hub：不启泵（事件流由测试自己投递），HTTP 客户端只构造不连网。
    ///
    /// 出站接收端由调用方持有并保持存活——否则「发送失败」会来自队列关闭，
    /// 掩盖我们要断言的那条路径（事件流已死）。
    fn test_hub() -> (Arc<SessionHub>, mpsc::Receiver<Outbound>) {
        let http = GatewayApiClient::new("http://127.0.0.1:9", None)
            .expect("reqwest client builds without a server");
        SessionHub::new_unstarted(http, "client-test".into())
    }

    #[test]
    fn entry_without_consumer_drops_events() {
        let entry = SessionEntry::new("s1");
        assert!(!entry.is_active());
        assert!(!entry.deliver(event("s1")), "no consumer → dropped");
    }

    #[test]
    fn armed_entry_delivers_events_in_order() {
        let entry = SessionEntry::new("s1");
        let mut rx = entry.arm();
        assert!(entry.is_active());
        assert!(entry.deliver(event("s1")));
        assert!(entry.deliver(event("s1")));
        assert_eq!(rx.try_recv().expect("first event").event_type(), "text");
        assert_eq!(rx.try_recv().expect("second event").event_type(), "text");
        entry.disarm();
        assert!(!entry.is_active());
        assert!(!entry.deliver(event("s1")), "disarmed → dropped again");
    }

    #[test]
    fn disarm_releases_queued_events_with_the_receiver() {
        let entry = SessionEntry::new("s1");
        let rx = entry.arm();
        entry.deliver(event("s1"));
        drop(rx);
        // 接收端没了 → 投递退化为「无消费者」，不 panic。
        assert!(!entry.deliver(event("s1")));
    }

    #[test]
    fn tool_state_survives_between_turns_of_the_same_entry() {
        let entry = SessionEntry::new("s1");
        // 第一轮：创建卡片。
        let stream: WingEvent = serde_json::from_value(json!({
            "type": "tool_call_stream",
            "tool_call_id": "tc1",
            "tool_name": "Edit",
            "created_at": "c", "session_id": "s1", "request_id": "r",
        }))
        .expect("fixture decodes");
        assert_eq!(entry.updates_for(&stream).len(), 1);
        // 第二轮（同一会话条目）：同一 id 不再创建。
        assert!(entry.updates_for(&stream).is_empty());
    }

    /// N5：事件通道满 → 丢弃（绝不阻塞事件泵）。
    #[test]
    fn a_full_event_channel_drops_instead_of_blocking() {
        let entry = SessionEntry::new("s1");
        let mut rx = entry.arm();
        for i in 0..EVENT_BUFFER {
            assert!(entry.deliver(event("s1")), "buffered event {i} must fit");
        }
        assert!(
            !entry.deliver(event("s1")),
            "a full buffer must drop (warn) rather than block the pump"
        );
        // 消费一条之后又能投递。
        rx.try_recv().expect("one buffered event");
        assert!(entry.deliver(event("s1")));
    }

    /// N-1：`stream_dead()` 是给在途 ask 的等待信号——置位前不返回、置位后立刻返回。
    #[tokio::test]
    async fn stream_dead_signal_wakes_waiters_and_is_sticky() {
        let (hub, _outbound_rx) = test_hub();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), hub.stream_dead())
                .await
                .is_err(),
            "还活着的事件流不该唤醒等待者"
        );

        hub.mark_stream_dead();
        tokio::time::timeout(Duration::from_millis(20), hub.stream_dead())
            .await
            .expect("置位后立刻返回");

        // 粘性：后来的等待者（以及重复调用）立刻返回。
        hub.mark_stream_dead();
        tokio::time::timeout(Duration::from_millis(20), hub.stream_dead())
            .await
            .expect("已死是粘性状态");
    }

    /// S1：事件流已死时，新一轮 prompt 立刻失败——绝不 arm 一个「没人投递」的 turn。
    #[tokio::test]
    async fn turn_on_a_dead_stream_fails_fast() {
        let (hub, _outbound_rx) = test_hub();
        let entry = hub.register("s1");

        // 对照组：健康的事件流正常 arm 并正常收刀。
        let turn = hub
            .begin_turn("s1", "hi")
            .await
            .expect("a healthy stream arms");
        assert_eq!(turn.session_id(), "s1");
        assert!(entry.is_active());
        drop(turn);
        assert!(!entry.is_active());

        // WS 断开（pump 退出前置位）之后：
        hub.mark_stream_dead();
        let err = match hub.begin_turn("s1", "hi").await {
            Ok(_) => panic!("a dead stream must fail fast"),
            Err(err) => err,
        };
        assert_eq!(err, HubError::Disconnected);
        assert!(
            !entry.is_active(),
            "绝不能留下 armed 却无人投递的 turn（客户端会只看到连接消失）"
        );

        // 同一个标记也挡住「签发永远收不到事件的会话」。
        let err = hub
            .new_session(NewSessionParams {
                workspace: PathBuf::from("/tmp"),
                template: None,
                model: None,
            })
            .await
            .expect_err("session/new on a dead stream must fail fast");
        assert_eq!(err, HubError::Disconnected);
    }

    /// S3：终态之后的尾帧必须被本轮消化掉，不能留给下一个排队 prompt。
    #[tokio::test]
    async fn trailing_terminal_frames_are_drained_before_the_gate_opens() {
        let (hub, _outbound_rx) = test_hub();
        let entry = hub.register("s1");
        let mut turn = hub.begin_turn("s1", "hi").await.expect("turn arms");

        // 后端异常帧序：turn_result → error → done（三帧连发）。
        entry.deliver(event_of("turn_result", "s1"));
        let first = turn.next_event().await.expect("terminal frame");
        assert_eq!(first.event_type(), "turn_result");
        entry.deliver(
            serde_json::from_value(json!({
                "type": "error",
                "message": "处理消息失败：异常：boom",
                "created_at": "2026-01-01T00:00:00+00:00",
                "session_id": "s1",
                "request_id": "req-1",
            }))
            .expect("error fixture decodes"),
        );
        entry.deliver(event_of("done", "s1"));

        let trailing = turn.drain_trailing().await;
        assert!(
            trailing.iter().any(|e| e.event_type() == "error"),
            "尾随 error 必须被本轮取走：{trailing:?}"
        );
        assert!(
            trailing.iter().any(|e| e.event_type() == "done"),
            "`done` 到达即停（正常路径只多读一帧）"
        );

        // 收刀之后：下一个 prompt 重新 arm，通道里不允许有残帧。
        drop(turn);
        assert!(!entry.is_active());
        let mut next = entry.arm();
        assert!(
            matches!(next.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "上一个轮次的尾帧不允许留给下一个 prompt"
        );
    }

    /// S3：没有尾帧时（如 cancel 路径）窗口有界——不能无限等。
    #[tokio::test]
    async fn drain_trailing_is_bounded_when_nothing_follows() {
        let (hub, _outbound_rx) = test_hub();
        hub.register("s1");
        let mut turn = hub.begin_turn("s1", "hi").await.expect("turn arms");

        let started = tokio::time::Instant::now();
        let trailing = turn.drain_trailing().await;
        let elapsed = started.elapsed();
        assert!(trailing.is_empty());
        assert!(
            elapsed >= TRAILING_FRAME_GRACE,
            "静默窗口没等满就返回了？elapsed={elapsed:?}"
        );
        assert!(
            elapsed < TRAILING_FRAME_GRACE * 4,
            "窗口必须写死有界：elapsed={elapsed:?}"
        );
    }

    #[test]
    fn hub_error_display_is_human_readable() {
        assert_eq!(
            HubError::UnknownSession("s1".into()).to_string(),
            "unknown session: s1"
        );
        assert_eq!(
            HubError::Gateway("500".into()).to_string(),
            "gateway request failed: 500"
        );
        assert_eq!(
            HubError::Disconnected.to_string(),
            "gateway connection lost"
        );
    }

    /// 03：elicitation 门控 = 声明能力 + 未被 `-32601` 降级（降级粘性）。
    #[test]
    fn elicitation_capability_is_gated_and_sticky() {
        let (hub, _outbound_rx) = test_hub();
        // 未 initialize / 未声明 → 不支持（默认保守）。
        assert!(!hub.elicitation_form_supported());
        hub.register_client(ClientCapabilities::new(), None);
        assert!(!hub.elicitation_form_supported());

        // 声明 form 能力 → 支持（omnigent 那种「声明了 elicitation 但没有 form」也算不支持）。
        hub.register_client(
            ClientCapabilities::new().elicitation(ElicitationCapabilities::new()),
            None,
        );
        assert!(!hub.elicitation_form_supported());
        hub.register_client(
            ClientCapabilities::new().elicitation(
                ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
            ),
            None,
        );
        assert!(hub.elicitation_form_supported());

        // `-32601` 降级：粘性——即便能力仍在声明里也不再试探；重复降级幂等。
        hub.downgrade_elicitation();
        assert!(!hub.elicitation_form_supported());
        hub.downgrade_elicitation();
        assert!(!hub.elicitation_form_supported());
        hub.register_client(
            ClientCapabilities::new().elicitation(
                ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
            ),
            None,
        );
        assert!(
            !hub.elicitation_form_supported(),
            "降级是进程级粘性状态，重新 initialize 不该复活它"
        );
    }
}
