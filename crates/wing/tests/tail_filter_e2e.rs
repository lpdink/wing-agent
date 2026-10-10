//! End-to-end: `wing tail` / `wing head --filter` against a fake gateway.
//!
//! The unit tests in `cmd::messages` pin the selection matrix on the typed
//! view; these pin the two things only a real process can show:
//!
//! 1. **The CLI vocabulary is closed.** `--filter user,content` used to reach
//!    no match arm, so the selection fell through to "everything" and the
//!    output was the *unfiltered* view — a silent degrade, reported by a user
//!    on a real session and "fixed" three times at different call sites. Names
//!    now go through one `ValueEnum`, so an unknown value is a clap error
//!    (exit 2, `[possible values: …]` on stderr) **before** any request.
//! 2. **What comes out of the process.** Text and `--json` from the same
//!    filter must carry the same elements and nothing else, and `head` / `tail`
//!    must slice the same selection from the two ends.
//!
//! The fake gateway answers `/api/health` (gateway discovery) and
//! `/api/session/get` (the history) only — no WS, no runtime.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SESSION_ID: &str = "20261010-074437-440e10fb";

/// The history served by the fake gateway: one row per element, each carrying
/// a sentinel that only that element renders.
///
/// The shape mirrors the Message projection (`serialize_message`), including
/// the `reasoning_content` the reported session had — the leak the user saw
/// was reasoning + tool calls + tool results showing up under `user,content`.
fn history() -> serde_json::Value {
    serde_json::json!([
        {"role": "user", "uuid": "u-user", "content": "USER-TEXT"},
        {
            "role": "assistant",
            "uuid": "u-asst",
            "reasoning_content": "REASONING",
            "content": "ANSWER",
            "tool_calls": [{"id": "call-1", "name": "bash", "arguments": {"cmd": "ls"}}]
        },
        {
            "role": "tool",
            "uuid": "u-tool",
            "tool_call_id": "call-1",
            "content": "RESULT-BODY"
        },
        {"role": "user", "uuid": "u-user-2", "content": "USER-TEXT-2"},
    ])
}

// ============================================================
// Fake gateway: minimal HTTP/1.1
// ============================================================

async fn serve(listener: TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(handle_conn(stream));
    }
}

async fn handle_conn(mut stream: TcpStream) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let Some((head, _body)) = read_request(&mut stream, &mut buf).await else {
            return;
        };
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        let path = target.split('?').next().unwrap_or("").to_string();

        let body = match path.as_str() {
            "/api/health" => {
                r#"{"service":"wing-gateway","status":"ok","version":"test","uptime":1}"#
                    .to_string()
            }
            "/api/session/get" => serde_json::json!({
                "session_id": SESSION_ID,
                "name": null,
                "template_name": null,
                "workspace": null,
                "status": "inactive",
                "messages": history(),
                "agent": null,
            })
            .to_string(),
            _ => "{}".to_string(),
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
            body.len(),
            body
        );
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Read one HTTP request head (+ the declared body); the rest stays buffered.
async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
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

// ============================================================
// Fixtures
// ============================================================

fn scratch_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wing-tail-filter-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("core")).unwrap();
    dir
}

/// Start the fake gateway and point a fresh `$WING_HOME` at it (the CLI reads
/// its endpoint from `$WING_HOME/core/config.yaml`).
async fn start_gateway(tag: &str) -> PathBuf {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(serve(listener));

    let home = scratch_home(tag);
    std::fs::write(
        home.join("core").join("config.yaml"),
        format!("gateway:\n  host: 127.0.0.1\n  port: {port}\n"),
    )
    .unwrap();
    home
}

/// Run the real binary and collect (exit code, stdout, stderr).
async fn run_wing(args: &[&str], home: &PathBuf) -> (i32, String, String) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_wing"));
    command
        .args(args)
        .env("WING_HOME", home)
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

/// The reported case: user text + assistant text, nothing else.
const REPORTED_FILTER: &str = "user,content";

// ============================================================
// Tests
// ============================================================

/// The reported path, text mode: `--filter user,content` prints the two text
/// elements and not one byte of reasoning / tool call / tool result.
#[tokio::test]
async fn tail_user_content_prints_only_the_text_elements() {
    let home = start_gateway("tail-text").await;
    let (code, stdout, stderr) =
        run_wing(&["tail", SESSION_ID, "--filter", REPORTED_FILTER], &home).await;
    assert_eq!(code, 0, "stderr: {stderr}");

    for expected in ["USER-TEXT", "ANSWER"] {
        assert!(stdout.contains(expected), "{expected} missing:\n{stdout}");
    }
    for leaked in ["REASONING", "→ [call-1]", "← call-1", "RESULT-BODY"] {
        assert!(!stdout.contains(leaked), "{leaked} leaked:\n{stdout}");
    }
    // Both user rows are selected; the assistant row yields its text only.
    assert!(stdout.contains("USER-TEXT-2"), "{stdout}");
    assert_eq!(
        stdout.matches("[user]").count(),
        2,
        "the two user rows: {stdout}"
    );

    // Baseline: the default `all` shows everything — the regression was that
    // `user,content` produced exactly this output.
    let (code, all, stderr) = run_wing(&["tail", SESSION_ID, "--filter", "all"], &home).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    for sentinel in [
        "USER-TEXT",
        "REASONING",
        "ANSWER",
        "→ [call-1]",
        "RESULT-BODY",
    ] {
        assert!(
            all.contains(sentinel),
            "{sentinel} missing from `all`:\n{all}"
        );
    }

    let _ = std::fs::remove_dir_all(&home);
}

