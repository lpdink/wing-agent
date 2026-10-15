//! End-to-end: the control-plane subcommands, driven through the real binary.
//!
//! `wing branches` / `fork` / `rewind` / `interrupt` (`int`) / `compact` /
//! `update` / `reload` / `new` / `resume` / `restart` are thin wiring over
//! endpoints the API client already had — and wiring is exactly what unit
//! tests on the typed view cannot show. These tests spawn the real process
//! against a stub gateway and pin what a caller depends on:
//!
//! 1. **The request path**: method, endpoint and JSON body of every command
//!    (including "only the fields you passed" for `update` and "instruction
//!    optional" for `compact`) — read off the wire, not off an internal type.
//! 2. **The output contract**: text and `--json` carry the same facts; exit
//!    codes are 0 on success and non-zero on failure (404 / failed reload /
//!    no update fields).
//! 3. **The uuid loop is closed by the CLI alone**: the uuids `fork` / `rewind`
//!    take come from `wing branches` and are fed back verbatim.
//! 4. **Hydration convention**: a 404 from a session-scoped command means
//!    "evicted", so the CLI resumes once and retries (`branches` / `fork` /
//!    `rewind` / `compact` / `update`); `interrupt` deliberately does not —
//!    there is nothing to interrupt in a session the gateway does not hold.
//!
//! The stub answers only the endpoints these commands use; it is not a
//! gateway, it is a wire recorder with a little state (the interrupt flips
//! `working` to `idle`, `update` fields mirror back through `info`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

const SESSION_ID: &str = "20261010-120000-abcdef01";
/// The one session on disk; every other id is a 404.
const UNKNOWN_ID: &str = "ghost-session";
/// A session that sits in the archive: resume / send answer the structured
/// 404 (`error="session_archived"`) the CLI must turn into a restore hint.
const ARCHIVED_ID: &str = "archived-session";
/// A session the gateway refuses to archive (busy): 409 with the reason.
const BUSY_ID: &str = "busy-session";
/// A session already in the archive (archive → `already archived`);
/// restoring it is a no-op (`not archived` only applies to live ids, but the
/// stub reuses this id for the restore-side skip too).
const SETTLED_ID: &str = "settled-session";

// ============================================================
// Stub gateway
// ============================================================

/// One request the CLI sent, with the status the stub answered.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    body: String,
    status: u16,
}

struct StubState {
    requests: Vec<Recorded>,
    /// Status served by `/api/session/info` (interrupt flips it to idle).
    status: String,
    /// Whether the session is "in gateway memory". When false, `interrupt`
    /// 404s and the paths in `hydrate_paths` 404 (eviction simulation).
    loaded: bool,
    /// Paths that 404 while `!loaded` — a resume loads the session.
    hydrate_paths: Vec<&'static str>,
    /// Fields applied by `/api/session/update`, mirrored back by `info`.
    updated: serde_json::Map<String, Value>,
    /// Session tags (set by create, readable/extendable via `/api/session/tag`).
    tags: Vec<String>,
    /// `/api/system/reload` items, in the order the stub reports them.
    reload_items: Vec<(String, bool, Option<String>)>,
    /// Connected WS clients (client_id → outbound frame sender). The subscribe
    /// call pushes the `sync_session` replay through the matching sender.
    ws_clients: HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>,
    next_client_id: u32,
    /// The ask the session snapshot carries (`None` = nothing pending).
    pending_ask: Option<Value>,
    /// Push a live ask this many ms after a subscribe (the "ask arrives while
    /// the caller is already listening" path).
    ask_after_subscribe_ms: Option<u64>,
    /// Push the ask **before** the snapshot on subscribe: the live ask
    /// outrunning the replay.
    ask_before_snapshot: bool,
}

impl Default for StubState {
    fn default() -> Self {
        Self {
            requests: Vec::new(),
            status: "idle".into(),
            loaded: true,
            hydrate_paths: Vec::new(),
            updated: serde_json::Map::new(),
            tags: Vec::new(),
            reload_items: [
                "config.yaml",
                "hooks",
                "prompt commands",
                "provider",
                "skills & rules",
                "log level",
            ]
            .into_iter()
            .map(|name| (name.to_string(), true, None))
            .collect(),
            ws_clients: HashMap::new(),
            next_client_id: 0,
            pending_ask: None,
            ask_after_subscribe_ms: None,
            ask_before_snapshot: false,
        }
    }
}

/// A stub gateway: `127.0.0.1:0`, minimal HTTP/1.1, shared state.
struct Stub {
    state: Arc<Mutex<StubState>>,
    home: PathBuf,
    /// The port this stub listens on (its `WING_HOME` config points here).
    port: u16,
}

impl Stub {
    async fn start(tag: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(StubState::default()));
        let server_state = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(handle_conn(stream, server_state.clone()));
            }
        });

        let home = scratch_home(tag);
        std::fs::write(
            home.join("core").join("config.yaml"),
            format!("gateway:\n  host: 127.0.0.1\n  port: {port}\n"),
        )
        .unwrap();

        Self { state, home, port }
    }

    fn with(&self, f: impl FnOnce(&mut StubState)) {
        f(&mut self.state.lock().unwrap());
    }

    /// Requests seen so far (method, path, status, body).
    ///
    /// `/api/health` is filtered out: every command probes it on startup
    /// (gateway discovery), so it is transport noise in assertions about a
    /// command's own calls.
    fn requests(&self) -> Vec<Recorded> {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|request| request.path != "/api/health")
            .cloned()
            .collect()
    }

    /// The requests whose path ends with `suffix` (the repeated calls of one
    /// command are the interesting sequence: 404 → resume → retry).
    fn path_hits(&self, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|request| request.path == path)
            .collect()
    }

    /// The (method, path, body) triples, for exact-order assertions.
    fn trace(&self) -> Vec<(String, String, String)> {
        self.requests()
            .into_iter()
            .map(|request| (request.method, request.path, request.body))
            .collect()
    }
}

fn scratch_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wing-control-plane-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("core")).unwrap();
    dir
}

