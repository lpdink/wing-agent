//! `wing acp` 的整机 hermetic e2e：真二进制 + 假网关 + 官方 ACP 客户端。
//!
//! 场景清单与断言口径见 `06_e2e/proposal.md` 与 `06_e2e/design.md`；装置（假网关 /
//! 客户端 / 临时 WING_HOME）见 [`acp_harness`]。纪律：不联网、不碰用户网关与 `~/.wing`，
//! 端口临时分配，每测试独立进程。
//!
//! 十个测试 = 清单七组（list / load+resume / close 各自成函数，便于失败定位）。

#![allow(clippy::print_stderr)] // harness 的调试回调（WING_ACP_E2E_TRACE=1）写 stderr

mod acp_harness;

use std::collections::BTreeMap;
use std::sync::Arc;

use acp_harness::ElicitationReply;
use acp_harness::ElicitationScript;
use acp_harness::Harness;
use acp_harness::PermissionReply;
use acp_harness::PermissionScript;
use acp_harness::Recorder;
use acp_harness::bash_ask;
use acp_harness::commands_catalog;
use acp_harness::context_stats;
use acp_harness::describe_kinds;
use acp_harness::diff_content;
use acp_harness::done;
use acp_harness::interrupted;
use acp_harness::models_catalog;
use acp_harness::questions_ask;
use acp_harness::reasoning;
use acp_harness::run_client;
use acp_harness::session_row;
use acp_harness::session_title;
use acp_harness::settle;
use acp_harness::spawn_prompt;
use acp_harness::sync_session;
use acp_harness::text;
use acp_harness::tool_call;
use acp_harness::tool_call_result;
use acp_harness::tool_call_stream;
use acp_harness::turn_result;
use acp_harness::update_kind;
use acp_harness::wing_event;
use agent_client_protocol::Agent;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::CancelNotification;
use agent_client_protocol::schema::v1::ClientCapabilities;
use agent_client_protocol::schema::v1::CloseSessionRequest;
use agent_client_protocol::schema::v1::ContentBlock;
use agent_client_protocol::schema::v1::ElicitationCapabilities;
use agent_client_protocol::schema::v1::ElicitationContentValue;
use agent_client_protocol::schema::v1::ElicitationFormCapabilities;
use agent_client_protocol::schema::v1::ErrorCode;
use agent_client_protocol::schema::v1::Implementation;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::schema::v1::ListSessionsRequest;
use agent_client_protocol::schema::v1::LoadSessionRequest;
use agent_client_protocol::schema::v1::NewSessionRequest;
use agent_client_protocol::schema::v1::PromptRequest;
use agent_client_protocol::schema::v1::ResumeSessionRequest;
use agent_client_protocol::schema::v1::SetSessionConfigOptionRequest;
use agent_client_protocol::schema::v1::StopReason;
use agent_client_protocol::schema::v1::TextContent;
use agent_client_protocol::schema::v1::ToolCall;
use agent_client_protocol::schema::v1::ToolCallContent;
use agent_client_protocol::schema::v1::ToolCallStatus;
use agent_client_protocol::schema::v1::ToolCallUpdate;
use agent_client_protocol::schema::v1::ToolKind;
use serde_json::json;

/// 场景开始：initialize（默认能力 = terminal；`elicitation` 决定是否广告表单能力）。
async fn initialize(
    connection: &ConnectionTo<Agent>,
    elicitation: bool,
) -> Result<(), agent_client_protocol::Error> {
    let mut capabilities = ClientCapabilities::new().terminal(true);
    if elicitation {
        capabilities = capabilities
            .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
    }
    let response = connection
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(capabilities)
                .client_info(Implementation::new("wing-e2e", "0.1")),
        )
        .block_task()
        .await?;
    assert_eq!(response.protocol_version, ProtocolVersion::V1);
    Ok(())
}

// ============================================================
// B1 initialize
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn initialize_advertises_v1_and_session_lifecycle() {
    let harness = Harness::start().await;
    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            let response = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(ClientCapabilities::new().terminal(true))
                        .client_info(Implementation::new("wing-e2e", "0.1")),
                )
                .block_task()
                .await?;

            assert_eq!(response.protocol_version, ProtocolVersion::V1);
            let caps = response.agent_capabilities;
            assert!(
                caps.load_session,
                "session/load 已实现，必须广告 loadSession"
            );
            let session = caps.session_capabilities;
            assert!(session.list.is_some(), "session/list 需要广告");
            assert!(session.resume.is_some(), "session/resume 需要广告");
            assert!(session.close.is_some(), "session/close 需要广告");
            assert!(session.delete.is_none(), "wing 没有删除 API，不广告 delete");
            assert!(
                session.additional_directories.is_none(),
                "单 workspace 会话，不广告 additionalDirectories"
            );
            assert!(
                !caps.prompt_capabilities.image
                    && !caps.prompt_capabilities.audio
                    && !caps.prompt_capabilities.embedded_context,
                "promptCapabilities 必须全 false（不广告 image/audio/embeddedContext）"
            );
            let info = response.agent_info.expect("agentInfo 是必广告项");
            assert_eq!(info.name, "wing");
            assert!(
                info.version.contains('('),
                "agentInfo.version 与 `wing --version` 同口径（版本 + 短 commit）：{}",
                info.version
            );

            // 更高版本：固定回 v1，不回显请求版本（01 r2 S2 的回归点——SDK 的版本守卫
            // 是空实现，端口必须自己把关）。
            let newer = connection
                .send_request(InitializeRequest::new(ProtocolVersion::from(2)))
                .block_task()
                .await?;
            assert_eq!(
                newer.protocol_version,
                ProtocolVersion::V1,
                "本构建不做版本守卫：固定回 v1，不回显客户端请求的版本"
            );
            Ok(())
        },
    )
    .await
    .expect("initialize 场景");
}

