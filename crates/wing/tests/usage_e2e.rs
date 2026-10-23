//! End-to-end: `wing usage` and the `wing info` usage block, driven through the
//! real binary against a stub gateway.
//!
//! Unit tests pin the rendering; wiring is exactly what they cannot show. These
//! tests read the wire and pin what a caller depends on:
//!
//! 1. **The request path**: `/api/usage` with the query the plan resolved
//!    (`since`/`until`/`group_by`/`limit`), including "an old-by-default flag is
//!    not sent at all".
//! 2. **The output contract**: text carries a title / a heatmap row / a `TOTAL`
//!    line; `--json` is the endpoint response verbatim (no local envelope).
//! 3. **Exit codes and the gate order**: a bad `--limit` exits non-zero *before*
//!    the gateway is even probed; an unknown `--session` is the gateway's 404
//!    turned into one sentence.
//! 4. **`wing info` gains a block without changing its `--json` shape.**

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

mod common;

use common::e2e::{read_request, run_wing, start_fake_gateway};

const SESSION_ID: &str = "20261023-101500-abcdef01";
const UNKNOWN_ID: &str = "ghost-session";

// ============================================================
// Stub gateway
// ============================================================

/// One request the CLI sent, with the status the stub answered.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    query: String,
    status: u16,
}

#[derive(Default)]
struct StubState {
    requests: Vec<Recorded>,
    /// `/api/usage` 回 404「未知路由」——一个**还不服务这个端点**的老网关。
    usage_missing: bool,
}

struct Stub {
    state: Arc<Mutex<StubState>>,
    home: std::path::PathBuf,
}

impl Stub {
    async fn start(tag: &str) -> Self {
        let state = Arc::new(Mutex::new(StubState::default()));
        let (home, _port) = start_fake_gateway("wing-usage", tag, state.clone(), handle_conn).await;
        Self { state, home }
    }

    /// Requests seen so far (`/api/health` filtered out: every command probes it
    /// on startup, so it is transport noise in assertions about a command's own
    /// calls).
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
}

async fn handle_conn(mut stream: TcpStream, state: Arc<Mutex<StubState>>) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some((head, _body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
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
            let guard = state.lock().unwrap();
            route(&path, &query, &guard)
        };
        state.lock().unwrap().requests.push(Recorded {
            method,
            path,
            query,
            status,
        });

        let response = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            if status == 200 { "OK" } else { "Not Found" },
            response_body.len(),
            response_body
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn route(path: &str, query: &str, state: &StubState) -> (u16, String) {
    match path {
        "/api/health" => (
            200,
            json!({
                "service": "wing-gateway",
                "status": "ok",
                "version": "test",
                "uptime": 1,
            })
            .to_string(),
        ),
        "/api/usage" if state.usage_missing => (
            404,
            json!({"error": "not_found", "detail": "Not Found"}).to_string(),
        ),
        "/api/usage" => {
            let params = parse_query(query);
            if params.get("session_id").map(String::as_str) == Some(UNKNOWN_ID) {
                return (
                    404,
                    json!({"error": "not_found", "detail": "session not found: 'ghost-session'"})
                        .to_string(),
                );
            }
            let group_by = params
                .get("group_by")
                .cloned()
                .unwrap_or_else(|| "day".to_string());
            (200, usage_body(&group_by, &params))
        }
        "/api/session/info" => (200, session_info_body()),
        "/api/session/list" => (200, json!({"sessions": []}).to_string()),
        _ => (
            404,
            json!({"error": "not_found", "detail": path}).to_string(),
        ),
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (pair.to_string(), String::new()),
        })
        .collect()
}

/// 11 个度量的全零填充 + 一组「今天很忙」的数字（值不重要，形状是契约）。
fn measures(prompt: i64, completion: i64, calls: i64) -> Value {
    json!({
        "sessions": 2,
        "sessions_created": 1,
        "user_messages": 4,
        "llm_calls": calls,
        "tool_calls": 6,
        "tool_errors": 0,
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "cached_tokens": prompt,
        "llm_time_ms": 1_000,
        "tool_time_ms": 500,
    })
}