async fn handle_conn(mut stream: TcpStream, state: Arc<Mutex<StubState>>) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some((head, body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        // WebSocket upgrade (`wing asks` / `wing wait` connect to read events):
        // the request head is already consumed, so the 101 is written by hand
        // and the stream is wrapped for framing (`wait_fail_fast_e2e` pattern).
        if header_value(&head, "upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        {
            serve_ws(stream, &head, state).await;
            return;
        }
        let request_line = head.lines().next().unwrap_or("").to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let path = target.split('?').next().unwrap_or("").to_string();
        let query = target
            .split_once('?')
            .map(|(_, query)| query.to_string())
            .unwrap_or_default();

        let (status, response_body) = {
            let mut guard = state.lock().unwrap();
            let (status, response_body) = route(&method, &path, &query, &body, &head, &mut guard);
            guard.requests.push(Recorded {
                method: method.clone(),
                path: path.clone(),
                // The **request** body — what the CLI sent.
                body: body.clone(),
                status,
            });
            (status, response_body)
        };

        let response = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            status_text(status),
            response_body.len(),
            response_body
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Serve one WebSocket client: register it (so `/api/session/subscribe` can
/// push the snapshot through it), answer the handshake with `connected`, then
/// forward outbound frames until either side closes.
async fn serve_ws(mut stream: TcpStream, head: &str, state: Arc<Mutex<StubState>>) {
    let key = header_value(head, "sec-websocket-key").unwrap_or_default();
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

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let client_id = {
        let mut guard = state.lock().unwrap();
        let client_id = format!("stub-client-{}", guard.next_client_id);
        guard.next_client_id += 1;
        guard.ws_clients.insert(client_id.clone(), tx);
        client_id
    };
    let connected = json!({"type": "connected", "client_id": client_id}).to_string();
    if ws.send(Message::Text(connected.into())).await.is_err() {
        state.lock().unwrap().ws_clients.remove(&client_id);
        return;
    }
    loop {
        tokio::select! {
            outgoing = rx.recv() => match outgoing {
                Some(frame) => {
                    if ws.send(Message::Text(frame.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            incoming = ws.next() => match incoming {
                None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
    state.lock().unwrap().ws_clients.remove(&client_id);
}

/// Route one request; returns `(status, body)`.
fn route(
    method: &str,
    path: &str,
    query: &str,
    body: &str,
    head: &str,
    state: &mut StubState,
) -> (u16, String) {
    let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    // POST endpoints carry the id in the body, GET endpoints in the query.
    let session_id = request["session_id"]
        .as_str()
        .map(str::to_string)
        .or_else(|| query_param(query, "session_id"))
        .unwrap_or_else(|| SESSION_ID.to_string());

    // A session the stub does not hold: every session endpoint 404s.
    if session_id == UNKNOWN_ID {
        return not_loaded();
    }

    // An archived session: hydration-style endpoints answer the structured
    // 404 the real gateway tags (`error="session_archived"`); plain lookups
    // stay a plain not-found, exactly like the real routes.
    if session_id == ARCHIVED_ID && path != "/api/session/archive" {
        return match (method, path) {
            ("POST", "/api/session/resume")
            | ("POST", "/api/session/send")
            | ("POST", "/api/session/interrupt")
            | ("GET", "/api/session/get")
            | ("GET", "/api/session/info")
            | ("GET", "/api/session/branches") => archived_404(),
            _ => not_loaded(),
        };
    }

    match (method, path) {
        ("GET", "/api/health") => (
            200,
            json!({"service": "wing-gateway", "status": "ok", "version": "test", "uptime": 1})
                .to_string(),
        ),

        // Subscribe: push the session snapshot (what a mid-join subscriber
        // receives after the replay) through the caller's WS.
        ("POST", "/api/session/subscribe") => {
            let client_id = header_value(head, "x-client-id").unwrap_or_default();
            if let Some(tx) = state.ws_clients.get(&client_id) {
                if state.ask_before_snapshot {
                    // Live ask outrunning the replay (anything the subscriber
                    // reads first is a live frame, not the snapshot).
                    let _ = tx.send(ask_event().to_string());
                }
                let _ = tx.send(session_snapshot(state));
                // Optional live ask: it arrives after the snapshot, so only a
                // listener that keeps reading (`--wait`) sees it.
                if let (Some(delay_ms), None) =
                    (state.ask_after_subscribe_ms, state.pending_ask.as_ref())
                {
                    let tx = tx.clone();
                    let ask = ask_event().to_string();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        let _ = tx.send(ask);
                    });
                }
            }
            (200, json!({"ok": true}).to_string())
        }

        // 归档 / 恢复（`wing archive` 的唯一写端点）。
        ("POST", "/api/session/archive") => {
            let archived = request["archived"].as_bool().unwrap_or(true);
            let sid = request["session_id"]
                .as_str()
                .unwrap_or(SESSION_ID)
                .to_string();
            if sid == BUSY_ID {
                return (
                    409,
                    json!({"error": "conflict",
                           "detail": "session cannot be archived: status=working"})
                    .to_string(),
                );
            }
            let (changed, detail) = if sid == SETTLED_ID {
                (false, if archived { "already archived" } else { "not archived" })
            } else if sid == ARCHIVED_ID && archived {
                (false, "already archived")
            } else {
                (true, if archived { "archived" } else { "restored" })
            };
            (
                200,
                json!({
                    "ok": true,
                    "session_id": sid,
                    "archived": archived,
                    "changed": changed,
                    "detail": detail,
                })
                .to_string(),
            )
        }

        // 归档列表（读索引的投影；第二条故意带多字节时间戳）。
        ("GET", "/api/session/archive") => (
            200,
            json!({"sessions": [
                {"id": ARCHIVED_ID, "name": "old chat", "archived_at": "2026-10-01T10:20:30.9",
                 "tags": ["favorite"], "model_display_name": "DeepSeek Flash"},
                {"id": "archived-multibyte", "name": "日本語のセッション",
                 "archived_at": "日本語日本語日本語日"}
            ]})
            .to_string(),
        ),

        // The stdio / ACP preflight (`cmd::setup::preflight_or_report`).
        ("GET", "/api/settings/status") => (
            200,
            json!({"valid": true, "setup_mode": false, "problems": [], "fingerprint": "fp-1"})
                .to_string(),
        ),

        ("POST", "/api/session/create") => {
            let template = request["template_name"].as_str().unwrap_or("default");
            // create-or-adopt: echo the requested id like the real gateway
            // (the stdio frontend rejects a mismatched id).
            let created_id = request["session_id"].as_str().unwrap_or("created-by-stub");
            // Create is where tags are born (atomic with the session).
            state.tags = request["tags"]
                .as_array()
                .map(|tags| {
                    tags.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            (
                200,
                json!({
                    "session_id": created_id,
                    "template_name": template,
                    "workspace": request["workspace"].clone(),
                    "backend": "file",
                })
                .to_string(),
            )
        }

        ("POST", "/api/session/resume") => {
            // Hydration: the session becomes loaded.
            state.loaded = true;
            (
                200,
                json!({
                    "session_id": SESSION_ID,
                    "template_name": "executor",
                    "workspace": "/tmp/stub-ws",
                })
                .to_string(),
            )
        }

        ("POST", "/api/session/tag") => {
            // Atomic-ish: idempotent add, then report the resulting set.
            if let Some(add) = request["add"].as_array() {
                for tag in add.iter().filter_map(Value::as_str) {
                    if !state.tags.iter().any(|existing| existing == tag) {
                        state.tags.push(tag.to_string());
                    }
                }
            }
            let added: Vec<Value> = request["add"].as_array().cloned().unwrap_or_default();
            (
                200,
                json!({
                    "ok": true,
                    "session_id": SESSION_ID,
                    "tags": state.tags,
                    "added": added,
                    "removed": [],
                })
                .to_string(),
            )
        }

        ("GET", "/api/session/branches") if state_404(state, path) => not_loaded(),
        ("GET", "/api/session/branches") => (
            200,
            json!({
                "targets": [
                    {"uuid": "u-first", "content": "first question", "role": "user"},
                    {"uuid": "u-second", "content": "second question\nwith two lines", "role": "user"},
                    {"uuid": "current", "content": "(current)", "role": "user"},
                ]
            })
            .to_string(),
        ),

        ("POST", "/api/session/fork") if state_404(state, path) => not_loaded(),
        ("POST", "/api/session/fork") => (
            200,
            json!({
                "session_id": "forked-by-stub",
                "draft": "first question",
            })
            .to_string(),
        ),

        ("POST", "/api/session/rewind") if state_404(state, path) => not_loaded(),
        ("POST", "/api/session/rewind") => (
            200,
            json!({"ok": true, "draft": "first question"}).to_string(),
        ),

        ("POST", "/api/session/compact") if state_404(state, path) => not_loaded(),
        ("POST", "/api/session/compact") => (
            200,
            json!({"ok": true, "original_tokens": 12345, "compressed_tokens": 2345}).to_string(),
        ),

        ("POST", "/api/session/update") if state_404(state, path) => not_loaded(),
        ("POST", "/api/session/update") => {
            // Mirror the applied fields back through `info` (partial update:
            // whatever the CLI sent, nothing else).
            if let Value::Object(fields) = &request {
                for (key, value) in fields {
                    if key != "session_id" {
                        state.updated.insert(key.clone(), value.clone());
                    }
                }
            }
            (200, json!({"ok": true}).to_string())
        }

        ("POST", "/api/session/interrupt") => {
            if !state.loaded {
                return not_loaded();
            }
            state.status = "idle".into();
            (200, json!({"ok": true}).to_string())
        }

        ("POST", "/api/session/send") => {
            // A prompt starts a turn: subscribed frontends (stdio) hear the
            // terminal frame and can exit; one-shot `wing run` never listens.
            let frame = json!({
                "type": "turn_result",
                "subtype": "success",
                "is_error": false,
                "result": "stub turn done",
                "num_turns": 1,
                "duration_ms": 1,
                "session_id": session_id,
                "created_at": "2026-10-10T12:00:00",
                "request_id": "req-stub",
            })
            .to_string();
            for tx in state.ws_clients.values() {
                let _ = tx.send(frame.clone());
            }
            (200, json!({"ok": true, "request_id": "req-stub"}).to_string())
        }

        ("GET", "/api/session/info") => {
            let model_id = state
                .updated
                .get("model_id")
                .and_then(Value::as_str)
                .unwrap_or("dfmodel");
            (
                200,
                json!({
                    "model": model_id,
                    "model_id": model_id,
                    "provider_name": "stub",
                    "api_url": "http://stub",
                    "tools": ["Bash"],
                    "total_tokens": 0,
                    "context_window_tokens": 128000,
                    "thinking": state.updated.get("thinking").cloned().unwrap_or(json!(false)),
                    "reasoning_effort": state.updated.get("reasoning_effort").cloned().unwrap_or(Value::Null),
                    "yolo": state.updated.get("yolo").cloned().unwrap_or(json!(false)),
                    "session_name": state.updated.get("title").cloned().unwrap_or(Value::Null),
                    "workdir": "/tmp/stub-ws",
                    "status": state.status,
                    "context_stats": {"message_count": 4, "total_tokens": 40},
                })
                .to_string(),
            )
        }

        ("GET", "/api/session/get") => (
            200,
            json!({
                "session_id": SESSION_ID,
                "name": null,
                "template_name": "executor",
                "workspace": "/tmp/stub-ws",
                "status": state.status,
                "agent": null,
                "messages": [
                    {"role": "user", "uuid": "u-first", "content": "first question"},
                    {
                        "role": "assistant",
                        "uuid": "u-ask",
                        "content": "asking",
                        "tool_calls": [{
                            "id": "call-42",
                            "name": "ask_user",
                            "arguments": {"question": "proceed?"},
                        }],
                    },
                ],
            })
            .to_string(),
        ),

        ("POST", "/api/system/reload") => {
            let items: Vec<Value> = state
                .reload_items
                .iter()
                .map(|(name, ok, detail)| {
                    json!({"name": name, "ok": ok, "detail": detail})
                })
                .collect();
            let ok = state.reload_items.iter().all(|(_, ok, _)| *ok);
            (200, json!({"ok": ok, "results": items}).to_string())
        }

        ("POST", "/api/shutdown") => (200, json!({"status": "shutting_down"}).to_string()),

        _ => {
            let _ = query;
            (404, json!({"error": "not_found", "detail": path}).to_string())
        }
    }
}

/// `true` when `path` must 404 because the session is not loaded.
fn state_404(state: &StubState, path: &str) -> bool {
    !state.loaded && state.hydrate_paths.contains(&path)
}

fn not_loaded() -> (u16, String) {
    (
        404,
        json!({"error": "not_found", "detail": "session not found"}).to_string(),
    )
}

/// The gateway's archived-session 404: same status as a plain miss, a
/// different structured `error` — exactly what the CLI must branch on.
fn archived_404() -> (u16, String) {
    (
        404,
        json!({
            "error": "session_archived",
            "detail": "session 'archived-session' is archived; restore it before use",
        })
        .to_string(),
    )
}

/// The `ask` event (multi-question format) the stub reports as pending.
fn ask_event() -> Value {
    json!({
        "type": "ask",
        "session_id": SESSION_ID,
        "created_at": "2026-10-10T12:00:00",
        "request_id": "req-stub",
        "tool_call_id": "call-42",
        "questions": [{
            "id": "q1",
            "header": "verify",
            "question": "Proceed?",
            "multiSelect": false,
            "options": [
                {"label": "yes", "description": "go ahead"},
                {"label": "no", "description": ""},
            ],
        }],
    })
}

/// The `sync_session` snapshot pushed on subscribe: status + the still-active
/// fact events (a pending ask lives there).
fn session_snapshot(state: &StubState) -> String {
    let events: Vec<Value> = state.pending_ask.iter().cloned().collect();
    json!({
        "type": "sync_session",
        "session_id": SESSION_ID,
        "created_at": "2026-10-10T12:00:00",
        "request_id": "req-stub",
        "messages": [],
        "uncommitted": null,
        "uncommitted_tools": [],
        "events": events,
        "status": state.status,
        "turn_started_at": null,
        "agent": null,
        "name": null,
        "draft": null,
    })
    .to_string()
}

/// Query-string parameter (`?a=1&b=2`); no percent-decoding — session ids and
/// the values used here are plain ASCII.
fn query_param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

/// Case-insensitive header lookup on the request head.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

/// Read one HTTP request head (+ the declared body); the rest stays buffered.
async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<(String, String)> {
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let body_start = pos + 4;
            if buf.len() >= body_start + content_length {
                let body = String::from_utf8_lossy(&buf[body_start..body_start + content_length])
                    .to_string();
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

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Error",
    }
}

// ============================================================
// Harness
// ============================================================

/// Run the real binary and collect (exit code, stdout, stderr).
async fn run_wing(args: &[&str], stub: &Stub) -> (i32, String, String) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(args)
        .env("WING_HOME", &stub.home)
        .env_remove("RUST_LOG")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn().unwrap();
    let output = match tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await
    {
        Ok(result) => result.unwrap(),
        Err(_) => panic!("`wing {args:?}` did not exit in 30s"),
    };
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn json_of(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON value ({e}): {stdout:?}"))
}

/// Body of one recorded POST, parsed.
fn body_of(stub: &Stub, path: &str) -> Value {
    let hit = stub
        .path_hits(path)
        .pop()
        .unwrap_or_else(|| panic!("no request recorded for {path}: {:?}", stub.trace()));
    serde_json::from_str(&hit.body)
        .unwrap_or_else(|e| panic!("{path} body is not JSON ({e}): {:?}", hit.body))
}

// ============================================================
// Tests
// ============================================================

/// The uuid loop, end to end and CLI-only: `branches` prints the nodes (the
/// uuids are not truncated), `fork` / `rewind` take the printed uuid back.
#[tokio::test]
async fn branches_fork_and_rewind_close_the_uuid_loop() {
    let stub = Stub::start("branch-loop").await;

    // The human surface lists the uuids in full (they are what gets copied).
    let (code, stdout, stderr) = run_wing(&["branches", SESSION_ID], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    for uuid in ["u-first", "u-second", "current"] {
        assert!(stdout.contains(uuid), "{uuid} missing:\n{stdout}");
    }
    assert!(
        stdout.contains("second question with two lines"),
        "multi-line content is flattened into the cell:\n{stdout}"
    );

    // The machine surface carries the same targets; the uuid is taken from
    // here — no hand-written uuid anywhere in this test.
    let (code, stdout, stderr) = run_wing(&["branches", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let branches = json_of(&stdout);
    let target_uuid = branches["targets"][0]["uuid"].as_str().unwrap().to_string();
    assert_eq!(target_uuid, "u-first");

    let (code, stdout, stderr) =
        run_wing(&["fork", SESSION_ID, "--at", &target_uuid, "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let fork = json_of(&stdout);
    assert_eq!(fork["session_id"], "forked-by-stub");
    assert_eq!(fork["source_session_id"], SESSION_ID);
    assert_eq!(fork["target_uuid"], target_uuid);
    assert_eq!(fork["draft"], "first question");
    assert_eq!(
        body_of(&stub, "/api/session/fork"),
        json!({"source_session_id": SESSION_ID, "target_uuid": target_uuid}),
        "the fork request carries exactly the uuid branches printed"
    );

    let (code, stdout, stderr) =
        run_wing(&["rewind", SESSION_ID, "--to", &target_uuid], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("draft:"), "{stdout}");
    assert!(stdout.contains("first question"), "{stdout}");
    assert_eq!(
        body_of(&stub, "/api/session/rewind"),
        json!({"session_id": SESSION_ID, "target_uuid": target_uuid})
    );

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `interrupt` flips a working session to idle; the `int` alias is the same
/// command.
#[tokio::test]
async fn interrupt_turns_working_into_idle() {
    let stub = Stub::start("interrupt").await;
    stub.with(|state| state.status = "working".into());

    let (code, stdout, stderr) = run_wing(&["info", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(json_of(&stdout)["status"], "working");

    let (code, stdout, stderr) = run_wing(&["interrupt", SESSION_ID], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains(SESSION_ID), "{stdout}");
    assert_eq!(
        body_of(&stub, "/api/session/interrupt"),
        json!({"session_id": SESSION_ID})
    );

    let (code, stdout, stderr) = run_wing(&["info", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        json_of(&stdout)["status"],
        "idle",
        "interrupt must reach the gateway (the stub flips on it)"
    );

    // The visible alias is the same command.
    let (code, stdout, stderr) = run_wing(&["int", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        json_of(&stdout),
        json!({"ok": true, "session_id": SESSION_ID})
    );

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `update` sends **only** the fields given (partial update is the whole
/// point), maps the flags to the wire vocabulary, and refuses an empty call
/// before sending anything.
#[tokio::test]
async fn update_sends_only_the_fields_given() {
    let stub = Stub::start("update").await;

    let (code, stdout, stderr) = run_wing(
        &[
            "update", SESSION_ID, "--model", "ds-flash", "--title", "nightly",
        ],
        &stub,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("model_id"), "{stdout}");
    assert_eq!(
        body_of(&stub, "/api/session/update"),
        json!({"session_id": SESSION_ID, "model_id": "ds-flash", "title": "nightly"}),
        "未给字段（thinking / yolo / tools …）一个都不能上线"
    );

    // The update landed: the stub mirrors it back through `info`.
    let (code, stdout, stderr) = run_wing(&["info", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let info = json_of(&stdout);
    assert_eq!(info["model_id"], "ds-flash");
    assert_eq!(info["session_name"], "nightly");

    // Booleans: bare flag = on, `off` = off — and still nothing else rides along.
    let (code, _, stderr) = run_wing(&["update", SESSION_ID, "--yolo"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/update"),
        json!({"session_id": SESSION_ID, "yolo": true})
    );

    let (code, _, stderr) = run_wing(&["update", SESSION_ID, "--thinking", "off"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/update"),
        json!({"session_id": SESSION_ID, "thinking": false})
    );

    // `--tools` is a full replacement (comma-separated refs → array).
    let (code, _, stderr) =
        run_wing(&["update", SESSION_ID, "--tools", "core.Bash, Read"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/update"),
        json!({"session_id": SESSION_ID, "tools": ["core.Bash", "Read"]})
    );

    // No fields at all: refused **before** any request (the endpoint would 400
    // too, but the CLI can say what is missing).
    let (code, stdout, stderr) = run_wing(&["update", SESSION_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("no update fields given"), "{stderr}");
    assert!(stderr.contains("--model"), "{stderr}");
    assert_eq!(
        stub.path_hits("/api/session/update").len(),
        4,
        "the refused call must not reach the wire"
    );

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `compact` without an instruction sends no `instruction` key at all (the
/// default strategy is the gateway's), with one it passes it through.
#[tokio::test]
async fn compact_passes_the_instruction_through() {
    let stub = Stub::start("compact").await;

    let (code, stdout, stderr) = run_wing(&["compact", SESSION_ID], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("12345"), "{stdout}");
    assert!(stdout.contains("2345"), "{stdout}");
    assert_eq!(
        body_of(&stub, "/api/session/compact"),
        json!({"session_id": SESSION_ID}),
        "无 instruction 时该键必须缺席（默认策略由网关决定）"
    );

    let (code, _stdout, stderr) =
        run_wing(&["compact", SESSION_ID, "keep the design decisions"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/compact"),
        json!({"session_id": SESSION_ID, "instruction": "keep the design decisions"})
    );
    let compacted = json_of(&run_wing(&["compact", SESSION_ID, "--json"], &stub).await.1);
    assert_eq!(
        compacted,
        json!({
            "ok": true,
            "session_id": SESSION_ID,
            "original_tokens": 12345,
            "compressed_tokens": 2345,
        })
    );

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `reload` prints the items in the gateway's own order (that order is the
/// contract) and exits non-zero when the reload failed.
#[tokio::test]
async fn reload_reports_items_in_contract_order() {
    let stub = Stub::start("reload").await;

    let (code, stdout, stderr) = run_wing(&["reload"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let names = [
        "config.yaml",
        "hooks",
        "prompt commands",
        "provider",
        "skills & rules",
        "log level",
    ];
    let positions: Vec<usize> = names
        .iter()
        .map(|name| {
            stdout
                .find(name)
                .unwrap_or_else(|| panic!("{name} missing:\n{stdout}"))
        })
        .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "报告顺序必须与网关下发的名字序一致：{positions:?}\n{stdout}"
    );

    // `--json` is the raw response.
    let (code, stdout, _) = run_wing(&["reload", "--json"], &stub).await;
    assert_eq!(code, 0);
    assert_eq!(json_of(&stdout)["ok"], true);
    assert_eq!(json_of(&stdout)["results"][0]["name"], "config.yaml");

    // One failed item: reported as-is, exit non-zero.
    stub.with(|state| {
        state.reload_items[3] = ("provider".into(), false, Some("unknown protocol".into()));
    });
    let (code, stdout, stderr) = run_wing(&["reload"], &stub).await;
    assert_ne!(code, 0, "a failed reload must exit non-zero");
    assert!(stderr.is_empty(), "{stderr}");
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains("provider"), "{stdout}");
    assert!(stdout.contains("unknown protocol"), "{stdout}");
    // 其余项照常上报（失败不中止）。
    assert!(stdout.contains("log level"), "{stdout}");

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `new` creates (workspace defaults to cwd, tags are read back) and `resume`
/// hydrates + summarizes; an unknown session is a clean non-zero 404.
#[tokio::test]
async fn new_and_resume_round_trip() {
    let stub = Stub::start("new-resume").await;

    let (code, stdout, stderr) = run_wing(
        &["new", "--agent", "executor", "--tag", "nightly", "--json"],
        &stub,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let new = json_of(&stdout);
    assert_eq!(new["session_id"], "created-by-stub");
    assert_eq!(new["template_name"], "executor");
    assert_eq!(new["tags"], json!(["nightly"]), "标签读回，不靠回显");

    let create_body = body_of(&stub, "/api/session/create");
    assert_eq!(create_body["template_name"], "executor");
    assert_eq!(create_body["tags"], json!(["nightly"]));
    assert!(
        create_body["workspace"]
            .as_str()
            .is_some_and(|w| !w.is_empty()),
        "没给 --workspace 时用本进程 cwd：{create_body}"
    );

    let (code, stdout, stderr) = run_wing(&["resume", SESSION_ID], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("status:"), "{stdout}");
    assert!(stdout.contains("executor"), "{stdout}");
    assert_eq!(
        body_of(&stub, "/api/session/resume"),
        json!({"session_id": SESSION_ID})
    );

    // The summary is the state *after* hydration, not an echo of the request.
    let (code, stdout, _) = run_wing(&["resume", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0);
    let resumed = json_of(&stdout);
    assert_eq!(resumed["session_id"], SESSION_ID);
    assert_eq!(resumed["status"], "idle");
    assert_eq!(resumed["message_count"], 4);

    // Unknown session: 404 → one clear stderr line, non-zero exit.
    let (code, stdout, stderr) = run_wing(&["resume", UNKNOWN_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains(UNKNOWN_ID), "{stderr}");
    assert!(stderr.contains("not found"), "{stderr}");

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `wing run --agent`: the chosen agent rides the create request as
/// `template_name` and comes back in the output; without the flag the gateway
/// default decides (the request carries no template).
#[tokio::test]
async fn run_carries_the_agent_flag() {
    let stub = Stub::start("run-agent").await;

    let (code, stdout, stderr) = run_wing(
        &["run", "--agent", "executor", "-p", "just ok", "--json"],
        &stub,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let out = json_of(&stdout);
    assert_eq!(out["session_id"], "created-by-stub");
    assert_eq!(out["template_name"], "executor");
    assert_eq!(
        body_of(&stub, "/api/session/create")["template_name"],
        "executor"
    );
    assert_eq!(body_of(&stub, "/api/session/send")["content"], "just ok");

    // Without --agent the key is absent: the gateway's default template decides.
    let stub = Stub::start("run-default-agent").await;
    let (code, stdout, stderr) = run_wing(&["run", "-p", "just ok", "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let out = json_of(&stdout);
    assert_eq!(out["template_name"], "default");
    let create_body = body_of(&stub, "/api/session/create");
    assert!(
        create_body.get("template_name").is_none(),
        "键必须缺席（而不是显式 null）：{create_body}"
    );

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// The stdio frontend carries the agent too: `wing -p ... --agent executor`
/// creates a session on that template. The flag lives on the top-level `Cli`
/// for exactly this path — the stdio argument filter only keeps flags it knows
/// (see `filter_keeps_agent_and_its_value`), so without it the flag and its
/// value would be dropped and the session would silently fall back to the
/// default template.
#[tokio::test]
async fn stdio_prompt_carries_the_agent_flag() {
    let stub = Stub::start("stdio-agent").await;

    let (code, _stdout, stderr) = run_wing(&["-p", "hi", "--agent", "executor"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/create")["template_name"],
        "executor"
    );

    // `--session-id` on a **fresh** id is a create, so `--agent` applies there
    // too (the pre-request warning says exactly this: ignored only when the id
    // already exists). The requested id survives round-trip verbatim.
    let (code, _stdout, stderr) = run_wing(
        &[
            "-p",
            "hi",
            "--session-id",
            "fresh-agent-id",
            "--agent",
            "executor",
        ],
        &stub,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let create_body = body_of(&stub, "/api/session/create");
    assert_eq!(create_body["session_id"], "fresh-agent-id");
    assert_eq!(create_body["template_name"], "executor");

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// Answering an Ask: `--tool-call-id` reaches `/api/session/send`, and the id
/// itself is discoverable through `wing tail` (the tool call line carries it).
#[tokio::test]
async fn ask_answer_carries_the_tool_call_id() {
    let stub = Stub::start("ask-answer").await;

    let (code, _stdout, stderr) = run_wing(
        &[
            "run",
            "-r",
            SESSION_ID,
            "-p",
            "yes, proceed",
            "--tool-call-id",
            "call-42",
        ],
        &stub,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/send"),
        json!({
            "session_id": SESSION_ID,
            "content": "yes, proceed",
            "tool_call_id": "call-42",
        }),
        "回答 Ask 必须带上 tool_call_id（定向 resolve feedback waiter）"
    );

    // Without the flag the key is absent — the message takes the inbox path.
    let (code, _, stderr) = run_wing(&["run", "-r", SESSION_ID, "-p", "next task"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let sent = body_of(&stub, "/api/session/send");
    assert_eq!(
        sent,
        json!({"session_id": SESSION_ID, "content": "next task"})
    );
    assert!(sent.get("tool_call_id").is_none(), "{sent}");

    // Once the ask is committed the call is on the chain, so `wing tail`
    // shows the id too (the *pending* discovery path is `wing asks`, covered
    // by the asks tests).
    let (code, stdout, stderr) = run_wing(&["tail", SESSION_ID, "-t", "tool_call"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("call-42"), "{stdout}");
    assert!(stdout.contains("ask_user"), "{stdout}");

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// The hydration convention, session-scoped: every command that needs a loaded
/// session resumes once on 404 and retries.
#[tokio::test]
async fn evicted_sessions_are_hydrated_once_on_404() {
    let stub = Stub::start("hydrate").await;
    stub.with(|state| {
        state.loaded = false;
        state.hydrate_paths = vec![
            "/api/session/branches",
            "/api/session/fork",
            "/api/session/rewind",
            "/api/session/compact",
            "/api/session/update",
        ];
    });

    let commands: &[(&str, &[&str])] = &[
        ("branches", &["branches", SESSION_ID]),
        ("fork", &["fork", SESSION_ID, "--at", "u-first"]),
        ("rewind", &["rewind", SESSION_ID, "--to", "u-first"]),
        ("compact", &["compact", SESSION_ID]),
        ("update", &["update", SESSION_ID, "--title", "t"]),
    ];

    for (name, args) in commands {
        stub.with(|state| state.loaded = false);
        let before = stub.requests().len();
        let (code, _, stderr) = run_wing(args, &stub).await;
        assert_eq!(code, 0, "{name}: stderr: {stderr}");

        let trace = stub.trace()[before..].to_vec();
        let paths: Vec<&str> = trace.iter().map(|(_, path, _)| path.as_str()).collect();
        assert!(
            paths.contains(&"/api/session/resume"),
            "{name}: 404 后必须先水合：{paths:?}"
        );
        assert_eq!(
            paths
                .iter()
                .filter(|path| **path == "/api/session/resume")
                .count(),
            1,
            "{name}: 只水合一次：{paths:?}"
        );
        let statuses: Vec<u16> = stub.requests()[before..]
            .iter()
            .map(|request| request.status)
            .collect();
        assert_eq!(
            statuses.first(),
            Some(&404),
            "{name}: 第一次调用应当命中 404（模拟已逐出）：{statuses:?}"
        );
        assert_eq!(
            statuses.last(),
            Some(&200),
            "{name}: 重试必须成功：{statuses:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `interrupt` deliberately does **not** hydrate: a session the gateway does
/// not hold has nothing to interrupt, and the error says so.
#[tokio::test]
async fn interrupt_refuses_to_hydrate_a_session_it_cannot_reach() {
    let stub = Stub::start("interrupt-404").await;
    stub.with(|state| state.loaded = false);

    let (code, stdout, stderr) = run_wing(&["interrupt", SESSION_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("not loaded"), "{stderr}");
    assert!(stderr.contains(SESSION_ID), "{stderr}");
    let paths: Vec<String> = stub
        .requests()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(
        paths,
        vec!["/api/session/interrupt"],
        "不该为中断水合一个没有在跑的东西"
    );

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `wing asks` reports a **pending** ask from the live session snapshot — the
/// only place its `tool_call_id` exists while the ask is unresolved (the tool
/// call is not on the chain yet, so `wing tail` cannot see it) — and the id it
/// prints is exactly what answers the ask.
#[tokio::test]
async fn asks_reports_a_pending_ask_and_its_tool_call_id() {
    let stub = Stub::start("asks-snapshot").await;
    stub.with(|state| {
        state.pending_ask = Some(ask_event());
        state.status = "waiting".into();
    });

    let (code, stdout, stderr) = run_wing(&["asks", SESSION_ID], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("tool_call_id: call-42"), "{stdout}");
    assert!(stdout.contains("[verify] Proceed?"), "{stdout}");
    assert!(stdout.contains("yes — go ahead"), "{stdout}");

    // The machine surface: the id is right there for the answering command.
    let (code, stdout, stderr) = run_wing(&["asks", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let asks = json_of(&stdout);
    assert_eq!(asks["status"], "waiting");
    assert_eq!(asks["asks"][0]["tool_call_id"], "call-42");
    let tool_call_id = asks["asks"][0]["tool_call_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Closed loop, CLI only: the printed id answers the ask.
    let (code, _, stderr) = run_wing(
        &[
            "run",
            "-r",
            SESSION_ID,
            "-p",
            "yes",
            "--tool-call-id",
            &tool_call_id,
        ],
        &stub,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/send")["tool_call_id"],
        "call-42"
    );

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// Nothing pending: the report says so (exit 0, it is a query); `--wait`
/// blocks and then fails loudly — a caller that asked to wait must hear that
/// no ask came.
#[tokio::test]
async fn asks_with_nothing_pending_reports_and_wait_fails_loudly() {
    let stub = Stub::start("asks-empty").await;

    let (code, stdout, stderr) = run_wing(&["asks", SESSION_ID], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("No pending asks"), "{stdout}");
    assert!(stdout.contains("idle"), "{stdout}");

    let (code, stdout, _) = run_wing(&["asks", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0);
    assert_eq!(
        json_of(&stdout),
        json!({"session_id": SESSION_ID, "status": "idle", "asks": []})
    );

    let (code, stdout, stderr) = run_wing(&["asks", SESSION_ID, "--wait", "1"], &stub).await;
    assert_ne!(
        code, 0,
        "a wait window that closes with no ask is a failure"
    );
    assert!(stdout.contains("No pending asks"), "{stdout}");
    assert!(stderr.contains("no ask appeared within 1s"), "{stderr}");

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `--wait` catches an ask that arrives **after** the snapshot (the common
/// race: a turn in flight has not reached its ask yet).
#[tokio::test]
async fn asks_wait_catches_a_live_ask() {
    let stub = Stub::start("asks-live").await;
    stub.with(|state| {
        state.status = "working".into();
        state.ask_after_subscribe_ms = Some(300);
    });

    let (code, stdout, stderr) = run_wing(&["asks", SESSION_ID, "--wait", "10"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("tool_call_id: call-42"), "{stdout}");
    assert!(stdout.contains("Proceed?"), "{stdout}");

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// The stdio frontend carries the directed answer too: `wing -p ... --tool-call-id`
/// reaches `/api/session/send` with the id. The flag lives on the top-level Cli
/// for exactly this path — the stdio argument filter only keeps flags it knows
/// (see `filter_keeps_tool_call_id_and_its_value`), so without it the id would
/// be dropped and the message would silently become a plain one.
#[tokio::test]
async fn stdio_prompt_carries_the_tool_call_id() {
    let stub = Stub::start("stdio-ask").await;

    let (code, _stdout, stderr) = run_wing(
        &[
            "-p",
            "yes, proceed",
            "-r",
            SESSION_ID,
            "--tool-call-id",
            "call-42",
        ],
        &stub,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/send"),
        json!({
            "session_id": SESSION_ID,
            "content": "yes, proceed",
            "tool_call_id": "call-42",
        })
    );

    // Without the flag the message is a plain one (no `tool_call_id` key).
    let (code, _, stderr) = run_wing(&["-p", "next task", "-r", SESSION_ID], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let sent = body_of(&stub, "/api/session/send");
    assert!(sent.get("tool_call_id").is_none(), "{sent}");

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// `wing restart` acts on **one** endpoint — the one it was given (config or
/// `--port`), never "the config for the stop half, the flag for the start
/// half". Regression: the stop half used to re-read the config, so
/// `restart --port X` stopped the config gateway and started an orphan on X.
///
/// The stub stays reachable after the shutdown, so this also pins the other
/// half of the contract: a stop that does not finish is a **failed** restart
/// (non-zero, `ok: false`, and nothing started), not a success with a warning —
/// starting anyway would make `wing start` see the dying process as "already
/// running" and skip the spawn, leaving no gateway behind at all.
#[tokio::test]
async fn restart_acts_on_the_endpoint_it_was_given() {
    let config_stub = Stub::start("restart-config").await;
    let target_stub = Stub::start("restart-target").await;

    let (code, stdout, stderr) = run_wing(
        &["restart", "--port", &target_stub.port.to_string(), "--json"],
        &config_stub,
    )
    .await;

    assert_eq!(
        config_stub.path_hits("/api/shutdown").len(),
        0,
        "the config endpoint must not be stopped while the flagged one is restarted"
    );
    assert_eq!(
        target_stub.path_hits("/api/shutdown").len(),
        1,
        "the stop half and the start half must share one endpoint"
    );

    assert_ne!(code, 0, "an unfinished stop must fail the restart");
    assert_eq!(
        json_of(&stdout),
        json!({
            "ok": false,
            "host": "127.0.0.1",
            "port": target_stub.port,
            "stop": "shutdown_initiated",
        })
    );
    assert!(
        stderr.contains("still reachable") && stderr.contains("no new gateway was started"),
        "{stderr}"
    );

    let _ = std::fs::remove_dir_all(&config_stub.home);
    let _ = std::fs::remove_dir_all(&target_stub.home);
}

/// A GET endpoint with an unknown session 404s like the POST ones (the stub
/// reads `session_id` from the query, which is where GET puts it).
#[tokio::test]
async fn unknown_session_on_a_get_endpoint_is_a_clean_404() {
    let stub = Stub::start("get-404").await;

    let (code, stdout, stderr) = run_wing(&["branches", UNKNOWN_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains(UNKNOWN_ID), "{stderr}");
    assert!(stderr.contains("not found"), "{stderr}");

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// A live ask that outruns the snapshot is reported the same way as one that
/// is in it — including the status, which is `waiting` because an ask is
/// pending, not whatever the session was doing a moment earlier.
#[tokio::test]
async fn asks_reports_an_ask_that_outran_the_snapshot() {
    let stub = Stub::start("asks-live-first").await;
    stub.with(|state| {
        state.status = "working".into();
        state.ask_before_snapshot = true;
    });

    let (code, stdout, stderr) = run_wing(&["asks", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let asks = json_of(&stdout);
    assert_eq!(asks["asks"][0]["tool_call_id"], "call-42");
    assert_eq!(
        asks["status"], "waiting",
        "an outstanding ask is what `waiting` means: {asks}"
    );

    let _ = std::fs::remove_dir_all(&stub.home);
}

/// A snapshot entry whose type is known but whose payload does not decode is a
/// protocol error, not an empty answer: reporting "no pending asks" with exit
/// 0 for a session that may be blocked on one would send the caller's
/// `--tool-call-id` loop spinning against nothing.
#[tokio::test]
async fn asks_fails_loudly_on_a_drifted_ask_event() {
    let stub = Stub::start("asks-drift").await;
    stub.with(|state| {
        state.status = "waiting".into();
        // An `ask` this build cannot decode (unknown field shape).
        state.pending_ask = Some(json!({
            "type": "ask",
            "session_id": SESSION_ID,
            "created_at": "2026-10-10T12:00:00",
            "request_id": "req-stub",
            "tool_call_id": "call-42",
            "questions": "not-an-array",
        }));
    });

    let (code, stdout, stderr) = run_wing(&["asks", SESSION_ID], &stub).await;
    assert_ne!(code, 0, "stdout: {stdout}");
    assert!(
        !stdout.contains("No pending asks"),
        "a drifted ask must not read as `none`: {stdout}"
    );
    assert!(stderr.contains("cannot decode"), "{stderr}");
    assert!(stderr.contains("`ask`"), "{stderr}");

    let _ = std::fs::remove_dir_all(&stub.home);
}

// ============================================================
// 归档（wing archive）
// ============================================================

/// 归档一条会话：请求体是"设置状态"（archived=true），回执进结果表。
#[tokio::test]
async fn archive_moves_the_session_and_reports_it() {
    let stub = Stub::start("archive-move").await;
    let (code, stdout, stderr) = run_wing(&["archive", SESSION_ID], &stub).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        body_of(&stub, "/api/session/archive"),
        json!({"session_id": SESSION_ID, "archived": true}),
        "归档 = 把状态设为 true（不是动作对）"
    );
    assert!(stdout.contains("archived"), "{stdout}");
    assert!(stdout.contains(SESSION_ID), "{stdout}");

    // --json 与文本同源。
    let (code, stdout, _) = run_wing(&["archive", SESSION_ID, "--json"], &stub).await;
    assert_eq!(code, 0);
    let parsed = json_of(&stdout);
    assert_eq!(parsed["results"][0]["archived"], json!(true));
    assert_eq!(parsed["results"][0]["changed"], json!(true));
}

/// 恢复是同一个子命令的另一个方向；幂等 no-op 是 skipped，不是错误。
#[tokio::test]
async fn archive_restore_direction_and_idempotent_skip() {
    let stub = Stub::start("archive-restore").await;
    let (code, stdout, stderr) = run_wing(&["archive", "--restore", SETTLED_ID], &stub).await;
    assert_eq!(code, 0, "{stderr}");
    let body = body_of(&stub, "/api/session/archive");
    assert_eq!(
        body["archived"],
        json!(false),
        "--restore 发的是 archived=false"
    );
    assert!(stdout.contains("skipped"), "{stdout}");
    assert!(stdout.contains("not archived"), "{stdout}");
}

/// 单会话失败（409）只让它那一行是 error，其余照常，退出码非零。
#[tokio::test]
async fn archive_conflicts_fail_their_row_only() {
    let stub = Stub::start("archive-conflict").await;
    let (code, stdout, _) = run_wing(&["archive", BUSY_ID, SESSION_ID], &stub).await;
    assert_ne!(code, 0, "失败行必须让退出码非零");
    assert!(stdout.contains("status=working"), "{stdout}");
    assert!(stdout.contains("archived"), "其余行照常处理：{stdout}");
    assert_eq!(
        stub.path_hits("/api/session/archive").len(),
        2,
        "两个 id 都必须发出（单会话失败不阻断其余）"
    );
}

/// 列表渲染：表格 / JSON 同源；多字节时间戳不得 panic（B2 回归）。
#[tokio::test]
async fn archive_list_renders_the_index_and_survives_multibyte_timestamps() {
    let stub = Stub::start("archive-list").await;
    let (code, stdout, stderr) = run_wing(&["archive", "--list"], &stub).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("old chat"), "{stdout}");
    assert!(stdout.contains("2026-10-01 10:20"), "{stdout}");
    assert!(stdout.contains("DeepSeek Flash"), "{stdout}");
    assert!(stdout.contains("favorite"), "{stdout}");
    assert!(stdout.contains("日本語のセッション"), "{stdout}");

    let (code, stdout, _) = run_wing(&["archive", "--list", "--json"], &stub).await;
    assert_eq!(code, 0);
    let parsed = json_of(&stdout);
    assert_eq!(parsed[0]["id"], json!(ARCHIVED_ID));
    assert_eq!(parsed[1]["archived_at"], json!("日本語日本語日本語日"));

    // 形态互斥是使用错误：网关不可达也拦得住（校验先于 ensure_gateway）。
    let (code, _, stderr) = run_wing(&["archive", "--list", SESSION_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("--list cannot be combined"), "{stderr}");
}

/// 归档会话的 404 必须是可行动提示（结构化码 → CLI 文案），不是普通的
/// "not found"，也不是裸的网关 detail（info 与 tail 两条链都验一遍）。
#[tokio::test]
async fn archived_session_gets_the_restore_hint_not_a_plain_404() {
    let stub = Stub::start("archive-hint").await;

    let (code, _, stderr) = run_wing(&["info", ARCHIVED_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("is archived"), "{stderr}");
    assert!(stderr.contains("wing archive --restore"), "{stderr}");
    assert!(
        !stderr.contains("not loaded and not on disk"),
        "归档提示不得退化成普通 404 文案：{stderr}"
    );

    let (code, _, stderr) = run_wing(&["tail", ARCHIVED_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("wing archive --restore"), "{stderr}");

    let (code, _, stderr) = run_wing(&["interrupt", ARCHIVED_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("is archived"), "{stderr}");
    assert!(stderr.contains("wing archive --restore"), "{stderr}");

    // stdio（`-p -r`）同样走统一文案：编排方看到的是可行动的恢复指引。
    let (code, _, stderr) = run_wing(&["-p", "hi", "-r", ARCHIVED_ID], &stub).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("wing archive --restore"), "{stderr}");

    // `run -r` 也走统一文案（编排方拿到的不是裸网关 detail）。
    let (code, _, stderr) = run_wing(&["run", "-r", ARCHIVED_ID, "-p", "hi"], &stub).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("wing archive --restore"), "{stderr}");

    // 内存态查询（get / info）也回结构化码：裸 API 消费方看得出来"它没消失，
    // 只是被归档了"——上面 info / tail 两条链各留下一条记录。
    for endpoint in ["session/info", "session/get"] {
        let hits = stub.path_hits(&format!("/api/{endpoint}"));
        let hit = hits
            .last()
            .unwrap_or_else(|| panic!("no request recorded for {endpoint}"));
        assert_eq!(hit.status, 404, "{endpoint} 应为 404");
    }
}
