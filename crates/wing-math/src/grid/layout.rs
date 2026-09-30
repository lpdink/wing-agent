// ---------------------------------------------------------------------------
// 来源（vendored）
//   crate   : term-maths
//   version : 1.0.0
//   repo    : 包元数据未声明 repository（crates.io `term-maths`；发布时
//             `.cargo_vcs_info.json` git sha1 = 5a2de3b29f1d4cec72c8685b8d52ddfb53519676）
//   license : MIT OR Apache-2.0，Copyright (c) 2026 Jack Geraghty
//             （原文见 crate 根 LICENSE-MIT / LICENSE-APACHE）
//   原路径  : src/layout.rs
//
// 本地改动（相对上游，逐条）：
//   1. 模块路径：`rust_latex_parser::` -> `crate::latex::`，
//      `crate::xxx` -> `crate::grid::xxx`。
//   2. `layout_accent` 的 Hat / Dot / DoubleDot 改用新增的私有辅助函数
//      `center_accent`：上游直接 `center_in`，而 `center_in` 在余量为奇数时把
//      多余列放在**右侧**，导致 `\hat{abc}` 渲染成 `/\ `（accent 左偏一列）。
//      `center_accent` 把多余列放左侧。仅影响"单字形 accent + 奇数余量宽度"
//      这一种输入，其余路径一字未动（见函数旁注释）。
//   3. `build_delimiter` 由私有改为 `pub(crate)`（逻辑未改），供
//      `crate::environments` 生成拉伸定界符。
//   4. `layout_seq` 改为一次性拼装（`crate::compose::beside_all`）：上游对每个子节点
//      反复 `beside`，第 k 步复制前 k 块拼出的整个网格，长公式退化成 O(n²)
//      （实测 2000 项单行公式 release 282 ms）。语义逐格等价（对拍见 compose 单测）。
//   5. `layout_binom`：上游 `above(&top, &bot, baseline - 1)` 在 `top` 为空块时
//      `0 - 1` 下溢 panic（`\binom{}{}`，debug 直接 abort），且堆叠块与定界符块基线
//      不一致，`\binom{n}{k}` 被画成错位三行。改为纯堆叠 + 基线归中。
//   6. `layout_sqrt`：空被开方数（`\sqrt{}`）直接返回空块。上游会让 `body_h == 0`
//      走进"多行"分支，生成基线 = 1、高度 = 1 的块（**基线越界**），`beside` 按基线
//      对齐时把整块下推一行，于是 `= \sqrt{}` 变成 2 行、`\begin{align} a &= \sqrt{}`
//      多出一个空行。
//   7. 运行 `cargo fmt`（仓库门禁要求 `cargo fmt --check` 干净）。上游文件未经 rustfmt
//      处理，因此有纯空白差异；已用「先 rustfmt 上游文件、再与本文件逐行 diff」核对，
//      除上述改动外逐字一致（核对脚本见 crate 根 NOTICE 的「内联保真度」一节）。
//   除以上七点外与上游逐字一致。
// ---------------------------------------------------------------------------

use crate::latex::{AccentKind, EqNode, MathFontKind, MatrixKind};

use crate::grid::mathfont;
use crate::grid::rendered_block::RenderedBlock;

/// Render an `EqNode` AST into a `RenderedBlock`.
pub fn layout(node: &EqNode) -> RenderedBlock {
    match node {
        EqNode::Text(s) => layout_text(s),
        EqNode::Space(pts) => layout_space(*pts),
        EqNode::Seq(children) => layout_seq(children),
        EqNode::Frac(num, den) => layout_frac(num, den),
        EqNode::Sup(base, sup) => layout_sup(base, sup),
        EqNode::Sub(base, sub) => layout_sub(base, sub),
        EqNode::SupSub(base, sup, sub) => layout_supsub(base, sup, sub),
        EqNode::Sqrt(body) => layout_sqrt(body),
        EqNode::BigOp {
            symbol,
            lower,
            upper,
        } => layout_bigop(symbol, lower, upper),
        EqNode::Accent(body, kind) => layout_accent(body, kind),
        EqNode::Limit { name, lower } => layout_limit(name, lower),
        EqNode::TextBlock(s) => RenderedBlock::from_text(s),
        EqNode::MathFont { kind, content } => layout_mathfont(kind, content),
        EqNode::Delimited {
            left,
            right,
            content,
        } => layout_delimited(left, right, content),
        EqNode::Matrix { kind, rows } => layout_matrix(kind, rows),
        EqNode::Cases { rows } => layout_cases(rows),
        EqNode::Binom(top, bottom) => layout_binom(top, bottom),
        EqNode::Brace {
            content,
            label,
            over,
        } => layout_brace(content, label, over),
        EqNode::StackRel {
            base,
            annotation,
            over,
        } => layout_stackrel(base, annotation, over),
    }
}

