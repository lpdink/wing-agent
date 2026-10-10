//! 多行环境适配（本项目自研，非上游代码）。
//!
//! ## 为什么必须在进上游之前自己处理
//!
//! 上游 `parse_begin_env` 只认 `matrix` 家族与 `cases`；遇到 `align` 等环境会返回
//! `Text("\begin{env}")` 且**不消费 body**，随后 body 被当普通序列解析、撞上 `&` 直接
//! `break` —— 实测 `\begin{align} a &= b + c \\ d &= e \end{align}` 输出 `\begin{align} a `
//! （只剩半行，**内容被静默吞掉**）。所以这些环境必须在进入上游 parser 之前被摘出来，
//! 由我们自己按 `\\` 分行、按 `&` 分列、按列对齐后堆叠。
//!
//! ## 支持的环境与对齐规则
//!
//! | 环境 | 列对齐 |
//! |---|---|
//! | `align` `align*` `aligned` `alignat` `alignat*` `alignedat` `flalign` `flalign*` `split` `eqnarray` `eqnarray*` | 偶数列右对齐、奇数列左对齐（LaTeX `align` 语义） |
//! | `gather` `gather*` `multline` `multline*` `center` `equation` `equation*` `displaymath` | 每行居中 |
//! | `array` | 按前导列格式串 `{lcr}` 逐列指定，缺省居中 |
//!
//! 其余环境（含 `cases` / `matrix` 家族）不在这里处理：它们上游能正确渲染。
//!
//! ## 形态
//!
//! - **裸环境**：`[前缀] \begin{ENV}…\end{ENV} [后缀]` —— 前缀/后缀各自排版后与网格
//!   基线对齐 `beside`（例如 `f(x) = \begin{aligned}…\end{aligned}`）。
//! - **定界符形态**：`\left<L> [前缀] \begin{ENV}…\end{ENV} [后缀] \right<R>` —— 摘掉
//!   外层 `\left`/`\right` 后，用**拉伸到网格高度**的定界符包住整个结果。不特殊处理的话
//!   `\left\{` 会被当独立单行块渲染成裸 `{`，旁边挂一个多行网格，视觉完全错位。
//! - 前缀/后缀里仍有 `\begin` / `\left` / `\right` → 返回 [`Option::None`]（组合语义
//!   我们没定义，宁可整条降级）。

use unicode_width::UnicodeWidthStr;

use crate::grid::layout::build_delimiter;
use crate::grid::rendered_block::RenderedBlock;
use crate::guard::{self, Reject};
use crate::normalize::delimiter_command_char;
use crate::scan::{EnvSpan, brace_arg, command_at, scan, slice, split_top_level};

/// 单元格列对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColAlign {
    Left,
    Center,
    Right,
}

/// 多行环境的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvKind {
    /// 偶数列右对齐、奇数列左对齐。
    AlignPairs,
    /// 所有列居中。
    Centered,
    /// 按列格式串逐列指定（`array`）。
    Array,
}

/// 列间距（格）。
const COL_GAP: usize = 2;

/// 环境名 → 类别。`None` 表示"不归我们管"（交给上游，或由 guard 拒绝）。
fn classify(name: &str) -> Option<EnvKind> {
    match name.trim_end_matches('*') {
        "align" | "aligned" | "alignat" | "alignedat" | "flalign" | "split" | "eqnarray" => {
            Some(EnvKind::AlignPairs)
        }
        "gather" | "multline" | "center" | "equation" | "displaymath" => Some(EnvKind::Centered),
        "array" => Some(EnvKind::Array),
        _ => None,
    }
}

/// 这个环境是不是"多行环境"（需要我们自己分行分列）。
pub(crate) fn is_multiline(name: &str) -> bool {
    classify(name).is_some()
}

/// `alignat` / `alignedat` 这类环境在 `\begin{...}` 后还跟一个 `{n}` 参数。
fn takes_count_arg(name: &str) -> bool {
    matches!(name.trim_end_matches('*'), "alignat" | "alignedat")
}

