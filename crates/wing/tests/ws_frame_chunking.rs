//! 全链路（进程内假 WS 网关）：>16 MiB 的事件分片投递 → 客户端不断连、应用层
//! 收到完整事件。
//!
//! 背景（2026-09-13 实测）：`sync_session` 载荷 22,151,988 B 作为单帧投递时，
//! 客户端读任务死于 `Message too long: 22151988 > 16777216`，会话在 TUI 里
//! 永久打不开。本变更让网关把超限载荷切成信封帧（`type == "_chunk"`）、客户端
//! 在读任务内合并还原——应用层零感知。
//!
//! 本文件的对照用例把这条契约钉死：**同一份 >16 MiB 载荷**单帧投递必死
//! （`FrameTooLarge`），分片投递必活（完整事件送达）。回退分片重组 → 前者依旧、
//! 后者变成「收到空事件 / 未知事件」，测试立即失败。

use std::time::Duration;

use futures_util::SinkExt;
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use wing::gateway::chunk::CHUNK_TYPE;
use wing::gateway::{CloseReason, GatewayClient, Limits};
use wing::protocol::WingEvent;

/// 客户端单帧上限（tungstenite 默认）——分片存在的理由。
const FRAME_LIMIT: usize = 16 * 1024 * 1024;

/// 一份 > 16 MiB 的 `sync_session` 载荷（单帧投递必死）。
fn oversized_sync_payload() -> String {
    let content = "x".repeat(20 * 1024 * 1024);
    json!({
        "type": "sync_session",
        "session_id": "s1",
        "messages": [{"role": "assistant", "content": content}],
        "uncommitted": null,
        "uncommitted_tools": [],
        "events": [],
        "turn_started_at": null,
        "created_at": "2026-09-13T00:00:00+00:00",
        "request_id": "req-1",
    })
    .to_string()
}

fn text_payload(content: &str) -> String {
    json!({
        "type": "text",
        "content": content,
        "created_at": "2026-09-13T00:00:00+00:00",
        "request_id": "req-1",
    })
    .to_string()
}

/// 网关侧切分的测试镜像：按字符边界把载荷切成 `parts` 片，包成 `_chunk` 信封。
fn chunkify(payload: &str, id: &str, parts: usize) -> Vec<String> {
    let mut boundaries = vec![0usize];
    for i in 1..parts {
        let mut cut = payload.len() * i / parts;
        while cut > 0 && !payload.is_char_boundary(cut) {
            cut -= 1;
        }
        boundaries.push(cut);
    }
    boundaries.push(payload.len());

    (0..parts)
        .map(|i| {
            json!({
                "type": CHUNK_TYPE,
                "id": id,
                "index": i,
                "count": parts,
                "of_type": "sync_session",
                "data": &payload[boundaries[i]..boundaries[i + 1]],
            })
            .to_string()
        })
        .collect()
}

/// 发送握手帧（每个客户端连接后等待它）。
async fn send_handshake(ws: &mut WebSocketStream<TcpStream>) {
    ws.send(Message::Text(
        r#"{"type":"connected","client_id":"test"}"#.into(),
    ))
    .await
    .unwrap();
}

/// 起一个只接受一条连接、把 `frames` 顺序发完的假网关。
async fn spawn_server<F>(handler: F) -> String
where
    F: FnOnce(WebSocketStream<TcpStream>) -> futures_util::future::BoxFuture<'static, ()>
        + Send
        + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        handler(ws).await;
    });
    format!("ws://{addr}/ws")
}

fn sync_content(event: &WingEvent) -> Option<&str> {
    match event {
        WingEvent::SyncSession { messages, .. } => messages.first()?.get("content")?.as_str(),
        _ => None,
    }
}

