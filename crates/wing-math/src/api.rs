//! 窄接口：LaTeX → 单行字符串 / 字符网格。
//!
//! 三条公开入口（`render_inline` / `render_display` / `render_block`）共享同一条管线：
//!
//! ```text
//! normalize → scan → guard(输入侧) → latex 解析 + grid 排版
//!                                  └─ environments(多行环境适配)
//!           → guard(输出侧 + 预算) → 裁行 → 返回
//! ```
//!
//! ## `None` 的确切语义
//!
//! 所有 `None` 都表示**"这条公式不该由本引擎渲染"**，上层应降级为源码字面量。触发条件：
//!
//! | 条件 | 说明 |
//! |---|---|
//! | 归一化后为空 / 渲染结果只有空白 | 没有可渲染的东西（`\sqrt{}` / `\,` 也归这一类） |
//! | `&` / `\\` 出现在**没人消费**的地方 | 上游会在任意花括号深度静默截断（丢内容），任何深度都拒绝；只有环境体与 `\text{…}` 例外 |
//! | 某环境一行的列数**超过它的容量** | `cases` 每行只有 2 个槽位（value + condition），第 3 列起会被上游丢掉 |
//! | **解析器没有消费完输入** | 兜底判据：上游在 `&` / `\\` / `}` / `\right` / `\end` 处 `break`，剩下的就是被静默丢掉的内容（例如 `\\` 被当成 `\frac` 的第二个参数时） |
//! | 花括号不配平 | 上游会把组内/组外内容互相吃掉 |
//! | `\begin{X}` / `\end{X}` 不配对 | 同上 |
//! | `\left` / `\right` 后面不是**合法定界符记号** | 裸 `\` 会被上游当定界符画出来；字母 / CJK（`\right文`）也会被多画一次 |
//! | ≥ 2 个顶层多行环境 | 组合语义未定义 |
//! | 多行环境的单元格 / 前后缀渲染失败 | **整条降级**，绝不用空格占位（内容会丢） |
//! | `array` 的列格式串里有看不懂的内容（`p{2cm}` / `@{}` / 嵌套组） | 忽略等于静默丢内容 |
//! | `\left` / `\right` 计数不等 | 定界符必然错位 |
//! | 源码以孤立的反斜杠结尾（`x + \`） | 上游会把它原样渲染出来 |
//! | AST 里出现 `\`（文本节点 / 定界符） | 有命令没被渲染（`\ce` / `\dfrac` / 未支持环境…） |
//! | 结果里出现控制字符 | 会破坏"行内单行 / 显示按网格行"的契约 |
//! | 源码 > 8192 字符 / 花括号嵌套 > 47 层 / **解析递归深度 > 96** | 预算闸；后两条覆盖 brace-free 的深嵌套（`\left(\left(…`），见 [`MAX_PARSE_DEPTH`] |
//! | `\\` > 256 个 或 `&` > 256 个（环境体内） | 结果必然超预算，在排版**之前**拒绝 |
//! | `render_inline`：结果不是单行 | 多行块塞不进行内 |
//! | `render_display`：宽度 > `max_width` / 高度或面积超预算 | 装不下 |
//!
//! `Some` 的保证：**没有未渲染的命令被原样吐出来**（AST 层判定；注意 `\hat` 的几何
//! 字形本身含反斜杠，所以这条不能说成"输出不含 `\`"），且主体符号不丢。

use unicode_width::UnicodeWidthStr;

use crate::environments::{is_multiline, render_multiline_env};
use crate::grid::layout::layout;
use crate::grid::rendered_block::RenderedBlock;
use crate::guard::{self, Reject};
use crate::latex::parse_equation_with_depth;
use crate::normalize::normalize;
use crate::scan::scan;

/// 多行环境的最大嵌套深度（单元格里再套环境）。
const MAX_ENV_DEPTH: usize = 8;