/// 渲染 `chars` 里由 `span` 描述的顶层多行环境。
///
/// `chars` 是**归一化之后**的整条公式；`depth` 是当前的环境嵌套深度（单元格递归时会 +1）。
///
/// 返回 [`Reject`] 表示"这个环境（或它的某个单元格/前后缀）渲染不了"。**必须整条上抛**：
/// 单元格渲染失败绝不能退化成"空格占位"——那就是静默丢内容。
pub(crate) fn render_multiline_env(
    chars: &[char],
    span: &EnvSpan,
    depth: usize,
) -> Result<RenderedBlock, Reject> {
    let kind = classify(&span.name).ok_or(Reject::UnsupportedEnvironment)?;

    let prefix = &chars[..span.start];
    let suffix = &chars[span.end..];

    // 摘掉 `\left<L>` / `\right<R>`
    let (left_delim, prefix) = take_left_delim(prefix);
    let (right_delim, suffix) = take_right_delim(suffix);
    if has_env_or_delim(prefix) || has_env_or_delim(suffix) {
        return Err(Reject::UnsupportedEnvironment);
    }

    let prefix_src = text(prefix);
    let suffix_src = text(suffix);
    let prefix_block = render_cell(&prefix_src, depth)?;
    let suffix_block = render_cell(&suffix_src, depth)?;

    let body: Vec<char> = chars[span.body_start..span.body_end].to_vec();
    let (body, spec) = strip_preamble(&span.name, kind, &body)?;
    let grid = build_grid(kind, &spec, &body, depth)?;

    // 前缀 / 后缀与网格**首行基线**对齐，中间隔一列，避免 `f(x) =a = b` 这种粘连
    let gap = RenderedBlock::from_text(" ");
    let mut block = match prefix_block {
        Some(p) => p.beside(&gap).beside(&grid),
        None => grid,
    };
    if let Some(s) = suffix_block {
        block = block.beside(&gap).beside(&s);
    }

    if left_delim.is_none() && right_delim.is_none() {
        return Ok(block);
    }
    // 定界符与块同高才不会让 `beside` 多长出一行：统一把基线放到中线
    let height = block.height();
    let block = RenderedBlock::new(block.cells().to_vec(), height / 2);
    let block = match left_delim {
        Some(d) => build_delimiter(&d, height).beside(&block),
        None => block,
    };
    Ok(match right_delim {
        Some(d) => block.beside(&build_delimiter(&d, height)),
        None => block,
    })
}

/// 摘掉 body 的前导参数：`array` 的列格式串、`alignat` 的列数。
///
/// 返回 `(去掉参数后的 body, 列格式)`。
fn strip_preamble(
    name: &str,
    kind: EnvKind,
    body: &[char],
) -> Result<(Vec<char>, Vec<ColAlign>), Reject> {
    let mut i = 0;
    while i < body.len() && body[i].is_whitespace() {
        i += 1;
    }
    match kind {
        EnvKind::Array => {
            let (spec, next) = brace_arg(body, i).ok_or(Reject::UnsupportedEnvironment)?;
            let cols = parse_array_spec(&spec).ok_or(Reject::UnsupportedEnvironment)?;
            Ok((body[next..].to_vec(), cols))
        }
        EnvKind::AlignPairs if takes_count_arg(name) => {
            if body.get(i) == Some(&'{') {
                let (_, next) = brace_arg(body, i).ok_or(Reject::UnsupportedEnvironment)?;
                Ok((body[next..].to_vec(), Vec::new()))
            } else {
                Ok((body.to_vec(), Vec::new()))
            }
        }
        _ => Ok((body.to_vec(), Vec::new())),
    }
}

