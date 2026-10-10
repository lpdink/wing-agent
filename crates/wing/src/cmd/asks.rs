//! `wing asks` — the pending Ask question(s) of a session, with the
//! `tool_call_id` needed to answer them.
//!
//! `POST /api/session/send` resolves a pending ask only when the message
//! carries that ask's `tool_call_id` (`--tool-call-id` on `wing run`), so a
//! caller has to be able to find the id **while the ask is still pending**.
//! Neither `wing tail` nor `wing head` can: a pending ask's tool call belongs
//! to the *uncommitted* assistant message (the provider stream accumulator is
//! the authority mid-turn), so it is not on the chain yet. `wing ps` / `info`
//! do not carry it either.
//!
//! What does carry it is the live event stream: subscribing triggers a
//! `sync_session` snapshot whose `events` include the still-active ask (that
//! is exactly how the TUI rebuilds its ask panel after a reconnect), and a
//! fresh ask arrives as a live `ask` event. This command subscribes, reads
//! that snapshot, and prints the pending asks — one shot, no session content
//! touched (like the other read commands, subscribing hydrates an evicted
//! session back into gateway memory).
//!
//! ```sh
//! wing asks "$SID"                 # the snapshot: pending asks, or none
//! wing asks "$SID" --wait 60       # block until an ask appears (or fail)
//! ASK=$(wing asks "$SID" --json | jq -r '.asks[0].tool_call_id')
//! wing run -r "$SID" -p "yes" --tool-call-id "$ASK"
//! ```
//!
//! Exit codes: 0 = the report was produced (with or without pending asks,
//! mirroring `wing ps`); with `--wait N`, a window that closes without any ask
//! is a failure (non-zero) — a caller that asked to block until an ask wants
//! to hear that none came.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use crate::gateway::GatewayClient;
use crate::protocol::AskQuestion;
use crate::protocol::SessionStatus;
use crate::protocol::WingEvent;

use super::common;

/// How long the (local) gateway snapshot may take before we give up on it.
/// Subscribe → replay is loopback-fast; this only protects against a health
/// endpoint that lies (something answering `/api/health` without speaking the
/// protocol).
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);

/// One still-pending ask, in the shape the `ask` event carries it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PendingAsk {
    /// Echo back via `wing run --tool-call-id` to answer this ask.
    pub tool_call_id: String,
    /// Multi-question format (`AskUserQuestion`): one entry per question.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<AskQuestion>,
    /// Legacy single-question format (dangerous-command confirmation).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

/// Output of `wing asks`.
#[derive(Serialize)]
struct AsksOutput {
    session_id: String,
    /// Session status when the ask(s) were found — `waiting` whenever an ask
    /// is pending (an outstanding ask is exactly what that status means).
    status: String,
    asks: Vec<PendingAsk>,
}

