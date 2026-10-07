//! ACP handler 注册与轮次驱动（`wing acp` 的协议面）。
//!
//! 方法（其余一律不注册，SDK 自动回 `method not found`）：
//!
//! | 方法 | 形态 | 要点 |
//! |------|------|------|
//! | `initialize` | request | 固定回 v1 + `agentInfo`；能力广告见 [`agent_capabilities`] |
//! | `session/new` | request | 用客户端 `cwd` 建 wing 会话（workspace），忽略 `mcpServers` / `additionalDirectories`；应答后补发 `available_commands_update` |
//! | `session/prompt` | request | 拍平 prompt → WS 投递 → 事件流转 update → 终态收口 |
//! | `session/cancel` | notification | → HTTP interrupt；在途轮次等 `interrupted` 事件回 `stopReason: "cancelled"` |
//! | `session/list` | request | `GET /api/session/list` → 过滤 / 分页 / 映射（[`list_page`]） |
//! | `session/load` | request | resume 校验 → 挂载（arm + subscribe）→ 回放 `sync_session` 快照 → 标题/用量/命令 → 响应（[`restore_session`]） |
//! | `session/resume` | request | 同 load，不回放 |
//! | `session/close` | request | 在途轮次按 cancel 处理 → 回收 hub 条目（N6）→ unsubscribe + release（幂等成功） |
//! | `session/set_config_option` | request | 只认 `model`（`provider:model` 值域 / 裸模型名）→ `POST /api/session/update` → 回**全量** options（见 [`super::model`]） |
//!
//! `session/new` / `session/load` / `session/resume` 的响应都带 `configOptions`
//! （id=`model`；构造与解析全在 [`super::model`]）；外部改模型（TUI / 其它前端）触发
//! `session_state_changed` 时，中继在 hub 的分流路径上（见 `super::session::SessionHub::dispatch`），
//! 不在这里。
//!
//! （03 的 Ask 映射不注册新方法：它在 `session/prompt` 的轮次循环里分流，见 [`super::ask`]。）
//!
//! 纪律：SDK 的 handler 在 dispatch loop 内执行并阻塞后续消息，因此**任何会等外部
//! 事件的活都必须 `cx.spawn(...)` 出去**（`session/new` 的 HTTP 往返、整轮 prompt、
//! load/resume 的回放、close 的收尾等待、模型热切换的 HTTP 往返都是）。

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::Agent;
use agent_client_protocol::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::Error;
use agent_client_protocol::Responder;
use agent_client_protocol::Stdio;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::AgentCapabilities;
use agent_client_protocol::schema::v1::AvailableCommand;
use agent_client_protocol::schema::v1::AvailableCommandsUpdate;
use agent_client_protocol::schema::v1::CancelNotification;
use agent_client_protocol::schema::v1::CloseSessionRequest;
use agent_client_protocol::schema::v1::CloseSessionResponse;
use agent_client_protocol::schema::v1::Implementation;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::schema::v1::InitializeResponse;
use agent_client_protocol::schema::v1::ListSessionsRequest;
use agent_client_protocol::schema::v1::ListSessionsResponse;
use agent_client_protocol::schema::v1::LoadSessionRequest;
use agent_client_protocol::schema::v1::LoadSessionResponse;
use agent_client_protocol::schema::v1::NewSessionRequest;
use agent_client_protocol::schema::v1::NewSessionResponse;
use agent_client_protocol::schema::v1::PromptRequest;
use agent_client_protocol::schema::v1::PromptResponse;
use agent_client_protocol::schema::v1::ResumeSessionRequest;
use agent_client_protocol::schema::v1::ResumeSessionResponse;
use agent_client_protocol::schema::v1::SessionCapabilities;
use agent_client_protocol::schema::v1::SessionCloseCapabilities;
use agent_client_protocol::schema::v1::SessionConfigOption;
use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::schema::v1::SessionInfo as SessionListEntry;
use agent_client_protocol::schema::v1::SessionListCapabilities;
use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::schema::v1::SessionResumeCapabilities;
use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::SetSessionConfigOptionRequest;
use agent_client_protocol::schema::v1::SetSessionConfigOptionResponse;
use agent_client_protocol::schema::v1::StopReason;

use super::AcpArgs;
use super::ask;
use super::model;
use super::session::Attached;
use super::session::HubError;
use super::session::NewSessionParams;
use super::session::SNAPSHOT_TIMEOUT;
use super::session::SessionHub;
use super::session::Turn;
use super::translate;
use crate::protocol::WingEvent;

