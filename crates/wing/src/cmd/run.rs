//! `wing run` — non-blocking task launch.
//!
//! Creates a session, sends the prompt, and returns immediately with
//! the session ID. Unlike `wing -p` (blocking, Claude Code compatible),
//! `wing run` does not wait for the task to complete.
//!
//! # Example (agent orchestrator pattern)
//!
//! ```sh
//! SID=$(wing run -p "fix the bug" --json | jq -r .session_id)
//! # ... do other work ...
//! wing wait "$SID"
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use serde::Serialize;
use wing_api_client::models::{AgentOverride, CreateSessionRequest};

use super::args::RunArgs;
use super::common;

/// Output of `wing run`.
#[derive(Serialize)]
struct RunOutput {
    session_id: String,
    /// The **recorded** creator of this session (read back; `null` for a root
    /// or when the gateway predates session lineage).
    parent_session_id: Option<String>,
    template_name: String,
    /// 当前模型的调用名（发给上游的值）。
    model: String,
    /// 当前模型的引用词（∈ 配置声明的 id 空间；不可用时 None）。
    model_id: Option<String>,
    /// 提供该模型的 provider（运行期事实，来自 session info）。
    provider: Option<String>,
    /// 声明的展示名（未声明 / 不可用时 None）。
    model_display_name: Option<String>,
    tools: Vec<String>,
    /// Resulting tags reported by session info (on `-r`: existing + newly
    /// added); falls back to the requested `--tag` list if info is unavailable.
    tags: Vec<String>,
    workspace: Option<String>,
    prompt: String,
    started_at: String,
}

