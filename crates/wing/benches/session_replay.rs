//! Session-replay benchmarks — opening a session, and the first frame after it.
//!
//! What a user waits for when a session is opened, resumed or switched: the
//! frontend replays the `SyncSession` snapshot into ChatView cells
//! (`crates/wing/src/app/replay.rs`) and draws the first frame. Both halves are
//! covered here, on the **production call sequence**:
//!
//! 1. `chat.clear()` (`projection.rs:542`, `ChatView::clear` at `model.rs:343`);
//! 2. committed messages → `replay_messages` (`projection.rs:614`);
//! 3. the uncommitted assistant projection → the same `replay_messages`
//!    (`projection.rs:618`);
//! 4. unfinished tool calls → the *live* `ToolCallStream` branch
//!    (`projection.rs:630-661` → `188-218`): append to an existing cell, else
//!    `ToolCallBlock::new_streaming` + push (never final on this path);
//! 5. fact events → `replay_events` (`projection.rs:665`);
//! 6. the copy-candidate refresh that closes the replay →
//!    `chat.collect_assistant_messages()` (`projection.rs:692` →
//!    `commands.rs:306-308`; the App stores the result in its popup cache);
//! 7. the first frame: `CellContext` + `ChatViewWidget` into
//!    `scrollbar::content_area(band)` (`app/mod.rs:579-589`).
//!
//! ```text
//! replay/<n>   clear + the whole assembly, no frame      (n = 200/1000/3000 messages)
//! frame/<n>    the same replay + the first frame into an 80×50 Buffer
//!              ("open a session, content on screen")
//! scroll/<n>   a session that is already open: one page further down + a frame
//! ```
//!
//! `frame/<n>` deliberately repeats the replay inside the iteration: the first
//! frame is only a *first* frame while the caches are cold, and a replay is
//! what leaves them that way (`update_heights` — `viewport.rs:210/252`). The
//! delta against `replay/<n>` is the first-paint cost; the stderr report
//! prints the two halves separately for the first pass.
//!
//! The payload is synthetic and deterministic — a pure function of `(SEED, n)`
//! (see [`payload`]): no fixture files, no gateway, no `~/.wing`. The first
//! **executed** pass of each case prints a phase breakdown to stderr: it runs
//! during warm-up, so its numbers are a cold first execution, not the medians
//! criterion reports (3000-message cases land within ~1% of the median; the
//! small ones come out several times slower). The payload fingerprint line
//! (bytes + FNV-1a) is what two worktrees compare to prove they measured the
//! same content.
//!
//! Tuning knobs are **not** uniform: `sample_size(10)` and a 1 s warm-up are
//! pinned by the groups below, so `--sample-size` / `--warm-up-time` on the
//! command line are ignored (and criterion rejects any sample size < 10
//! outright). What really drives a run is the `<filter>` and
//! `--measurement-time`.
//!
//! Run:    cargo bench --bench session_replay
//! CI:     cargo bench --bench session_replay -- --measurement-time 2            (all 9)
//! Filter: cargo bench --bench session_replay -- 'replay/1000'
//!         cargo bench --bench session_replay -- '^(replay|frame)/'

use std::cell::Cell;
use std::time::Duration;
use std::time::Instant;

use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use serde_json::Value;
use serde_json::json;
use wing::app::replay::replay_events;
use wing::app::replay::replay_messages;
use wing::config::LayoutConfig;
use wing::config::ThemePalette;
use wing::config::rendering::ThinkingMode;
use wing::render::markdown::ImageOpts;
use wing::render::renderable::CellContext;
use wing::ui::cells::tool_call::ToolCallBlock;
use wing::ui::chat_view::ChatCell;
use wing::ui::chat_view::ChatView;
use wing::ui::chat_view::ChatViewWidget;
use wing::ui::scrollbar;

/// Fixed seed — the payload is a pure function of `(SEED, n)`.
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Message counts of the measured payloads (short / common-large / extreme).
const SIZES: [usize; 3] = [200, 1000, 3000];

/// Chat band width of the frame benchmarks.
const WIDTH: u16 = 80;
/// Chat band height of the frame benchmarks.
const HEIGHT: u16 = 50;

