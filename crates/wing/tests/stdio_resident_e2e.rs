//! End-to-end: 常驻 stdio（`--input-format stream-json --output-format stream-json`）。
//!
//! 与 `wait_fail_fast_e2e.rs` 同一套 harness 思路：真实 `CARGO_BIN_EXE_wing`
//! 二进制 + 进程内最小 HTTP/1.1 + WS 假网关 + 假 `$WING_HOME`（网关指到假端口，
//! 不碰任何真实环境）。覆盖常驻语义的四个形状：
//!
//! - 多轮：两轮各有 result、顺序正确、两条之间进程不退出；
//! - EOF 收尾：空闲 EOF 立即退（退出码 = 最后一轮结果）；在途轮等其终态后才退
//!   （迟到的 result 仍然产出）；
//! - 中断形状（CloudCLI）：`control_request{interrupt}` → 假网关回 `interrupted`
//!   → 前端补合成终态帧 + `control_response` 应答 → 关 stdin 后退出；
//! - 轮中消息：turn 未结束时再投 `user` → 转发到 `POST /api/session/send`；
//! - 退出码：最后一轮结果说了算（先失败后成功 = 0；最后一轮失败 ≠ 0）。

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

const SESSION_ID: &str = "20260101-000000-resident";

/// 单条 stdout 帧 / 进程退出的等待上限（远小于 CI 超时，失败时快速失败）。
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

// ============================================================
// 假网关：场景与共享状态
// ============================================================

/// HTTP 侧 → WS 侧的推进信号。
enum HttpEvent {
    /// 收到第 `seq` 条（1 起）`POST /api/session/send`。
    Send { seq: usize },
    /// 收到 `POST /api/session/interrupt`。
    Interrupt,
}

/// 假网关的事件脚本。
#[derive(Clone, Copy, Debug)]
enum Scenario {
    /// 每收到一条 send 就推一轮（turn_started → assistant → result），
    /// result 文本 = `turn-{seq}`。
    Turns,
    /// 第一条 send：推 turn_started + assistant 后**延时**推 result（观察
    /// 「EOF 落在轮中」的收尾）；之后同 `Turns`。
    SlowFirstTurn,
    /// 第一条 send：只推 turn_started + assistant；收到 interrupt 后推
    /// `interrupted`（不推 turn_result——被打断的轮次后端不发终态帧）。
    Interrupt,
    /// 第一条 send：推 turn_started + assistant；收到第二条 send 后推第一轮
    /// 的 result（用它断言「轮中消息被转发」）。
    InTurnForward,
    /// 第一条 send：失败轮；第二条：成功轮。
    ErrorThenSuccess,
    /// 单轮失败。
    ErrorTurn,
}

struct FakeGateway {
    scenario: Scenario,
    /// 每条 `POST /api/session/send` 的正文（按到达顺序）。
    sends: Mutex<Vec<String>>,
    /// interrupt 请求计数。
    interrupts: AtomicUsize,
    /// HTTP → WS 的通知口（WS 升级后由 WS 任务取走）。
    notify: Mutex<Option<mpsc::UnboundedSender<HttpEvent>>>,
    /// 通知口的接收端（WS 升级时一次性取走；一个测试一个客户端连接）。
    events: Mutex<Option<mpsc::UnboundedReceiver<HttpEvent>>>,
}

impl FakeGateway {
    fn sends(&self) -> Vec<String> {
        self.sends.lock().unwrap().clone()
    }

    fn interrupts(&self) -> usize {
        self.interrupts.load(Ordering::SeqCst)
    }

    fn notify(&self, event: HttpEvent) {
        if let Some(tx) = self.notify.lock().unwrap().as_ref() {
            let _ = tx.send(event);
        }
    }

    /// 记一条 send 并唤醒 WS 侧。
    fn on_send(&self, content: String) {
        let seq = {
            let mut sends = self.sends.lock().unwrap();
            sends.push(content);
            sends.len()
        };
        self.notify(HttpEvent::Send { seq });
    }
}

// ============================================================
// 假网关：HTTP/1.1 + WS 升级
// ============================================================

