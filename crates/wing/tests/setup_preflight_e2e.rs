//! End-to-end: the first-run preflight's degradation path, driven through the real binary.
//!
//! `wing` / `wing -p` (stdio) / `wing acp` 共用同一次预检（`cmd/setup.rs` 的
//! `preflight_or_report`）。两条硬红线只有**真进程**能钉住 —— `ExitCode` 在 stable 上
//! 读不回来（`ExitCode::to_i32` 是 unstable），stdout 的"零字节"也必须由管道实测：
//!
//! 1. 配置不可用 ⇒ stdio / ACP 只写 **stderr** 并退出 **78**（`EX_CONFIG`），stdout
//!    **一个字节都不能有** —— 那是 NDJSON 帧 / JSON-RPC 消息的通道，写一个字节就是协议污染。
//! 2. 配置可用 ⇒ 照常走今天的启动链（不 78、不打降级报告），预检是纯增量能力。
//!
//! 假网关只答 `/api/health`、`/api/settings/status`、`/api/settings/get`；
//! `/ws` 回 400，用来观察预检放行后是否实际尝试了握手。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

mod common;

use common::e2e::{RUN_TIMEOUT, read_request, run_wing, start_fake_gateway};

#[derive(Clone, Copy)]
enum Mode {
    /// 降级期：`valid=false` + 一条问题。
    Unusable,
    /// 正常：`valid=true`。
    Usable,
    /// 设置状态响应缺少 `valid` 等字段。
    Legacy,
    /// 健康端点声明了另一个服务。
    OtherService,
}

type Requests = Arc<Mutex<Vec<String>>>;

// ============================================================
// 假网关：最小 HTTP/1.1（不升级 WS）
// ============================================================
async fn handle_conn(mut stream: TcpStream, (mode, requests): (Mode, Requests)) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some((head, _body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let path = target.split('?').next().unwrap_or("");
        requests.lock().unwrap().push(path.to_owned());

        // `/ws`：让握手失败（不升级）—— 用来验证"配置可用时确实去连了 WS"。
        if path == "/ws" {
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                .await;
            return;
        }

        let body = match path {
            "/api/health" if matches!(mode, Mode::OtherService) => {
                r#"{"service":"fixture-service","status":"ok","version":"test","uptime":1}"#
                    .to_string()
            }
            "/api/health" => {
                r#"{"service":"wing-gateway","status":"ok","version":"test","uptime":1}"#
                    .to_string()
            }
            "/api/settings/status" => match mode {
                Mode::Unusable => r#"{"valid":false,"setup_mode":true,"fingerprint":"fp-1",
                     "problems":[{"path":"providers","kind":"empty_list",
                     "message":"providers list cannot be empty","hint":null}]}"#
                    .replace('\n', ""),
                Mode::Usable | Mode::OtherService => {
                    r#"{"valid":true,"setup_mode":false,"fingerprint":"fp-1","problems":[]}"#
                        .to_string()
                }
                Mode::Legacy => "{}".to_string(),
            },
            "/api/settings/get" => r#"{"values":{},"secrets":{},"fingerprint":"fp-1",
                 "problems":[],"setup_mode":true,"config_path":"/tmp/wing-e2e/core/config.yaml"}"#
                .replace('\n', ""),
            _ => "{}".to_string(),
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            body.len(),
            body
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}
struct Stub {
    home: PathBuf,
    port: u16,
    requests: Requests,
}

impl Stub {
    async fn start(mode: Mode, tag: &str) -> Self {
        let requests = Requests::default();
        let (home, port) = start_fake_gateway(
            "wing-setup-e2e",
            tag,
            (mode, Arc::clone(&requests)),
            handle_conn,
        )
        .await;
        Self {
            home,
            port,
            requests,
        }
    }

