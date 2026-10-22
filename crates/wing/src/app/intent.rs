//! AppIntent — side-effect declarations from the App state machine.
//!
//! All operations from App to the external world (gateway, clipboard) are
//! expressed as `AppIntent` variants. The runner drains pending intents after
//! each draw and executes them in order.
//!
//! Fetch-type intents (read-only HTTP queries) are executed as background
//! tasks to avoid blocking the main event loop. Results are delivered back
//! via [`FetchResult`] through an mpsc channel.

use wing_api_client::models::{
    AgentsResponse, BranchesResponse, CommandsResponse, ModelsResponse, SessionInfoResponse,
    SessionListResponse, SettingsGetResponse, SettingsSchemaResponse, SettingsSetResponse,
};

use crate::util::image_compress::CompressOptions;
use crate::util::image_compress::CompressedImage;

/// Payload of a background fetch result.
pub enum FetchPayload {
    /// Session runtime info (model, tokens, thinking, yolo).
    Info(Box<SessionInfoResponse>),
    /// Available commands list.
    Commands(CommandsResponse),
    /// Available model list.
    Models(ModelsResponse),
    /// Branch targets for fork/rewind.
    Branches(BranchesResponse),
    /// Available agent templates.
    Agents(AgentsResponse),
    /// Session list.
    SessionList(SessionListResponse),
    /// Context stats display text.
    ContextInfo(String),
    /// Skills info display text.
    SkillsInfo(String),
    /// Compact session completed (original_tokens, compressed_tokens).
    CompactDone { original: i64, compressed: i64 },
    /// 设置目录 + 稀疏文档快照（`GET schema` + `GET get`，并发一次拿回）。
    ///
    /// 装箱：`SettingsSchemaResponse` 带着整棵 catalog，是枚举里最大的一档
    /// （`FetchPayload` 会进 mpsc 的缓冲区，别让每个变体都按最大档算大小）。
    Settings {
        schema: Box<SettingsSchemaResponse>,
        state: Box<SettingsGetResponse>,
    },
    /// 一次保存的 Gateway 半边回执（`POST /api/settings/set`）。
    SettingsSaved(Box<SettingsSetResponse>),
    /// Gateway 半边失败：`conflict` 区分 409（指纹冲突）与传输 / 协议错误。
    SettingsSaveError { message: String, conflict: bool },
    /// 一条粘贴图片的压缩回执（[`AppIntent::CompressClipboardImage`]）。
    ///
    /// `Err` 是给 toast 的文案（压缩器错误已格式化）。落点靠 `target` 认领：它是
    /// 那张占位符 chip 的句柄（草稿被清空 / 重写之后句柄作废，结果被丢弃而不是插到
    /// 别处）。这个变体**不属于任何会话**：它回来时走 `App::on_clipboard_compressed`，
    /// 不看会话守卫（草稿跨会话还在）。
    ClipboardImage {
        target: crate::ui::input_area::ImageChipTarget,
        result: Result<CompressedImage, String>,
    },
    /// 一张媒体图的字节回执（[`AppIntent::LoadMedia`]）。
    ///
    /// `Err` 是给日志的一句话（对象缺失 / 归档 / 网关不可达）：气泡里那一张保持 caption，
    /// lane 记下失败、离开视口再回来时重试（见 `app::images::media`）。
    MediaBytes {
        /// 媒体对象 id（认领键；同一 id 的重复回执按后到者算）。
        media_id: String,
        result: Result<std::sync::Arc<Vec<u8>>, String>,
    },

    /// 一条本地剪贴板读取回执（[`AppIntent::ReadClipboard`]）。
    ///
    /// 与 `ClipboardImage` 同一条契约：结果属于 composer 而不是会话，
    /// `handle_fetch_result` 在会话守卫**之前**分派它（换过会话草稿还在，粘贴照落）。
    ClipboardPaste(
        Result<
            crate::util::clipboard_paste::PasteOutcome,
            crate::util::clipboard_paste::PasteFailure,
        >,
    ),
    /// 一条消息的**后台发送失败**回执（`SendMessage` 后台任务，见
    /// `runner::spawn_send_message`）。
    ///
    /// 条目按 `request_id` 认领（pending 就是它的锚），所以和压缩一样在会话守卫
    /// **之前**分派：换过会话后也该有反馈（条目不在时只 toast，不碰草稿）。
    SendFailed {
        /// 与 `SendMessage` / pending 条目同一个关联 id。
        request_id: String,
        /// 给 toast 的文案（`Image upload failed: …` / `Send failed: …`）。
        reason: String,
    },
    /// Show a toast message (for errors or success feedback from background tasks).
    Toast { message: String, is_error: bool },
}

