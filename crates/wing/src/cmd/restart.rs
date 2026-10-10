//! `wing restart` — stop + start, as one command.
//!
//! The scripted `wing stop && wing start` pair, with the two idempotent cases
//! handled the same way they are on their own: a gateway that is not running
//! is simply started, and one that is running is shut down first (polling
//! health until it is gone) before the replacement is spawned and probed.
//!
//! **A stop that does not finish is a failed restart, not a warning.** When
//! the old process is still reachable after the stop window, starting anyway
//! is worse than useless: `wing start` probes the endpoint, sees the dying
//! process answering, concludes "already running" and skips the spawn — and
//! once that process finishes exiting a few seconds later there is no gateway
//! at all, while the command has already reported success. So that case stops
//! here: nothing is started, the receipt says `ok: false` with
//! `stop: "shutdown_initiated"`, and the exit code is non-zero. A retry once
//! the old process is gone (or a `wing start`) is a plain second command.
//!
//! `--json` reports the endpoint and the stop outcome; the spawn messages
//! from `wing start` go to stderr, so stdout stays machine-readable.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use serde::Serialize;

use super::backend_config::read_backend_gateway_config;
use super::{start, stop};

/// Output of `wing restart` (also used with `--json`).
#[derive(Serialize)]
struct RestartOutput {
    /// True only when a reachable gateway is serving the endpoint at return
    /// time. `false` means the restart did **not** happen (the old process
    /// would not go away), not merely that it went slowly.
    ok: bool,
    host: String,
    port: u16,
    /// What the stop half found — `"stopped"` / `"not_running"` /
    /// `"shutdown_initiated"` (`false`-`ok` case: the old process was still
    /// reachable after the stop window, so nothing was started).
    stop: &'static str,
}

/// Entry point for `wing restart`.
pub async fn run(host: Option<String>, port: Option<u16>, json: bool) -> ExitCode {
    let gw = read_backend_gateway_config();
    let host = host.unwrap_or(gw.host);
    let port = port.unwrap_or(gw.port);

    let stop_outcome = match stop::stop_gateway_quiet(&host, port).await {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("wing restart error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stop_label = match stop_outcome {
        stop::StopOutcome::Stopped => "stopped",
        stop::StopOutcome::NotRunning => "not_running",
        stop::StopOutcome::ShutdownInitiated => return report_unreplaced(host, port, json),
    };

    if let Err(e) = start::start_gateway(&host, port).await {
        eprintln!("wing restart error: {e}");
        return ExitCode::FAILURE;
    }

    let output = RestartOutput {
        ok: true,
        host,
        port,
        stop: stop_label,
    };
    if json {
        super::common::print_json_compact(&output);
    } else {
        println!(
            "restart: gateway ready at ws://{}:{}/ws (stop: {})",
            output.host, output.port, output.stop
        );
    }
    ExitCode::SUCCESS
}

/// The stop half left the old process running: report it and start nothing.
fn report_unreplaced(host: String, port: u16, json: bool) -> ExitCode {
    let output = RestartOutput {
        ok: false,
        host,
        port,
        stop: "shutdown_initiated",
    };
    if json {
        super::common::print_json_compact(&output);
    } else {
        println!(
            "restart: FAILED — the gateway at ws://{}:{}/ws did not exit; \
             no new gateway was started",
            output.host, output.port
        );
    }
    eprintln!(
        "wing restart error: the old gateway at ws://{}:{}/ws was still reachable \
         after the stop window and no new gateway was started; check `wing status` \
         and retry once it is gone",
        output.host, output.port
    );
    ExitCode::FAILURE
}