#[tokio::test]
async fn chunked_oversized_event_survives_and_arrives_complete() {
    let payload = oversized_sync_payload();
    assert!(payload.len() > FRAME_LIMIT, "对照前提：单帧投递必超限");
    let frames = chunkify(&payload, "1", 3);
    assert!(frames.iter().all(|f| f.len() < FRAME_LIMIT));

    let url = spawn_server(move |mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            for frame in frames {
                ws.send(Message::Text(frame.into())).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(10), client.recv_event())
        .await
        .expect("reassembly must not stall")
        .expect("connection must stay alive");

    let content = sync_content(&event).expect("a complete sync_session event");
    assert_eq!(
        content.len(),
        20 * 1024 * 1024,
        "载荷必须逐字节完整（回退重组 → 这里收到的是未知事件 / 什么都没有）"
    );
    assert!(client.close_reason().is_none(), "客户端不得因载荷大小断连");
}

#[tokio::test]
async fn same_payload_as_a_single_frame_kills_the_client() {
    // 对照：这正是本变更修掉的故障（22 MB 单帧 → 读任务死亡）。
    let payload = oversized_sync_payload();

    let url = spawn_server(move |mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            let _ = ws.send(Message::Text(payload.into())).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert!(client.recv_event().await.is_none(), "stream must be over");
    match client.close_reason() {
        Some(CloseReason::FrameTooLarge { size, .. }) => {
            assert!(*size > FRAME_LIMIT, "{size}");
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn live_event_between_fragments_arrives_after_the_complete_event() {
    let payload = oversized_sync_payload();
    let frames = chunkify(&payload, "2", 3);
    let mut with_live = vec![frames[0].clone(), text_payload("live"), frames[1].clone()];
    with_live.push(frames[2].clone());

    let url = spawn_server(move |mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            for frame in with_live {
                ws.send(Message::Text(frame.into())).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), client.recv_event())
        .await
        .expect("must not stall")
        .expect("sync must arrive first");
    assert!(
        matches!(first, WingEvent::SyncSession { .. }),
        "分片期间插入的 live 事件不得抢先（否则 chat.clear() 吞掉它）"
    );

    let second = tokio::time::timeout(Duration::from_secs(10), client.recv_event())
        .await
        .expect("must not stall")
        .expect("buffered live event must follow");
    match second {
        WingEvent::Text { content, .. } => assert_eq!(content, "live"),
        other => panic!("expected the buffered text event, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_envelope_disconnects_with_reason() {
    let frames = vec![
        json!({
            "type": CHUNK_TYPE,
            "id": "3",
            "index": 0,
            "count": 1_000_000_000usize,
            "of_type": "sync_session",
            "data": "{}",
        })
        .to_string(),
    ];

    let url = spawn_server(move |mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            for frame in frames {
                ws.send(Message::Text(frame.into())).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert!(client.recv_event().await.is_none(), "must fail fast");
    match client.close_reason() {
        Some(CloseReason::ReassemblyFailed { detail }) => {
            assert!(detail.contains("count"), "{detail}");
        }
        other => panic!("expected ReassemblyFailed, got {other:?}"),
    }
    assert!(
        client
            .close_reason()
            .unwrap()
            .describe()
            .contains("chunk reassembly failed"),
        "原因必须可读"
    );
}

#[tokio::test]
async fn unfinished_reassembly_times_out_and_disconnects() {
    let frames = chunkify(&"x".repeat(4096), "4", 2);
    let first = frames[0].clone();

    let url = spawn_server(move |mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            ws.send(Message::Text(first.into())).await.unwrap();
            // 后半片永远不来。
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
    })
    .await;

    let limits = Limits {
        idle_timeout: Duration::from_millis(200),
        ..Limits::default()
    };
    let mut client = GatewayClient::connect_with_limits(&url, None, limits)
        .await
        .unwrap();

    let ended = tokio::time::timeout(Duration::from_secs(5), client.recv_event())
        .await
        .expect("timeout must fire")
        .is_none();
    assert!(ended);
    match client.close_reason() {
        Some(CloseReason::ReassemblyFailed { detail }) => {
            assert!(detail.contains("not completed"), "{detail}");
        }
        other => panic!("expected ReassemblyFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn buffer_cap_disconnects_with_reason() {
    let frames = vec![
        json!({
            "type": CHUNK_TYPE,
            "id": "5",
            "index": 0,
            "count": 2,
            "of_type": "sync_session",
            "data": "x".repeat(900),
        })
        .to_string(),
        text_payload(&"y".repeat(900)),
    ];

    let url = spawn_server(move |mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            for frame in frames {
                ws.send(Message::Text(frame.into())).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    })
    .await;

    let limits = Limits {
        max_buffered_bytes: 1024,
        ..Limits::default()
    };
    let mut client = GatewayClient::connect_with_limits(&url, None, limits)
        .await
        .unwrap();

    assert!(client.recv_event().await.is_none(), "must fail fast");
    match client.close_reason() {
        Some(CloseReason::ReassemblyFailed { detail }) => {
            assert!(detail.contains("buffer exceeded"), "{detail}");
        }
        other => panic!("expected ReassemblyFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn small_events_are_unaffected() {
    let url = spawn_server(|mut ws| {
        Box::pin(async move {
            send_handshake(&mut ws).await;
            ws.send(Message::Text(text_payload("hello").into()))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
    })
    .await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    match client.recv_event().await {
        Some(WingEvent::Text { content, .. }) => assert_eq!(content, "hello"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert!(client.close_reason().is_none());
}
