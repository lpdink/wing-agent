//! Intent runner — executes side-effects produced by the App state machine.
//!
//! The App state machine declares *what* should happen by pushing `AppIntent`
//! variants. This module implements *how* each intent is executed, keeping
//! I/O concerns out of the core state machine.
//!
//! Fetch-type intents (read-only HTTP queries) are spawned as background
//! tasks to avoid blocking the main event loop. Results are sent back via
//! an mpsc channel as [`FetchResult`] variants.

use tokio::sync::mpsc;

use crate::app::intent::{AppIntent, FetchPayload, FetchResult};
use crate::app::settings::GatewaySaveReport;
use crate::app::settings::InterfaceSaveReport;
use crate::app::settings::PendingSettingsSave;
use crate::app::settings::changed_leaf_paths;
use crate::app::transport::GatewayEndpoint;
use crate::app::transport::Transport;
use crate::protocol::MediaRef;
use crate::shared::pinning::{is_pinned, pin_tag_ops};
use crate::tui::WingTerminal;
use crate::ui::toast::Toast;
use crate::util::image_compress::CompressOptions;
use crate::util::image_compress::CompressedImage;
use crate::util::image_compress::compress_image;
use crate::util::program_status;
use crate::util::title;

use super::App;

/// Upper bound for one clipboard copy as seen from the run loop.
///
/// The helper probe already runs on the blocking pool, but the intent itself
/// is awaited serially by the event loop — a wedged helper (a hung
/// `pbcopy`-alike) would otherwise freeze input and rendering indefinitely.
/// Timing out degrades to the OSC52-less failure path: a `Copy failed` toast
/// with the reason.
const CLIPBOARD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Execute a single intent, performing any necessary I/O.
///
/// `transport` is `None` when the gateway connection is lost; intents that
/// require the gateway are silently discarded in that case. `*endpoint` is the
/// gateway the retry ladder would reconnect to — an explicit restart
/// ([`AppIntent::RestartGateway`]) may rewrite it (the config file is the
/// source of truth for host:port, and `Ctrl+R` exists to apply that change).
///
/// `send_chain` is the tail of the message-send pipeline: [`AppIntent::SendMessage`]
/// runs in a background task, and the tail chains consecutive sends so their
/// frames reach the gateway in submission order (see `spawn_send_message`).
///
/// Fetch-type intents are spawned as background tasks; their results arrive
/// asynchronously via `fetch_tx`.
pub async fn execute_intent(
    app: &mut App,
    transport: &mut Option<Transport>,
    endpoint: &mut GatewayEndpoint,
    terminal: &mut WingTerminal,
    intent: AppIntent,
    fetch_tx: &mpsc::Sender<FetchResult>,
    send_chain: &mut Option<tokio::task::JoinHandle<()>>,
) {
    match intent {
        AppIntent::CopyToClipboard(text) => {
            // Platform helper first (real system clipboard), OSC52 as the
            // fallback — see `util::clipboard`. The helper probe runs on the
            // blocking pool, but this intent is still awaited serially by the
            // run loop, so bound it: the loop resumes after at most
            // `CLIPBOARD_TIMEOUT` even when a helper wedges (the copy then
            // reports a failure instead of freezing the UI).
            let copy = tokio::time::timeout(
                CLIPBOARD_TIMEOUT,
                crate::util::clipboard::copy_best_effort(terminal.backend_mut(), &text),
            )
            .await;
            let outcome = match copy {
                Ok(Ok(path)) => Ok(path),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(anyhow::anyhow!(
                    "clipboard helper timed out after {} ms",
                    CLIPBOARD_TIMEOUT.as_millis()
                )),
            };
            match outcome {
                Ok(path) => {
                    tracing::debug!(?path, "clipboard copy");
                    app.show_toast(Toast::info("Copied!", std::time::Duration::from_secs(2)));
                }
                Err(e) => {
                    tracing::warn!("clipboard copy failed: {e}");
                    app.show_toast(Toast::warning(
                        format!("Copy failed: {e}"),
                        std::time::Duration::from_secs(3),
                    ));
                }
            }
        }
        AppIntent::SetSessionPin { pinned } => {
            // pin = 当前会话上的一个普通标签（前端约定，后端零感知）。写走
            // 通用标签端点：加 `pin` / 删 `pin`，幂等——重复点击不会累积。
            // 目标状态 → 请求体的映射是纯函数（`pin_tag_ops`），有单测锚定：
            // 翻反了只会"点了没反应"（幂等 + 回读纠正），现场没有报错。
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let (add, remove) = pin_tag_ops(pinned);
                match http.tag_session(&session_id, add, remove).await {
                    Ok(resp) => {
                        // 以服务端回读为准（而不是点击时的乐观值）。
                        app.status.pinned = is_pinned(&resp.tags);
                        tracing::debug!(pinned = app.status.pinned, "session pin toggled");
                    }
                    Err(e) => {
                        tracing::warn!("session pin failed: {e}");
                        app.show_toast(Toast::warning(
                            format!("Pin failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::OpenLink(target) => {
            // Resolution + spawn happen on the blocking pool; the intent is
            // awaited with a bound so a wedged opener cannot freeze the UI.
            match crate::util::open::open_target(&target).await {
                Ok(plan) => {
                    tracing::debug!(program = ?plan.program, describe = %plan.describe, "opened link");
                }
                Err(e) => {
                    tracing::warn!("open link failed: {e:#}");
                    app.show_toast(Toast::warning(
                        format!("Open failed: {e:#}"),
                        std::time::Duration::from_secs(3),
                    ));
                }
            }
        }
        AppIntent::SendMessage {
            content,
            tool_call_id,
            request_id,
            images,
        } => {
            dispatch_send_message(
                app,
                transport.as_ref(),
                send_chain,
                OutgoingMessage {
                    session_id: app.session_id.clone(),
                    content,
                    tool_call_id,
                    request_id,
                    images,
                },
                fetch_tx.clone(),
            );
        }
        AppIntent::CompressClipboardImage { bytes, options } => {
            let tx = fetch_tx.clone();
            // 结果属于 composer 而不是会话（`handle_fetch_result` 不看会话守卫），
            // 这里的 session_id 只是 `FetchResult` 的形状要求。
            let session_id = app.session_id.clone();
            tokio::spawn(async move {
                let result = compress_clipboard_image(bytes, options).await;
                let _ = tx
                    .send(FetchResult {
                        session_id,
                        payload: FetchPayload::ClipboardImage(result),
                    })
                    .await;
            });
        }
        AppIntent::FetchInfo => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http.get_session_info(&session_id).await {
                        Ok(info) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::Info(Box::new(info)),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get session info failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::FetchCommands => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    match http.get_commands().await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Commands(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get commands failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::FetchModels => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    match http.get_models().await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Models(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get models failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::FetchBranches => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http.get_branches(&session_id).await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::Branches(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get branches failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::FetchAgents => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    match http.get_agents().await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Agents(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("get agents failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::UpdateSession {
            model_id,
            agent,
            title,
            thinking,
            reasoning_effort,
            yolo,
            workspace,
        } => {
            if let Some(t) = transport {
                let req = wing_api_client::models::UpdateSessionRequest {
                    session_id: app.session_id.clone(),
                    model_id: model_id.clone(),
                    agent: agent.clone(),
                    title: title.clone(),
                    thinking,
                    reasoning_effort: reasoning_effort.clone(),
                    yolo,
                    workspace: workspace.clone(),
                    // TUI 没有工具集编辑器：工具集切换只由 `wing update --tools`
                    // 从 CLI 驱动（同端点，键缺席 = 不动）。
                    tools: None,
                };
                match t.http.update_session(&req).await {
                    Ok(_) => apply_update_session(
                        app,
                        model_id,
                        agent,
                        title,
                        thinking,
                        reasoning_effort,
                        yolo,
                        workspace,
                    ),
                    Err(e) => {
                        app.chat
                            .push(crate::ui::chat_view::ChatCell::ErrorMessage(format!(
                                "Update failed: {e}"
                            )));
                    }
                }
            }
        }
        AppIntent::CreateSession { workspace } => {
            if let Some(t) = transport {
                let old_session_id = app.session_id.clone();
                let req = wing_api_client::models::CreateSessionRequest {
                    workspace: workspace.clone(),
                    ..Default::default()
                };
                match t.http.create_session(&req).await {
                    Ok(resp) => {
                        let new_sid = resp.session_id.clone();
                        t.switch_session(&old_session_id, &new_sid).await;
                        app.invalidate_session_cache();
                        tracing::info!(old = old_session_id, new = new_sid, "session created");
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Create failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ResumeSession {
            session_id: target_id,
        } => {
            if let Some(t) = transport {
                let old_session_id = app.session_id.clone();
                match t.http.resume_session(&target_id).await {
                    Ok(resp) => {
                        let new_sid = resp.session_id.clone();
                        t.switch_session(&old_session_id, &new_sid).await;
                        app.invalidate_session_cache();
                        tracing::info!(old = old_session_id, new = new_sid, "session resumed");
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Resume failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ForkSession { target_uuid } => {
            if let Some(t) = transport {
                let old_session_id = app.session_id.clone();
                match t.http.fork_session(&old_session_id, &target_uuid).await {
                    Ok(resp) => {
                        let new_sid = resp.session_id.clone();
                        t.switch_session(&old_session_id, &new_sid).await;
                        app.invalidate_session_cache();
                        tracing::info!(old = old_session_id, new = new_sid, "session forked");
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Fork failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::FetchSessionList => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    match http.list_sessions().await {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::SessionList(resp),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!("list sessions failed: {e}");
                        }
                    }
                });
            }
        }
        AppIntent::CompactSession { instruction } => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http
                        .compact_session(&session_id, instruction.as_deref())
                        .await
                    {
                        Ok(resp) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::CompactDone {
                                        original: resp.original_tokens,
                                        compressed: resp.compressed_tokens,
                                    },
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Toast {
                                        message: format!("Compact failed: {e}"),
                                        is_error: true,
                                    },
                                })
                                .await;
                        }
                    }
                });
            }
        }
        AppIntent::InterruptSession => {
            if let Some(t) = transport
                && let Err(e) = t.http.interrupt_session(&app.session_id).await
            {
                tracing::warn!("interrupt failed: {e}");
            }
        }
        AppIntent::RewindSession { target_uuid } => {
            if let Some(t) = transport {
                match t.http.rewind_session(&app.session_id, &target_uuid).await {
                    Ok(_) => {
                        app.show_toast(Toast::info(
                            "Session rewound",
                            std::time::Duration::from_secs(2),
                        ));
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Rewind failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ReloadSystem => {
            if let Some(t) = transport {
                match t.http.reload_system().await {
                    Ok(resp) => {
                        let status = if resp.ok { "✅" } else { "⚠️" };
                        let details: Vec<String> = resp
                            .results
                            .iter()
                            .map(|r| {
                                if r.ok {
                                    format!("✅ {}", r.name)
                                } else {
                                    format!(
                                        "❌ {}: {}",
                                        r.name,
                                        r.detail.as_deref().unwrap_or("unknown")
                                    )
                                }
                            })
                            .collect();
                        app.show_toast(Toast::info(
                            format!("{status} Reload: {}", details.join(", ")),
                            std::time::Duration::from_secs(4),
                        ));
                    }
                    Err(e) => {
                        app.show_toast(Toast::error(
                            format!("Reload failed: {e}"),
                            std::time::Duration::from_secs(3),
                        ));
                    }
                }
            }
        }
        AppIntent::ShowContextInfo => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http.get_session_info(&session_id).await {
                        Ok(info) => {
                            let mut text = format!(
                                "Messages: {}\nTokens: {} / {}\n",
                                info.context_stats.message_count,
                                info.context_stats.total_tokens,
                                info.context_window_tokens,
                            );
                            if !info.system_prompt.is_empty() {
                                text.push_str("\n--- System Prompt ---\n");
                                text.push_str(&info.system_prompt);
                            }
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::ContextInfo(text),
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Toast {
                                        message: format!("Context info failed: {e}"),
                                        is_error: true,
                                    },
                                })
                                .await;
                        }
                    }
                });
            }
        }
        AppIntent::ShowSkillsInfo => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                let sid = session_id.clone();
                tokio::spawn(async move {
                    match http.get_session_info(&session_id).await {
                        Ok(info) => {
                            let text = if info.skills_info.is_empty() {
                                "No skills loaded.".to_string()
                            } else {
                                info.skills_info
                            };
                            let _ = tx
                                .send(FetchResult {
                                    session_id: sid,
                                    payload: FetchPayload::SkillsInfo(text),
                                })
                                .await;
                        }
                        Err(e) => {
                            let _ = tx
                                .send(FetchResult {
                                    session_id,
                                    payload: FetchPayload::Toast {
                                        message: format!("Skills info failed: {e}"),
                                        is_error: true,
                                    },
                                })
                                .await;
                        }
                    }
                });
            }
        }
        AppIntent::SetTitle(t) => {
            let writer = terminal.backend_mut();
            if let Err(e) = title::set_title(writer, &t) {
                tracing::warn!("failed to set terminal title: {e}");
            }
        }
        AppIntent::Notify(message) => {
            let writer = terminal.backend_mut();
            if let Err(e) = crate::util::osc9::send_notification(writer, &message) {
                tracing::warn!("failed to send OSC 9 notification: {e}");
            }
        }
        AppIntent::SetProgramStatus(report) => {
            let writer = terminal.backend_mut();
            if let Err(e) = program_status::write(writer, &report) {
                tracing::warn!("failed to report program status: {e}");
            }
        }
        // ---- 设置面板 ----
        AppIntent::FetchSettings | AppIntent::ReloadSettings => {
            if let Some(t) = transport {
                let http = t.http.clone();
                let session_id = app.session_id.clone();
                let tx = fetch_tx.clone();
                tokio::spawn(async move {
                    // schema 是静态目录、get 是当前文档：并发两个请求一次回齐
                    // （面板打开 / 后台刷新 / R 重载三条路径共用这一份结果）。
                    let (schema, state) = tokio::join!(http.settings_schema(), http.settings_get());
                    let payload = match (schema, state) {
                        (Ok(schema), Ok(state)) => FetchPayload::Settings {
                            schema: Box::new(schema),
                            state: Box::new(state),
                        },
                        (Err(e), _) | (_, Err(e)) => FetchPayload::Toast {
                            message: format!("载入设置失败：{e}"),
                            is_error: true,
                        },
                    };
                    let _ = tx
                        .send(FetchResult {
                            session_id,
                            payload,
                        })
                        .await;
                });
            }
        }
        AppIntent::SaveSettings {
            gateway,
            base,
            interface,
            gateway_dirty,
            interface_dirty,
        } => {
            // Interface 半边：本地原子写（几 KB + `.bak`），同步做 —— 用户按下 `s`
            // 之后本地文件是不是写成了，不该等一个 HTTP RTT。
            let report = if interface_dirty {
                let previous = crate::config::store::read_interface_doc()
                    .ok()
                    .map(|read| read.doc);
                let changed = changed_leaf_paths(previous.as_ref(), &interface).len();
                match crate::config::store::write_interface_doc(&interface) {
                    Ok(outcome) => {
                        let report = InterfaceSaveReport::Ok {
                            path: outcome.path.display().to_string(),
                            changed,
                        };
                        app.settle_interface_save(&report, &interface);
                        report
                    }
                    Err(e) => {
                        let report = InterfaceSaveReport::Err {
                            message: e.to_string(),
                        };
                        app.settle_interface_save(&report, &interface);
                        report
                    }
                }
            } else {
                InterfaceSaveReport::Skipped
            };
            // 两半的状态先存住：合并回执（与缓存回写）都读这一份。
            app.settings_save = Some(PendingSettingsSave {
                interface: report,
                document: (*gateway).clone(),
            });
            // Gateway 半边：异步（回执两半合体，不阻塞主循环）。
            if gateway_dirty {
                if let Some(t) = transport.as_ref() {
                    let http = t.http.clone();
                    let session_id = app.session_id.clone();
                    let tx = fetch_tx.clone();
                    let request = wing_api_client::models::SettingsSetRequest {
                        base: Some(base),
                        document: (*gateway).clone(),
                    };
                    tokio::spawn(async move {
                        let payload = match http.settings_set(&request).await {
                            Ok(response) => FetchPayload::SettingsSaved(Box::new(response)),
                            Err(e) => FetchPayload::SettingsSaveError {
                                message: e.to_string(),
                                conflict: e.is_conflict(),
                            },
                        };
                        let _ = tx
                            .send(FetchResult {
                                session_id,
                                payload,
                            })
                            .await;
                    });
                } else {
                    app.settle_gateway_save(GatewaySaveReport::Failed {
                        message: "网关未连接".to_string(),
                        conflict: false,
                    });
                }
            } else {
                // 只有 Interface 半边：立刻收口（回执只说那一半）。
                app.settle_gateway_save(GatewaySaveReport::Skipped);
            }
        }
        AppIntent::RestartGateway => {
            super::settings::restart_gateway(app, transport, endpoint).await;
        }
    }
}

/// 一条用户消息的完整发送：带图先把每张上传成引用，再把消息（含引用）交给网关。
///
/// 从 `execute_intent` 抽出来是为了可测——`execute_intent` 需要 `WingTerminal`
/// （真实 stdout），单测造不出来（同 `apply_update_session` 的抽取理由）。
///
/// **失败契约**：上传或 WS 发送任一步失败 ⇒ 整条**不发**（不发送半截消息——
/// 少一张图的引用列表会被当成完整输入）。调用方拿 `Err(reason)` 去把提交产物**还给
/// 草稿**（后台任务把它包成回执 → `App::restore_failed_send` 从 pending 队列取回文本
/// 与图片 chip + warning toast）；已经上传成功的图留在存储里（政策：媒体永不删除）。
///
/// **返回**：`Ok(())` = 帧已投进 WS 写通道（成功路径不动 pending：条目等后端的
/// `user_message_accepted` 提升）；`Err(reason)` = 整条没发出去（无连接 / 上传失败 /
/// WS 写不出去），`reason` 是给 toast 的文案。
pub(super) async fn deliver_user_message(
    http: &wing_api_client::GatewayClient,
    ws: &crate::gateway::RequestSender,
    message: &mut OutgoingMessage,
) -> Result<(), String> {
    // 图片 move 出去（不克隆压缩产物）：回执只需要剩下的两个 id。
    let media = if message.images.is_empty() {
        // 纯文本：帧里没有 `media` 键，行为与引入图片之前逐字节相同。
        None
    } else {
        let images = std::mem::take(&mut message.images);
        match upload_images(http, &message.session_id, images).await {
            Ok(refs) => Some(refs),
            Err(e) => {
                tracing::warn!("image upload failed: {e}");
                return Err(format!("Image upload failed: {e}"));
            }
        }
    };

    if let Err(e) = ws
        .send_message(
            &message.session_id,
            &message.content,
            message.tool_call_id.take(),
            message.request_id.clone(),
            media,
        )
        .await
    {
        tracing::error!("failed to send message: {e}");
        return Err(format!("Send failed: {e}"));
    }
    Ok(())
}

/// 一条待发消息的载荷：`SendMessage` 的字段 + 目标会话，总是整体传递（也把下面几个
/// 函数的参数表压回可读范围）。
pub(super) struct OutgoingMessage {
    pub(super) session_id: String,
    pub(super) content: String,
    pub(super) tool_call_id: Option<String>,
    pub(super) request_id: String,
    pub(super) images: Vec<CompressedImage>,
}

/// `AppIntent::SendMessage` 的落地：无连接 → 本地回填；有连接 → 后台任务（[`spawn_send_message`]）。
///
/// 事件循环**一步都不等网络**：逐张上传 + WS 发送在慢链路（远程网关 / SSH 隧道）上是
/// N 个完整往返，内联 await 会把 UI（重绘 / 按键）冻住秒级；冻结足够久还会把
/// `TermEvent` 通道填满，联动丢粘贴流量（见 `docs/dev/tui-input.md` 的「后台发送」段）。
/// 与 `CompressClipboardImage` 同一个范式：副作用在后台跑，结果经 `FetchResult` 回执回 App。
pub(super) fn dispatch_send_message(
    app: &mut App,
    transport: Option<&Transport>,
    send_chain: &mut Option<tokio::task::JoinHandle<()>>,
    message: OutgoingMessage,
    fetch_tx: mpsc::Sender<FetchResult>,
) {
    let Some(transport) = transport else {
        // Gateway connection lost — nothing left the client, so the content
        // goes back to the composer. 纯本地动作（无 I/O），不必绕后台任务。
        app.restore_failed_send(&message.request_id, "Not sent — gateway disconnected");
        return;
    };
    spawn_send_message(transport, send_chain, message, fetch_tx);
}

/// 把一条消息的发送挪进后台任务（[`dispatch_send_message`] 的「有连接」分支）。
///
/// **顺序是契约**：`send_chain` 把相邻两条消息的任务串起来——第二条要等第一条整条
/// （上传 + 发送）结束才开始。后端按帧到达顺序入队（消息顺序是合并语义的一部分），
/// 而两条快速提交（第一条的图还在上传、用户又发了一条）本来由「事件循环内联 await」
/// 天然串行；挪进后台后若不串，第二条会超车。串行只发生在**后台任务之间**，事件循环
/// 一步不等；每条消息内部仍是「先逐张上传（失败即停）→ 再发帧」。
fn spawn_send_message(
    transport: &Transport,
    send_chain: &mut Option<tokio::task::JoinHandle<()>>,
    mut message: OutgoingMessage,
    fetch_tx: mpsc::Sender<FetchResult>,
) {
    let http = transport.http.clone();
    let ws = transport.ws.request_sender();
    let previous = send_chain.take();
    let handle = tokio::spawn(async move {
        if let Some(previous) = previous {
            // 上一条的成败由它自己的回执处理，这里只接顺序。
            let _ = previous.await;
        }
        if let Err(reason) = deliver_user_message(&http, &ws, &mut message).await {
            // 内容还给草稿：回执走和压缩同一条 fetch 通道（App 侧不看会话守卫——
            // 条目按 request_id 认领，找不到就只 toast，不碰草稿）。
            let _ = fetch_tx
                .send(FetchResult {
                    session_id: message.session_id,
                    payload: FetchPayload::SendFailed {
                        request_id: message.request_id,
                        reason,
                    },
                })
                .await;
        }
    });
    *send_chain = Some(handle);
}

/// 压缩一张粘贴进来的图片，在阻塞池上（解码 / 缩放 / JPEG 编码都是纯 CPU）。
///
/// 从 `execute_intent` 抽出来是为了可测——`execute_intent` 需要 `WingTerminal`
/// （真实 stdout），单测造不出来（同 `deliver_user_message` 的抽取理由）。错误在这里
/// 格式化成给 toast 的文案；`Err(String)` 而不是 `CompressError`，是因为调用点只需要
/// 一句话（阻塞池的 join 失败也只有这句话能说）。
pub(super) async fn compress_clipboard_image(
    bytes: Vec<u8>,
    options: CompressOptions,
) -> Result<CompressedImage, String> {
    match tokio::task::spawn_blocking(move || compress_image(&bytes, &options)).await {
        Ok(Ok(image)) => Ok(image),
        Ok(Err(e)) => Err(e.to_string()),
        Err(join) => Err(format!("compression task failed: {join}")),
    }
}

/// 逐张上传（顺序 = 提交顺序 = 消息内 media 列表顺序）。
///
/// 错误在这里格式化成给 toast 的文案（调用点只 toast）；展示名（`name`）不走上传：
/// chip 的 `label` 是纯前端展示（05），后端只存字节与元数据。
async fn upload_images(
    http: &wing_api_client::GatewayClient,
    session_id: &str,
    images: Vec<CompressedImage>,
) -> Result<Vec<MediaRef>, String> {
    let mut refs = Vec::with_capacity(images.len());
    for image in images {
        let uploaded = http
            .upload_media(session_id, image.bytes, None)
            .await
            .map_err(|e| e.to_string())?;
        refs.push(uploaded);
    }
    Ok(refs)
}

/// Apply optimistic local status update after a successful UpdateSession HTTP call.
///
/// Updates `app.status` fields and shows a summary toast.
#[allow(clippy::too_many_arguments)]
fn apply_update_session(
    app: &mut App,
    model_id: Option<String>,
    agent: Option<String>,
    title: Option<String>,
    thinking: Option<bool>,
    reasoning_effort: Option<String>,
    yolo: Option<bool>,
    workspace: Option<String>,
) {
    let on_off = |b: bool| if b { "on" } else { "off" };

    // Optimistic model identity, resolved from the last `/api/models` snapshot
    // (the panel that produced this switch rendered from it): call name,
    // provider and display label. When the snapshot does not cover the id
    // (stale cache), the id itself stands in for the call name — the gateway's
    // `session_state_changed` right after carries the authoritative values.
    let model = model_id
        .as_deref()
        .map(|id| match app.model_declaration(id) {
            Some((name, provider, label)) => crate::ui::status_bar::ModelUpdate {
                id: Some(id.to_string()),
                name: name.to_string(),
                provider: Some(provider.to_string()),
                display_name: (label != id).then(|| label.to_string()),
            },
            None => crate::ui::status_bar::ModelUpdate {
                id: Some(id.to_string()),
                name: id.to_string(),
                provider: None,
                display_name: None,
            },
        });

    // Build toast parts from non-None fields. The model part goes through the
    // shared formatter so the display label (and the raw id on its own line)
    // match the picker's immediate toast exactly.
    let parts: Vec<String> = [
        model_id.as_deref().map(|id| app.model_switch_toast(id)),
        agent.as_ref().map(|a| format!("Agent: {a}")),
        title.as_ref().map(|t| format!("Title: {t}")),
        thinking.map(|t| format!("Think: {}", on_off(t))),
        reasoning_effort.as_ref().map(|e| format!("Effort: {e}")),
        yolo.map(|y| format!("YOLO: {}", on_off(y))),
        workspace.as_ref().map(|w| format!("Workdir: {w}")),
    ]
    .into_iter()
    .flatten()
    .collect();

    // Apply to local status.
    let affects_list = title.is_some() || workspace.is_some();
    app.status
        .apply_session_update(model, agent, title, thinking, reasoning_effort, yolo);

    if let Some(w) = workspace {
        app.status.workdir = Some(w);
    }

    // Title/workspace changes affect the session list display — invalidate cache.
    if affects_list {
        app.invalidate_session_cache();
    }

    if !parts.is_empty() {
        // The picker showed an immediate toast on Enter; this one is the
        // server-confirmed update (and carries the other fields of a
        // multi-field intent) — same text, refreshed timer.
        app.show_toast(Toast::info(
            parts.join(" · "),
            std::time::Duration::from_secs(3),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::ui::popup::command::SessionCandidate;

    fn app_with_cached_sessions() -> App {
        let mut app = App::new("test-session".into(), AppConfig::default(), None);
        app.popup.cache.sessions = vec![SessionCandidate {
            id: "s1".into(),
            title: "Old Title".into(),
            workspace: "/old".into(),
            status: "idle".into(),
            last_interaction: "2025-01-01T00:00:00Z".into(),
            pinned: false,
            pin_added_at: None,
        }];
        app
    }

    #[test]
    fn test_update_session_invalidates_cache_on_title() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            None,
            None,
            Some("New Title".into()),
            None,
            None,
            None,
            None,
        );
        assert!(app.popup.cache.sessions.is_empty());
    }

    #[test]
    fn test_update_session_invalidates_cache_on_workspace() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("/new/dir".into()),
        );
        assert!(app.popup.cache.sessions.is_empty());
    }

    #[test]
    fn test_update_session_preserves_cache_on_model_only() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            Some("gpt-4o".into()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!app.popup.cache.sessions.is_empty());
    }

    /// The optimistic update resolves the whole identity group (call name /
    /// provider / label) from the `/api/models` snapshot by **id**.
    #[test]
    fn test_update_session_resolves_the_identity_from_the_snapshot() {
        let mut app = app_with_cached_sessions();
        app.model_sources = vec![labeled_sources()];
        apply_update_session(
            &mut app,
            Some("dfmodel".into()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(app.status.model, "dfmodel");
        assert_eq!(app.status.model_id.as_deref(), Some("dfmodel"));
        assert_eq!(app.status.provider.as_deref(), Some("qoder"));
        assert_eq!(
            app.status.model_display_name.as_deref(),
            Some("DeepSeek-Flash")
        );
    }

    /// id 不在快照里（陈旧缓存）：id 顶上调用名，provider 缺席——权威值随
    /// `session_state_changed` 到达。
    #[test]
    fn test_update_session_with_an_unknown_id_keeps_the_id_as_the_fallback() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            Some("mystery-id".into()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(app.status.model, "mystery-id");
        assert_eq!(app.status.model_id.as_deref(), Some("mystery-id"));
        assert_eq!(app.status.provider, None);
        assert_eq!(app.status.model_display_name, None);
    }

    #[test]
    fn test_update_session_preserves_cache_on_thinking_only() {
        let mut app = app_with_cached_sessions();
        apply_update_session(
            &mut app,
            None,
            None,
            None,
            Some(true),
            Some("high".into()),
            None,
            None,
        );
        assert!(!app.popup.cache.sessions.is_empty());
    }

    /// Model group declaring a display name for `dfmodel` — the source the
    /// close-to-the-panel optimistic label is resolved from.
    fn labeled_sources() -> wing_api_client::models::ProviderModels {
        wing_api_client::models::ProviderModels {
            provider: "qoder".into(),
            models: vec![wing_api_client::models::ModelDetail {
                id: "dfmodel".into(),
                name: "dfmodel".into(),
                display_name: Some("DeepSeek-Flash".into()),
                description: None,
                context_window_tokens: None,
                keep_recent_tokens: None,
                capabilities: Default::default(),
            }],
        }
    }

    #[test]
    fn test_update_session_toast_and_status_use_the_display_label() {
        let mut app = app_with_cached_sessions();
        app.model_sources = vec![labeled_sources()];

        apply_update_session(
            &mut app,
            Some("dfmodel".into()),
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let toast = app.toast.as_ref().expect("model switch toast");
        assert_eq!(toast.message, "Model: DeepSeek-Flash (qoder)\n↳ dfmodel");
        // The optimistic status carries the label too, so the status bar does
        // not flash the raw name before the gateway event lands.
        assert_eq!(app.status.model, "dfmodel");
        assert_eq!(
            app.status.model_display_name.as_deref(),
            Some("DeepSeek-Flash")
        );
        assert_eq!(app.status.provider.as_deref(), Some("qoder"));
    }

    #[test]
    fn test_update_session_model_without_declaration_keeps_the_toast_single_line() {
        let mut app = app_with_cached_sessions();
        app.model_sources = vec![wing_api_client::models::ProviderModels {
            provider: "p".into(),
            models: vec![wing_api_client::models::ModelDetail {
                id: "plain".into(),
                name: "plain".into(),
                display_name: None,
                description: None,
                context_window_tokens: None,
                keep_recent_tokens: None,
                capabilities: Default::default(),
            }],
        }];

        apply_update_session(
            &mut app,
            Some("plain".into()),
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let toast = app.toast.as_ref().expect("model switch toast");
        assert_eq!(toast.message, "Model: plain (p)");
        assert_eq!(app.status.model_display_name, None);
    }
}