// ============================================================
// B2 session/new + prompt（流式帧序）
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn prompt_streams_in_order_and_reports_end_turn() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[(
        "fake",
        &[("echo-1", "Echo One"), ("echo-2", "Echo Two")],
    )]));
    gateway.set_commands(commands_catalog(&[
        ("model", "Switch the model", "<provider:model>"),
        ("tips", "Show the tips panel", ""),
    ]));

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let rec = Arc::clone(&recorder);
    let gw = Arc::clone(&gateway);
    let ws = workspace.clone();

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            initialize(&connection, false).await?;

            let new = connection
                .send_request(NewSessionRequest::new(ws.clone()))
                .block_task()
                .await?;
            let sid = new.session_id.to_string();
            rec.marker("new-response");

            // 建会话请求体：workspace = 客户端 cwd；**不设 yolo**（危险操作要在 ACP 可见）。
            let create = gw
                .wait_request("session/create", |r| r.path == "/api/session/create")
                .await;
            assert_eq!(create.method, "POST");
            let create_body = create.body.expect("create 请求体");
            assert_eq!(create_body["workspace"], ws.as_str());
            assert!(
                create_body["agent"].get("yolo").is_none(),
                "acp 前端不得设置 yolo：{create_body}"
            );

            // 订阅带 X-Client-Id（与 WS 握手一致）。
            let subscribe = gw
                .wait_request("session/subscribe", |r| r.path == "/api/session/subscribe")
                .await;
            assert_eq!(subscribe.client_id.as_deref(), Some(gw.client_id()));
            assert_eq!(
                subscribe.body.expect("subscribe 请求体")["session_id"],
                sid.as_str()
            );

            // 应答里的模型 configOptions（id=model；值域 `provider:model`）。
            let options = new.config_options.expect("configOptions 已广告");
            let option = serde_json::to_value(&options[0]).expect("option 序列化");
            assert_eq!(option["id"], "model");
            assert_eq!(option["category"], "model");
            assert_eq!(option["type"], "select");
            assert_eq!(option["currentValue"], "fake:echo-1");
            assert_eq!(option["options"][0]["group"], "fake");
            assert_eq!(option["options"][0]["options"][0]["value"], "fake:echo-1");
            assert_eq!(option["options"][0]["options"][0]["name"], "Echo One");

            // 命令列表在应答**之后**补发（marker 之前没有任何 update）。
            rec.wait_update("命令列表", &sid, |update| {
                update_kind(update) == "available_commands_update"
            })
            .await;
            assert!(
                rec.updates_before_marker(&sid, "new-response").is_empty(),
                "命令列表必须在 session/new 应答之后"
            );
            let commands = rec.updates_for(&sid)[0].clone();
            assert_eq!(commands["sessionUpdate"], "available_commands_update");
            assert_eq!(commands["availableCommands"][0]["name"], "model");
            assert_eq!(
                commands["availableCommands"][0]["description"],
                "Switch the model <provider:model>"
            );

            // 一轮 prompt：推脚本帧（真实后端的形状），等终态。
            let prompt = spawn_prompt(&connection, new.session_id.clone(), "改一下这个文件")?;
            let inbound = gw
                .wait_ws_inbound("prompt 上行帧", |frame| {
                    frame["session_id"] == sid.as_str() && frame["content"] == "改一下这个文件"
                })
                .await;
            assert!(
                inbound.get("tool_call_id").is_none(),
                "普通 prompt 不带 tool_call_id：{inbound}"
            );

            let path = format!("{ws}/src/lib.rs");
            let frames = vec![
                reasoning("先看一眼这个文件"),
                text("我来改这个文件"),
                tool_call_stream("tc-1", "Edit"),
                tool_call("tc-1", "Edit", json!({"path": path})),
                diff_content("tc-1", &path, Some("old\n"), "new\n"),
                tool_call_result("tc-1", "Edit", "applied 1 hunk", true),
                session_title("修 ACP e2e"),
                context_stats(123, 262_144),
                turn_result(),
                done(),
            ];
            for frame in frames {
                gw.push(wing_event(&sid, frame));
            }
            let response = prompt.await.expect("prompt 任务被取消")?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            // 帧序（形状 + 顺序）。
            assert_eq!(
                describe_kinds(&rec.update_kinds(&sid)),
                concat!(
                    "available_commands_update → agent_thought_chunk → agent_message_chunk → ",
                    "tool_call → tool_call_update → tool_call_update → tool_call_update → ",
                    "session_info_update → usage_update",
                ),
                "一轮 prompt 的完整帧序"
            );

            let updates = rec.updates_for(&sid);
            assert_eq!(updates[1]["content"]["text"], "先看一眼这个文件");
            assert_eq!(updates[2]["content"]["text"], "我来改这个文件");

            // 工具卡片：stream 建卡（pending）→ 权威参数补齐（in_progress + 标题/定位/rawInput）。
            let created: ToolCall =
                serde_json::from_value(updates[3].clone()).expect("tool_call 帧解码");
            assert_eq!(created.tool_call_id.to_string(), "tc-1");
            assert_eq!(created.title, "Edit");
            assert_eq!(created.name.as_deref(), Some("Edit"));
            assert_eq!(created.kind, ToolKind::Edit);
            assert_eq!(
                created.status,
                ToolCallStatus::Pending,
                "早期信号 = pending"
            );

            let filled: ToolCallUpdate =
                serde_json::from_value(updates[4].clone()).expect("tool_call_update 帧解码");
            assert_eq!(filled.tool_call_id.to_string(), "tc-1");
            assert_eq!(filled.fields.title.as_deref(), Some(path.as_str()));
            assert_eq!(filled.fields.kind, Some(ToolKind::Edit));
            assert_eq!(filled.fields.status, Some(ToolCallStatus::InProgress));
            assert_eq!(filled.fields.raw_input, Some(json!({"path": path})));
            let locations = filled.fields.locations.expect("Edit 卡片带 locations");
            assert_eq!(locations.len(), 1);
            assert_eq!(locations[0].path.to_string_lossy(), path);

            // diff：锚定到卡片，整表重发（只有一条 diff 时 content = [diff]）。
            let diff: ToolCallUpdate =
                serde_json::from_value(updates[5].clone()).expect("diff 帧解码");
            let content = diff.fields.content.expect("diff content");
            match &content[0] {
                ToolCallContent::Diff(diff) => {
                    assert_eq!(diff.path.to_string_lossy(), path);
                    assert_eq!(diff.old_text.as_deref(), Some("old\n"));
                    assert_eq!(diff.new_text, "new\n");
                }
                other => panic!("期望 Diff 内容，得到 {other:?}"),
            }

            // 结果收口：completed + 整表 content（diff + 结果行）+ rawOutput。
            let finished: ToolCallUpdate =
                serde_json::from_value(updates[6].clone()).expect("结果帧解码");
            assert_eq!(finished.fields.status, Some(ToolCallStatus::Completed));
            assert_eq!(finished.fields.raw_output, Some(json!("applied 1 hunk")));
            let content = finished.fields.content.expect("结果帧带 content 整表");
            assert_eq!(content.len(), 2, "diff + 结果行");
            match &content[1] {
                ToolCallContent::Content(entry) => match &entry.content {
                    ContentBlock::Text(text) => assert_eq!(text.text, "applied 1 hunk"),
                    other => panic!("期望文本内容，得到 {other:?}"),
                },
                other => panic!("期望内容行，得到 {other:?}"),
            }

            // 标题 / 用量。
            assert_eq!(updates[7]["title"], "修 ACP e2e");
            assert_eq!(updates[8]["used"], 123);
            assert_eq!(updates[8]["size"], 262_144);
            Ok(())
        },
    )
    .await
    .expect("prompt 流式场景");
}

