//! TUI 会话门禁：两条硬不变量必须钉在**真实生产源码**上，而不是注释与代码结构里。
//!
//! r1 的 B1（向导 → 主 TUI 键盘全哑）修法是「用构造消灭坏时序」：接驳必须在**本进程任何
//! 一次 crossterm `poll` 之前**装上（第一段 TUI 的 `begin_tui_session`），并且**段与段之间
//! 绝不拆**（拆解只属于进程收尾 `end_tui_session`）。这两条只是时序约束，编译器管不着：
//! 把 `install()` 挪走 / 删掉、或在 `restore_terminal` 里加回 `restore()`，全量测试照样
//! 全绿，而端到端会精确复现 B1（复审 r2 的 M1/M2/M10 变异与实验）。
//!
//! 探针化（DECRQM）又加了两条同类时序：**settle 必须早于本段第一次 poll**（阴性拆接驳
//! 只有这一次机会，事件源注册之后 fd 0 不能再换），**mode 5522 与协议机只在阳性后**启动
//! （阴性终端一个 5522 字节都不许发）。它们同样只活在源码文本里，所以照旧在这里钉。
//!
//! 所以这里用与 `tests/layer_guard.rs` **同构**的做法：扫描真实 `src/**`，文本匹配、零豁免。
//!
//! 五条规则：
//!
//! 1. `stdin_proxy::install(` 在生产源码里**恰好一次**，且位于 `begin_tui_session`
//!    （安装点唯一 = 时序不可被挪走）；`begin_tui_session()` 的调用点恰好一次，位于
//!    `init_terminal_with`（会话确实由第一段 `init_terminal*` 开启）。
//! 2. `stdin_proxy::restore(` **恰好一次**，且位于 `end_tui_session`；`end_tui_session()`
//!    在 `cmd/mod.rs` 至少被调用一次（收尾路径存在：guard / panic hook / 主路径 / 向导分支）。
//! 3. `restore_terminal`（段级屏恢复）**不含**这两个名字 —— 段边界永远不碰会话。
//! 4. `stdin_proxy::settle_paste_channel(` **恰好两处**：`app/mod.rs::run_app` 中位于
//!    `spawn_event_stream()`（主 TUI 第一次 poll 的创建点）之前，`cmd/mod.rs::run_tui` 中
//!    位于 `run_setup_tui(`（向导第一次 poll）之前。
//! 5. `spawn_event_stream` 里 mode 5522 与协议机都门控在探针 verdict 上（`paste_channel()`
//!    在前，`?5522h` 与 `clipboard_protocol::spawn(` 在后）；`\x1b[?5522h` 全生产只有这一处。
//!
//! 白空格不敏感（`stdin_proxy :: install (` 也算命中，与 `layer_guard` 同一约定）；测试代码
//! 不算生产源码（文件尾部的 `#[cfg(test)] mod …` 之前的部分才是生产正文）。

use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// 包内源码根（`crates/wing/src`）。
const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// 唯一的安装调用点所在文件。
const SESSION_FILE: &str = "tui/mod.rs";

const INSTALL_CALL: &str = "stdin_proxy::install(";
const RESTORE_CALL: &str = "stdin_proxy::restore(";
const BEGIN_CALL: &str = "begin_tui_session()";
const END_CALL: &str = "end_tui_session()";
const SETTLE_CALL: &str = "stdin_proxy::settle_paste_channel(";
const VERDICT_READ: &str = "stdin_proxy::paste_channel()";
const SPAWN_CALL: &str = "spawn_event_stream()";
const MACHINE_CALL: &str = "clipboard_protocol::spawn(";
const ENABLE_CALL: &str = "enable_paste_events(&mut";
/// 源码里的字面量（含转义字节），不是渲染后的 `?5522h`。
const PASTE_EVENTS_BYTES: &str = r"\x1b[?5522h";

/// 一条命中：文件内 1 起的行号 + 该行原文。
#[derive(Debug, PartialEq, Eq)]
struct Hit {
    line: usize,
    text: String,
}

/// 逐文件扫描的产物：`src` 相对路径 + 生产正文（测试模块已切掉）。
#[derive(Debug)]
struct Source {
    path: String,
    production: String,
}

fn sources() -> Vec<Source> {
    let root = PathBuf::from(SRC);
    assert!(root.is_dir(), "源码根不存在：{}", root.display());
    let mut out = Vec::new();
    collect(&root, &mut out);
    assert!(
        out.iter().any(|s| s.path == SESSION_FILE),
        "门禁没有扫到 {SESSION_FILE}（walk 提前停了？）"
    );
    out
}