fn usage_body(group_by: &str, params: &HashMap<String, String>) -> String {
    let since = params
        .get("since")
        .cloned()
        .unwrap_or_else(|| "all".to_string());
    let until = params
        .get("until")
        .cloned()
        .unwrap_or_else(|| "2026-10-23".to_string());
    let (rows, truncated) = match group_by {
        "origin" => (
            vec![
                json!({"key": "me", "label": null, "parent_session_id": null,
                       "sessions": 1, "sessions_created": 1, "user_messages": 4,
                       "llm_calls": 3, "tool_calls": 6, "tool_errors": 0,
                       "prompt_tokens": 1200, "completion_tokens": 10,
                       "cached_tokens": 1200, "llm_time_ms": 1000, "tool_time_ms": 500}),
                json!({"key": "agents", "label": null, "parent_session_id": null,
                       "sessions": 1, "sessions_created": 0, "user_messages": 0,
                       "llm_calls": 0, "tool_calls": 0, "tool_errors": 0,
                       "prompt_tokens": 0, "completion_tokens": 0,
                       "cached_tokens": 0, "llm_time_ms": 0, "tool_time_ms": 0}),
            ],
            false,
        ),
        "session" => (
            vec![json!({
                "key": params.get("session_id").cloned().unwrap_or_default(),
                "label": null,
                "parent_session_id": null,
                "sessions": 1, "sessions_created": 1, "user_messages": 4,
                "llm_calls": 3, "tool_calls": 6, "tool_errors": 0,
                "prompt_tokens": 1200, "completion_tokens": 10,
                "cached_tokens": 1200, "llm_time_ms": 1000, "tool_time_ms": 500,
            })],
            false,
        ),
        "model" => (
            vec![json!({
                "key": "gateway/deepseek-flash",
                "label": "DeepSeek-Flash",
                "parent_session_id": null,
                "sessions": 2, "sessions_created": 1, "user_messages": 4,
                "llm_calls": 3, "tool_calls": 0, "tool_errors": 0,
                "prompt_tokens": 1200, "completion_tokens": 10,
                "cached_tokens": 1200, "llm_time_ms": 1000, "tool_time_ms": 0,
            })],
            false,
        ),
        _ => (
            vec![
                json!({"key": "2026-10-22", "label": null, "parent_session_id": null,
                       "sessions": 1, "sessions_created": 0, "user_messages": 1,
                       "llm_calls": 1, "tool_calls": 2, "tool_errors": 0,
                       "prompt_tokens": 500, "completion_tokens": 5,
                       "cached_tokens": 400, "llm_time_ms": 400, "tool_time_ms": 100}),
                json!({"key": "2026-10-23", "label": null, "parent_session_id": null,
                       "sessions": 2, "sessions_created": 1, "user_messages": 4,
                       "llm_calls": 3, "tool_calls": 6, "tool_errors": 0,
                       "prompt_tokens": 1200, "completion_tokens": 10,
                       "cached_tokens": 1200, "llm_time_ms": 1000, "tool_time_ms": 500}),
            ],
            false,
        ),
    };
    json!({
        "range": {"since": since, "until": until, "data_since": "2026-10-20"},
        "group_by": group_by,
        "totals": measures(1_700, 15, 4),
        "rows": rows,
        "truncated": truncated,
    })
    .to_string()
}

fn session_info_body() -> String {
    json!({
        "model": "deepseek-flash",
        "api_url": "http://127.0.0.1:39998",
        "tools": ["Bash", "Read"],
        "total_tokens": 1234,
        "context_window_tokens": 128000,
        "thinking": false,
        "reasoning_effort": null,
        "yolo": false,
        "session_name": "usage probe",
        "status": "idle",
        "context_stats": {"message_count": 12, "total_tokens": 1234},
        "usage": {"turns": 3, "llm_calls": 5, "tool_calls": 7,
                  "user_messages": 3, "prompt_tokens": 1200,
                  "completion_tokens": 10, "cached_tokens": 900,
                  "llm_time_ms": 1000, "tool_time_ms": 500},
    })
    .to_string()
}

// ============================================================
// Tests
// ============================================================

fn today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

