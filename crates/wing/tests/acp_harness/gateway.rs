//! 假网关：进程内 HTTP + WS 服务，替代 `wing-gateway` 供真 `wing acp` 进程连接。
//!
//! 设计（见 06 design D1/D4）：
//!
//! - 端口 `127.0.0.1:0` 临时分配，测试把 `host:port` 写进临时 `WING_HOME` 的
//!   `core/config.yaml`；`wing acp` 的 `ensure_gateway_running` 健康检查打到这里
//!   （`service == "wing-gateway"`），**不会**尝试拉起真网关；
//! - HTTP 侧用 hyper（reqwest 的传递依赖，零新 crate）：13 个端点全是固定 JSON，
//!   keep-alive / Content-Length 分帧等正确性交给 hyper；
//! - WS 侧用 tokio-tungstenite：`TcpStream::peek` 看请求首行分流（`GET /ws` 交给
//!   `accept_async` 自己做握手），首帧发 `ConnectResponse`，之后收集上行 `ClientRequest`
//!   并按脚本推送 `WingEvent` 帧；
//! - 所有 HTTP 请求（方法 / 路径 / `X-Client-Id` / body）与 WS 上行帧**原样记录**，
//!   供断言（协议帧本身，不是实现细节）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::Request;
use hyper::Response;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// 假网关的 client_id（首帧 `ConnectResponse` 里发出去；断言 unsubscribe 头用）。
const FAKE_CLIENT_ID: &str = "fake-gateway-client";

/// 一条被记录的 HTTP 请求。
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// `X-Client-Id` 头（订阅 / 退订请求才有）。
    pub client_id: Option<String>,
    /// JSON 请求体（无体 / 非 JSON 为 None）。
    pub body: Option<Value>,
}

/// 会话的「当前模型」状态（`GET /api/session/get` 的 agent 字段 + `POST update` 写入）。
///
/// 四元组与真网关同构：`model_id` 是引用词，`model` 是调用名，`provider` 是运行期事实，
/// `display_name` 是展示名。
#[derive(Debug, Clone)]
struct SessionState {
    model_id: Option<String>,
    provider: String,
    model: String,
    display_name: Option<String>,
    workspace: Option<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            model_id: Some("echo-1".to_string()),
            provider: "fake".to_string(),
            model: "echo-1".to_string(),
            display_name: Some("Echo One".to_string()),
            workspace: None,
        }
    }
}

/// `/api/session/info` 的可变字段。
#[derive(Debug, Clone)]
struct SessionInfoState {
    name: Option<String>,
    total_tokens: i64,
    context_window_tokens: i64,
}

impl Default for SessionInfoState {
    fn default() -> Self {
        Self {
            name: None,
            total_tokens: 42,
            context_window_tokens: 262_144,
        }
    }
}

/// 假网关的可变状态（测试与连接任务共享）。
pub struct GatewayState {
    requests: Mutex<Vec<RecordedRequest>>,
    ws_inbound: Mutex<Vec<Value>>,
    ws_tx: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    ws_pending: Mutex<Vec<Message>>,
    create_seq: AtomicUsize,
    sessions: Mutex<Vec<Value>>,
    resumable: Mutex<HashMap<String, Option<String>>>,
    snapshots: Mutex<HashMap<String, Value>>,
    states: Mutex<HashMap<String, SessionState>>,
    infos: Mutex<HashMap<String, SessionInfoState>>,
    models: Mutex<Value>,
    commands: Mutex<Value>,
}

