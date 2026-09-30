//! 自检与降级判据（本项目自研，非上游代码）。
//!
//! 上游引擎有三条会让结果"看起来正常但实际错了"的路径，本模块把它们变成**可判定**的
//! 拒绝理由：
//!
//! 1. **静默截断（输入侧）**：顶层 `\\` / `&` 会让上游 parser 直接 `break`，输出里连
//!    一个 `\` 都不会留下（实测 `a \\ b` → `a `）。这类问题只能在输入侧发现，所以有
//!    [`check_source`]；它建立在 [`crate::scan::scan`] 的结构信息上（顺带把行/列分隔符
//!    数量、花括号深度、孤立反斜杠这些"必然超预算 / 必然出错"的信号前移到解析之前）。
//! 2. **命令泄漏（AST 侧）**：上游渲染器渲染未知命令时会兜底成 `\name` 文本节点
//!    （含未支持环境的 `Text("\begin{env}")`）。判据是 AST 里出现 `\`，见 [`check_ast`]。
//!    **不能**改成"输出文本里有 `\`"——`\hat` 的几何字形（`/\`、`/‾‾\`）自带反斜杠，
//!    那样会把正常渲染误判成泄漏。
//! 3. **网格被污染**：结果里出现控制字符（`\n` / `\t`）或整块只有空白，
//!    见 [`check_output`]。
//!
//! 另外这里集中放**预算闸**常量：上游排版是纯 CPU + 线性分配，正常公式毫秒级，但仍要
//! 给"恶意/畸形输入"设上界，避免一次渲染吃掉整屏内存或爆栈。

use crate::grid::rendered_block::RenderedBlock;
use crate::scan::Structure;

/// 源码字符数上限。单条公式的合理上界；上游实测 10000 项输入也不慢，但要有界。
pub(crate) const MAX_SOURCE_CHARS: usize = 8192;

/// 花括号嵌套深度上限。上游是递归下降 parser + 递归排版，深度直接等于调用栈深度。
///
/// **实测依据**（review r1 的 S2）：debug 构建 + 2 MiB 线程栈（`cargo test` 测试线程 /
/// `std::thread` 默认）下，嵌套 `\frac{…}{1}` 到 ~195 层就会 stack overflow（不可捕获的
/// abort）。取 64 留 3 倍余量；真实公式的嵌套深度远小于此（连分式/嵌套矩阵通常 < 20）。
pub(crate) const MAX_BRACE_DEPTH: usize = 64;

/// 渲染结果的高度上限（行）。
pub(crate) const MAX_HEIGHT: usize = 256;

/// 行分隔符（`\\`）数量上限。每个顶层 `\\` 至少产生一行，超过它结果必然超 [`MAX_HEIGHT`]，
/// 所以在源码扫描阶段就能拒绝，不必先排版（S2：把预算闸前移）。
pub(crate) const MAX_ROW_SEPARATORS: usize = MAX_HEIGHT;

/// 列分隔符（`&`）数量上限。同上，超过它结果必然超宽。
pub(crate) const MAX_COL_SEPARATORS: usize = 256;

/// 渲染结果的单元格总数上限（宽 × 高），防内存爆炸。
pub(crate) const MAX_CELLS: usize = 262_144;

/// 拒绝理由。所有 `None` 都能对应到这里的一条，便于单测钉住判据边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reject {
    /// 归一化后没有可渲染内容（含"渲染出来只有空白"）。
    Empty,
    /// 源码超长。
    TooLong,
    /// 结构嵌套过深（花括号深度 / 解析器递归深度）。
    TooDeep,
    /// 出现没人消费的 `&`（不在环境体 / `\text{}` 里），任何花括号深度都算。
    UnmanagedColumnSeparator,
    /// 出现没人消费的 `\\`（同上）。
    UnmanagedRowSeparator,
    /// 花括号不配平。
    UnbalancedBraces,
    /// `\begin{X}` / `\end{X}` 不配对或名字不一致。
    UnbalancedEnvironment,
    /// 出现两个及以上顶层环境（组合语义未定义）。
    MultipleEnvironments,
    /// 环境适配层处理不了（空 body / 参数畸形 / 前后缀里还剩环境或定界符）。
    UnsupportedEnvironment,
    /// `\left` / `\right` 计数不等。
    UnbalancedDelimiter,
    /// `\left` / `\right` 后面不是合法定界符（裸反斜杠会被上游当定界符画出来）。
    UnsupportedDelimiter,
    /// 源码以孤立的反斜杠结尾（上游会把裸 `\` 渲染出来）。
    IncompleteInput,
    /// 有命令没被渲染、原样漏出来（AST 里出现 `\` 文本 / 定界符）。
    LeakedCommand,
    /// 渲染结果里出现控制字符（`\n` / `\t` …）——会破坏"单行 / 网格行"契约。
    ControlCharacter,
    /// 结果宽度超过调用方给的 `max_width`。
    TooWide,
    /// 结果高度（或源码里的行分隔符数量）超预算。
    TooTall,
    /// 结果单元格总数超预算，或源码里的列分隔符数量超预算。
    TooManyCells,
    /// 行内渲染结果不是单行。
    NotSingleLine,
}

