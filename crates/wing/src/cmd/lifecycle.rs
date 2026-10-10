//! `wing new` / `wing resume` — session creation and hydration.
//!
//! `wing new` creates an **empty** session and prints its id — the building
//! block for "make me a session, I will drive it myself" (`wing update` to
//! shape it, `wing run -r` to prompt it). Its workspace defaults to the
//! current directory, exactly like every other creation path (`wing run`,
//! the TUI, stdio).
//!
//! `wing resume` is the explicit hydration entry: it loads an evicted session
//! back into gateway memory and prints the state summary. The 404→resume
//! convention already hydrates on demand for `wing tail` / `info` /
//! `branches` / `fork` / …; `resume` makes the step itself a command, which
//! is what a script wants when it needs the session loaded *before* it does
//! something else.
//!
//! ```sh
//! SID=$(wing new --template executor --tag nightly --json | jq -r .session_id)
//! wing resume "$SID"          # → status / model / token summary
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use wing_api_client::models::CreateSessionRequest;

use super::common;

// ============================================================
// wing new
// ============================================================

/// `wing new` arguments.
#[derive(Args, Debug, Clone)]
pub struct NewArgs {
    /// Workspace (working directory) for the session (default: the current directory).
    #[arg(long)]
    pub workspace: Option<String>,

    /// Agent template name (default: the gateway's default template).
    #[arg(long)]
    pub template: Option<String>,

    /// Tags attached at creation (repeatable / comma-separated).
    #[arg(long = "tag", value_delimiter = ',')]
    pub tag: Vec<String>,
}

/// Output of `wing new`.
#[derive(Serialize)]
struct NewOutput {
    session_id: String,
    template_name: String,
    workspace: Option<String>,
    /// Storage backend ("file" = durable, "memory" = ephemeral); `null` on
    /// gateways predating backend selection.
    backend: Option<String>,
    /// The session's resulting tags (read back, not echoed).
    tags: Vec<String>,
}

/// Entry point for `wing new`.
pub async fn run_new(args: NewArgs, json: bool) -> ExitCode {
    match new_inner(&args).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_new(&output);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing new error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn new_inner(args: &NewArgs) -> Result<NewOutput> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;

    // 与其它创建路径同口径：没给 --workspace 就用本进程的 cwd（"会话在我在的
    // 地方干活"），给了就照给。
    let workspace = match &args.workspace {
        Some(path) => Some(path.clone()),
        None => std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string()),
    };

    let request = CreateSessionRequest {
        template_name: args.template.clone(),
        workspace,
        // Atomic: the session is born tagged (no dispatch-without-tags window).
        tags: (!args.tag.is_empty()).then(|| args.tag.clone()),
        ..Default::default()
    };
    let resp = http
        .create_session(&request)
        .await
        .map_err(|e| anyhow::anyhow!("failed to create session: {e}"))?;
    // 创建即带标；读回校验——旧网关（早于 tags 端点）在这里响亮失败，
    // 而不是静默丢标后让调用方以为会话已按标签登记。
    let tags = common::ensure_tags_applied(&http, &resp.session_id, &args.tag).await?;

    Ok(NewOutput {
        session_id: resp.session_id,
        template_name: resp.template_name,
        workspace: resp.workspace,
        backend: resp.backend,
        tags,
    })
}

fn print_new(output: &NewOutput) {
    println!("session_id:  {}", output.session_id);
    println!("template:    {}", output.template_name);
    if let Some(backend) = &output.backend {
        println!("backend:     {backend}");
    }
    if !output.tags.is_empty() {
        println!("tags:        {}", output.tags.join(", "));
    }
    if let Some(workspace) = &output.workspace {
        println!("workspace:   {workspace}");
    }
}

// ============================================================
// wing resume
// ============================================================