/// Entry point for `wing run`.
pub async fn run(args: RunArgs, json: bool) -> ExitCode {
    match run_inner(args).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_text(&output);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing run error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_inner(args: RunArgs) -> Result<RunOutput> {
    // 1. Ensure gateway is running.
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;

    // 2. Create session (with agent override).
    let workspace = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    let tools = args.tools.as_ref().map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect::<Vec<String>>()
    });

    let started_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    let (session_id, template_name, resolved_workspace, created_parent) =
        if let Some(ref resume_id) = args.resume {
            // `--agent` 只作用在创建路径：resume 的会话保持自己的 agent。给了没反应
            // 必须可诊断（与 stdio 路径的 resume-ignored 警告同口径）。
            if args.agent.is_some() {
                tracing::warn!(
                    "--agent is ignored on resume: the resumed session keeps its agent \
                 (switch it with `wing update <sid> --agent <name>`)"
                );
            }
            // 恢复失败（含"已归档"）走统一的会话错误文案：`wing run -r <归档 id>`
            // 拿到的是一句可行动的提示，不是裸的网关 detail。
            let resp = http
                .resume_session(resume_id)
                .await
                .map_err(|e| common::session_error(resume_id, &e))?;
            // `-r` + `--tag`: tags are added to the resumed session (idempotent;
            // the resumed session keeps its existing tags).
            if !args.tag.is_empty() {
                http.tag_session(&resp.session_id, Some(args.tag.clone()), None)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to tag session: {e}"))?;
            }
            // 恢复不是创建：不读 env（既有会话的 parent 是它出生时的事实）。
            (
                resp.session_id,
                resp.template_name.unwrap_or_default(),
                resp.workspace,
                None,
            )
        } else {
            let override_ = AgentOverride {
                model_id: args.model.clone(),
                system_prompt: args.system_prompt.clone(),
                append_system_prompt: args.append_system_prompt.clone(),
                tools,
                max_turns: args.max_turns,
                effort: args.effort.clone(),
                yolo: Some(true),
            };

            // 血缘采集：Bash 工具给 agent 的 shell 注入了 `WING_SESSION_ID`，
            // 于是"由谁创建"在创建请求里带上（软信号，非法值丢弃 + 留痕）。
            let requested_parent = common::parent_session_id_from_env();
            let create_req = CreateSessionRequest {
                template_name: args.agent.clone(),
                workspace: workspace.clone(),
                agent: Some(override_),
                backend: None,
                // Atomic: the session is born tagged (no dispatch-without-tags window).
                tags: (!args.tag.is_empty()).then(|| args.tag.clone()),
                // `wing run` 不暴露 --session-id：id 由后端生成（stdio 前端才有
                // create-or-adopt，见 crates/wing/src/stdio/mod.rs）。
                session_id: None,
                parent_session_id: requested_parent.clone(),
            };

            let resp = http.create_session(&create_req).await?;
            // 创建即带标；读回校验——旧网关（早于 tags 端点）会在这里响亮失败，
            // 而不是静默丢标后继续把 prompt 发出去。
            common::ensure_tags_applied(&http, &resp.session_id, &args.tag).await?;
            // 血缘是软机制：请求了但网关没带（旧网关）只记日志，不吵。
            common::warn_if_parent_dropped(
                requested_parent.as_deref(),
                resp.parent_session_id.as_deref(),
            );
            (
                resp.session_id,
                resp.template_name,
                resp.workspace,
                resp.parent_session_id,
            )
        };

    // 3. Send prompt (non-blocking — the HTTP call returns immediately,
    //    the agent processes asynchronously). Always sent, including on
    //    resume: `wing run -r <sid> -p "next task"` resumes + sends.
    //    `--tool-call-id` routes the prompt to a pending Ask's feedback
    //    waiter instead of the session inbox (answering an ask).
    http.send_message(&session_id, &args.prompt, args.tool_call_id.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send prompt: {e}"))?;

    // 4. Fetch session info for model/tools/tags display (resulting state —
    //    on `-r` this includes tags the session already had). The model
    //    identity comes from the session itself (`model_id` + call name +
    //    provider fact + declared label).
    let (model, model_id, provider, model_display_name, tools_list, tags, info_parent) =
        match http.get_session_info(&session_id).await {
            Ok(info) => (
                info.model,
                info.model_id,
                info.provider_name,
                info.model_display_name,
                info.tools,
                info.tags,
                info.parent_session_id,
            ),
            Err(e) => {
                // Non-fatal: the session was created and the prompt was sent.
                // The agent is working; we just couldn't get display metadata.
                // The requested `--model` is the best available identity here
                // (it is exactly what was sent as `model_id`).
                tracing::warn!("Failed to get session info: {e}");
                let requested = args.model.clone();
                (
                    requested.clone().unwrap_or_default(),
                    requested,
                    None,
                    None,
                    vec![],
                    args.tag,
                    None,
                )
            }
        };

    // 血缘（读回）：创建路径以 create 响应为准（对账的权威通道，上面已比对）；
    // `-r` 没有 create 响应，取 info 读回的既有事实。
    let parent_session_id = created_parent.or(info_parent);

    Ok(RunOutput {
        session_id,
        parent_session_id,
        template_name,
        model,
        model_id,
        provider,
        model_display_name,
        tools: tools_list,
        tags,
        workspace: resolved_workspace,
        prompt: args.prompt,
        started_at,
    })
}

fn print_text(output: &RunOutput) {
    println!("session_id:  {}", output.session_id);
    if let Some(ref parent) = output.parent_session_id {
        println!("parent:      {parent}");
    }
    println!("model:       {}", output.model);
    if let Some(ref id) = output.model_id {
        println!("model_id:    {id}");
    }
    if let Some(ref label) = output.model_display_name {
        println!("model_label: {label}");
    }
    if let Some(ref p) = output.provider {
        println!("provider:    {p}");
    }
    println!("template:    {}", output.template_name);
    println!("tools:       {}", output.tools.join(", "));
    if !output.tags.is_empty() {
        println!("tags:        {}", output.tags.join(", "));
    }
    if let Some(ref ws) = output.workspace {
        println!("workspace:   {ws}");
    }
    // Truncate prompt for display if too long.
    let prompt_display = common::truncate_chars(&output.prompt, 80);
    println!("prompt:      {prompt_display}");
    println!("started_at:  {}", output.started_at);
}