// ============================================================
// B3 session/cancel
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn cancel_interrupts_the_gateway_and_reports_cancelled() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[("fake", &[("echo-1", "Echo One")])]));

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let gw = Arc::clone(&gateway);
    let ws = workspace.clone();

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            initialize(&connection, false).await?;
            let new = connection
                .send_request(NewSessionRequest::new(ws))
                .block_task()
                .await?;
            let sid = new.session_id.to_string();

            let prompt = spawn_prompt(&connection, new.session_id.clone(), "跑一个长任务")?;
            gw.wait_ws_inbound("prompt 上行帧", |frame| {
                frame["session_id"] == sid.as_str() && frame["content"] == "跑一个长任务"
            })
            .await;

            // 轮次在途：cancel 必须变成网关侧的 interrupt。
            connection.send_notification(CancelNotification::new(new.session_id.clone()))?;
            let interrupt = gw
                .wait_request("interrupt", |r| r.path == "/api/session/interrupt")
                .await;
            assert_eq!(
                interrupt.body.expect("interrupt 请求体")["session_id"],
                sid.as_str()
            );

            // 网关广播 interrupted → 轮次以 stopReason: cancelled 收口。
            gw.push(wing_event(&sid, interrupted()));
            let response = prompt.await.expect("prompt 任务被取消")?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);
            Ok(())
        },
    )
    .await
    .expect("cancel 场景");
}