/// Output of `wing resume`.
#[derive(Serialize)]
struct ResumeOutput {
    session_id: String,
    template_name: Option<String>,
    workspace: Option<String>,
    /// Runtime state after hydration (idle / working / waiting); empty when
    /// the info read failed (the hydrate itself succeeded).
    status: String,
    /// Call name of the bound model.
    model: String,
    /// Reference word of the bound model (null when unresolvable).
    model_id: Option<String>,
    message_count: i64,
    total_tokens: i64,
}

/// Entry point for `wing resume`.
pub async fn run_resume(session_id: &str, json: bool) -> ExitCode {
    match resume_inner(session_id).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_resume(&output);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing resume error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn resume_inner(session_id: &str) -> Result<ResumeOutput> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    let resp = http
        .resume_session(session_id)
        .await
        .map_err(|e| common::session_error(session_id, &e))?;

    // 状态摘要是尽力而为的附加值：水合已经成功，info 读失败（如旧网关）不该
    // 把整条命令判失败——照常打印可确认的部分。
    let (status, model, model_id, message_count, total_tokens) =
        match http.get_session_info(session_id).await {
            Ok(info) => (
                info.status,
                info.model,
                info.model_id,
                info.context_stats.message_count,
                info.context_stats.total_tokens,
            ),
            Err(e) => {
                tracing::warn!("resume: session info unavailable: {e}");
                (String::new(), String::new(), None, 0, 0)
            }
        };

    Ok(ResumeOutput {
        session_id: resp.session_id,
        template_name: resp.template_name,
        workspace: resp.workspace,
        status,
        model,
        model_id,
        message_count,
        total_tokens,
    })
}

fn print_resume(output: &ResumeOutput) {
    print!("{}", format_resume(output));
}

/// The human summary (pure, so the fallbacks are testable).
fn format_resume(output: &ResumeOutput) -> String {
    let mut text = String::new();
    text.push_str(&format!("session_id:  {}\n", output.session_id));
    if !output.status.is_empty() {
        text.push_str(&format!("status:      {}\n", output.status));
    }
    if output.model.is_empty() {
        text.push_str("model:       -\n");
    } else if let Some(model_id) = &output.model_id {
        text.push_str(&format!("model:       {} ({model_id})\n", output.model));
    } else {
        text.push_str(&format!("model:       {}\n", output.model));
    }
    if let Some(template) = &output.template_name {
        text.push_str(&format!("template:    {template}\n"));
    }
    if let Some(workspace) = &output.workspace {
        text.push_str(&format!("workspace:   {workspace}\n"));
    }
    text.push_str(&format!("messages:    {}\n", output.message_count));
    text.push_str(&format!("tokens:      {}\n", output.total_tokens));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_text_shows_model_identity_and_skips_missing_parts() {
        let output = ResumeOutput {
            session_id: "s1".into(),
            template_name: Some("executor".into()),
            workspace: Some("/tmp/ws".into()),
            status: "idle".into(),
            model: "dfmodel-2026".into(),
            model_id: Some("ds-flash".into()),
            message_count: 12,
            total_tokens: 3456,
        };
        let text = format_resume(&output);
        assert!(text.contains("session_id:  s1"), "{text}");
        assert!(text.contains("status:      idle"), "{text}");
        assert!(
            text.contains("model:       dfmodel-2026 (ds-flash)"),
            "{text}"
        );
        assert!(text.contains("messages:    12"), "{text}");
        assert!(text.contains("tokens:      3456"), "{text}");

        // info 读失败的降级形态：status / model 行不出现，可确认的部分照常。
        let bare = ResumeOutput {
            session_id: "s1".into(),
            template_name: None,
            workspace: None,
            status: String::new(),
            model: String::new(),
            model_id: None,
            message_count: 0,
            total_tokens: 0,
        };
        let text = format_resume(&bare);
        assert!(!text.contains("status:"), "{text}");
        assert!(text.contains("session_id:  s1"), "{text}");
        assert!(text.contains("messages:    0"), "{text}");
    }
}