/// The reported path, `--json`: one stripped record per selected row — the
/// same elements text mode printed, with no raw payload smuggled in.
#[tokio::test]
async fn tail_user_content_json_carries_the_same_elements() {
    let home = start_gateway("tail-json").await;
    let (code, stdout, stderr) = run_wing(
        &["tail", SESSION_ID, "--filter", REPORTED_FILTER, "--json"],
        &home,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");

    let records: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        records,
        serde_json::json!([
            {"uuid": "u-user", "content": "USER-TEXT"},
            {"uuid": "u-asst", "content": "ANSWER"},
            {"uuid": "u-user-2", "content": "USER-TEXT-2"},
        ]),
        "stripped text records only"
    );
    // No raw-payload shape: the `all` view keeps `role`, element filters never.
    assert!(!stdout.contains("\"role\""), "{stdout}");
    assert!(!stdout.contains("REASONING"), "{stdout}");
    assert!(!stdout.contains("tool_calls"), "{stdout}");

    // `--json --filter all` stays byte-compatible with the raw messages (the
    // documented exception: `all` is "no filter", so nothing is stripped).
    let (code, stdout, stderr) =
        run_wing(&["tail", SESSION_ID, "--filter", "all", "--json"], &home).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    let all: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(all, history(), "raw payloads verbatim");

    let _ = std::fs::remove_dir_all(&home);
}

/// `head` / `tail` slice the *same* selection from the two ends — the filter
/// decides what is in the run, the direction only decides which end.
#[tokio::test]
async fn head_and_tail_slice_the_same_selection() {
    let home = start_gateway("head-tail").await;

    let (code, head, stderr) = run_wing(
        &["head", SESSION_ID, "-n", "1", "-t", REPORTED_FILTER],
        &home,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        head.contains("USER-TEXT") && !head.contains("USER-TEXT-2"),
        "{head}"
    );
    assert!(!head.contains("ANSWER"), "{head}");

    let (code, tail, stderr) = run_wing(
        &["tail", SESSION_ID, "-n", "1", "-t", REPORTED_FILTER],
        &home,
    )
    .await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        tail.contains("USER-TEXT-2") && !tail.contains("ANSWER"),
        "{tail}"
    );

    // A single element still renders exactly that element (`--filter content`
    // must not drag reasoning along).
    let (code, content, stderr) = run_wing(&["tail", SESSION_ID, "-t", "content"], &home).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(content.contains("ANSWER"), "{content}");
    assert!(!content.contains("REASONING"), "{content}");
    assert!(!content.contains("→ [call-1]"), "{content}");

    let _ = std::fs::remove_dir_all(&home);
}

/// The closed vocabulary: a value outside it is rejected by the process
/// (exit 2 + the valid names on stderr) instead of quietly printing the
/// unfiltered view.
#[tokio::test]
async fn unknown_filter_value_is_rejected_before_any_request() {
    let home = start_gateway("filter-reject").await;

    for value in ["bogus", "reasoning_content", "*"] {
        let (code, stdout, stderr) =
            run_wing(&["tail", SESSION_ID, "--filter", value], &home).await;
        assert_eq!(code, 2, "{value}: clap usage error\nstderr: {stderr}");
        assert!(stdout.is_empty(), "{value}: nothing on stdout: {stdout}");
        assert!(
            stderr.contains("invalid value") && stderr.contains(&format!("'{value}'")),
            "{value}: stderr names the value: {stderr}"
        );
        assert!(
            stderr.contains(
                "possible values: all, user, assistant, tool_call, tool_result, reasoning, content"
            ),
            "{value}: stderr lists the vocabulary: {stderr}"
        );
    }

    // An empty segment (`--filter user,`) is a value error too, not "user".
    let (code, _, stderr) = run_wing(&["tail", SESSION_ID, "--filter", "user,"], &home).await;
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("--filter"), "{stderr}");

    // `head` shares the vocabulary (one definition, two commands).
    let (code, _, stderr) = run_wing(&["head", SESSION_ID, "-t", "bogus"], &home).await;
    assert_eq!(code, 2, "stderr: {stderr}");
    assert!(stderr.contains("possible values"), "{stderr}");

    let _ = std::fs::remove_dir_all(&home);
}
