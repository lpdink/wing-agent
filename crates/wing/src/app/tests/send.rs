//! 发送链路（`runner::deliver_user_message` / `dispatch_send_message`）的组件测试：
//! 上传 → 引用 → 发送，以及失败契约。
//!
//! 进程内假网关：WS 端口用 `tokio-tungstenite::accept_async`（首帧发 `ConnectResponse`，
//! 之后原样收上行帧），HTTP 端口是手写的 HTTP/1.1 假服务（同 `wing-api-client` 单测的手法，
//! 零新依赖）。两个端口独立——`GatewayEndpoint` 的 `ws_url` / `http_base` 本来就分开。
//!
//! 这里测的是**线格式**：上传请求的路径 / 原始字节 / content-type，WS 帧里的 `media` 引用，
//! 以及任一步失败时"整条不发"。进生产的路径是 `dispatch_send_message`（后台任务 + 失败
//! 回执），测试按 `run_app` 的接线把回执喂回 `handle_fetch_result`。

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::SinkExt;
use futures_util::StreamExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::support::*;
use crate::app::intent::FetchPayload;
use crate::app::intent::FetchResult;
use crate::app::transport::GatewayEndpoint;
use crate::app::transport::Transport;
use crate::app::transport::connect_transport;
use crate::ui::chat_view::Submission;
use crate::ui::toast::ToastKind;
use crate::util::image_compress::CompressedImage;

/// 一张伪造的压缩产物（上传链路不解析字节，内容任意）。
fn image(byte: u8, len: usize) -> CompressedImage {
    CompressedImage {
        bytes: vec![byte; len],
        width: 8,
        height: 8,
    }
}

/// 假网关收到的一次上传。
#[derive(Debug, Clone)]
struct Upload {
    path: String,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// 上传端点的应答方式。
#[derive(Debug, Clone)]
enum UploadRoute {
    /// 200 + `{"ok":true,"media":{…}}`（`bytes` 回显请求体长度、`width/height` 固定 8）。
    Ok,
    /// 同 `Ok`，但每次应答先等 `delay_ms`（慢链路：把「后台发送不阻塞调用方」与
    /// 「两条快速提交保持顺序」的时序差做成确定性的）。
    OkDelayed { delay_ms: u64 },
    /// 前 `after_ok` 次成功，之后按 `(status, body)` 失败。
    FailAfter {
        after_ok: usize,
        status: u16,
        body: &'static str,
    },
}

/// 进程内假网关（WS + HTTP 两个临时端口）。
struct FakeGateway {
    ws_url: String,
    http_base: String,
    frames: Arc<Mutex<Vec<String>>>,
    uploads: Arc<Mutex<Vec<Upload>>>,
    route: Arc<Mutex<UploadRoute>>,
}

impl FakeGateway {
    async fn start() -> Self {
        Self::start_with_ws(true).await
    }

    /// WS 在握手后立刻发 Close 并断开（驱动"发不出去"的失败契约）。
    async fn start_with_a_dead_ws() -> Self {
        Self::start_with_ws(false).await
    }

