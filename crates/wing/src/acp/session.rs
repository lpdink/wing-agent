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
//!
//! # 挂载（session/load · session/resume）
//!
//! 已存在的会话经 [`SessionHub::attach_and_subscribe`] 挂载：入表（幂等）→ 取 gate →
//! arm 事件接收端 → subscribe。**arm 必须早于 subscribe**（订阅会立刻推一份
//! `sync_session` 快照，晚 arm 就没人接）——两条动作封在同一个方法里，顺序写不坏。
//! [`Attached`] 的 Drop = 卸载（disarm，回到空闲语义）；[`SessionHub::close_session`]
//! 负责回收条目与卡片记忆（幂等）。

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

use agent_client_protocol::Client;
use agent_client_protocol::ConnectionTo;
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

/// 挂载（`session/load` / `session/resume`）后等 `sync_session` 快照的预算。
///
/// 订阅 → 网关推快照是本地 HTTP + WS 的往返，正常毫秒级；窗口只用来兜「网关没推 /
/// 事件丢了」，超时以可诊断错误收口（绝不让客户端无限等）。
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// `session/close` 等活跃轮次收口的预算（见 [`SessionHub::close_session`]）。
const CLOSE_GRACE: Duration = Duration::from_secs(5);

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
    /// 有界等待超时（回放快照 / 关闭收口）；字段是给用户看的诊断。
    Timeout(String),
}

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSession(id) => write!(f, "unknown session: {id}"),
            Self::Gateway(detail) => write!(f, "gateway request failed: {detail}"),
            Self::Disconnected => write!(f, "gateway connection lost"),
            Self::Timeout(detail) => write!(f, "timed out: {detail}"),
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
    /// 当前 armed 的事件接收口；`None` = 没有消费者（事件丢弃）。
    events: Option<mpsc::Sender<WingEvent>>,
    /// 会话级工具卡片记忆（跨轮次）。
    tools: ToolCards,
    /// 有一轮 **prompt** 已 armed：`session/cancel` / `close` 据此决定发不发 interrupt
    /// （05 审查 N3：挂载窗口没有后端轮次，对空闲会话 interrupt 是多余行为）。
    turn_armed: bool,
    /// 有一次**挂载**（`session/load` / `session/resume`）已 armed：同会话 prompt 排队等它，
    /// `close` 也要等它收口——但它**不是**在途轮次（不发 interrupt）。
    attach_armed: bool,
    /// 模型变更中继在途（04）：同一会话同时只有一次在中继（见 [`SessionEntry::claim_model_relay`]）。
    model_relay_in_flight: bool,
    /// 中继在途期间又收到模型变更（收尾时再跑一次，合并成「以最新状态为准」）。
    model_relay_dirty: bool,
}

impl SessionEntry {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            gate: Arc::new(tokio::sync::Mutex::new(())),
            inner: Mutex::new(EntryState::default()),
        }
    }

    /// 装上事件通道（调用方已持有 gate；armed 标记由 [`SessionEntry::arm_turn`] /
    /// [`SessionEntry::arm_attached`] 各自置位）。
    fn install_events(state: &mut EntryState) -> mpsc::Receiver<WingEvent> {
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        state.events = Some(tx);
        rx
    }

    /// 装上一轮 **prompt** 的事件通道并置位 `turn_armed`（调用方已持有 gate）。
    fn arm_turn(&self) -> mpsc::Receiver<WingEvent> {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        let rx = Self::install_events(&mut state);
        state.turn_armed = true;
        rx
    }

    /// 装上一次**挂载**的事件通道并置位 `attach_armed`（调用方已持有 gate）。
    fn arm_attached(&self) -> mpsc::Receiver<WingEvent> {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        let rx = Self::install_events(&mut state);
        state.attach_armed = true;
        rx
    }

    /// 卸下通道、清 armed 标记（轮次 / 挂载收口；Drop 与回收路径也走这里）。
    fn disarm(&self) {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        state.events = None;
        state.turn_armed = false;
        state.attach_armed = false;
    }

    /// 是否有**在途轮次**（`request_cancel` 的判据；挂载期不算）。
    fn has_turn_in_flight(&self) -> bool {
        self.inner.lock().expect("entry mutex poisoned").turn_armed
    }

    /// 是否有 armed 的消费者（轮次或挂载）——`close` 的等待判据。
    fn is_armed(&self) -> bool {
        let state = self.inner.lock().expect("entry mutex poisoned");
        state.turn_armed || state.attach_armed
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

    /// 模型变更中继的去重/合并协议（04，见 `model::relay_model_change`）：
    /// 返回 true = 现在没有在途中继，调用方**负责跑一次**；false = 已有在途，
    /// 本次变更被记成「积压」（收尾时由 [`SessionEntry::finish_model_relay`] 再跑一次）。
    fn claim_model_relay(&self) -> bool {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        if state.model_relay_in_flight {
            state.model_relay_dirty = true;
            false
        } else {
            state.model_relay_in_flight = true;
            true
        }
    }

    /// 中继收尾：期间又收到变更 → true（调用方再跑一次，重取权威状态）；否则清在途标记。
    fn finish_model_relay(&self) -> bool {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        if state.model_relay_dirty {
            state.model_relay_dirty = false;
            true
        } else {
            state.model_relay_in_flight = false;
            false
        }
    }

    /// 会话级工具卡片映射（持锁期间不许 await）。
    fn updates_for(&self, event: &WingEvent) -> Vec<SessionUpdate> {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        state.tools.apply(event)
    }

    /// 历史回放投影：`sync_session` 快照 → update 序列（持锁期间不许 await）。
    ///
    /// 投影写进**本会话的同一份**卡片记忆：回放建好的卡片，后续实时
    /// `diff_content` / `tool_call_stream` 能继续锚定（见 `translate::replay`）。
    fn replay_updates(&self, snapshot: &WingEvent) -> Vec<SessionUpdate> {
        let mut state = self.inner.lock().expect("entry mutex poisoned");
        super::translate::replay::replay_updates(&mut state.tools, snapshot)
    }
}

