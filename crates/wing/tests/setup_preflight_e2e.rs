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

#[cfg(unix)]
mod tui_paste {
    use super::*;
    use std::fs::{self, File};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::process::Stdio;
    use std::time::Duration;

    use futures_util::SinkExt;
    use serde_json::{Value, json};
    use tokio::sync::mpsc;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::{Message, handshake::derive_accept_key, protocol::Role};

    use crate::common::e2e::wing_command;
    use crate::common::pty::{openpty_pair, read_written, write_all};

    const INTERFACE_KEY: &str = "fixture-interface-pty-32";
    const PROVIDER_KEY: &str = "fixture-provider-pty-83";

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.trim()))
    }

    #[derive(Clone)]
    struct SetupStub {
        seen: mpsc::UnboundedSender<(&'static str, bool)>,
        operational: bool,
        stage: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    async fn serve_setup(mut stream: TcpStream, stub: SetupStub) {
        use std::sync::atomic::Ordering;
        let SetupStub {
            seen,
            operational,
            stage,
        } = stub;
        let mut buffer = Vec::new();
        let problem = json!({"path":"provider_token", "kind":"missing_required", "message":"Fixture provider key is required"});
        while let Some((head, body)) = read_request(&mut stream, &mut buffer).await {
            let path = head.split_whitespace().nth(1).unwrap_or_default();
            let auth_ok = header(&head, "authorization")
                .is_some_and(|value| value.strip_prefix("Bearer ") == Some(INTERFACE_KEY));
            let mut status = "200 OK";
            let result = match path {
                "/api/settings/status" if operational && stage.load(Ordering::SeqCst) > 0 => {
                    let check = stage.fetch_add(1, Ordering::SeqCst);
                    let _ = seen.send(("readiness auth", auth_ok));
                    if !auth_ok || check == 1 {
                        status = if auth_ok {
                            "503 Unavailable"
                        } else {
                            "401 Unauthorized"
                        };
                        json!({"error":"fixture-check"})
                    } else {
                        json!({"valid":true,"setup_mode":false,"problems":[]})
                    }
                }
                "/api/settings/status" => {
                    json!({"valid":false,"setup_mode":!operational,"problems":[problem]})
                }
                "/api/settings/get" => {
                    json!({"values":{},"setup_mode":!operational,"fingerprint":"fixture-base","config_path":"fixture.yaml","problems":[problem]})
                }
                "/api/settings/schema" => {
                    json!({"version":"fixture","config_path":"fixture.yaml","root":{
                        "key":"config","path":"config","title":"Config","kind":"object","doc":"",
                        "children":[{"key":"provider_token","path":"provider_token","title":"Provider key","doc":"Fixture key","kind":"secret","secret":true,"required":true}]
                    }})
                }
                "/api/settings/set" => {
                    let old_key = header(&head, "authorization") == Some("Bearer old-fixture-key");
                    let matched = old_key
                        && stage.load(Ordering::SeqCst) == 0
                        && serde_json::from_slice::<Value>(&body).ok()
                            == Some(json!({
                                "base":"fixture-base","document":{"provider_token":PROVIDER_KEY}
                            }));
                    if matched {
                        stage.store(1, Ordering::SeqCst);
                    }
                    let _ = seen.send(("provider saved", matched));
                    json!({"ok":matched,"fingerprint":"saved","problems":[],"setup_mode_exited":matched && !operational,"changed":["provider_token"]})
                }
                "/ws" => {
                    let _ = seen.send(("WS auth", auth_ok));
                    let key = header(&head, "sec-websocket-key").expect("WebSocket upgrade");
                    let accept = derive_accept_key(key.as_bytes());
                    let reply = format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                    );
                    stream.write_all(reply.as_bytes()).await.unwrap();
                    let mut ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
                    let _ = ws
                        .send(Message::Text(
                            r#"{"type":"connected","client_id":"fixture"}"#.into(),
                        ))
                        .await;
                    let _ = seen.closed().await;
                    return;
                }
                "/api/session/create" => {
                    let _ = seen.send(("HTTP auth", auth_ok));
                    json!({"session_id":"fixture","template_name":"test","workspace":null})
                }
                _ => json!({}),
            };
            let result = result.to_string();
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                result.len(),
                result
            );
            if stream.write_all(reply.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    fn bracketed(text: &str) -> String {
        format!("\x1b[200~{text}\x1b[201~")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn setup_paste_saves_and_authenticates_with_the_new_interface_key() {
        verify_setup_auth(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn operational_repair_retries_readiness_with_the_rotated_key() {
        verify_setup_auth(true).await;
    }

    async fn verify_setup_auth(operational: bool) {
        let (seen, mut requests) = mpsc::unbounded_channel();
        let stub = SetupStub {
            seen,
            operational,
            stage: Default::default(),
        };
        let tag = if operational {
            "tui-rotation"
        } else {
            "tui-paste"
        };
        let (home, port) = start_fake_gateway("wing-setup-e2e", tag, stub, serve_setup).await;
        fs::create_dir_all(home.join("tui")).unwrap();
        let interface_path = home.join("tui/config.yaml");
        fs::write(
            &interface_path,
            "api_key: old-fixture-key\ncolors:\n  preset: dark\nrendering:\n  images: off\n",
        )
        .unwrap();

        let (master, slave) = openpty_pair();
        // Both descriptors are owned by this test until transferred to the child.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        let size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
        let port = port.to_string();
        let mut command = wing_command(&["tui", "--host", "127.0.0.1", "--port", &port], &home);
        command
            .env("TERM", "xterm-256color")
            .env(
                "RUST_LOG",
                "wing=debug,tungstenite::handshake::client=trace",
            )
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // Give crossterm a real controlling terminal for winsize and mode changes.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY.into(), 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        let fd = master.as_raw_fd();
        let mut output = Vec::new();
        let mut wait_for = |fragment: &str| {
            let text = fragment.as_bytes();
            output.extend(read_written(fd, Duration::from_secs(4), |buf| {
                buf.windows(text.len()).any(|window| window == text)
            }));
            assert!(
                output.windows(text.len()).any(|window| window == text),
                "expected setup frame fragment {fragment:?}"
            );
        };
        wait_for("Fixture provider key is required");

        let interface_paste = format!(
            "/{}\r\r{}",
            bracketed("api_key\r\n"),
            bracketed(&format!("{INTERFACE_KEY}\r\n"))
        );
        write_all(fd, interface_paste.as_bytes());
        wait_for(&"•".repeat(INTERFACE_KEY.len()));
        let provider_paste = format!(
            "\r/{}\r\r{}\rs",
            bracketed("provider_token"),
            bracketed(&format!("{PROVIDER_KEY}\n"))
        );
        write_all(fd, provider_paste.as_bytes());
        let readiness_frame = if operational {
            // Drain redraws while the wizard handles the save and readiness request.
            let frame = read_written(fd, Duration::from_millis(800), |_| false);
            for expected in ["provider saved", "readiness auth"] {
                let observed = tokio::time::timeout(Duration::from_secs(4), requests.recv())
                    .await
                    .unwrap_or_else(|_| panic!("missing fixture request {expected}"));
                assert_eq!(observed, Some((expected, true)));
            }
            write_all(fd, b"s");
            frame
        } else {
            Vec::new()
        };
        wait_for("\x1b[?1049l");
        output.extend(readiness_frame);
        for expected in [
            if operational {
                "readiness auth"
            } else {
                "provider saved"
            },
            "WS auth",
            "HTTP auth",
        ] {
            let observed = tokio::time::timeout(Duration::from_secs(4), requests.recv())
                .await
                .unwrap();
            assert_eq!(observed, Some((expected, true)));
        }
        assert!(
            fs::read_to_string(&interface_path)
                .unwrap()
                .contains(INTERFACE_KEY)
        );
        for key in [INTERFACE_KEY, PROVIDER_KEY] {
            assert!(
                !output
                    .windows(key.len())
                    .any(|bytes| bytes == key.as_bytes())
            );
        }
        let logs = || -> String {
            fs::read_dir(home.join("tui/logs"))
                .unwrap()
                .flatten()
                .filter_map(|entry| fs::read_to_string(entry.path()).ok())
                .collect()
        };
        tokio::time::timeout(Duration::from_secs(4), async {
            while !logs().contains("session created") {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("startup log");
        let logs = logs();
        assert!(!logs.contains(INTERFACE_KEY) && !logs.contains(PROVIDER_KEY));
        // Close the controller first so the child can leave the PTY on macOS.
        drop(master);
        tokio::time::timeout(Duration::from_secs(4), child.kill())
            .await
            .expect("TUI child exit")
            .unwrap();
        drop(requests);
        fs::remove_dir_all(home).unwrap();
    }
}