    async fn start_with_ws(keep_ws_alive: bool) -> Self {
        let frames: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let uploads: Arc<Mutex<Vec<Upload>>> = Arc::new(Mutex::new(Vec::new()));
        let route = Arc::new(Mutex::new(UploadRoute::Ok));

        // ---- WS ----
        let ws_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ws");
        let ws_url = format!("ws://{}/ws", ws_listener.local_addr().unwrap());
        {
            let sink = Arc::clone(&frames);
            tokio::spawn(async move {
                while let Ok((stream, _)) = ws_listener.accept().await {
                    let sink = Arc::clone(&sink);
                    tokio::spawn(async move {
                        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        // 握手：第一条出站帧必须是 ConnectResponse。
                        let hello = r#"{"type":"connected","client_id":"fake-gw"}"#;
                        if ws.send(Message::Text(hello.into())).await.is_err() {
                            return;
                        }
                        if !keep_ws_alive {
                            // 死连接的确定性造法：Close 帧 + 立刻断开。
                            let _ = ws.send(Message::Close(None)).await;
                            let _ = ws.close(None).await;
                            return;
                        }
                        while let Some(Ok(message)) = ws.next().await {
                            if let Message::Text(text) = message {
                                // 原样记录（golden 断言要比对**原始字节**，不是 Value 的重排）。
                                sink.lock().expect("frames").push(text.to_string());
                            }
                        }
                    });
                }
            });
        }

        // ---- HTTP ----
        let http_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind http");
        let http_base = format!("http://{}", http_listener.local_addr().unwrap());
        {
            let uploads = Arc::clone(&uploads);
            let route = Arc::clone(&route);
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = http_listener.accept().await {
                    let uploads = Arc::clone(&uploads);
                    let route = Arc::clone(&route);
                    tokio::spawn(async move {
                        let Some((head, body)) = read_request(&mut stream).await else {
                            return;
                        };
                        let (status, payload) = {
                            let target = request_target(&head);
                            uploads.lock().expect("uploads").push(Upload {
                                path: target.clone(),
                                content_type: header_value(&head, "content-type"),
                                body: body.clone(),
                            });
                            match route.lock().expect("route").clone() {
                                UploadRoute::Ok | UploadRoute::OkDelayed { .. } => {
                                    let id =
                                        format!("{:064x}", 0xAB_C0FFEE_u64 + body.len() as u64);
                                    let media = format!(
                                        r#"{{"id":"{id}","mime":"image/jpeg","bytes":{},"width":8,"height":8}}"#,
                                        body.len()
                                    );
                                    (200, format!(r#"{{"ok":true,"media":{media}}}"#))
                                }
                                UploadRoute::FailAfter {
                                    after_ok,
                                    status,
                                    body: text,
                                } => {
                                    let seen = uploads.lock().expect("uploads").len() - 1;
                                    if seen < after_ok {
                                        let media = format!(
                                            r#"{{"id":"{:064x}","mime":"image/jpeg","bytes":{},"width":8,"height":8}}"#,
                                            0xAB_C0FFEE_u64 + seen as u64,
                                            body.len()
                                        );
                                        (200, format!(r#"{{"ok":true,"media":{media}}}"#))
                                    } else {
                                        (status, text.to_string())
                                    }
                                }
                            }
                        };
                        // 慢链路：应答之前先压住（guard 不跨 await）。
                        let delay = match *route.lock().expect("route") {
                            UploadRoute::OkDelayed { delay_ms } => Some(delay_ms),
                            UploadRoute::Ok | UploadRoute::FailAfter { .. } => None,
                        };
                        if let Some(delay_ms) = delay {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                        let response = format!(
                            "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{payload}",
                            if status == 200 { "OK" } else { "Error" },
                            payload.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.flush().await;
                        let _ = stream.shutdown().await;
                    });
                }
            });
        }

        Self {
            ws_url,
            http_base,
            frames,
            uploads,
            route,
        }
    }

    /// 上传端点固定成功（默认）或「前 N 次成功后失败」。
    fn fail_uploads_after(&self, after_ok: usize, status: u16, body: &'static str) {
        *self.route.lock().expect("route") = UploadRoute::FailAfter {
            after_ok,
            status,
            body,
        };
    }

    /// 每次上传应答前先压 `delay_ms`（慢链路）。
    fn delay_uploads(&self, delay_ms: u64) {
        *self.route.lock().expect("route") = UploadRoute::OkDelayed { delay_ms };
    }

    fn endpoint(&self) -> GatewayEndpoint {
        GatewayEndpoint {
            ws_url: self.ws_url.clone(),
            http_base: self.http_base.clone(),
            api_key: None,
        }
    }

    fn frames(&self) -> Vec<String> {
        self.frames.lock().expect("frames").clone()
    }

    fn uploads(&self) -> Vec<Upload> {
        self.uploads.lock().expect("uploads").clone()
    }
}

/// 读到请求头 + 按 `Content-Length` 收齐 body。
async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let body_start = head_end + 4;
    let content_length = header_value(&head, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < body_start + content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body_end = (body_start + content_length).min(buf.len());
    Some((head, buf[body_start..body_end].to_vec()))
}

/// 请求行里的 request-target（含 query）。
fn request_target(head: &str) -> String {
    head.split("\r\n")
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .unwrap_or_default()
        .to_string()
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.split("\r\n").skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

/// 建 transport（真实 WS 握手 + HTTP client，指向假网关）。
async fn connect(gateway: &FakeGateway) -> Transport {
    connect_transport(&gateway.endpoint())
        .await
        .expect("connect to the fake gateway")
}

/// 等 HTTP 侧收到 `n` 次上传请求（后台任务推进的观测点）。
async fn wait_for_uploads(gateway: &FakeGateway, n: usize) -> Vec<Upload> {
    for _ in 0..400 {
        let uploads = gateway.uploads();
        if uploads.len() >= n {
            return uploads;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("假网关没有在预期时间内收到 {n} 次上传");
}

/// 等 WS 侧收到 `n` 帧。
///
/// 发送是异步的：`send_message` 只是把请求投进写通道，真正的帧要经写任务 + 服务端
/// 各一次调度才落到假网关的记录里——断言前必须有界等待，不能假设"调用返回 = 已送达"。
async fn wait_for_frames(gateway: &FakeGateway, n: usize) -> Vec<String> {
    for _ in 0..400 {
        let frames = gateway.frames();
        if frames.len() >= n {
            return frames;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("假网关没有在预期时间内收到 {n} 帧");
}

/// 一条提交的派发（生产的 `dispatch_send_message` 入口）：先挂 pending（与 submit 侧同构），
/// 再交给后台任务链。返回回执接收端——成功路径恒为空，失败路径有且只有一条。
///
/// `chain` 由调用方持有：需要观察「后台任务还没跑完 / 两条提交保序」的用例显式传它；
/// drop 句柄不等于取消任务（tokio 任务在句柄 drop 后照跑）。
fn dispatch(
    app: &mut crate::app::App,
    transport: Option<&Transport>,
    chain: &mut Option<tokio::task::JoinHandle<()>>,
    request_id: &str,
    content: &str,
    images: Vec<CompressedImage>,
) -> mpsc::Receiver<FetchResult> {
    app.chat.push_pending(
        request_id.into(),
        Submission {
            content: content.to_string(),
            images: images.clone(),
        },
    );
    let (tx, rx) = mpsc::channel(8);
    super::super::runner::dispatch_send_message(
        app,
        transport,
        chain,
        super::super::runner::OutgoingMessage {
            session_id: "test-session".into(),
            content: content.to_string(),
            tool_call_id: None,
            request_id: request_id.into(),
            images,
        },
        tx,
    );
    rx
}

/// 单条提交的口袋版派发（`req-1`，自己持链尾）。
fn send(
    app: &mut crate::app::App,
    transport: Option<&Transport>,
    content: &str,
    images: Vec<CompressedImage>,
) -> mpsc::Receiver<FetchResult> {
    let mut chain = None;
    dispatch(app, transport, &mut chain, "req-1", content, images)
}

/// 等一条失败回执（后台任务的失败路径；成功路径没有回执）。
async fn wait_for_receipt(rx: &mut mpsc::Receiver<FetchResult>) -> FetchResult {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("失败回执必须到达")
        .expect("回执通道还开着")
}

#[tokio::test]
async fn images_are_uploaded_in_order_and_ride_the_ws_frame() {
    let gateway = FakeGateway::start().await;
    let transport = connect(&gateway).await;
    let mut app = test_app();

    let images = vec![image(0xAA, 300), image(0xBB, 120)];
    let mut rx = send(&mut app, Some(&transport), "看这两张", images);

    // WS：一条消息帧，带两条引用（顺序 = 提交顺序）。帧到达 = 两次上传已完成
    // （后台任务是「先逐张上传再发帧」，所以从帧反查上传不留竞态窗）。
    let frames = wait_for_frames(&gateway, 1).await;
    assert_eq!(frames.len(), 1);

    // 上传：按提交顺序、原始字节、无 base64（`application/octet-stream`）。
    let uploads = gateway.uploads();
    assert_eq!(uploads.len(), 2, "两张图 = 两次上传");
    assert_eq!(
        uploads[0].path,
        "/api/session/media?session_id=test-session"
    );
    assert_eq!(uploads[0].body, vec![0xAA; 300]);
    assert_eq!(
        uploads[1].path,
        "/api/session/media?session_id=test-session"
    );
    assert_eq!(uploads[1].body, vec![0xBB; 120]);
    for upload in &uploads {
        assert_eq!(
            upload.content_type.as_deref(),
            Some("application/octet-stream")
        );
    }
    let frame: serde_json::Value = serde_json::from_str(&frames[0]).expect("frame is JSON");
    assert_eq!(frame["request_id"], "req-1");
    assert_eq!(frame["session_id"], "test-session");
    assert_eq!(frame["content"], "看这两张");
    let media = frame["media"].as_array().expect("media array");
    assert_eq!(media.len(), 2);
    for (index, entry) in media.iter().enumerate() {
        assert_eq!(entry["mime"], "image/jpeg");
        assert!(entry["id"].as_str().is_some_and(|id| id.len() == 64));
        assert_eq!(
            entry["bytes"].as_u64(),
            Some(uploads[index].body.len() as u64),
            "引用回显的是该次上传的产物"
        );
    }

    // 成功路径不动 pending：条目等后端的 `user_message_accepted` 才提升（fake 网关不发）。
    assert_eq!(app.chat.pending.len(), 1, "成功发送不丢 pending 条目");
    assert!(app.toast.is_none(), "成功路径不 toast");
    assert!(rx.try_recv().is_err(), "成功路径没有回执");
}

/// 纯文本路径的兼容红线：帧与引入图片之前**逐字节相同**（没有 `media` 键），也不产生上传。
#[tokio::test]
async fn a_text_only_message_is_the_old_frame_without_an_upload() {
    let gateway = FakeGateway::start().await;
    let transport = connect(&gateway).await;
    let mut app = test_app();

    let mut rx = send(&mut app, Some(&transport), "hello", Vec::new());

    let frames = wait_for_frames(&gateway, 1).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0], r#"{"request_id":"req-1","session_id":"test-session","content":"hello"}"#,
        "无 media 时不产生新键（旧网关逐字节兼容）"
    );
    assert!(gateway.uploads().is_empty(), "纯文本不上传");
    assert_eq!(app.chat.pending.len(), 1, "成功发送不丢 pending 条目");
    assert!(rx.try_recv().is_err(), "成功路径没有回执");
}

/// 上传失败：整条不发（不发送半截）、pending 取回、内容回到草稿、warning toast 带原因。
#[tokio::test]
async fn an_upload_failure_sends_nothing_and_gives_the_draft_back() {
    let gateway = FakeGateway::start().await;
    gateway.fail_uploads_after(0, 400, r#"{"error":"bad_media","detail":"not an image"}"#);
    let transport = connect(&gateway).await;
    let mut app = test_app();

    let mut rx = send(&mut app, Some(&transport), "图呢", vec![image(0x01, 10)]);
    let receipt = wait_for_receipt(&mut rx).await;
    app.handle_fetch_result(receipt);

    assert!(gateway.frames().is_empty(), "半截消息绝不发送");
    assert!(app.chat.pending.is_empty(), "pending 条目取回");
    assert_eq!(
        app.input.text(),
        "图呢[Image #1 8×8 10B]",
        "文本与图片 chip 都回到草稿"
    );
    let toast = app.toast.as_ref().expect("failure toast");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert!(
        toast.message.contains("Image upload failed"),
        "{}",
        toast.message
    );
    assert!(toast.message.contains("not an image"), "{}", toast.message);
    assert!(
        toast.message.contains("draft restored"),
        "{}",
        toast.message
    );
    assert_restore_is_pure(&mut app);
}

/// 多张图中的第 2 张失败：第 3 张不再上传，整条不发（顺序语义 + 不留半截），
/// 三张**全部**回到草稿（发出去的半截不成立，用户拿到的是完整原件）。
#[tokio::test]
async fn a_failure_stops_the_remaining_uploads_and_the_send() {
    let gateway = FakeGateway::start().await;
    gateway.fail_uploads_after(1, 400, r#"{"error":"bad_media","detail":"too large"}"#);
    let transport = connect(&gateway).await;
    let mut app = test_app();

    let images = vec![image(0x01, 10), image(0x02, 20), image(0x03, 30)];
    let mut rx = send(&mut app, Some(&transport), "三张", images);
    let receipt = wait_for_receipt(&mut rx).await;
    app.handle_fetch_result(receipt);

    let uploads = gateway.uploads();
    assert_eq!(uploads.len(), 2, "第 3 张不再尝试：失败就停");
    assert!(gateway.frames().is_empty(), "整条不发");
    assert!(app.chat.pending.is_empty());
    assert_eq!(
        app.input.text(),
        "三张[Image #1 8×8 10B][Image #2 8×8 20B][Image #3 8×8 30B]",
        "失败不吞图：整条原件回到草稿"
    );
    assert_restore_is_pure(&mut app);
}

/// 网关断开（transport 为 None）：不上传、不发送，文本与图片回到草稿 + toast 说明。
#[tokio::test]
async fn a_disconnected_gateway_gives_the_message_back() {
    let mut app = test_app();
    // 无连接 = 纯本地回填：派发同步做完，不 spawn、无回执。
    let mut rx = send(&mut app, None, "hello", vec![image(0x01, 10)]);
    assert!(rx.try_recv().is_err(), "无连接不产生回执");

    assert!(app.chat.pending.is_empty());
    assert_eq!(app.input.text(), "hello[Image #1 8×8 10B]", "内容回到草稿");
    let toast = app.toast.as_ref().expect("failure toast");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert_eq!(
        toast.message,
        "Not sent — gateway disconnected; draft restored"
    );
    assert_restore_is_pure(&mut app);
}

/// 无媒体的断连路径（toast 保留旧子句 + 说明内容去向）：纯文本也走同一条回填路径，
/// 文本原样回到草稿。
#[tokio::test]
async fn a_text_only_send_also_gives_the_text_back() {
    let mut app = test_app();
    send(&mut app, None, "hello", Vec::new());
    let toast = app.toast.as_ref().expect("failure toast");
    assert_eq!(
        toast.message,
        "Not sent — gateway disconnected; draft restored"
    );
    assert_eq!(app.input.text(), "hello");
}

/// 回填时草稿里已经有东西（用户在消息在飞的那几秒里又打了字）：**不覆盖**——
/// 失败的消息追加在末尾，草稿里原有的内容（含 chip）一根不动。
#[tokio::test]
async fn a_refill_appends_after_a_draft_the_user_retyped_in() {
    let mut app = test_app();
    app.submit_message(crate::ui::input_area::SubmitPayload {
        text: "第一句".into(),
        images: vec![crate::ui::input_area::ImagePayload {
            image: image(0x07, 12),
            label: "[Image #1 8×8 12B]".into(),
        }],
    });
    app.drain_intents();
    // 消息在飞：用户又写了一句（光标在草稿中间，回填必须落到整份草稿末尾）。
    app.input.set_text("第二句");
    app.input.insert_str("\n接着写");
    app.input.cursor_row = 0;
    app.input.cursor_col = 0;

    send(&mut app, None, "第一句", vec![image(0x07, 12)]);

    assert_eq!(
        app.input.text(),
        "第二句\n接着写\n第一句[Image #1 8×8 12B]",
        "用户新写的没有被吃掉，失败的消息接在末尾"
    );
}

/// WS 发不出去（死连接）：同一失败契约（内容回到草稿 + `Send failed` toast），且不发半截。
#[tokio::test]
async fn a_dead_ws_connection_fails_the_send_with_the_same_contract() {
    let gateway = FakeGateway::start_with_a_dead_ws().await;
    let transport = connect(&gateway).await;

    // 等 Close 帧被读任务记录（之后写任务才会在下一个写操作上失败）。
    for _ in 0..400 {
        if transport.ws.close_reason().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        transport.ws.close_reason().is_some(),
        "假网关的 Close 帧没有到达"
    );

    // 写任务在第一次写失败后退出；通道层从那之后的发送会被拒绝。
    // （`send_message` 只等待通道投递，所以"写任务已死"要靠探测，不能靠等待。）
    let mut rejected = false;
    for _ in 0..400 {
        if transport
            .ws
            .send_message("test-session", "probe", None, "probe".into(), None)
            .await
            .is_err()
        {
            rejected = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(rejected, "写任务没有随死连接退出");

    let mut app = test_app();
    let mut rx = send(&mut app, Some(&transport), "hi", vec![image(0x05, 4)]);
    let receipt = wait_for_receipt(&mut rx).await;
    app.handle_fetch_result(receipt);
    assert!(app.chat.pending.is_empty(), "pending 条目取回");
    assert_eq!(app.input.text(), "hi[Image #1 8×8 4B]", "内容回到草稿");
    let toast = app.toast.as_ref().expect("failure toast");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert!(
        toast.message.starts_with("Send failed"),
        "{}",
        toast.message
    );
    assert!(
        toast.message.ends_with("draft restored"),
        "{}",
        toast.message
    );
}

/// 失败回执到达时 pending 条目已经不在队列里（被打断丢弃 / 已提升 / 换过会话）：只提示
/// 原因，**不假装回填**（草稿一根不动，toast 也不说 `draft restored`——那句只属于真的
/// 把内容还回去的路径）。
#[tokio::test]
async fn a_failure_receipt_with_no_pending_entry_only_toasts() {
    let mut app = test_app();
    app.input.set_text("draft");

    app.handle_fetch_result(FetchResult {
        session_id: "test-session".into(),
        payload: FetchPayload::SendFailed {
            request_id: "req-gone".into(),
            reason: "Not sent — gateway disconnected".into(),
        },
    });

    let toast = app.toast.as_ref().expect("failure toast");
    assert_eq!(toast.kind, ToastKind::Warning);
    assert!(toast.message.contains("Not sent"), "{}", toast.message);
    assert!(
        !toast.message.contains("draft restored"),
        "没有回填就不说回填：{}",
        toast.message
    );
    assert_eq!(app.input.text(), "draft", "草稿不被碰");
    assert!(app.chat.pending.is_empty());
}

/// 派发**不等整条往返**：慢链路（每次上传 400 ms）下 `dispatch_send_message` 返回后，
/// 调用方继续跑（这里观察到后台任务已经把上传请求发出去），而整条**还没**完成——链尾
/// 未结束、帧没发出去；随后后台任务自己走完。
///
/// 这是「上传 + 发送已移出事件循环」的机制级断言：派发返回时整条**还在飞**（链尾在跑、
/// 上传请求已发出而应答未回、帧还没有），随后由后台任务自己走完，调用方没有任何等待。
#[tokio::test]
async fn dispatch_returns_before_the_round_trip_finishes() {
    let gateway = FakeGateway::start().await;
    gateway.delay_uploads(400);
    let transport = connect(&gateway).await;
    let mut app = test_app();

    let mut chain = None;
    let mut rx = dispatch(
        &mut app,
        Some(&transport),
        &mut chain,
        "req-1",
        "慢链路",
        vec![image(0x01, 16)],
    );

    // 派发已返回，调用方还能继续做事：后台任务把上传请求发了出去（任务确实在跑），
    // 而 400 ms 的上传应答还没回——帧不可能已经在路上。
    wait_for_uploads(&gateway, 1).await;
    let handle = chain.as_ref().expect("派发必须挂上链尾");
    assert!(
        !handle.is_finished(),
        "派发返回时整条往返不该已经跑完（那正是内联 await 的形态）"
    );
    assert!(
        gateway.frames().is_empty(),
        "上传应答还没回，帧不该已经发出"
    );

    // 后台任务自己走完：帧到达、没有回执（成功路径）。
    let frames = wait_for_frames(&gateway, 1).await;
    assert_eq!(frames.len(), 1);
    assert!(rx.try_recv().is_err(), "成功路径没有回执");
}

/// 两条快速提交（第一条的图还在上传、用户又发了一条）保持**提交顺序**。
///
/// 后端按帧到达顺序入队（消息顺序是合并语义的一部分），而内联 await 的形态天然串行；
/// 挪进后台后靠链尾串行（`spawn_send_message`）。第一条 2 张图（2×150 ms），第二条
/// 1 张（150 ms）：不串行时第二条会先到（测试会红），串行时严格按提交序。
#[tokio::test]
async fn two_quick_submissions_keep_their_order() {
    let gateway = FakeGateway::start().await;
    gateway.delay_uploads(150);
    let transport = connect(&gateway).await;
    let mut app = test_app();
    let mut chain = None;

    let mut rx_a = dispatch(
        &mut app,
        Some(&transport),
        &mut chain,
        "req-a",
        "first",
        vec![image(0xAA, 300), image(0xAB, 300)],
    );
    let mut rx_b = dispatch(
        &mut app,
        Some(&transport),
        &mut chain,
        "req-b",
        "second",
        vec![image(0xBB, 40)],
    );

    let frames = wait_for_frames(&gateway, 2).await;
    let contents: Vec<serde_json::Value> = frames
        .iter()
        .map(|f| serde_json::from_str(f).expect("frame is JSON"))
        .collect();
    assert_eq!(contents[0]["content"], "first", "{frames:?}");
    assert_eq!(contents[1]["content"], "second", "{frames:?}");

    // 上传也按提交序：第一条的两张先上，第二条的一张后上。
    let uploads = gateway.uploads();
    assert_eq!(uploads.len(), 3, "三次上传");
    assert_eq!(uploads[0].body, vec![0xAA; 300]);
    assert_eq!(uploads[1].body, vec![0xAB; 300]);
    assert_eq!(uploads[2].body, vec![0xBB; 40]);
    assert!(rx_a.try_recv().is_err(), "两条都成功 = 没有回执");
    assert!(rx_b.try_recv().is_err(), "两条都成功 = 没有回执");
}

/// 回填是**纯数据恢复**：不产生 intent（不重发、不重复上传）、不往 chat 里写 cell。
fn assert_restore_is_pure(app: &mut crate::app::App) {
    assert!(
        app.drain_intents().is_empty(),
        "回填不发任何 intent（重发 / 上传都没有）"
    );
    assert!(app.chat.cells.is_empty(), "回填不写 chat（内容只回草稿）");
}
