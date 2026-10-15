//! 会话血缘的 CLI 端到端：真 `wing` 二进制 + 进程内假网关（不联网、不碰用户 `~/.wing`）。
//!
//! 判据是**网关实际收到的请求体**与**命令的读回输出**（协议面，不是内部状态）：
//!
//! - `wing -p`（stdio，编排器主力路径，即 agent 的 Bash 里跑的那条）遇到 `WING_SESSION_ID`
//!   时，创建请求带上它；
//! - `wing new` / `wing run` 请求带上它，且 `--json` 输出 = **读回**的 parent；
//! - 环境里没有它（用户自己的 shell）= 根会话（请求不带字段、输出 null）。
//!
//! 假网关复用 [`acp_harness::gateway`]（自包含），这里只补它依赖的两个最小 fixture。

mod acp_harness {
    use std::time::Duration;

    use serde_json::Value;
    use serde_json::json;

    // 假网关自包含（`acp_harness/gateway.rs`）；本 crate 只用其中一部分 API
    // （列表 / 创建 / 记录），其余归 acp_e2e 的场景。
    #[path = "gateway.rs"]
    #[allow(dead_code)]
    pub mod gateway;

    /// 条件轮询（有界超时）——原版在 `acp_harness/mod.rs`（捆着 ACP 客户端依赖）。
    pub async fn wait_until<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待超时：{what}（假网关没收到预期请求？）"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// 事件帧补元数据（原版 fixture 的最小复制；`gateway.rs` 的推帧路径要用）。
    pub fn wing_event(session_id: &str, mut frame: Value) -> Value {
        frame["session_id"] = json!(session_id);
        frame["created_at"] = json!("2026-01-01T00:00:00+00:00");
        frame["request_id"] = json!("req-e2e");
        frame
    }
}

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use acp_harness::gateway::FakeGateway;
use acp_harness::gateway::RecordedRequest;
use acp_harness::wait_until;

static HOME_SEQ: AtomicUsize = AtomicUsize::new(0);

/// 全新 `$WING_HOME`（Drop 清理），`core/config.yaml` 指向假网关。
struct TempHome {
    path: PathBuf,
}

