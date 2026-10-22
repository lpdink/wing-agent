//! `wing run` — non-blocking task launch.
//!
//! Creates a session, sends the prompt, and returns immediately with
//! the session ID. Unlike `wing -p` (blocking, Claude Code compatible),
//! `wing run` does not wait for the task to complete.
//!
//! # Wake-up notification (default on)
//!
//! The spawned session also registers a wake-up back to MINE when it finishes
//! (or asks a question) — inside an agent's shell (`WING_SESSION_ID`) that is
//! the non-blocking replacement for `wing wait`:
//!
//! ```sh
//! SID=$(wing run -p "fix the bug" --json | jq -r .session_id)
//! wing watch "$SID"            # explicit, if you turned the default off
//! # ... do other work; my session is woken when $SID finishes ...
//! wing wait "$SID"             # still the "I insist on blocking" option
//! ```
//!
//! `--no-notify` turns it off, `--notify-to <SID>` redirects it, `--notify-on`
//! / `--notify-prompt` shape the payload. With no target (`WING_SESSION_ID`
//! absent and no `--notify-to`) it is silently off — a root shell has nobody to
//! notify.
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
use wing_api_client::GatewayClient as GatewayApiClient;
use wing_api_client::models::{AgentOverride, CreateSessionRequest, WakeCreateRequest};

use super::args::RunArgs;
use super::common;
use super::wake;

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
    /// 唤醒登记（成功时）；`None` = 没登记（见 `notify_off`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    notify: Option<NotifyOutput>,
    /// 没登记的原因（`off (no parent session)` / `off (--no-notify)` / 失败文案）。
    #[serde(skip_serializing_if = "Option::is_none")]
    notify_off: Option<String>,
}

/// 唤醒登记的回执（`wing run` 的 JSON 与文本输出共用）。
#[derive(Serialize, Clone)]
struct NotifyOutput {
    id: i64,
    target_session_id: String,
    on_events: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

/// notify 的决策（纯逻辑，单测覆盖）。
#[derive(Debug, PartialEq, Eq)]
enum NotifyPlan {
    /// 要登记，目标会话是它。
    Register(String),
    /// 不登记，原因是这一句（单行，进文本输出的 `notify:` 行）。
    Off(String),
}

/// `--no-notify` / 定向回答 / 目标解析 → 计划。目标解析为空的隐式缺省是**静默关闭**
/// （没有父会话可叫，不是错误）。`parent` 是 `WING_SESSION_ID` 的**读回值**（注入以便单测）。
///
/// `answering` = 这次调用带着 `--tool-call-id`，即**定向回答一个待答复的 ask**。这种调用
/// 不登记唤醒：派发时那条触发器仍在武装（ask 从不消耗 done 配额），再登记一次只会让同一个
/// ask / 同一个收口被投递多次——父会话照两条信封各答一次时，第二次答复已经找不到 waiter，
/// 会掉进子会话的 inbox 变成一条普通用户消息（凭空多起一轮）。网关侧对同源订阅做的是
/// 刷新而不是新增，但这里连一次无意义的写都不发，并且**在输出里说明原因**。
fn plan_notify(
    no_notify: bool,
    answering: bool,
    notify_to: Option<&str>,
    parent: Option<&str>,
) -> NotifyPlan {
    if no_notify {
        return NotifyPlan::Off("off (--no-notify)".to_string());
    }
    if answering {
        return NotifyPlan::Off(
            "off (answering an ask: the dispatch-time trigger is still armed)".to_string(),
        );
    }
    if let Some(target) = notify_to.map(str::trim).filter(|value| !value.is_empty()) {
        return NotifyPlan::Register(target.to_string());
    }
    match parent.map(str::trim).filter(|value| !value.is_empty()) {
        Some(target) => NotifyPlan::Register(target.to_string()),
        None => NotifyPlan::Off("off (no parent session)".to_string()),
    }
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
        Err(e) => common::fail("run", e),
    }
}