// ── deterministic payload generator ─────────────────────────────

/// xorshift64* — ten lines, no dependency, bit-identical everywhere.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Zero is a fixed point of xorshift: never let it in.
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Index in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() >> 32) as usize % n
    }

    fn pick<'a, T>(&mut self, pool: &'a [T]) -> &'a T {
        &pool[self.below(pool.len())]
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    fn one_in(&mut self, d: usize) -> bool {
        self.below(d) == 0
    }
}

/// Development-flavoured sentences: every prose block is built from these, so
/// the text has realistic lengths (30-60 chars) without a fixture file.
const SENTENCES: &[&str] = &[
    "这个改动会影响到会话重放的路径，需要同时检查工具调用的配对关系。",
    "我先把 `SessionManager` 的注册流程读一遍，确认没有遗漏的分支。",
    "测试跑过了，只有流式渲染那个用例需要更新快照。",
    "网关侧的帧切分在上限之内没有问题，客户端读任务会合并还原。",
    "如果上下文里还有半截的 tool 块，中断提交时会把它整块剔除。",
    "这段代码的复杂度主要在滚动时的行高缓存，宽度变化会触发全量重算。",
    "同步之后我把分支的父指针重新接上，避免链上出现孤立的节点。",
    "`uv sync` 之后本地可以用 wing-gateway 起一个隔离的 WING_HOME。",
    "性能回归需要相对基线的对比，单次数字的绝对值没有太大意义。",
    "我把两个文件的 diff 贴出来，确认一下窗口的起始行号是否正确。",
    "渲染层不应该感知协议差异，格式化交给 markdown 层处理。",
    "这个报错来自空闲超时，SSE 连接在一百多秒没有新数据时会被断开。",
    "把工具集冻结成声明视图之后，缓存前缀就不会被后插入的提醒打断。",
    "先跑一遍 cargo clippy --all-targets，再提交，避免在 CI 上返工。",
    "会话的元数据和消息日志都归 SessionStore 管理，别在别处写文件。",
    "搜索了一圈没找到调用点，可能是通过闭包注入的远程工具。",
    "我倾向把这段逻辑放到 context 包下，它和窗口投影的职责更接近。",
    "重放顺序按链序来，事实事件锚定到对应的工具卡片之后。",
];

/// Identifiers that read like this repository's own code.
const IDENTS: &[&str] = &[
    "session", "cells", "blocks", "payload", "delta", "window", "cursor", "chain", "store",
    "buffer",
];

/// Repository-shaped paths (nothing here touches the filesystem).
const FILES: &[&str] = &[
    "crates/wing/src/app/replay.rs",
    "crates/wing/src/ui/chat_view/viewport.rs",
    "crates/wing/src/protocol/events.rs",
    "crates/wing/src/gateway/client.rs",
    "libs/core/wing/runtime.py",
    "libs/core/wing/session/session.py",
    "libs/core/wing/gateway/routes/ws.py",
    "docs/dev/architecture.md",
];

const COMMANDS: &[&str] = &[
    "cargo test -p wing --lib",
    "cargo clippy --all-targets --quiet -- -D warnings",
    "uv run pytest libs/core/tests -x -q",
    "rg -n \"SyncSession\" libs/core/wing crates/wing/src",
    "git diff --stat HEAD~1",
    "make check-python",
];

const PATTERNS: &[&str] = &[
    "SyncSession",
    "replay_messages",
    "fn update_heights",
    "tool_call_id",
    "EventTarget",
];

const GLOBS: &[&str] = &[
    "**/*.rs",
    "crates/wing/src/ui/**/*.rs",
    "libs/core/wing/**/*.py",
    "benches/*.rs",
];

/// Output lines of a test/build run.
const BASH_LINES: &[&str] = &[
    "   Compiling wing v0.1.0 (/Users/dev/ws/wing)",
    "    Finished `test` profile [unoptimized + debuginfo] target(s) in 12.42s",
    "     Running unittests src/lib.rs (target/debug/deps/wing-1a2b3c4d5e6f7a8b)",
    "test result: ok. 512 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out",
    "warning: unused variable: `payload`",
    "   Doc-tests wing",
    "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured",
];

