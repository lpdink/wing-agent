//! End-to-end: 下游关掉 stdout 时进程安静地死于 SIGPIPE（shell 看到 141），
//! 不 panic、stderr 不留回溯。
//!
//! 这是 unix 管道那一半契约：`wing ps | head`、`| grep -q`、编排器把管道关掉，
//! 都靠它收场（`seq` / `yes` / `grep` 同款）。Rust 运行期默认把 SIGPIPE 设成
//! `SIG_IGN`，于是 `println!` / `print!` 会把 EPIPE 翻成 `failed printing to
//! stdout: Broken pipe` 的 panic（101 + 回溯）；`main` 在解析参数之前恢复默认处置，
//! 一次覆盖所有写点。
//!
//! 这条测试盯的是**进程级**的事实，因此三条用例各代表一种时刻：
//!
//! 1. **第一次写就没有读端**（`config path`，离线命令）：最普通的一条 `print!`；
//! 2. **clap 的帮助输出**（`--help`）：写发生在 `dispatch` 之前，处置必须装得更早
//!    （clap 自己吞掉写错误，修复前是静默截断 + 退出码 0，不是 panic）；
//! 3. **写了几帧之后读端才走**（`ps -w`，每 2 秒重绘）：长跑命令不会对着死管道
//!    继续刷屏。
//!
//! 断言 SIGPIPE 而不是「非零退出码」：141 才是 shell / `set -o pipefail` /
//! 编排层认的「消费者走了」这个词。stdio 帧口（`StdoutSink`）的同一形态由
//! `stdio_one_shot_e2e::stdio_frames_die_with_sigpipe_when_the_reader_leaves` 钉住。

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

mod common;

use common::e2e::{
    RUN_TIMEOUT, closed_stdout, read_head, scratch_home, start_fake_gateway, wing_command,
};

/// `ps -w` 的假网关回的那一行会话 id：读到它就说明第一帧已经画完。
const SESSION_ID: &str = "20260101-000000-sigpipe";

/// 断言子进程死于 SIGPIPE 且 stderr 为空（下游走了不是 wing 的失败）。
fn assert_died_of_sigpipe(output: &std::process::Output) {
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGPIPE),
        "expected a SIGPIPE death (shell sees 141), got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.is_empty(),
        "a downstream that left is not a wing failure — stderr must stay empty: {stderr:?}"
    );
}

#[tokio::test]
async fn a_closed_stdout_kills_with_sigpipe_and_no_panic() {
    let home = scratch_home("wing-sigpipe", "closed-stdout");
    let child = wing_command(&["config", "path"], &home)
        .stdout(closed_stdout())
        .spawn()
        .unwrap();

    let output = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
        .await
        .expect("wing kept running with an unwritable stdout")
        .unwrap();

    assert_died_of_sigpipe(&output);
}

/// 处置必须早于 clap：`--help` 的写发生在 `dispatch` 之前。
#[tokio::test]
async fn the_disposition_covers_clap_help_output() {
    let home = scratch_home("wing-sigpipe", "help");
    let child = wing_command(&["--help"], &home)
        .stdout(closed_stdout())
        .spawn()
        .unwrap();

    let output = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
        .await
        .expect("clap kept running with an unwritable stdout")
        .unwrap();

    assert_died_of_sigpipe(&output);
}

/// 假网关：只会答 `ps` 需要的两条端点（`ensure_gateway` 的健康检查 + 会话列表）。
async fn handle_conn(mut stream: TcpStream, (): ()) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some(head) = read_head(&mut stream, &mut buf).await else {
            return;
        };
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let (status, body) = match target.split('?').next().unwrap_or("") {
            "/api/health" => (
                200,
                json!({"service": "wing-gateway", "status": "ok", "version": "test", "uptime": 1})
                    .to_string(),
            ),
            "/api/session/list" => (
                200,
                json!({"sessions": [{"id": SESSION_ID, "name": "sigpipe probe", "status": "idle"}]})
                    .to_string(),
            ),
            // 别的路径说明了测试前提坏了，而不是一条要回答的请求：断开连接即可。
            _ => return,
        };
        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            body.len()
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

#[tokio::test]
async fn a_watch_loop_dies_quietly_when_the_reader_leaves() {
    let (home, _port) = start_fake_gateway("wing-sigpipe", "ps-watch", (), handle_conn).await;
    let mut child = wing_command(&["ps", "-w"], &home).spawn().unwrap();

    // 读够一帧再扔掉读端：之后的第一次重绘写就是 EPIPE（`| head` 的真实形状）。
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut seen: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let drew_a_frame = tokio::time::timeout(Duration::from_secs(10), async {
        while !seen
            .windows(SESSION_ID.len())
            .any(|window| window == SESSION_ID.as_bytes())
        {
            match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => seen.extend_from_slice(&chunk[..n]),
            }
        }
        true
    })
    .await
    .expect("the first frame never arrived");
    assert!(
        drew_a_frame,
        "the watch loop must draw before we close the pipe"
    );
    drop(stdout);

    let output = tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output())
        .await
        .expect("`ps -w` kept redrawing into a pipe nobody reads")
        .unwrap();

    assert_died_of_sigpipe(&output);
}