/// Entry point for `wing asks`.
pub async fn run_asks(session_id: &str, wait_secs: u64, json: bool) -> ExitCode {
    match asks_inner(session_id, wait_secs).await {
        Ok((output, waited_out)) => {
            if json {
                common::print_json_compact(&output);
            } else if output.asks.is_empty() {
                println!("No pending asks (status: {}).", output.status);
            } else {
                print!("{}", format_asks(&output));
            }
            if waited_out && output.asks.is_empty() {
                eprintln!(
                    "wing asks error: no ask appeared within {wait_secs}s \
                     (session status: {})",
                    output.status
                );
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("wing asks error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn asks_inner(session_id: &str, wait_secs: u64) -> Result<(AsksOutput, bool)> {
    let (host, port) = common::ensure_gateway().await?;
    let ws_url = format!("ws://{host}:{port}/ws");
    let api_key = common::load_api_key();
    let mut gateway = GatewayClient::connect(&ws_url, api_key.as_deref())
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to the gateway: {e}"))?;
    let client_id = gateway.client_id().to_string();

    let http = common::create_api_client(&host, port)?;
    // Subscribing is what produces the snapshot (and hydrates an evicted
    // session, like `wing info`); a session that does not exist 404s here.
    http.subscribe(session_id, &client_id)
        .await
        .map_err(|e| common::session_error(session_id, &e))?;

    // The snapshot: the first `sync_session` for this session.
    let mut status = String::new();
    let mut asks: Vec<PendingAsk> = Vec::new();
    let mut snapshot_seen = false;
    while let Some(event) = next_event(&mut gateway, SNAPSHOT_TIMEOUT).await? {
        if event.session_id() != Some(session_id) {
            continue;
        }
        match event {
            WingEvent::SyncSession {
                events,
                status: snapshot_status,
                ..
            } => {
                status = status_str(snapshot_status);
                asks = collect_asks(events)?;
                snapshot_seen = true;
                break;
            }
            // A live ask can outrace the snapshot (subscribe lands while the
            // turn is mid-ask). Same report either way — and the status is
            // `waiting` by definition: an ask is outstanding.
            _ if pending_ask(&event).is_some() => {
                status = status_str(SessionStatus::Waiting);
                asks.extend(pending_ask(&event));
                snapshot_seen = true;
                break;
            }
            _ => {}
        }
    }
    if !snapshot_seen {
        anyhow::bail!(
            "the gateway sent no session snapshot within {}s (something other \
             than wing-gateway may be answering /api/health)",
            SNAPSHOT_TIMEOUT.as_secs()
        );
    }

    // `--wait`: no ask in the snapshot — keep listening for a live one.
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    let mut waited_out = false;
    while asks.is_empty() && wait_secs > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            waited_out = true;
            break;
        }
        let Some(event) = next_event(&mut gateway, remaining).await? else {
            waited_out = true;
            break;
        };
        if event.session_id() != Some(session_id) {
            continue;
        }
        if let Some(ask) = pending_ask(&event) {
            // An outstanding ask *is* why the session is `waiting`, whatever
            // the snapshot said a moment ago (same rule as the branch above).
            status = status_str(SessionStatus::Waiting);
            asks.push(ask);
            break;
        }
        // The turn ending before any ask means no ask is coming.
        if matches!(event, WingEvent::TurnResult { .. }) {
            waited_out = true;
            break;
        }
    }

    Ok((
        AsksOutput {
            session_id: session_id.to_string(),
            status,
            asks,
        },
        waited_out,
    ))
}

/// Receive one event within `timeout`; `None` = the window closed first.
///
/// A closed stream is an error, not a timeout: the caller must be able to say
/// why the report failed (same discipline as `wing wait`).
async fn next_event(gateway: &mut GatewayClient, timeout: Duration) -> Result<Option<WingEvent>> {
    match tokio::time::timeout(timeout, gateway.recv_event()).await {
        Ok(Some(event)) => Ok(Some(event)),
        Ok(None) => {
            let reason = gateway
                .close_reason()
                .map(|reason| reason.describe())
                .unwrap_or_else(|| "event stream closed".to_string());
            anyhow::bail!("gateway event stream closed before the ask arrived ({reason})")
        }
        Err(_) => Ok(None),
    }
}

/// Turn the snapshot's fact events into pending asks (in chain order).
///
/// A snapshot entry that decodes but is not an `ask` is skipped — including an
/// unknown event **type** (`WingEvent`'s catch-all), which is forward
/// compatibility rather than a problem.
///
/// A **known** type whose payload does not decode is the opposite: it means
/// the gateway and this binary disagree about a type they both know, and the
/// entry it failed to decode could be exactly the pending ask this command
/// exists to report. Silently dropping it would answer "no pending asks" —
/// with exit 0 — for a session that is blocked on one, so the drift fails the
/// command instead.
fn collect_asks(events: Vec<serde_json::Value>) -> Result<Vec<PendingAsk>> {
    let mut asks = Vec::new();
    for value in events {
        match serde_json::from_value::<WingEvent>(value.clone()) {
            Ok(event) => asks.extend(pending_ask(&event)),
            Err(e) => {
                let event_type = value
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("<no type>");
                anyhow::bail!(
                    "the gateway sent a `{event_type}` event this build cannot decode: {e} \
                     (the CLI and the gateway ship as one version — is the running gateway \
                     older or newer than this `wing`?)"
                );
            }
        }
    }
    Ok(asks)
}

fn pending_ask(event: &WingEvent) -> Option<PendingAsk> {
    match event {
        WingEvent::Ask {
            tool_call_id,
            questions,
            question,
            choices,
            required,
            ..
        } => Some(PendingAsk {
            tool_call_id: tool_call_id.clone(),
            questions: questions.clone(),
            question: question.clone(),
            choices: choices.clone(),
            required: *required,
        }),
        _ => None,
    }
}

/// The wire spelling of a status (`"waiting"`, …) — one definition, the enum's.
fn status_str(status: SessionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Human report: one block per ask, its questions indented.
fn format_asks(output: &AsksOutput) -> String {
    let mut text = String::new();
    text.push_str(&format!("session_id:   {}\n", output.session_id));
    text.push_str(&format!("status:       {}\n", output.status));
    for ask in &output.asks {
        text.push_str(&format!("tool_call_id: {}\n", ask.tool_call_id));
        for question in &ask.questions {
            text.push_str(&format!(
                "  [{}] {}\n",
                question.tab_label(),
                common::single_line(&question.question)
            ));
            for option in &question.options {
                if option.description.trim().is_empty() {
                    text.push_str(&format!("    {}\n", option.label));
                } else {
                    text.push_str(&format!(
                        "    {} — {}\n",
                        option.label,
                        common::single_line(&option.description)
                    ));
                }
            }
        }
        // Legacy single-question format.
        if !ask.question.is_empty() {
            text.push_str(&format!("  {}\n", common::single_line(&ask.question)));
        }
        for choice in &ask.choices {
            text.push_str(&format!("    {choice}\n"));
        }
        if ask.required {
            text.push_str("    (required — pick one of the choices)\n");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The real wire shape of the replay material: the ask event as the
    /// gateway dumps it into `sync_session.events` (multi-question format).
    fn ask_event_value() -> serde_json::Value {
        json!({
            "type": "ask",
            "session_id": "s1",
            "created_at": "2026-10-10T22:06:26.935654",
            "request_id": "req-1",
            "tool_call_id": "call_fake_ask_1",
            "questions": [
                {
                    "id": "q1",
                    "header": "verify",
                    "question": "Proceed with the manual verification?",
                    "multiSelect": false,
                    "options": [
                        {"label": "yes", "description": "go ahead"},
                        {"label": "no", "description": ""}
                    ]
                }
            ]
        })
    }

    #[test]
    fn collect_asks_picks_ask_events_and_skips_the_rest() {
        let events = vec![
            // 已知类型、非 ask：解码通过，只是不产出。
            json!({
                "type": "turn_started",
                "session_id": "s1",
                "created_at": "2026-10-10T22:06:26.935654",
                "request_id": "req-1",
            }),
            ask_event_value(),
            // 未知类型（前向兼容）：`WingEvent::Unknown`，跳过。
            json!({"type": "not_a_known_event", "session_id": "s1"}),
        ];
        let asks = collect_asks(events).expect("known types must decode");
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].tool_call_id, "call_fake_ask_1");
        assert_eq!(asks[0].questions[0].id, "q1");
        assert_eq!(asks[0].questions[0].tab_label(), "verify");
        assert_eq!(asks[0].questions[0].options[0].label, "yes");
        assert!(!asks[0].questions[0].multi_select);
    }

    #[test]
    fn collect_asks_reads_the_legacy_single_question_shape() {
        let events = vec![json!({
            "type": "ask",
            "session_id": "s1",
            "created_at": "2026-10-10T22:06:26.935654",
            "request_id": "req-1",
            "tool_call_id": "call-bash",
            "question": "Run `rm -rf /`?",
            "choices": ["yes", "no"],
            "required": true,
        })];
        let asks = collect_asks(events).expect("decodes");
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].question, "Run `rm -rf /`?");
        assert_eq!(asks[0].choices, ["yes", "no"]);
        assert!(asks[0].required);
        assert!(asks[0].questions.is_empty());
    }

    /// 已知类型但字段漂移 ⇒ 响亮失败，绝不静默跳过：跳过一个有 ask 的条目
    /// 就是对在挂的 ask 报「没有」并且 exit 0（应答闭环会因此空转）。
    #[test]
    fn collect_asks_fails_loudly_on_a_drifted_known_event() {
        // ask 缺 tool_call_id（必填字段漂移）。
        let drifted_ask = json!({
            "type": "ask",
            "session_id": "s1",
            "created_at": "2026-10-10T22:06:26.935654",
            "request_id": "req-1",
            "questions": [{"id": "q1", "question": "Proceed?", "options": 7}],
        });
        let error = collect_asks(vec![drifted_ask]).unwrap_err().to_string();
        assert!(error.contains("`ask`"), "{error}");
        assert!(error.contains("cannot decode"), "{error}");

        // 未知类型仍然跳过（前向兼容），不误伤。
        let unknown_type = json!({"type": "brand_new_event", "session_id": "s1"});
        assert!(collect_asks(vec![unknown_type]).unwrap().is_empty());
    }

    /// `--json` 是机器面：`tool_call_id` 恒在场，空 `questions` 不出现。
    #[test]
    fn json_keeps_the_tool_call_id_and_drops_empty_fields() {
        let output = AsksOutput {
            session_id: "s1".into(),
            status: "waiting".into(),
            asks: vec![PendingAsk {
                tool_call_id: "c1".into(),
                questions: Vec::new(),
                question: String::new(),
                choices: Vec::new(),
                required: false,
            }],
        };
        assert_eq!(
            serde_json::to_value(&output).unwrap(),
            json!({
                "session_id": "s1",
                "status": "waiting",
                "asks": [{"tool_call_id": "c1"}],
            })
        );
    }

    #[test]
    fn format_asks_marks_the_legacy_required_form() {
        let asks = collect_asks(vec![json!({
            "type": "ask",
            "session_id": "s1",
            "created_at": "2026-10-10T22:06:26.935654",
            "request_id": "req-1",
            "tool_call_id": "call-bash",
            "question": "Run it?",
            "choices": ["yes", "no"],
            "required": true,
        })])
        .expect("decodes");
        let text = format_asks(&AsksOutput {
            session_id: "s1".into(),
            status: "waiting".into(),
            asks,
        });
        assert!(text.contains("Run it?"), "{text}");
        assert!(text.contains("required"), "{text}");
    }

    #[test]
    fn format_asks_renders_questions_and_options() {
        let asks = collect_asks(vec![ask_event_value()]).expect("decodes");
        let text = format_asks(&AsksOutput {
            session_id: "s1".into(),
            status: "waiting".into(),
            asks,
        });
        assert!(text.contains("tool_call_id: call_fake_ask_1"), "{text}");
        assert!(
            text.contains("[verify] Proceed with the manual verification?"),
            "{text}"
        );
        assert!(text.contains("yes — go ahead"), "{text}");
        assert!(text.contains("    no\n"), "无描述选项不补破折号：{text}");
    }

    #[test]
    fn status_str_uses_the_wire_spelling() {
        assert_eq!(status_str(SessionStatus::Waiting), "waiting");
        assert_eq!(status_str(SessionStatus::Working), "working");
        assert_eq!(status_str(SessionStatus::Idle), "idle");
    }
}
