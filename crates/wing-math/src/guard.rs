//! 自检与降级判据（本项目自研，非上游代码）。
//!
//! 上游引擎有两条会让结果"看起来正常但实际错了"的路径，本模块把它们变成**可判定**的
//! 拒绝理由：
//!
//! 1. **静默截断（输入侧）**：顶层 `\\` / `&` 会让上游 parser 直接 `break`，输出里连
//!    一个 `\` 都不会留下（实测 `a \\ b` → `a `）。这类问题只能在输入侧发现，所以有
//!    [`check_source`]；它建立在 [`crate::scan::scan`] 的结构信息上。
//! 2. **命令泄漏（输出侧）**：上游渲染器本身**从不产生 `\`**，正文里的 `\` 只可能来自
//!    `parse_command` 的未知命令兜底 `format!("\\{}", name)`（含未支持环境的
//!    `Text("\begin{env}")`）。所以"输出里有 `\`" ⟺ "有东西没被渲染、原样漏出来了"，
//!    一条判据覆盖 `\ce` / `\dfrac` / `\colon` / `\begin{align}` 残留等全部情况，
//!    见 [`check_output`]。
//!
//! 另外这里集中放**预算闸**常量：上游排版是纯 CPU + 线性分配，正常公式毫秒级，但仍要
//! 给"恶意/畸形输入"设上界，避免一次渲染吃掉整屏内存。

use crate::grid::rendered_block::RenderedBlock;
use crate::scan::Structure;

/// 源码字符数上限。单条公式的合理上界；上游实测 10000 项输入也不慢，但要有界。
pub(crate) const MAX_SOURCE_CHARS: usize = 8192;

/// 花括号嵌套深度上限。上游是递归下降 parser，深度直接等于调用栈深度。
/// 上游自带 200 层测试（实测无栈溢出），这里留一倍余量。
pub(crate) const MAX_BRACE_DEPTH: usize = 256;

/// 渲染结果的高度上限（行）。
pub(crate) const MAX_HEIGHT: usize = 256;

/// 渲染结果的单元格总数上限（宽 × 高），防内存爆炸。
pub(crate) const MAX_CELLS: usize = 262_144;

/// 拒绝理由。所有 `None` 都能对应到这里的一条，便于单测钉住判据边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reject {
    /// 归一化后没有可渲染内容。
    Empty,
    /// 源码超长。
    TooLong,
    /// 花括号嵌套过深。
    TooDeep,
    /// 顶层 `&`（`scan::Structure::top_level_column_sep`）。
    TopLevelColumnSep,
    /// 顶层 `\\`（`scan::Structure::top_level_row_sep`）。
    TopLevelRowSep,
    /// `\begin{X}` / `\end{X}` 不配对或名字不一致。
    UnbalancedEnvironment,
    /// 出现两个及以上顶层环境（组合语义未定义）。
    MultipleEnvironments,
    /// `\left` / `\right` 计数不等。
    UnbalancedDelimiter,
    /// 渲染输出里出现 `\`：有命令没被渲染、原样漏出来了。
    LeakedCommand,
    /// 结果宽度超过调用方给的 `max_width`。
    TooWide,
    /// 结果高度超预算。
    TooTall,
    /// 结果单元格总数超预算。
    TooManyCells,
    /// 行内渲染结果不是单行。
    NotSingleLine,
}

/// 输入侧结构自检：在**归一化之后、解析之前**调用。
pub(crate) fn check_source(chars: &[char], st: &Structure) -> Result<(), Reject> {
    if chars.len() > MAX_SOURCE_CHARS {
        return Err(Reject::TooLong);
    }
    if st.max_brace_depth > MAX_BRACE_DEPTH {
        return Err(Reject::TooDeep);
    }
    if st.top_level_column_sep {
        return Err(Reject::TopLevelColumnSep);
    }
    if st.top_level_row_sep {
        return Err(Reject::TopLevelRowSep);
    }
    if !st.balanced_envs {
        return Err(Reject::UnbalancedEnvironment);
    }
    if st.envs.len() > 1 {
        return Err(Reject::MultipleEnvironments);
    }
    if st.left_count != st.right_count {
        return Err(Reject::UnbalancedDelimiter);
    }
    Ok(())
}