/// 解析器递归深度上界（review r2 的 S1；r3 的 S1 按栈实测收紧到 96）。
///
/// 花括号深度拦不住 **brace-free** 的嵌套（`\left(\left(…`、`\hat \hat …`、
/// `\frac \frac …`、`\sqrt \sqrt …`），那些链既没有 `{` 也不产生任何结构信号，却会
/// 让 parser / layout 递归到爆栈。所以闸门放在解析器自己的递归计数上（它覆盖**所有**
/// 会递归的构造），并且**在排版之前**判定：`parse_equation_with_depth` 一旦置位就
/// 直接拒绝，AST 与排版都不跑。
///
/// 深度计数的口径是"每进入一层 `parse_atom` / `parse_sequence_until_ex` 加 1"，
/// 所以**一层结构通常占 2 个单位**。实测映射（本机 release）：
///
/// | 输入 | 解析深度 |
/// |---|---|
/// | 真实公式：二次方程 / `cases` / 矩阵套分式 / 高斯积分 | 8 / 4 / 7 / 5 |
/// | 嵌套分式 ×5 | 17 |
/// | 花括号 `{…}` ×47 / ×48 | **96 / 98** |
/// | `\left(` 链 ×46 / ×48 | 93 / 97 |
/// | `\frac ` 链 ×94 / ×96 | 96 / 98 |
/// | `\left(` ×200 / ×250（review r2 的 abort 点） | 401 / 501 |
/// | `\frac{…}{1}` 嵌套 ×195（同上） | 587 |
///
/// 取 **96**：真实公式（≤ 17）有 5 倍以上余量，同时把"闸门路径"的最坏栈需求从
/// 128 时的 ~929 KiB 压到 ~689 KiB（debug 实测，见下表）。
///
/// 栈实测（debug 构建，子进程二分最小可用线程栈；review r3 的 S1 要求把数字写死在这里）：
///
/// | 输入 | 最小可用线程栈（debug） |
/// |---|---|
/// | `\frac ` / `\sqrt ` 链 ×1364（闸门路径最坏） | **689 KiB** |
/// | `\hat ` / `\mathbb ` 链 ×1364 | 625 KiB |
/// | `\left(` ×250 + `\right)` ×250（平衡） | 401 KiB |
/// | 嵌套分式 ×40（**真的渲染**） | 353 KiB |
/// | 花括号 ×47（闸门通过但不渲染） | < 32 KiB |
///
/// 对应余量：我们自己的 1.5 MiB 回归测试（`budgets::max_depth_inputs_survive_a_small_stack`）
/// ≈ 2.2x、libtest 默认 2 MiB ≈ 3.0x、主线程 8 MiB ≈ 12x。
/// **契约边界**：1 MiB 线程栈也够用（1.49x），但 512 KiB 不行 —— 递归本身就有栈成本，
/// 我们不承诺任意小的栈。
const MAX_PARSE_DEPTH: usize = 96;

/// 显示公式的渲染结果：等宽字符网格。
///
/// - [`lines`](Self::lines) 已消除**公共前导缩进**、并去掉每行的行尾空白；
/// - [`width`](Self::width) 是这些行里最长的**显示宽度**（`unicode-width`，
///   所以 CJK 宽字符按 2 列记），可以直接用来判断"能不能放进容器"；
/// - [`baseline`](Self::baseline) 是基线行号（0-based），供上层做纵向对齐。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMath {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

impl RenderedMath {
    /// 网格行（不含行尾空白）。
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// 最长行的显示宽度（列）。
    pub fn width(&self) -> usize {
        self.width
    }

    /// 行数。
    pub fn height(&self) -> usize {
        self.lines.len()
    }

    /// 基线行号（0-based from top）。
    pub fn baseline(&self) -> usize {
        self.baseline
    }

    /// 取走行。
    pub fn into_lines(self) -> Vec<String> {
        self.lines
    }

    /// 用 `'\n'` 连接所有行（测试 / 调试用）。
    pub fn to_plain_text(&self) -> String {
        self.lines.join("\n")
    }
}

/// 行内公式：结果必须落在**单行**，否则返回 [`Option::None`]。
///
/// 返回值已去掉首尾空白（行内嵌进正文里，两端的空白由排版层决定）。
///
/// ```
/// assert_eq!(wing_math::render_inline(r"x^2 + y^2").as_deref(), Some("x² + y²"));
/// assert_eq!(wing_math::render_inline(r"\frac{a}{b}"), None);  // 分式是多行的
/// assert_eq!(wing_math::render_inline(r"\ce{2H2O}"), None);    // 未支持命令
/// ```
pub fn render_inline(src: &str) -> Option<String> {
    render_inline_checked(src).ok()
}