fn layout_text(s: &str) -> RenderedBlock {
    RenderedBlock::from_text(s)
}

/// Map a character to its Unicode superscript equivalent, if one exists.
fn to_superscript_char(ch: char) -> Option<char> {
    match ch {
        '0' => Some('⁰'),
        '1' => Some('¹'),
        '2' => Some('²'),
        '3' => Some('³'),
        '4' => Some('⁴'),
        '5' => Some('⁵'),
        '6' => Some('⁶'),
        '7' => Some('⁷'),
        '8' => Some('⁸'),
        '9' => Some('⁹'),
        '+' => Some('⁺'),
        '-' => Some('⁻'),
        '=' => Some('⁼'),
        '(' => Some('⁽'),
        ')' => Some('⁾'),
        'n' => Some('ⁿ'),
        'i' => Some('ⁱ'),
        _ => None,
    }
}

/// Map a character to its Unicode subscript equivalent, if one exists.
fn to_subscript_char(ch: char) -> Option<char> {
    match ch {
        '0' => Some('₀'),
        '1' => Some('₁'),
        '2' => Some('₂'),
        '3' => Some('₃'),
        '4' => Some('₄'),
        '5' => Some('₅'),
        '6' => Some('₆'),
        '7' => Some('₇'),
        '8' => Some('₈'),
        '9' => Some('₉'),
        '+' => Some('₊'),
        '-' => Some('₋'),
        '=' => Some('₌'),
        '(' => Some('₍'),
        ')' => Some('₎'),
        'a' => Some('ₐ'),
        'e' => Some('ₑ'),
        'h' => Some('ₕ'),
        'i' => Some('ᵢ'),
        'j' => Some('ⱼ'),
        'k' => Some('ₖ'),
        'l' => Some('ₗ'),
        'm' => Some('ₘ'),
        'n' => Some('ₙ'),
        'o' => Some('ₒ'),
        'p' => Some('ₚ'),
        'r' => Some('ᵣ'),
        's' => Some('ₛ'),
        't' => Some('ₜ'),
        'u' => Some('ᵤ'),
        'v' => Some('ᵥ'),
        'x' => Some('ₓ'),
        _ => None,
    }
}

/// Try to convert a node's text content to Unicode superscript characters.
/// Returns None if any character lacks a superscript form.
fn try_unicode_superscript(node: &EqNode) -> Option<String> {
    let text = extract_flat_text(node)?;
    text.chars().map(to_superscript_char).collect()
}

/// Try to convert a node's text content to Unicode subscript characters.
fn try_unicode_subscript(node: &EqNode) -> Option<String> {
    let text = extract_flat_text(node)?;
    text.chars().map(to_subscript_char).collect()
}

/// Extract flat text from simple nodes (Text, Seq of Text).
fn extract_flat_text(node: &EqNode) -> Option<String> {
    match node {
        EqNode::Text(s) => Some(s.clone()),
        EqNode::Seq(children) => {
            let mut result = String::new();
            for child in children {
                match child {
                    EqNode::Text(s) => result.push_str(s),
                    EqNode::Space(_) => {} // skip spaces in scripts
                    _ => return None,
                }
            }
            if result.is_empty() {
                None
            } else {
                Some(result)
            }
        }
        _ => None,
    }
}

/// Render a Space node. The parser auto-inserts Space nodes around operators.
/// Negative and very small spaces collapse. Standard operator spaces (3–5pt)
/// become a single space. Larger explicit spaces (\quad etc.) grow accordingly.
fn layout_space(pts: f32) -> RenderedBlock {
    if pts <= 0.0 || pts < 2.0 {
        RenderedBlock::empty()
    } else if pts >= 18.0 {
        // \quad or larger
        RenderedBlock::from_text("  ")
    } else {
        RenderedBlock::from_char(' ')
    }
}

/// Check if a node is whitespace-like (Space node or Text containing only spaces).
fn is_space_like(node: &EqNode) -> bool {
    match node {
        EqNode::Space(_) => true,
        EqNode::Text(s) => s.chars().all(|c| c == ' '),
        _ => false,
    }
}

