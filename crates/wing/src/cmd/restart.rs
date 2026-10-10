//! `wing restart` — stop + start, as one command.
//!
//! The scripted `wing stop && wing start` pair, with the two idempotent cases
//! handled the same way they are on their own: a gateway that is not running
//! is simply started, and one that is running is shut down first (polling
//! health until it is gone) before the replacement is spawned and probed.
//!
//! `--json` reports the stop outcome and the endpoint; the spawn messages
//! from `wing start` go to stderr, so stdout stays machine-readable.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use serde::Serialize;

use super::backend_config::read_backend_gateway_config;
use super::{start, stop};

/// Output of `wing restart` (also used with `--json`).
#[derive(Serialize)]
struct RestartOutput {
    ok: bool,
    host: String,
    port: u16,
    /// What the stop half found — `"stopped"` / `"not_running"` /
    /// `"shutdown_initiated"` (the old process was still reachable after 5s).
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
        stop::StopOutcome::ShutdownInitiated => {
            // 旧进程 5s 内仍可达：start 会看到"已在运行"并按幂等语义返回——
            // 这可能不是调用方要的重启，必须说出来。
            eprintln!(
                "warning: the old gateway was still reachable after 5s; \
                 the restart may not have replaced it (check `wing status`)"
            );
            "shutdown_initiated"
        }
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