/// [`render_inline`] 的内部实现：保留拒绝理由，便于单测钉住判据边界。
fn render_inline_checked(src: &str) -> Result<String, Reject> {
    let block = render_block_checked(src)?;
    if block.height() != 1 {
        return Err(Reject::NotSingleLine);
    }
    let line = block
        .cells()
        .first()
        .map(|row| row.concat())
        .ok_or(Reject::Empty)?;
    let line = line.trim().to_string();
    if line.is_empty() {
        return Err(Reject::Empty);
    }
    Ok(line)
}

/// 显示公式：返回字符网格；超宽或疑似截断返回 [`Option::None`]。
///
/// `max_width` 是容器可用列数。结果**左对齐**、不做居中：居中属于排版层策略，上层用
/// [`RenderedMath::width`] 自己算留白即可。
///
/// ```
/// let m = wing_math::render_display(r"\sum_{i=1}^{n} i", 40).unwrap();
/// assert!(m.lines().len() >= 3);
/// assert!(m.width() <= 40);
/// assert_eq!(wing_math::render_display(r"\sum_{i=1}^{n} i", 2), None);
/// ```
pub fn render_display(src: &str, max_width: usize) -> Option<RenderedMath> {
    render_display_checked(src, max_width).ok()
}

/// [`render_display`] 的内部实现：保留拒绝理由，便于单测钉住判据边界。
fn render_display_checked(src: &str, max_width: usize) -> Result<RenderedMath, Reject> {
    let block = render_block_checked(src)?;
    let lines = block_lines(&block);
    if lines.is_empty() {
        return Err(Reject::Empty);
    }
    let width = lines
        .iter()
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0);
    if width > max_width {
        return Err(Reject::TooWide);
    }
    Ok(RenderedMath {
        lines,
        width,
        baseline: block.baseline(),
    })
}

/// 原始排版结果（[`RenderedBlock`]，上游网格类型）。
///
/// 已经过归一化、自检与预算闸；上层若要自己拼装（居中、拼表格、复用基线），用这个。
/// 与另外两个入口共享全部降级语义。
pub fn render_block(src: &str) -> Option<RenderedBlock> {
    render_block_checked(src).ok()
}

/// [`render_block`] 的内部实现：保留拒绝理由，便于单测钉住判据边界。
fn render_block_checked(src: &str) -> Result<RenderedBlock, Reject> {
    render_block_at(src, 0)
}

/// 多行环境单元格会递归走同一条管线，`depth` 用来给嵌套设上界。
///
/// 供 `environments` 渲染单元格 / 前后缀时调用：单元格内容与顶层公式享受**完全相同**的
/// 归一化、自检与降级语义（所以 `\begin{align}` 里嵌 `\begin{aligned}` 也能被处理，
/// 而内层出现未支持命令时同样会把理由上抛、让整条公式降级为字面量）。
pub(crate) fn render_block_at(src: &str, depth: usize) -> Result<RenderedBlock, Reject> {
    if depth > MAX_ENV_DEPTH {
        return Err(Reject::TooDeep);
    }

    let normalized = normalize(src);
    let chars: Vec<char> = normalized.chars().collect();
    if chars.is_empty() {
        return Err(Reject::Empty);
    }

    let structure = scan(&chars);
    guard::check_source(&chars, &structure)?;

    let block = match structure.envs.first() {
        Some(span) if is_multiline(&span.name) => render_multiline_env(&chars, span, depth)?,
        _ => {
            let (ast, too_deep, consumed) = parse_equation_with_depth(&normalized, MAX_PARSE_DEPTH);
            if too_deep {
                // 越界的 AST 是残缺的：既不能渲染，也不能拿它做泄漏判断
                return Err(Reject::TooDeep);
            }
            if consumed < chars.len() {
                // 解析器提前收工 = 剩下的输入会被静默丢掉（上游在 `&` / `\\` / `}` /
                // `\right` / `\end` 处 `break`）。这是"内容丢失"的最后一道兜底判据：
                // 前面的结构自检负责给出更精确的理由，这里保证**没有任何形态能漏网**
                // （review r3 的 B1：`\frac` 把 `\\` 吃成参数时，行/列计数会与实际消费
                // 不一致，只有"消费长度"能发现）。
                return Err(Reject::UnconsumedInput);
            }
            guard::check_ast(&ast)?;
            layout(&ast)
        }
    };
    let block = clamp_baseline(block);

    guard::check_output(&block)?;
    Ok(block)
}