fn layout_seq(children: &[EqNode]) -> RenderedBlock {
    // Flatten nested Seqs so we can handle spacing uniformly.
    let flat = flatten_seq(children);
    // Collapse consecutive whitespace-like nodes into a single space.
    //
    // 本地改动（见文件头）：上游这里对每个子节点反复调用 `RenderedBlock::beside`，
    // 第 k 步都要复制"前 k 块拼出来的整个网格"，长公式退化成 O(n²)（实测 2000 项
    // 单行公式 release 282 ms）。改成先收集再一次性拼装，语义逐格等价。
    let mut parts: Vec<RenderedBlock> = Vec::with_capacity(flat.len());
    let mut prev_was_space = false;
    for child in &flat {
        if is_space_like(child) {
            if !prev_was_space {
                prev_was_space = true;
                parts.push(RenderedBlock::from_char(' '));
            }
            continue;
        }
        prev_was_space = false;
        parts.push(layout(child));
    }
    crate::compose::beside_all(&parts)
}

/// Trim leading/trailing whitespace from a node.
/// Strips Space nodes and whitespace-only Text nodes from Seq boundaries.
fn trim_node(node: &EqNode) -> EqNode {
    match node {
        EqNode::Seq(children) => {
            let trimmed: Vec<EqNode> = children
                .iter()
                .map(|c| match c {
                    EqNode::Text(s) => EqNode::Text(s.trim().to_string()),
                    other => other.clone(),
                })
                .filter(|c| !is_space_like(c) || !matches!(c, EqNode::Text(s) if s.is_empty()))
                .collect();
            // Remove leading/trailing space-like nodes
            let start = trimmed.iter().position(|c| !is_space_like(c)).unwrap_or(0);
            let end = trimmed
                .iter()
                .rposition(|c| !is_space_like(c))
                .map_or(0, |i| i + 1);
            if start >= end {
                return EqNode::Seq(vec![]);
            }
            EqNode::Seq(trimmed[start..end].to_vec())
        }
        EqNode::Text(s) => EqNode::Text(s.trim().to_string()),
        other => other.clone(),
    }
}

/// Recursively flatten nested Seq nodes into a single flat list.
fn flatten_seq(children: &[EqNode]) -> Vec<&EqNode> {
    let mut result = Vec::new();
    for child in children {
        if let EqNode::Seq(inner) = child {
            result.extend(flatten_seq(inner));
        } else {
            result.push(child);
        }
    }
    result
}

fn layout_frac(num: &EqNode, den: &EqNode) -> RenderedBlock {
    let num_block = layout(num);
    let den_block = layout(den);

    let bar_width = num_block.width().max(den_block.width()) + 2; // +2 for padding
    let bar = RenderedBlock::hline('─', bar_width);

    let num_centered = num_block.center_in(bar_width);
    let den_centered = den_block.center_in(bar_width);

    // Stack: numerator, bar, denominator. Baseline is the bar row.
    let top = RenderedBlock::above(&num_centered, &bar, 0);
    let baseline_row = top.height() - 1; // bar is the last row of 'top'
    RenderedBlock::above(&top, &den_centered, baseline_row)
}

fn layout_sup(base: &EqNode, sup: &EqNode) -> RenderedBlock {
    // Try inline Unicode superscript first
    if let Some(sup_text) = try_unicode_superscript(sup) {
        let base_block = layout(base);
        let sup_block = RenderedBlock::from_text(&sup_text);
        return base_block.beside(&sup_block);
    }

    let base_block = layout(base);
    let sup_block = layout(sup);

    let can_overlap = base_block.height() > 1;
    let sup_above = if can_overlap {
        sup_block.height().saturating_sub(1)
    } else {
        sup_block.height()
    };

    let rows = build_sup_sub_grid(
        base_block.cells(),
        base_block.width(),
        base_block.baseline(),
        sup_block.cells(),
        sup_block.width(),
        None,
        0,
    );

    let total_height = rows.len();
    let baseline = sup_above + base_block.baseline();

    RenderedBlock::new(rows, baseline.min(total_height.saturating_sub(1)))
}

fn layout_sub(base: &EqNode, sub: &EqNode) -> RenderedBlock {
    // Try inline Unicode subscript first
    if let Some(sub_text) = try_unicode_subscript(sub) {
        let base_block = layout(base);
        let sub_block = RenderedBlock::from_text(&sub_text);
        return base_block.beside(&sub_block);
    }

    let base_block = layout(base);
    let sub_block = layout(sub);

    let rows = build_sup_sub_grid(
        base_block.cells(),
        base_block.width(),
        base_block.baseline(),
        &[],
        0,
        Some((sub_block.cells(), sub_block.width())),
        0,
    );

    let baseline = base_block.baseline();
    let total_height = rows.len();

    RenderedBlock::new(rows, baseline.min(total_height.saturating_sub(1)))
}