// ============================================================
// B4 Bash 确认 → permission
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn bash_confirmation_maps_to_permission_and_writes_the_token_back() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[("fake", &[("echo-1", "Echo One")])]));

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let rec = Arc::clone(&recorder);
    let gw = Arc::clone(&gateway);
    let ws = workspace.clone();

    // 三轮：y（allow_once）/ yolo（allow_always）/ cancelled outcome（→ 拒绝 token "n"）。
    permissions.push(PermissionReply::Select("y".to_string()));
    permissions.push(PermissionReply::Select("yolo".to_string()));
    permissions.push(PermissionReply::Cancel);

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            initialize(&connection, false).await?;
            let new = connection
                .send_request(NewSessionRequest::new(ws))
                .block_task()
                .await?;
            let sid = new.session_id.to_string();

            for (turn, command, tool_call_id, expected) in [
                (1, "rm -rf /tmp/wing-e2e-scratch", "tc-bash-1", "y"),
                (2, "git push --force", "tc-bash-2", "yolo"),
                (3, "mkfs.ext4 /dev/disk9", "tc-bash-3", "n"),
            ] {
                let prompt = spawn_prompt(&connection, new.session_id.clone(), command)?;
                gw.wait_ws_inbound("prompt 上行帧", |frame| {
                    frame["session_id"] == sid.as_str() && frame["content"] == command
                })
                .await;

                // 真实帧序：工具卡片先建（title = 命令首行），再发 ask。
                gw.push(wing_event(
                    &sid,
                    tool_call(tool_call_id, "Bash", json!({"command": command})),
                ));
                gw.push(wing_event(
                    &sid,
                    bash_ask(tool_call_id, &format!("Allow running: {command}?")),
                ));

                // 权限卡片：optionId 即回写 token，kind 与语义对齐。
                let request = rec.wait_permission("permission 请求", turn).await;
                assert_eq!(request["sessionId"], sid.as_str());
                assert_eq!(request["toolCall"]["toolCallId"], tool_call_id);
                assert_eq!(
                    request["options"],
                    json!([
                        {"optionId": "y", "name": "Yes, run it", "kind": "allow_once"},
                        {"optionId": "yolo", "name": "Yes, and don't ask again", "kind": "allow_always"},
                        {"optionId": "n", "name": "No", "kind": "reject_once"},
                    ]),
                    "Bash 三 token 的 optionId / kind 映射"
                );

                // 回写：WS ClientRequest{content, tool_call_id}（定向 resolve feedback waiter）。
                let answer = gw
                    .wait_ws_inbound("ask 回写", |frame| {
                        frame["tool_call_id"] == tool_call_id
                    })
                    .await;
                assert_eq!(answer["content"], expected, "第 {turn} 轮的回写 token");
                assert_eq!(answer["session_id"], sid.as_str());

                gw.push(wing_event(&sid, tool_call_result(tool_call_id, "Bash", "ok", true)));
                gw.push(wing_event(&sid, turn_result()));
                gw.push(wing_event(&sid, done()));
                let response = prompt.await.expect("prompt 任务被取消")?;
                assert_eq!(response.stop_reason, StopReason::EndTurn, "第 {turn} 轮");

                let card: ToolCall = serde_json::from_value(
                    rec.updates_for(&sid)
                        .into_iter()
                        .find(|update| {
                            update_kind(update) == "tool_call"
                                && update["toolCallId"] == tool_call_id
                        })
                        .expect("工具卡片已创建"),
                )
                .expect("tool_call 帧解码");
                assert_eq!(card.kind, ToolKind::Execute);
                assert_eq!(card.title, command, "Bash 标题 = 命令首行");
                assert_eq!(card.raw_input, Some(json!({"command": command})));
            }
            Ok(())
        },
    )
    .await
    .expect("Bash 确认场景");
}

