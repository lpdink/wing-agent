//! `wing interrupt` / `wing compact` / `wing update` — session control.
//!
//! The three mutating operations a session has beyond plain message sending:
//!
//! - `wing interrupt <sid>` (alias `int`) — stop the running turn (the TUI's
//!   Esc). Interrupting is only meaningful for a session the gateway has
//!   **loaded** (a working session always is): a 404 is reported as such
//!   instead of being papered over with a hydration that would interrupt
//!   nothing.
//! - `wing compact <sid> [instruction]` — manual context compaction, with an
//!   optional focus instruction. Hydrates on 404.
//! - `wing update <sid> [flags]` — partial state update; **only the flags you
//!   pass are sent** (the endpoint applies a key only when it is present).
//!   One gateway-side exception to read before scripting: `--agent` switches
//!   the template, and the new template's `thinking` / `reasoning_effort` /
//!   `yolo` values come with it (the gateway rebuilds the agent) — the command
//!   says so in its output rather than pretending those fields were untouched.
//!   Hydrates on 404.
//!
//! ```sh
//! wing interrupt "$SID"
//! wing compact "$SID" "keep the architecture decisions"
//! wing update "$SID" --model ds-flash --title "nightly build" --yolo
//! wing update "$SID" --thinking off
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;
use wing_api_client::models::UpdateSessionRequest;

use super::common;

// ============================================================
// wing interrupt
// ============================================================

/// Output of `wing interrupt`.
#[derive(Serialize)]
struct InterruptOutput {
    ok: bool,
    session_id: String,
}

/// Entry point for `wing interrupt`.
pub async fn run_interrupt(session_id: &str, json: bool) -> ExitCode {
    match interrupt_inner(session_id).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                println!("interrupted session {}", output.session_id);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing interrupt error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn interrupt_inner(session_id: &str) -> Result<InterruptOutput> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    // 刻意不做 404→resume：中断的语义对象是"在飞的任务"，而在飞的任务只存在于
    // 已加载的会话里——把已逐出的会话重新水合只为中断一个不存在的过程，是假动作。
    http.interrupt_session(session_id).await.map_err(|e| {
        if e.is_not_found() {
            anyhow::anyhow!(
                "session {session_id} is not loaded in the gateway — nothing is running to \
                 interrupt (`wing ps --all` lists every known session)"
            )
        } else {
            common::session_error(session_id, &e)
        }
    })?;
    Ok(InterruptOutput {
        ok: true,
        session_id: session_id.to_string(),
    })
}

// ============================================================
// wing compact
// ============================================================

/// Output of `wing compact`.
#[derive(Serialize)]
struct CompactOutput {
    ok: bool,
    session_id: String,
    original_tokens: i64,
    compressed_tokens: i64,
}

/// Entry point for `wing compact`.
pub async fn run_compact(session_id: &str, instruction: Option<&str>, json: bool) -> ExitCode {
    match compact_inner(session_id, instruction).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_compact(&output);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing compact error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn compact_inner(session_id: &str, instruction: Option<&str>) -> Result<CompactOutput> {
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    let resp = common::hydrate_on_404(&http, session_id, || {
        http.compact_session(session_id, instruction)
    })
    .await?;
    Ok(CompactOutput {
        ok: resp.ok,
        session_id: session_id.to_string(),
        original_tokens: resp.original_tokens,
        compressed_tokens: resp.compressed_tokens,
    })
}

fn print_compact(output: &CompactOutput) {
    println!("session_id:        {}", output.session_id);
    println!("original_tokens:   {}", output.original_tokens);
    println!("compressed_tokens: {}", output.compressed_tokens);
}

// ============================================================
// wing update
// ============================================================

/// The value of a state-update flag that toggles a boolean.
///
/// `on` / `off` spell the direction; `true` / `false` are accepted aliases.
/// A bare flag (`--yolo`) means `on`, so the common case needs no value while
/// the opposite direction stays one token away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    #[value(alias = "true")]
    On,
    #[value(alias = "false")]
    Off,
}