fn layout_supsub(base: &EqNode, sup: &EqNode, sub: &EqNode) -> RenderedBlock {
    // Try inline Unicode for both scripts
    let sup_inline = try_unicode_superscript(sup);
    let sub_inline = try_unicode_subscript(sub);

    if let (Some(sup_text), Some(sub_text)) = (&sup_inline, &sub_inline) {
        let base_block = layout(base);
        let scripts = format!("{}{}", sup_text, sub_text);
        // Subscript chars go right after superscript chars, all inline
        // Actually stack them: sup on same line, sub on same line
        // For compactness: base followed by sup_text on top row, sub_text on bottom
        // Simplest: just append both inline
        return base_block.beside(&RenderedBlock::from_text(&scripts));
    }

    // Fall back to multi-row layout
    let base_block = layout(base);
    let sup_block = layout(sup);
    let sub_block = layout(sub);

    let can_overlap_sup = base_block.height() > 1;
    let sup_above = if can_overlap_sup {
        sup_block.height().saturating_sub(1)
    } else {
        sup_block.height()
    };

    let rows = build_sup_sub_grid(
        base_block.cells(),
        base_block.width(),
        base_block.baseline(),
        sup_block.cells(),
        sup_block.width(),
        Some((sub_block.cells(), sub_block.width())),
        0,
    );

    let total_height = rows.len();
    let baseline = sup_above + base_block.baseline();

    RenderedBlock::new(rows, baseline.min(total_height.saturating_sub(1)))
}

/// Build a grid for base with optional superscript above-right and subscript below-right.
///
/// Layout:
/// ```text
///          [sup rows]
///   [base] [overlap ]
///          [sub rows]
/// ```
///
/// The superscript's last row overlaps with the base's first row (right side).
/// The subscript's first row overlaps with the base's last row (right side).
/// Build a grid for base with optional superscript above-right and subscript below-right.
///
/// For a single-row base like `x`:
/// - `x^2`   renders as:  ` 2`  /  `x `
/// - `x_i`   renders as:  `x `  /  ` i`
/// - `x_i^2` renders as:  ` 2`  /  `x `  /  ` i`
///
/// For multi-row bases, sup overlaps with the top row and sub with the bottom row.
fn build_sup_sub_grid(
    base_cells: &[Vec<String>],
    base_width: usize,
    _base_baseline: usize,
    sup_cells: &[Vec<String>],
    sup_width: usize,
    sub: Option<(&[Vec<String>], usize)>,
    _sub_baseline: usize,
) -> Vec<Vec<String>> {
    let base_height = base_cells.len();
    let sup_height = sup_cells.len();
    let (sub_cells, sub_width) = sub.unwrap_or((&[], 0));
    let sub_height = sub_cells.len();
    let has_sup = sup_height > 0;
    let has_sub = sub_height > 0;

    let script_width = sup_width.max(sub_width);

    // For single-row bases with both sup and sub, don't overlap — stack all three.
    // For multi-row bases or single script, allow 1 row of overlap.
    let can_overlap_sup = has_sup && base_height > 1;
    let can_overlap_sub = has_sub && base_height > 1 && !(has_sup && base_height <= 2);

    let sup_above = if can_overlap_sup {
        sup_height.saturating_sub(1)
    } else {
        sup_height
    };

    let sub_below = if can_overlap_sub {
        sub_height.saturating_sub(1)
    } else {
        sub_height
    };

    let total_height = sup_above + base_height + sub_below;
    let mut rows = Vec::with_capacity(total_height);

    let empty_script = || std::iter::repeat_n(" ".to_string(), script_width);

    // Helper to append a script row (or padding) to a row
    fn append_script_row(
        row: &mut Vec<String>,
        cells: &[Vec<String>],
        idx: usize,
        script_width: usize,
    ) {
        if idx < cells.len() {
            row.extend(cells[idx].iter().cloned());
            let used = cells[idx].len();
            row.extend(std::iter::repeat_n(
                " ".to_string(),
                script_width.saturating_sub(used),
            ));
        } else {
            row.extend(std::iter::repeat_n(" ".to_string(), script_width));
        }
    }

    // Phase 1: sup-only rows above the base
    for r in 0..sup_above {
        let mut row = vec![" ".to_string(); base_width];
        append_script_row(&mut row, sup_cells, r, script_width);
        rows.push(row);
    }

    // Phase 2: base rows (with possible script overlap)
    for (r, base_row) in base_cells.iter().enumerate().take(base_height) {
        let mut row = base_row.clone();

        // Check if a sup row overlaps here
        let sup_idx = if can_overlap_sup {
            sup_above + r
        } else {
            usize::MAX
        };
        // Check if a sub row overlaps here
        let sub_overlap_start = if can_overlap_sub {
            base_height.saturating_sub(sub_height)
        } else {
            usize::MAX
        };
        let sub_idx = if r >= sub_overlap_start && can_overlap_sub {
            r - sub_overlap_start
        } else {
            usize::MAX
        };

        if sup_idx < sup_height {
            append_script_row(&mut row, sup_cells, sup_idx, script_width);
        } else if sub_idx < sub_height {
            append_script_row(&mut row, sub_cells, sub_idx, script_width);
        } else {
            row.extend(empty_script());
        }

        rows.push(row);
    }

    // Phase 3: sub-only rows below the base
    let sub_start = if can_overlap_sub {
        sub_height.min(base_height)
    } else {
        0
    };
    for r in sub_start..sub_height {
        let mut row = vec![" ".to_string(); base_width];
        append_script_row(&mut row, sub_cells, r, script_width);
        rows.push(row);
    }

    rows
}

