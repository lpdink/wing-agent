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
//! `Usable` 形态下 `/ws` 回 400（没有 WS 可连 ⇒ 客户端在 WS 握手上失败，进程退出）。

use std::path::PathBuf;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

mod common;

use common::e2e::{read_request, run_wing, start_fake_gateway};

/// 假网关给 `/api/settings/status` 的答案。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 降级期：`valid=false` + 一条问题。
    Unusable,
    /// 正常：`valid=true`。
    Usable,
}

// ============================================================
// 假网关：最小 HTTP/1.1（不升级 WS）
// ============================================================
async fn handle_conn(mut stream: TcpStream, mode: Mode) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some((head, _body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        let path = target.split('?').next().unwrap_or("").to_string();

        // `/ws`：让握手失败（不升级）—— 用来验证"配置可用时确实去连了 WS"。
        if path == "/ws" {
            let _ = stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                .await;
            return;
        }

        let body = match path.as_str() {
            "/api/health" => {
                r#"{"service":"wing-gateway","status":"ok","version":"test","uptime":1}"#
                    .to_string()
            }
            "/api/settings/status" => match mode {
                Mode::Unusable => r#"{"valid":false,"setup_mode":true,"fingerprint":"fp-1",
                     "problems":[{"path":"providers","kind":"empty_list",
                     "message":"providers list cannot be empty","hint":null}]}"#
                    .replace('\n', ""),
                Mode::Usable => {
                    r#"{"valid":true,"setup_mode":false,"fingerprint":"fp-1","problems":[]}"#
                        .to_string()
                }
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
// ============================================================
// 夹具
// ============================================================
// ============================================================
// 测试
// ============================================================

/// 绑端口 + 写出指向它的 `WING_HOME`。
async fn start_gateway(mode: Mode, tag: &str) -> PathBuf {
    let (home, _port) = start_fake_gateway("wing-setup-e2e", tag, mode, handle_conn).await;
    home
}

/// stdio 的降级 = stderr 有清单与出路 + **78** + stdout 零字节。
#[tokio::test]
async fn stdio_exits_78_with_the_problem_list_and_an_empty_stdout() {
    let home = start_gateway(Mode::Unusable, "stdio-unusable").await;
    let (code, stdout, stderr) = run_wing(&["-p", "hi"], &home).await;

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
    let _ = std::fs::remove_dir_all(&home);
}

/// 同一条预检在 ACP 形态上的红线：stdout（JSON-RPC 通道）零字节 + 78。
#[tokio::test]
async fn acp_exits_78_with_an_empty_stdout() {
    let home = start_gateway(Mode::Unusable, "acp-unusable").await;
    let (code, stdout, stderr) = run_wing(&["acp"], &home).await;

    assert_eq!(code, 78, "EX_CONFIG：\nstderr: {stderr}");
    assert!(
        stdout.is_empty(),
        "ACP 的 stdout 必须零字节（JSON-RPC 通道）：{:?}",
        stdout
    );
    assert!(stderr.contains("网关配置不可用"), "{stderr}");
    let _ = std::fs::remove_dir_all(&home);
}

/// 反向：配置可用时**不**走降级路径 —— 预检不许拦下一个能用的网关。
///
/// 假网关没有 WS，所以进程会在 WS 握手上失败（非 78、非 setup 报告）——
/// 这恰好证明它确实进了今天的启动链。
#[tokio::test]
async fn usable_config_goes_down_the_normal_startup_chain() {
    let home = start_gateway(Mode::Usable, "stdio-usable").await;
    let (code, _stdout, stderr) = run_wing(&["-p", "hi"], &home).await;

    assert_ne!(code, 78, "配置可用时不该按 EX_CONFIG 退出：\n{stderr}");
    assert!(
        !stderr.contains("网关配置不可用"),
        "配置可用时不该出现降级报告：{stderr}"
    );
    assert!(
        stderr.contains("ws://"),
        "应当真的去连 WS（今天的启动链）：{stderr}"
    );
    let _ = std::fs::remove_dir_all(&home);
}