/// `{lcr}` → 逐列对齐（`|` `@{}` 等间距/分隔记号一律忽略）。
fn parse_array_spec(spec: &str) -> Option<Vec<ColAlign>> {
    let mut cols = Vec::new();
    for c in spec.chars() {
        match c {
            'l' => cols.push(ColAlign::Left),
            'c' => cols.push(ColAlign::Center),
            'r' => cols.push(ColAlign::Right),
            // 列间距线：我们不做间距，忽略是安全的
            '|' | ' ' | '\t' => {}
            // 其它（`p{2cm}` / `@{}` / `*{}` / 嵌套组里的内容…）我们既不理解、
            // 也无法渲染 —— 忽略就等于静默丢内容，整条降级
            _ => return None,
        }
    }
    Some(cols)
}

/// 排列规则：给定类别与列号（0-based），返回该列的对齐。
fn align_of(kind: EnvKind, spec: &[ColAlign], col: usize) -> ColAlign {
    match kind {
        EnvKind::AlignPairs => {
            if col.is_multiple_of(2) {
                ColAlign::Right
            } else {
                ColAlign::Left
            }
        }
        EnvKind::Centered => ColAlign::Center,
        EnvKind::Array => spec.get(col).copied().unwrap_or(ColAlign::Center),
    }
}

/// 把 body 切成网格：`\\` 分行、`&` 分列、每格独立排版、按列对齐堆叠。
///
/// 任一单元格渲染失败都会把 [`Reject`] 上抛（**不允许**用空格占位）。
fn build_grid(
    kind: EnvKind,
    spec: &[ColAlign],
    body: &[char],
    depth: usize,
) -> Result<RenderedBlock, Reject> {
    // 1. 分行分列（空行丢弃：末尾的 `\\` 会产生一个空行）
    let mut rows: Vec<Vec<String>> = Vec::new();
    for row_range in split_top_level(body, true) {
        let row_chars = &body[row_range];
        let cells: Vec<String> = split_top_level(row_chars, false)
            .iter()
            .map(|c| slice(row_chars, c).trim().to_string())
            .collect();
        if cells.iter().all(|c| c.is_empty()) {
            continue;
        }
        rows.push(cells);
    }
    if rows.is_empty() {
        return Err(Reject::Empty);
    }
    // 早拒绝：行/列数已经超预算时不必再排版（S2：把预算闸前移）
    if rows.len() > guard::MAX_HEIGHT {
        return Err(Reject::TooTall);
    }
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return Err(Reject::Empty);
    }
    if ncols > guard::MAX_COL_SEPARATORS + 1 {
        return Err(Reject::TooManyCells);
    }

    // 2. 每格独立排版（空单元格保持 None，占位但不贡献高度；失败则整条上抛）
    let mut cells: Vec<Vec<Option<RenderedBlock>>> = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut rendered = Vec::with_capacity(row.len());
        for cell in row {
            rendered.push(render_cell(cell, depth + 1)?);
        }
        cells.push(rendered);
    }

    // 3. 列宽
    let mut col_widths = vec![1usize; ncols];
    for row in &cells {
        for (c, cell) in row.iter().enumerate() {
            if let Some(b) = cell {
                col_widths[c] = col_widths[c].max(b.width());
            }
        }
    }

    // 4. 逐行拼装
    let gap = RenderedBlock::from_text(&" ".repeat(COL_GAP));
    let mut row_blocks: Vec<RenderedBlock> = Vec::with_capacity(cells.len());
    for row in &cells {
        let mut row_block: Option<RenderedBlock> = None;
        for (c, width) in col_widths.iter().enumerate() {
            let padded = pad_cell(
                row.get(c).and_then(|x| x.as_ref()),
                *width,
                align_of(kind, spec, c),
            );
            row_block = Some(match row_block {
                None => padded,
                Some(prev) => prev.beside(&gap).beside(&padded),
            });
        }
        row_blocks.push(row_block.ok_or(Reject::Empty)?);
    }

    // 5. 竖向堆叠（不留空行）。每个行块在步骤 4 里已被补到网格宽度（单元格补齐 +
    //    列间距），所以 `above` 只做纯粹的上下拼接；宽度不足时它会自行右补空格。
    // 网格基线取**首行**的基线：`aligned` 这类环境在正文里是按第一行对齐的，
    // 这样 `f(x) = \begin{aligned}…` 的前缀会落在第一行而不是中间
    let baseline = row_blocks[0].baseline();
    let mut iter = row_blocks.into_iter();
    let mut grid = iter.next().ok_or(Reject::Empty)?;
    for block in iter {
        grid = RenderedBlock::above(&grid, &block, grid.height().saturating_sub(1));
    }

    let height = grid.height();
    if height == 0 {
        return Err(Reject::Empty);
    }
    Ok(RenderedBlock::new(
        grid.cells().to_vec(),
        baseline.min(height - 1),
    ))
}