async fn serve(listener: TcpListener, gateway: Arc<FakeGateway>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(handle_conn(stream, Arc::clone(&gateway)));
    }
}

async fn handle_conn(mut stream: TcpStream, gateway: Arc<FakeGateway>) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some((head, body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        let target = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        let path = target.split('?').next().unwrap_or("").to_string();

        let is_upgrade = header_value(&head, "upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
        if path == "/ws" && is_upgrade {
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
            let ws = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
            // 本次连接独占通知口（一个测试一个客户端连接）。
            let events = gateway.events.lock().unwrap().take();
            match events {
                Some(events) => drive_ws(ws, Arc::clone(&gateway), events).await,
                None => return,
            }
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
            "/api/session/send" => {
                let content = serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|v| v["content"].as_str().map(str::to_string))
                    .unwrap_or_default();
                gateway.on_send(content);
                r#"{"ok":true,"request_id":"fake-req"}"#.to_string()
            }
            "/api/session/interrupt" => {
                gateway.interrupts.fetch_add(1, Ordering::SeqCst);
                gateway.notify(HttpEvent::Interrupt);
                r#"{"ok":true}"#.to_string()
            }
            // subscribe / unsubscribe 等：无需特判，回通用 ok。
            _ => r#"{"ok":true}"#.to_string(),
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

/// 完成客户端握手，然后按场景推事件。
async fn drive_ws(
    mut ws: WebSocketStream<TcpStream>,
    gateway: Arc<FakeGateway>,
    mut events: mpsc::UnboundedReceiver<HttpEvent>,
) {
    if ws
        .send(Message::Text(
            json!({"type": "connected", "client_id": "fake"})
                .to_string()
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    let mut started = false;
    loop {
        tokio::select! {
            Some(event) = events.recv() => {
                match (gateway.scenario, event) {
                    (Scenario::Turns, HttpEvent::Send { seq }) => {
                        emit_turn(&mut ws, seq).await;
                    }
                    (Scenario::SlowFirstTurn, HttpEvent::Send { seq }) => {
                        if seq == 1 {
                            // 轮起了、但 result 迟到：给测试留出「EOF 落在轮中」的窗口。
                            emit(&mut ws, turn_started_event()).await;
                            emit(&mut ws, assistant_event("slow")).await;
                            tokio::time::sleep(Duration::from_millis(400)).await;
                            emit(&mut ws, result_event("turn-1", false)).await;
                        } else {
                            emit_turn(&mut ws, seq).await;
                        }
                    }
                    (Scenario::Interrupt, HttpEvent::Send { .. }) => {
                        if !started {
                            started = true;
                            emit(&mut ws, turn_started_event()).await;
                            emit(&mut ws, assistant_event("cut")).await;
                        }
                    }
                    (Scenario::Interrupt, HttpEvent::Interrupt) => {
                        emit(&mut ws, interrupted_event()).await;
                    }
                    (Scenario::InTurnForward, HttpEvent::Send { seq }) => {
                        if seq == 1 {
                            emit(&mut ws, turn_started_event()).await;
                            emit(&mut ws, assistant_event("long")).await;
                        } else {
                            // 第二条消息到达 = 第一轮可以收口了。
                            emit(&mut ws, result_event("turn-1", false)).await;
                        }
                    }
                    (Scenario::ErrorThenSuccess, HttpEvent::Send { seq }) => {
                        emit(&mut ws, turn_started_event()).await;
                        if seq == 1 {
                            emit(&mut ws, result_event("", true)).await;
                        } else {
                            emit(&mut ws, result_event(&format!("turn-{seq}"), false)).await;
                        }
                    }
                    (Scenario::ErrorTurn, HttpEvent::Send { .. }) => {
                        emit(&mut ws, turn_started_event()).await;
                        emit(&mut ws, result_event("", true)).await;
                    }
                    // 其余组合（如 Turns 场景收到 Interrupt）：不推事件。
                    _ => {}
                }
            }
            msg = ws.next() => match msg {
                // 客户端在 stdio 模式不走 WS 上行（消息走 HTTP）；连接结束即收摊。
                None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn emit(ws: &mut WebSocketStream<TcpStream>, event: Value) {
    let _ = ws.send(Message::Text(event.to_string().into())).await;
}

/// 一轮的完整事件序列（turn_started → assistant → result）。
async fn emit_turn(ws: &mut WebSocketStream<TcpStream>, seq: usize) {
    emit(ws, turn_started_event()).await;
    emit(ws, assistant_event(&format!("turn-{seq}"))).await;
    emit(ws, result_event(&format!("turn-{seq}"), false)).await;
}

// ============================================================
// 假事件构造（形状与真实网关一致）
// ============================================================

fn turn_started_event() -> Value {
    json!({
        "type": "turn_started",
        "session_id": SESSION_ID,
        "created_at": "2026-01-01T00:00:00+00:00",
        "request_id": "fake",
    })
}

fn assistant_event(text: &str) -> Value {
    json!({
        "type": "assistant_turn",
        "uuid": "a-1",
        "content_blocks": [{"type": "text", "text": text}],
        "model": "fake-model",
        "stop_reason": "end_turn",
        "session_id": SESSION_ID,
        "created_at": "2026-01-01T00:00:00+00:00",
        "request_id": "fake",
    })
}

fn result_event(text: &str, is_error: bool) -> Value {
    let mut event = json!({
        "type": "turn_result",
        "uuid": "r-1",
        "subtype": if is_error { "error_during_execution" } else { "success" },
        "is_error": is_error,
        "num_turns": 1,
        "duration_ms": 5,
        "session_id": SESSION_ID,
        "created_at": "2026-01-01T00:00:00+00:00",
        "request_id": "fake",
    });
    if is_error {
        event["errors"] = json!(["boom"]);
    } else {
        event["result"] = json!(text);
    }
    event
}

fn interrupted_event() -> Value {
    json!({
        "type": "interrupted",
        "dropped_request_ids": [],
        "session_id": SESSION_ID,
        "created_at": "2026-01-01T00:00:00+00:00",
        "request_id": "fake",
    })
}

// ============================================================
// HTTP/1.1 解析（与 wait_fail_fast_e2e 同一实现）
// ============================================================

/// Read one HTTP request head (+ body) from `stream`, buffering leftovers.
async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
    loop {
        if let Some(pos) = find(buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let content_length = header_value(&head, "content-length")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = pos + 4;
            if buf.len() >= body_start + content_length {
                let body = buf[body_start..body_start + content_length].to_vec();
                buf.drain(..body_start + content_length);
                return Some((head, body));
            }
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Case-insensitive header lookup over the raw request head.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

// ============================================================
// 测试 harness
// ============================================================

fn scratch_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wing-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("core")).unwrap();
    dir
}

/// 起假网关，并把一个全新 `$WING_HOME` 指向它。
async fn start_gateway(scenario: Scenario, tag: &str) -> (PathBuf, Arc<FakeGateway>, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let (notify_tx, notify_rx) = mpsc::unbounded_channel();
    let gateway = Arc::new(FakeGateway {
        scenario,
        sends: Mutex::new(Vec::new()),
        interrupts: AtomicUsize::new(0),
        notify: Mutex::new(Some(notify_tx)),
        events: Mutex::new(Some(notify_rx)),
    });
    tokio::spawn(serve(listener, Arc::clone(&gateway)));

    let home = scratch_home(tag);
    std::fs::write(
        home.join("core").join("config.yaml"),
        format!("gateway:\n  host: 127.0.0.1\n  port: {port}\n"),
    )
    .unwrap();
    (home, gateway, port)
}

/// 被测进程（真实 `wing` 二进制，常驻 stdio 形态）。
struct WingProcess {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: BufReader<tokio::process::ChildStdout>,
    stderr: BufReader<tokio::process::ChildStderr>,
    /// 已读到的 stdout 帧（累积，供多条件断言复用）。
    frames: Vec<Value>,
}

impl WingProcess {
    /// 常驻 stdio（stream-json in + out）形态。
    async fn spawn(home: &PathBuf) -> Self {
        Self::spawn_with(home, &[]).await
    }

    /// 额外 CLI 参数（如 `-p`）叠加在常驻形态之上。
    async fn spawn_with(home: &PathBuf, extra: &[&str]) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"))
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ])
            .args(extra)
            .env("WING_HOME", home)
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let stderr = BufReader::new(child.stderr.take().unwrap());
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
            frames: Vec::new(),
        }
    }

    /// 投一条 stdin NDJSON 帧。
    async fn write_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        stdin.write_all(line.as_bytes()).await.unwrap();
        stdin.write_all(b"\n").await.unwrap();
        stdin.flush().await.unwrap();
    }

    /// 关掉 stdin（EOF 收尾信号）。
    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// 读 stdout 直到出现满足 `pred` 的**新**帧；返回该帧。
    async fn read_frame(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(found) = self.frames.iter().find(|f| pred(f)) {
            return found.clone();
        }
        let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout_at(deadline, self.stdout.read_line(&mut line))
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for {what}; frames so far: {:?}",
                        self.frames
                    )
                })
                .unwrap();
            if read == 0 {
                // stdout 提前关闭 = 进程退出了：把 stderr 一起带进诊断。
                let stderr = self.drain_stderr().await;
                panic!(
                    "stdout closed while waiting for {what}; frames: {:?}\nstderr: {stderr}",
                    self.frames
                );
            }
            let value: Value = serde_json::from_str(line.trim_end())
                .unwrap_or_else(|e| panic!("non-JSON stdout line ({e}): {line}"));
            self.frames.push(value.clone());
            if pred(&value) {
                return value;
            }
        }
    }

    /// 进程是否仍存活（终态帧之后不得提前退出）。
    fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// 读干 stderr（进程退出后立即 EOF；有界等待，避免诊断本身挂住）。
    async fn drain_stderr(&mut self) -> String {
        let mut stderr = String::new();
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.stderr.read_to_string(&mut stderr),
        )
        .await;
        stderr
    }

    /// 等进程退出并断言退出码（stdin 已被幂等关闭）。
    async fn expect_exit(&mut self, code: i32) {
        self.stdin = None;
        let status = tokio::time::timeout(EXIT_TIMEOUT, self.child.wait())
            .await
            .unwrap_or_else(|_| panic!("process did not exit; frames so far: {:?}", self.frames))
            .unwrap();

        let stderr = self.drain_stderr().await;
        assert_eq!(
            status.code(),
            Some(code),
            "unexpected exit code\nstderr: {stderr}\nframes: {:?}",
            self.frames
        );
    }
}