fn layout_sqrt(body: &EqNode) -> RenderedBlock {
    let body_block = layout(body);
    let body_h = body_block.height();
    let body_w = body_block.width();

    // 本地改动（见文件头）：空被开方数（`\sqrt{}`）直接返回空块。
    //
    // 上游会带着 `body_h == 0` 走进下面的"多行"分支，生成只有一行 overline 的块，
    // 而基线算成 `1 + 0 = 1` —— **等于高度**，越界。`beside` 按基线对齐时会把整块
    // 往下推一行，于是 `= \sqrt{}` 渲染成 2 行（首行全空）、`\begin{align} a &= \sqrt{}`
    // 多出一个空行。空块在 `beside` / `above` 里本来就会被忽略，语义与"没有这一项"一致。
    if body_h == 0 {
        return RenderedBlock::empty();
    }

    // Single-row body:   ___
    //                   √abc
    //
    // Multi-row body:    ________
    //                   ╱  num
    //                  ╱  ─────
    //                 √   den

    if body_h == 1 {
        // Simple case: √ prefix with overline above
        let mut rows = Vec::with_capacity(2);
        // Overline row
        let mut top = vec![" ".to_string()];
        top.extend(std::iter::repeat_n("─".to_string(), body_w));
        rows.push(top);
        // Body row with √
        let mut bot = vec!["√".to_string()];
        bot.extend(body_block.cells()[0].iter().cloned());
        rows.push(bot);
        RenderedBlock::new(rows, 1) // baseline at body row
    } else {
        // Multi-row: radical extends upward
        let mut rows = Vec::with_capacity(body_h + 1);

        // Overline row
        let mut top = vec![" ".to_string()];
        top.extend(std::iter::repeat_n("─".to_string(), body_w));
        rows.push(top);

        // Body rows with radical on the left
        for r in 0..body_h {
            let radical_char = if r == body_h - 1 { "√" } else { "│" };
            let mut row = vec![radical_char.to_string()];
            row.extend(body_block.cells()[r].iter().cloned());
            rows.push(row);
        }

        let baseline = 1 + body_block.baseline();
        RenderedBlock::new(rows, baseline)
    }
}

/// Build a multi-row operator symbol for integrals (⌠⎮⌡) and large Σ/∏.
fn build_bigop_symbol(symbol: &str) -> RenderedBlock {
    match symbol {
        "∫" => {
            // 3-row integral using bracket pieces
            let rows = vec![
                vec!["⌠".to_string()],
                vec!["⎮".to_string()],
                vec!["⌡".to_string()],
            ];
            RenderedBlock::new(rows, 1) // baseline at middle
        }
        "∬" => {
            let rows = vec![
                vec!["⌠".to_string(), "⌠".to_string()],
                vec!["⎮".to_string(), "⎮".to_string()],
                vec!["⌡".to_string(), "⌡".to_string()],
            ];
            RenderedBlock::new(rows, 1)
        }
        "∮" => {
            // Contour integral — use single char since no multi-row form exists
            let rows = vec![
                vec!["⌠".to_string()],
                vec!["⎮".to_string()],
                vec!["⌡".to_string()],
            ];
            RenderedBlock::new(rows, 1)
        }
        _ => {
            // Σ, ∏, etc. — single character is fine, they're already wide enough
            RenderedBlock::from_text(symbol)
        }
    }
}

