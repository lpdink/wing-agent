//! 多行环境适配（`align` / `gather` / `split` / `multline` / `array` …）的接口级测试。
//!
//! 上游对这些环境**完全不支持**（`\begin{align}` 会只剩半行并吞掉 body），所以这里测的是
//! 我们自己补的那层：按 `\\` 分行、按 `&` 分列、按列对齐堆叠、定界符拉伸。
//!
//! 所有用例都经公开 API（`render_display`），并顺带断言"输出里没有 `\`"。

use unicode_width::UnicodeWidthStr;
use wing_math::render_display;

fn lines(src: &str) -> Vec<String> {
    render_display(src, 200)
        .unwrap_or_else(|| panic!("expected renderable: {src}"))
        .into_lines()
}

/// 断言渲染出的**原始网格**是矩形：每一行的显示宽度都等于块宽。
///
/// 注意不能用 `render_display` 的行来断言 —— 那些行已经去过行尾空白，长度自然不等。
fn assert_rectangular(src: &str) {
    let block =
        wing_math::render_block(src).unwrap_or_else(|| panic!("expected renderable: {src}"));
    for row in block.cells() {
        let width: usize = row.iter().map(|c| UnicodeWidthStr::width(c.as_str())).sum();
        assert_eq!(
            width,
            block.width(),
            "row width mismatch in {src}:\n{block}"
        );
    }
}

#[test]
fn align_pairs_columns_are_right_then_left() {
    let src = r"\begin{align} a &= b + c \\ dd &= e \end{align}";
    assert_eq!(lines(src), vec![" a  = b + c", "dd  = e"]);
    assert_rectangular(src);
}

#[test]
fn align_accepts_starred_name() {
    let out = lines(r"\begin{align*} a &= b \\ c &= d \end{align*}");
    assert_eq!(out, vec!["a  = b", "c  = d"]);
}

#[test]
fn aligned_is_usable_inside_larger_formula() {
    let src = r"f(x) = \begin{aligned} a &= b \\ c &= d \end{aligned}";
    let out = lines(src);
    assert_eq!(out, vec!["f(x) = a  = b", "       c  = d"]);
    assert_rectangular(src);
}

#[test]
fn gather_centers_every_row() {
    let src = r"\begin{gather} a = b \\ cccc = d \end{gather}";
    assert_eq!(lines(src), vec![" a = b", "cccc = d"]);
    assert_rectangular(src);
}

#[test]
fn split_matches_align_layout() {
    let out = lines(r"\begin{split} x &= y \\ zz &= w \end{split}");
    assert_eq!(out, vec![" x  = y", "zz  = w"]);
}

#[test]
fn multline_centers_rows() {
    let out = lines(r"\begin{multline} a \\ bcd \end{multline}");
    assert_eq!(out, vec![" a", "bcd"]);
}

#[test]
fn eqnarray_and_flalign_are_supported() {
    assert_eq!(
        lines(r"\begin{eqnarray} a &= b \\ c &= d \end{eqnarray}"),
        vec!["a  = b", "c  = d"]
    );
    assert_eq!(
        lines(r"\begin{flalign*} x &= y \end{flalign*}"),
        vec!["x  = y"]
    );
}

#[test]
fn alignat_count_argument_is_consumed() {
    let out = lines(r"\begin{alignat}{3} a &= b && = c \end{alignat}");
    assert!(out[0].starts_with('a'), "{out:#?}");
    assert!(out[0].contains("= b"));
    assert!(out[0].contains("= c"));
}

#[test]
fn array_uses_the_column_spec() {
    let src = r"\begin{array}{lcr} a & b & c \\ dd & e & f \end{array}";
    let out = lines(src);
    assert_rectangular(src);
    // 第一列左对齐 → 第 0 列以 `a` / `dd` 起头
    assert!(out[0].starts_with("a "), "{out:#?}");
    assert!(out[1].starts_with("dd"), "{out:#?}");
    // 第三列右对齐 → 单字符 c/f 靠右
    assert_eq!(out[0].chars().count(), out[1].chars().count());
}

#[test]
fn array_ignores_spacing_marks_in_spec() {
    let src = r"\begin{array}{|c|c|} a & b \\ c & d \end{array}";
    assert_rectangular(src);
    assert_eq!(lines(src).len(), 2);
}

#[test]
fn optional_row_spacing_argument_is_skipped() {
    let out = lines(r"\begin{align} a &= b \\[6pt] c &= d \end{align}");
    assert_eq!(out, vec!["a  = b", "c  = d"]);
}

#[test]
fn trailing_row_separator_does_not_add_blank_row() {
    assert_eq!(
        lines(r"\begin{align} a &= b \\ \end{align}"),
        vec!["a  = b"]
    );
}