/// 单元格排版：整体 trim + 去掉首尾全空列 + 行宽归一化。
///
/// 单元格内容**递归走完整管线**（[`crate::api::render_block_at`]），所以它享受与顶层
/// 公式完全相同的归一化、自检与降级语义，也支持再嵌一层多行环境。
///
/// 返回：
/// - `Ok(None)`：格子是空的（仍然占列宽，但不贡献高度）；
/// - `Ok(Some(block))`：渲染成功；
/// - `Err(Reject)`：**渲染失败** —— 必须整条上抛，绝不能用空格占位（B1）。
fn render_cell(src: &str, depth: usize) -> Result<Option<RenderedBlock>, Reject> {
    let src = src.trim();
    if src.is_empty() {
        return Ok(None);
    }
    match crate::api::render_block_at(src, depth) {
        Ok(block) => {
            let block = normalize_row_widths(trim_blank_columns(block));
            if block.height() == 0 {
                return Ok(None);
            }
            // 渲染出来只有空白（`\sqrt{}` / `\,` 这类）：交给 Empty 语义处理，
            // 不能让它在网格里留下一行空白
            if !block
                .cells()
                .iter()
                .any(|row| row.iter().any(|c| c.trim() != ""))
            {
                return Ok(None);
            }
            Ok(Some(block))
        }
        // 真·空内容（`\,`、`{}` 之类）：占位但不贡献高度，不算丢内容
        Err(Reject::Empty) => Ok(None),
        Err(other) => Err(other),
    }
}

/// 一行的显示宽度（按 `unicode-width`，CJK 宽字符算 2 列）。
fn row_width(row: &[String]) -> usize {
    row.iter().map(|c| UnicodeWidthStr::width(c.as_str())).sum()
}

/// 让块的每一行都有相同的**显示宽度**。
///
/// 上游 `RenderedBlock` 在宽字符（CJK/emoji）参与组合时，"格数"与"列数"会不一致
/// （`from_text` 一格一个 `char`，而 `width` 按 `unicode-width` 记账），于是块内各行
/// 宽度参差，参与 `beside` / `above` 组合时会错列。这里按显示宽度把每行补齐。
fn normalize_row_widths(block: RenderedBlock) -> RenderedBlock {
    let widths: Vec<usize> = block.cells().iter().map(|r| row_width(r)).collect();
    let max = widths.iter().copied().max().unwrap_or(0);
    if widths.iter().all(|w| *w == max) {
        return block;
    }
    let cells: Vec<Vec<String>> = block
        .cells()
        .iter()
        .zip(&widths)
        .map(|(row, w)| {
            let mut row = row.clone();
            row.extend(std::iter::repeat_n(" ".to_string(), max - w));
            row
        })
        .collect();
    RenderedBlock::new(cells, block.baseline())
}

/// `&[char]` → `String`。
fn text(chars: &[char]) -> String {
    chars.iter().collect()
}

