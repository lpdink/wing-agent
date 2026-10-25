//! End-to-end: `wing -p` 一次性 stdio 的收口契约（真二进制 + 假网关）。
//!
//! 钉住 issue #246：后端在一轮结尾**只发 `error` + `done`、不发 `turn_result`**
//! 时，一次性流程必须退出（退出码 1）并留下一条失败终态帧——而不是永远等
//! `recv_event`（进程挂死）。这里只回答一个问题：**进程到底退不退**；帧的形状由
//! `src/stdio/` 的单测钉住。
//!
//! harness 与 `stdio_resident_e2e.rs` 同一套：真实 `CARGO_BIN_EXE_wing` +
//! 进程内最小 HTTP/1.1 + WS 假网关 + 假 `$WING_HOME`（不碰任何真实环境）。
//! 一次性模式只发一条 prompt，所以帧脚本在握手应答之后**一次推完**——代价是这些
//! 用例对帧序不敏感（帧序由 `stdio_resident_e2e` 与渲染层单测覆盖）。

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

mod common;

use common::e2e::{read_request, run_wing, start_fake_gateway};

const SESSION_ID: &str = "20260101-000000-one-shot";

/// 假网关在这一轮里报的错误原文（`_run` 兜底路径上的 `error` 事件）。
const ERROR_MESSAGE: &str = "处理消息失败：异常：RuntimeError: boom";

// ============================================================
// 假网关：一条连接一次 WS 握手 + 一段脚本化帧
// ============================================================

/// WS 帧脚本：握手应答（`connected`）之后按顺序推出去的帧
/// （帧序取真实后端形状，见各脚本的注释）。
type Script = fn() -> Vec<Value>;

/// 事件公共字段（假网关不区分请求，`request_id` 只是占位）。
fn meta(kind: &str) -> Value {
    json!({
        "type": kind,
        "session_id": SESSION_ID,
        "created_at": "2026-01-01T00:00:00",
        "request_id": "fake",
    })
}

fn turn_started() -> Value {
    meta("turn_started")
}

fn done() -> Value {
    meta("done")
}

fn turn_result(subtype: &str, is_error: bool) -> Value {
    let mut value = json!({
        "type": "turn_result",
        "uuid": "r-1",
        "subtype": subtype,
        "is_error": is_error,
        "num_turns": 1,
        "duration_ms": 5,
        "session_id": SESSION_ID,
        "created_at": "2026-01-01T00:00:00",
        "request_id": "fake",
    });
    if is_error {
        value["errors"] = json!([ERROR_MESSAGE]);
    } else {
        value["result"] = json!("hello there");
    }
    value
}

/// issue #246 的形状：`turn_started → error → done`（没有 `turn_result`）。
fn script_error_then_done() -> Vec<Value> {
    let mut error = meta("error");
    error["message"] = json!(ERROR_MESSAGE);
    vec![turn_started(), error, done()]
}

/// 连 `error` 都没有：`done` 是唯一的收口信号。
fn script_done_only() -> Vec<Value> {
    vec![turn_started(), done()]
}

/// 正常轮的后端帧序：`turn_result → done`（`done` 不得再补第二条帧）。
fn script_success_then_done() -> Vec<Value> {
    vec![turn_started(), turn_result("success", false), done()]
}