// ============================================================
// B5 问答形态 → elicitation 表单
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn questions_ask_maps_to_elicitation_form() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[("fake", &[("echo-1", "Echo One")])]));

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let rec = Arc::clone(&recorder);
    let gw = Arc::clone(&gateway);
    let ws = workspace.clone();

    // 三题：单选 / 多选 / 未答（自由文本）。
    let questions = json!([
        {
            "id": "q1", "question": "选哪个水果？", "header": "Fruit", "multiSelect": false,
            "options": [{"label": "apple", "description": ""}, {"label": "banana", "description": ""}],
        },
        {
            "id": "q2", "question": "打哪些标签？", "header": "Tags", "multiSelect": true,
            "options": [{"label": "urgent", "description": ""}, {"label": "later", "description": ""}],
        },
        {"id": "q3", "question": "补充说明？", "header": "", "multiSelect": false, "options": []},
    ]);
    let mut content = BTreeMap::new();
    content.insert(
        "q1".to_string(),
        ElicitationContentValue::String("banana".to_string()),
    );
    content.insert(
        "q2".to_string(),
        ElicitationContentValue::StringArray(vec!["urgent".to_string(), "later".to_string()]),
    );
    // q3 故意不答 → 占位符。
    elicitations.push(ElicitationReply::Accept(content));
    // 第二轮：客户端回 `-32601` → 进程级降级（粘性），此后 ask 一律走回退。
    elicitations.push(ElicitationReply::MethodNotFound);
    // 降级后的两轮回退：第 1 题选第一个选项 / 第 2 轮选第二个选项。
    permissions.push(PermissionReply::Select("__wing_opt_0_0".to_string()));
    permissions.push(PermissionReply::Select("__wing_opt_0_1".to_string()));

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            initialize(&connection, true).await?;
            let new = connection
                .send_request(NewSessionRequest::new(ws))
                .block_task()
                .await?;
            let sid = new.session_id.to_string();

            let prompt = spawn_prompt(&connection, new.session_id.clone(), "帮我确认几个问题")?;
            gw.wait_ws_inbound("prompt 上行帧", |frame| {
                frame["session_id"] == sid.as_str()
            })
            .await;
            gw.push(wing_event(&sid, tool_call("tc-ask", "AskUserQuestion", json!({}))));
            gw.push(wing_event(&sid, questions_ask("tc-ask", questions)));

            // 表单请求：scope 带 toolCallId，schema 每题一个 property。
            let request = rec.wait_elicitation("elicitation 请求", 1).await;
            assert_eq!(request["mode"], "form");
            assert_eq!(request["sessionId"], sid.as_str());
            assert_eq!(request["toolCallId"], "tc-ask");
            assert_eq!(request["message"], "3 questions");
            let schema = &request["requestedSchema"];
            assert_eq!(schema["type"], "object");
            let properties = schema["properties"].as_object().expect("properties 是对象");
            assert_eq!(
                properties.keys().cloned().collect::<Vec<_>>(),
                ["q1", "q2", "q3"],
                "property key = question.id"
            );
            assert_eq!(properties["q1"]["type"], "string", "单选 = string + oneOf");
            assert_eq!(properties["q1"]["oneOf"][0]["const"], "apple");
            assert_eq!(properties["q2"]["type"], "array", "多选 = array");
            assert_eq!(properties["q2"]["items"]["anyOf"][0]["const"], "urgent");
            assert_eq!(properties["q3"]["type"], "string", "自由文本 = string");
            assert!(
                properties["q3"].get("oneOf").is_none(),
                "自由文本没有 options"
            );

            // 表单提交流回写：逐题 `header: answer` 行（未答出占位）。
            let answer = gw
                .wait_ws_inbound("ask 回写", |frame| frame["tool_call_id"] == "tc-ask")
                .await;
            assert_eq!(
                answer["content"],
                "Fruit: banana\nTags: urgent, later\nq3: (user did not answer)"
            );

            gw.push(wing_event(&sid, turn_result()));
            gw.push(wing_event(&sid, done()));
            let response = prompt.await.expect("prompt 任务被取消")?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            assert!(
                rec.permissions().is_empty(),
                "有 elicitation 能力时不该走逐题 permission 回退"
            );

            // 第二轮：elicitation 被回 -32601 → 降级 + 本轮回退（逐题 permission）。
            let single = json!([
                {
                    "id": "r1", "question": "重试吗？", "header": "Retry", "multiSelect": false,
                    "options": [{"label": "a", "description": ""}, {"label": "b", "description": ""}],
                },
            ]);
            let prompt = spawn_prompt(&connection, new.session_id.clone(), "再问一次")?;
            gw.wait_ws_inbound("prompt 上行帧", |frame| {
                frame["content"] == "再问一次"
            })
            .await;
            gw.push(wing_event(&sid, questions_ask("tc-ask-2", single.clone())));
            let second = rec.wait_elicitation("第二次 elicitation", 2).await;
            assert_eq!(second["message"], "重试吗？", "单题取问题全文");
            let fallback = rec.wait_permission("降级后的回退 permission", 1).await;
            assert_eq!(fallback["toolCall"]["toolCallId"], "tc-ask-2:r1");
            let answer = gw
                .wait_ws_inbound("第二轮回写", |frame| frame["tool_call_id"] == "tc-ask-2")
                .await;
            assert_eq!(answer["content"], "Retry: a");
            gw.push(wing_event(&sid, turn_result()));
            let response = prompt.await.expect("prompt 任务被取消")?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);

            // 第三轮：降级是进程级粘性状态——不再发 elicitation，直接回退。
            let prompt = spawn_prompt(&connection, new.session_id.clone(), "第三次")?;
            gw.wait_ws_inbound("prompt 上行帧", |frame| {
                frame["content"] == "第三次"
            })
            .await;
            gw.push(wing_event(&sid, questions_ask("tc-ask-3", single)));
            let fallback = rec.wait_permission("粘性降级后的回退 permission", 2).await;
            assert_eq!(fallback["toolCall"]["toolCallId"], "tc-ask-3:r1");
            let answer = gw
                .wait_ws_inbound("第三轮回写", |frame| frame["tool_call_id"] == "tc-ask-3")
                .await;
            assert_eq!(answer["content"], "Retry: b");
            gw.push(wing_event(&sid, turn_result()));
            let response = prompt.await.expect("prompt 任务被取消")?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            assert_eq!(
                rec.elicitations().len(),
                2,
                "降级后不再发 elicitation（粘性）"
            );
            Ok(())
        },
    )
    .await
    .expect("elicitation 场景");
}

// ============================================================
// B6 问答形态 → 无 elicitation 能力的回退
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn questions_ask_falls_back_to_per_question_permissions() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[("fake", &[("echo-1", "Echo One")])]));

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let rec = Arc::clone(&recorder);
    let gw = Arc::clone(&gateway);
    let ws = workspace.clone();

    // 第 1 题选第二个选项（banana）；第 2 题（自由文本）选 Continue（未答、继续）。
    permissions.push(PermissionReply::Select("__wing_opt_0_1".to_string()));
    permissions.push(PermissionReply::Select("__wing_continue__".to_string()));

    let questions = json!([
        {
            "id": "q1", "question": "选哪个水果？", "header": "Fruit", "multiSelect": false,
            "options": [{"label": "apple", "description": ""}, {"label": "banana", "description": ""}],
        },
        {"id": "q2", "question": "补充说明？", "header": "Notes", "multiSelect": false, "options": []},
    ]);

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            // 不广告 elicitation：走逐题 permission 回退。
            initialize(&connection, false).await?;
            let new = connection
                .send_request(NewSessionRequest::new(ws))
                .block_task()
                .await?;
            let sid = new.session_id.to_string();

            let prompt = spawn_prompt(&connection, new.session_id.clone(), "帮我确认几个问题")?;
            gw.wait_ws_inbound("prompt 上行帧", |frame| {
                frame["session_id"] == sid.as_str()
            })
            .await;
            gw.push(wing_event(
                &sid,
                tool_call("tc-ask", "AskUserQuestion", json!({})),
            ));
            gw.push(wing_event(&sid, questions_ask("tc-ask", questions)));

            // 逐题串行：合成 toolCallId `<真实 id>:<题号>`，选项保留 id + 逃生选项。
            let first = rec.wait_permission("第 1 题的 permission", 1).await;
            assert_eq!(first["toolCall"]["toolCallId"], "tc-ask:q1");
            assert_eq!(first["options"][0]["optionId"], "__wing_opt_0_0");
            assert_eq!(first["options"][0]["name"], "apple");
            assert_eq!(first["options"][0]["kind"], "allow_once");
            assert_eq!(first["options"][1]["optionId"], "__wing_opt_0_1");
            assert_eq!(
                first["options"][2],
                json!({"optionId": "__wing_skip__", "name": "Skip", "kind": "reject_once"})
            );

            let second = rec.wait_permission("第 2 题的 permission", 2).await;
            assert_eq!(second["toolCall"]["toolCallId"], "tc-ask:q2");
            assert_eq!(
                second["options"][0],
                json!({"optionId": "__wing_continue__", "name": "Continue", "kind": "allow_once"})
            );
            assert_eq!(second["options"][1]["optionId"], "__wing_skip__");

            let answer = gw
                .wait_ws_inbound("ask 回写", |frame| frame["tool_call_id"] == "tc-ask")
                .await;
            assert_eq!(
                answer["content"],
                "Fruit: banana\nNotes: (user did not answer)"
            );

            gw.push(wing_event(&sid, turn_result()));
            gw.push(wing_event(&sid, done()));
            let response = prompt.await.expect("prompt 任务被取消")?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            assert!(
                rec.elicitations().is_empty(),
                "客户端没有广告 elicitation：不该收到 elicitation/create"
            );
            Ok(())
        },
    )
    .await
    .expect("permission 回退场景");
}