// ============================================================
// stdin 帧构造
// ============================================================

fn initialize_frame() -> String {
    json!({
        "type": "control_request",
        "request_id": "init-1",
        "request": {"subtype": "initialize", "hooks": null, "agents": null},
    })
    .to_string()
}

fn user_frame(content: &str) -> String {
    json!({
        "type": "user",
        "message": {"role": "user", "content": content},
    })
    .to_string()
}

fn interrupt_frame() -> String {
    json!({
        "type": "control_request",
        "request_id": "int-1",
        "request": {"subtype": "interrupt"},
    })
    .to_string()
}

fn is_result(frame: &Value) -> bool {
    frame["type"] == "result"
}

fn cleanup(home: &PathBuf) {
    let _ = std::fs::remove_dir_all(home);
}

// ============================================================
// 测试
// ============================================================

/// a) + b) 多轮：两条 result 依次到达、之间进程不退出；EOF 后退出码 = 0。
#[tokio::test]
async fn resident_serves_multiple_turns_and_exits_on_eof() {
    let (home, gateway, _port) = start_gateway(Scenario::Turns, "resident-turns").await;
    let mut wing = WingProcess::spawn(&home).await;

    wing.write_line(&initialize_frame()).await;
    wing.write_line(&user_frame("first")).await;

    // 第一轮结束 ≠ 进程结束。
    let r1 = wing.read_frame("result #1", is_result).await;
    assert_eq!(r1["subtype"], "success");
    assert_eq!(r1["result"], "turn-1");
    assert!(wing.is_running(), "常驻进程不得在第一条 result 后退出");

    // 第二轮：轮间消息同样转发并产出自己的 result。
    wing.write_line(&user_frame("second")).await;
    let r2 = wing
        .read_frame("result #2", |f| is_result(f) && f["result"] == "turn-2")
        .await;
    assert_eq!(r2["subtype"], "success");

    // 顺序：result#1 先于 result#2；总数恰好两条（不多不少）。
    let results: Vec<&Value> = wing.frames.iter().filter(|f| is_result(f)).collect();
    assert_eq!(results.len(), 2, "两轮各一条 result：{:?}", wing.frames);
    assert_eq!(results[0]["result"], "turn-1");
    assert_eq!(results[1]["result"], "turn-2");

    // 假网关侧看到两条消息（首轮 prompt + 第二轮转发）。
    assert_eq!(
        gateway.sends(),
        vec!["first".to_string(), "second".to_string()]
    );

    // EOF 收尾：退出码 = 最后一轮（成功 = 0）。
    wing.close_stdin();
    wing.expect_exit(0).await;
    cleanup(&home);
}