impl TempHome {
    fn new(port: u16) -> Self {
        let seq = HOME_SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("wing-lineage-e2e-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(path.join("core")).expect("create temp WING_HOME");
        std::fs::write(
            path.join("core").join("config.yaml"),
            format!("gateway:\n  host: 127.0.0.1\n  port: {port}\n"),
        )
        .expect("write gateway config into the temp WING_HOME");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 起一个 `wing`（真二进制），env 里只给测试要的东西。
fn spawn_wing(home: &Path, args: &[&str], parent_env: Option<&str>) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(args)
        .env("WING_HOME", home)
        .env_remove("WING_SESSION_ID")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(parent) = parent_env {
        command.env("WING_SESSION_ID", parent);
    }
    command.spawn().expect("spawn wing")
}

/// 等一条创建请求并取出请求体。
async fn wait_create(gateway: &FakeGateway) -> serde_json::Value {
    let create: RecordedRequest = gateway
        .wait_request("session/create", |request| {
            request.path == "/api/session/create"
        })
        .await;
    create.body.expect("create 请求体")
}

/// `wing -p`（stdio）：agent 的 Bash 里跑的那条路径。
#[tokio::test(flavor = "multi_thread")]
async fn stdio_records_the_env_parent_on_create() {
    let gateway = FakeGateway::start();
    let home = TempHome::new(gateway.port());
    let mut child = spawn_wing(
        home.path(),
        &["-p", "hi", "--output-format", "json"],
        Some("20261015-211837-sched"),
    );

    let body = wait_create(&gateway).await;
    assert_eq!(body["parent_session_id"], "20261015-211837-sched");

    let _ = child.kill().await;
}

/// 环境里没有 `WING_SESSION_ID`（用户自己的 shell）→ 根会话：请求不带字段。
#[tokio::test(flavor = "multi_thread")]
async fn stdio_without_the_env_sends_no_parent() {
    let gateway = FakeGateway::start();
    let home = TempHome::new(gateway.port());
    let mut child = spawn_wing(home.path(), &["-p", "hi", "--output-format", "json"], None);

    let body = wait_create(&gateway).await;
    assert!(
        body.get("parent_session_id").is_none(),
        "根会话不得凭空长出父：{body}"
    );

    let _ = child.kill().await;
}

/// `wing new --json`：请求带上，输出 = 读回的 parent；无 env 时输出 null。
#[tokio::test(flavor = "multi_thread")]
async fn wing_new_reports_the_recorded_parent() {
    let gateway = FakeGateway::start();
    let home = TempHome::new(gateway.port());

    let child = spawn_wing(home.path(), &["new", "--json"], Some("sched"));
    let output = child.wait_with_output().await.expect("wing new exits");
    assert!(output.status.success(), "{output:?}");
    let body = wait_create(&gateway).await;
    assert_eq!(body["parent_session_id"], "sched");
    let printed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("wing new --json prints JSON");
    assert_eq!(printed["parent_session_id"], "sched");

    // 用户自己的 shell：根会话（读回 null，不带字段）。
    let child = spawn_wing(home.path(), &["new", "--json"], None);
    let output = child.wait_with_output().await.expect("wing new exits");
    let printed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(printed["parent_session_id"].is_null(), "{printed}");
}

/// `wing run --json`：请求带上，输出 = 读回的 parent。
#[tokio::test(flavor = "multi_thread")]
async fn wing_run_reports_the_recorded_parent() {
    let gateway = FakeGateway::start();
    let home = TempHome::new(gateway.port());

    let child = spawn_wing(
        home.path(),
        &["run", "-p", "task for the crew", "--json"],
        Some("sched"),
    );
    let output = child.wait_with_output().await.expect("wing run exits");
    assert!(output.status.success(), "{output:?}");
    let body = wait_create(&gateway).await;
    assert_eq!(body["parent_session_id"], "sched");
    let printed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("wing run --json prints JSON");
    assert_eq!(printed["parent_session_id"], "sched");
}

/// `wing info`：有父显示 `parent:` 行；根会话不显示。
///
/// `wait_create` 之外还要等 info 帧准备好——创建与 `wing info` 是两条进程，第一条
/// 先落地（`wing new` 退出即已创建），所以这里串行跑。
#[tokio::test(flavor = "multi_thread")]
async fn wing_info_shows_the_parent_line() {
    let gateway = FakeGateway::start();
    let home = TempHome::new(gateway.port());

    let child = spawn_wing(home.path(), &["new", "--json"], Some("sched"));
    let output = child.wait_with_output().await.expect("wing new exits");
    assert!(output.status.success(), "{output:?}");
    let created: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    let session_id = created["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    let child = spawn_wing(home.path(), &["info", &session_id], None);
    let output = child.wait_with_output().await.expect("wing info exits");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("parent:              sched"), "{stdout}");

    // 根会话：没有 parent 行（缺席即根，不写空行）。
    let child = spawn_wing(home.path(), &["new", "--json"], None);
    let output = child.wait_with_output().await.expect("wing new exits");
    let root: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    let root_id = root["session_id"].as_str().expect("session id").to_string();

    let child = spawn_wing(home.path(), &["info", &root_id], None);
    let output = child.wait_with_output().await.expect("wing info exits");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(!stdout.contains("parent:"), "{stdout}");
}

/// `wing ps --parent`：种子行来自网关，CLI 只列直接子（含 inactive；不含孙）。
#[tokio::test(flavor = "multi_thread")]
async fn wing_ps_filters_by_parent() {
    let gateway = FakeGateway::start();
    let home = TempHome::new(gateway.port());
    gateway.set_sessions(vec![
        row("sched", None, "working"),
        row("kid", Some("sched"), "inactive"),
        row("grandkid", Some("kid"), "idle"),
    ]);

    let child = spawn_wing(home.path(), &["ps", "--parent", "sched", "--json"], None);
    let output = child.wait_with_output().await.expect("wing ps exits");
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("wing ps --json prints JSON");
    let ids: Vec<&str> = rows.iter().filter_map(|row| row["id"].as_str()).collect();
    assert_eq!(ids, ["kid"], "只列直接子（inactive 也要在）：{rows:?}");

    let _ = wait_until("session/list", || {
        gateway
            .requests_for("/api/session/list")
            .into_iter()
            .last()
            .map(|_| ())
    })
    .await;
}

/// `wing ps --tree`：子会话挂在创建者下面（前缀进 SESSION ID 列）。
#[tokio::test(flavor = "multi_thread")]
async fn wing_ps_tree_draws_the_forest() {
    let gateway = FakeGateway::start();
    let home = TempHome::new(gateway.port());
    gateway.set_sessions(vec![
        row("sched", None, "working"),
        row("kid", Some("sched"), "idle"),
    ]);

    let child = spawn_wing(home.path(), &["ps", "--tree", "--all"], None);
    let output = child.wait_with_output().await.expect("wing ps exits");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("└─ kid"), "{stdout}");
    // 根行没有前缀。
    assert!(stdout.contains("┃ sched"), "{stdout}");
}

/// 一行 `/api/session/list` 会话（只给血缘相关的字段）。
fn row(id: &str, parent: Option<&str>, status: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": id,
        "created_at": null,
        "template_name": "default",
        "workspace": null,
        "parent_session_id": parent,
        "last_interaction": "2026-10-15T21:00:00.000000",
        "status": status,
        "tags": [],
        "tag_meta": {},
        "model_id": null,
        "model_name": null,
        "provider_name": null,
        "model_display_name": null,
    })
}
