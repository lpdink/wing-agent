//! `wing watch` — 订阅一个会话的收口 / 提问，把结果送回我的会话。
//!
//! `wing run` 派出去的子会话做完就没人知道了：要么 `wing wait` 阻塞，要么人工轮询。
//! `wing watch` 是第三种姿势——**登记一次，然后干别的**：子会话收口（任意 subtype）或
//! 提问时，我的会话收到一条普通用户消息（内置信封：subtype / 轮数 / 耗时 / 最后回复，
//! 或提问内容 + 回答用的 `tool_call_id`）。
//!
//! ```sh
//! wing watch "$CHILD"                 # 它下次收口时叫我（默认只叫一次）
//! wing watch "$CHILD" --on ask        # 只在它提问时叫我（不消耗 done 配额）
//! wing watch "$CHILD" --times 3       # 盯三次收口
//! wing watch "$CHILD" -p "看下结果"   # 用我的话代替内置信封
//! wing watch --list [--all]
//! wing watch --cancel 7
//! ```
//!
//! # 语义（与网关同一套，见 `docs/dev/wake.md`）
//!
//! - 目标（我被叫醒的会话）缺省 = `WING_SESSION_ID`；不在 agent 的 shell 里就必须 `--to <sid>`；
//! - `done` 信封送达即消耗一次 done 配额，配额归零 → 整条触发器终结（缺省配额 1）；
//! - `ask` 唤醒**不**消耗配额（"一直帮我盯着它提问"）；
//! - 目标或源会话进归档区 → 触发器自动取消；
//! - 不做防环：source == target 会被网关 400 拦下。

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Args;
use wing_api_client::models::WakeCreateRequest;

use super::common;
use super::wake::{WakeCancelOutput, WakeOutput, format_wakes, parse_on_events, resolve_target};

/// `wing watch` 的参数。
#[derive(Args, Debug, Clone)]
pub struct WatchArgs {
    /// Session to watch (the source: the one whose completion / question wakes you).
    pub session_id: Option<String>,

    /// Where to deliver the wake-up (default: `WING_SESSION_ID`).
    #[arg(long, value_name = "SID")]
    pub to: Option<String>,

    /// Which events wake you: `done`, `ask`, or both (default: `done,ask`).
    #[arg(long, value_name = "EVENTS")]
    pub on: Option<String>,

    /// How many completions to deliver (default: 1; asks do not consume it).
    #[arg(long, value_name = "N")]
    pub times: Option<i64>,

    /// Send this text verbatim instead of the built-in envelope.
    #[arg(short = 'p', long = "prompt", value_name = "TEXT")]
    pub prompt: Option<String>,

    /// List wake triggers instead of creating one.
    #[arg(long)]
    pub list: bool,

    /// With --list: include finished (done / canceled / failed) triggers.
    #[arg(long)]
    pub all: bool,

    /// Cancel the trigger with this id.
    #[arg(long, value_name = "ID")]
    pub cancel: Option<i64>,
}

impl WatchArgs {
    /// The `POST /api/wake/create` body for this invocation (target resolved).
    pub(crate) fn request(&self, target: &str) -> Result<WakeCreateRequest> {
        let source = self
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("no session to watch: wing watch <SID>"))?;
        if let Some(times) = self.times
            && times < 1
        {
            bail!("--times must be >= 1 (got {times})");
        }
        let on_events = match self.on.as_deref() {
            Some(raw) => Some(parse_on_events(raw)?),
            None => None,
        };
        Ok(WakeCreateRequest {
            kind: "session".into(),
            target_session_id: target.to_string(),
            source_session_id: Some(source.to_string()),
            on_events,
            spec: None,
            prompt: self
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            times: self.times,
        })
    }
}

/// Entry point for `wing watch`.
pub async fn run(args: WatchArgs, json: bool) -> ExitCode {
    match run_inner(args, json).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wing watch error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_inner(args: WatchArgs, json: bool) -> Result<()> {
    // 用法先判：纯参数错误不该先把网关拉起来。
    let mode = validate_mode(&args)?;
    let (host, port) = common::ensure_gateway().await?;
    let http = common::create_api_client(&host, port)?;

    match mode {
        WatchMode::Cancel(id) => {
            let resp = http.cancel_wake(id).await?;
            if json {
                common::print_json_compact(&WakeCancelOutput {
                    id,
                    canceled: resp.canceled,
                });
            } else if resp.canceled {
                println!("canceled {id}");
            } else {
                println!("{id} was already finished (nothing to cancel)");
            }
        }
        WatchMode::List => {
            let resp = http.list_wakes(args.all).await?;
            if json {
                common::print_json_compact(&resp.wakes);
            } else {
                let palette = crate::config::ThemePalette::from_config(
                    &crate::config::AppConfig::load().colors,
                );
                let out = common::TableOutput::detect();
                print!("{}", format_wakes(&resp.wakes, args.all, &palette, &out));
            }
        }
        WatchMode::Create => {
            let target = resolve_target(args.to.as_deref())?;
            let request = args.request(&target)?;
            let resp = http.create_wake(&request).await?;
            if json {
                common::print_json_compact(&WakeOutput { wake: resp.wake });
            } else {
                print!("{}", super::wake::format_wake_detail(&resp.wake));
            }
        }
    }
    Ok(())
}

/// 本次调用是哪一种形态（三种形态的参数集合互斥）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WatchMode {
    Create,
    List,
    Cancel(i64),
}