// ============================================================
// B7 set_config_option + 外部变更中继
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn set_config_option_updates_the_session_and_relays_external_changes() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[
        ("fake", &[("echo-1", "Echo One"), ("echo-2", "Echo Two")]),
        ("other", &[("solo-1", "Solo One")]),
    ]));

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let rec = Arc::clone(&recorder);
    let gw = Arc::clone(&gateway);
    let ws = workspace.clone();

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            initialize(&connection, false).await?;
            let new = connection
                .send_request(NewSessionRequest::new(ws))
                .block_task()
                .await?;
            let sid = new.session_id.to_string();

            // 值域是全量的（两个 provider 的分组），currentValue 是当前模型。
            let options = new.config_options.expect("configOptions 已广告");
            let option = serde_json::to_value(&options[0]).expect("option 序列化");
            assert_eq!(option["currentValue"], "fake:echo-1");
            let groups = option["options"].as_array().expect("分组值域");
            assert_eq!(groups.len(), 2, "两个 provider 各一组");
            assert_eq!(groups[0]["group"], "fake");
            assert_eq!(
                groups[0]["options"]
                    .as_array()
                    .expect("fake 组的值")
                    .iter()
                    .map(|value| value["value"].as_str().unwrap_or_default())
                    .collect::<Vec<_>>(),
                ["fake:echo-1", "fake:echo-2"]
            );
            assert_eq!(groups[1]["group"], "other");
            assert_eq!(groups[1]["options"][0]["value"], "other:solo-1");

            // 切换：`provider:model` 值 id → `POST /api/session/update{model, provider}`。
            let switched = connection
                .send_request(SetSessionConfigOptionRequest::new(
                    new.session_id.clone(),
                    "model",
                    "fake:echo-2",
                ))
                .block_task()
                .await?;
            let update = gw
                .wait_request("session/update", |r| r.path == "/api/session/update")
                .await;
            assert_eq!(
                update.body.expect("update 请求体"),
                json!({"session_id": sid, "model": "echo-2", "provider": "fake"})
            );
            let option = serde_json::to_value(&switched.config_options[0]).expect("option 序列化");
            assert_eq!(
                option["currentValue"], "fake:echo-2",
                "响应带回全量 options（网关是权威）"
            );

            // 外部（TUI 等）改模型 → `session_state_changed` → `config_option_update` 中继。
            gw.set_session_state(&sid, "fake", "echo-3", Some("Echo Three"));
            gw.push(wing_event(
                &sid,
                json!({"type": "session_state_changed", "model": "echo-3", "model_display_name": "Echo Three"}),
            ));
            let relay = rec
                .wait_update("config_option_update 中继", &sid, |update| {
                    update_kind(update) == "config_option_update"
                })
                .await;
            assert_eq!(relay["configOptions"][0]["id"], "model");
            assert_eq!(relay["configOptions"][0]["currentValue"], "fake:echo-3");

            // 未知 config id：invalid params（不静默接受一个不存在的 option）。
            let error = connection
                .send_request(SetSessionConfigOptionRequest::new(
                    new.session_id.clone(),
                    "thinking",
                    "true",
                ))
                .block_task()
                .await
                .expect_err("未知 config id 必须报错");
            assert_eq!(error.code, ErrorCode::InvalidParams);
            assert!(
                error.message.contains("unknown config option")
                    || error
                        .data
                        .as_ref()
                        .is_some_and(|data| data.to_string().contains("unknown config option")),
                "错误要指明原因：{error:?}"
            );
            Ok(())
        },
    )
    .await
    .expect("模型切换场景");
}

