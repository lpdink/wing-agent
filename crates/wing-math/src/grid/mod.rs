// ---------------------------------------------------------------------------
// 来源（vendored，逐字内联）
//   crate   : term-maths 1.0.0 — MIT OR Apache-2.0, Copyright (c) 2026 Jack Geraghty
//             （原文见 crate 根 LICENSE-MIT / LICENSE-APACHE）
//   repo    : 包元数据未声明 repository（crates.io `term-maths`；发布时 `.cargo_vcs_info.json`
//             git sha1 = 5a2de3b29f1d4cec72c8685b8d52ddfb53519676）
//   原路径  : src/lib.rs
//
// 本地改动（相对上游）：
//   1. 模块路径：`rust_latex_parser::` -> `crate::latex::`；doc 示例里的 `term_maths::render` ->
//      `wing_math::grid::render`。
//   2. 移除未内联的后端与其 feature 门：`crossterm_renderer` / `ratatui_widget` / `python`（PyO3）
//      三个模块与对应 `#[cfg(feature = ...)]` 分支、re-export。本 crate 不暴露任何 feature；这三个
//      后端不属引擎（见 crate 根 NOTICE）。
//   3. 运行 `cargo fmt`（门禁要求干净；上游文件未经 rustfmt 处理故有纯空白差异，已逐行 diff 核对）。
//   除以上三点外与上游逐字一致。
// ---------------------------------------------------------------------------

//! # term-maths
//!
//! Character-grid mathematical notation renderer for terminals.
//!
//! Accepts LaTeX math input and renders it as 2D Unicode character art suitable
//! for display in a terminal. Targets JuliaMono as the recommended font.
//!
//! ## Quick Start
//!
//! ```rust
//! let block = wing_math::grid::render(r"\frac{a}{b}");
//! println!("{}", block);
//! //  a
//! // ───
//! //  b
//! ```
//!
//! ## Output Backends
//!
//! - **Plain text** — always available via [`render()`] and [`Display`](std::fmt::Display)
//! - **LaTeX round-trip** — serialise back to LaTeX via [`to_latex()`]

pub mod latex_renderer;
pub mod layout;
pub mod mathfont;
pub mod rendered_block;
pub mod renderer;

pub use latex_renderer::LatexRenderer;
pub use rendered_block::RenderedBlock;
pub use renderer::{MathRenderer, TerminalRenderer};

use crate::latex::parse_equation;

/// Parse a LaTeX math string and render it as a 2D character grid.
///
/// This is the primary entry point for the library.
///
/// ```rust
/// let block = wing_math::grid::render(r"x^2 + y^2 = z^2");
/// assert_eq!(format!("{}", block), "x² + y² = z²");
/// ```
pub fn render(latex: &str) -> RenderedBlock {
    let ast = parse_equation(latex);
    layout::layout(&ast)
}

/// Parse a LaTeX math string and serialise it back to LaTeX (round-trip).
///
/// Useful for normalising LaTeX input or for the LaTeX output backend.
pub fn to_latex(latex: &str) -> String {
    let ast = parse_equation(latex);
    let renderer = LatexRenderer;
    renderer.render(&ast)
}