/// Result of a background fetch intent, delivered via mpsc channel.
///
/// Carries the `session_id` that was active when the request was spawned,
/// so the receiver can discard stale results after a session switch.
pub struct FetchResult {
    /// Session ID at spawn time — used to discard stale results.
    pub session_id: String,
    /// The actual payload.
    pub payload: FetchPayload,
}

/// A side-effect intent produced by the App state machine.
///
/// The runner calls `drain_intents()` after each draw cycle and matches on
/// each variant to perform the corresponding I/O operation.
#[derive(Debug)]
pub enum AppIntent {
    /// Send a user message to the current session via gateway.
    ///
    /// `tool_call_id` — Some when this message answers a pending Ask event
    /// (routes to the ask's feedback waiter); None for plain user input.
    ///
    /// `request_id` — client-generated correlation id; echoes back in
    /// `delivered` / `user_message_accepted` events so the app can match
    /// the message against its pending queue.
    ///
    /// `images` — 随消息发出的图片（压缩产物，按提交顺序）。空 = 纯文本，
    /// runner 走原路径（不上传、帧里没有 `media` 键）；非空时 runner 先逐张
    /// 上传再发消息（见 `runner::spawn_send_message`）。
    SendMessage {
        content: String,
        tool_call_id: Option<String>,
        request_id: String,
        images: Vec<crate::util::image_compress::CompressedImage>,
    },

    /// Write text to clipboard via OSC52 escape sequence.
    CopyToClipboard(String),

    /// 读系统剪贴板（L1；`Ctrl+V` / 终端回传的 `super+v` 的唯一动作）。
    ///
    /// runner 把读取放进后台任务（子进程 + 每条命令的超时在 `util::clipboard_paste`），
    /// 结果经 [`FetchPayload::ClipboardPaste`] 回到 App —— 事件循环一步不等
    /// （图片读取本来就要秒级，内联 await 会冻 UI）。
    ReadClipboard,

    /// 压缩一张刚粘贴进来的图片（通道给的是原始字节）。
    ///
    /// 压缩在阻塞池上跑（解码 / 缩放 / 编码是纯 CPU），结果经
    /// [`FetchPayload::ClipboardImage`] 回到 App —— 事件循环只做入队与插 chip。
    /// 选项由 App 在入队时从 `paste.image.*` 取（`PasteImageConfig::compress_options`
    /// 已夹紧越界值），所以改配置只影响之后的那一张。
    ///
    /// `target` 是那张**已经在草稿里的**占位符（字节到手时它就在了）：压缩完成
    /// 只是把它翻成正式 chip，不新建、不挪位置。
    CompressClipboardImage {
        target: crate::ui::input_area::ImageChipTarget,
        bytes: Vec<u8>,
        options: CompressOptions,
    },

    /// 切换当前会话的 pin（置顶 = 会话上的一个普通标签，前端约定、后端零感知
    /// ——见 `crate::shared::pinning`；写入走通用的 `/api/session/tag`）。
    SetSessionPin { pinned: bool },

    /// Open a markdown link target with the system opener (browser for URLs,
    /// default application for local files). The raw destination is carried
    /// verbatim — resolution happens in `util::open` on the blocking pool.
    OpenLink(String),

    /// Create a new session via HTTP API.
    CreateSession { workspace: Option<String> },

    /// Resume an existing session via HTTP API.
    ResumeSession { session_id: String },

    /// Fork from a branch target via HTTP API.
    ForkSession { target_uuid: String },

    /// Fetch session list via HTTP API for popup candidates.
    FetchSessionList,

    /// Fetch session runtime info (model, tokens, thinking, yolo) via HTTP API.
    FetchInfo,

    /// Fetch available commands list via HTTP API.
    FetchCommands,

    /// 抓网关上的媒体图字节（`GET /api/session/media`），给消息里的图用。
    ///
    /// 由 `Images` 的媒体 lane 在**画帧之后**发现（`media_requests`：屏幕上、没字节、
    /// 没在飞的那些），所以它和其它 fetch intent 一样只是「后台去查」，事件循环一步不等。
    /// `media_ids` 一批 ≤ `app::images::media::MAX_INFLIGHT`；每张的回执按 id 认领。
    LoadMedia {
        session_id: String,
        media_ids: Vec<String>,
    },

    /// Fetch available model list via HTTP API for popup candidates.
    FetchModels,