/// One plausible source line for the given language.
fn code_line(rng: &mut Rng, lang: &str) -> String {
    let a = *rng.pick(IDENTS);
    let b = *rng.pick(IDENTS);
    match lang {
        "python" => match rng.below(6) {
            0 => format!("    {a} = {b}.resolve()"),
            1 => format!("    for item in {a}.items():"),
            2 => format!("    logger.debug(\"{a}=%s\", len({b}))"),
            3 => format!("    {a}.append({b})"),
            4 => format!("    assert {a} is not None"),
            _ => format!("    # {b} is rebuilt on the next frame"),
        },
        "typescript" => match rng.below(6) {
            0 => format!("    const {a} = {b}.length;"),
            1 => format!("    {a}.push({b});"),
            2 => format!("    console.debug(`{a}: ${{{b}}}`);"),
            3 => format!("    for (const item of {a}) {{"),
            4 => format!("    await {a}.flush();"),
            _ => format!("    // {b} arrives with the next snapshot"),
        },
        "bash" => match rng.below(6) {
            0 => format!("cargo test -p {a} --no-run"),
            1 => format!("rg -n \"{b}\" crates/wing/src | head -20"),
            2 => "git -C /Users/dev/ws/wing diff --stat".to_string(),
            3 => format!("jq '.{a} // empty' target/perf/raw/{b}.json"),
            4 => format!("uv run python -m pytest libs/core/tests -k {a}"),
            _ => format!("echo \"{b} done\""),
        },
        _ => match rng.below(6) {
            0 => format!("    let {a} = {b}.len();"),
            1 => format!("    let mut {a} = {b}.clone();"),
            2 => format!("    {a}.push({b});"),
            3 => format!("    tracing::debug!(count = {a}.len(), \"replayed\");"),
            4 => format!("    builder.with_{a}({b})"),
            _ => format!("    // {b} is rebuilt on the next frame"),
        },
    }
}

/// A fenced code block (the syntect path of the render).
fn code_block(rng: &mut Rng) -> String {
    let lang = *rng.pick(&["rust", "python", "typescript", "bash"]);
    let lines = rng.range(4, 14);
    let mut body = String::new();
    for _ in 0..lines {
        body.push_str(&code_line(rng, lang));
        body.push('\n');
    }
    format!("```{lang}\n{body}```")
}

/// `min..=max` sentences joined by spaces — one line of prose. A block takes a
/// consecutive run of the pool (random start, wrapping), so it never repeats a
/// sentence while the start keeps two blocks of the same payload apart.
fn sentences(rng: &mut Rng, min: usize, max: usize) -> String {
    let count = rng.range(min, max);
    let start = rng.below(SENTENCES.len());
    let mut out = String::new();
    for i in 0..count {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(SENTENCES[(start + i) % SENTENCES.len()]);
    }
    out
}

/// 1..=3 paragraphs, each of `min..=max` sentences — a message body.
fn prose(rng: &mut Rng, min: usize, max: usize) -> String {
    let count = rng.range(1, 3);
    let mut out = String::new();
    for p in 0..count {
        if p > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&sentences(rng, min, max));
    }
    out
}

/// An assistant answer: prose, sometimes a bullet list, sometimes a code block.
fn assistant_text(rng: &mut Rng) -> String {
    let mut out = prose(rng, 1, 3);
    if rng.one_in(3) {
        out.push_str("\n\n- ");
        out.push_str(rng.pick(SENTENCES));
        out.push_str("\n- ");
        out.push_str(rng.pick(SENTENCES));
    }
    if rng.one_in(2) {
        out.push_str("\n\n");
        out.push_str(&code_block(rng));
    }
    out
}

/// A `path` argument, workspace-qualified.
fn file_path(rng: &mut Rng) -> String {
    format!("/Users/dev/ws/wing/{}", rng.pick(FILES))
}