fn collect(dir: &Path, out: &mut Vec<Source>) {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let path = path
                .strip_prefix(SRC)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(Source {
                path,
                production: production_text(&text).to_string(),
            });
        }
    }
}

/// 文件的生产正文：**文件尾部**的测试模块（`#[cfg(test)]` + 紧随的 `mod …`）之前的部分。
///
/// 只切「标记后面跟着模块声明」的那一处：`#[cfg(test)]` 也能标在单个 item 上（测试专用
/// 访问器），其后仍然是生产代码，切在那里会把生产代码藏起来。
fn production_text(text: &str) -> &str {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut offset = 0;
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with("#[cfg(test)]")
            && lines[index + 1..]
                .iter()
                .find(|next| !is_insignificant(next))
                .is_some_and(|next| opens_a_module(next))
        {
            return &text[..offset];
        }
        offset += line.len();
    }
    text
}

fn is_insignificant(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || line.starts_with("//") || line.starts_with("#[")
}

fn opens_a_module(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("mod ") || line.starts_with("pub mod ") || line.starts_with("pub(crate) mod ")
}

/// 把 `text` 里所有白空格去掉，逐字符记录 `(压缩串里的字节偏移, 原始行号)`——源码里有中文
/// 注释（多字节），命中偏移是**字节**偏移，所以映射必须按字节走（同 `layer_guard` 的做法）。
fn squashed_lines(text: &str) -> (String, Vec<(usize, usize)>) {
    let mut squashed = String::with_capacity(text.len());
    let mut at = Vec::with_capacity(text.len());
    let mut line = 1;
    for ch in text.chars() {
        if ch == '\n' {
            line += 1;
            continue;
        }
        if ch.is_whitespace() {
            continue;
        }
        at.push((squashed.len(), line));
        squashed.push(ch);
    }
    (squashed, at)
}

/// 压缩串里字节偏移 `offset` 处的原始行号。
fn line_of(at: &[(usize, usize)], offset: usize) -> usize {
    match at.binary_search_by_key(&offset, |(offset, _)| *offset) {
        Ok(index) => at[index].1,
        Err(index) => at.get(index.saturating_sub(1)).map_or(1, |(_, line)| *line),
    }
}

/// `needle` 在 `text` 里的全部命中（白空格不敏感，注释也算 —— 引用也是引用）。
fn hits(text: &str, needle: &str) -> Vec<Hit> {
    let (squashed, at) = squashed_lines(text);
    let original: Vec<&str> = text.lines().collect();
    let mut out: Vec<Hit> = Vec::new();
    let mut from = 0;
    while let Some(offset) = squashed[from..].find(needle) {
        let start = from + offset;
        let line = line_of(&at, start);
        if !out.iter().any(|hit| hit.line == line) {
            out.push(Hit {
                line,
                text: original.get(line - 1).unwrap_or(&"").trim().to_string(),
            });
        }
        from = start + 1;
    }
    out
}

/// 顶层函数声明（行首的 `fn` / `pub fn` / `pub(crate) fn`，含 `async fn`）：返回名字。
fn top_level_fn(line: &str) -> Option<&str> {
    let line = line
        .strip_prefix("pub(crate) ")
        .or_else(|| line.strip_prefix("pub "))
        .unwrap_or(line);
    let line = line.strip_prefix("async ").unwrap_or(line);
    let line = line.strip_prefix("fn ")?;
    let end = line.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    Some(&line[..end])
}

/// `line` 所在的顶层函数名（向上找最近的顶层 `fn` 声明）。
fn enclosing_fn(text: &str, line: usize) -> Option<String> {
    let mut found = None;
    for (index, raw) in text.lines().enumerate() {
        if index + 1 > line {
            break;
        }
        if let Some(name) = top_level_fn(raw) {
            found = Some(name.to_string());
        }
    }
    found
}

/// 只算「真调用」的命中：行首不是 `//` 的（把调用注释掉不再算数——规则 4/5 数的是调用点，
/// 注释里点名不算；既有规则 1-3 仍按「引用也是引用」的老口径）。
fn live_hits(text: &str, needle: &str) -> Vec<Hit> {
    hits(text, needle)
        .into_iter()
        .filter(|hit| !hit.text.starts_with("//"))
        .collect()
}