    /// Fetch branch targets via HTTP API for popup candidates.
    FetchBranches,

    /// Fetch available agent template list via HTTP API for popup candidates.
    FetchAgents,

    /// Update session state (model id, agent, title, thinking, reasoning_effort,
    /// yolo, workspace) via HTTP API.
    UpdateSession(SessionUpdate),

    /// Set the terminal title via OSC 0 escape sequence.
    SetTitle(String),

    /// Send a desktop notification via OSC 9 escape sequence.
    Notify(String),

    /// Report the program status to the terminal via OSC 7501 (Program Status
    /// Protocol). Carries the fully formatted sequence: the App formats once,
    /// so its dedup compares exactly what goes on the wire.
    SetProgramStatus(String),

    /// Compact the current session context via HTTP API.
    /// `instruction` is an optional user-directed compaction focus
    /// (parsed from `/compact <instruction>`), appended to the compact prompt.
    CompactSession { instruction: Option<String> },

    /// Interrupt the current agent turn via HTTP API.
    InterruptSession,

    /// Rewind the session to a specific message via HTTP API.
    RewindSession { target_uuid: String },

    /// Reload system configuration via HTTP API.
    ReloadSystem,

    /// Fetch and display context stats (messages, tokens) via HTTP API.
    ShowContextInfo,

    /// Fetch and display skills info via HTTP API.
    ShowSkillsInfo,

    // ---- 设置面板 ----
    /// `GET /api/settings/schema` + `GET /api/settings/get`（并发），产出
    /// [`FetchPayload::Settings`]。打开面板的 cache-first 刷新与首次拉取共用它。
    FetchSettings,

    /// `R`：丢弃本地改动并重拉 `get`（结果的落地口径见 `App::settings_reload_pending`）。
    ReloadSettings,

    /// `s`：一键保存两边。Interface 半边在 intent 执行时同步写盘；Gateway 半边异步 POST。
    SaveSettings {
        /// 本次要提交给网关的稀疏文档。
        gateway: Box<serde_json::Value>,
        /// 乐观并发的基准指纹（面板持有的那一个）。
        base: String,
        /// 要写进 `~/.wing/tui/config.yaml` 的稀疏文档。
        interface: Box<serde_json::Value>,
        /// 哪边真的改了：跳过没有改动的一边（不发空 POST / 不重写文件）。
        gateway_dirty: bool,
        interface_dirty: bool,
    },

    /// `Ctrl+R`：立即重启网关（shutdown → 等不可达 → 重新拉起 → 重连 → 会话恢复）。
    /// 轮次进行中由 App 在分派点拒绝。
    RestartGateway,
}

/// [`AppIntent::UpdateSession`] 的字段：全部 `Option`，`None` = 这一项不动。
///
/// 具名字段而不是 7 个位置参数：形参表与请求映射都只写一份（`SessionUpdate`），
/// 构造点各一行。
#[derive(Debug, Clone, Default)]
pub struct SessionUpdate {
    /// 模型引用词（model_id）——唯一的模型变更入口。
    pub model_id: Option<String>,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub thinking: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub yolo: Option<bool>,
    pub workspace: Option<String>,
}

impl AppIntent {
    /// Update the session model by its reference word (`model_id`).
    pub fn set_model(model_id: String) -> Self {
        Self::UpdateSession(SessionUpdate {
            model_id: Some(model_id),
            ..Default::default()
        })
    }

    /// Update the session agent template.
    pub fn set_agent(agent: String) -> Self {
        Self::UpdateSession(SessionUpdate {
            agent: Some(agent),
            ..Default::default()
        })
    }

    /// Update the session title.
    pub fn set_title(title: String) -> Self {
        Self::UpdateSession(SessionUpdate {
            title: Some(title),
            ..Default::default()
        })
    }

    /// Update the thinking mode (and optionally reasoning effort).
    pub fn set_thinking(enabled: bool, effort: Option<String>) -> Self {
        Self::UpdateSession(SessionUpdate {
            thinking: Some(enabled),
            reasoning_effort: effort,
            ..Default::default()
        })
    }

    /// Update the YOLO mode (auto-approve tool calls).
    pub fn set_yolo(enabled: bool) -> Self {
        Self::UpdateSession(SessionUpdate {
            yolo: Some(enabled),
            ..Default::default()
        })
    }

    /// Update the session working directory.
    pub fn set_workdir(path: String) -> Self {
        Self::UpdateSession(SessionUpdate {
            workspace: Some(path),
            ..Default::default()
        })
    }
}
