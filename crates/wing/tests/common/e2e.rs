//! CLI e2e 底座：假网关 + 隔离的 `WING_HOME` + 跑真二进制。
//!
//! 每个 `*_e2e.rs` 自己决定假网关怎么答（`handler` / 状态机），但「绑端口 → 写
//! `core/config.yaml` → 起 accept 循环 → 用 `WING_HOME` 跑 `wing <subcmd>`」这套脚手架
//! 只有一份：WING_HOME 隔离、超时、杀子进程这三件事各写一遍就只会修到一半。

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

/// 一次 CLI 调用的超时（超时即 panic：挂死与失败必须可区分）。
pub const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// 一枚测试独占的 `WING_HOME`（`prefix` 让失败时的临时目录名可辨认）。
pub fn scratch_home(prefix: &str, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("core")).unwrap();
    dir
}

/// accept 循环：每条连接一个 task，交给 `handler`。
async fn serve<S, F, Fut>(listener: TcpListener, state: S, handler: F)
where
    S: Clone + Send + 'static,
    F: Fn(TcpStream, S) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(handler(stream, state.clone()));
    }
}

/// 绑一个随机端口起假网关，并写出一份指向它的 `WING_HOME`。
///
/// 返回 `(home, port)`。`WING_HOME/core/config.yaml` 的 `gateway.host/port` 是 CLI 发现
/// 网关的**唯一**约定，所以它和端口绑定必须成对出现。
pub async fn start_fake_gateway<S, F, Fut>(
    prefix: &str,
    tag: &str,
    state: S,
    handler: F,
) -> (PathBuf, u16)
where
    S: Clone + Send + 'static,
    F: Fn(TcpStream, S) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener, state, handler));

    let home = scratch_home(prefix, tag);
    std::fs::write(
        home.join("core").join("config.yaml"),
        format!("gateway:\n  host: 127.0.0.1\n  port: {port}\n"),
    )
    .unwrap();
    (home, port)
}

/// 跑真二进制的 `Command`，已套好测试环境（WING_HOME 隔离 / 日志不落 / 无 stdin /
/// 子进程随测试收掉）。
///
/// [`run_wing`] 是它的「跑到结束、收三元组」版本；需要自己拿 stdout（读一帧就走、
/// 关掉读端看写失败）或自己判退出状态的用例从这里起步，**不要再抄一遍环境设置**。
pub fn wing_command(args: &[&str], home: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(args)
        .env("WING_HOME", home)
        .env_remove("RUST_LOG")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command
}

/// 跑一次真二进制，回收 `(退出码, stdout, stderr)`。超时 / 挂死算失败。
pub async fn run_wing(args: &[&str], home: &Path) -> (i32, String, String) {
    let child = wing_command(args, home).spawn().unwrap();
    let output = match tokio::time::timeout(RUN_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => result.unwrap(),
        Err(_) => panic!("`wing {args:?}` did not exit in {}s", RUN_TIMEOUT.as_secs()),
    };
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// 一个**读端已经关掉**的管道，可作子进程的 stdout：写端在下发前就交出去，读端在
/// 这里关掉，所以子进程的第一次写必定 EPIPE（不依赖「谁先跑」的竞态）。
#[cfg(unix)]
pub fn closed_stdout() -> std::process::Stdio {
    use std::os::fd::{FromRawFd, OwnedFd};

    let mut fds = [0i32; 2];
    // SAFETY: 两个合法的 fd 槽位；失败即测试环境坏了。
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
    // SAFETY: 读端不需要（也不该）留给任何人。
    unsafe { libc::close(fds[0]) };
    // SAFETY: `fds[1]` 是本进程独占的写端，所有权交给 `Stdio`。
    std::process::Stdio::from(unsafe { OwnedFd::from_raw_fd(fds[1]) })
}

/// 读一条完整 HTTP/1.1 请求：`(head, body)`；连接关闭（或读失败）返回 `None`。
///
/// `buf` 跨请求复用：一次读到的字节可能带着下一条请求的开头。
pub async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
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
                let body = buf[body_start..body_start + content_length].to_vec();
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

/// 只读 head 的变体（没有 body 的端点用）：请求头之后的下行留给下一条请求。
pub async fn read_head(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            buf.drain(..pos + 4);
            return Some(head);
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