/// The `arguments` of one tool call — shaped like the built-in tools' schemas.
fn tool_args(rng: &mut Rng, name: &str) -> Value {
    match name {
        "Read" => json!({"path": file_path(rng)}),
        "Bash" => json!({"command": rng.pick(COMMANDS)}),
        "Grep" => json!({"pattern": rng.pick(PATTERNS), "path": "crates/wing/src"}),
        "Glob" => json!({"pattern": rng.pick(GLOBS)}),
        "Edit" => json!({
            "path": file_path(rng),
            "old_string": code_line(rng, "rust"),
            "new_string": code_line(rng, "rust"),
        }),
        "Write" => json!({"path": file_path(rng), "content": code_block(rng)}),
        "TodoWrite" => json!({
            "todos": [
                {"content": rng.pick(SENTENCES), "status": "completed"},
                {"content": rng.pick(SENTENCES), "status": "in_progress"},
                {"content": rng.pick(SENTENCES), "status": "pending"},
            ]
        }),
        _ => json!({}),
    }
}

/// The tool result that pairs with `tool_args` — per tool, with the texture the
/// real one has (numbered file content, build output, hit list, diff snippet).
fn tool_result(rng: &mut Rng, name: &str) -> String {
    match name {
        "Read" => {
            let start = rng.range(1, 400);
            let lines = rng.range(8, 40);
            let mut out = String::new();
            for i in 0..lines {
                out.push_str(&format!("{:>6}| {}\n", start + i, code_line(rng, "rust")));
            }
            out
        }
        "Bash" => {
            let lines = rng.range(3, 12);
            let mut out = String::new();
            for _ in 0..lines {
                out.push_str(rng.pick(BASH_LINES));
                out.push('\n');
            }
            out
        }
        "Grep" => {
            let hits = rng.range(4, 18);
            let mut out = String::new();
            for _ in 0..hits {
                out.push_str(&format!(
                    "{}:{}: {}",
                    rng.pick(FILES),
                    rng.range(1, 900),
                    code_line(rng, "rust").trim()
                ));
                out.push('\n');
            }
            out
        }
        "Glob" => {
            let hits = rng.range(3, 12);
            let mut out = String::new();
            for _ in 0..hits {
                out.push_str(&format!("/Users/dev/ws/wing/{}\n", rng.pick(FILES)));
            }
            out
        }
        "Edit" => {
            let start = rng.range(1, 300);
            let mut out = format!(
                "The file {} has been updated. Here's the result of running `cat -n` on a \
                 snippet of the edited file:\n",
                file_path(rng)
            );
            for i in 0..rng.range(4, 12) {
                out.push_str(&format!("{:>6}| {}\n", start + i, code_line(rng, "rust")));
            }
            out
        }
        "Write" => format!("File created successfully at: {}", file_path(rng)),
        "TodoWrite" => "Todo list updated.".to_string(),
        _ => code_block(rng),
    }
}

/// A windowed `diff_content` fact event, anchored on its producing call — the
/// shape `DiffContentEvent` sends (`old_text`/`new_text` are the change window
/// ± context lines, `*_start_line` the absolute 1-based window starts).
fn diff_event(rng: &mut Rng, tool_call_id: &str, path: &str) -> Value {
    let start = rng.range(1, 400);
    let lines = rng.range(4, 12);
    let mut old_text = String::new();
    let mut new_text = String::new();
    let changed = rng.below(lines);
    for i in 0..lines {
        let line = code_line(rng, "rust");
        if i == changed {
            old_text.push_str(&format!("{}\n", line.replace("let ", "let old_")));
            new_text.push_str(&format!("{}\n", line));
        } else {
            old_text.push_str(&line);
            old_text.push('\n');
            new_text.push_str(&line);
            new_text.push('\n');
        }
    }
    json!({
        "type": "diff_content",
        "path": path,
        "old_text": old_text,
        "new_text": new_text,
        "old_start_line": start,
        "new_start_line": start,
        "tool_call_id": tool_call_id,
        "created_at": "2026-01-01T00:00:00+00:00",
        "session_id": "bench-session",
        "request_id": "req-0001",
    })
}

/// A `SyncSession` snapshot — the four replay groups the backend sends
/// (`runtime.py:536 _push_sync`).
struct Payload {
    /// Committed Message projections, exactly `n` records.
    messages: Vec<Value>,
    /// The single uncommitted assistant projection of the in-flight turn.
    uncommitted: Value,
    /// Unfinished tool calls' raw args fragments (`{tool_call_id, tool_name, args_fragment}`).
    uncommitted_tools: Vec<Value>,
    /// Durable fact events, in chain order.
    events: Vec<Value>,
}