#[tokio::test]
async fn usage_json_is_the_endpoint_response_and_the_query_is_fixed() {
    let stub = Stub::start("json").await;
    let (code, stdout, stderr) = run_wing(&["usage", "--json", "--since", "7d"], &stub.home).await;
    assert_eq!(code, 0, "stderr: {stderr}");

    // 单个 JSON 行（`print_json_compact`）：端点响应原样，不发明本地包封。
    let value: Value = serde_json::from_str(stdout.trim()).expect("one JSON line");
    assert_eq!(value["group_by"], "day");
    assert_eq!(value["range"]["data_since"], "2026-10-20");
    assert_eq!(value["rows"][0]["key"], "2026-10-22");
    assert_eq!(value["rows"][1]["llm_calls"], 3);
    assert_eq!(value["totals"]["prompt_tokens"], 1700);

    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/api/usage");
    let since = (today() - chrono::Duration::days(7))
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(
        requests[0].query,
        format!(
            "since={since}&until={}&group_by=day",
            today().format("%Y-%m-%d")
        ),
        "日期由 CLI 翻译成网关本地日期，`--limit` 不出现在仪表盘请求里"
    );
}

#[tokio::test]
async fn usage_text_draws_the_dashboard() {
    let stub = Stub::start("dashboard").await;
    let (code, stdout, stderr) = run_wing(&["usage"], &stub.home).await;
    assert_eq!(code, 0, "stderr: {stderr}");

    assert!(stdout.contains("WING USAGE"), "{stdout}");
    assert!(stdout.contains("→"), "范围行：{stdout}");
    assert!(
        stdout.contains("less · ░ ▒ ▓ █ more"),
        "热力图图例：{stdout}"
    );
    assert!(stdout.contains("active"), "统计行：{stdout}");
    // WHO 表（含合计行）与 MODEL 表。
    assert!(stdout.contains("LLM CALLS"), "{stdout}");
    assert!(stdout.contains("total"), "{stdout}");
    assert!(stdout.contains("DeepSeek-Flash"), "{stdout}");

    // 三次并发请求：day / origin / model(limit=5)。
    let requests = stub.requests();
    let group_bys: Vec<&str> = requests
        .iter()
        .map(|request| {
            if request.query.contains("group_by=origin") {
                "origin"
            } else if request.query.contains("group_by=model") {
                "model"
            } else {
                "day"
            }
        })
        .collect();
    assert_eq!(requests.len(), 3, "{requests:?}");
    for dim in ["day", "origin", "model"] {
        assert!(group_bys.contains(&dim), "缺 {dim}：{group_bys:?}");
    }
    let model = requests
        .iter()
        .find(|request| request.query.contains("group_by=model"))
        .expect("model query");
    assert!(
        model.query.contains("limit=5"),
        "仪表盘 MODEL 表取前 5：{model:?}"
    );
}

#[tokio::test]
async fn a_breakdown_sends_its_dimension_and_limit() {
    let stub = Stub::start("model").await;
    let (code, stdout, stderr) = run_wing(
        &["usage", "--by", "model", "--limit", "3", "--json"],
        &stub.home,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");

    let requests = stub.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].query.contains("group_by=model") && requests[0].query.contains("limit=3"),
        "{requests:?}"
    );
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["group_by"], "model");
    assert_eq!(value["rows"][0]["label"], "DeepSeek-Flash");

    // 文本形态：MODEL 表 + TOTAL 行。
    let (code, stdout, _stderr) = run_wing(&["usage", "--by", "model"], &stub.home).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("PROVIDER"), "{stdout}");
    assert!(stdout.contains("TOTAL"), "{stdout}");
}

#[tokio::test]
async fn an_unknown_session_is_one_sentence_and_a_failure() {
    let stub = Stub::start("unknown").await;
    let (code, stdout, stderr) = run_wing(&["usage", "--session", UNKNOWN_ID], &stub.home).await;
    assert_eq!(code, 1, "stdout: {stdout}");
    assert!(stderr.contains("wing usage error:"), "{stderr}");
    assert!(
        stderr.contains(&format!("session not found: {UNKNOWN_ID}")),
        "{stderr}"
    );
    // 统计不该把会话读回内存：失败的仪表盘只会重复只读查询（含判定用的无作用域探测），
    // 没有 resume / create。
    let requests = stub.requests();
    assert!(!requests.is_empty(), "{requests:?}");
    assert!(
        requests.iter().all(|request| request.path == "/api/usage"),
        "只允许只读的用量查询：{requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| !request.query.contains("session_id")),
        "无作用域的探测请求是「会话不存在 vs 网关太旧」的判据：{requests:?}"
    );
}