// ============================================================
// B8 session/list
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn session_list_maps_filters_and_paginates() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[("fake", &[("echo-1", "Echo One")])]));
    gateway.set_sessions(vec![
        session_row(
            "s-old",
            Some("旧会话"),
            Some(&workspace),
            Some("2026-01-02T03:04:05.123456"),
        ),
        session_row("s-nows", Some("没有 workspace"), None, None),
        session_row("s-rel", None, Some("relative/dir"), None),
    ]);

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let ws = workspace.clone();

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            initialize(&connection, false).await?;

            // 不带 cwd：无 / 非绝对 workspace 的会话跳过，字段映射到位。
            let all = connection
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            assert_eq!(all.sessions.len(), 1, "只有 s-old 可列出");
            assert!(all.next_cursor.is_none(), "一页装得下就没有游标");
            let entry = &all.sessions[0];
            assert_eq!(entry.session_id.to_string(), "s-old");
            assert_eq!(entry.cwd.to_string_lossy(), ws);
            assert_eq!(entry.title.as_deref(), Some("旧会话"));
            let updated_at = entry.updated_at.as_deref().expect("updatedAt 已转换");
            let parsed = chrono::DateTime::parse_from_rfc3339(updated_at)
                .unwrap_or_else(|err| panic!("updatedAt 必须是 RFC3339: {updated_at} ({err})"));
            assert_eq!(
                parsed
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
                "2026-01-02T03:04:05",
                "本地 naive → RFC3339 的墙上时间不变"
            );

            // cwd 精确匹配（尾斜杠容忍、前缀不误伤）。
            let same = connection
                .send_request(
                    ListSessionsRequest::new().cwd(std::path::PathBuf::from(format!("{ws}/"))),
                )
                .block_task()
                .await?;
            assert_eq!(same.sessions.len(), 1);
            let other = connection
                .send_request(
                    ListSessionsRequest::new().cwd(std::path::PathBuf::from(format!("{ws}/sub"))),
                )
                .block_task()
                .await?;
            assert!(other.sessions.is_empty(), "前缀不命中");

            // 越界 offset：空页 + 无游标（不是错误）。
            let beyond = connection
                .send_request(ListSessionsRequest::new().cursor("999"))
                .block_task()
                .await?;
            assert!(beyond.sessions.is_empty());
            assert!(beyond.next_cursor.is_none());

            // 非法游标：invalid params。
            let error = connection
                .send_request(ListSessionsRequest::new().cursor("not-a-cursor"))
                .block_task()
                .await
                .expect_err("非法 cursor 必须报错");
            assert_eq!(error.code, ErrorCode::InvalidParams);
            Ok(())
        },
    )
    .await
    .expect("session/list 场景");
}

// ============================================================
// B9 session/load（回放） + session/resume（不回放）
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn session_load_replays_before_the_response_and_resume_skips_it() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[("fake", &[("echo-1", "Echo One")])]));
    gateway.set_commands(commands_catalog(&[("model", "Switch the model", "")]));
    gateway.add_resumable("s-old", Some(&workspace));
    gateway.set_session_state("s-old", "fake", "echo-1", Some("Echo One"));
    gateway.set_session_info("s-old", Some("旧会话"), 321, 262_144);

    let a_path = format!("{workspace}/src/a.rs");
    let b_path = format!("{workspace}/src/b.rs");
    let messages = json!([
        {"role": "user", "content": "看一下这个 bug"},
        {
            "role": "assistant",
            "content": "我来改",
            "reasoning_content": "先看代码",
            "tool_calls": [
                {"id": "tc-a", "name": "Edit", "arguments": {"path": a_path}},
                {"id": "tc-b", "name": "Write", "arguments": {"path": b_path}},
            ],
            "tool_call_id": null,
        },
        {"role": "tool", "content": "applied a", "tool_call_id": "tc-a", "tool_name": "Edit"},
    ]);
    let events = json!([
        {
            "type": "diff_content", "tool_call_id": "tc-b", "path": b_path,
            "old_text": null, "new_text": "new file\n",
            "created_at": "2026-01-01T00:00:00+00:00", "session_id": "s-old", "request_id": "req-1",
        },
    ]);
    let mut snapshot = sync_session("s-old", messages, events);
    // 半成品投影不得漏进回放。
    snapshot["uncommitted"] = json!({"role": "assistant", "content": "UNCOMMITTED-LEAK"});
    gateway.set_snapshot("s-old", snapshot);

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let rec = Arc::clone(&recorder);
    let gw = Arc::clone(&gateway);

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            initialize(&connection, false).await?;

            // load：resume（存在性校验）→ 订阅 → 回放 → 标题/用量/命令 → 响应。
            let loaded = connection
                .send_request(LoadSessionRequest::new("s-old", workspace.clone()))
                .block_task()
                .await?;
            rec.marker("load-response");
            assert!(loaded.config_options.is_some(), "load 响应带 configOptions");

            let resume = gw
                .wait_request("session/resume", |r| r.path == "/api/session/resume")
                .await;
            assert_eq!(
                resume.body.expect("resume 请求体"),
                json!({"session_id": "s-old"})
            );
            let subscribe = gw
                .wait_request("session/subscribe", |r| {
                    r.path == "/api/session/subscribe"
                        && r.body
                            .as_ref()
                            .is_some_and(|body| body["session_id"] == "s-old")
                })
                .await;
            assert_eq!(subscribe.client_id.as_deref(), Some(gw.client_id()));

            // 回放帧序（全部在响应之前——marker 之后的第一条 update 就是 resume 的收尾帧）。
            let replayed = rec.updates_before_marker("s-old", "load-response");
            assert_eq!(
                describe_kinds(&replayed.iter().map(update_kind).collect::<Vec<_>>()),
                concat!(
                    "user_message_chunk → agent_thought_chunk → agent_message_chunk → ",
                    "tool_call → tool_call → tool_call_update → tool_call_update → ",
                    "session_info_update → usage_update → available_commands_update",
                ),
                "load 回放帧序（全部在响应之前）"
            );
            assert!(
                !replayed
                    .iter()
                    .any(|update| update.to_string().contains("UNCOMMITTED-LEAK")),
                "半成品投影不得进回放：{replayed:?}"
            );
            assert_eq!(replayed[0]["content"]["text"], "看一下这个 bug");
            let first_card: ToolCall =
                serde_json::from_value(replayed[3].clone()).expect("tc-a 创建帧");
            assert_eq!(first_card.tool_call_id.to_string(), "tc-a");
            assert_eq!(first_card.title, a_path);
            assert_eq!(first_card.kind, ToolKind::Edit);
            assert_eq!(first_card.status, ToolCallStatus::InProgress);
            let completed: ToolCallUpdate =
                serde_json::from_value(replayed[5].clone()).expect("tc-a 收口帧");
            assert_eq!(completed.fields.status, Some(ToolCallStatus::Completed));
            let diff: ToolCallUpdate =
                serde_json::from_value(replayed[6].clone()).expect("tc-b diff 帧");
            match &diff.fields.content.expect("diff content")[0] {
                ToolCallContent::Diff(diff) => {
                    assert_eq!(diff.path.to_string_lossy(), b_path);
                    assert_eq!(diff.new_text, "new file\n");
                }
                other => panic!("期望 Diff 内容，得到 {other:?}"),
            }
            assert_eq!(replayed[7]["title"], "旧会话");
            assert_eq!(replayed[8]["used"], 321);
            assert_eq!(replayed[8]["size"], 262_144);

            // resume：不回放，只补标题 / 用量 / 命令。
            let resumed = connection
                .send_request(ResumeSessionRequest::new("s-old", workspace.clone()))
                .block_task()
                .await?;
            rec.marker("resume-response");
            assert!(resumed.config_options.is_some());
            let after = rec.updates_after_marker("s-old", "load-response");
            assert_eq!(
                describe_kinds(&after.iter().map(update_kind).collect::<Vec<_>>()),
                "session_info_update → usage_update → available_commands_update",
                "resume 不回放历史（只补事实帧）"
            );

            // 未知会话：invalid params，且不产生 subscribe。
            let subscribes_before = gw.count("/api/session/subscribe");
            let error = connection
                .send_request(LoadSessionRequest::new("s-missing", workspace.clone()))
                .block_task()
                .await
                .expect_err("未知会话必须报错");
            assert_eq!(error.code, ErrorCode::InvalidParams);
            assert!(error.message.contains("unknown session") || error.data.is_some());
            settle().await;
            assert_eq!(
                gw.count("/api/session/subscribe"),
                subscribes_before,
                "未知会话不该产生订阅"
            );
            Ok(())
        },
    )
    .await
    .expect("load/resume 场景");
}