/// `name` 这个顶层函数的正文（从声明行到下一个行首 `}`）。找不到即 panic（门禁不得空转）。
fn function_body(text: &str, name: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = lines
        .iter()
        .position(|line| top_level_fn(line) == Some(name))
        .unwrap_or_else(|| panic!("production source has no top-level `fn {name}`"));
    let end = lines[start..]
        .iter()
        .position(|line| line.trim_end() == "}")
        .unwrap_or_else(|| panic!("`fn {name}` has no top-level closing brace"));
    lines[start..start + end + 1].concat()
}

/// 全 `src/**` 里 `needle` 的命中，附文件路径。
fn hits_in_sources(needle: &str) -> Vec<(String, Hit)> {
    sources()
        .into_iter()
        .flat_map(|source| {
            hits(&source.production, needle)
                .into_iter()
                .map(move |hit| (source.path.clone(), hit))
        })
        .collect()
}

fn describe(hits: &[(String, Hit)]) -> String {
    hits.iter()
        .map(|(path, hit)| format!("\n  {path}:{}  {}", hit.line, hit.text))
        .collect()
}

/// 规则 1：接驳的安装点唯一、且在 `begin_tui_session`；会话确实由第一段 `init_terminal*` 开。
#[test]
fn the_splice_is_installed_once_by_begin_tui_session() {
    let installs = hits_in_sources(INSTALL_CALL);
    assert_eq!(
        installs.len(),
        1,
        "`{INSTALL_CALL}` 必须恰好一处（安装时序不能有第二个入口）：{}",
        describe(&installs)
    );
    let (path, hit) = &installs[0];
    assert_eq!(path, SESSION_FILE, "安装点必须在 {SESSION_FILE}");
    let source = sources()
        .into_iter()
        .find(|s| s.path == SESSION_FILE)
        .expect("session file");
    assert_eq!(
        enclosing_fn(&source.production, hit.line).as_deref(),
        Some("begin_tui_session"),
        "安装点必须位于 `begin_tui_session`（真实 tty 进 raw 之后、任何 poll 之前）"
    );

    // 会话的开启点也唯一：第一段 `init_terminal*` 通过它开，别处不许开。
    let begins: Vec<(String, Hit)> = hits_in_sources(BEGIN_CALL)
        .into_iter()
        .filter(|(path, _)| path == SESSION_FILE)
        .filter(|(_, hit)| {
            enclosing_fn(&source.production, hit.line).as_deref() != Some("begin_tui_session")
        })
        .collect();
    assert_eq!(
        begins.len(),
        1,
        "`{BEGIN_CALL}` 的调用点必须恰好一处：{}",
        describe(&begins)
    );
    assert_eq!(
        enclosing_fn(&source.production, begins[0].1.line).as_deref(),
        Some("init_terminal_with"),
        "`begin_tui_session()` 只能由第一段 `init_terminal*` 调用"
    );
}

/// 规则 2：接驳的拆解点唯一、且在 `end_tui_session`；收尾路径确实存在。
#[test]
fn the_splice_is_restored_once_by_end_tui_session() {
    let restores = hits_in_sources(RESTORE_CALL);
    assert_eq!(
        restores.len(),
        1,
        "`{RESTORE_CALL}` 必须恰好一处（段边界绝不许拆接驳）：{}",
        describe(&restores)
    );
    let (path, hit) = &restores[0];
    assert_eq!(path, SESSION_FILE, "拆解点必须在 {SESSION_FILE}");
    let source = sources()
        .into_iter()
        .find(|s| s.path == SESSION_FILE)
        .expect("session file");
    assert_eq!(
        enclosing_fn(&source.production, hit.line).as_deref(),
        Some("end_tui_session"),
        "拆解点必须位于 `end_tui_session`（进程收尾：先还 fd 0 再写回 termios）"
    );

    // 收尾必须真的被调用：`cmd/mod.rs` 承载 guard / panic hook / 主路径 / 向导分支。
    let ends_in_cmd = hits_in_sources(END_CALL)
        .into_iter()
        .filter(|(path, _)| path == "cmd/mod.rs")
        .count();
    assert!(
        ends_in_cmd >= 1,
        "`{END_CALL}` 必须至少有一个生产调用点（否则终端留在 raw）"
    );
}