impl OnOff {
    fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

/// `wing update` arguments — every field optional; **only what is passed is
/// sent** (the endpoint treats an absent key as "leave it alone").
#[derive(Args, Debug, Clone)]
pub struct UpdateArgs {
    /// Session ID.
    pub session_id: String,

    /// Switch model (references `providers[].models` id in the gateway config).
    #[arg(long = "model")]
    pub model: Option<String>,

    /// Switch agent template.
    #[arg(long)]
    pub agent: Option<String>,

    /// Set the session title.
    #[arg(long)]
    pub title: Option<String>,

    /// Thinking mode (on|off; bare `--thinking` = on).
    #[arg(long, value_name = "on|off", num_args = 0..=1, default_missing_value = "on")]
    pub thinking: Option<OnOff>,

    /// Reasoning effort: low|medium|high|xhigh|max.
    #[arg(long)]
    pub effort: Option<String>,

    /// YOLO mode — auto-approve tool calls (on|off; bare `--yolo` = on).
    #[arg(long, value_name = "on|off", num_args = 0..=1, default_missing_value = "on")]
    pub yolo: Option<OnOff>,

    /// Switch the workspace (working directory) used by new tool calls.
    #[arg(long)]
    pub workspace: Option<String>,

    /// Replace the **whole** tool set (comma-separated refs: `namespace.name`
    /// or bare name; an empty value = no tools).
    #[arg(long)]
    pub tools: Option<String>,
}

/// One field the update actually sent, as `(wire field, value)`.
#[derive(Debug, Serialize)]
struct AppliedField {
    field: &'static str,
    value: String,
}

/// Output of `wing update`.
#[derive(Serialize)]
struct UpdateOutput {
    ok: bool,
    session_id: String,
    applied: Vec<AppliedField>,
    /// Gateway semantics the receipt cannot express as a field change
    /// (currently: `--agent` re-applies the new template's thinking / effort /
    /// yolo values). Empty on a plain field update.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

/// The one update flag whose effect exceeds its own field: switching the
/// template rebuilds the agent, so the new template's values apply to the
/// fields the caller did **not** name explicitly (the gateway applies the
/// template first and the given fields after it, so explicit flags win). The
/// note lists only what actually follows the template — naming a field the
/// caller set explicitly would contradict the `applied` list right above it.
fn agent_switch_note(args: &UpdateArgs) -> Option<String> {
    args.agent.as_ref()?;
    let mut from_template: Vec<&str> = Vec::new();
    if args.thinking.is_none() {
        from_template.push("thinking");
    }
    if args.effort.is_none() {
        from_template.push("reasoning_effort");
    }
    if args.yolo.is_none() {
        from_template.push("yolo");
    }
    if from_template.is_empty() {
        return None;
    }
    let listed = from_template.join(" / ");
    Some(if from_template.len() == 3 {
        format!("--agent switches the template: {listed} follow the new template")
    } else {
        format!(
            "--agent switches the template: {listed} follow the new template              (the fields given explicitly were applied after it)"
        )
    })
}

/// Entry point for `wing update`.
pub async fn run_update(args: UpdateArgs, json: bool) -> ExitCode {
    match update_inner(&args).await {
        Ok(output) => {
            if json {
                common::print_json_compact(&output);
            } else {
                print_update(&output);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("wing update error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn update_inner(args: &UpdateArgs) -> Result<UpdateOutput> {
    let (request, applied) = build_update(args)?;

    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;
    common::hydrate_on_404(&http, &args.session_id, || http.update_session(&request)).await?;

    let notes: Vec<String> = agent_switch_note(args).into_iter().collect();
    Ok(UpdateOutput {
        ok: true,
        session_id: args.session_id.clone(),
        applied,
        notes,
    })
}

/// Fold the flags into a request body + the list of fields actually sent.
///
/// **Partial update by construction**: a flag that was not passed is `None`
/// and the corresponding key is omitted from the body (`skip_serializing_if`),
/// so the gateway never sees — and never resets — a field the caller did not
/// name.
fn build_update(args: &UpdateArgs) -> Result<(UpdateSessionRequest, Vec<AppliedField>)> {
    let mut applied: Vec<AppliedField> = Vec::new();

    if let Some(model) = &args.model {
        applied.push(AppliedField {
            field: "model_id",
            value: model.clone(),
        });
    }
    if let Some(agent) = &args.agent {
        applied.push(AppliedField {
            field: "agent",
            value: agent.clone(),
        });
    }
    if let Some(title) = &args.title {
        applied.push(AppliedField {
            field: "title",
            value: title.clone(),
        });
    }
    if let Some(thinking) = args.thinking {
        applied.push(AppliedField {
            field: "thinking",
            value: thinking.is_on().to_string(),
        });
    }
    if let Some(effort) = &args.effort {
        applied.push(AppliedField {
            field: "reasoning_effort",
            value: effort.clone(),
        });
    }
    if let Some(yolo) = args.yolo {
        applied.push(AppliedField {
            field: "yolo",
            value: yolo.is_on().to_string(),
        });
    }
    if let Some(workspace) = &args.workspace {
        applied.push(AppliedField {
            field: "workspace",
            value: workspace.clone(),
        });
    }
    // 全量替换语义：显式给出的空值 = 空工具集（不是"不动"——不动是不传该旗标）。
    let tools = args.tools.as_ref().map(|raw| {
        raw.split(',')
            .map(|tool| tool.trim().to_string())
            .filter(|tool| !tool.is_empty())
            .collect::<Vec<String>>()
    });
    if let Some(tools) = &tools {
        applied.push(AppliedField {
            field: "tools",
            value: tools.join(","),
        });
    }

    if applied.is_empty() {
        anyhow::bail!(
            "no update fields given; pass at least one of \
             --model/--agent/--title/--thinking/--effort/--yolo/--workspace/--tools"
        );
    }

    Ok((
        UpdateSessionRequest {
            session_id: args.session_id.clone(),
            model_id: args.model.clone(),
            agent: args.agent.clone(),
            title: args.title.clone(),
            thinking: args.thinking.map(OnOff::is_on),
            reasoning_effort: args.effort.clone(),
            yolo: args.yolo.map(OnOff::is_on),
            workspace: args.workspace.clone(),
            tools,
        },
        applied,
    ))
}

fn print_update(output: &UpdateOutput) {
    println!("session_id:  {}", output.session_id);
    for field in &output.applied {
        println!("{:<13} {}", format!("{}:", field.field), field.value);
    }
    for note in &output.notes {
        println!("note:        {note}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(session_id: &str) -> UpdateArgs {
        UpdateArgs {
            session_id: session_id.into(),
            model: None,
            agent: None,
            title: None,
            thinking: None,
            effort: None,
            yolo: None,
            workspace: None,
            tools: None,
        }
    }

    #[test]
    fn build_update_sends_only_the_fields_given() {
        let mut given = args("s1");
        given.model = Some("ds-flash".into());
        given.title = Some("nightly".into());
        let (request, applied) = build_update(&given).unwrap();
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"session_id": "s1", "model_id": "ds-flash", "title": "nightly"}),
            "未给字段一个都不能出现在请求体里"
        );
        let fields: Vec<&str> = applied.iter().map(|f| f.field).collect();
        assert_eq!(fields, ["model_id", "title"]);
    }

    #[test]
    fn build_update_maps_on_off_flags() {
        let mut given = args("s1");
        given.thinking = Some(OnOff::Off);
        given.yolo = Some(OnOff::On);
        given.effort = Some("high".into());
        let (request, applied) = build_update(&given).unwrap();
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "session_id": "s1",
                "thinking": false,
                "reasoning_effort": "high",
                "yolo": true,
            })
        );
        assert_eq!(applied[0].value, "false");
        assert_eq!(applied[2].value, "true");
    }

