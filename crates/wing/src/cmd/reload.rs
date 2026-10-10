//! `wing reload` — hot-reload the gateway configuration.
//!
//! The endpoint's item **name order is the external contract** (config.yaml →
//! hooks → prompt commands → provider → skills & rules → log level; pinned by
//! the probe suite, append-only): the CLI prints the items in exactly the
//! order the gateway reported, so an operator reads the same sequence in
//! `--json`, in text, and in the docs.
//!
//! Failure semantics are the gateway's and are reported as-is: the
//! `config.yaml` item failing aborts the rest (later items are simply not in
//! the response), every other failure does not stop the run, and nothing is
//! rolled back. The process exits non-zero iff the overall result failed —
//! what a script needs to gate on.
//!
//! ```sh
//! wing reload
//! wing reload --json | jq -r '.results[] | "\(.name)\t\(.ok)"'
//! ```

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::Result;
use wing_api_client::models::ReloadResponse;

use super::common;

/// Entry point for `wing reload`.
pub async fn run_reload(json: bool) -> ExitCode {
    match reload_inner().await {
        Ok(resp) => {
            if json {
                common::print_json_compact(&resp);
            } else {
                print!("{}", format_reload(&resp));
            }
            if resp.ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("wing reload error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn reload_inner() -> Result<ReloadResponse> {
    // `ensure_gateway` starts a gateway when none is running — and a fresh
    // process read the config at boot, so the per-item receipt below describes
    // a pipeline that had nothing to re-read. Say that on stderr instead of
    // letting the receipt imply a live config change.
    let was_running = gateway_responding().await;
    let (host, port) = common::ensure_gateway().await?;
    if !was_running {
        eprintln!(
            "note: no gateway was running — it was just started, and a fresh \
             process already loads the config at boot (the items below are that \
             pipeline run, not a re-read of an edited file)"
        );
    }
    let http = common::create_api_client(&host, port)?;
    Ok(http.reload_system().await?)
}

/// Whether a wing gateway answers on the configured endpoint (a cheap probe;
/// unlike [`common::ensure_gateway`] it starts nothing).
async fn gateway_responding() -> bool {
    let gw = super::backend_config::read_backend_gateway_config();
    let base = format!("http://{}:{}", gw.host, gw.port);
    match wing_api_client::GatewayClient::new(&base, None) {
        Ok(client) => {
            matches!(client.health().await, Ok(health) if health.service == "wing-gateway")
        }
        Err(_) => false,
    }
}

/// Render the per-item report, in the response's own order (the name order is
/// the contract — never sort, never group).
fn format_reload(resp: &ReloadResponse) -> String {
    let mut text = String::new();
    text.push_str(if resp.ok {
        "reload: ok\n"
    } else {
        "reload: FAILED\n"
    });
    let name_width = resp
        .results
        .iter()
        .map(|item| item.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(24);
    for item in &resp.results {
        let status = if item.ok { "ok" } else { "FAIL" };
        text.push_str(&format!(
            "  {:<4} {:<name_width$} {}\n",
            status,
            common::truncate_chars(&item.name, name_width),
            common::single_line(item.detail.as_deref().unwrap_or("")),
        ));
    }
    if resp.results.is_empty() {
        text.push_str("  (no items reported)\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::ReloadResultItem;

    fn item(name: &str, ok: bool, detail: Option<&str>) -> ReloadResultItem {
        ReloadResultItem {
            name: name.into(),
            ok,
            detail: detail.map(str::to_string),
        }
    }

    /// 名字序是对外契约：输出顺序 == 响应顺序，一个字都不许重排。
    #[test]
    fn reload_report_keeps_the_contract_order() {
        let resp = ReloadResponse {
            ok: true,
            results: vec![
                item("config.yaml", true, None),
                item("hooks", true, None),
                item("prompt commands", true, None),
                item("provider", true, None),
                item("skills & rules", true, None),
                item("log level", true, None),
            ],
        };
        let text = format_reload(&resp);
        let names = [
            "config.yaml",
            "hooks",
            "prompt commands",
            "provider",
            "skills & rules",
            "log level",
        ];
        let positions: Vec<usize> = names
            .iter()
            .map(|name| {
                text.find(name)
                    .unwrap_or_else(|| panic!("{name} missing:\n{text}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "顺序错了：{positions:?}\n{text}"
        );
        assert!(text.starts_with("reload: ok\n"), "{text}");
    }

    #[test]
    fn reload_report_shows_failures_and_details() {
        let resp = ReloadResponse {
            ok: false,
            results: vec![
                item("config.yaml", false, Some("providers 不得为空")),
                // config 失败即中止：后续项根本不在响应里。
            ],
        };
        let text = format_reload(&resp);
        assert!(text.starts_with("reload: FAILED\n"), "{text}");
        assert!(text.contains("FAIL config.yaml"), "{text}");
        assert!(text.contains("providers 不得为空"), "{text}");

        // 单项失败（非 config）继续——逐项如实打印。
        let resp = ReloadResponse {
            ok: false,
            results: vec![
                item("config.yaml", true, None),
                item("hooks", false, Some("boom\nsecond line")),
                item("prompt commands", true, None),
            ],
        };
        let text = format_reload(&resp);
        assert!(text.contains("ok   config.yaml"), "{text}");
        assert!(text.contains("FAIL hooks"), "{text}");
        // detail 里的换行被压平，不撕开报告。
        assert!(text.contains("boom second line"), "{text}");
        assert!(text.contains("prompt commands"), "{text}");
    }

    #[test]
    fn reload_report_with_no_items_is_still_readable() {
        let resp = ReloadResponse {
            ok: true,
            results: vec![],
        };
        assert_eq!(format_reload(&resp), "reload: ok\n  (no items reported)\n");
    }
}