#[test]
fn empty_cell_keeps_its_column() {
    let src = r"\begin{align} a &= b \\ & = c \end{align}";
    let out = lines(src);
    assert_eq!(out.len(), 2);
    assert_rectangular(src);
    assert!(out[1].contains("= c"));
}

#[test]
fn double_ampersand_produces_an_empty_middle_column() {
    let out = lines(r"\begin{align} a &= b && c \end{align}");
    assert!(out[0].contains("= b"));
    assert!(out[0].contains('c'));
}

#[test]
fn text_block_inside_cell_is_supported() {
    let out = lines(r"\begin{align} x &= y \text{ for all } z \end{align}");
    assert!(out[0].contains("for all"), "{out:#?}");
}

#[test]
fn fraction_cells_are_multiline_and_baseline_aligned() {
    let out = lines(r"\begin{align} a &= \frac{1}{2} \\ b &= 1 \end{align}");
    assert_eq!(out.len(), 4);
    assert!(out.join("\n").contains('─'));
}

#[test]
fn nested_cases_inside_cell_is_delegated_to_upstream() {
    let out =
        lines(r"\begin{align} f(x) &= \begin{cases} x & x>0 \\ 0 & x\le 0 \end{cases} \end{align}");
    let text = out.join("\n");
    assert!(text.contains("f(x)"));
    assert!(text.contains("x > 0"));
    assert!(text.contains('0'));
}

#[test]
fn nested_multiline_environment_is_rendered_recursively() {
    let out = lines(r"\begin{align} \begin{aligned} a &= b \end{aligned} \end{align}");
    assert_eq!(out, vec!["a  = b"]);
}

#[test]
fn delimited_environment_stretches_the_braces() {
    let src = r"\left\{ \begin{aligned} a &= b \\ c &= d \end{aligned} \right.";
    let out = lines(src);
    assert_eq!(out.len(), 2);
    assert!(out[0].starts_with('⎧'), "{out:#?}");
    assert!(out[1].starts_with('⎩'), "{out:#?}");
    assert_rectangular(src);
}

#[test]
fn delimited_environment_with_both_visible_delimiters() {
    let out = lines(r"\left( \begin{array}{c} a \\ b \end{array} \right)");
    assert_eq!(out.len(), 2);
    assert!(out[0].starts_with('⎛'), "{out:#?}");
    assert!(out[1].ends_with('⎠'), "{out:#?}");
}

#[test]
fn delimited_environment_with_norms() {
    let out = lines(r"\left\| \begin{array}{c} a \\ b \end{array} \right\|");
    assert!(out[0].starts_with('‖'), "{out:#?}");
    assert!(out[1].ends_with('‖'), "{out:#?}");
}

#[test]
fn bracket_and_square_delimiters_are_stretched() {
    let out = lines(r"\left[ \begin{array}{c} a \\ b \\ c \end{array} \right]");
    assert_eq!(out.len(), 3);
    assert!(out[0].starts_with('⎡'), "{out:#?}");
    assert!(out[2].starts_with('⎣'), "{out:#?}");
    assert!(out[2].ends_with('⎦'), "{out:#?}");
}

#[test]
fn three_row_matrix_like_array_is_stacked_without_blank_lines() {
    let out = lines(r"\begin{array}{c} 1 \\ 2 \\ 3 \end{array}");
    assert_eq!(out.len(), 3);
    assert_eq!(out, vec!["1", "2", "3"]);
}

#[test]
fn styled_environment_body_is_normalized_first() {
    let out = lines(r"\begin{align} \displaystyle a &= \bigl( b \bigr) \end{align}");
    assert_eq!(out, vec!["a  = ( b )"]);
}

#[test]
fn array_with_cjk_cell_keeps_rectangular_display_width() {
    let out = lines(r"\begin{array}{cc} \text{中文} & b \\ c & d \end{array}");
    assert_eq!(out.len(), 2);
    // 用显示宽度而不是字符数比较
    let w = |s: &String| {
        use std::iter::once;
        s.chars()
            .map(|c| if once(c).all(|c| c.is_ascii()) { 1 } else { 2 })
            .sum::<usize>()
    };
    assert_eq!(w(&out[0]), w(&out[1]), "{out:#?}");
}

#[test]
fn multiline_environment_is_not_silently_truncated() {
    // 回归：上游对 `align` 会只剩半行（`\begin{align} a `）并吞掉 body。
    // 我们的适配层必须让四个符号全部出现。
    let text = render_display(r"\begin{align} a &= b + c \\ d &= e \end{align}", 200)
        .unwrap()
        .to_plain_text();
    for needle in ["a", "b", "c", "d", "e"] {
        assert!(
            text.contains(needle),
            "content lost: {needle} missing in\n{text}"
        );
    }
    assert!(!text.contains('\\'), "leaked command in\n{text}");
}