#[tokio::test]
async fn a_bad_limit_exits_before_the_gateway_is_touched() {
    let stub = Stub::start("limit").await;
    let (code, stdout, stderr) = run_wing(&["usage", "--limit", "0"], &stub.home).await;
    assert_eq!(code, 1, "stdout: {stdout}");
    assert!(
        stderr.contains("--limit must be between 1 and 200 (got 0)"),
        "{stderr}"
    );
    assert!(
        stub.requests().is_empty(),
        "闸门在连接之前：{:?}",
        stub.requests()
    );
}

#[tokio::test]
async fn a_bad_tree_without_a_session_is_gated_too() {
    let stub = Stub::start("tree").await;
    let (code, _stdout, stderr) = run_wing(&["usage", "--tree"], &stub.home).await;
    assert_eq!(code, 1);
    assert!(stderr.contains("--tree requires --session"), "{stderr}");
    assert!(stub.requests().is_empty(), "{:?}", stub.requests());
}

/// 同一个 404 在两种上下文里是两句不同的话：带 `--session` 是「会话不存在」，
/// 不带它只能说明网关没有这个路由；两者都 404 时仍以「网关太旧」为准（那是根因）。
#[tokio::test]
async fn a_gateway_without_the_endpoint_is_named_as_stale() {
    let stub = Stub::start("stale").await;
    stub.state.lock().unwrap().usage_missing = true;

    // 带作用域也一样：无作用域的探测同样 404 → 根因是路由不存在。
    let (code, _stdout, stderr) = run_wing(&["usage", "--session", SESSION_ID], &stub.home).await;
    assert_eq!(code, 1);
    assert!(
        stderr.contains("does not serve /api/usage"),
        "带 --session 也不能误报成会话不存在：{stderr}"
    );

    let (code, _stdout, stderr) = run_wing(&["usage"], &stub.home).await;
    assert_eq!(code, 1);
    assert!(
        stderr.contains("does not serve /api/usage"),
        "老网关文案：{stderr}"
    );
    assert!(stderr.contains("wing stop && wing start"), "{stderr}");
    // 仪表盘三次并发查询都会撞上这个 404（第一条错误就是结论）。
    assert!(
        stub.requests().iter().all(|request| request.status == 404),
        "{:?}",
        stub.requests()
    );
}

#[tokio::test]
async fn info_gains_a_usage_block_and_keeps_its_json_shape() {
    let stub = Stub::start("info").await;
    let (code, stdout, stderr) = run_wing(&["info", SESSION_ID], &stub.home).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("context:"), "{stdout}");
    assert!(stdout.contains("usage:"), "{stdout}");
    assert!(stdout.contains("messages:           4"), "{stdout}");
    assert!(
        stdout.contains("calls:              3 llm · 6 tools"),
        "{stdout}"
    );
    assert!(stdout.contains("(100% cached)"), "{stdout}");
    // 端点只返回了自身一行 → 没有子孙，不打 tree 行。
    assert!(!stdout.contains("tree:"), "{stdout}");

    // 那次用量查询：`since=all` + 会话作用域 + 子孙 + 上限。
    let usage = stub
        .requests()
        .into_iter()
        .find(|request| request.path == "/api/usage")
        .expect("info asks the usage endpoint");
    assert_eq!(
        usage.query,
        format!("since=all&group_by=session&session_id={SESSION_ID}&tree=true&limit=200")
    );

    // `--json` 形状不变：还是 `SessionInfoResponse`，没有本地用量块。
    let (code, stdout, _stderr) = run_wing(&["info", SESSION_ID, "--json"], &stub.home).await;
    assert_eq!(code, 0);
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["session_name"], "usage probe");
    assert_eq!(value["usage"]["turns"], 3);
    assert!(value.get("usage_block").is_none(), "{stdout}");
}
