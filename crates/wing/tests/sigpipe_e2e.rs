//! End-to-end: 下游关掉 stdout 时进程安静地死于 SIGPIPE（shell 看到 141），
//! 不 panic、stderr 不留回溯。
//!
//! 这是 unix 管道那一半契约：`wing ps | head`、`| grep -q`、编排器把管道关掉，
//! 都靠它收场（`seq` / `yes` / `grep` 同款）。Rust 运行期默认把 SIGPIPE 设成
//! `SIG_IGN`，于是每一次写都变成「`failed printing to stdout: Broken pipe` 的
//! panic + 101」——所以 `main` 在解析参数之前把处置恢复成默认值，一处覆盖所有
//! 写点（`cmd` 下的 143 处 `print!` / `println!`、stdio 的帧 sink、clap 的帮助
//! 输出）。
//!
//! 这条测试盯的是**进程级**的事实，因此两条用例各代表一种写失败的时刻：
//!
//! 1. **第一次写就没有读端**（`config path`，离线命令）：证明最普通的一条
//!    `print!` 不再 panic；
//! 2. **写了几帧之后读端才走**（`ps -w`，每 2 秒重绘）：证明长跑命令不会对着
//!    死管道继续刷屏，也不会把「下游走了」当成自己的错误。
//!
//! 断言 SIGPIPE 而不是「非零退出码」：141 才是 shell / `set -o pipefail` /
//! 编排层认的「消费者走了」这个词，101（panic）是把截断说成 wing 的 bug。

#![cfg(unix)]

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

mod common;

use common::e2e::{read_head, scratch_home, start_fake_gateway};

/// `ps -w` 的假网关回的那一行会话 id：读到它就说明第一帧已经画完。
const SESSION_ID: &str = "20260101-000000-sigpipe";

/// 子进程必须在这么久之内死掉：`ps -w` 的刷新间隔是 2s，留足两轮。
const DEATH_TIMEOUT: Duration = Duration::from_secs(20);

/// 一个**读端已经关掉**的管道，作为子进程的 stdout。
///
/// 读端在 `spawn` 之前就不存在，所以子进程的第一次写必定 EPIPE——这条断言因此
/// 没有「谁先跑」的竞态。
fn orphan_stdout() -> Stdio {
    let mut fds = [0i32; 2];
    // SAFETY: 两个合法的 fd 槽位；失败即测试环境坏了。
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
    // SAFETY: 读端不需要（也不该）留给任何人。
    unsafe { libc::close(fds[0]) };
    // SAFETY: `fds[1]` 是本进程独占的写端，所有权交给 `Stdio`。
    Stdio::from(unsafe { OwnedFd::from_raw_fd(fds[1]) })
}

fn wing_command(home: &std::path::Path, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(args)
        .env("WING_HOME", home)
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn a_closed_stdout_kills_with_sigpipe_and_no_panic() {
    let home = scratch_home("wing-sigpipe", "closed-stdout");
    let child = wing_command(&home, &["config", "path"])
        .stdout(orphan_stdout())
        .spawn()
        .unwrap();

    let output = tokio::time::timeout(DEATH_TIMEOUT, child.wait_with_output())
        .await
        .expect("wing kept running with an unwritable stdout")
        .unwrap();

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
    let mut child = wing_command(&home, &["ps", "-w"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

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
    drop(stdout);
    assert!(
        drew_a_frame,
        "the watch loop must draw before we close the pipe"
    );

    let output = tokio::time::timeout(DEATH_TIMEOUT, child.wait_with_output())
        .await
        .expect("`ps -w` kept redrawing into a pipe nobody reads")
        .unwrap();

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