/// 本步实现的 ACP 服务端（跑在 stdio 上，直到客户端关闭连接）。
///
/// 返回 `Ok(())` = stdin EOF（客户端关连接，干净退出）；`Err` = 网关断开或内部错误。
pub async fn serve(hub: Arc<SessionHub>, args: AcpArgs) -> Result<(), Error> {
    let defaults = Arc::new(SessionDefaults {
        template: args.agent,
        model: args.model,
    });

    // 每个 handler 各持一份 Arc（闭包是 FnMut，且要求 'static）。
    let hub_init = Arc::clone(&hub);
    let hub_new = Arc::clone(&hub);
    let hub_prompt = Arc::clone(&hub);
    let hub_cancel = Arc::clone(&hub);
    let hub_list = Arc::clone(&hub);
    let hub_load = Arc::clone(&hub);
    let hub_resume = Arc::clone(&hub);
    let hub_close = Arc::clone(&hub);
    let hub_config = Arc::clone(&hub);

    Agent
        .builder()
        .name("wing")
        .on_receive_request(
            async move |request: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _cx: ConnectionTo<Client>| {
                hub_init.register_client(
                    request.client_capabilities.clone(),
                    request.client_info.clone(),
                );
                responder.respond(initialize_response(&request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let hub = Arc::clone(&hub_new);
                let defaults = Arc::clone(&defaults);
                // `spawn` 自身要借用 `cx`，闭包里用的连接单独克隆一份。
                let task_cx = cx.clone();
                cx.spawn(async move {
                    match create_session(&hub, &defaults, request).await {
                        Ok((session_id, response)) => {
                            responder.respond(response)?;
                            // 应答之后再补发命令列表：此刻客户端已认识该 sessionId，
                            // 通知不会落到「未知会话」上。
                            if let Some(update) = available_commands_update(&hub).await {
                                task_cx.send_notification(SessionNotification::new(
                                    session_id,
                                    SessionUpdate::AvailableCommandsUpdate(update),
                                ))?;
                            }
                        }
                        Err(error) => responder.respond_with_error(error)?,
                    }
                    Ok(())
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest,
                        responder: Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let hub = Arc::clone(&hub_prompt);
                match translate::flatten_prompt(&request.prompt) {
                    Some(text) => {
                        // `spawn` 自身要借用 `cx`，闭包里用的连接单独克隆一份。
                        let task_cx = cx.clone();
                        // 在途响应凭据：应答之后才释放——WS 断开时进程要等它归零
                        // 才退出，否则客户端看不到「为什么这轮消失了」（见 session.rs）。
                        let pending = hub.pending_reply();
                        cx.spawn(async move {
                            match run_turn(&hub, &task_cx, request.session_id, text).await {
                                Ok(response) => responder.respond(response)?,
                                // `error` 事件终结轮次 = JSON-RPC error（与 stdio 前端同语义）。
                                Err(error) => responder.respond_with_error(error)?,
                            }
                            drop(pending);
                            Ok(())
                        })?;
                        Ok(())
                    }
                    None => {
                        // 拍平后为空：没有任何可投递给模型的内容。
                        responder
                            .respond_with_error(Error::invalid_params().data(
                                "prompt contains no usable content (text or resource links)",
                            ))?;
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _cx: ConnectionTo<Client>| {
                hub_cancel.request_cancel(&notification.session_id.to_string());
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: ListSessionsRequest,
                        responder: Responder<ListSessionsResponse>,
                        cx: ConnectionTo<Client>| {
                let hub = Arc::clone(&hub_list);
                cx.spawn(async move {
                    match list_sessions(&hub, request).await {
                        Ok(response) => responder.respond(response)?,
                        Err(error) => responder.respond_with_error(error)?,
                    }
                    Ok(())
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let hub = Arc::clone(&hub_load);
                // 在途响应凭据：应答之后才释放——WS 断开时进程要等它归零才退出
                // （与 session/prompt 同款，见 session.rs 的 PendingReplies）。
                let pending = hub.pending_reply();
                // `spawn` 自身要借用 `cx`，闭包里用的连接单独克隆一份。
                let task_cx = cx.clone();
                cx.spawn(async move {
                    log_ignored_request_fields(
                        request.mcp_servers.len(),
                        request.additional_directories.len(),
                    );
                    let session_key = request.session_id.to_string();
                    let result = restore_session(
                        &hub,
                        &task_cx,
                        request.session_id,
                        &request.cwd,
                        HistoryReplay::Load,
                    )
                    .await;
                    match result {
                        Ok(()) => {
                            // 04：模型 options 与标题 / 用量 / 命令同批，在响应之前。
                            let options = model_options(&hub, &session_key).await;
                            responder
                                .respond(LoadSessionResponse::new().config_options(options))?;
                        }
                        Err(error) => responder.respond_with_error(error)?,
                    }
                    drop(pending);
                    Ok(())
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest,
                        responder: Responder<ResumeSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let hub = Arc::clone(&hub_resume);
                let pending = hub.pending_reply();
                let task_cx = cx.clone();
                cx.spawn(async move {
                    log_ignored_request_fields(
                        request.mcp_servers.len(),
                        request.additional_directories.len(),
                    );
                    let session_key = request.session_id.to_string();
                    let result = restore_session(
                        &hub,
                        &task_cx,
                        request.session_id,
                        &request.cwd,
                        HistoryReplay::Skip,
                    )
                    .await;
                    match result {
                        Ok(()) => {
                            // 04：模型 options 与标题 / 用量 / 命令同批，在响应之前。
                            let options = model_options(&hub, &session_key).await;
                            responder
                                .respond(ResumeSessionResponse::new().config_options(options))?;
                        }
                        Err(error) => responder.respond_with_error(error)?,
                    }
                    drop(pending);
                    Ok(())
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        cx: ConnectionTo<Client>| {
                let hub = Arc::clone(&hub_config);
                // 在途响应凭据（与 load/resume 同款）：更新是一次 HTTP 往返，
                // WS 万一断开，进程要等错误帧入队才收尾。
                let pending = hub.pending_reply();
                cx.spawn(async move {
                    match model::set_config_option(&hub, &request).await {
                        Ok(options) => {
                            responder.respond(SetSessionConfigOptionResponse::new(options))?
                        }
                        Err(error) => responder.respond_with_error(error)?,
                    }
                    drop(pending);
                    Ok(())
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest,
                        responder: Responder<CloseSessionResponse>,
                        cx: ConnectionTo<Client>| {
                let hub = Arc::clone(&hub_close);
                let pending = hub.pending_reply();
                cx.spawn(async move {
                    hub.close_session(&request.session_id.to_string()).await;
                    responder.respond(CloseSessionResponse::new())?;
                    drop(pending);
                    Ok(())
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(Stdio::new(), async move |cx| {
            // 04：把连接句柄留给 hub——模型变更中继从事件分流路径发 `config_option_update`
            // （那条路径没有 handler 的 `cx`）。
            hub.register_client_connection(cx.clone());
            // 客户端关连接时通知 hub：WS 断开的收尾窗口据此尽早退出
            // （不做这件事也不影响正确性，只是收尾要等满窗口）。
            let watcher = cx.clone();
            let watcher_hub = Arc::clone(&hub);
            cx.spawn(async move {
                watcher.incoming_closed().await;
                watcher_hub.note_client_closed();
                Ok(())
            })?;

            // 两个终止条件：stdin EOF（正常收尾，退 0）与 WS 断开（报错，退 1）。
            tokio::select! {
                () = cx.incoming_closed() => Ok(()),
                () = hub.closed() => Err(Error::internal_error()
                    .data("gateway connection lost; wing acp is exiting")),
            }
        })
        .await
}

/// `session/new` 的进程级默认值。
struct SessionDefaults {
    template: Option<String>,
    model: Option<String>,
}

// ============================================================
// initialize
// ============================================================

/// 初始化应答：协议版本**固定回 v1**（不回显客户端请求的版本）+ 保守能力广告。
///
/// - 版本：本构建不做协议版本守卫——只开 `stdio` 时 SDK 的 `ProtocolCompat` 是
///   `unstable_protocol_v2` 门后的空实现（实测 `initialize{protocolVersion:2}` 会被
///   原样接受），所以这里自己把关。schema 对 `InitializeResponse.protocolVersion` 的
///   定义是「客户端指定的版本（若 agent 支持），否则 agent 支持的最新版本」——只支持
///   v1 就回 v1，客户端若不支持 v1，断不断由它判断。更高版本记 warn（不静默错答）。
/// - `promptCapabilities` 全 false：不广告 image / audio / embeddedContext（收到
///   未广告的 prompt 块会 warn 跳过，见 `translate::flatten_prompt`）；
/// - 会话生命周期能力见 [`agent_capabilities`]；
/// - `agentInfo` 是给客户端日志/关于页的可读身份。
fn initialize_response(request: &InitializeRequest) -> InitializeResponse {
    if request.protocol_version != ProtocolVersion::V1 {
        tracing::warn!(
            requested = %request.protocol_version,
            "acp: client requested a protocol version this build does not support; replying v1",
        );
    }
    InitializeResponse::new(ProtocolVersion::V1)
        .agent_capabilities(agent_capabilities())
        .agent_info(Implementation::new("wing", agent_version()))
}

/// `agentInfo` 的版本串：与 `wing --version` 同口径（版本 + 短 commit）。
///
/// workspace 版本恒为 `0.0.0`，裸版本号在客户端的 About / 日志里没有信息量；
/// commit hash 由 `build.rs` 注入（`WING_COMMIT_HASH`）。
fn agent_version() -> String {
    format!(
        "{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("WING_COMMIT_HASH")
    )
}

/// agent 能力广告（05 步：会话生命周期落地后升级）。
///
/// - `loadSession: true`：支持 `session/load`（回放历史为 update 序列）；
/// - `sessionCapabilities { list, resume, close }`：三种会话生命周期方法（`{}` = 支持）；
/// - **不广告** `delete`（wing 无删除 API）与 `additionalDirectories`（单 workspace）；
/// - `promptCapabilities` 保持全 false（不广告 image / audio / embeddedContext）。
fn agent_capabilities() -> AgentCapabilities {
    AgentCapabilities::new()
        .load_session(true)
        .session_capabilities(
            SessionCapabilities::new()
                .list(SessionListCapabilities::new())
                .resume(SessionResumeCapabilities::new())
                .close(SessionCloseCapabilities::new()),
        )
}

// ============================================================
// session/new
// ============================================================

/// 建 ACP 会话：校验 cwd → 建 wing 会话 → 返回（sessionId, 应答）。
///
/// 命令列表由调用方在应答之后补发（见 handler）。
async fn create_session(
    hub: &Arc<SessionHub>,
    defaults: &SessionDefaults,
    request: NewSessionRequest,
) -> Result<(SessionId, NewSessionResponse), Error> {
    let cwd = validate_cwd(&request.cwd)?;
    log_ignored_request_fields(
        request.mcp_servers.len(),
        request.additional_directories.len(),
    );

    let session_id = hub
        .new_session(NewSessionParams {
            workspace: cwd,
            template: defaults.template.clone(),
            model: defaults.model.clone(),
        })
        .await
        .map_err(hub_error)?;

    tracing::info!(session_id = %session_id, "acp: session created");
    // 04：模型 config options（id=model / category=model / select，值域 `provider:model`）。
    let options = model_options(hub, &session_id).await;
    let session_id = SessionId::new(session_id);
    Ok((
        session_id.clone(),
        NewSessionResponse::new(session_id).config_options(options),
    ))
}

/// 会话的模型 config options（04）：取不到 / 空表都**不广告**（返回 None），只记日志。
///
/// 建会话 / 打开线程是主目的：模型目录或会话状态的一次失败不该把整个会话打掉。缺了
/// options 的客户端只是没有模型下拉，下一次 `session/load` 或外部变更的中继都会重新构造。
async fn model_options(
    hub: &Arc<SessionHub>,
    session_id: &str,
) -> Option<Vec<SessionConfigOption>> {
    match model::options_for(hub, session_id).await {
        Ok(options) if !options.is_empty() => Some(options),
        Ok(_) => {
            tracing::info!(
                session_id,
                "acp: the gateway lists no models; config options not advertised"
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                session_id,
                error = %err,
                "acp: model config options unavailable; not advertised"
            );
            None
        }
    }
}

/// cwd 校验：ACP 要求绝对路径；相对/不存在都会让后续工具落到意外目录。
fn validate_cwd(cwd: &std::path::Path) -> Result<PathBuf, Error> {
    if !cwd.is_absolute() {
        return Err(Error::invalid_params()
            .data(format!("cwd must be an absolute path: {}", cwd.display())));
    }
    if !cwd.is_dir() {
        return Err(Error::invalid_params().data(format!(
            "cwd does not exist or is not a directory: {}",
            cwd.display()
        )));
    }
    Ok(cwd.to_path_buf())
}

/// `/api/commands` → `available_commands_update`（客户端据此提供 `/命令` 补全）。
///
/// 命令调用不需要额外代码：客户端把 `/name args` 当 prompt 发来，网关自己会展开
/// prompt 命令（`SessionManager._post`）。
async fn available_commands_update(hub: &Arc<SessionHub>) -> Option<AvailableCommandsUpdate> {
    let response = match hub.http.get_commands().await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(error = %err, "acp: /api/commands failed; commands not advertised");
            return None;
        }
    };
    let commands = response
        .commands
        .into_iter()
        .map(|command| {
            let description = if command.params.trim().is_empty() {
                command.description
            } else {
                // 参数提示并进描述：ACP 的 AvailableCommand 只有 name/description/input。
                format!("{} {}", command.description, command.params)
                    .trim()
                    .to_string()
            };
            AvailableCommand::new(command.name, description)
        })
        .collect();
    Some(AvailableCommandsUpdate::new(commands))
}

// ============================================================
// session/list
// ============================================================

/// `session/list` 的一页大小（任务书允许 ≤100；Zed 逐页请求，缺 `nextCursor` 即结束）。
const LIST_PAGE_SIZE: usize = 100;

/// `GET /api/session/list` → 过滤 / 分页 / 映射。
///
/// 网关已给出排序（活跃优先 + 时间降序），这里只做协议侧的形状转换。
async fn list_sessions(
    hub: &Arc<SessionHub>,
    request: ListSessionsRequest,
) -> Result<ListSessionsResponse, Error> {
    let response = hub.http.list_sessions().await.map_err(|err| {
        tracing::warn!(error = %err, "acp: /api/session/list failed");
        hub_error(HubError::Gateway(err.to_string()))
    })?;
    list_page(
        response.sessions,
        request.cwd.as_deref(),
        request.cursor.as_deref(),
    )
}

/// `session/list` 的过滤 / 分页 / 映射（纯函数，便于单测）。
///
/// - `cwd` 存在时按 workspace **精确匹配**（`PathBuf` 按组件比较：尾斜杠、冗余分隔符
///   不影响判定）；
/// - `workspace` 缺失或不是绝对路径 → 跳过（ACP 要求 `cwd` 是绝对路径）并记 debug；
/// - `cursor` 是不透明字符串（本实现 = 十进制 offset）；非法 → invalid params；
/// - 页大小 [`LIST_PAGE_SIZE`]；还有余量时回 `nextCursor`，否则缺省 = 结束。
///
/// **已知取舍**：每页都重新拉全量列表再按 offset 切——页间列表若发生变动
/// （网关按「活跃优先 + 时间降序」排序，新会话或活跃度变化都会重排），会出现重复项或漏项。
/// 会话规模小的时候无感；要更稳就把游标换成「上一页最后一条的 session_id」这类稳定锚点
/// （需要定义锚点消失时的回退语义），暂不做。
fn list_page(
    sessions: Vec<wing_api_client::models::SessionInfo>,
    cwd: Option<&std::path::Path>,
    cursor: Option<&str>,
) -> Result<ListSessionsResponse, Error> {
    let offset = parse_list_cursor(cursor)?;
    let entries: Vec<SessionListEntry> = sessions
        .iter()
        .filter_map(|session| session_entry(session, cwd))
        .collect();
    let total = entries.len();
    let page: Vec<SessionListEntry> = entries
        .into_iter()
        .skip(offset)
        .take(LIST_PAGE_SIZE)
        .collect();
    let consumed = offset + page.len();
    let next_cursor = (consumed < total).then(|| consumed.to_string());
    Ok(ListSessionsResponse::new(page).next_cursor(next_cursor))
}

/// 不透明 cursor → offset（十进制；非法 → invalid params）。
fn parse_list_cursor(cursor: Option<&str>) -> Result<usize, Error> {
    match cursor {
        None => Ok(0),
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| Error::invalid_params().data(format!("invalid cursor: {raw}"))),
    }
}

/// 一条网关会话 → ACP `SessionInfo`；`None` = 跳过（见 [`list_page`]）。
fn session_entry(
    session: &wing_api_client::models::SessionInfo,
    cwd: Option<&std::path::Path>,
) -> Option<SessionListEntry> {
    let workspace = match session
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty())
        .map(PathBuf::from)
    {
        Some(workspace) if workspace.is_absolute() => workspace,
        _ => {
            tracing::debug!(
                session_id = %session.id,
                "acp: session without an absolute workspace; not listed",
            );
            return None;
        }
    };
    if cwd.is_some_and(|cwd| cwd != workspace.as_path()) {
        return None;
    }
    let mut entry = SessionListEntry::new(session.id.clone(), workspace);
    if let Some(title) = session
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        entry = entry.title(title.to_string());
    }
    if let Some(updated_at) = session.last_interaction.as_deref().and_then(rfc3339_local) {
        entry = entry.updated_at(updated_at);
    }
    Some(entry)
}

/// 网关的 `last_interaction`（本地 naive ISO，`datetime.now().isoformat()`）→ RFC3339。
///
/// 解析失败 / 时钟歧义（DST 回拨）→ `None`（ACP 的 `updatedAt` 可选，宁缺毋滥）。
fn rfc3339_local(raw: &str) -> Option<String> {
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    naive
        .and_local_timezone(chrono::Local)
        .earliest()
        .map(|at| at.to_rfc3339())
}

// ============================================================
// session/load · session/resume
// ============================================================

/// 挂载后是否回放历史快照（`session/load` 回放；`session/resume` 不回放）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryReplay {
    /// 读 `sync_session` 快照 → 投影成 update 序列。
    Load,
    /// 不回放：挂载后直接补标题 / 用量 / 命令。
    Skip,
}

/// `session/load` / `session/resume` 的共同流程（帧序见 `session.rs::attach_session`）：
///
/// 1. `POST /api/session/resume`：校验会话存在（404 → invalid params）+ 水合进网关内存；
/// 2. `hub.attach_session`：入表 + 取 gate + **arm 事件接收端**（必须早于订阅）；
/// 3. `hub.subscribe_session`：订阅；网关随即推一份 `sync_session` 快照；
/// 4. （load）读快照 → 逐条回放投影；
/// 5. 标题 / 用量 / 命令（`GET /api/session/info` + `/api/commands`）；
/// 6. disarm（drop [`Attached`]）→ 调用方回响应（**回放完成前不响应**）。
async fn restore_session(
    hub: &Arc<SessionHub>,
    cx: &ConnectionTo<Client>,
    session_id: SessionId,
    cwd: &std::path::Path,
    replay: HistoryReplay,
) -> Result<(), Error> {
    let key = session_id.to_string();
    let mut attached = attach_existing(hub, &key, cwd).await?;

    if replay == HistoryReplay::Load {
        let snapshot = attached
            .read_snapshot(SNAPSHOT_TIMEOUT)
            .await
            .map_err(hub_error)?;
        for update in attached.replay_updates(&snapshot) {
            send_update(cx, &session_id, update)?;
        }
    }

    for update in session_facts(hub, &key).await {
        send_update(cx, &session_id, update)?;
    }
    if let Some(update) = available_commands_update(hub).await {
        send_update(
            cx,
            &session_id,
            SessionUpdate::AvailableCommandsUpdate(update),
        )?;
    }

    // 卸载：回到空闲语义（事件丢弃，下一个 prompt 自己再 arm）。
    drop(attached);
    Ok(())
}

/// resume（校验 + 水合）→ 挂载（arm + subscribe）；返回挂载句柄（持有 gate 与事件接收端）。
///
/// 顺序即语义：`resume` 先校验存在性（404 → invalid params），`attach_and_subscribe`
/// 再把「先 arm 后 subscribe」封死（订阅会立刻推 `sync_session` 快照，晚 arm 就没有
/// 消费者，回放无从谈起）。
async fn attach_existing(
    hub: &Arc<SessionHub>,
    session_id: &str,
    cwd: &std::path::Path,
) -> Result<Attached, Error> {
    let resumed = hub.http.resume_session(session_id).await.map_err(|err| {
        if err.is_not_found() {
            // 未知会话：客户端拿 invalid params（与 hub 的 UnknownSession 同码）。
            Error::invalid_params().data(format!("unknown session: {session_id}"))
        } else {
            hub_error(HubError::Gateway(err.to_string()))
        }
    })?;
    if let Some(workspace) = resumed.workspace.as_deref().map(PathBuf::from)
        && workspace.is_absolute()
        && workspace.as_path() != cwd
    {
        // 路径写法差异（符号链接 / 尾斜杠）不该打断打开线程：只记日志。
        tracing::info!(
            session_id,
            requested = %cwd.display(),
            workspace = %workspace.display(),
            "acp: load/resume cwd differs from the session workspace",
        );
    }

    let attached = hub
        .attach_and_subscribe(session_id)
        .await
        .map_err(hub_error)?;
    Ok(attached)
}

/// 会话的标题与上下文用量（`GET /api/session/info`）。
///
/// 取不到只记 warn：标题/用量是补充信息，不该让 load/resume 整体失败。
async fn session_facts(hub: &Arc<SessionHub>, session_id: &str) -> Vec<SessionUpdate> {
    match hub.http.get_session_info(session_id).await {
        Ok(info) => translate::session_info_updates(
            info.session_name.as_deref(),
            info.total_tokens,
            info.context_window_tokens,
        ),
        Err(err) => {
            tracing::warn!(
                session_id,
                error = %err,
                "acp: /api/session/info failed; title/usage not sent",
            );
            Vec::new()
        }
    }
}

/// 发一条 `session/update` 通知（失败记 warn 后继续向上抛：连接坏掉时调用方收口）。
fn send_update(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    update: SessionUpdate,
) -> Result<(), Error> {
    cx.send_notification(SessionNotification::new(session_id.clone(), update))
        .map_err(|err| {
            tracing::warn!(error = %err, "acp: failed to send session/update");
            err
        })
}

/// 记录本构建不桥接的请求字段（MCP / 额外工作目录）：明确记日志，别让客户端以为生效了。
fn log_ignored_request_fields(mcp_servers: usize, additional_directories: usize) {
    if mcp_servers > 0 {
        tracing::info!(
            count = mcp_servers,
            "acp: mcpServers ignored (MCP bridge not implemented)"
        );
    }
    if additional_directories > 0 {
        tracing::info!(
            count = additional_directories,
            "acp: additionalDirectories ignored (single workspace per session)"
        );
    }
}

// ============================================================
// session/prompt
// ============================================================

/// 在途 ask 的收口方式（[`run_turn`] 的等待结果）。
enum AskReply {
    /// 需要回写这个答案（`None` = 事件不可应答，只记日志）。
    Write(Option<ask::AskOutcome>),
    /// 本会话已请求取消：不回写（feedback waiter 已被 interrupt 清除）。
    Abandoned,
}

/// 驱动一轮 prompt，直到终态（或错误）。
///
/// 事件流：`turn.next_event()` → [`Turn::updates_for`] 翻译 → `session/update` 通知；
/// `turn_end` 判定终态（`turn_result` / `interrupted` / `error`）。
async fn run_turn(
    hub: &Arc<SessionHub>,
    cx: &ConnectionTo<Client>,
    session_id: SessionId,
    text: String,
) -> Result<PromptResponse, Error> {
    let session_key = session_id.to_string();
    let mut turn: Turn = hub
        .begin_turn(&session_key, &text)
        .await
        .map_err(hub_error)?;

    loop {
        let Some(event) = turn.next_event().await else {
            // 事件流终止（WS 断开 / hub 收尾）：报可诊断的错误，让客户端看到原因。
            return Err(
                Error::internal_error().data("gateway event stream ended before the turn finished")
            );
        };

        // Ask 事件：按形态分流到 permission / elicitation / 回退三径（见 `ask` 模块），
        // 答案经 WS `ClientRequest{content, tool_call_id}` 定向 resolve feedback waiter。
        //
        // 就地 await（见 `ask` 模块与 design D5）：同一会话至多一个在途 agent→client
        // 请求，后续 ask 留在会话事件缓冲里排队（与 TUI 的 ask 面板 FIFO 同语义）。
        // 等待期间后端轮次被 feedback waiter 阻塞，不会有事件堆积。
        //
        // 等客户端作答时**同时等事件流终止**（review r1 N-1）：WS 一断就用默认答案收口，
        // 让轮次走到下面 `next_event() == None` 的「gateway event stream ended」错误分支——
        // 否则 `session/prompt` 会一直卡在这条客户端请求上，只以连接消失告终（无可诊断错误）。
        if let WingEvent::Ask { tool_call_id, .. } = &event {
            // 三路收口：客户端作答 / 事件流已死（默认答案）/ **本会话已请求取消**。
            //
            // 取消路：网关 interrupt 已经（或正在）清掉后端的 feedback waiter——迟到的
            // 客户端应答不能再回写：网关对无 waiter 的定向消息会 fallthrough 成普通用户
            // 消息（幽灵轮次 + 持久化历史污染）。丢弃它，放行事件循环去消费随后到达的
            // `interrupted`，本轮以 `cancelled` 收口（客户端不关权限卡/表单也不再悬挂）。
            let reply = tokio::select! {
                // `biased` + 取消臂在前：取消已置位时优先走取消路——取消后丢弃应答永远
                // 安全（轮次本来要以 `cancelled` 收口），随机挑臂没有语义收益。
                biased;
                () = turn.cancel_requested() => AskReply::Abandoned,
                outcome = ask::resolve(hub, cx, &session_id, &event) => AskReply::Write(outcome),
                () = hub.stream_dead() => {
                    tracing::warn!(
                        session_id = %session_key,
                        tool_call_id = %tool_call_id,
                        "acp: gateway event stream died while an ask was pending; answering with the default"
                    );
                    AskReply::Write(ask::default_answer(&event))
                }
            };
            match reply {
                AskReply::Write(Some(outcome)) => {
                    if outcome.elicitation_unsupported {
                        // elicitation 被客户端回 `-32601`：进程级粘性降级，
                        // 此后所有 ask 直接走回退路径。
                        hub.downgrade_elicitation();
                    }
                    if let Err(err) = turn.answer_ask(tool_call_id, &outcome.answer).await {
                        tracing::warn!(error = %err, "acp: failed to answer ask");
                    }
                }
                // 不可应答（空 tool_call_id —— `ask::classify` 已记 warn）。
                AskReply::Write(None) => tracing::warn!(
                    session_id = %session_key,
                    "acp: ask event left unanswered"
                ),
                AskReply::Abandoned => tracing::debug!(
                    session_id = %session_key,
                    tool_call_id = %tool_call_id,
                    "acp: ask abandoned after cancel; the turn will close as cancelled"
                ),
            }
        }

        for update in turn.updates_for(&event) {
            cx.send_notification(SessionNotification::new(session_id.clone(), update))
                .map_err(|err| {
                    tracing::warn!(error = %err, "acp: failed to send session/update");
                    err
                })?;
        }

        let Some(end) = translate::turn_end(&event) else {
            continue;
        };

        // 终态之后、把 gate 还回去之前，先把**本轮尾帧**消化掉：后端的异常帧序是
        // `turn_result → error → done`，只消费第一个终态的话，那条 `error` 会被下一个
        // 排队 prompt 当成自己的终态（见 `Turn::drain_trailing` 与 design 的 S3）。
        for trailing in turn.drain_trailing().await {
            for update in turn.updates_for(&trailing) {
                cx.send_notification(SessionNotification::new(session_id.clone(), update))
                    .map_err(|err| {
                        tracing::warn!(error = %err, "acp: failed to send session/update");
                        err
                    })?;
            }
        }

        return match end {
            translate::TurnEnd::EndTurn => Ok(PromptResponse::new(StopReason::EndTurn)),
            translate::TurnEnd::Cancelled => Ok(PromptResponse::new(StopReason::Cancelled)),
            translate::TurnEnd::Failed(message) => Err(Error::internal_error().data(message)),
        };
    }
}

// ============================================================
// hub 错误 → JSON-RPC error
// ============================================================

/// hub 层失败 → ACP JSON-RPC error（未知会话 = invalid params；其余 = internal error）。
fn hub_error(error: HubError) -> Error {
    match error {
        HubError::UnknownSession(_) => Error::invalid_params().data(error.to_string()),
        HubError::Gateway(_) | HubError::Disconnected | HubError::Timeout(_) => {
            Error::internal_error().data(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ErrorCode;
    use agent_client_protocol::schema::v1::SessionConfigSelectOption;
    use agent_client_protocol::schema::v1::SessionConfigSelectOptions;
    use serde_json::json;

    /// 04：`configOptions` 缺席 = 不广告（不是空数组）；有值时是我们在 `model` 模块里
    /// 构造的那份（`session/new` / `load` / `resume` 三个响应共用同一形状）。
    #[test]
    fn session_responses_carry_the_model_option_only_when_advertised() {
        let bare = serde_json::to_value(NewSessionResponse::new(SessionId::new("s1")))
            .expect("response serializes");
        assert_eq!(bare["sessionId"], "s1");
        assert!(
            bare.get("configOptions").is_none(),
            "取不到目录 / 没有值时不发 configOptions 字段"
        );

        let option = SessionConfigOption::select(
            "model",
            "Model",
            "dashscope:glm-4.6",
            SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                "dashscope:glm-4.6",
                "GLM-4.6",
            )]),
        );
        let advertised = serde_json::to_value(
            NewSessionResponse::new(SessionId::new("s1")).config_options(Some(vec![option])),
        )
        .expect("response serializes");
        assert_eq!(advertised["configOptions"][0]["id"], "model");
        assert_eq!(
            advertised["configOptions"][0]["currentValue"],
            "dashscope:glm-4.6"
        );

        // load / resume 的响应形状同款（字段名 `configOptions`）。
        let loaded = serde_json::to_value(LoadSessionResponse::new()).expect("serializes");
        assert!(loaded.get("configOptions").is_none());
        let resumed = serde_json::to_value(ResumeSessionResponse::new()).expect("serializes");
        assert!(resumed.get("configOptions").is_none());
    }

    #[test]
    fn initialize_replies_v1_and_advertises_session_lifecycle() {
        let request: InitializeRequest = serde_json::from_value(json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": true, "writeTextFile": true},
                "terminal": true
            },
            "clientInfo": {"name": "zed", "version": "0.190.0"}
        }))
        .expect("initialize fixture decodes");

        let response = initialize_response(&request);
        assert_eq!(response.protocol_version, ProtocolVersion::V1);
        assert!(
            response.agent_capabilities.load_session,
            "session/load 已实现，必须广告 loadSession"
        );
        let session = &response.agent_capabilities.session_capabilities;
        assert!(session.list.is_some(), "session/list 需要广告");
        assert!(session.resume.is_some(), "session/resume 需要广告");
        assert!(session.close.is_some(), "session/close 需要广告");
        assert!(
            session.delete.is_none(),
            "wing 没有删除 API，不广告 session/delete"
        );
        assert!(
            session.additional_directories.is_none(),
            "单 workspace 会话，不广告 additionalDirectories"
        );
        assert!(
            !response.agent_capabilities.prompt_capabilities.image
                && !response.agent_capabilities.prompt_capabilities.audio
                && !response
                    .agent_capabilities
                    .prompt_capabilities
                    .embedded_context,
            "prompt capabilities must stay false until images are supported"
        );
        let info = response.agent_info.expect("agentInfo is advertised");
        assert_eq!(info.name, "wing");
        assert_eq!(
            info.version,
            format!(
                "{} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("WING_COMMIT_HASH")
            ),
            "agentInfo 与 `wing --version` 同口径（版本 + 短 commit）"
        );
    }

    #[test]
    fn initialize_with_a_newer_protocol_version_still_replies_v1() {
        // 本构建不做版本守卫：更高版本既不能 echo 回去（那等于声称支持 v2），
        // 也不能静默错答——固定回 v1，客户端自己判断要不要继续。
        let request: InitializeRequest = serde_json::from_value(json!({
            "protocolVersion": 2,
            "clientCapabilities": {},
        }))
        .expect("initialize fixture decodes");

        let response = initialize_response(&request);
        assert_eq!(response.protocol_version, ProtocolVersion::V1);
    }

    #[test]
    fn cwd_must_be_absolute_and_exist() {
        let err = validate_cwd(std::path::Path::new("relative/dir")).expect_err("rejects relative");
        assert_eq!(
            err.code,
            agent_client_protocol::schema::v1::ErrorCode::InvalidParams
        );

        let missing = std::path::Path::new("/nonexistent/wing-acp-cwd");
        assert!(validate_cwd(missing).is_err());

        let here = std::env::current_dir().expect("cwd");
        assert_eq!(validate_cwd(&here).expect("absolute dir is accepted"), here);
    }

    #[test]
    fn hub_errors_map_to_jsonrpc_codes() {
        use agent_client_protocol::schema::v1::ErrorCode;
        assert_eq!(
            hub_error(HubError::UnknownSession("s1".into())).code,
            ErrorCode::InvalidParams
        );
        assert_eq!(
            hub_error(HubError::Disconnected).code,
            ErrorCode::InternalError
        );
        assert_eq!(
            hub_error(HubError::Gateway("500".into())).code,
            ErrorCode::InternalError
        );
        assert_eq!(
            hub_error(HubError::Timeout("snapshot".into())).code,
            ErrorCode::InternalError
        );
    }

    // ---- session/list ----

    /// 网关 `SessionInfo` fixture（只填本步用到的字段）。
    fn api_session(
        id: &str,
        name: Option<&str>,
        workspace: Option<&str>,
        last_interaction: Option<&str>,
    ) -> wing_api_client::models::SessionInfo {
        wing_api_client::models::SessionInfo {
            id: id.to_string(),
            name: name.map(str::to_string),
            created_at: None,
            template_name: None,
            workspace: workspace.map(str::to_string),
            last_interaction: last_interaction.map(str::to_string),
            status: "inactive".to_string(),
            tags: Vec::new(),
            tag_meta: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn list_maps_fields_and_skips_sessions_without_an_absolute_workspace() {
        let response = list_page(
            vec![
                api_session(
                    "s1",
                    Some("修 ACP 前端"),
                    Some("/Users/x/proj"),
                    Some("2026-01-02T03:04:05.123456"),
                ),
                api_session(
                    "s2",
                    Some("没有 workspace"),
                    None,
                    Some("2026-01-02T03:04:05"),
                ),
                api_session("s3", None, Some("relative/dir"), None),
                api_session("s4", Some("空 workspace"), Some("   "), None),
            ],
            None,
            None,
        )
        .expect("no cursor → Ok");

        assert_eq!(
            response.sessions.len(),
            1,
            "无 / 非绝对 workspace 的会话跳过"
        );
        assert!(response.next_cursor.is_none(), "一页装得下 → 没有下一页");
        let entry = &response.sessions[0];
        assert_eq!(entry.session_id.to_string(), "s1");
        assert_eq!(entry.cwd, PathBuf::from("/Users/x/proj"));
        assert_eq!(entry.title.as_deref(), Some("修 ACP 前端"));
        assert!(entry.additional_directories.is_empty());
        let updated_at = entry.updated_at.as_deref().expect("updatedAt 已转换");
        let parsed = chrono::DateTime::parse_from_rfc3339(updated_at)
            .unwrap_or_else(|err| panic!("updatedAt 必须是 RFC3339: {updated_at} ({err})"));
        // 本地 naive ISO 转回 RFC3339：墙上时间不变。
        assert_eq!(
            parsed
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string(),
            "2026-01-02T03:04:05"
        );
    }

    #[test]
    fn list_keeps_sessions_with_unparsable_timestamps_and_blank_names() {
        let response = list_page(
            vec![api_session(
                "s1",
                Some("   "),
                Some("/tmp/x"),
                Some("not-a-timestamp"),
            )],
            None,
            None,
        )
        .expect("no cursor → Ok");

        assert_eq!(response.sessions.len(), 1, "时间戳坏掉不丢会话");
        let entry = &response.sessions[0];
        assert!(entry.updated_at.is_none(), "解析失败 → 不发 updatedAt");
        assert_eq!(entry.title, None, "空白 name 视同没有标题");
    }

    #[test]
    fn list_filters_by_cwd_exactly_but_tolerates_path_spelling() {
        let sessions = || {
            vec![
                api_session("s1", None, Some("/work/a"), None),
                api_session("s2", None, Some("/work/b"), None),
                api_session("s3", None, Some("/work/a/sub"), None),
            ]
        };

        let all = list_page(sessions(), None, None).expect("no cursor");
        assert_eq!(all.sessions.len(), 3, "不带 cwd = 不过滤");

        let filtered =
            list_page(sessions(), Some(std::path::Path::new("/work/a")), None).expect("no cursor");
        assert_eq!(filtered.sessions.len(), 1);
        assert_eq!(filtered.sessions[0].session_id.to_string(), "s1");

        // `PathBuf` 按组件比较：尾斜杠 / 冗余分隔符不影响匹配。
        let sloppy =
            list_page(sessions(), Some(std::path::Path::new("/work/a/")), None).expect("no cursor");
        assert_eq!(sloppy.sessions.len(), 1);
        assert_eq!(sloppy.sessions[0].session_id.to_string(), "s1");

        // 前缀不匹配（`/work/a` 不会命中 `/work/a/sub`）。
        let sub = list_page(sessions(), Some(std::path::Path::new("/work/a/sub")), None)
            .expect("no cursor");
        assert_eq!(sub.sessions.len(), 1);
        assert_eq!(sub.sessions[0].session_id.to_string(), "s3");
    }

    #[test]
    fn list_paginates_with_an_opaque_cursor() {
        let sessions: Vec<_> = (0..LIST_PAGE_SIZE + 3)
            .map(|i| api_session(&format!("s{i:03}"), None, Some("/work/a"), None))
            .collect();

        let first = list_page(sessions.clone(), None, None).expect("first page");
        assert_eq!(first.sessions.len(), LIST_PAGE_SIZE);
        assert_eq!(first.sessions[0].session_id.to_string(), "s000");
        assert_eq!(
            first.next_cursor.as_deref(),
            Some(LIST_PAGE_SIZE.to_string().as_str()),
            "还有余量 → 回 nextCursor"
        );

        let second =
            list_page(sessions.clone(), None, first.next_cursor.as_deref()).expect("second page");
        assert_eq!(second.sessions.len(), 3);
        assert_eq!(
            second.sessions[0].session_id.to_string(),
            format!("s{LIST_PAGE_SIZE:03}")
        );
        assert!(second.next_cursor.is_none(), "最后一页不带上游 cursor");

        // offset 超出总数：空页 + 结束（不是错误）。
        let beyond = list_page(sessions, None, Some("1000")).expect("beyond the end");
        assert!(beyond.sessions.is_empty());
        assert!(beyond.next_cursor.is_none());
    }

    #[test]
    fn list_rejects_an_invalid_cursor() {
        let err = list_page(vec![], None, Some("not-a-cursor")).expect_err("invalid cursor");
        assert_eq!(err.code, ErrorCode::InvalidParams);
        // 空串同样非法（cursor 是网关发出的不透明值，不会为空）。
        assert!(list_page(vec![], None, Some("")).is_err());
    }

    #[test]
    fn local_naive_timestamps_convert_to_rfc3339() {
        for raw in [
            "2026-01-02T03:04:05",
            "2026-01-02T03:04:05.123456",
            "2026-01-02T03:04:05.123",
        ] {
            let rfc3339 = rfc3339_local(raw).unwrap_or_else(|| panic!("{raw} 应当可转换"));
            assert!(chrono::DateTime::parse_from_rfc3339(&rfc3339).is_ok());
        }
        for raw in ["", "2026-01-02", "2026-01-02 03:04:05", "garbage"] {
            assert!(rfc3339_local(raw).is_none(), "{raw:?} 应当拒绝");
        }
    }
}