/// 输入侧结构自检：在**归一化之后、解析之前**调用。
///
/// 检查顺序有意为之：先报"会静默丢内容"的（红线），再报预算，最后报形状。
pub(crate) fn check_source(chars: &[char], st: &Structure) -> Result<(), Reject> {
    // ── 红线：会静默丢内容的形状 ──
    if st.unmanaged_column_sep {
        return Err(Reject::UnmanagedColumnSeparator);
    }
    if st.unmanaged_row_sep {
        return Err(Reject::UnmanagedRowSeparator);
    }
    if !st.balanced_braces {
        return Err(Reject::UnbalancedBraces);
    }
    if !st.balanced_envs {
        return Err(Reject::UnbalancedEnvironment);
    }
    if st.unmanaged_delimiter {
        return Err(Reject::UnsupportedDelimiter);
    }
    if st.dangling_backslash {
        return Err(Reject::IncompleteInput);
    }
    if st.left_count != st.right_count {
        return Err(Reject::UnbalancedDelimiter);
    }
    if st.envs.len() > 1 {
        return Err(Reject::MultipleEnvironments);
    }

    // ── 预算闸（同样在解析与排版之前） ──
    if chars.len() > MAX_SOURCE_CHARS {
        return Err(Reject::TooLong);
    }
    if st.max_brace_depth > MAX_BRACE_DEPTH {
        return Err(Reject::TooDeep);
    }
    if st.row_separator_count > MAX_ROW_SEPARATORS {
        return Err(Reject::TooTall);
    }
    if st.column_separator_count > MAX_COL_SEPARATORS {
        return Err(Reject::TooManyCells);
    }
    Ok(())
}