    #[test]
    fn build_update_replaces_tools_and_trims() {
        let mut given = args("s1");
        given.tools = Some(" core.Bash , Read ".into());
        let (request, _) = build_update(&given).unwrap();
        assert_eq!(
            serde_json::to_value(&request).unwrap()["tools"],
            serde_json::json!(["core.Bash", "Read"])
        );

        // 显式空值 = 空工具集（全量替换），不是"不动"。
        given.tools = Some("  ".into());
        let (request, _) = build_update(&given).unwrap();
        assert_eq!(
            serde_json::to_value(&request).unwrap()["tools"],
            serde_json::json!([])
        );
    }

    /// `--agent` 的效果超出它自己的字段：模板重建会带上调用方**没有显式给出**
    /// 的 thinking / effort / yolo —— 回执必须说出来，且只点名这些字段：把显式
    /// 给出（因此覆盖模板值）的字段也列进去，就和上方的 applied 自相矛盾。
    #[test]
    fn agent_switch_note_names_only_the_fields_that_follow_the_template() {
        let mut given = args("s1");
        given.agent = Some("executor".into());
        let (_, applied) = build_update(&given).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].field, "agent");

        let note = agent_switch_note(&given).expect("note");
        for field in ["thinking", "reasoning_effort", "yolo"] {
            assert!(note.contains(field), "{note}");
        }

