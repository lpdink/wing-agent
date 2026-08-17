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
    model: String,
    provider: Option<String>,
    tools: Vec<String>,
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
        (
            resp.session_id,
            resp.template_name.unwrap_or_default(),
            resp.workspace,
        )
    } else {
        let override_ = AgentOverride {
            model: args.model.clone(),
            provider: args.provider.clone(),
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
        };

        let resp = http.create_session(&create_req).await?;
        (resp.session_id, resp.template_name, resp.workspace)
    };

    // 3. Send prompt (non-blocking — the HTTP call returns immediately,
    //    the agent processes asynchronously). Always sent, including on
    //    resume: `wing run -r <sid> -p "next task"` resumes + sends.
    http.send_message(&session_id, &args.prompt, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send prompt: {e}"))?;

    // 4. Fetch session info for model/tools display.
    let (model, tools_list) = match http.get_session_info(&session_id).await {
        Ok(info) => (info.model, info.tools),
        Err(e) => {
            // Non-fatal: the session was created and the prompt was sent.
            // The agent is working; we just couldn't get display metadata.
            tracing::warn!("Failed to get session info: {e}");
            (args.model.unwrap_or_default(), vec![])
        }
    };

    Ok(RunOutput {
        session_id,
        template_name,
        model,
        provider: args.provider,
        tools: tools_list,
        workspace: resolved_workspace,
        prompt: args.prompt,
        started_at,
    })
}

fn print_text(output: &RunOutput) {
    println!("session_id:  {}", output.session_id);
    println!("model:       {}", output.model);
    if let Some(ref p) = output.provider {
        println!("provider:    {p}");
    }
    println!("template:    {}", output.template_name);
    println!("tools:       {}", output.tools.join(", "));
    if let Some(ref ws) = output.workspace {
        println!("workspace:   {ws}");
    }
    // Truncate prompt for display if too long.
    let prompt_display = common::truncate_chars(&output.prompt, 80);
    println!("prompt:      {prompt_display}");
    println!("started_at:  {}", output.started_at);
}