fn layout_bigop(
    symbol: &str,
    lower: &Option<Box<EqNode>>,
    upper: &Option<Box<EqNode>>,
) -> RenderedBlock {
    let op_block = build_bigop_symbol(symbol);

    let upper_block = upper.as_ref().map(|u| layout(u));
    let lower_block = lower.as_ref().map(|l| layout(l));

    let max_width = [
        op_block.width(),
        upper_block.as_ref().map_or(0, |b| b.width()),
        lower_block.as_ref().map_or(0, |b| b.width()),
    ]
    .into_iter()
    .max()
    .unwrap_or(1);

    let op_centered = op_block.center_in(max_width);

    let mut result = if let Some(ub) = &upper_block {
        let ub_centered = ub.center_in(max_width);
        let baseline = ub_centered.height(); // op starts after upper limit
        RenderedBlock::above(&ub_centered, &op_centered, baseline)
    } else {
        op_centered.clone()
    };

    // Baseline at the middle of the operator symbol
    let op_mid = upper_block.as_ref().map_or(0, |b| b.height()) + op_block.height() / 2;

    if let Some(lb) = &lower_block {
        let lb_centered = lb.center_in(max_width);
        result = RenderedBlock::above(&result, &lb_centered, op_mid);
    }

    RenderedBlock::new(result.cells().to_vec(), op_mid)
}

fn layout_accent(body: &EqNode, kind: &AccentKind) -> RenderedBlock {
    let body_block = layout(body);
    let w = body_block.width();

    let accent_block = match kind {
        AccentKind::Bar => {
            // Overline: use ‾ repeated across full width
            RenderedBlock::hline('‾', w)
        }
        AccentKind::Hat => {
            if w <= 1 {
                RenderedBlock::from_char('^')
            } else if w <= 3 {
                center_accent(RenderedBlock::from_text("/\\"), w)
            } else {
                // Wide hat: /‾‾‾\ shape
                let inner = w.saturating_sub(2);
                let hat_str: String = std::iter::once('/')
                    .chain(std::iter::repeat_n('‾', inner))
                    .chain(std::iter::once('\\'))
                    .collect();
                RenderedBlock::from_text(&hat_str)
            }
        }
        AccentKind::Tilde => {
            if w <= 1 {
                RenderedBlock::from_char('~')
            } else {
                // Wide tilde using ˜ repeated or ~ centered
                RenderedBlock::hline('~', w)
            }
        }
        AccentKind::Vec => {
            if w <= 1 {
                RenderedBlock::from_char('→')
            } else {
                // Arrow spanning width: ──→
                let shaft = w.saturating_sub(1);
                let arrow_str: String = std::iter::repeat_n('─', shaft)
                    .chain(std::iter::once('→'))
                    .collect();
                RenderedBlock::from_text(&arrow_str)
            }
        }
        AccentKind::Dot => center_accent(RenderedBlock::from_char('˙'), w),
        AccentKind::DoubleDot => center_accent(RenderedBlock::from_text("¨"), w),
    };

    let baseline = accent_block.height() + body_block.baseline();
    RenderedBlock::above(&accent_block, &body_block, baseline)
}

/// Center a single-glyph accent over a body of width `target`.
///
/// 本地改动（见文件头）：上游此处直接用 [`RenderedBlock::center_in`]，而
/// `center_in` 在余量为奇数时把**多余的那一列放在右侧**，于是 `\hat{abc}`
/// 渲染成 `/\ `（accent 左偏一列）。accent 是单字形，左偏看起来像"没对准"，
/// 所以这里改成多余列放左侧。
fn center_accent(block: RenderedBlock, target: usize) -> RenderedBlock {
    if target <= block.width() {
        return block;
    }
    let total = target - block.width();
    let left = total.div_ceil(2);
    block.pad(left, total - left, 0, 0)
}

fn layout_limit(name: &str, lower: &Option<Box<EqNode>>) -> RenderedBlock {
    let name_block = RenderedBlock::from_text(name);

    if let Some(low) = lower {
        let low_block = layout(low);
        let max_width = name_block.width().max(low_block.width());
        let name_centered = name_block.center_in(max_width);
        let low_centered = low_block.center_in(max_width);
        let baseline = name_centered.height() - 1;
        RenderedBlock::above(&name_centered, &low_centered, baseline)
    } else {
        name_block
    }
}

fn layout_mathfont(kind: &MathFontKind, content: &EqNode) -> RenderedBlock {
    // Extract text and apply Unicode math font mapping
    if let Some(text) = extract_flat_text(content) {
        let mapped = mathfont::map_str(kind, &text);
        RenderedBlock::from_text(&mapped)
    } else {
        // Complex content inside font command — render normally
        layout(content)
    }
}

fn layout_delimited(left: &str, right: &str, content: &EqNode) -> RenderedBlock {
    let content_block = layout(content);
    let h = content_block.height();

    let left_block = build_delimiter(left, h);
    let right_block = build_delimiter(right, h);

    left_block.beside(&content_block).beside(&right_block)
}

