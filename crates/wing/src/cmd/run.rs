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

    let (session_id, template_name, resolved_workspace) = if let Some(ref resume_id) = args.resume {
        let resp = http.resume_session(resume_id).await?;
        // `-r` + `--tag`: tags are added to the resumed session (idempotent;
        // the resumed session keeps its existing tags).
        if !args.tag.is_empty() {
            http.tag_session(&resp.session_id, Some(args.tag.clone()), None)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to tag session: {e}"))?;
        }
        (
            resp.session_id,
            resp.template_name.unwrap_or_default(),
            resp.workspace,
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

        let create_req = CreateSessionRequest {
            template_name: None,
            workspace: workspace.clone(),
            agent: Some(override_),
            backend: None,
            // Atomic: the session is born tagged (no dispatch-without-tags window).
            tags: (!args.tag.is_empty()).then(|| args.tag.clone()),
            // `wing run` 不暴露 --session-id：id 由后端生成（stdio 前端才有
            // create-or-adopt，见 crates/wing/src/stdio/mod.rs）。
            session_id: None,
        };

        let resp = http.create_session(&create_req).await?;
        // 创建即带标；读回校验——旧网关（早于 tags 端点）会在这里响亮失败，
        // 而不是静默丢标后继续把 prompt 发出去。
        common::ensure_tags_applied(&http, &resp.session_id, &args.tag).await?;
        (resp.session_id, resp.template_name, resp.workspace)
    };

    // 3. Send prompt (non-blocking — the HTTP call returns immediately,
    //    the agent processes asynchronously). Always sent, including on
    //    resume: `wing run -r <sid> -p "next task"` resumes + sends.
    http.send_message(&session_id, &args.prompt, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send prompt: {e}"))?;

    // 4. Fetch session info for model/tools/tags display (resulting state —
    //    on `-r` this includes tags the session already had). The model
    //    identity comes from the session itself (`model_id` + call name +
    //    provider fact + declared label).
    let (model, model_id, provider, model_display_name, tools_list, tags) =
        match http.get_session_info(&session_id).await {
            Ok(info) => (
                info.model,
                info.model_id,
                info.provider_name,
                info.model_display_name,
                info.tools,
                info.tags,
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
                )
            }
        };

    Ok(RunOutput {
        session_id,
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