/// 泄漏自检：**在 AST 上**判断有没有命令没被渲染。
///
/// 上游渲染器本身会产出反斜杠字形的地方有两处：
/// 1. `\hat` 的 `/\`、`/‾‾\` 几何（见 `grid/layout.rs::layout_accent`）；
/// 2. `\left` / `\right` 的定界符字符（`Delimited { left, right }`）。
///
/// 所以"输出文本里有 `\`"并不等于泄漏（review r1 的 S1：`\hat{ab}` 曾被自己的字形误伤）。
/// 真正的泄漏只在 AST 层可判定，且必须**穷尽每个节点类型与每个字段**：
///
/// - `Text` / `TextBlock` 里的 `\`：上游 `parse_command` 对未知命令的兜底
///   `format!("\\{}", name)`（含未支持环境的 `Text("\begin{env}")`）与用户写在
///   `\text{…}` 里的反斜杠；
/// - `Delimited.left` / `Delimited.right` 里的 `\`：`\left` 后面跟了非法定界符时，
///   上游把裸 `\` 当成定界符（review r2 的 B2）。
///
/// 这里的 `match` **刻意不写 `_` 兜底分支**：上游 AST 一旦增删变体，编译就会失败，
/// 逼着人回来补判据（防止再出现"漏了一个节点类型"的回归）。
pub(crate) fn check_ast(node: &crate::latex::EqNode) -> Result<(), Reject> {
    use crate::latex::EqNode;
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n {
            EqNode::Text(s) | EqNode::TextBlock(s) => {
                if s.contains('\\') {
                    return Err(Reject::LeakedCommand);
                }
            }
            EqNode::Delimited {
                left,
                right,
                content,
            } => {
                if left.contains('\\') || right.contains('\\') {
                    return Err(Reject::LeakedCommand);
                }
                stack.push(content);
            }
            EqNode::Seq(nodes) => stack.extend(nodes.iter()),
            EqNode::Sup(a, b) | EqNode::Sub(a, b) | EqNode::Frac(a, b) | EqNode::Binom(a, b) => {
                stack.push(a);
                stack.push(b);
            }
            EqNode::SupSub(a, b, c) => {
                stack.push(a);
                stack.push(b);
                stack.push(c);
            }
            EqNode::Sqrt(a) | EqNode::Accent(a, _) => stack.push(a),
            EqNode::BigOp { lower, upper, .. } => {
                if let Some(l) = lower {
                    stack.push(l);
                }
                if let Some(u) = upper {
                    stack.push(u);
                }
            }
            EqNode::Limit { lower, .. } => {
                if let Some(l) = lower {
                    stack.push(l);
                }
            }
            EqNode::MathFont { content, .. } => stack.push(content),
            EqNode::Matrix { rows, .. } => stack.extend(rows.iter().flatten()),
            EqNode::Cases { rows } => {
                for (v, c) in rows {
                    stack.push(v);
                    if let Some(c) = c {
                        stack.push(c);
                    }
                }
            }
            EqNode::Brace { content, label, .. } => {
                stack.push(content);
                if let Some(l) = label {
                    stack.push(l);
                }
            }
            EqNode::StackRel {
                base, annotation, ..
            } => {
                stack.push(base);
                stack.push(annotation);
            }
            EqNode::Space(_) => {}
        }
    }
    Ok(())
}