// ============================================================
// B10 session/close
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn session_close_unsubscribes_and_releases_idempotently() {
    let harness = Harness::start().await;
    let gateway = Arc::clone(&harness.gateway);
    let workspace = harness.workspace();
    gateway.set_models(models_catalog(&[("fake", &[("echo-1", "Echo One")])]));

    let recorder = Recorder::new();
    let permissions = PermissionScript::new();
    let elicitations = ElicitationScript::new();
    let gw = Arc::clone(&gateway);
    let ws = workspace.clone();

    run_client(
        harness.agent(),
        recorder,
        permissions,
        elicitations,
        async move |connection: ConnectionTo<Agent>| {
            initialize(&connection, false).await?;
            let new = connection
                .send_request(NewSessionRequest::new(ws))
                .block_task()
                .await?;
            let sid = new.session_id.to_string();

            // close：unsubscribe（带 X-Client-Id）+ release。
            connection
                .send_request(CloseSessionRequest::new(new.session_id.clone()))
                .block_task()
                .await?;
            let unsubscribe = gw
                .wait_request("unsubscribe", |r| r.path == "/api/session/unsubscribe")
                .await;
            assert_eq!(unsubscribe.client_id.as_deref(), Some(gw.client_id()));
            assert_eq!(
                unsubscribe.body.expect("unsubscribe 请求体"),
                json!({"session_id": sid})
            );
            let release = gw
                .wait_request("release", |r| r.path == "/api/session/release")
                .await;
            assert_eq!(
                release.body.expect("release 请求体"),
                json!({"session_id": sid})
            );

            // 幂等：重复 close 不再发 HTTP（hub 已回收条目）。
            let unsubscribes = gw.count("/api/session/unsubscribe");
            let releases = gw.count("/api/session/release");
            connection
                .send_request(CloseSessionRequest::new(new.session_id.clone()))
                .block_task()
                .await?;
            // 未知会话的 close 同样幂等成功。
            connection
                .send_request(CloseSessionRequest::new("sess-never"))
                .block_task()
                .await?;
            settle().await;
            assert_eq!(gw.count("/api/session/unsubscribe"), unsubscribes);
            assert_eq!(gw.count("/api/session/release"), releases);

            // 关闭后的会话不再可用。
            let error = connection
                .send_request(PromptRequest::new(
                    new.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("还在吗".to_string()))],
                ))
                .block_task()
                .await
                .expect_err("close 之后的 prompt 必须报错");
            assert_eq!(error.code, ErrorCode::InvalidParams);
            Ok(())
        },
    )
    .await
    .expect("close 场景");
}