/// Build a vertically-scaled delimiter.
///
/// 本地改动（见文件头）：由私有改为 `pub(crate)`，供 `crate::environments`
/// 给"定界符包住多行环境"的形态生成拉伸定界符。逻辑未改。
pub(crate) fn build_delimiter(delim: &str, height: usize) -> RenderedBlock {
    if delim == "." || delim.is_empty() {
        // Invisible delimiter
        return RenderedBlock::new(vec![vec![" ".to_string()]; height], height / 2);
    }

    if height <= 1 {
        return RenderedBlock::new(vec![vec![delim.to_string()]], 0);
    }

    let (top, mid, bot) = match delim {
        "(" => ("⎛", "⎜", "⎝"),
        ")" => ("⎞", "⎟", "⎠"),
        "[" => ("⎡", "⎢", "⎣"),
        "]" => ("⎤", "⎥", "⎦"),
        "{" => ("⎧", "⎨", "⎩"),
        "}" => ("⎫", "⎬", "⎭"),
        "|" => ("│", "│", "│"),
        "‖" => ("‖", "‖", "‖"),
        _ => (delim, delim, delim),
    };

    let mut rows = Vec::with_capacity(height);
    rows.push(vec![top.to_string()]);
    for _ in 1..height.saturating_sub(1) {
        rows.push(vec![mid.to_string()]);
    }
    if height > 1 {
        rows.push(vec![bot.to_string()]);
    }

    RenderedBlock::new(rows, height / 2)
}

fn layout_matrix(kind: &MatrixKind, matrix_rows: &[Vec<EqNode>]) -> RenderedBlock {
    if matrix_rows.is_empty() {
        return RenderedBlock::empty();
    }

    // Render all cells, trimming whitespace from cell content
    let rendered: Vec<Vec<RenderedBlock>> = matrix_rows
        .iter()
        .map(|row| row.iter().map(|cell| layout(&trim_node(cell))).collect())
        .collect();

    let num_cols = rendered.iter().map(|r| r.len()).max().unwrap_or(0);

    // Compute column widths
    let mut col_widths = vec![0usize; num_cols];
    for row in &rendered {
        for (c, cell) in row.iter().enumerate() {
            col_widths[c] = col_widths[c].max(cell.width());
        }
    }

    // Build each matrix row using beside() for proper baseline alignment.
    // Then stack rows vertically with a separator gap.
    let col_sep = 2; // spaces between columns
    let separator = RenderedBlock::from_text(&" ".repeat(col_sep));

    let mut row_blocks: Vec<RenderedBlock> = Vec::new();

    for row in &rendered {
        let mut row_block = RenderedBlock::empty();
        for (c, cell) in row.iter().enumerate() {
            let padded = cell.center_in(col_widths[c]);
            if !row_block.is_empty() {
                row_block = row_block.beside(&separator);
            }
            row_block = row_block.beside(&padded);
        }
        // Pad to fill missing columns
        for w in col_widths.iter().take(num_cols).skip(row.len()) {
            row_block = row_block.beside(&separator);
            row_block = row_block.beside(&RenderedBlock::from_text(&" ".repeat(*w)));
        }
        row_blocks.push(row_block);
    }

    // Stack rows vertically. Each row_block already has correct baselines from beside().
    let grid_width = row_blocks.iter().map(|r| r.width()).max().unwrap_or(0);

    let mut grid = RenderedBlock::empty();
    for row_block in &row_blocks {
        let padded = row_block.center_in(grid_width);
        if grid.is_empty() {
            grid = padded;
        } else {
            let baseline = grid.height() / 2; // intermediate baseline
            grid = RenderedBlock::above(&grid, &padded, baseline);
        }
    }

    // Set baseline to middle of entire grid
    let total_height = grid.height();
    let grid = RenderedBlock::new(grid.cells().to_vec(), total_height / 2);

    // Wrap with delimiters based on matrix kind
    let (left, right) = match kind {
        MatrixKind::Paren => ("(", ")"),
        MatrixKind::Bracket => ("[", "]"),
        MatrixKind::Brace => ("{", "}"),
        MatrixKind::VBar => ("|", "|"),
        MatrixKind::DoubleVBar => ("‖", "‖"),
        MatrixKind::Plain => ("", ""),
    };

    if left.is_empty() {
        grid
    } else {
        let left_d = build_delimiter(left, total_height);
        let right_d = build_delimiter(right, total_height);
        left_d.beside(&grid).beside(&right_d)
    }
}