        // 显式给出的字段覆盖模板值：note 不再点名它们（否则与 applied 矛盾）。
        let mut partial = args("s1");
        partial.agent = Some("executor".into());
        partial.thinking = Some(OnOff::Off);
        let note = agent_switch_note(&partial).expect("note");
        assert!(!note.contains("thinking /"), "{note}");
        assert!(note.contains("reasoning_effort"), "{note}");
        assert!(note.contains("yolo"), "{note}");
        assert!(note.contains("given explicitly"), "{note}");

        // 三个字段全部显式给出 ⇒ 没有"跟随模板"的字段 ⇒ 不产生 note。
        let mut full = args("s1");
        full.agent = Some("executor".into());
        full.thinking = Some(OnOff::On);
        full.effort = Some("high".into());
        full.yolo = Some(OnOff::Off);
        assert!(agent_switch_note(&full).is_none());

        // 不给 --agent 永远没有 note。
        assert!(agent_switch_note(&args("s1")).is_none());

        // 输出面：notes 为空时 serde 省掉该键。
        let plain = UpdateOutput {
            ok: true,
            session_id: "s1".into(),
            applied: vec![AppliedField {
                field: "title",
                value: "t".into(),
            }],
            notes: Vec::new(),
        };
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("notes").is_none(), "{json}");
    }

    #[test]
    fn build_update_requires_at_least_one_field() {
        let error = build_update(&args("s1")).unwrap_err().to_string();
        assert!(error.contains("no update fields given"), "{error}");
        assert!(error.contains("--model"), "{error}");
    }

    #[test]
    fn on_off_values_and_aliases() {
        assert!(OnOff::On.is_on() && !OnOff::Off.is_on());
        // clap 的取值 + 别名（true / false 是 on / off 的别名）。
        for (raw, want) in [("on", OnOff::On), ("off", OnOff::Off)] {
            let parsed = OnOff::from_str(raw, true).expect("canonical value");
            assert_eq!(parsed, want, "{raw}");
        }
        assert_eq!(OnOff::from_str("true", true).expect("alias"), OnOff::On);
        assert_eq!(OnOff::from_str("false", true).expect("alias"), OnOff::Off);
        assert!(OnOff::from_str("maybe", true).is_err());
    }
}
