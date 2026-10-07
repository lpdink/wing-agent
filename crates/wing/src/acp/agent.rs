//! ACP handler 注册与轮次驱动（`wing acp` 的协议面）。
//!
//! 本步注册四个方法；其余方法一律不注册（SDK 自动回 `method not found`）：
//!
//! | 方法 | 形态 | 要点 |
//! |------|------|------|
//! | `initialize` | request | 回客户端请求的协议版本（SDK 协议守卫已保证是 v1）+ `agentInfo`；能力全保守（见 [`agent_capabilities`]） |
//! | `session/new` | request | 用客户端 `cwd` 建 wing 会话（workspace），忽略 `mcpServers` / `additionalDirectories`；应答后补发 `available_commands_update` |
//! | `session/prompt` | request | 拍平 prompt → WS 投递 → 事件流转 update → 终态收口 |
//! | `session/cancel` | notification | → HTTP interrupt；在途轮次等 `interrupted` 事件回 `stopReason: "cancelled"` |
//!
//! **后续步骤在这里追加**（03 ask / 04 model / 05 sessions）：`set_config_option`、
//! `list`/`load`/`resume`/`close` 都是同形的 `on_receive_request` 注册；handler 里
//! 可以拿 `hub`（`Arc<SessionHub>`）与 handler 自带的 `cx: ConnectionTo<Client>`。
//!
//! 纪律：SDK 的 handler 在 dispatch loop 内执行并阻塞后续消息，因此**任何会等外部
//! 事件的活都必须 `cx.spawn(...)` 出去**（`session/new` 的 HTTP 往返、整轮 prompt 都是）。

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::Agent;
use agent_client_protocol::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::Error;
use agent_client_protocol::Responder;
use agent_client_protocol::Stdio;
use agent_client_protocol::schema::v1::AgentCapabilities;
use agent_client_protocol::schema::v1::AvailableCommand;
use agent_client_protocol::schema::v1::AvailableCommandsUpdate;
use agent_client_protocol::schema::v1::CancelNotification;
use agent_client_protocol::schema::v1::Implementation;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::schema::v1::InitializeResponse;
use agent_client_protocol::schema::v1::NewSessionRequest;
use agent_client_protocol::schema::v1::NewSessionResponse;
use agent_client_protocol::schema::v1::PromptRequest;
use agent_client_protocol::schema::v1::PromptResponse;
use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::StopReason;

use super::AcpArgs;
use super::session::HubError;
use super::session::NewSessionParams;
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
        .connect_with(Stdio::new(), async move |cx| {
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

/// 本步的能力广告：**全部保守**。
///
/// - `promptCapabilities` 全 false：不广告 image / audio / embeddedContext（收到
///   未广告的 prompt 块会 warn 跳过，见 `translate::flatten_prompt`）；
/// - `loadSession` 与 `sessionCapabilities`（resume/close/list）留给 05 步；
/// - `agentInfo` 是给客户端日志/关于页的可读身份。
fn initialize_response(request: &InitializeRequest) -> InitializeResponse {
    InitializeResponse::new(request.protocol_version)
        .agent_capabilities(agent_capabilities())
        .agent_info(Implementation::new("wing", env!("CARGO_PKG_VERSION")))
}

/// 本步的 agent 能力（05 步在这里升级）。
fn agent_capabilities() -> AgentCapabilities {
    AgentCapabilities::new()
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
    if !request.mcp_servers.is_empty() {
        // 本步不桥接 MCP（顶层任务书 Out of Scope）：明确记日志，别让客户端以为生效了。
        tracing::info!(
            count = request.mcp_servers.len(),
            "acp: mcpServers ignored (MCP bridge not implemented)"
        );
    }
    if !request.additional_directories.is_empty() {
        tracing::info!(
            count = request.additional_directories.len(),
            "acp: additionalDirectories ignored (single workspace per session)"
        );
    }

    let session_id = hub
        .new_session(NewSessionParams {
            workspace: cwd,
            template: defaults.template.clone(),
            model: defaults.model.clone(),
        })
        .await
        .map_err(hub_error)?;

    // 04 步：这里补 configOptions（id=model / category=model / select / provider:model 值域）。
    tracing::info!(session_id = %session_id, "acp: session created");
    let session_id = SessionId::new(session_id);
    Ok((session_id.clone(), NewSessionResponse::new(session_id)))
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
// session/prompt
// ============================================================

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

        // Ask 占位：立即应答以免轮次卡死（详见 `translate::ask_placeholder_answer`）。
        // 03 步替换：Bash 确认 → session/request_permission；AskUserQuestion →
        // elicitation/create（能力门控），回答同样经 WS ClientRequest 回写。
        if let (WingEvent::Ask { tool_call_id, .. }, Some(answer)) =
            (&event, translate::ask_placeholder_answer(&event))
        {
            tracing::warn!(
                session_id = %session_key,
                tool_call_id = %tool_call_id,
                "acp: ask event placeholder-answered (03 step replaces this)"
            );
            if !tool_call_id.is_empty()
                && let Err(err) = turn.answer_ask(tool_call_id, &answer).await
            {
                tracing::warn!(error = %err, "acp: failed to answer ask");
            }
        }

        for update in turn.updates_for(&event) {
            cx.send_notification(SessionNotification::new(session_id.clone(), update))
                .map_err(|err| {
                    tracing::warn!(error = %err, "acp: failed to send session/update");
                    err
                })?;
        }

        match translate::turn_end(&event) {
            Some(translate::TurnEnd::EndTurn) => {
                return Ok(PromptResponse::new(StopReason::EndTurn));
            }
            Some(translate::TurnEnd::Cancelled) => {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            Some(translate::TurnEnd::Failed(message)) => {
                return Err(Error::internal_error().data(message));
            }
            None => {}
        }
    }
}

// ============================================================
// hub 错误 → JSON-RPC error
// ============================================================

/// hub 层失败 → ACP JSON-RPC error（未知会话 = invalid params；其余 = internal error）。
fn hub_error(error: HubError) -> Error {
    match error {
        HubError::UnknownSession(_) => Error::invalid_params().data(error.to_string()),
        HubError::Gateway(_) | HubError::Disconnected => {
            Error::internal_error().data(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_echoes_version_and_stays_conservative() {
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
        assert_eq!(
            response.protocol_version,
            agent_client_protocol::schema::ProtocolVersion::V1
        );
        assert!(!response.agent_capabilities.load_session);
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
        assert!(!info.version.is_empty());
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
    }
}