/// b) EOF 落在轮中：等该轮终态后才退出，迟到的 result 仍然产出。
#[tokio::test]
async fn resident_eof_waits_for_the_in_flight_turn() {
    let (home, _gateway, _port) = start_gateway(Scenario::SlowFirstTurn, "resident-slow").await;
    let mut wing = WingProcess::spawn(&home).await;

    wing.write_line(&user_frame("slow")).await;
    // 轮已开始（assistant 已到），终态还没来。
    wing.read_frame("assistant frame", |f| f["type"] == "assistant")
        .await;

    // 关 stdin：在途轮必须等到它的终态，迟到的 result 仍要写出去。
    wing.close_stdin();
    let result = wing
        .read_frame("result of the in-flight turn", is_result)
        .await;
    assert_eq!(result["result"], "turn-1");
    assert_eq!(result["subtype"], "success");

    wing.expect_exit(0).await;
    cleanup(&home);
}

/// c) 中断形状（CloudCLI）：interrupt → interrupted → 合成终态帧 +
/// control_response；stdin 释放后退出（退出码 0）。
#[tokio::test]
async fn resident_interrupt_synthesizes_terminal_frame_and_exits_on_release() {
    let (home, gateway, _port) = start_gateway(Scenario::Interrupt, "resident-interrupt").await;
    let mut wing = WingProcess::spawn(&home).await;

    wing.write_line(&initialize_frame()).await;
    wing.write_line(&user_frame("cut me short")).await;
    wing.read_frame("assistant frame", |f| f["type"] == "assistant")
        .await;

    // 编排器的「停止」按钮：control_request{interrupt}（SDK 在 await 应答）。
    wing.write_line(&interrupt_frame()).await;

    // 应答纪律：control_response 必须回 success + 原 request_id（initialize
    // 的应答也在流里，按 request_id 区分）。
    let response = wing
        .read_frame("control_response for int-1", |f| {
            f["type"] == "control_response" && f["response"]["request_id"] == "int-1"
        })
        .await;
    assert_eq!(response["response"]["subtype"], "success");
    assert_eq!(response["response"]["request_id"], "int-1");

    // 终态帧纪律：interrupted 不发 turn_result，前端补一条合成 result。
    let terminal = wing
        .read_frame("synthesized terminal frame", is_result)
        .await;
    assert_eq!(terminal["subtype"], "error_during_execution");
    assert_eq!(terminal["is_error"], true);
    assert_eq!(terminal["terminal_reason"], "aborted_streaming");
    assert!(terminal.get("result").is_none(), "{terminal}");
    assert_eq!(gateway.interrupts(), 1, "中断必须打到网关");

    // release：关 stdin → 退出（中止是编排器主动动作 → 退出码 0）。
    wing.close_stdin();
    wing.expect_exit(0).await;
    cleanup(&home);
}