/// Bundle size of the next turn: 2 = a plain Q&A (`user` + `assistant`),
/// 3..5 = `user` + `assistant` + 1..3 paired tool results. The tail is chosen
/// so that **no** remainder is ever exactly 1 — any `n >= 2` is filled exactly.
fn bundle_size(rng: &mut Rng, remaining: usize) -> usize {
    let candidates: &[usize] = match remaining {
        2 => &[2],
        3 => &[3],
        4 => &[2, 4],
        5 => &[2, 3, 5],
        6 => &[2, 3, 4],
        _ => &[2, 3, 4, 5],
    };
    *rng.pick(candidates)
}

/// Every tool call of one turn: `(tool_call_id, tool_name)`.
fn tool_calls(rng: &mut Rng, turn: usize, count: usize) -> Vec<(String, String)> {
    let pool = ["Read", "Bash", "Grep", "Edit", "Write", "TodoWrite", "Glob"];
    (0..count)
        .map(|i| {
            (
                format!("call_{turn:05}_{i}"),
                (*rng.pick(&pool)).to_string(),
            )
        })
        .collect()
}

/// The synthetic snapshot for `n` messages. Deterministic: same `n` → same
/// payload, byte for byte. One seed drives one generator sequence, so a
/// smaller payload is a prefix of a larger one up to its final turns (only
/// `bundle_size`'s tail guard reads the remaining count).
fn payload(n: usize) -> Payload {
    let mut rng = Rng::new(SEED);
    let mut messages: Vec<Value> = Vec::with_capacity(n);
    let mut events: Vec<Value> = Vec::new();
    let mut remaining = n;
    let mut turn = 0usize;
    while remaining > 0 {
        let bundle = bundle_size(&mut rng, remaining);
        turn += 1;
        messages.push(json!({
            "role": "user",
            "content": prose(&mut rng, 1, 2),
            "uuid": format!("user-{turn:05}"),
        }));

        let calls = tool_calls(&mut rng, turn, bundle.saturating_sub(2));
        let mut assistant = json!({
            "role": "assistant",
            "content": assistant_text(&mut rng),
            "uuid": format!("assistant-{turn:05}"),
        });
        if !rng.one_in(5) {
            assistant["reasoning_content"] = Value::String(prose(&mut rng, 2, 4));
        }
        if !calls.is_empty() {
            let list: Vec<Value> = calls
                .iter()
                .map(|(id, name)| {
                    json!({"id": id, "name": name, "arguments": tool_args(&mut rng, name)})
                })
                .collect();
            assistant["tool_calls"] = Value::Array(list);
        }
        messages.push(assistant);

        for (id, name) in &calls {
            messages.push(json!({
                "role": "tool",
                "content": tool_result(&mut rng, name),
                "uuid": format!("tool-{id}"),
                "tool_call_id": id,
            }));
            // Edit / Write produce a diff view on resume — occasionally, the
            // way a session does (not every edit leaves a fact event behind).
            if matches!(name.as_str(), "Edit" | "Write") && rng.one_in(3) {
                let path = file_path(&mut rng);
                events.push(diff_event(&mut rng, id, &path));
            }
        }
        remaining -= bundle;
    }

    // The in-flight tail: a turn that was interrupted mid-execution, so the
    // replay runs its last two steps on every payload (an idle session would
    // leave `uncommitted` / `uncommitted_tools` as dead code here).
    let uncommitted = json!({
        "role": "assistant",
        "content": assistant_text(&mut rng),
        "uuid": "assistant-inflight",
        "reasoning_content": prose(&mut rng, 2, 4),
    });
    let uncommitted_tools = vec![
        json!({
            "tool_call_id": "call_inflight_bash",
            "tool_name": "Bash",
            // Raw, still-streaming args are a *fragment*: the value is cut
            // mid-string, exactly what the accumulator holds before the call
            // is finalized (the frontend's partial parser renders it as-is).
            "args_fragment": "{\"command\": \"cargo bench --bench session_replay -- --sample-size 10 \
                              --warm-up-time 1 --measurement-time",
        }),
        json!({
            "tool_call_id": "call_inflight_ask",
            "tool_name": "AskUserQuestion",
            "args_fragment": "{\"questions\": [{\"id\": \"q1\", \"question\": \"要现在跑完整套件吗？\", \
                              \"header\": \"跑测试\", \"options\": [{\"label\": \"只跑这一档\"}, \
                              {\"label\": \"全部\"}",
        }),
    ];
    events.push(json!({
        "type": "ask",
        "tool_call_id": "call_inflight_ask",
        "questions": [{
            "id": "q1",
            "question": "要现在跑完整套件吗？",
            "header": "跑测试",
            "options": [{"label": "只跑这一档"}, {"label": "全部"}],
        }],
        "required": true,
        "created_at": "2026-01-01T00:00:00+00:00",
        "session_id": "bench-session",
        "request_id": "req-0002",
    }));

    Payload {
        messages,
        uncommitted,
        uncommitted_tools,
        events,
    }
}

