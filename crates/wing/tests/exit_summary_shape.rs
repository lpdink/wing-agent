//! 形态守门：退出摘要只属于 TUI 形态。
//!
//! 退出摘要在终端恢复后打印到 stdout——那是 TUI 的终端。stdio 与 ACP 的
//! stdout 是**协议通道**（NDJSON / JSON-RPC），一张表就是一帧垃圾，会直接
//! 破坏契约。Rust 的单 crate 结构没法用可见性表达"只许 cmd 调用"（`pub(crate)`
//! 只挡 crate 外），所以规则在这里以文本扫描钉住（与 `layer_guard` 同一手法）：
//!
//! 1. `exit_summary` 模块存在且被唯一入口调用（防重命名后本测试静默真空）；
//! 2. 引用它的文件只有 TUI 入口 `cmd/mod.rs` 与它自己——stdio / acp / 编排
//!    CLI 任何地方出现引用即红。
//!
//! 扫描是文本级的（丢空白后匹配）：注释与跨行路径一样算数。

use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// Crate source root (`crates/wing/src`).
const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// 允许引用 `exit_summary` 的文件：模块自身 + TUI 入口。
const ALLOWED: &[&str] = &["cmd/exit_summary.rs", "cmd/mod.rs"];

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

    // 唯一入口真的调用它（否则"没人用"也会让下面的扫描通过）。
    let entry_text = fs::read_to_string(&entry).expect("cmd/mod.rs readable");
    assert!(
        entry_text.contains("exit_summary::print_exit_summary"),
        "TUI 入口不再调用退出摘要——形态守门失去意义"
    );

    let mut files = Vec::new();
    collect_rs(&root, &mut files);
    assert!(files.len() > 50, "扫描面异常小，疑似扫描器失效");

    for file in files {
        let rel = relative(&file);
        let text = fs::read_to_string(&file).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"));
        for (index, line) in text.lines().enumerate() {
            if !line.replace(' ', "").contains("exit_summary") {
                continue;
            }
            assert!(
                ALLOWED.contains(&rel.as_str()),
                "{}:{} 引用了退出摘要——它只属于 TUI 形态（stdio / acp 的 stdout 是协议通道）：\n  {}",
                rel,
                index + 1,
                line.trim()
            );
        }
    }
}