/// 规则 3：段级屏恢复 `restore_terminal` 永远不碰会话。
#[test]
fn restore_terminal_never_touches_the_session() {
    let source = sources()
        .into_iter()
        .find(|s| s.path == SESSION_FILE)
        .expect("session file");
    let body = function_body(&source.production, "restore_terminal");
    for needle in ["stdin_proxy::install", "stdin_proxy::restore"] {
        assert!(
            !hits(&body, needle)
                .iter()
                .any(|hit| !hit.text.starts_with("//")),
            "`restore_terminal` 不得引用 `{needle}` —— 段边界拆接驳会把第二段键盘掐死"
        );
    }
}

/// 某文件的生产正文（找不到即 panic：门禁不得空转）。
fn production_of(path: &str) -> String {
    sources()
        .into_iter()
        .find(|source| source.path == path)
        .unwrap_or_else(|| panic!("门禁没有扫到 {path}"))
        .production
}

/// 规则 4：探针判定只在两处，且都赶在各自段的第一次 crossterm `poll` 之前。
///
/// - `app/mod.rs::run_app`：`spawn_event_stream()`（主 TUI 第一次 poll 的创建点）之前；
/// - `cmd/mod.rs::run_tui`：`run_setup_tui(`（向导自己的第一次 poll）之前。
///
/// 阴性拆接驳（`restore` 的 `dup2`）只有这一次机会：事件源单例一注册，fd 0 就不能再换。
#[test]
fn the_probe_settles_before_the_first_poll_of_each_segment() {
    const APP_FILE: &str = "app/mod.rs";
    const CMD_FILE: &str = "cmd/mod.rs";
    let settles = hits_in_sources(SETTLE_CALL)
        .into_iter()
        .filter(|(_, hit)| !hit.text.starts_with("//"))
        .collect::<Vec<_>>();
    assert_eq!(
        settles.len(),
        2,
        "`{SETTLE_CALL}` 必须恰好两处真调用（主 TUI 与向导各一次）：{}",
        describe(&settles)
    );

    // 主 TUI：位于 `run_app`，且在 `spawn_event_stream()` 之前。
    let app = production_of(APP_FILE);
    let (path, hit) = settles
        .iter()
        .find(|(path, _)| path == APP_FILE)
        .unwrap_or_else(|| panic!("{APP_FILE} 必须有 settle 调用点：{}", describe(&settles)));
    assert_eq!(path, APP_FILE);
    assert_eq!(
        enclosing_fn(&app, hit.line).as_deref(),
        Some("run_app"),
        "主 TUI 的判定必须在 `run_app`（{APP_FILE}:{}）",
        hit.line
    );
    let spawns = hits(&app, SPAWN_CALL);
    assert_eq!(spawns.len(), 1, "`{SPAWN_CALL}` 必须恰好一次：{spawns:?}");
    assert!(
        hit.line < spawns[0].line,
        "settle（{}）必须早于 `{SPAWN_CALL}`（{}）——后者创建本进程第一次 poll",
        hit.line,
        spawns[0].line
    );

    // 向导：位于 `run_tui`，且在 `run_setup_tui(`（向导的第一次 poll）之前。
    let cmd = production_of(CMD_FILE);
    let (path, hit) = settles
        .iter()
        .find(|(path, _)| path == CMD_FILE)
        .unwrap_or_else(|| panic!("{CMD_FILE} 必须有 settle 调用点：{}", describe(&settles)));
    assert_eq!(path, CMD_FILE);
    assert_eq!(
        enclosing_fn(&cmd, hit.line).as_deref(),
        Some("run_tui"),
        "向导的判定必须在 `run_tui`（{CMD_FILE}:{}）",
        hit.line
    );
    let wizard = hits(&cmd, "run_setup_tui(");
    assert_eq!(wizard.len(), 1, "向导循环必须恰好启动一次：{wizard:?}");
    assert!(
        hit.line < wizard[0].line,
        "settle（{}）必须早于向导的第一次 poll（{}）",
        hit.line,
        wizard[0].line
    );
}