impl Payload {
    /// The four groups as the JSON the snapshot travels as — the report's byte
    /// count and the determinism guard's comparison both use it.
    fn encoded_json(&self) -> String {
        let mut out = serde_json::to_string(&self.messages).unwrap_or_default();
        for group in [&self.uncommitted_tools, &self.events] {
            out.push_str(&serde_json::to_string(group).unwrap_or_default());
        }
        out.push_str(&serde_json::to_string(&self.uncommitted).unwrap_or_default());
        out
    }
}

// ── the production replay assembly ──────────────────────────────

/// Wall time of each step of one assembly.
#[derive(Clone, Copy, Default)]
struct Phases {
    messages: Duration,
    uncommitted: Duration,
    tools: Duration,
    events: Duration,
    copies: Duration,
}

impl Phases {
    fn total(&self) -> Duration {
        self.messages + self.uncommitted + self.tools + self.events + self.copies
    }
}

/// Replay one snapshot into `chat`, in the production order
/// (`app/projection.rs:542-668`). The step timings are always taken — four
/// clock reads cost ~100 ns against a µs-to-ms assembly, and keeping one code
/// path means the reported breakdown and the measured pass can never drift.
fn replay(chat: &mut ChatView, payload: &Payload) -> Phases {
    // 1. Full state replacement: drop cells + height caches (`model.rs:343`).
    chat.clear();

    // 2. Committed messages.
    let t = Instant::now();
    replay_messages(chat, &payload.messages);
    let messages = t.elapsed();

    // 3. The uncommitted assistant projection — the same replay path.
    let t = Instant::now();
    replay_messages(chat, std::slice::from_ref(&payload.uncommitted));
    let uncommitted = t.elapsed();

    // 4. Unfinished tool calls, through what the live `ToolCallStream` branch
    //    does for each entry (`projection.rs:188-218`): settle the active
    //    thinking block, append to an existing cell, else start a streaming
    //    one. `is_final` is never set on this path — the args are partial.
    let t = Instant::now();
    for tool in &payload.uncommitted_tools {
        chat.finish_active_thinking(Instant::now());
        let id = tool["tool_call_id"].as_str().unwrap_or_default();
        let name = tool["tool_name"].as_str().unwrap_or_default();
        let fragment = tool["args_fragment"].as_str().unwrap_or_default();
        if let Some(idx) = chat.tool_call_index(id) {
            chat.append_tool_args_fragment_by_index(idx, fragment);
        } else {
            let mut block = ToolCallBlock::new_streaming(name.to_string(), id.to_string());
            block.append_args_fragment(fragment);
            chat.push(ChatCell::ToolCall(block));
        }
    }
    let tools = t.elapsed();

    // 5. Durable fact events (diff anchored onto cells built above, ask
    //    rendered as a card). The returned panels are the App's ask-flow
    //    bookkeeping — not a render cost, so they are dropped.
    let t = Instant::now();
    let _ = replay_events(chat, &payload.events);
    let events = t.elapsed();

    // 6. The copy-candidate refresh that closes the production replay
    //    (`projection.rs:692`): the App stores the result in its popup cache
    //    (`commands.rs:306-308`), which is a field assignment here — the
    //    O(cells) scan below is the whole cost. Black-boxed so the optimizer
    //    cannot drop the work an App would really do.
    let t = Instant::now();
    std::hint::black_box(chat.collect_assistant_messages());
    let copies = t.elapsed();

    Phases {
        messages,
        uncommitted,
        tools,
        events,
        copies,
    }
}

