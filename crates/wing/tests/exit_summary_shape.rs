//! 形态守门：退出摘要只属于 TUI 形态。
//!
//! 退出摘要在终端恢复后打印到 stdout——那是 TUI 的终端。stdio 与 ACP 的
//! stdout 是**协议通道**（NDJSON / JSON-RPC），一张表就是一帧垃圾，会直接
//! 破坏契约。Rust 的单 crate 结构没法用可见性表达"只许 TUI 入口调用"
//! （`pub(crate)` 只挡 crate 外），所以规则在这里以文本扫描钉住（与
//! `layer_guard` 同一手法）：
//!
//! 1. `exit_summary` 模块与被引用关系存在（防重命名后本测试静默真空）；
//! 2. 引用它的文件只有 `cmd/exit_summary.rs` 与 TUI 入口 `cmd/mod.rs`；
//! 3. `cmd/mod.rs` 里的调用必须落在 **`run_tui` 函数体内**——同文件的
//!    `dispatch` / `dispatch_stdio` 都是 stdio 的宿主，文件级白名单挡不住
//!    "把调用搬进 stdio 分支"这种事故。
//!
//! 扫描是文本级的（丢空白后匹配）：注释与跨行路径一样算数。函数归属用
//! "往上找最近的 `fn` 声明"判定——本仓库的函数签名都写在同一行，`fn` 关键字
//! 不出现在字符串里；这是刻意接受的口径（守护规则本身由本测试的正例与
//! 反例说明）。

use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// Crate source root (`crates/wing/src`).
const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// 允许引用 `exit_summary` 的文件：模块自身 + TUI 入口。
const ALLOWED: &[&str] = &["cmd/exit_summary.rs", "cmd/mod.rs"];

/// `cmd/mod.rs` 里唯一允许调用摘要的函数。
const ENTRY_FN: &str = "run_tui";

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn relative(path: &Path) -> String {
    path.strip_prefix(SRC)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 一次引用：所在行、行文本、是否注释行，与往上最近的 `fn` 声明名（顶层 = None）。
struct Reference {
    line: usize,
    text: String,
    is_comment: bool,
    function: Option<String>,
}

/// `source` 里所有提到 `exit_summary` 的行及其函数归属。
fn references(source: &str) -> Vec<Reference> {
    let mut out = Vec::new();
    let mut function: Option<String> = None;
    for (index, line) in source.lines().enumerate() {
        if let Some(name) = fn_name(line) {
            function = Some(name);
        }
        if line.replace(' ', "").contains("exit_summary") {
            out.push(Reference {
                line: index + 1,
                text: line.trim().to_string(),
                is_comment: line.trim_start().starts_with("//"),
                function: function.clone(),
            });
        }
    }
    out
}

/// `async fn run_tui(` / `fn foo(` → 函数名。
fn fn_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed
        .strip_prefix("pub ")
        .or_else(|| trimmed.strip_prefix("pub(crate) "))
        .unwrap_or(trimmed);
    let rest = rest
        .strip_prefix("async fn ")
        .or_else(|| rest.strip_prefix("fn "))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

#[test]
fn only_the_tui_entry_reaches_the_exit_summary() {
    let root = PathBuf::from(SRC);
    let summary = root.join("cmd/exit_summary.rs");
    let entry = root.join("cmd/mod.rs");
    assert!(
        summary.is_file(),
        "退出摘要模块不见了（{}）——本守门测试会静默真空，先把它找回来",
        summary.display()
    );

    let entry_text = fs::read_to_string(&entry).expect("cmd/mod.rs readable");
    let entry_refs = references(&entry_text);
    assert!(
        entry_refs
            .iter()
            .any(|r| r.function.as_deref() == Some(ENTRY_FN)),
        "TUI 入口（{ENTRY_FN}）不再调用退出摘要——形态守门失去意义"
    );
    for reference in &entry_refs {
        // 顶层 `mod exit_summary;` 是模块声明本身；其余引用必须在 run_tui 里。
        let is_module_decl = reference.function.is_none()
            && reference.text.starts_with("mod ")
            && reference.text.ends_with(';');
        assert!(
            is_module_decl || reference.function.as_deref() == Some(ENTRY_FN),
            "cmd/mod.rs:{} 的摘要引用不在 `{ENTRY_FN}` 里（stdio / CLI 分发都在同一个文件，\
             文件级白名单挡不住这类事故）：{}",
            reference.line,
            reference.text
        );
    }

    let mut files = Vec::new();
    collect_rs(&root, &mut files);
    assert!(files.len() > 50, "扫描面异常小，疑似扫描器失效");

    for file in files {
        let rel = relative(&file);
        if ALLOWED.contains(&rel.as_str()) {
            continue;
        }
        // 协议前端（stdio / acp）零容忍：连指路的注释都不该出现在那里。
        // 其余位置只在**代码**里出现才算事故——「谁打印」由代码决定，
        // 一个文档指针（如 app 层注释指向读数在哪）不改变形态。
        let protocol_frontend = rel.starts_with("stdio/") || rel.starts_with("acp/");
        let text = fs::read_to_string(&file).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"));
        for reference in references(&text) {
            if reference.is_comment && !protocol_frontend {
                continue;
            }
            panic!(
                "{}:{} 引用了退出摘要——它只属于 TUI 形态（stdio / acp 的 stdout 是协议通道）：{}",
                rel, reference.line, reference.text
            );
        }
    }
}
