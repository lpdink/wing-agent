//! # wing-math
//!
//! LaTeX 数学子集 → 终端字符网格（terminal character grid）。
//!
//! 这个 crate 是**借鉴内联**（vendored）上游两个 crate 的产物，外加一层我们自己的
//! 适配与降级保护：
//!
//! | 模块 | 来源 | 许可 |
//! |---|---|---|
//! | [`latex`] | [rust-latex-parser](https://github.com/William-Selna/Rust-LaTeX-Parser) 0.1.0 | MIT，Copyright (c) 2026 William-Selna |
//! | [`grid`] | crates.io `term-maths` 1.0.0 | MIT OR Apache-2.0，Copyright (c) 2026 Jack Geraghty |
//! | `api` / `guard` / `normalize` / `environments` / `scan`（私有） | 本项目自研 | Apache-2.0（同工作区） |
//!
//! 逐文件的出处与「我们改了什么」写在每个内联文件的顶部注释；许可原文与登记表见
//! crate 根目录的 `NOTICE` / `LICENSE-MIT` / `LICENSE-APACHE` /
//! `LICENSE-MIT-rust-latex-parser`。
//!
//! ## 为什么不是"直接用上游"
//!
//! 上游能渲染 `\frac` / `\sqrt` / 上下标 / 积分 / 求和 / 矩阵 / `cases` 等，质量不错；
//! 但它在两类输入上会**静默吞内容**（顶层 `\\` / `&` 直接截断；`\begin{align}` 家族
//! 只吐半行），在另一类输入上会把没渲染的 LaTeX 命令原样吐给用户（`\ce` / `\dfrac` /
//! `\left\{` …）。对本项目而言"公式被静默吞掉"是 B 级缺陷，所以引擎的形态是：
//!
//! ```text
//! src ──normalize──▶ guard(输入侧结构自检) ──▶ latex 解析 + grid 排版
//!                                             └─▶ environments(多行环境适配)
//!        ──guard(AST 泄漏自检 + 网格自检 + 预算)──▶ RenderedMath
//! ```
//!
//! 任何一步拿不准就返回 [`Option::None`]，由上层降级为源码字面量 —— **宁可显示原始
//! LaTeX，也不显示半截公式**。这条在**单元格层**同样成立：多行环境里任何一格或前后缀
//! 渲染失败，整条公式都会降级，不会被替换成空格。
//!
//! ## 窄接口
//!
//! ```
//! // 行内：必须落在单行，否则 None
//! assert_eq!(wing_math::render_inline(r"x^2 + y^2").as_deref(), Some("x² + y²"));
//! // 行内的分式是多行的，装不进一行 → None（上层降级为字面量）
//! assert_eq!(wing_math::render_inline(r"\frac{a}{b}"), None);
//!
//! // 显示：字符网格
//! let m = wing_math::render_display(r"\frac{a}{b}", 40).unwrap();
//! assert_eq!(m.lines(), [" a", "───", " b"]);
//! assert_eq!(m.width(), 3);
//! ```

mod compose;
mod environments;
mod guard;
mod normalize;
mod scan;

pub mod api;
pub mod grid;
pub mod latex;

pub use api::{RenderedMath, render_block, render_display, render_inline};
pub use grid::rendered_block::RenderedBlock;