/// d) 轮中消息：turn 未结束时再投 `user` → 转发 `POST /api/session/send`。
#[tokio::test]
async fn resident_forwards_in_turn_user_messages() {
    let (home, gateway, _port) = start_gateway(Scenario::InTurnForward, "resident-in-turn").await;
    let mut wing = WingProcess::spawn(&home).await;

    wing.write_line(&user_frame("first")).await;
    wing.read_frame("assistant frame", |f| f["type"] == "assistant")
        .await;

    // 轮还没结束：这条消息不得丢弃，必须转发（网关 inbox 决定 steer / 排队）。
    wing.write_line(&user_frame("while busy")).await;

    // 假网关收到第二条 send 才推第一轮的 result——它到了即证明转发成功。
    let result = wing.read_frame("result of turn 1", is_result).await;
    assert_eq!(result["result"], "turn-1");

    assert_eq!(
        gateway.sends(),
        vec!["first".to_string(), "while busy".to_string()],
        "轮中消息必须逐字转发到 POST /api/session/send"
    );

    wing.close_stdin();
    wing.expect_exit(0).await;
    cleanup(&home);
}

/// e) 退出码口径：最后一轮结果说了算（先失败后成功 = 0；最后一轮失败 ≠ 0）。
#[tokio::test]
async fn resident_exit_code_follows_the_last_turn() {
    // 先失败后成功 → 0。
    let (home, _gateway, _port) =
        start_gateway(Scenario::ErrorThenSuccess, "resident-exit-0").await;
    let mut wing = WingProcess::spawn(&home).await;

    wing.write_line(&user_frame("fails")).await;
    let failed = wing.read_frame("failed result", is_result).await;
    assert_eq!(failed["is_error"], true);

    wing.write_line(&user_frame("succeeds")).await;
    let ok = wing
        .read_frame("successful result", |f| {
            is_result(f) && f["is_error"] == false
        })
        .await;
    assert_eq!(ok["result"], "turn-2");

    wing.close_stdin();
    wing.expect_exit(0).await;
    cleanup(&home);

    // 最后一轮失败 → 非零。
    let (home, _gateway, _port) = start_gateway(Scenario::ErrorTurn, "resident-exit-1").await;
    let mut wing = WingProcess::spawn(&home).await;

    wing.write_line(&user_frame("fails")).await;
    let failed = wing.read_frame("failed result", is_result).await;
    assert_eq!(failed["is_error"], true);
    assert_eq!(failed["subtype"], "error_during_execution");

    wing.close_stdin();
    wing.expect_exit(1).await;
    cleanup(&home);
}

