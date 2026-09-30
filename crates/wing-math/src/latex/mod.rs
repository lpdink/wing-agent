// ---------------------------------------------------------------------------
// 来源（vendored，逐字内联）
//   crate   : rust-latex-parser
//   version : 0.1.0
//   repo    : https://github.com/William-Selna/Rust-LaTeX-Parser
//             （发布时 .cargo_vcs_info.json git sha1 = 086d394561465b4154272951f5cd524f77855e99）
//   license : MIT，Copyright (c) 2026 William-Selna
//             （原文见 crate 根 LICENSE-MIT-rust-latex-parser；上游 crate 未随包附 LICENSE 文件，
//               本文件由仓库 LICENSE 取得）
//   原路径  : src/lib.rs
//
// 本地改动（相对上游）：
//   1. 模块路径：`crate::ast` -> `crate::latex::ast`，`rust_latex_parser::`
//      -> `crate::latex::`（内联后模块位置变化）。
//   2. doc 示例里的 use 路径同步改写为 `wing_math::latex::...`。
//   3. 运行 `cargo fmt`（仓库门禁要求 `cargo fmt --check` 干净）。上游文件未经 rustfmt
//      处理，因此有纯空白差异；已用「先 rustfmt 上游文件、再与本文件逐行 diff」核对，
//      除上述改动外逐字一致（核对脚本见 crate 根 NOTICE 的「内联保真度」一节）。
//   除以上三点外与上游逐字一致（含文件内联测试）。
// ---------------------------------------------------------------------------

//! # rust-latex-parser
//!
//! A LaTeX equation parser that produces an abstract syntax tree.
//!
//! Feed it a string of LaTeX math markup, get back an [`EqNode`] tree. No
//! rendering opinions, no font dependencies, no runtime allocation tricks —
//! just parsing.
//!
//! The tree is yours to walk however you want: render to SVG, convert to
//! MathML, draw on a Skia canvas, dump to a terminal. The crate doesn't care.
//!
//! # Quick start
//!
//! ```
//! use wing_math::latex::{parse_equation, EqNode};
//!
//! let tree = parse_equation("\\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}");
//! assert!(matches!(tree, EqNode::Frac(_, _)));
//! ```
//!
//! # Bareword shortcuts
//!
//! You don't always need backslashes. The parser recognizes common names as
//! barewords, so `pi` works the same as `\pi`, `sqrt(x)` works like
//! `\sqrt{x}`, and `int_0^1` works like `\int_0^1`.
//!
//! Parentheses after bareword operators act as invisible grouping:
//! `sqrt(x+1)` parses the full `x+1` as the argument.
//!
//! # Supported syntax
//!
//! | Category | Examples |
//! |----------|---------|
//! | Superscripts / subscripts | `x^2`, `x_{i+1}`, `x^2_3` |
//! | Fractions | `a/b`, `\frac{a}{b}` |
//! | Square roots | `sqrt(x)`, `\sqrt{x}` |
//! | Greek letters | `pi`, `\alpha`, `\Omega` |
//! | Big operators | `\sum_{i=0}^{n}`, `int_0^1`, `\prod` |
//! | Limit operators | `lim_{x \to 0}`, `sin`, `log_2` |
//! | Accents | `\hat{x}`, `\bar{x}`, `\vec{v}` |
//! | Matrices | `\begin{pmatrix} a & b \\\\ c & d \end{pmatrix}` |
//! | Cases | `\begin{cases} x & x>0 \\\\ 0 & x=0 \end{cases}` |
//! | Delimiters | `\left( ... \right)` |
//! | Math fonts | `\mathbb{R}`, `\mathcal{F}`, `\mathbf{v}` |
//! | Binomials | `\binom{n}{k}` |
//! | Braces | `\overbrace{a+b}^{n}`, `\underbrace{...}_{text}` |
//! | Stacked | `\overset{def}{=}`, `\underset{lim}{=}` |
//! | 130+ symbols | `\pm`, `\leq`, `\in`, `\rightarrow`, `\infty`, ... |
//!
//! # Error handling
//!
//! The parser never fails. Malformed input produces a best-effort tree:
//! unmatched braces get ignored, unknown commands become literal text nodes,
//! and so on. This is intentional — it keeps live-preview editors responsive
//! while the user is still typing.

pub mod ast;
pub mod parser;

pub use ast::{AccentKind, EqMetrics, EqNode, MathFontKind, MatrixKind};
pub use parser::{latex_to_unicode, parse_equation};