// ============================================================
// 会话 hub
// ============================================================

/// 会话表 + 出站队列 + WS 事件泵的持有者。
///
/// 公开面（03/04/05 的接入点）：
///
/// | 步骤 | 用到的入口 |
/// |------|-----------|
/// | 03 | [`SessionHub::elicitation_form_supported`] / [`SessionHub::downgrade_elicitation`]（Ask 能力门控）、[`SessionHub::answer_ask`]（应答 Ask） |
/// | 04 | [`SessionHub::register_client_connection`] / [`SessionHub::client_connection`]（中继的出站）、[`SessionHub::finish_model_relay`]（中继收尾）；模型切换本身走 `http.update_session`，触发点在 [`SessionHub::dispatch`]（**不**经 `Turn::updates_for`，见 design D7） |
/// | 05 | [`SessionHub::attach_and_subscribe`] / [`SessionHub::close_session`]（load/resume 挂载 + 回收） |
/// | 05+ | [`SessionHub::session_ids`] / [`SessionHub::knows`]（会话查询） |
pub struct SessionHub {
    /// 网关 HTTP 客户端（建会话 / 订阅 / interrupt；update/list/load 也用它）。
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
    /// ACP 客户端连接句柄（`agent::serve` 的 `connect_with` 注册）。
    ///
    /// 04 的模型变更中继从**事件分流**路径发 `config_option_update`——那条路径没有 handler
    /// 的 `cx`，所以连接得在 hub 里留一份。一个进程只服务一个 ACP 连接（stdio 传输性质），
    /// 一份就够（同前提见 03 的能力门控）。
    client: Mutex<Option<ConnectionTo<Client>>>,
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
            client: Mutex::new(None),
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

    /// 注册 ACP 客户端连接（`agent::serve` 在 `connect_with` 里调用；重复注册覆盖）。
    ///
    /// 只在**事件分流**路径里用：04 的模型变更中继要从 `dispatch` 发
    /// `config_option_update`，而那里没有 handler 的 `cx`（见 [`SessionHub::client_connection`]）。
    pub fn register_client_connection(&self, client: ConnectionTo<Client>) {
        *self.client.lock().expect("hub mutex poisoned") = Some(client);
    }