/// 去掉块首尾的**全空列**。上游单元格内容常带首尾空格（例如 `= b + c` 前面那个
/// 操作符空格），不裁掉的话 `align` 的列间距会忽宽忽窄。
fn trim_blank_columns(block: RenderedBlock) -> RenderedBlock {
    let cells = block.cells();
    let min_cols = cells.iter().map(|r| r.len()).min().unwrap_or(0);
    let max_cols = cells.iter().map(|r| r.len()).max().unwrap_or(0);
    // 宽字符（CJK）会让"格数"与"显示宽度"不再一一对应，这种情况不裁，保守处理
    if min_cols == 0 || min_cols != max_cols {
        return block;
    }
    let mut left = 0;
    while left < min_cols && cells.iter().all(|r| r[left] == " ") {
        left += 1;
    }
    let mut right = min_cols;
    while right > left && cells.iter().all(|r| r[right - 1] == " ") {
        right -= 1;
    }
    if left == 0 && right == min_cols {
        return block;
    }
    let rows: Vec<Vec<String>> = cells.iter().map(|r| r[left..right].to_vec()).collect();
    RenderedBlock::new(rows, block.baseline())
}

/// 把单元格补齐到列宽（空单元格补成一行空格，保证列不塌陷）。
fn pad_cell(block: Option<&RenderedBlock>, width: usize, align: ColAlign) -> RenderedBlock {
    let Some(b) = block else {
        return RenderedBlock::from_text(&" ".repeat(width));
    };
    let diff = width.saturating_sub(b.width());
    let (left, right) = match align {
        ColAlign::Left => (0, diff),
        ColAlign::Right => (diff, 0),
        ColAlign::Center => (diff / 2, diff - diff / 2),
    };
    b.pad(left, right, 0, 0)
}

/// 从 `\left<D>` 里摘出定界符字符，返回 `(定界符, 剩余前缀)`。
///
/// - `\left.`（不可见）→ `None`（但仍算"有定界符形态"，只是不画）；
/// - 前缀不以 `\left` 开头 → `(None, 原样)`。
fn take_left_delim(chars: &[char]) -> (Option<String>, &[char]) {
    let mut i = 0;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    let Some((name, after)) = command_at(chars, i) else {
        return (None, chars);
    };
    if name != "left" {
        return (None, chars);
    }
    let mut j = after;
    while j < chars.len() && chars[j] == ' ' {
        j += 1;
    }
    match delimiter_text(chars, j) {
        Some((text, next)) => (visible(text), &chars[next..]),
        None => (None, chars),
    }
}

/// 从结尾的 `\right<D>` 里摘出定界符字符，返回 `(定界符, 剩余后缀)`。
fn take_right_delim(chars: &[char]) -> (Option<String>, &[char]) {
    let mut end = chars.len();
    while end > 0 && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    let mut i = end;
    while i > 0 {
        i -= 1;
        if chars[i] != '\\' {
            continue;
        }
        let Some((name, after)) = command_at(chars, i) else {
            continue;
        };
        if name != "right" {
            continue;
        }
        let mut j = after;
        while j < end && chars[j] == ' ' {
            j += 1;
        }
        let Some((text, next)) = delimiter_text(chars, j) else {
            continue;
        };
        if next <= end && chars[next..end].iter().all(|c| c.is_whitespace()) {
            return (visible(text), &chars[..i]);
        }
    }
    (None, chars)
}

/// `.` 表示不可见定界符。
fn visible(text: String) -> Option<String> {
    if text == "." { None } else { Some(text) }
}

/// 读一个定界符记号（一个字符，或一个定界符命令），返回 `(文本, 之后的下标)`。
fn delimiter_text(chars: &[char], i: usize) -> Option<(String, usize)> {
    let &c = chars.get(i)?;
    if c == '\\' {
        if let Some((name, after)) = command_at(chars, i) {
            return delimiter_command_char(&name).map(|ch| (ch.to_string(), after));
        }
        return None;
    }
    Some((c.to_string(), i + 1))
}