async fn handle_conn(mut stream: TcpStream, script: Script) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some((head, _body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        let path = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string();

        if path == "/ws"
            && header_value(&head, "upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            let key = header_value(&head, "sec-websocket-key").unwrap_or_default();
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {}\r\n\r\n",
                derive_accept_key(key.as_bytes())
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                return;
            }
            let mut ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
            let connected = json!({"type": "connected", "client_id": "fake"});
            for frame in std::iter::once(connected).chain(script()) {
                if ws
                    .send(Message::Text(frame.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // 进程退出会关掉 WS：留着连接等对端关闭（不主动写、不主动关）。
            while let Some(Ok(_)) = ws.next().await {}
            return;
        }

        let payload = match path.as_str() {
            "/api/health" => {
                r#"{"service":"wing-gateway","status":"ok","version":"test","uptime":1}"#
                    .to_string()
            }
            "/api/session/create" => format!(
                r#"{{"session_id":"{SESSION_ID}","template_name":"default","workspace":null,"backend":"memory"}}"#
            ),
            // `send` / `subscribe` / `settings/status` 等一律回通用 ok
            // （预检把解不出的 body 当「网关早于 Setting API」→ Ready）。
            _ => r#"{"ok":true,"request_id":"fake-req"}"#.to_string(),
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            payload.len(),
            payload
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// 请求头里取一个字段（大小写不敏感）。
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

async fn run(script: Script, tag: &str, args: &[&str]) -> (i32, String, String) {
    let (home, _port) =
        start_fake_gateway("wing-one-shot", tag, script, |stream, script| async move {
            handle_conn(stream, script).await;
        })
        .await;
    run_wing(args, &home).await
}

/// stdout 的完整帧（按顺序；空行忽略）。
fn frames(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

// ============================================================
// issue #246：没有 turn_result 的收口
// ============================================================

/// `--output-format stream-json`（SDK 的真实形状）：退出码 1 + 一条失败终态帧。
#[tokio::test]
async fn stream_json_exits_after_error_and_done_without_a_turn_result() {
    let (code, stdout, stderr) = run(
        script_error_then_done,
        "stream-json",
        &["-p", "hello", "--output-format", "stream-json"],
    )
    .await;

    assert_eq!(
        code, 1,
        "后端没能完成这一轮：退出码必须非 0\nstderr: {stderr}"
    );
    let frames = frames(&stdout);
    assert_eq!(frames.len(), 1, "一条终态帧：{stdout}");
    assert_eq!(frames[0]["type"], "result");
    assert_eq!(frames[0]["subtype"], "error_during_execution");
    assert_eq!(frames[0]["is_error"], true);
    assert_eq!(
        frames[0]["errors"],
        json!([ERROR_MESSAGE]),
        "后端错误原文必须进帧"
    );
    assert!(frames[0].get("result").is_none(), "{}", frames[0]);
    assert_eq!(frames[0]["session_id"], SESSION_ID);
}

/// `--output-format json`：契约仍是「单个 result 对象」。
#[tokio::test]
async fn json_exits_after_error_and_done_without_a_turn_result() {
    let (code, stdout, stderr) = run(
        script_error_then_done,
        "json",
        &["-p", "hello", "--output-format", "json"],
    )
    .await;

    assert_eq!(code, 1, "stderr: {stderr}");
    let frames = frames(&stdout);
    assert_eq!(frames.len(), 1, "json 模式只出一条：{stdout}");
    assert_eq!(frames[0]["type"], "result");
    assert_eq!(frames[0]["subtype"], "error_during_execution");
    assert_eq!(frames[0]["errors"], json!([ERROR_MESSAGE]));
}

/// 默认 text 模式：错误文本走 stderr，stdout 一个字节都不写。
#[tokio::test]
async fn text_exits_after_error_and_done_and_keeps_stdout_clean() {
    let (code, stdout, stderr) = run(script_error_then_done, "text", &["-p", "hello"]).await;

    assert_eq!(code, 1, "stderr: {stderr}");
    assert_eq!(stdout, "", "stdout 是协议通道：错误文本只进 stderr");
    assert!(
        stderr.contains(ERROR_MESSAGE),
        "错误原文必须在 stderr 上可诊断: {stderr}"
    );
    assert!(stderr.contains("Error: "), "stderr: {stderr}");
}

/// 兜底文案：本轮连 `error` 事件都没有时，stderr 仍要说清「没有终态帧」。
#[tokio::test]
async fn text_reports_the_missing_terminal_frame_when_no_error_preceded_done() {
    let (code, stdout, stderr) = run(script_done_only, "no-error", &["-p", "hello"]).await;

    assert_eq!(code, 1, "stderr: {stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("without a terminal frame"),
        "兜底文案要说清这一轮没有终态帧: {stderr}"
    );
}

// ============================================================
// 正常路径：逐字节不变
// ============================================================

/// 成功轮：`turn_result` 收口 → 退出码 0，且 `done` 不再补帧
/// （json 模式的契约是「单个 result 对象」，多一条就是行为变化）。
#[tokio::test]
async fn json_success_emits_a_single_result_frame_before_the_trailing_done() {
    let (code, stdout, stderr) = run(
        script_success_then_done,
        "success",
        &["-p", "hello", "--output-format", "json"],
    )
    .await;

    assert_eq!(code, 0, "成功轮：退出码 0\nstderr: {stderr}");
    let frames = frames(&stdout);
    assert_eq!(frames.len(), 1, "json 模式一条 result：{stdout}");
    assert_eq!(frames[0]["subtype"], "success");
    assert_eq!(frames[0]["result"], "hello there");
}