    /// 客户端连接句柄（未注册时为 None）。
    ///
    /// 一个进程只服务一个 ACP 连接：这一份是全部。`initialize` 之前的 `session/new` 不可能
    /// 发生（协议先 initialize），所以注册时机不构成竞态。
    pub fn client_connection(&self) -> Option<ConnectionTo<Client>> {
        self.client.lock().expect("hub mutex poisoned").clone()
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

    /// 已知会话 id 快照（诊断用；`session/list` 的数据源是网关的 `/api/session/list`，
    /// 不是本表——本表只有本进程服务过的会话）。
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
        let rx = entry.arm_turn();
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
    /// 只有会话确实有**在途 prompt** 时才发起 HTTP interrupt——空闲期发会把网关的
    /// `interrupted` 广播留给下一个 prompt，等于凭空把它打回 cancelled。挂载窗口
    /// （`session/load` / `session/resume`）同样没有后端轮次，也不发（05 审查 N3）。
    /// 网关随后广播 `interrupted`，在途 [`Turn`] 收到即以 `cancelled` 收口
    /// （`session/cancel` 必回 `stopReason: "cancelled"`）。
    pub fn request_cancel(self: &Arc<Self>, session_id: &str) {
        let Ok(entry) = self.entry(session_id) else {
            tracing::debug!(session_id, "acp: cancel for an unknown session; ignored");
            return;
        };
        if !entry.has_turn_in_flight() {
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

    // ============================================================
    // 挂载与回收（session/load · session/resume · session/close）
    // ============================================================

    /// 挂载与订阅一个**已存在**的会话（`session/load` / `session/resume` 的共同入口）：
    /// 入表（幂等）→ 取 gate → arm 事件接收端 → subscribe。
    ///
    /// **arm 必须早于 subscribe**：网关的订阅会立刻推一份 `sync_session` 快照，arm 晚
    /// 一步就没有消费者，快照被丢弃（回放无从谈起）。两条动作封在一个方法里，顺序不
    /// 会写错。
    ///
    /// 订阅失败 → 回滚（只回收**本次新建**的条目；见下）并报错——不留一个「在表但
    /// 收不到事件」的会话（那种会话的 prompt 会静默挂起）。
    ///
    /// 回滚的两个边界（05 审查 N1/N2）：
    ///
    /// - **只回收本次新建的条目**：重复挂载一个既有会话时，条目（含卡片记忆）与网关侧
    ///   订阅本来健康，一次失败的重复挂载不该把它们删掉；
    /// - **best-effort 撤销网关侧订阅**（只对新建条目）：`POST /api/session/subscribe`
    ///   是「先建路由后应答」（见 `routes/session.py`），响应丢失时网关仍认为本 client
    ///   订阅着该会话——不撤销会把会话钉在网关内存里（release 被 409 拒绝、reaper 逐出
    ///   不了）。既有条目的订阅**不能**撤（那是它的健康订阅）。
    ///
    /// 取 gate 会排队等同会话的在途轮次 / 上一次挂载收口（复用 `Turn` 的串行化语义）：
    /// 挂载期间 armed，同会话的 `session/prompt` 也排队等挂载结束——回放中途换掉事件
    /// 通道会让回放读不到快照帧。
    pub async fn attach_and_subscribe(&self, session_id: &str) -> Result<Attached, HubError> {
        let (attached, created) = self.attach_session(session_id).await?;
        if let Err(error) = self.subscribe_session(session_id).await {
            if created {
                self.remove_entry(session_id);
                if let Err(err) = self.http.unsubscribe(session_id, &self.client_id).await {
                    tracing::warn!(
                        session_id,
                        error = %err,
                        "acp: rollback unsubscribe failed; the route self-heals when this process exits",
                    );
                }
            } else {
                tracing::debug!(
                    session_id,
                    "acp: re-attach failed to subscribe; keeping the existing entry"
                );
            }
            // 既有条目的回滚 = 仅 disarm（`attached` 随错误返回被 drop 时已经做了）。
            return Err(error);
        }
        Ok(attached)
    }

    /// 挂载的第一段：入表 + gate + arm（见 [`SessionHub::attach_and_subscribe`]）。
    ///
    /// 返回 `(attached, created)`：`created` = 条目是否是**本次新建**的（回滚判据，
    /// N1）——第二道死流复查命中时也据此回滚（N3）。
    async fn attach_session(&self, session_id: &str) -> Result<(Attached, bool), HubError> {
        // 事件流已死 = 递不出任何事件（快照也不会来）：不挂一个注定失败的会话。
        if self.stream_dead.load(Ordering::SeqCst) {
            return Err(HubError::Disconnected);
        }
        let (entry, created) = self.entry_or_register(session_id);
        let gate = Arc::clone(&entry.gate).lock_owned().await;
        // 拿到 gate 之后再查一次（与 `begin_turn` 同款）：排队期间泵可能已经退出。
        if self.stream_dead.load(Ordering::SeqCst) {
            // 本次新建的条目连同卡片记忆一起回滚（06 审查 N3）：死流上不留「在表但
            // 无人投递」的半截会话。只回收新建条目——既有条目是别的挂载/轮次正在用
            // 的（与订阅失败的回滚同判据，见 [`SessionHub::attach_and_subscribe`]）。
            // 本路径还没 subscribe，无需撤销网关侧订阅。
            if created {
                self.remove_entry(session_id);
            }
            return Err(HubError::Disconnected);
        }
        let rx = entry.arm_attached();
        Ok((
            Attached {
                session_id: session_id.to_string(),
                entry,
                rx,
                _gate: gate,
            },
            created,
        ))
    }

    /// 挂载的第二段：订阅（幂等）；网关随即推一份 `sync_session` 快照。
    ///
    /// 只由 [`SessionHub::attach_and_subscribe`] 调用（快照要有人接）。
    async fn subscribe_session(&self, session_id: &str) -> Result<(), HubError> {
        self.http
            .subscribe(session_id, &self.client_id)
            .await
            .map_err(gateway_error)
    }

    /// 关闭会话：视在途轮次为 cancel（有界等待收口）→ 回收会话表条目与卡片记忆
    /// （01 审查 N6）→ unsubscribe + release（幂等，best-effort）。
    ///
    /// **幂等成功**：重复 close / close 不认识（或刚被回收）的会话都不报错——与 Zed 的
    /// 关线程流程兼容。网关侧 release 的失败（忙碌 / 被订阅 / 非持久后端）只记 warn：
    /// 本进程持有的资源（条目、卡片记忆、订阅）已经释放，网关内存由 reaper 兜底。
    ///
    /// HTTP 调用只在「本进程确实持有该会话」时发起——不认识就没有任何资源要退订，
    /// 也不该去逐出一个自己从未持有的会话。
    pub async fn close_session(self: &Arc<Self>, session_id: &str) {
        let known = match self.entry(session_id) {
            Ok(entry) => {
                // ACP：close 必须先当作 `session/cancel` 处理，让**在途轮次**有机会以
                // `cancelled` 收口（而不是被我们抽掉通道、以内部错误收场）。挂载窗口
                // 没有后端轮次——不发 interrupt（05 审查 N3），但仍要等它收口。
                if entry.has_turn_in_flight() {
                    self.request_cancel(session_id);
                }
                if entry.is_armed() {
                    // 等 gate 归还 = 轮次 / 挂载已收口（有界；超时则强制 disarm 回收）。
                    let reclaimed =
                        tokio::time::timeout(CLOSE_GRACE, Arc::clone(&entry.gate).lock_owned())
                            .await;
                    if reclaimed.is_err() {
                        tracing::warn!(
                            session_id,
                            grace_s = CLOSE_GRACE.as_secs(),
                            "acp: session still busy after the close grace window; reclaiming anyway",
                        );
                    }
                }
                self.remove_entry(session_id);
                true
            }
            Err(_) => {
                tracing::debug!(
                    session_id,
                    "acp: close for an unknown session; nothing to reclaim"
                );
                false
            }
        };
        if !known {
            return;
        }
        if let Err(err) = self.http.unsubscribe(session_id, &self.client_id).await {
            tracing::warn!(session_id, error = %err, "acp: unsubscribe failed during close");
        }
        if let Err(err) = self.http.release_session(session_id).await {
            tracing::warn!(
                session_id,
                error = %err,
                "acp: release failed during close; the gateway reaper will reclaim it later"
            );
        }
    }

    /// 取既有条目；不存在则建一个并入表（挂载路径用，幂等——重复挂载不丢卡片记忆）。
    ///
    /// 返回 `(entry, created)`：`created == true` 只对**本次新建**的条目——回滚时据此
    /// 决定要不要回收（05 审查 N1：既有条目不能误删）。
    fn entry_or_register(&self, session_id: &str) -> (Arc<SessionEntry>, bool) {
        let mut state = self.state.lock().expect("hub mutex poisoned");
        if let Some(entry) = state.sessions.get(session_id) {
            return (Arc::clone(entry), false);
        }
        let entry = Arc::new(SessionEntry::new(session_id));
        state
            .sessions
            .insert(session_id.to_string(), Arc::clone(&entry));
        (entry, true)
    }

    /// 会话出表 + 卸下事件通道（卡片记忆随条目 `Arc` 释放）。
    ///
    /// 返回被移除的条目（不存在 → `None`）；重复调用幂等。
    fn remove_entry(&self, session_id: &str) -> Option<Arc<SessionEntry>> {
        let entry = self
            .state
            .lock()
            .expect("hub mutex poisoned")
            .sessions
            .remove(session_id);
        if let Some(entry) = &entry {
            entry.disarm();
        }
        entry
    }

    /// 事件分流（WS 泵调用）：按 `meta.session_id` 投递到对应会话。
    ///
    /// 另外挂 04 的**模型变更中继**：`session_state_changed{model}` 是会话级事实，
    /// 与「有没有在途 prompt」无关（空闲时下面 `deliver` 会把它丢掉），所以触发点在这里
    /// 而不是 prompt 轮次的事件循环里（design D7）。中继经 `claim_model_relay` 合并：
    /// 同一会话同时只有一次在途，期间的变更合到下一次。
    fn dispatch(self: &Arc<Self>, event: WingEvent) {
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
            if super::model::is_model_change(&event) && entry.claim_model_relay() {
                let hub = Arc::clone(self);
                let session_id = session_id.clone();
                tokio::spawn(async move {
                    super::model::relay_model_change(&hub, &session_id).await;
                });
            }
            entry.deliver(event);
        }
    }

    /// 模型变更中继的收尾（[`SessionEntry::finish_model_relay`]；会话已被回收 → false）。
    pub fn finish_model_relay(&self, session_id: &str) -> bool {
        self.entry(session_id)
            .map(|entry| entry.finish_model_relay())
            .unwrap_or(false)
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
// 挂载句柄（session/load · session/resume）
// ============================================================

/// 一次会话挂载（`attach_session` 的产物）：条目 + 事件接收端 + gate guard。
///
/// 生命周期即「挂载期」：期间 `attach_armed = true`（同会话 prompt 排队等 gate，
/// 但**不是**在途轮次——`close` / `cancel` 不据此发 interrupt，05 审查 N3），Drop 时
/// 一并卸下事件通道、释放 gate（回到空闲语义：事件丢弃，下一个 prompt 自己再 arm）。
///
/// 职责分工：调用方 `subscribe_session` → [`Attached::read_snapshot`] 读快照 →
/// [`Attached::replay_updates`] 投影 → Drop（disarm）。
pub struct Attached {
    session_id: String,
    entry: Arc<SessionEntry>,
    rx: mpsc::Receiver<WingEvent>,
    /// 持有到挂载结束：挂载期间同会话的 prompt 在 `begin_turn` 里排队等它。
    _gate: OwnedMutexGuard<()>,
}

impl Attached {
    /// 本会话的 wing 会话 id（原样作 ACP sessionId）。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 读事件直到 `sync_session` 快照（有界超时）。
    ///
    /// 返回的必是 `WingEvent::SyncSession`。订阅之后、快照之前可能混进实时帧
    /// （例如另一进程正在跑轮次）：**丢弃并记 debug**——回放是一次性投影，混入
    /// 半成品状态正是 `session/load` 要避免的。
    ///
    /// 错误：窗口耗尽 → [`HubError::Timeout`]；事件流终止（WS 断开 / hub 收尾）
    /// → [`HubError::Disconnected`]（快照不会来了，别让客户端干等）。
    pub async fn read_snapshot(&mut self, timeout: Duration) -> Result<WingEvent, HubError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(event)) => {
                    if matches!(event, WingEvent::SyncSession { .. }) {
                        return Ok(event);
                    }
                    tracing::debug!(
                        session_id = %self.session_id,
                        event_type = event.event_type(),
                        "acp: frame before the replay snapshot dropped",
                    );
                }
                Ok(None) => return Err(HubError::Disconnected),
                Err(_) => {
                    return Err(HubError::Timeout(format!(
                        "session {}: sync_session snapshot did not arrive within {} ms",
                        self.session_id,
                        timeout.as_millis(),
                    )));
                }
            }
        }
    }

