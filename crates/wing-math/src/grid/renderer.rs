// ---------------------------------------------------------------------------
// 来源（vendored，逐字内联）
//   crate   : term-maths
//   version : 1.0.0
//   repo    : 包元数据未声明 repository（crates.io `term-maths`；发布时
//             `.cargo_vcs_info.json` git sha1 = 5a2de3b29f1d4cec72c8685b8d52ddfb53519676）
//   license : MIT OR Apache-2.0，Copyright (c) 2026 Jack Geraghty
//             （原文见 crate 根 LICENSE-MIT / LICENSE-APACHE）
//   原路径  : renderer.rs
//
// 本地改动（相对上游）：
//   1. 模块路径：`rust_latex_parser::` -> `crate::latex::`，
//      `crate::xxx` -> `crate::grid::xxx`（内联后模块位置变化）。
//   2. 运行 `cargo fmt`（仓库门禁要求 `cargo fmt --check` 干净）。上游文件未经 rustfmt
//      处理，因此有纯空白差异；已用「先 rustfmt 上游文件、再与本文件逐行 diff」核对，
//      除上述改动外逐字一致（核对脚本见 crate 根 NOTICE 的「内联保真度」一节）。
//   除以上两点外与上游逐字一致。
// ---------------------------------------------------------------------------

//! Output backend trait and implementations.

use crate::latex::EqNode;

use crate::grid::rendered_block::RenderedBlock;

/// Trait for rendering an `EqNode` AST into a target output format.
pub trait MathRenderer {
    type Output;

    /// Render an equation AST node into the target output.
    fn render(&self, node: &EqNode) -> Self::Output;
}

/// Default renderer that produces a `RenderedBlock` (2D character grid).
/// Always available — no feature gates required.
pub struct TerminalRenderer;

impl MathRenderer for TerminalRenderer {
    type Output = RenderedBlock;

    fn render(&self, node: &EqNode) -> RenderedBlock {
        crate::grid::layout::layout(node)
    }
}
