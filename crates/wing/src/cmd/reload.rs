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
pub async fn run_reload(json: bool, host: Option<String>, port: Option<u16>) -> ExitCode {
    match reload_inner(host, port).await {
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
        Err(e) => common::fail("reload", e),
    }
}

async fn reload_inner(host: Option<String>, port: Option<u16>) -> Result<ReloadResponse> {
    let configured = super::backend_config::read_backend_gateway_config();
    let host = host.unwrap_or(configured.host);
    let port = port.unwrap_or(configured.port);
    let discovery = wing_api_client::GatewayClient::new(format!("http://{host}:{port}"), None)?;
    if !matches!(discovery.health().await, Ok(health) if health.service == "wing-gateway") {
        anyhow::bail!(
            "no running Wing gateway found at {host}:{port}. Run `wing start` to start one, \
             or use `wing reload --host <old-host> --port <old-port>` if its address changed."
        );
    }
    Ok(common::create_api_client(&host, port)?
        .reload_system()
        .await?)
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
        let detail = resp.display_detail(item);
        let status = if item.ok { "ok" } else { "FAIL" };
        text.push_str(&format!(
            "  {:<4} {:<name_width$} {}\n",
            status,
            common::truncate_chars(&item.name, name_width),
            common::single_line(detail.unwrap_or("")),
        ));
    }
    if resp.results.is_empty() {
        text.push_str("  (no items reported)\n");
    }
    if resp.new_session_defaults_changed {
        text.push_str("New session defaults updated.\n");
    }
    if let Some(paths) = resp
        .pending_restart_paths
        .as_ref()
        .filter(|paths| !paths.is_empty())
    {
        let config_failed = resp
            .results
            .first()
            .is_some_and(|item| item.name == "config.yaml" && !item.ok);
        let action = if config_failed {
            "Fix config.yaml before restarting."
        } else {
            "In a TUI connected to this gateway, press Ctrl+R in settings."
        };
        text.push_str(&format!(
            "Restart required for {}. {action}\n",
            paths.join(", ")
        ));
    }
    if !resp.ok {
        text.push_str("Check the failed steps, then run `wing reload` again.\n");
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
            diagnostics_redacted: true,
            pending_restart_paths: None,
            new_session_defaults_changed: false,
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
            diagnostics_redacted: true,
            pending_restart_paths: Some(vec![]),
            new_session_defaults_changed: false,
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
            diagnostics_redacted: true,
            pending_restart_paths: Some(vec![]),
            new_session_defaults_changed: false,
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
    fn reload_report_from_an_older_gateway_only_shows_the_failed_step() {
        let response: ReloadResponse = serde_json::from_str(
            r#"{"ok":false,"results":[{"name":"config.yaml","ok":false,"detail":"raw config value"}]}"#,
        ).unwrap();
        let text = format_reload(&response);
        assert!(text.contains("FAIL config.yaml"), "{text}");
        assert!(!text.contains("raw config value"), "{text}");
        assert!(!text.contains("Restart required"), "{text}");
        let wire = serde_json::to_value(&response).unwrap();
        assert!(wire.get("pending_restart_paths").is_none());
    }

    #[test]
    fn restart_advice_depends_on_whether_the_config_loaded() {
        for config_ok in [true, false] {
            let response = ReloadResponse {
                diagnostics_redacted: true,
                pending_restart_paths: Some(vec![
                    "gateway.port".into(),
                    "gateway.cors.origins".into(),
                ]),
                new_session_defaults_changed: config_ok,
                ok: config_ok,
                results: vec![item("config.yaml", config_ok, None)],
            };
            let text = format_reload(&response);
            assert!(text.contains("Restart required for gateway.port, gateway.cors.origins."));
            assert_eq!(text.contains("press Ctrl+R"), config_ok);
            assert_eq!(text.contains("New session defaults updated."), config_ok);
            assert_eq!(
                text.contains("Fix config.yaml before restarting."),
                !config_ok
            );
            assert_eq!(text.contains("run `wing reload` again"), !config_ok);
        }
    }

    #[tokio::test]
    async fn reload_rejects_a_different_http_service_after_a_bare_health_check() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
            }
            assert!(request.starts_with("GET /api/health HTTP/1.1"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            let body = r#"{"service":"another-service","status":"ok","version":"test","uptime":0}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.get_mut().write_all(reply.as_bytes()).await.unwrap();
        });
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reload_inner(Some("127.0.0.1".into()), Some(port)),
        )
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
        assert!(error.contains(&format!(
            "no running Wing gateway found at 127.0.0.1:{port}"
        )));
        assert!(error.contains("--host <old-host> --port <old-port>"));
        server.await.unwrap();
    }

    #[test]
    fn reload_report_with_no_items_is_still_readable() {
        let resp = ReloadResponse {
            diagnostics_redacted: true,
            pending_restart_paths: None,
            new_session_defaults_changed: false,
            ok: true,
            results: vec![],
        };
        assert_eq!(format_reload(&resp), "reload: ok\n  (no items reported)\n");
    }
}
