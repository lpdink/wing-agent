//! 测试模块共用的小工具。
//!
//! 各用例里「量行宽 / 把行集拼成文本」的写法原本在每个测试模块里各抄一份；这里只留一份。
//! 放在 `render/` 而不是 `ui/`：两层都要用，而依赖方向只有 `ui → render`。

/// 一行的显示宽度（生产实现见 [`crate::render::markdown::types`]）。
pub(crate) use super::markdown::types::line_width;

/// 行集的纯文本（每行一条，`\n` 连接）。
pub(crate) fn text_of(lines: &[ratatui::text::Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