/// 上游少数路径会给出**越界基线**（实测：`\sqrt{}` 空根号体时 `baseline == height`，
/// 因为 `layout_sqrt` 的多行分支无条件写 `1 + body.baseline()`）。基线越界对下游
/// （纵向对齐、`baseline()` 断言）是脏数据，这里统一夹到合法区间，不改内容。
fn clamp_baseline(block: RenderedBlock) -> RenderedBlock {
    if block.height() == 0 || block.baseline() < block.height() {
        return block;
    }
    RenderedBlock::new(block.cells().to_vec(), block.height() - 1)
}

/// 网格 → 行：去掉行尾空白，再消除所有非空行的**公共前导缩进**。
///
/// 消除公共缩进是为了让上层拿到"贴着左边界"的结果（终端里每省一列都是赚的），同时不改
/// 变网格内部的相对对齐。行数不变，所以 [`RenderedBlock::baseline`] 仍然有效。
fn block_lines(block: &RenderedBlock) -> Vec<String> {
    let mut lines: Vec<String> = block
        .cells()
        .iter()
        .map(|row| row.concat().trim_end().to_string())
        .collect();
    let indent = lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ').count())
        .min()
        .unwrap_or(0);
    if indent > 0 {
        for line in &mut lines {
            *line = line.chars().skip(indent).collect();
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_renders_single_row() {
        assert_eq!(render_inline(r"x^2 + y^2").as_deref(), Some("x² + y²"));
        assert_eq!(render_inline(r"\alpha + \beta").as_deref(), Some("α + β"));
        assert_eq!(render_inline(r"\mathbb{R}^n").as_deref(), Some("ℝⁿ"));
    }

    #[test]
    fn inline_trims_and_rejects_multiline() {
        assert_eq!(render_inline(r"   x   ").as_deref(), Some("x"));
        assert_eq!(render_inline(r"\frac{a}{b}"), None);
        assert_eq!(render_inline(r"\sum_{i=1}^{n}"), None);
    }

    #[test]
    fn inline_rejects_empty_and_unsupported() {
        assert_eq!(render_inline(""), None);
        assert_eq!(render_inline("   "), None);
        assert_eq!(render_inline(r"\ce{2H2O}"), None);
        assert_eq!(render_inline(r"\unknowncmd{1}"), None);
    }

    #[test]
    fn display_reports_width_and_lines() {
        let m = render_display(r"\frac{a}{b}", 40).unwrap();
        assert_eq!(m.lines(), [" a", "───", " b"]);
        assert_eq!(m.width(), 3);
        assert_eq!(m.height(), 3);
        assert_eq!(m.to_plain_text(), " a\n───\n b");
    }

    #[test]
    fn display_enforces_max_width() {
        let m = render_display(r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}", 80).unwrap();
        let w = m.width();
        assert!(w > 3);
        assert!(render_display(r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}", w).is_some());
        assert!(render_display(r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}", w - 1).is_none());
    }

    #[test]
    fn display_strips_common_indent() {
        // 每一行都有同样的前导缩进时会被消除
        let m = render_display(r"\begin{gather} a \\ b \end{gather}", 40).unwrap();
        assert!(
            m.lines().iter().all(|l| !l.starts_with(' ')),
            "{:?}",
            m.lines()
        );
    }

    #[test]
    fn block_is_shared_engine_output() {
        let b = render_block(r"\frac{a}{b}").unwrap();
        assert_eq!(b.width(), 3);
        assert_eq!(b.height(), 3);
    }
}
