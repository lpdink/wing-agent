//! 发送链路（`runner::send_user_message`）的组件测试：上传 → 引用 → 发送，以及失败契约。
//!
//! 进程内假网关：WS 端口用 `tokio-tungstenite::accept_async`（首帧发 `ConnectResponse`，
//! 之后原样收上行帧），HTTP 端口是手写的 HTTP/1.1 假服务（同 `wing-api-client` 单测的手法，
//! 零新依赖）。两个端口独立——`GatewayEndpoint` 的 `ws_url` / `http_base` 本来就分开。
//!
//! 这里测的是**线格式**：上传请求的路径 / 原始字节 / content-type，WS 帧里的 `media` 引用，
//! 以及任一步失败时"整条不发"。

use std::sync::Arc;
use std::sync::Mutex;

use futures_util::SinkExt;
use futures_util::StreamExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use super::support::*;
use crate::app::transport::GatewayEndpoint;
use crate::app::transport::Transport;
use crate::app::transport::connect_transport;
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
                                UploadRoute::Ok => {
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

/// 驱动被测路径：`execute_intent` 的 SendMessage 臂（抽出来的函数）。
async fn send(
    app: &mut crate::app::App,
    transport: Option<&Transport>,
    content: &str,
    images: Vec<CompressedImage>,
) {
    // 与 submit 侧同构：先挂 pending，再发。
    app.chat.push_pending(
        "req-1".into(),
        crate::ui::chat_view::Submission {
            content: content.to_string(),
            images: images.clone(),
        },
    );
    super::super::runner::send_user_message(
        app,
        transport,
        content.to_string(),
        None,
        "req-1".into(),
        images,
    )
    .await;
}

#[tokio::test]
async fn images_are_uploaded_in_order_and_ride_the_ws_frame() {
    let gateway = FakeGateway::start().await;
    let transport = connect(&gateway).await;
    let mut app = test_app();

    let images = vec![image(0xAA, 300), image(0xBB, 120)];
    send(&mut app, Some(&transport), "看这两张", images).await;

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

    // WS：一条消息帧，带两条引用（顺序 = 提交顺序）。
    let frames = wait_for_frames(&gateway, 1).await;
    assert_eq!(frames.len(), 1);
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
}

/// 纯文本路径的兼容红线：帧与引入图片之前**逐字节相同**（没有 `media` 键），也不产生上传。
#[tokio::test]
async fn a_text_only_message_is_the_old_frame_without_an_upload() {
    let gateway = FakeGateway::start().await;
    let transport = connect(&gateway).await;
    let mut app = test_app();

    send(&mut app, Some(&transport), "hello", Vec::new()).await;

    let frames = wait_for_frames(&gateway, 1).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0], r#"{"request_id":"req-1","session_id":"test-session","content":"hello"}"#,
        "无 media 时不产生新键（旧网关逐字节兼容）"
    );
    assert!(gateway.uploads().is_empty(), "纯文本不上传");
    assert_eq!(app.chat.pending.len(), 1, "成功发送不丢 pending 条目");
}

/// 上传失败：整条不发（不发送半截）、pending 取回、内容回到草稿、warning toast 带原因。
#[tokio::test]
async fn an_upload_failure_sends_nothing_and_gives_the_draft_back() {
    let gateway = FakeGateway::start().await;
    gateway.fail_uploads_after(0, 400, r#"{"error":"bad_media","detail":"not an image"}"#);
    let transport = connect(&gateway).await;
    let mut app = test_app();

    send(&mut app, Some(&transport), "图呢", vec![image(0x01, 10)]).await;

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
    send(&mut app, Some(&transport), "三张", images).await;

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
    send(&mut app, None, "hello", vec![image(0x01, 10)]).await;

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

/// 无媒体的断连路径同款（文案保留旧子句 + 说明内容去向）。
#[tokio::test]
async fn a_text_only_send_keeps_the_disconnected_behaviour() {
    let mut app = test_app();
    send(&mut app, None, "hello", Vec::new()).await;
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

    send(&mut app, None, "第一句", vec![image(0x07, 12)]).await;

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
    send(&mut app, Some(&transport), "hi", vec![image(0x05, 4)]).await;
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

/// 回填是**纯数据恢复**：不产生 intent（不重发、不重复上传）、不往 chat 里写 cell。
fn assert_restore_is_pure(app: &mut crate::app::App) {
    assert!(
        app.drain_intents().is_empty(),
        "回填不发任何 intent（重发 / 上传都没有）"
    );
    assert!(app.chat.cells.is_empty(), "回填不写 chat（内容只回草稿）");
}