    fn assert_requests(&self, expected: &[&str]) {
        assert_eq!(self.requests.lock().unwrap().as_slice(), expected);
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// stdio 的降级 = stderr 有清单与出路 + **78** + stdout 零字节。
#[tokio::test]
async fn stdio_exits_78_with_the_problem_list_and_an_empty_stdout() {
    let stub = Stub::start(Mode::Unusable, "stdio-unusable").await;
    let (code, stdout, stderr) = run_wing(&["-p", "hi"], &stub.home).await;

    assert_eq!(code, 78, "EX_CONFIG：\nstderr: {stderr}");
    assert!(
        stdout.is_empty(),
        "stdout 必须零字节（NDJSON 通道）：{:?}",
        stdout
    );
    assert!(
        stderr.contains("网关配置不可用"),
        "stderr 要说清为什么不能启动：{stderr}"
    );
    assert!(
        stderr.contains("providers") && stderr.contains("providers list cannot be empty"),
        "stderr 要带上后端的问题清单：{stderr}"
    );
    assert!(
        stderr.contains("修复方式") && stderr.contains("/tmp/wing-e2e/core/config.yaml"),
        "stderr 要有出路（含可编辑的路径）：{stderr}"
    );
    assert!(!stderr.contains("panicked"), "不该 panic：{stderr}");
    stub.assert_requests(&["/api/health", "/api/settings/status", "/api/settings/get"]);
}

/// 同一条预检在 ACP 形态上的红线：stdout（JSON-RPC 通道）零字节 + 78。
#[tokio::test]
async fn acp_exits_78_with_an_empty_stdout() {
    let stub = Stub::start(Mode::Unusable, "acp-unusable").await;
    let (code, stdout, stderr) = run_wing(&["acp"], &stub.home).await;

    assert_eq!(code, 78, "EX_CONFIG：\nstderr: {stderr}");
    assert!(
        stdout.is_empty(),
        "ACP 的 stdout 必须零字节（JSON-RPC 通道）：{:?}",
        stdout
    );
    assert!(stderr.contains("网关配置不可用"), "{stderr}");
    stub.assert_requests(&["/api/health", "/api/settings/status", "/api/settings/get"]);
}

#[tokio::test]
async fn invalid_api_key_stops_both_frontends_after_health() {
    let stub = Stub::start(Mode::Usable, "invalid-api-key").await;
    let tui_dir = stub.home.join("tui");
    std::fs::create_dir(&tui_dir).unwrap();
    std::fs::write(tui_dir.join("config.yaml"), "api_key: \"fixture\\nkey\"\n").unwrap();

    for (index, (args, message)) in [
        (&["-p", "hi"][..], "Failed to create HTTP client"),
        (&["acp"][..], "failed to create HTTP client"),
    ]
    .into_iter()
    .enumerate()
    {
        let (code, stdout, stderr) = run_wing(args, &stub.home).await;
        assert_eq!(code, 1, "{args:?}: {stderr}");
        assert!(stdout.is_empty(), "{args:?}: {stdout:?}");
        assert!(
            stderr.starts_with(&format!(
                "wing error: {message}: invalid API key for HTTP header:"
            )),
            "{args:?}: {stderr}"
        );
        stub.assert_requests(&["/api/health", "/api/health"][..=index]);
    }
}

#[tokio::test]
async fn acp_attempts_websocket_after_a_legacy_status_response() {
    let stub = Stub::start(Mode::Legacy, "acp-legacy-status").await;
    let (code, stdout, stderr) = run_wing(&["acp"], &stub.home).await;

    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.is_empty(), "{stdout:?}");
    let ws_url = format!("ws://127.0.0.1:{}/ws", stub.port);
    assert!(
        stderr.starts_with(&format!(
            "wing error: failed to connect to gateway at {ws_url}:"
        )),
        "{stderr}"
    );
    stub.assert_requests(&["/api/health", "/api/settings/status", "/ws"]);
}

#[tokio::test]
async fn default_tui_reports_the_gateway_start_failure() {
    let stub = Stub::start(Mode::OtherService, "default-tui-other-service").await;
    let (code, stdout, stderr) = run_wing(&[], &stub.home).await;

    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(
        stderr.starts_with(&format!(
            "wing error: Port {} is already in use by another process (not wing-gateway)",
            stub.port,
        )),
        "{stderr}"
    );
    // Discovery and the start command both check whether the listener belongs to Wing.
    stub.assert_requests(&["/api/health", "/api/health"]);
}

#[tokio::test]
async fn public_stdio_gateway_entry_discovers_running_gateway() {
    const PORT_ENV: &str = "WING_SETUP_PREFLIGHT_WRAPPER_PORT";
    if let Ok(port) = std::env::var(PORT_ENV) {
        let endpoint = wing::stdio::ensure_gateway_running().await.unwrap();
        assert_eq!(endpoint, ("127.0.0.1".into(), port.parse::<u16>().unwrap()));
        return;
    }

    // Re-exec this test so the library reads an isolated WING_HOME.
    let stub = Stub::start(Mode::Usable, "stdio-public-entry").await;
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "public_stdio_gateway_entry_discovers_running_gateway",
        ])
        .env("WING_HOME", &stub.home)
        .env(PORT_ENV, stub.port.to_string())
        .kill_on_drop(true);
    let output = tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .expect("public gateway entry timed out")
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    stub.assert_requests(&["/api/health"]);
}

/// 反向：配置可用时**不**走降级路径 —— 预检不许拦下一个能用的网关。
///
/// 假网关没有 WS，所以进程会在 WS 握手上失败（非 78、非 setup 报告）——
/// 这恰好证明它确实进了今天的启动链。
#[tokio::test]
async fn usable_config_goes_down_the_normal_startup_chain() {
    let stub = Stub::start(Mode::Usable, "stdio-usable").await;
    let (code, _stdout, stderr) = run_wing(&["-p", "hi"], &stub.home).await;

    assert_ne!(code, 78, "配置可用时不该按 EX_CONFIG 退出：\n{stderr}");
    assert!(
        !stderr.contains("网关配置不可用"),
        "配置可用时不该出现降级报告：{stderr}"
    );
    assert!(
        stderr.contains("ws://"),
        "应当真的去连 WS（今天的启动链）：{stderr}"
    );
    stub.assert_requests(&["/api/health", "/api/settings/status", "/ws"]);
}