/// 空消息不得造成挂死：空文本在后端不驱动任何一轮（`run_turn` 直接 return、
/// 不发终态帧）——常驻模式跳过它的发送/转发，EOF 立即退出。
#[tokio::test]
async fn resident_skips_empty_messages_and_exits_on_eof() {
    let (home, gateway, _port) = start_gateway(Scenario::Turns, "resident-empty").await;
    let mut wing = WingProcess::spawn(&home).await;

    // 首条 = 空 prompt（如只有非文本 block 的消息）；第二条 = 轮间空消息。
    wing.write_line(&user_frame("")).await;
    wing.write_line(&user_frame("")).await;
    wing.close_stdin();

    wing.expect_exit(0).await;
    assert!(
        gateway.sends().is_empty(),
        "空消息不得转发（它们永远不会成轮）：{:?}",
        gateway.sends()
    );
    cleanup(&home);
}

/// `-p` + stream-json 组合：CLI prompt 是首轮，stdin 后续消息仍成新轮，
/// EOF 退出。
#[tokio::test]
async fn resident_with_cli_prompt_keeps_serving_stdin_turns() {
    let (home, gateway, _port) = start_gateway(Scenario::Turns, "resident-cli-prompt").await;
    let mut wing = WingProcess::spawn_with(&home, &["-p", "from cli"]).await;

    // 首轮 = CLI prompt（不等 stdin）。
    let r1 = wing.read_frame("result #1", is_result).await;
    assert_eq!(r1["result"], "turn-1");
    assert!(wing.is_running(), "常驻：CLI prompt 轮结束后进程仍在");

    // stdin 消息成新轮。
    wing.write_line(&user_frame("from stdin")).await;
    let r2 = wing
        .read_frame("result #2", |f| is_result(f) && f["result"] == "turn-2")
        .await;
    assert_eq!(r2["result"], "turn-2");

    assert_eq!(
        gateway.sends(),
        vec!["from cli".to_string(), "from stdin".to_string()]
    );

    wing.close_stdin();
    wing.expect_exit(0).await;
    cleanup(&home);
}