fn layout_cases(rows: &[(EqNode, Option<EqNode>)]) -> RenderedBlock {
    // Render as a left-brace delimited set of rows
    let rendered: Vec<RenderedBlock> = rows
        .iter()
        .map(|(val, cond)| {
            let val_block = layout(val);
            if let Some(c) = cond {
                let cond_block = layout(c);
                val_block
                    .beside(&RenderedBlock::from_text("  if "))
                    .beside(&cond_block)
            } else {
                val_block
            }
        })
        .collect();

    let max_width = rendered.iter().map(|b| b.width()).max().unwrap_or(0);
    let mut grid = RenderedBlock::empty();
    for row_block in &rendered {
        let padded = RenderedBlock::new(row_block.cells().to_vec(), row_block.baseline());
        // Pad to max width
        let full_row = RenderedBlock::new(
            padded
                .cells()
                .iter()
                .map(|r| {
                    let mut r = r.clone();
                    r.extend(std::iter::repeat_n(
                        " ".to_string(),
                        max_width.saturating_sub(r.len()),
                    ));
                    r
                })
                .collect(),
            padded.baseline(),
        );
        if grid.is_empty() {
            grid = full_row;
        } else {
            grid = RenderedBlock::above(&grid, &full_row, grid.height() / 2);
        }
    }

    let total_height = grid.height();
    let grid = RenderedBlock::new(grid.cells().to_vec(), total_height / 2);
    let left_brace = build_delimiter("{", total_height);
    left_brace.beside(&grid)
}

fn layout_binom(top: &EqNode, bottom: &EqNode) -> RenderedBlock {
    // Render as a fraction with parentheses instead of a bar
    let top_block = layout(top);
    let bot_block = layout(bottom);

    // 本地改动（见文件头）：上游写成
    //   `let baseline = top_centered.height(); above(&top, &bot, baseline - 1)`
    // 有两个问题：
    //   1. `top` 为空块（`\binom{}{}`）时 `height() == 0` → `0 - 1` 下溢 panic（debug）；
    //   2. 堆叠块的基线与定界符块的基线（`h/2`）不一致，`beside` 会把内容与括号
    //      错开一行（`\binom{n}{k}` 画成 `⎛ ⎞` / `⎝n⎠` / ` k`）。
    // 这里改成：先纯上下堆叠（空块自动跳过），再把基线统一放到中线，与定界符对齐。
    if top_block.is_empty() && bot_block.is_empty() {
        return RenderedBlock::empty();
    }

    let inner_width = top_block.width().max(bot_block.width()).max(1);
    let top_centered = top_block.center_in(inner_width);
    let bot_centered = bot_block.center_in(inner_width);

    let stacked = RenderedBlock::above(&top_centered, &bot_centered, 0);
    let h = stacked.height();
    let stacked = RenderedBlock::new(stacked.cells().to_vec(), h / 2);

    let left = build_delimiter("(", h);
    let right = build_delimiter(")", h);
    left.beside(&stacked).beside(&right)
}

fn layout_brace(content: &EqNode, label: &Option<Box<EqNode>>, over: &bool) -> RenderedBlock {
    let content_block = layout(content);
    let w = content_block.width();

    // Build a horizontal brace
    let brace_str = if *over { "⏞" } else { "⏟" };
    let brace_block = RenderedBlock::hline(brace_str.chars().next().unwrap(), w);

    if let Some(lbl) = label {
        let label_block = layout(lbl).center_in(w);
        if *over {
            let top = RenderedBlock::above(&label_block, &brace_block, label_block.height());
            let baseline = top.height() + content_block.baseline();
            RenderedBlock::above(&top, &content_block, baseline)
        } else {
            let bottom = RenderedBlock::above(&brace_block, &label_block, 0);
            let baseline = content_block.baseline();
            RenderedBlock::above(&content_block, &bottom, baseline)
        }
    } else if *over {
        let baseline = brace_block.height() + content_block.baseline();
        RenderedBlock::above(&brace_block, &content_block, baseline)
    } else {
        let baseline = content_block.baseline();
        RenderedBlock::above(&content_block, &brace_block, baseline)
    }
}

fn layout_stackrel(base: &EqNode, annotation: &EqNode, over: &bool) -> RenderedBlock {
    let base_block = layout(base);
    let ann_block = layout(annotation);
    let w = base_block.width().max(ann_block.width());
    let base_centered = base_block.center_in(w);
    let ann_centered = ann_block.center_in(w);

    if *over {
        let baseline = ann_centered.height() + base_block.baseline();
        RenderedBlock::above(&ann_centered, &base_centered, baseline)
    } else {
        let baseline = base_block.baseline();
        RenderedBlock::above(&base_centered, &ann_centered, baseline)
    }
}