/// 规则 5：mode 5522 与协议机都门控在探针 verdict 上，且 `?5522h` 全生产只有一处。
#[test]
fn the_paste_channel_starts_only_after_a_positive_probe() {
    let source = production_of(SESSION_FILE);
    let body = function_body(&source, "spawn_event_stream");

    let verdict = live_hits(&body, VERDICT_READ);
    assert_eq!(
        verdict.len(),
        1,
        "`spawn_event_stream` 必须恰好读一次判定：{verdict:?}"
    );
    assert!(
        live_hits(&body, "PasteChannel::Supported").len() == 1,
        "门控必须显式比 `PasteChannel::Supported`"
    );
    let machine = live_hits(&body, MACHINE_CALL);
    assert_eq!(machine.len(), 1, "协议机启动点必须唯一");
    let mode = live_hits(&body, ENABLE_CALL);
    assert_eq!(mode.len(), 1, "`?5522h` 的发送点必须唯一");
    assert!(
        verdict[0].line < mode[0].line && mode[0].line < machine[0].line,
        "顺序必须是「先判定、再开 mode、最后起协议机」（{} / {} / {}）",
        verdict[0].line,
        mode[0].line,
        machine[0].line
    );

    // 唯一的 5522 开模式字节：整棵生产树只有那一处 `\x1b[?5522h`（测试正文已切掉）。
    let writers = hits_in_sources(PASTE_EVENTS_BYTES)
        .into_iter()
        .filter(|(_, hit)| !hit.text.starts_with("//"))
        .collect::<Vec<_>>();
    assert_eq!(
        writers.len(),
        1,
        "`{PASTE_EVENTS_BYTES}` 必须恰好一处（唯一发送点）：{}",
        describe(&writers)
    );
    assert_eq!(writers[0].0, SESSION_FILE);
    let enable_body = function_body(&source, "enable_paste_events");
    assert_eq!(
        live_hits(&enable_body, PASTE_EVENTS_BYTES).len(),
        1,
        "唯一的发送点必须在 `enable_paste_events` 里"
    );
}

// ── 扫描器自检（合成输入）：门禁的牙长在这儿 ───────────────────

#[test]
fn the_scanner_sees_a_moved_call_and_ignores_test_only_ones() {
    let text = "\
fn init_terminal_with() {
    begin_tui_session()?;
}

fn begin_tui_session() -> io::Result<()> {
    stdin_proxy::install()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    fn t() {
        stdin_proxy::install().unwrap();
    }
}
";
    // 生产正文里的安装点找到了，且归属 `begin_tui_session`。
    let production = production_text(text);
    let installs = hits(production, INSTALL_CALL);
    assert_eq!(installs.len(), 1);
    assert_eq!(
        enclosing_fn(production, installs[0].line).as_deref(),
        Some("begin_tui_session")
    );
    // 测试模块里的那次不算。
    assert_eq!(hits(text, INSTALL_CALL).len(), 2, "全文两处（含测试）");
    // 开启点的调用归属 `init_terminal_with`。
    let begins: Vec<Hit> = hits(production, BEGIN_CALL)
        .into_iter()
        .filter(|hit| enclosing_fn(production, hit.line).as_deref() != Some("begin_tui_session"))
        .collect();
    assert_eq!(begins.len(), 1);
    assert_eq!(
        enclosing_fn(production, begins[0].line).as_deref(),
        Some("init_terminal_with")
    );

    // 把调用挪到别的函数：归属判定会把它指出来（门禁的相等断言随即变红）。
    let moved = text.replace("fn begin_tui_session()", "fn elsewhere()");
    let production = production_text(&moved);
    let installs = hits(production, INSTALL_CALL);
    assert_eq!(installs.len(), 1);
    assert_eq!(
        enclosing_fn(production, installs[0].line).as_deref(),
        Some("elsewhere"),
        "挪走的安装点必须被判到新归属"
    );
}

#[test]
fn the_scanner_handles_whitespace_and_the_body_extractor() {
    // 白空格不敏感：`stdin_proxy :: install (` 也是命中。
    let spaced = "fn f() {\n    stdin_proxy :: install ()?;\n}\n";
    assert_eq!(hits(spaced, INSTALL_CALL).len(), 1);

    // 函数体提取：只到顶层 `}`，不吞后面的函数。
    let text = "\
pub fn restore_terminal(terminal: &mut WingTerminal) -> Result<()> {
    leave_sequence(terminal.backend_mut())?;
    Ok(())
}

pub fn end_tui_session() -> io::Result<()> {
    stdin_proxy::restore();
    Ok(())
}
";
    let body = function_body(text, "restore_terminal");
    assert!(body.contains("leave_sequence"));
    assert!(!body.contains("end_tui_session"));
    assert_eq!(hits(&body, "stdin_proxy::restore(").len(), 0);
    let body = function_body(text, "end_tui_session");
    assert_eq!(hits(&body, RESTORE_CALL).len(), 1);
}