/// One frame of the chat band, the way `App::draw` renders it
/// (`app/mod.rs:579-589`): the widget into `content_area` of the band, with the
/// production `CellContext` — stock palette / layout (what the default config
/// yields), no Ctrl+O override, and `ImageOpts::off()`.
///
/// The image options are the one deliberate simplification: with the picture
/// lane enabled, production passes `ImageOpts::anchor(workspace, known, cell)`
/// even for a session with no pictures (`app/images.rs:531-555`), and `off()`
/// only when the lane is disabled. This payload carries no image or link
/// syntax, so the two paths differ by one empty-link-list iteration per cell —
/// equivalent work, simpler fixture.
fn draw_frame(
    view: &mut ChatView,
    buf: &mut Buffer,
    palette: &ThemePalette,
    layout: &LayoutConfig,
) {
    let ctx = CellContext {
        palette,
        thinking_mode: ThinkingMode::Visible,
        thinking_expanded: None,
        layout,
        images: ImageOpts::off(),
    };
    buf.reset();
    ChatViewWidget::new(view, ctx).render(scrollbar::content_area(band()), buf);
}

/// The chat band: the 80×50 viewport the frame benchmarks draw.
fn band() -> Rect {
    Rect::new(0, 0, WIDTH, HEIGHT)
}

// ── benches ─────────────────────────────────────────────────────

fn bench_replay(c: &mut Criterion) {
    let mut group = c.benchmark_group("replay");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    for n in SIZES {
        let payload = payload(n);
        let mut view = ChatView::new();
        // The report is once per case: criterion re-enters the closure for
        // warm-up and for every sample, so the flag must outlive the closure.
        let reported = Cell::new(false);
        group.bench_function(n.to_string(), |b| {
            b.iter(|| {
                let phases = replay(&mut view, &payload);
                if !reported.replace(true) {
                    report_replay(n, &payload, &phases, &view);
                }
            });
        });
    }
    group.finish();
}

fn bench_frame(c: &mut Criterion) {
    let palette = ThemePalette::default();
    let layout = LayoutConfig::default();
    let mut group = c.benchmark_group("frame");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    for n in SIZES {
        let payload = payload(n);
        let mut view = ChatView::new();
        let mut buf = Buffer::empty(band());
        let reported = Cell::new(false);
        group.bench_function(n.to_string(), |b| {
            b.iter(|| {
                let phases = replay(&mut view, &payload);
                let t = Instant::now();
                draw_frame(&mut view, &mut buf, &palette, &layout);
                let frame = t.elapsed();
                if !reported.replace(true) {
                    report_frame(n, &payload, &phases, frame, &view);
                }
            });
        });
    }
    group.finish();
}

fn bench_scroll(c: &mut Criterion) {
    let palette = ThemePalette::default();
    let layout = LayoutConfig::default();
    let mut group = c.benchmark_group("scroll");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    for n in SIZES {
        let payload = payload(n);
        let mut view = ChatView::new();
        let mut buf = Buffer::empty(band());
        // The session is already open: replayed and drawn once, so the caches
        // are the ones a user has after the first frame. Scrolling is what
        // they repeat from there — the production PageDown entry,
        // `page_down(chat_height - 2, chat_height)` (`app/modal.rs:447`),
        // restarted from the top at the bottom edge (Ctrl+Home,
        // `jump_top`), so one full pass walks the whole session.
        replay(&mut view, &payload);
        draw_frame(&mut view, &mut buf, &palette, &layout);
        let span = view.content_height().max(1);
        let reported = Cell::new(false);
        group.bench_function(n.to_string(), |b| {
            b.iter(|| {
                if view.is_at_bottom() {
                    view.jump_top();
                }
                view.page_down(HEIGHT as usize - 2, HEIGHT as usize);
                draw_frame(&mut view, &mut buf, &palette, &layout);
                if !reported.replace(true) {
                    write_stderr(&format!(
                        "session_replay: scroll/{n}: content {span} rows, one PageDown \
                         ({page} rows) + frame per iteration\n",
                        page = HEIGHT as usize - 2,
                    ));
                }
            });
        });
    }
    group.finish();
}