/// 输出侧自检 + 结果预算闸。
pub(crate) fn check_output(block: &RenderedBlock) -> Result<(), Reject> {
    if block.height() == 0 || block.width() == 0 {
        return Err(Reject::Empty);
    }
    if block.height() > MAX_HEIGHT {
        return Err(Reject::TooTall);
    }
    if block.width().saturating_mul(block.height()) > MAX_CELLS {
        return Err(Reject::TooManyCells);
    }
    let mut has_visible = false;
    for row in block.cells() {
        for cell in row {
            for ch in cell.chars() {
                // 控制字符会破坏"单行 / 每行一个网格行"的契约（review r1 的 B3）
                if ch.is_control() {
                    return Err(Reject::ControlCharacter);
                }
                if ch != ' ' {
                    has_visible = true;
                }
            }
        }
    }
    if !has_visible {
        // 全空白结果（`\sqrt{}` / `\,`）：渲染出来等于没有，交给上层显示源码
        return Err(Reject::Empty);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::latex::EqNode;
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
        assert_eq!(verdict(r"a \\ b"), Err(Reject::UnmanagedRowSeparator));
        assert_eq!(verdict(r"a & b"), Err(Reject::UnmanagedColumnSeparator));
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
        // 只有左括号：先撞上"花括号不配平"（会丢内容，优先报）
        let unbalanced = "{".repeat(MAX_BRACE_DEPTH + 1);
        assert_eq!(verdict(&unbalanced), Err(Reject::UnbalancedBraces));

        // 配平但更深：撞深度闸
        let deep = format!(
            "{}x{}",
            "{".repeat(MAX_BRACE_DEPTH + 1),
            "}".repeat(MAX_BRACE_DEPTH + 1)
        );
        assert_eq!(verdict(&deep), Err(Reject::TooDeep));

        // 上限本身可以通过
        let ok = format!(
            "{}x{}",
            "{".repeat(MAX_BRACE_DEPTH),
            "}".repeat(MAX_BRACE_DEPTH)
        );
        assert_eq!(verdict(&ok), Ok(()));
    }

    #[test]
    fn rejects_too_many_row_or_column_separators() {
        let rows = format!(
            r"\begin{{array}}{{c}} {} \end{{array}}",
            vec!["1"; MAX_ROW_SEPARATORS + 2].join(r" \\ ")
        );
        assert_eq!(verdict(&rows), Err(Reject::TooTall));

        let cols = format!(
            r"\begin{{array}}{{c}} {} \end{{array}}",
            vec!["1"; MAX_COL_SEPARATORS + 2].join(" & ")
        );
        assert_eq!(verdict(&cols), Err(Reject::TooManyCells));
    }

    #[test]
    fn rejects_dangling_backslash() {
        assert_eq!(verdict(r"x + \"), Err(Reject::IncompleteInput));
        // `\ `（反斜杠 + 空格）是合法的空格命令，不算孤立反斜杠
        assert_eq!(verdict(r"x + \ "), Ok(()));
    }

    #[test]
    fn ast_leak_check_is_precise() {
        use crate::latex::parse_equation;
        // 未知命令会被兜底成 `\name` 文本节点
        assert_eq!(
            check_ast(&parse_equation(r"\ce{2H2O}")),
            Err(Reject::LeakedCommand)
        );
        assert_eq!(
            check_ast(&parse_equation(r"\begin{unknown} x \end{unknown}")),
            Err(Reject::LeakedCommand)
        );
        assert_eq!(
            check_ast(&parse_equation(r"\unknowncmd{1}")),
            Err(Reject::LeakedCommand)
        );
        // 正常公式没有反斜杠
        assert_eq!(check_ast(&parse_equation(r"\frac{a}{b}")), Ok(()));
        assert_eq!(check_ast(&parse_equation(r"\hat{ab}")), Ok(()));
        assert_eq!(check_ast(&parse_equation(r"\{x\}")), Ok(()));
        // `\text{...}` 里的反斜杠同样属于"没被渲染"
        assert_eq!(
            check_ast(&parse_equation(r"\text{\alpha}")),
            Err(Reject::LeakedCommand)
        );

        // review r2 的 B2：`\left` / `\right` 的定界符字符串也是"渲染器会产出的 `\`"
        // 之一，必须单独检查（只进 content 不看 left/right 就是漏网）。
        let delimited = parse_equation(r"\left\unknowncmd y \right)");
        assert_eq!(check_ast(&delimited), Err(Reject::LeakedCommand));
        // 逐字段确认：只看 left / 只看 right 都能命中
        assert_eq!(
            check_ast(&EqNode::Delimited {
                left: "\\".to_string(),
                right: ")".to_string(),
                content: Box::new(EqNode::Text("x".to_string())),
            }),
            Err(Reject::LeakedCommand)
        );
        assert_eq!(
            check_ast(&EqNode::Delimited {
                left: "(".to_string(),
                right: "\\".to_string(),
                content: Box::new(EqNode::Text("x".to_string())),
            }),
            Err(Reject::LeakedCommand)
        );
        // 合法定界符（含 `\hat` 的几何字形路径）不受影响
        assert_eq!(
            check_ast(&EqNode::Delimited {
                left: "(".to_string(),
                right: ")".to_string(),
                content: Box::new(EqNode::Text("x".to_string())),
            }),
            Ok(())
        );
        // 嵌套深处也要能查到（遍历是栈式的，不是只看第一层）
        assert_eq!(
            check_ast(&parse_equation(
                r"\frac{1}{\sqrt{\left\unknowncmd y \right)}}"
            )),
            Err(Reject::LeakedCommand)
        );
    }

    #[test]
    fn rejects_blank_and_control_character_output() {
        // 全空白结果（`\sqrt{}` / `\,`）等于没渲染
        assert_eq!(
            check_output(&crate::grid::render(r"\sqrt{}")),
            Err(Reject::Empty)
        );
        assert_eq!(
            check_output(&crate::grid::render(r"\,")),
            Err(Reject::Empty)
        );

        let with_newline = RenderedBlock::new(vec![vec!["a\nb".to_string()]], 0);
        assert_eq!(check_output(&with_newline), Err(Reject::ControlCharacter));
        let with_tab = RenderedBlock::new(vec![vec!["a".to_string(), "\t".to_string()]], 0);
        assert_eq!(check_output(&with_tab), Err(Reject::ControlCharacter));
    }

    #[test]
    fn output_check_does_not_mistake_hat_geometry_for_a_leak() {
        // `\hat{ab}` 的渲染结果自带 `\` 字形（`/\`），它必须能通过输出侧自检
        let block = crate::grid::render(r"\hat{ab}");
        assert!(
            format!("{block}").contains('\\'),
            "geometry should contain a backslash"
        );
        assert_eq!(check_output(&block), Ok(()));
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