async fn run_inner(args: RunArgs) -> Result<RunOutput> {
    // 1. Ensure gateway is running.
    let http = common::connect().await?;

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

    // 3. Register the wake-up BEFORE sending the prompt: a fast child could
    //    otherwise finish between the send and the registration and we would
    //    lose the only notification we care about. Failures are warnings —
    //    notify is a soft mechanism (same posture as lineage), and an explicit
    //    `--notify-to` mistake is reported verbatim rather than swallowed.
    let parent = common::parent_session_id_from_env();
    let answering = args.tool_call_id.is_some();
    // `created` 决定 send 失败时能不能撤：只有**本次新建**的那一行才是我们的东西。
    let mut created_by_us = false;
    let (notify, notify_off) = match plan_notify(
        args.no_notify,
        answering,
        args.notify_to.as_deref(),
        parent.as_deref(),
    ) {
        NotifyPlan::Off(reason) => (None, Some(reason)),
        NotifyPlan::Register(target) => {
            match register_notify(&http, &args, &session_id, &target).await {
                Ok((notify, created)) => {
                    created_by_us = created;
                    (Some(notify), None)
                }
                Err(e) => (None, Some(format!("off (registration failed: {e})"))),
            }
        }
    };

    // 4. Send prompt (non-blocking — the HTTP call returns immediately,
    //    the agent processes asynchronously). Always sent, including on
    //    resume: `wing run -r <sid> -p "next task"` resumes + sends.
    //    `--tool-call-id` routes the prompt to a pending Ask's feedback
    //    waiter instead of the session inbox (answering an ask).
    if let Err(e) = http
        .send_message(&session_id, &args.prompt, args.tool_call_id.clone())
        .await
    {
        // The prompt never went out: drop the registration instead of leaving a
        // trigger that would fire on some later completion nobody asked about.
        //
        // Only our own row. A registration that merely *refreshed* an existing
        // subscription (`created == false`) belongs to whoever set it up: cancelling
        // it would destroy their explicit `wing watch --times N -p "..."` — the very
        // state this command promises never to overwrite — and leave the child with
        // nobody to wake.
        if let Some(registered) = notify.as_ref().filter(|_| created_by_us)
            && let Err(cancel_error) = http.cancel_wake(registered.id).await
        {
            tracing::warn!("failed to cancel wake #{}: {cancel_error}", registered.id);
        }
        return Err(anyhow::anyhow!("Failed to send prompt: {e}"));
    }

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
        notify,
        notify_off,
    })
}

/// 注册唤醒登记（失败只回错误，由调用方降级为警告）。
/// 注册唤醒；返回值里的 `bool` 是"本次新建"（`CreateWake.created`）。
async fn register_notify(
    http: &GatewayApiClient,
    args: &RunArgs,
    session_id: &str,
    target: &str,
) -> Result<(NotifyOutput, bool)> {
    let on_events = match args.notify_on.as_deref() {
        Some(raw) => Some(wake::parse_on_events(raw)?),
        None => None,
    };
    let request = WakeCreateRequest {
        kind: "session".into(),
        target_session_id: target.to_string(),
        source_session_id: Some(session_id.to_string()),
        on_events: on_events.clone(),
        spec: None,
        prompt: args
            .notify_prompt
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        times: None,
    };
    let resp = http.create_wake(&request).await?;
    Ok((
        NotifyOutput {
            id: resp.wake.id,
            target_session_id: resp.wake.target_session_id,
            on_events: resp.wake.on_events,
            prompt: resp.wake.prompt,
        },
        resp.created,
    ))
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
    // 每一行都在说"登记了没有、为什么"：`notify` 与 `notify_off` 恒有一侧在场
    // （`plan_notify` 只有这两种结局），所以不需要第三臂。
    if let Some(notify) = &output.notify {
        println!(
            "notify:      #{} → {} (on {})",
            notify.id, notify.target_session_id, notify.on_events
        );
    } else if let Some(reason) = &output.notify_off {
        println!("notify:      {reason}");
    }
    println!("started_at:  {}", output.started_at);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn notify_defaults_to_the_parent_session() {
        assert_eq!(
            plan_notify(false, false, None, Some("parent-1")),
            NotifyPlan::Register("parent-1".into())
        );
    }

    #[test]
    fn notify_to_wins_over_the_parent() {
        assert_eq!(
            plan_notify(false, false, Some("  other  "), Some("parent-1")),
            NotifyPlan::Register("other".into())
        );
    }

    #[test]
    fn no_target_means_silently_off() {
        assert_eq!(
            plan_notify(false, false, None, None),
            NotifyPlan::Off("off (no parent session)".into())
        );
        // 空白值等同于没给
        assert_eq!(
            plan_notify(false, false, Some("   "), Some("  ")),
            NotifyPlan::Off("off (no parent session)".into())
        );
    }

    #[test]
    fn no_notify_wins_over_every_target() {
        assert_eq!(
            plan_notify(true, false, Some("other"), Some("parent-1")),
            NotifyPlan::Off("off (--no-notify)".into())
        );
    }

    #[test]
    fn answering_an_ask_never_registers_another_trigger() {
        // 定向回答（`-r <child> --tool-call-id <id>`）：派发时那条触发器还在武装，
        // 再登记一次会让同一个 ask / 同一个 done 被投递多次。
        let plan = plan_notify(false, true, None, Some("parent-1"));
        assert_eq!(
            plan,
            NotifyPlan::Off(
                "off (answering an ask: the dispatch-time trigger is still armed)".into()
            )
        );
        // 显式 --notify-to 也不改变这一点（输出里会说清为什么没登记）
        assert!(matches!(
            plan_notify(false, true, Some("other"), Some("parent-1")),
            NotifyPlan::Off(_)
        ));
    }

    #[test]
    fn resuming_without_answering_still_registers() {
        // `-r <sid> -p "next task"`（不是回答 ask）：网关对同源订阅做刷新，
        // 所以重复登记是幂等的，`run` 照旧登记。
        assert_eq!(
            plan_notify(false, false, None, Some("parent-1")),
            NotifyPlan::Register("parent-1".into())
        );
    }
}