// ── stderr report (first executed pass per case, during warm-up) ──

/// The percentages of the phases against the assembly total.
fn share(part: Duration, total: Duration) -> f64 {
    if total.is_zero() {
        return 0.0;
    }
    100.0 * part.as_secs_f64() / total.as_secs_f64()
}

fn report_replay(n: usize, payload: &Payload, phases: &Phases, view: &ChatView) {
    let total = phases.total();
    write_stderr(&format!(
        "session_replay: replay/{n}: {} messages ({:.1} MiB json), {} cells | \
         {:.3}ms = messages {:.1}% ({:.3}ms) + uncommitted {:.1}% ({:.3}ms) + \
         tools {:.1}% ({:.3}ms) + events {:.1}% ({:.3}ms) + copies {:.1}% ({:.3}ms) | \
         {:.2}µs/message\n",
        payload.messages.len(),
        payload.encoded_json().len() as f64 / (1024.0 * 1024.0),
        view.len(),
        total.as_secs_f64() * 1e3,
        share(phases.messages, total),
        phases.messages.as_secs_f64() * 1e3,
        share(phases.uncommitted, total),
        phases.uncommitted.as_secs_f64() * 1e3,
        share(phases.tools, total),
        phases.tools.as_secs_f64() * 1e3,
        share(phases.events, total),
        phases.events.as_secs_f64() * 1e3,
        share(phases.copies, total),
        phases.copies.as_secs_f64() * 1e3,
        total.as_secs_f64() * 1e6 / payload.messages.len() as f64,
    ));
}

fn report_frame(n: usize, payload: &Payload, phases: &Phases, frame: Duration, view: &ChatView) {
    let total = phases.total();
    write_stderr(&format!(
        "session_replay: frame/{n}: {} messages, {} cells, {} content rows | \
         replay {:.3}ms + first frame {:.3}ms = {:.3}ms (frame {:.1}%)\n",
        payload.messages.len(),
        view.len(),
        view.content_height(),
        total.as_secs_f64() * 1e3,
        frame.as_secs_f64() * 1e3,
        (total + frame).as_secs_f64() * 1e3,
        share(frame, total + frame),
    ));
}

/// The crate denies `println!`/`eprintln!`; a bench reports through stderr.
fn write_stderr(text: &str) {
    use std::io::Write as _;
    let _ = std::io::stderr().write_all(text.as_bytes());
}

/// FNV-1a (64-bit) — the payload's cross-side fingerprint, no dependency.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// One entry point for the whole suite, so the determinism guard runs exactly
/// once per process (also under a `-- <filter>` run).
fn benches(c: &mut Criterion) {
    // Payload identity is a contract: a bench that measured different content
    // on every run could not be compared against a baseline, and perf-ci
    // compares two worktrees side by side. Every size is generated twice and
    // asserted byte-identical, the fill of the last two replay steps is
    // asserted, and each fingerprint is printed for the sides to compare.
    for n in SIZES {
        let first = payload(n);
        let again = payload(n);
        assert_eq!(
            first.messages.len(),
            n,
            "a payload must carry exactly n messages"
        );
        assert!(
            !first.uncommitted_tools.is_empty() && !first.events.is_empty(),
            "the in-flight tail must give replay steps 4 and 5 real work (n={n})"
        );
        let json = first.encoded_json();
        assert_eq!(
            json,
            again.encoded_json(),
            "payload generation is not deterministic (n={n})"
        );
        write_stderr(&format!(
            "session_replay: payload n={n} seed {SEED:#x}: {} bytes, fnv1a {:016x}\n",
            json.len(),
            fnv1a(json.as_bytes()),
        ));
    }

    bench_replay(c);
    bench_frame(c);
    bench_scroll(c);
}

criterion_group!(benches_group, benches);
criterion_main!(benches_group);