    /// 快照 → 回放 update 序列（同时写进本会话的卡片记忆，见
    /// [`SessionEntry::replay_updates`]）。
    pub fn replay_updates(&self, snapshot: &WingEvent) -> Vec<SessionUpdate> {
        self.entry.replay_updates(snapshot)
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.entry.disarm();
    }
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

    /// 测试用 hub，HTTP 指向一个「接受连接后立刻关闭」的本地监听。
    ///
    /// 用于 `close_session` 这类**真的会发 HTTP** 的路径：失败快速且确定（不依赖
    /// 9 端口/代理环境），断言只落在内存回收上（HTTP 是 best-effort，本来就不该影响结果）。
    async fn hub_with_dead_http() -> (Arc<SessionHub>, mpsc::Receiver<Outbound>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            while listener.accept().await.is_ok() {
                // 连接随上层 drop 立刻关闭：请求在拿到响应前失败。
            }
        });
        let http =
            GatewayApiClient::new(format!("http://{addr}"), None).expect("reqwest client builds");
        SessionHub::new_unstarted(http, "client-test".into())
    }

    /// 测试用 hub，HTTP 指向一个「记录请求行」的极简监听：**订阅一律 500**（触发回滚），
    /// 其余路径 200。用来观察「发不发 HTTP / 发的是哪条路径」（05 审查 N1/N2/N3）。
    ///
    /// 返回的 `Arc<Mutex<Vec<String>>>` 是请求行（`"POST /api/session/… HTTP/1.1"`）。
    async fn hub_with_recording_http() -> (
        Arc<SessionHub>,
        mpsc::Receiver<Outbound>,
        Arc<Mutex<Vec<String>>>,
    ) {
        use tokio::io::AsyncReadExt as _;
        use tokio::io::AsyncWriteExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&lines);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let line = String::from_utf8_lossy(&head)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    if line.is_empty() {
                        return;
                    }
                    let subscribe = line.contains("/api/session/subscribe");
                    recorded.lock().expect("request lines").push(line);
                    let (status, body) = if subscribe {
                        (
                            "500 Internal Server Error",
                            r#"{"error":"boom","detail":"subscribe failed"}"#,
                        )
                    } else {
                        ("200 OK", r#"{"ok":true}"#)
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        let http =
            GatewayApiClient::new(format!("http://{addr}"), None).expect("reqwest client builds");
        let (hub, outbound_rx) = SessionHub::new_unstarted(http, "client-test".into());
        (hub, outbound_rx, lines)
    }

    /// `sync_session` fixture（回放快照）。
    fn sync_event(session_id: &str) -> WingEvent {
        serde_json::from_value(json!({
            "type": "sync_session",
            "session_id": session_id,
            "status": "idle",
            "messages": [{"role": "user", "content": "hi"}],
            "events": [],
            "created_at": "2026-01-01T00:00:00+00:00",
            "request_id": "req-sync",
        }))
        .expect("sync fixture decodes")
    }

    /// 带 tool_call_id 的 `tool_call_stream` fixture（建一张卡片）。
    fn tool_stream(id: &str) -> WingEvent {
        serde_json::from_value(json!({
            "type": "tool_call_stream",
            "tool_call_id": id,
            "tool_name": "Edit",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-tool",
        }))
        .expect("tool fixture decodes")
    }

    #[test]
    fn entry_without_consumer_drops_events() {
        let entry = SessionEntry::new("s1");
        assert!(!entry.is_armed());
        assert!(!entry.has_turn_in_flight());
        assert!(!entry.deliver(event("s1")), "no consumer → dropped");
    }

    #[test]
    fn armed_entry_delivers_events_in_order() {
        let entry = SessionEntry::new("s1");
        let mut rx = entry.arm_turn();
        assert!(entry.is_armed());
        assert!(entry.has_turn_in_flight(), "轮次 armed ≠ 挂载 armed（N3）");
        assert!(entry.deliver(event("s1")));
        assert!(entry.deliver(event("s1")));
        assert_eq!(rx.try_recv().expect("first event").event_type(), "text");
        assert_eq!(rx.try_recv().expect("second event").event_type(), "text");
        entry.disarm();
        assert!(!entry.is_armed());
        assert!(!entry.has_turn_in_flight());
        assert!(!entry.deliver(event("s1")), "disarmed → dropped again");
    }

    #[test]
    fn disarm_releases_queued_events_with_the_receiver() {
        let entry = SessionEntry::new("s1");
        let rx = entry.arm_turn();
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
        let mut rx = entry.arm_turn();
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
        assert!(entry.has_turn_in_flight());
        drop(turn);
        assert!(!entry.is_armed());

        // WS 断开（pump 退出前置位）之后：
        hub.mark_stream_dead();
        let err = match hub.begin_turn("s1", "hi").await {
            Ok(_) => panic!("a dead stream must fail fast"),
            Err(err) => err,
        };
        assert_eq!(err, HubError::Disconnected);
        assert!(
            !entry.is_armed(),
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
        assert!(!entry.is_armed());
        let mut next = entry.arm_turn();
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
        assert_eq!(
            HubError::Timeout("snapshot".into()).to_string(),
            "timed out: snapshot"
        );
    }

    // ---- 挂载（session/load · session/resume） ----

    #[tokio::test]
    async fn attach_registers_an_unknown_session_and_arms_it() {
        let (hub, _outbound_rx) = test_hub();
        let (mut attached, _) = hub.attach_session("s1").await.expect("attach");
        assert_eq!(attached.session_id(), "s1");
        assert!(
            hub.knows("s1"),
            "挂载把会话放进表里（后续 begin_turn 依赖它）"
        );

        let entry = hub.entry("s1").expect("entry is registered");
        assert!(entry.is_armed(), "挂载期间 armed：同会话 prompt 排队等挂载");
        assert!(
            !entry.has_turn_in_flight(),
            "挂载不是在途轮次（N3：close/cancel 不该据此发 interrupt）"
        );
        assert!(entry.deliver(event("s1")), "armed：事件投递到挂载句柄");

        // 非 sync_session 的帧被跳过（回放只吃快照）→ 窗口耗尽超时。
        let err = attached
            .read_snapshot(Duration::from_millis(20))
            .await
            .expect_err("no snapshot arrived");
        match err {
            HubError::Timeout(detail) => {
                assert!(
                    detail.contains("sync_session"),
                    "诊断要指明缺什么：{detail}"
                );
            }
            other => panic!("expected Timeout, got {other}"),
        }

        // Drop = 卸载（disarm，回到空闲语义）。
        drop(attached);
        assert!(!entry.is_armed());
    }

    #[tokio::test]
    async fn read_snapshot_returns_the_sync_session_frame() {
        let (hub, _outbound_rx) = test_hub();
        let entry = hub.register("s1");
        let (mut attached, _) = hub.attach_session("s1").await.expect("attach");

        // 快照之前混进来的实时帧：丢弃（debug），不能污染回放。
        entry.deliver(event("s1"));
        entry.deliver(sync_event("s1"));

        let snapshot = attached
            .read_snapshot(Duration::from_secs(1))
            .await
            .expect("snapshot arrives");
        match &snapshot {
            WingEvent::SyncSession {
                messages, events, ..
            } => {
                assert_eq!(messages.len(), 1, "messages 原样带出（回放素材）");
                assert_eq!(messages[0]["role"], "user");
                assert!(events.is_empty());
            }
            other => panic!("expected sync_session, got {}", other.event_type()),
        }
    }

    #[tokio::test]
    async fn attach_and_subscribe_rolls_back_when_subscribe_fails() {
        // 订阅失败 = 这个会话在本进程里永远收不到事件（prompt 会静默挂起）：
        // 必须回滚条目，而不是留一个「在表但没订阅」的半截会话。
        let (hub, _outbound_rx) = hub_with_dead_http().await;
        let err = match hub.attach_and_subscribe("s1").await {
            Ok(_) => panic!("subscribe must fail against a dead gateway"),
            Err(err) => err,
        };
        assert!(
            matches!(err, HubError::Gateway(_)),
            "订阅失败原样上报：{err}"
        );
        assert!(
            !hub.knows("s1"),
            "订阅失败必须回滚条目（不留收不到事件的会话）"
        );
    }

    /// 05 审查 N1 + N2：新建条目的回滚 = 回收条目 **且** best-effort 撤销网关侧订阅
    /// （subscribe 是「先建路由后应答」，不撤会把会话钉在网关内存里）。
    #[tokio::test]
    async fn a_fresh_entry_rolls_back_and_unsubscribes() {
        let (hub, _outbound_rx, lines) = hub_with_recording_http().await;
        let Err(err) = hub.attach_and_subscribe("s1").await else {
            panic!("订阅必须失败（假网关对 /api/session/subscribe 回 500）");
        };
        assert!(matches!(err, HubError::Gateway(_)), "错误原样上报：{err}");
        assert!(!hub.knows("s1"), "新建条目必须回收");

        let lines = lines.lock().expect("request lines").clone();
        assert_eq!(lines.len(), 2, "subscribe + 回滚 unsubscribe：{lines:?}");
        assert!(
            lines[0].starts_with("POST /api/session/subscribe"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("POST /api/session/unsubscribe"),
            "回滚必须撤销网关侧订阅（N2）：{lines:?}"
        );
    }

    /// 05 审查 N1 + N2：重复挂载（条目已存在）失败时**保留**既有条目与卡片记忆，
    /// 且**不**撤销它的健康订阅——一次失败的重复挂载不该把它打坏。
    #[tokio::test]
    async fn a_failed_re_attach_keeps_the_existing_entry_and_its_subscription() {
        let (hub, _outbound_rx, lines) = hub_with_recording_http().await;
        let entry = hub.register("s1");
        assert_eq!(entry.updates_for(&tool_stream("tc1")).len(), 1);

        let Err(err) = hub.attach_and_subscribe("s1").await else {
            panic!("订阅必须失败（假网关对 /api/session/subscribe 回 500）");
        };
        assert!(matches!(err, HubError::Gateway(_)), "错误原样上报：{err}");

        assert!(hub.knows("s1"), "既有条目必须保留（N1）");
        let kept = hub.entry("s1").expect("entry");
        assert!(Arc::ptr_eq(&kept, &entry), "保留的是同一个条目");
        assert!(
            entry.updates_for(&tool_stream("tc1")).is_empty(),
            "卡片记忆随条目保留"
        );
        assert!(!entry.is_armed(), "失败路径要把本次挂载的通道卸下");

        let lines = lines.lock().expect("request lines").clone();
        assert_eq!(lines.len(), 1, "不撤销既有订阅（N2）：{lines:?}");
        assert!(
            lines[0].starts_with("POST /api/session/subscribe"),
            "{lines:?}"
        );
    }

    /// 05 审查 N3：挂载窗口没有后端轮次——`session/cancel` 不该向网关发 interrupt；
    /// 真有一轮 prompt 在途时才发（本测试同时钉住正反两面）。
    #[tokio::test]
    async fn cancel_only_interrupts_when_a_turn_is_in_flight() {
        let (hub, _outbound_rx, lines) = hub_with_recording_http().await;
        hub.register("s1");
        let (attached, _) = hub.attach_session("s1").await.expect("attach");

        hub.request_cancel("s1");
        // 有界静默：旧实现（只看 is_active）会在这里发出 interrupt。
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            lines.lock().expect("request lines").is_empty(),
            "挂载窗口（无在途轮次）不该发 interrupt：{:?}",
            lines.lock().expect("request lines")
        );

        // 对照组：真有一轮 prompt 在途，cancel 必须发 interrupt。
        drop(attached);
        let turn = hub.begin_turn("s1", "hi").await.expect("turn arms");
        hub.request_cancel("s1");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while lines.lock().expect("request lines").is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "在途轮次的 cancel 必须发 interrupt"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            lines.lock().expect("request lines")[0].starts_with("POST /api/session/interrupt"),
            "{:?}",
            lines.lock().expect("request lines")
        );
        drop(turn);
    }

    /// 05 审查 N3（close 面）：挂载窗口的 `close` 不发 interrupt——但等挂载收口后
    /// 照常回收（unsubscribe + release）。
    #[tokio::test]
    async fn close_during_a_mount_window_does_not_interrupt() {
        let (hub, _outbound_rx, lines) = hub_with_recording_http().await;
        hub.register("s1");
        let (attached, _) = hub.attach_session("s1").await.expect("attach");

        let closer = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move { hub.close_session("s1").await })
        };
        // 给 close 走到「等 gate」的时间（旧实现在这里发 interrupt）。
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(attached);
        closer.await.expect("close task");

        let lines = lines.lock().expect("request lines").clone();
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("/api/session/interrupt")),
            "挂载窗口的 close 不该发 interrupt：{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("/api/session/unsubscribe")),
            "close 仍要撤销订阅：{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("/api/session/release")),
            "close 仍要 release：{lines:?}"
        );
        assert!(!hub.knows("s1"), "close 回收条目");
    }

    #[tokio::test]
    async fn dispatch_routes_a_sync_session_to_the_armed_entry() {
        // 回归（smoke 实测）：`sync_session` 的 session_id 是**具名字段**，
        // 不在 flattened meta 里——`dispatch` 若按 meta 取 id 会把它当全局事件丢掉，
        // 回放永远等不到快照。这里直接从分流入口投递，锁住这条路径。
        let (hub, _outbound_rx) = test_hub();
        hub.register("s1");
        let (mut attached, _) = hub.attach_session("s1").await.expect("attach");
        hub.dispatch(sync_event("s1"));
        let snapshot = attached
            .read_snapshot(Duration::from_secs(1))
            .await
            .expect("sync_session 必须经分流到达挂载句柄");
        assert!(matches!(snapshot, WingEvent::SyncSession { .. }));
    }

    #[tokio::test]
    async fn read_snapshot_reports_a_dead_event_stream() {
        let (hub, _outbound_rx) = test_hub();
        let entry = hub.register("s1");
        let (mut attached, _) = hub.attach_session("s1").await.expect("attach");

        // WS 断开 / hub 收尾：通道被卸下 → 快照不会来了，立刻可诊断地失败。
        entry.disarm();
        let err = attached
            .read_snapshot(Duration::from_secs(1))
            .await
            .expect_err("dead stream");
        assert_eq!(err, HubError::Disconnected);
    }

    #[tokio::test]
    async fn attach_on_a_dead_stream_fails_fast() {
        let (hub, _outbound_rx) = test_hub();
        hub.stream_dead.store(true, Ordering::SeqCst);
        let err = match hub.attach_session("s1").await {
            Ok(_) => panic!("a dead stream must fail fast"),
            Err(err) => err,
        };
        assert_eq!(err, HubError::Disconnected);
        assert!(!hub.knows("s1"), "死流上不挂载、也不入表");
    }

    /// 06 审查 N3：死流在「入表之后、arm 之前」才被置位时，第二道复查必须把本次
    /// **新建**的条目录回滚——否则会话表里留一个收不到事件的半截会话。
    ///
    /// 窗口是**构造**出来的（不是竞速）：先扣住 hub 的 `state` 锁——`entry_or_register`
    /// 的必经点。`attach_session` 必然已经过了第一道死流检查（此刻还是 false）、还没
    /// 建条目；放锁前把 `stream_dead` 置位，任务继续后就会在第二道复查命中。
    #[tokio::test(flavor = "multi_thread")]
    async fn attach_on_a_stream_that_dies_while_queued_rolls_back_the_new_entry() {
        let (hub, _outbound_rx) = test_hub();
        let state_guard = hub.state.lock().expect("hub state lock");
        let attach = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move { hub.attach_session("s1").await })
        };
        // 有界等待：让 attach 任务走到 `state` 锁上（第一道检查已通过）。用阻塞
        // sleep：`state_guard` 是 std 锁，不能跨 await 持有（clippy
        // `await_holding_lock`）；multi-thread runtime 的其它 worker 照常跑任务。
        std::thread::sleep(Duration::from_millis(50));
        hub.stream_dead.store(true, Ordering::SeqCst);
        drop(state_guard);

        let err = match attach.await.expect("attach task") {
            Ok(_) => panic!("a dead stream must fail"),
            Err(err) => err,
        };
        assert_eq!(err, HubError::Disconnected);
        assert!(
            !hub.knows("s1"),
            "死流早退必须回滚本次新建的条目（N3）：不留收不到事件的半截会话"
        );
    }

    #[tokio::test]
    async fn replay_writes_into_the_session_card_memory() {
        let (hub, _outbound_rx) = test_hub();
        let entry = hub.register("s1");
        let (attached, _) = hub.attach_session("s1").await.expect("attach");

        let value = json!({
            "type": "sync_session",
            "session_id": "s1",
            "status": "idle",
            "messages": [{
                "role": "assistant",
                "content": "改了一处",
                "tool_calls": [{"id": "tc1", "name": "Edit", "arguments": {"path": "a.rs"}}],
            }],
            "events": [],
            "created_at": "c",
            "request_id": "r",
        });
        let snapshot: WingEvent = serde_json::from_value(value).expect("fixture decodes");
        let updates = attached.replay_updates(&snapshot);
        assert_eq!(updates.len(), 2, "文本块 + 工具卡片创建");

        // 卡片记忆落在会话条目上：回放建过的 id 在实时路径上是「已创建」。
        assert!(
            entry.updates_for(&tool_stream("tc1")).is_empty(),
            "回放与实时共用同一份 ToolCards"
        );
        assert_eq!(
            entry.updates_for(&tool_stream("tc2")).len(),
            1,
            "回放之后的新 id 仍会创建卡片"
        );
    }

    // ---- 回收（session/close · 01 审查 N6） ----

    #[tokio::test]
    async fn close_drops_the_entry_and_releases_the_card_memory() {
        let (hub, _outbound_rx) = hub_with_dead_http().await;
        let entry = hub.register("s1");

        // 先把会话级卡片记忆填上（同一 id 只创建一次 = 记忆已生效）。
        assert_eq!(entry.updates_for(&tool_stream("tc1")).len(), 1);
        assert!(entry.updates_for(&tool_stream("tc1")).is_empty());

        hub.close_session("s1").await;

        assert!(!hub.knows("s1"), "close 必须回收会话表条目（N6）");
        assert!(!entry.is_armed(), "关闭后的条目不再 arm");

        // 重新挂载：同一 tool_call_id 必须重新建卡——记忆随条目一起释放了。
        let (fresh, _) = hub.entry_or_register("s1");
        assert_eq!(
            fresh.updates_for(&tool_stream("tc1")).len(),
            1,
            "close 之后卡片记忆从零开始（N6）"
        );
    }

    #[tokio::test]
    async fn close_is_idempotent_and_a_noop_for_unknown_sessions() {
        let (hub, _outbound_rx) = test_hub();
        // 不认识（也从未建过）的会话：不发 HTTP、不报错、不建条目。
        hub.close_session("ghost").await;
        hub.close_session("ghost").await;
        assert!(!hub.knows("ghost"));

        // 建过再关：第二次是纯 no-op（同样成功）。
        hub.register("s1");
        hub.close_session("s1").await;
        hub.close_session("s1").await;
        assert!(!hub.knows("s1"));
    }

    #[tokio::test]
    async fn close_during_an_active_turn_cancels_then_reclaims() {
        let (hub, _outbound_rx) = hub_with_dead_http().await;
        let entry = hub.register("s1");
        let turn = hub.begin_turn("s1", "hi").await.expect("turn arms");
        assert!(entry.has_turn_in_flight());

        // close 会先按 cancel 语义处理（HTTP interrupt 立刻失败，只记 warn），
        // 然后有界等 gate 归还——轮次收刀（drop）后回收。
        let closer = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move { hub.close_session("s1").await })
        };
        drop(turn);
        closer.await.expect("close task");

        assert!(!hub.knows("s1"), "close 在活跃轮次收口后仍要回收条目");
        assert!(!entry.is_armed());
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

    // ---- 04：模型变更中继的合并协议 ----

    #[test]
    fn model_relay_claims_are_merged_while_one_is_in_flight() {
        let entry = SessionEntry::new("s1");
        assert!(entry.claim_model_relay(), "空闲：这次由我跑");
        assert!(!entry.claim_model_relay(), "在途：合并（只置积压标记）");
        assert!(!entry.claim_model_relay(), "再来一次仍只是合并");
        assert!(entry.finish_model_relay(), "收尾发现积压 → 再跑一次");
        assert!(
            !entry.finish_model_relay(),
            "第二次收尾没有积压 → 结束（在途标记清掉）"
        );
        assert!(entry.claim_model_relay(), "回到空闲后新的变更又能认领");
        assert!(!entry.finish_model_relay(), "干净收尾不再重跑");
    }

    #[test]
    fn finish_model_relay_is_a_noop_for_unknown_sessions() {
        let (hub, _outbound_rx) = test_hub();
        assert!(!hub.finish_model_relay("nope"));
    }

    /// 未注册客户端连接（`connect_with` 之前）时中继只记日志，但**必须**走收尾：
    /// 否则在途标记永远挂着，此后所有模型变更都只会被合并、再也不播报。
    #[tokio::test]
    async fn relay_model_change_clears_the_in_flight_flag_without_a_client_connection() {
        let (hub, _outbound_rx) = test_hub();
        let entry = hub.register("s1");
        assert!(hub.client_connection().is_none(), "测试 hub 不注册连接");

        assert!(entry.claim_model_relay());
        super::super::model::relay_model_change(&hub, "s1").await;
        assert!(
            entry.claim_model_relay(),
            "收尾已执行：后续变更仍能认领（标记没被挂死）"
        );
    }
}
