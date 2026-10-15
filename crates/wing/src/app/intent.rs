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
    SendMessage {
        content: String,
        tool_call_id: Option<String>,
        request_id: String,
    },

    /// Write text to clipboard via OSC52 escape sequence.
    CopyToClipboard(String),

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

    /// Fetch available model list via HTTP API for popup candidates.
    FetchModels,

    /// Fetch branch targets via HTTP API for popup candidates.
    FetchBranches,

    /// Fetch available agent template list via HTTP API for popup candidates.
    FetchAgents,

    /// Update session state (model id, agent, title, thinking, reasoning_effort,
    /// yolo, workspace) via HTTP API.
    UpdateSession {
        /// 模型引用词（model_id）——唯一的模型变更入口。
        model_id: Option<String>,
        agent: Option<String>,
        title: Option<String>,
        thinking: Option<bool>,
        reasoning_effort: Option<String>,
        yolo: Option<bool>,
        workspace: Option<String>,
    },

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

    // ---- 设置面板（design §12.4） ----
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
    /// 轮次进行中由 App 在分派点拒绝（design §12.5）。
    RestartGateway,
}

impl AppIntent {
    /// Build an `UpdateSession` intent with all fields `None`.
    fn update_session(f: impl FnOnce(&mut Self)) -> Self {
        let mut intent = Self::UpdateSession {
            model_id: None,
            agent: None,
            title: None,
            thinking: None,
            reasoning_effort: None,
            yolo: None,
            workspace: None,
        };
        f(&mut intent);
        intent
    }

    /// Update the session model by its reference word (`model_id`).
    pub fn set_model(model_id: String) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { model_id: m, .. } = i {
                *m = Some(model_id);
            }
        })
    }

    /// Update the session agent template.
    pub fn set_agent(agent: String) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { agent: a, .. } = i {
                *a = Some(agent);
            }
        })
    }

    /// Update the session title.
    pub fn set_title(title: String) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { title: t, .. } = i {
                *t = Some(title);
            }
        })
    }

    /// Update the thinking mode (and optionally reasoning effort).
    pub fn set_thinking(enabled: bool, effort: Option<String>) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession {
                thinking,
                reasoning_effort,
                ..
            } = i
            {
                *thinking = Some(enabled);
                *reasoning_effort = effort;
            }
        })
    }

    /// Update the YOLO mode (auto-approve tool calls).
    pub fn set_yolo(enabled: bool) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { yolo, .. } = i {
                *yolo = Some(enabled);
            }
        })
    }

    /// Update the session working directory.
    pub fn set_workdir(path: String) -> Self {
        Self::update_session(|i| {
            if let Self::UpdateSession { workspace, .. } = i {
                *workspace = Some(path);
            }
        })
    }
}