impl GatewayState {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            ws_inbound: Mutex::new(Vec::new()),
            ws_tx: Mutex::new(None),
            ws_pending: Mutex::new(Vec::new()),
            create_seq: AtomicUsize::new(0),
            sessions: Mutex::new(Vec::new()),
            resumable: Mutex::new(HashMap::new()),
            snapshots: Mutex::new(HashMap::new()),
            states: Mutex::new(HashMap::new()),
            infos: Mutex::new(HashMap::new()),
            models: Mutex::new(json!({"providers": []})),
            commands: Mutex::new(json!({"commands": []})),
        }
    }

    fn record_request(&self, request: RecordedRequest) {
        self.requests.lock().expect("gateway state").push(request);
    }

    fn record_ws_frame(&self, text: &str) {
        match serde_json::from_str::<Value>(text) {
            Ok(value) => self.ws_inbound.lock().expect("gateway state").push(value),
            // 客户端只发 JSON（`ClientRequest`）；解码失败不该静默——记成字符串。
            Err(_) => self
                .ws_inbound
                .lock()
                .expect("gateway state")
                .push(json!({"malformed": text})),
        }
    }

    fn attach_ws(&self, tx: mpsc::UnboundedSender<Message>) {
        let pending: Vec<Message> = {
            let mut pending = self.ws_pending.lock().expect("gateway state");
            std::mem::take(&mut pending)
        };
        *self.ws_tx.lock().expect("gateway state") = Some(tx.clone());
        for message in pending {
            let _ = tx.send(message);
        }
    }

    fn detach_ws(&self) {
        *self.ws_tx.lock().expect("gateway state") = None;
    }

    fn state_of(&self, session_id: &str) -> SessionState {
        self.states
            .lock()
            .expect("gateway state")
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }
}

/// 假网关句柄（测试侧）。
pub struct FakeGateway {
    addr: SocketAddr,
    state: Arc<GatewayState>,
}

impl FakeGateway {
    /// 起服务（绑定临时端口 + 派生 accept 循环）。
    pub fn start() -> Self {
        Self::bind("127.0.0.1:0")
    }

    /// 在**指定端口**起服务（冷启动回归用：端口先空着让 `wing acp` 判定「网关没在跑」，
    /// 等它拉起 `WING_GATEWAY_CMD` 之后由测试把服务绑上来）。
    pub fn start_on(port: u16) -> Self {
        Self::bind(("127.0.0.1", port))
    }

    fn bind(addr: impl std::net::ToSocketAddrs) -> Self {
        let listener = std::net::TcpListener::bind(addr).expect("bind fake gateway");
        listener
            .set_nonblocking(true)
            .expect("fake gateway listener is non-blocking");
        let addr = listener.local_addr().expect("fake gateway addr");
        let listener = TcpListener::from_std(listener).expect("tokio listener");
        let state = Arc::new(GatewayState::new());
        let accept_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let state = Arc::clone(&accept_state);
                tokio::spawn(async move { serve_connection(stream, state).await });
            }
        });
        Self { addr, state }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn client_id(&self) -> &'static str {
        FAKE_CLIENT_ID
    }

    // ---- 场景配置 ----

    /// `GET /api/models` 的目录。
    pub fn set_models(&self, catalog: Value) {
        *self.state.models.lock().expect("gateway state") = catalog;
    }

    /// `GET /api/commands` 的命令表。
    pub fn set_commands(&self, commands: Value) {
        *self.state.commands.lock().expect("gateway state") = commands;
    }

    /// `GET /api/session/list` 的会话行（原样 JSON）。
    pub fn set_sessions(&self, rows: Vec<Value>) {
        *self.state.sessions.lock().expect("gateway state") = rows;
    }

    /// 注册一个可 `POST /api/session/resume` 的会话（未知 → 404）。
    pub fn add_resumable(&self, session_id: &str, workspace: Option<&str>) {
        self.state
            .resumable
            .lock()
            .expect("gateway state")
            .insert(session_id.to_string(), workspace.map(str::to_string));
    }

    /// 订阅该会话时推的 `sync_session` 快照（缺省推空快照）。
    pub fn set_snapshot(&self, session_id: &str, frame: Value) {
        self.state
            .snapshots
            .lock()
            .expect("gateway state")
            .insert(session_id.to_string(), frame);
    }

    /// 预置会话状态（`GET /api/session/get`）：`(model_id, provider, 调用名, 展示名)`。
    pub fn set_session_state(
        &self,
        session_id: &str,
        model_id: Option<&str>,
        provider: &str,
        model: &str,
        display_name: Option<&str>,
    ) {
        self.state.states.lock().expect("gateway state").insert(
            session_id.to_string(),
            SessionState {
                model_id: model_id.map(str::to_string),
                provider: provider.to_string(),
                model: model.to_string(),
                display_name: display_name.map(str::to_string),
                workspace: None,
            },
        );
    }

    /// 预置 `/api/session/info`（标题 / 用量）。
    pub fn set_session_info(
        &self,
        session_id: &str,
        name: Option<&str>,
        total_tokens: i64,
        context_window_tokens: i64,
    ) {
        self.state.infos.lock().expect("gateway state").insert(
            session_id.to_string(),
            SessionInfoState {
                name: name.map(str::to_string),
                total_tokens,
                context_window_tokens,
            },
        );
    }

    // ---- 运行时 ----

    /// 向客户端推一帧（WS 未连接时排队，连接后立即冲刷）。
    pub fn push(&self, frame: Value) {
        push(&self.state, frame);
    }

    /// 已记录的 HTTP 请求快照。
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().expect("gateway state").clone()
    }

    /// 指定路径的请求记录。
    pub fn requests_for(&self, path: &str) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == path)
            .collect()
    }

    /// 指定路径的请求条数（负断言用）。
    pub fn count(&self, path: &str) -> usize {
        self.requests_for(path).len()
    }

    /// 已收到的 WS 上行帧快照。
    pub fn ws_inbound(&self) -> Vec<Value> {
        self.state.ws_inbound.lock().expect("gateway state").clone()
    }

    /// 等一条匹配的 WS 上行帧（条件轮询 + 有界超时）。
    pub async fn wait_ws_inbound(&self, what: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        crate::acp_harness::wait_until(what, || {
            self.ws_inbound().into_iter().find(|frame| predicate(frame))
        })
        .await
    }

    /// 等一条匹配的 HTTP 请求（条件轮询 + 有界超时）。
    pub async fn wait_request(
        &self,
        what: &str,
        predicate: impl Fn(&RecordedRequest) -> bool,
    ) -> RecordedRequest {
        crate::acp_harness::wait_until(what, || {
            self.requests()
                .into_iter()
                .find(|request| predicate(request))
        })
        .await
    }
}