/// 判定形态并拒绝**放错位置的**参数（静默吞掉参数 = 把这次调用变成另一次调用）。
pub(crate) fn validate_mode(args: &WatchArgs) -> Result<WatchMode> {
    let misplaced =
        args.to.is_some() || args.on.is_some() || args.times.is_some() || args.prompt.is_some();
    if let Some(id) = args.cancel {
        if args.list {
            bail!("--cancel cannot be combined with --list");
        }
        if args.session_id.is_some() {
            bail!("--cancel does not take a SESSION_ID (only the trigger id: --cancel <ID>)");
        }
        if misplaced || args.all {
            bail!("--cancel only takes --json");
        }
        return Ok(WatchMode::Cancel(id));
    }
    if args.list {
        if args.session_id.is_some() {
            bail!(
                "--list does not take a SESSION_ID (list every trigger with --list; \
                 to watch one session: wing watch <SID>)"
            );
        }
        if misplaced {
            bail!(
                "--list cannot be combined with --to / --on / --times / --prompt \
                 (to create one: wing watch <SID>)"
            );
        }
        return Ok(WatchMode::List);
    }
    if args.all {
        bail!("--all only applies to --list");
    }
    if args.session_id.is_none() {
        bail!("nothing to do: pass a SESSION_ID to watch, or --list / --cancel <ID>");
    }
    Ok(WatchMode::Create)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn args(session_id: Option<&str>) -> WatchArgs {
        WatchArgs {
            session_id: session_id.map(str::to_string),
            to: None,
            on: None,
            times: None,
            prompt: None,
            list: false,
            all: false,
            cancel: None,
        }
    }

    #[test]
    fn validate_mode_rejects_misplaced_flags() {
        let watch = args(Some("child"));
        assert_eq!(validate_mode(&watch).unwrap(), WatchMode::Create);

        // --cancel 只吃 id + --json
        let mut cancel = args(None);
        cancel.cancel = Some(3);
        assert_eq!(validate_mode(&cancel).unwrap(), WatchMode::Cancel(3));
        for mutate in [
            |a: &mut WatchArgs| a.list = true,
            |a: &mut WatchArgs| a.session_id = Some("child".into()),
            |a: &mut WatchArgs| a.on = Some("done".into()),
            |a: &mut WatchArgs| a.times = Some(2),
            |a: &mut WatchArgs| a.prompt = Some("x".into()),
            |a: &mut WatchArgs| a.to = Some("sid".into()),
            |a: &mut WatchArgs| a.all = true,
        ] {
            let mut cancel = args(None);
            cancel.cancel = Some(3);
            mutate(&mut cancel);
            assert!(validate_mode(&cancel).is_err(), "{cancel:?} 应当被拒");
        }

        // --list 只吃 --all
        let mut list = args(None);
        list.list = true;
        list.all = true;
        assert_eq!(validate_mode(&list).unwrap(), WatchMode::List);
        for mutate in [
            |a: &mut WatchArgs| a.session_id = Some("child".into()),
            |a: &mut WatchArgs| a.on = Some("done".into()),
            |a: &mut WatchArgs| a.times = Some(2),
            |a: &mut WatchArgs| a.prompt = Some("x".into()),
            |a: &mut WatchArgs| a.to = Some("sid".into()),
        ] {
            let mut list = args(None);
            list.list = true;
            mutate(&mut list);
            assert!(validate_mode(&list).is_err(), "{list:?} 应当被拒");
        }

        // 创建：必须有 SESSION_ID；--all 只配 --list
        assert!(validate_mode(&args(None)).is_err());
        let mut all_only = args(Some("child"));
        all_only.all = true;
        assert!(validate_mode(&all_only).is_err());
    }

    #[test]
    fn request_wires_the_source_and_target() {
        let request = args(Some("child")).request("parent").unwrap();
        assert_eq!(request.kind, "session");
        assert_eq!(request.target_session_id, "parent");
        assert_eq!(request.source_session_id.as_deref(), Some("child"));
        // 缺省交给网关：on_events / times / prompt / spec 都不上 wire
        assert!(request.on_events.is_none(), "缺省 done,ask 由网关补齐");
        assert!(request.times.is_none());
        assert!(request.prompt.is_none());
        assert!(request.spec.is_none());
    }

    #[test]
    fn request_normalizes_the_event_list() {
        let mut watch = args(Some("child"));
        watch.on = Some(" DONE , ask ".into());
        assert_eq!(
            watch.request("parent").unwrap().on_events.as_deref(),
            Some("done,ask")
        );

        watch.on = Some("done,error".into());
        assert!(watch.request("parent").is_err());
    }

    #[test]
    fn request_keeps_a_custom_prompt_and_quota() {
        let mut watch = args(Some("child"));
        watch.prompt = Some("  look at it  ".into());
        watch.times = Some(3);
        let request = watch.request("parent").unwrap();
        assert_eq!(request.prompt.as_deref(), Some("look at it"));
        assert_eq!(request.times, Some(3));
    }

    #[test]
    fn blank_prompt_falls_back_to_the_envelope() {
        let mut watch = args(Some("child"));
        watch.prompt = Some("   ".into());
        assert!(watch.request("parent").unwrap().prompt.is_none());
    }

    #[test]
    fn times_must_be_positive() {
        let mut watch = args(Some("child"));
        watch.times = Some(0);
        assert!(watch.request("parent").is_err());
    }

    #[test]
    fn watching_without_a_session_is_an_error() {
        assert!(args(None).request("parent").is_err());
        assert!(args(Some("  ")).request("parent").is_err());
    }
}