/// 输出侧泄漏自检 + 结果预算闸。
pub(crate) fn check_output(block: &RenderedBlock) -> Result<(), Reject> {
    if block.cells().iter().flatten().any(|c| c.contains('\\')) {
        return Err(Reject::LeakedCommand);
    }
    if block.height() == 0 || block.width() == 0 {
        return Err(Reject::Empty);
    }
    if block.height() > MAX_HEIGHT {
        return Err(Reject::TooTall);
    }
    if block.width().saturating_mul(block.height()) > MAX_CELLS {
        return Err(Reject::TooManyCells);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;

    fn src(s: &str) -> (Vec<char>, Structure) {
        let chars: Vec<char> = s.chars().collect();
        let st = scan(&chars);
        (chars, st)
    }

    fn verdict(s: &str) -> Result<(), Reject> {
        let (chars, st) = src(s);
        check_source(&chars, &st)
    }

    #[test]
    fn accepts_ordinary_formulas() {
        for s in [
            r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}",
            r"\int_0^\infty e^{-x^2} dx",
            r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
            r"\begin{cases} x^2 & x>0 \\ 0 & x\le 0 \end{cases}",
            r"f(x) = \left(1 + \frac{1}{n}\right)^n",
        ] {
            assert_eq!(verdict(s), Ok(()), "should accept: {s}");
        }
    }

    #[test]
    fn rejects_top_level_separators() {
        assert_eq!(verdict(r"a \\ b"), Err(Reject::TopLevelRowSep));
        assert_eq!(verdict(r"a & b"), Err(Reject::TopLevelColumnSep));
    }

    #[test]
    fn rejects_unbalanced_environment() {
        assert_eq!(
            verdict(r"\begin{align} a &= b"),
            Err(Reject::UnbalancedEnvironment)
        );
        assert_eq!(verdict(r"\end{matrix}"), Err(Reject::UnbalancedEnvironment));
    }

    #[test]
    fn rejects_multiple_top_level_envs() {
        assert_eq!(
            verdict(r"\begin{cases} a \end{cases} \begin{cases} b \end{cases}"),
            Err(Reject::MultipleEnvironments)
        );
    }

    #[test]
    fn rejects_unbalanced_delimiters() {
        assert_eq!(verdict(r"\left( x + y"), Err(Reject::UnbalancedDelimiter));
        assert_eq!(verdict(r"x + y \right)"), Err(Reject::UnbalancedDelimiter));
    }

    #[test]
    fn rejects_oversized_source() {
        let long = "x".repeat(MAX_SOURCE_CHARS + 1);
        assert_eq!(verdict(&long), Err(Reject::TooLong));
    }

    #[test]
    fn rejects_deep_nesting() {
        let deep = "{".repeat(MAX_BRACE_DEPTH + 1);
        assert_eq!(verdict(&deep), Err(Reject::TooDeep));
    }

    #[test]
    fn detects_leaked_commands_in_output() {
        // 未知命令会被上游兜底成 `\name` 字面量
        assert_eq!(
            check_output(&crate::grid::render(r"\ce{2H2O}")),
            Err(Reject::LeakedCommand)
        );
        // 正常公式不会有反斜杠
        assert_eq!(check_output(&crate::grid::render(r"\frac{a}{b}")), Ok(()));
    }

    #[test]
    fn rejects_empty_and_oversized_blocks() {
        assert_eq!(check_output(&RenderedBlock::empty()), Err(Reject::Empty));

        let tall = RenderedBlock::new(vec![vec!["x".to_string()]; MAX_HEIGHT + 1], 0);
        assert_eq!(check_output(&tall), Err(Reject::TooTall));

        // 高度不超（正好等于上限），靠面积闸挡住
        let wide = RenderedBlock::new(vec![vec!["x".to_string(); 1025]; MAX_HEIGHT], 0);
        assert!(wide.width() * wide.height() > MAX_CELLS);
        assert_eq!(check_output(&wide), Err(Reject::TooManyCells));
    }
}
