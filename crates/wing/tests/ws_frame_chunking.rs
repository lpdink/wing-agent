//! 全链路（进程内假 WS 网关）：>16 MiB 的事件分片投递 → 客户端不断连、应用层
//! 收到完整事件。
//!
//! 背景（2026-09-13 实测）：`sync_session` 载荷 22,151,988 B 作为单帧投递时，
//! 客户端读任务死于 `Message too long: 22151988 > 16777216`，会话在 TUI 里
//! 永久打不开。本变更让网关把超限载荷切成信封帧（`type == "_chunk"`）、客户端
//! 在读任务内合并还原——应用层零感知。对应单帧超限诊断见
//! `ws_close_reason::oversized_frame_is_reported_as_frame_too_large`。
use std::time::Duration;
use wing::protocol::SessionSnapshot;

use futures_util::SinkExt;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

use wing::gateway::chunk::CHUNK_TYPE;
use wing::gateway::{CloseReason, GatewayClient, Limits};
use wing::protocol::WingEvent;

#[allow(dead_code)]
#[path = "common/e2e.rs"]
mod e2e;

use e2e::{start_ws_server, wait_for_ws_disconnect};

/// 客户端单帧上限（tungstenite 默认）——分片存在的理由。
const FRAME_LIMIT: usize = 16 * 1024 * 1024;

fn sync_payload(content_bytes: usize) -> String {
    let content = "x".repeat(content_bytes);
    json!({
        "type": "sync_session",
        "session_id": "s1",
        "status": "idle",
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

async fn serve_frames(frames: Vec<String>) -> String {
    let (url, _) = start_ws_server(move |mut ws| async move {
        for frame in frames {
            ws.send(Message::Text(frame.into())).await.unwrap();
        }
        wait_for_ws_disconnect(ws).await;
    })
    .await;
    url
}

async fn recv(client: &mut GatewayClient) -> Option<WingEvent> {
    tokio::time::timeout(Duration::from_secs(10), client.recv_event())
        .await
        .expect("WebSocket client stalled")
}

fn sync_content(event: &WingEvent) -> Option<&str> {
    match event {
        WingEvent::SyncSession(SessionSnapshot { messages, .. }) => {
            messages.first()?.get("content")?.as_str()
        }
        _ => None,
    }
}

#[tokio::test]
async fn chunked_oversized_event_survives_and_arrives_complete() {
    const CONTENT_BYTES: usize = 20 * 1024 * 1024;
    let payload = sync_payload(CONTENT_BYTES);
    assert!(payload.len() > FRAME_LIMIT, "对照前提：单帧投递必超限");
    let frames = chunkify(&payload, "1", 3);
    assert!(frames.iter().all(|f| f.len() < FRAME_LIMIT));

    let url = serve_frames(frames).await;
    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    let event = recv(&mut client).await.expect("connection must stay alive");
    let content = sync_content(&event).expect("a complete sync_session event");
    assert_eq!(content.len(), CONTENT_BYTES);
    assert!(
        content.bytes().all(|byte| byte == b'x'),
        "载荷必须逐字节完整"
    );
    assert!(client.close_reason().is_none(), "客户端不得因载荷大小断连");
}

#[tokio::test]
async fn live_event_between_fragments_arrives_after_the_complete_event() {
    let payload = sync_payload(4096);
    let mut frames = chunkify(&payload, "2", 3);
    frames.insert(1, text_payload("live"));

    let url = serve_frames(frames).await;
    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    let first = recv(&mut client).await.expect("sync must arrive first");
    assert!(
        matches!(first, WingEvent::SyncSession(_)),
        "分片期间插入的 live 事件不得抢先（否则 chat.clear() 吞掉它）"
    );

    let second = recv(&mut client)
        .await
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

    let url = serve_frames(frames).await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    assert!(recv(&mut client).await.is_none(), "must fail fast");
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
    let mut frames = chunkify(&"x".repeat(4096), "4", 2);
    frames.truncate(1);
    let url = serve_frames(frames).await;

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

    let url = serve_frames(frames).await;

    let limits = Limits {
        max_buffered_bytes: 1024,
        ..Limits::default()
    };
    let mut client = GatewayClient::connect_with_limits(&url, None, limits)
        .await
        .unwrap();

    assert!(recv(&mut client).await.is_none(), "must fail fast");
    match client.close_reason() {
        Some(CloseReason::ReassemblyFailed { detail }) => {
            assert!(detail.contains("buffer exceeded"), "{detail}");
        }
        other => panic!("expected ReassemblyFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn small_events_are_unaffected() {
    let url = serve_frames(vec![text_payload("hello")]).await;

    let mut client = GatewayClient::connect(&url, None).await.unwrap();
    match recv(&mut client).await {
        Some(WingEvent::Text { content, .. }) => assert_eq!(content, "hello"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert!(client.close_reason().is_none());
}