/// 前缀/后缀里还剩环境或定界符命令 → 组合语义未定义，调用方应拒绝整条公式。
fn has_env_or_delim(chars: &[char]) -> bool {
    let st = scan(chars);
    !st.balanced_envs || !st.envs.is_empty() || st.left_count > 0 || st.right_count > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::normalize;
    use crate::scan::scan;

    fn render_env(src: &str) -> Option<RenderedBlock> {
        let normalized = normalize(src);
        let chars: Vec<char> = normalized.chars().collect();
        let st = scan(&chars);
        let span = st.envs.first()?;
        render_multiline_env(&chars, span, 0).ok()
    }

    fn lines(src: &str) -> Vec<String> {
        render_env(src)
            .map(|b| {
                format!("{}", b)
                    .lines()
                    .map(|l| l.trim_end().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn classify_known_environments() {
        for name in [
            "align",
            "align*",
            "aligned",
            "gather",
            "split",
            "multline*",
            "array",
            "eqnarray",
            "flalign*",
            "alignat",
        ] {
            assert!(is_multiline(name), "should be multiline: {name}");
        }
        for name in [
            "cases",
            "pmatrix",
            "matrix",
            "bmatrix",
            "unknown",
            "smallmatrix",
        ] {
            assert!(!is_multiline(name), "should not be multiline: {name}");
        }
    }

    #[test]
    fn align_pairs_aligns_columns() {
        // 偶数列右对齐、奇数列左对齐
        let out = lines(r"\begin{align} a &= b + c \\ dd &= e \end{align}");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], " a  = b + c");
        assert_eq!(out[1], "dd  = e");
    }

    #[test]
    fn gather_centers_rows() {
        let out = lines(r"\begin{gather} a = b \\ cccc = d \end{gather}");
        assert_eq!(out, vec![" a = b", "cccc = d"]);
    }

    #[test]
    fn split_behaves_like_align() {
        let out = lines(r"\begin{split} x &= y \\ z &= w \end{split}");
        assert_eq!(out, vec!["x  = y", "z  = w"]);
    }

    #[test]
    fn multline_is_centered() {
        let out = lines(r"\begin{multline} a \\ bcd \end{multline}");
        assert_eq!(out, vec![" a", "bcd"]);
    }

    #[test]
    fn eqnarray_alias() {
        let out = lines(r"\begin{eqnarray} a &= b \\ c &= d \end{eqnarray}");
        assert_eq!(out, vec!["a  = b", "c  = d"]);
    }

    #[test]
    fn alignat_count_arg_is_skipped() {
        let out = lines(r"\begin{alignat}{2} a &= b \\ c &= d \end{alignat}");
        assert_eq!(out, vec!["a  = b", "c  = d"]);
    }

    #[test]
    fn array_uses_column_spec() {
        let out = lines(r"\begin{array}{lcr} a & b & c \\ dd & e & f \end{array}");
        assert_eq!(out.len(), 2);
        // 第一列左对齐、第三列右对齐
        assert!(out[0].starts_with("a "));
        assert_eq!(out[0].chars().count(), out[1].chars().count());
    }

    #[test]
    fn array_defaults_to_center_without_extendable_spec() {
        let out = lines(r"\begin{array}{l} a & b \\ c & d \end{array}");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].chars().count(), out[1].chars().count());
    }

    #[test]
    fn optional_row_spacing_is_ignored() {
        let out = lines(r"\begin{align} a &= b \\[6pt] c &= d \end{align}");
        assert_eq!(out, vec!["a  = b", "c  = d"]);
    }

    #[test]
    fn trailing_row_separator_produces_no_blank_row() {
        let out = lines(r"\begin{align} a &= b \\ \end{align}");
        assert_eq!(out, vec!["a  = b"]);
    }

    #[test]
    fn empty_cells_keep_columns() {
        let out = lines(r"\begin{align} a &= b \\ & = c \end{align}");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].chars().count(), out[1].chars().count());
    }

    #[test]
    fn nested_cases_inside_cell_is_delegated_to_upstream() {
        let out = lines(
            r"\begin{align} f(x) &= \begin{cases} x & x>0 \\ 0 & x\le 0 \end{cases} \end{align}",
        );
        assert!(!out.is_empty());
        let joined = out.join("\n");
        assert!(joined.contains("f(x)"));
        assert!(joined.contains('⎧') || joined.contains('{'));
        assert!(joined.contains("x > 0"));
    }

    #[test]
    fn fraction_in_cell_is_multiline_aligned_on_baseline() {
        let out = lines(r"\begin{align} a &= \frac{1}{2} \\ b &= 1 \end{align}");
        // 分式 3 行 + 第二行 1 行 → 总 4 行
        assert_eq!(out.len(), 4);
        let joined = out.join("\n");
        assert!(joined.contains('─'));
        assert!(joined.contains("a"));
        assert!(joined.contains("b"));
    }

    #[test]
    fn delimited_environment_uses_stretched_delimiters() {
        let out = lines(r"\left\{ \begin{aligned} a &= b \\ c &= d \end{aligned} \right.");
        assert_eq!(out.len(), 2);
        // 拉伸后的左花括号用 ⎧ / ⎩，而不是裸 `{`
        assert!(out[0].starts_with('⎧'), "got {:?}", out[0]);
        assert!(out[1].starts_with('⎩'), "got {:?}", out[1]);
    }

    #[test]
    fn delimited_environment_with_both_visible_delimiters() {
        let out = lines(r"\left( \begin{array}{c} a \\ b \end{array} \right)");
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with('⎛'), "got {:?}", out[0]);
        assert!(out[1].ends_with('⎠'), "got {:?}", out[1]);
    }

    #[test]
    fn prefix_and_suffix_are_composed() {
        let out = lines(r"f(x) = \begin{aligned} a &= b \\ c &= d \end{aligned}");
        assert!(out.iter().any(|l| l.contains("f(x) =")), "got {out:?}");
    }

    #[test]
    fn nested_multiline_environment_is_rendered_recursively() {
        // 单元格内容递归走完整管线，所以内层 `aligned` 也能正常渲染
        let m = crate::render_display(
            r"\begin{align} \begin{aligned} a &= b \end{aligned} \end{align}",
            40,
        )
        .unwrap();
        assert_eq!(m.lines(), ["a  = b"]);
    }

    #[test]
    fn rejects_span_that_is_not_a_multiline_environment() {
        // `cases` 不归我们管（上游能渲染），走到这里说明调用方分类错了
        assert!(render_env(r"\begin{cases} a & b \end{cases}").is_none());
    }

    #[test]
    fn styles_and_sizing_are_normalized_before_layout() {
        let out = lines(r"\begin{align} \displaystyle a &= \bigl( b \bigr) \end{align}");
        assert_eq!(out, vec!["a  = ( b )"]);
    }

    #[test]
    fn trim_blank_columns_removes_edge_padding() {
        let block = RenderedBlock::new(
            vec![
                vec![" ".to_string(), "a".to_string(), " ".to_string()],
                vec![" ".to_string(), "b".to_string(), " ".to_string()],
            ],
            0,
        );
        let trimmed = trim_blank_columns(block);
        assert_eq!(trimmed.width(), 1);
        assert_eq!(format!("{}", trimmed), "a\nb");
    }

    #[test]
    fn trim_blank_columns_keeps_wide_char_blocks_untouched() {
        // 格数与显示宽度不一致时保守不裁
        let block = RenderedBlock::new(vec![vec!["中".to_string()]], 0);
        let trimmed = trim_blank_columns(block);
        assert_eq!(format!("{}", trimmed), "中");
    }

    #[test]
    fn build_grid_returns_none_for_empty_body() {
        assert!(render_env(r"\begin{align}\end{align}").is_none());
    }

    #[test]
    fn rejects_prefix_that_still_holds_a_delimiter() {
        // `\left` 出现在前缀中间（不是紧贴环境），我们没法把它拉伸到网格高度
        assert!(render_env(r"f(x) = \left( x \begin{aligned} a &= b \end{aligned}").is_none());
    }

    #[test]
    fn scan_span_round_trip_is_consistent() {
        let src = normalize(r"\begin{gather} a \\ b \end{gather}");
        let chars: Vec<char> = src.chars().collect();
        let st = scan(&chars);
        let span = &st.envs[0];
        assert_eq!(chars[span.start], '\\');
        assert_eq!(slice(&chars, &(span.start..span.end)), src);
    }
}
