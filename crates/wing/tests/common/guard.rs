//! 源码扫描原语：`layer_guard`（分层规则）与 `session_guard`（终端时序规则）共用。
//!
//! 两份门禁的规则表完全不同，但「怎么读真实 `src/**`」只有一份：白空格处理、测试模块
//! 截断、行号映射这些约定如果各写一遍，修 bug 只会修到一半。

use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// 包内源码根（`crates/wing/src`）。
pub const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// 逐文件扫描的产物：`path`（相对 `SRC`、斜杠分隔）+ 原始文本。
#[derive(Debug)]
pub struct Source {
    pub path: String,
    pub text: String,
}

/// 深度优先收集 `dir` 下全部 `*.rs`（排序，报告稳定）。
pub fn collect(dir: &Path, out: &mut Vec<Source>) {
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
            out.push(Source {
                path: relative(&path),
                text,
            });
        }
    }
}

/// `SRC` 相对、斜杠分隔的路径（如 `ui/cells/ask_msg.rs`）。
pub fn relative(path: &Path) -> String {
    path.strip_prefix(SRC)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 文件的生产正文：**文件尾部**的测试模块（`#[cfg(test)]` + 紧随的 `mod …`）之前的部分。
///
/// 只切「标记后面跟着模块声明」的那一处：`#[cfg(test)]` 也能标在单个 item 上（测试专用
/// 访问器），其后仍然是生产代码，切在那里会把生产代码藏起来。
pub fn production_text(text: &str) -> &str {
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

/// 一行 `mod` 声明可以躲在什么后面：空行、注释、属性。
pub fn is_insignificant(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || line.starts_with("//") || line.starts_with("#[")
}

/// 这一行是否声明了模块（`mod tests {` / `pub(crate) mod x;`）。
pub fn opens_a_module(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("mod ") || line.starts_with("pub mod ") || line.starts_with("pub(crate) mod ")
}

/// 白空格压缩后的源码：逐字符记「压缩串偏移 → 原始字节偏移 + 原始行号」。
///
/// 源码里有中文注释（多字节），命中偏移是**字节**偏移，所以映射必须按字节走。
/// Rust 的 token 流不在乎路径里的空白（`crate :: app`、跨行的 `crate::{`），
/// 门禁也不该在乎；但报告仍要给出真实行号。
pub struct Squashed {
    pub text: String,
    /// 每个保留字符的 `(压缩串偏移, 原始字节偏移, 1 起的行号)`；只经 `origin` 读取。
    at: Vec<(usize, usize, usize)>,
}

impl Squashed {
    pub fn new(source: &str) -> Self {
        let mut text = String::with_capacity(source.len());
        let mut at = Vec::with_capacity(source.len());
        let mut line = 1;
        for (offset, ch) in source.char_indices() {
            if ch == '\n' {
                line += 1;
                continue;
            }
            if ch.is_whitespace() {
                continue;
            }
            at.push((text.len(), offset, line));
            text.push(ch);
        }
        Self { text, at }
    }

    /// 压缩串偏移 `offset` 处字符的 `(原始字节偏移, 1 起行号)`。
    pub fn origin(&self, offset: usize) -> (usize, usize) {
        match self
            .at
            .binary_search_by_key(&offset, |(squashed, _, _)| *squashed)
        {
            Ok(i) => (self.at[i].1, self.at[i].2),
            // needle 总从保留字符起止；越界只可能是末尾的边界查询。
            Err(i) => self
                .at
                .get(i.saturating_sub(1))
                .map_or((0, 1), |&(_, o, l)| (o, l)),
        }
    }
}