// ============================================================
// 连接分流
// ============================================================

/// 一条 TCP 连接：peek 首行 → `/ws` 走 WS 握手，其余交给 hyper。
async fn serve_connection(stream: TcpStream, state: Arc<GatewayState>) {
    let mut buf = [0u8; 256];
    let is_ws = loop {
        match stream.peek(&mut buf).await {
            Ok(0) => return,
            Ok(n) => {
                let head = String::from_utf8_lossy(&buf[..n]);
                if head.contains("\r\n") || n >= buf.len() {
                    break head.starts_with("GET /ws");
                }
                // 首行还没到齐（请求是分段的）：短暂让出再 peek。
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(_) => return,
        }
    };
    if is_ws {
        if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
            ws_loop(ws, state).await;
        }
        return;
    }
    let io = TokioIo::new(stream);
    let service = service_fn(move |request: Request<Incoming>| {
        let state = Arc::clone(&state);
        async move { handle_http(request, state).await }
    });
    let _ = http1::Builder::new().serve_connection(io, service).await;
}

// ============================================================
// WS
// ============================================================

async fn ws_loop(ws: WebSocketStream<TcpStream>, state: Arc<GatewayState>) {
    let (mut sink, mut stream) = ws.split();
    let hello = json!({"type": "connected", "client_id": FAKE_CLIENT_ID}).to_string();
    if sink.send(Message::Text(hello.into())).await.is_err() {
        return;
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    state.attach_ws(tx);
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => state.record_ws_frame(text.as_str()),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            outgoing = rx.recv() => match outgoing {
                Some(message) => {
                    if sink.send(message).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    state.detach_ws();
}

// ============================================================
// HTTP
// ============================================================

async fn handle_http(
    request: Request<Incoming>,
    state: Arc<GatewayState>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or_default().to_string();
    let client_id = request
        .headers()
        .get("x-client-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body_bytes = request
        .into_body()
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    let body: Option<Value> = serde_json::from_slice(&body_bytes).ok();
    state.record_request(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        client_id,
        body: body.clone(),
    });

    let (status, payload) = route(&method, &path, &query, body.as_ref(), &state);
    let response = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(payload.to_string())))
        .expect("fake gateway response builds");
    Ok(response)
}

/// 全部端点的固定应答（路径与形状以 `wing-api-client` 的请求方为准）。
fn route(
    method: &str,
    path: &str,
    query: &str,
    body: Option<&Value>,
    state: &GatewayState,
) -> (u16, Value) {
    match (method, path) {
        ("GET", "/api/health") => (
            200,
            json!({
                "service": "wing-gateway",
                "status": "ok",
                "version": "0.0.0-e2e",
                "uptime": 1,
            }),
        ),
        ("GET", "/api/models") => (200, state.models.lock().expect("gateway state").clone()),
        ("GET", "/api/commands") => (200, state.commands.lock().expect("gateway state").clone()),
        ("GET", "/api/session/list") => (
            200,
            json!({"sessions": state.sessions.lock().expect("gateway state").clone()}),
        ),
        ("GET", "/api/session/get") => match query_param(query, "session_id") {
            Some(session_id) => (200, session_get_frame(state, &session_id)),
            None => not_found("missing session_id"),
        },
        ("GET", "/api/session/info") => match query_param(query, "session_id") {
            Some(session_id) => (200, session_info_frame(state, &session_id)),
            None => not_found("missing session_id"),
        },
        ("POST", "/api/session/create") => {
            let session_id = format!(
                "sess-{}",
                state.create_seq.fetch_add(1, Ordering::SeqCst) + 1
            );
            let workspace = body
                .and_then(|body| body.get("workspace"))
                .and_then(Value::as_str)
                .map(str::to_string);
            register_session(state, &session_id, workspace.clone());
            (
                200,
                json!({
                    "session_id": session_id,
                    "template_name": body
                        .and_then(|body| body.get("template_name"))
                        .and_then(Value::as_str)
                        .unwrap_or("default"),
                    "workspace": workspace,
                    "backend": "memory",
                }),
            )
        }
        ("POST", "/api/session/resume") => {
            let session_id = body
                .and_then(|body| body.get("session_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let known = state
                .resumable
                .lock()
                .expect("gateway state")
                .get(&session_id)
                .cloned();
            match known {
                Some(workspace) => (
                    200,
                    json!({
                        "session_id": session_id,
                        "template_name": "default",
                        "workspace": workspace,
                    }),
                ),
                None => not_found("unknown session"),
            }
        }
        ("POST", "/api/session/subscribe") => {
            let session_id = body
                .and_then(|body| body.get("session_id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            // 与真网关同语义（`routes/session.py`）：订阅即推一份快照。
            let frame = state
                .snapshots
                .lock()
                .expect("gateway state")
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| {
                    json!({
                        "type": "sync_session",
                        "session_id": session_id,
                        "status": "idle",
                        "messages": [],
                        "events": [],
                    })
                });
            let frame = crate::acp_harness::wing_event(session_id, frame);
            push(state, frame);
            (200, json!({"ok": true}))
        }
        ("POST", "/api/session/unsubscribe") => (200, json!({"ok": true})),
        ("POST", "/api/session/update") => {
            // 会话级状态变更（模型引用词）：按目录把 id 映射回调用名 / provider /
            // 展示名写回状态表（未命中 = 真网关的 400 语义），后续 session/get 反映新值。
            if let Some(body) = body {
                let session_id = body
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(model_id) = body.get("model_id").and_then(Value::as_str) {
                    let catalog = state.models.lock().expect("gateway state").clone();
                    match resolve_catalog_entry(&catalog, model_id) {
                        Some((provider, name, display)) => {
                            let mut states = state.states.lock().expect("gateway state");
                            let entry = states.entry(session_id.to_string()).or_default();
                            entry.model_id = Some(model_id.to_string());
                            entry.provider = provider;
                            entry.model = name;
                            entry.display_name = display;
                        }
                        None => {
                            return (
                                400,
                                json!({
                                    "detail": format!(
                                        "unknown model id '{model_id}'; available ids: {}",
                                        catalog_ids(&catalog).join(", ")
                                    ),
                                }),
                            );
                        }
                    }
                }
            }
            (200, json!({"ok": true}))
        }
        ("POST", "/api/session/interrupt") => (200, json!({"ok": true})),
        ("POST", "/api/session/release") => (
            200,
            json!({"ok": true, "released": true, "detail": "released by e2e"}),
        ),
        _ => not_found(path),
    }
}

fn not_found(detail: &str) -> (u16, Value) {
    (404, json!({"error": "not found", "detail": detail}))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.to_string())
    })
}

/// 会话注册：建会话 / resume 之后都能被 list、get、info 看到（形状与真网关一致）。
fn register_session(state: &GatewayState, session_id: &str, workspace: Option<String>) {
    state
        .resumable
        .lock()
        .expect("gateway state")
        .insert(session_id.to_string(), workspace.clone());
    state.states.lock().expect("gateway state").insert(
        session_id.to_string(),
        SessionState {
            workspace,
            ..SessionState::default()
        },
    );
    state
        .infos
        .lock()
        .expect("gateway state")
        .insert(session_id.to_string(), SessionInfoState::default());
}

fn session_get_frame(state: &GatewayState, session_id: &str) -> Value {
    let current = state.state_of(session_id);
    json!({
        "session_id": session_id,
        "name": null,
        "template_name": "default",
        "workspace": current.workspace,
        "status": "idle",
        "messages": [],
        "agent": {
            "model_name": current.model,
            "model_id": current.model_id,
            "system_prompt": null,
            "tools": [],
            "skills": [],
            "rules": [],
            "workspace": current.workspace,
            "provider_name": current.provider,
            "model_display_name": current.display_name,
        },
    })
}

fn session_info_frame(state: &GatewayState, session_id: &str) -> Value {
    let info = state
        .infos
        .lock()
        .expect("gateway state")
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    let current = state.state_of(session_id);
    json!({
        "model": current.model,
        "model_id": current.model_id,
        "provider_name": current.provider,
        "model_display_name": current.display_name,
        "api_url": "http://fake-gateway",
        "tools": [],
        "total_tokens": info.total_tokens,
        "context_window_tokens": info.context_window_tokens,
        "thinking": false,
        "reasoning_effort": null,
        "yolo": false,
        "session_name": info.name,
        "workdir": current.workspace,
        "status": "idle",
        "context_stats": {"message_count": 1, "total_tokens": info.total_tokens},
        "skills_info": "",
        "system_prompt": "",
        "tags": [],
        "tag_meta": {},
    })
}

/// 目录里按 **id** 查声明：`(provider, 调用名, 展示名)`（单键查表，与真网关同语义）。
fn resolve_catalog_entry(
    catalog: &Value,
    model_id: &str,
) -> Option<(String, String, Option<String>)> {
    catalog
        .get("providers")?
        .as_array()?
        .iter()
        .find_map(|group| {
            let provider = group.get("provider")?.as_str()?.to_string();
            let detail = group
                .get("models")?
                .as_array()?
                .iter()
                .find(|detail| detail.get("id").and_then(Value::as_str) == Some(model_id))?;
            let name = detail.get("name")?.as_str()?.to_string();
            let display = detail
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some((provider, name, display))
        })
}

/// 目录里的全部 id（400 的 available ids 用）。
fn catalog_ids(catalog: &Value) -> Vec<String> {
    catalog
        .get("providers")
        .and_then(Value::as_array)
        .map(|providers| {
            providers
                .iter()
                .flat_map(|group| {
                    group
                        .get("models")
                        .and_then(Value::as_array)
                        .map(|models| {
                            models
                                .iter()
                                .filter_map(|detail| {
                                    detail.get("id").and_then(Value::as_str).map(str::to_string)
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 与 [`FakeGateway::push`] 等价的内部版本（路由里推快照用）。
fn push(state: &GatewayState, frame: Value) {
    let message = Message::Text(frame.to_string().into());
    let mut guard = state.ws_tx.lock().expect("gateway state");
    match guard.as_mut() {
        Some(tx) => {
            let _ = tx.send(message);
        }
        None => state
            .ws_pending
            .lock()
            .expect("gateway state")
            .push(message),
    }
}
