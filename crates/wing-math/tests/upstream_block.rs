//! [`RenderedBlock`] 组合原语的不变量测试。
//!
//! 上游 `term-maths` 只在 `rendered_block.rs` 里测了 `from_char` / `from_text` /
//! `center_in` 等 happy path，缺的是**组合原语的不变量**（宽度守恒、基线守恒、宽字符
//! 行的显示宽度一致）。我们的多行环境适配完全建立在这些原语上，所以把这些不变量钉住。

use unicode_width::UnicodeWidthStr;
use wing_math::RenderedBlock;

/// 一行的显示宽度（按 `unicode-width`，CJK 宽字符算 2 列）。
fn row_width(row: &[String]) -> usize {
    row.iter().map(|c| UnicodeWidthStr::width(c.as_str())).sum()
}

/// 断言块是矩形：每一行的显示宽度都等于块宽。
fn assert_rectangular(block: &RenderedBlock) {
    for row in block.cells() {
        assert_eq!(
            row_width(row),
            block.width(),
            "row is not block-width:\n{block}"
        );
    }
}

#[test]
fn constructors_report_consistent_dimensions() {
    let b = RenderedBlock::from_char('x');
    assert_eq!((b.width(), b.height(), b.baseline()), (1, 1, 0));
    assert_rectangular(&b);

    let b = RenderedBlock::from_text("hello");
    assert_eq!((b.width(), b.height(), b.baseline()), (5, 1, 0));
    assert_rectangular(&b);

    let b = RenderedBlock::empty();
    assert_eq!((b.width(), b.height()), (0, 0));
    assert!(b.is_empty());

    let b = RenderedBlock::hline('─', 7);
    assert_eq!((b.width(), b.height()), (7, 1));
    assert_eq!(format!("{b}"), "───────");
}

#[test]
fn beside_preserves_width_and_baseline() {
    let left = RenderedBlock::new(
        vec![
            vec!["a".to_string()],
            vec!["b".to_string()],
            vec!["c".to_string()],
        ],
        1,
    );
    let right = RenderedBlock::from_text("XY");
    let joined = left.beside(&right);

    assert_eq!(joined.width(), left.width() + right.width());
    assert_eq!(joined.height(), 3);
    assert_eq!(joined.baseline(), 1);
    assert_rectangular(&joined);
    // 右侧块落在基线上
    assert_eq!(format!("{joined}"), "a  \nbXY\nc  ");
}

#[test]
fn beside_with_empty_block_is_identity() {
    let b = RenderedBlock::from_text("abc");
    let joined = b.beside(&RenderedBlock::empty());
    assert_eq!(joined.width(), b.width());
    assert_eq!(format!("{joined}"), "abc");
    // 注意：空的**左侧**块会被丢弃（上游语义），所以空列必须显式补空格
    let joined = RenderedBlock::empty().beside(&b);
    assert_eq!(joined.width(), b.width());
}

#[test]
fn beside_with_blank_block_keeps_the_column() {
    // 多行环境里空单元格靠这个行为占住列宽
    let blank = RenderedBlock::from_text("   ");
    let b = RenderedBlock::from_text("x");
    let joined = blank.beside(&b);
    assert_eq!(joined.width(), 4);
    assert_eq!(format!("{joined}"), "   x");
}

#[test]
fn above_aligns_widths_and_keeps_row_count() {
    let top = RenderedBlock::from_text("abc");
    let bottom = RenderedBlock::from_text("de");
    let stacked = RenderedBlock::above(&top, &bottom, 0);
    assert_eq!(stacked.height(), top.height() + bottom.height());
    assert_eq!(stacked.width(), 3);
    assert_rectangular(&stacked);
    assert_eq!(format!("{stacked}"), "abc\nde ");
}

#[test]
fn pad_grows_both_dimensions_and_shifts_the_baseline() {
    let b = RenderedBlock::from_char('x');
    let padded = b.pad(2, 3, 1, 4);
    assert_eq!((padded.width(), padded.height()), (6, 6));
    assert_eq!(padded.baseline(), 1);
    assert_rectangular(&padded);
}

#[test]
fn center_in_is_a_no_op_when_too_narrow() {
    let b = RenderedBlock::from_text("abcd");
    let centered = b.center_in(2);
    assert_eq!(centered.width(), 4);
    assert_eq!(format!("{centered}"), "abcd");
}

#[test]
fn center_in_never_shrinks_below_content() {
    for width in 1..12 {
        let b = RenderedBlock::from_text("abc");
        let centered = b.center_in(width);
        assert!(centered.width() >= b.width());
        assert_rectangular(&centered);
    }
}

#[test]
fn wide_characters_keep_display_width_accounting() {
    // `unicode-width` 视角：CJK 是 2 列；块宽必须按显示宽度算
    let cjk = RenderedBlock::from_text("中文");
    assert_eq!(cjk.width(), 4);
    assert_eq!(cjk.cells()[0].len(), 2, "cells 是格子而不是列");

    // 与 ASCII 组合后仍保持矩形
    let joined = cjk.beside(&RenderedBlock::from_text("x"));
    assert_eq!(joined.width(), 5);
    assert_rectangular(&joined);
}

#[test]
fn wide_character_rows_stay_rectangular_after_stacking() {
    let wide = RenderedBlock::from_text("中文");
    let narrow = RenderedBlock::from_text("abcd");
    let stacked = RenderedBlock::above(&wide, &narrow, 0);
    assert_eq!(stacked.width(), 4);
    assert_rectangular(&stacked);
}

#[test]
fn display_impl_joins_rows_with_newline() {
    let stacked = RenderedBlock::above(
        &RenderedBlock::from_text("ab"),
        &RenderedBlock::from_text("cd"),
        0,
    );
    assert_eq!(format!("{stacked}"), "ab\ncd");
}

#[test]
fn engine_output_is_rectangular() {
    // 端到端：引擎产出的网格也必须是矩形（多行环境会自己做补齐）
    for src in [
        r"\frac{a}{b}",
        r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
        r"\begin{align} a &= b + c \\ dd &= e \end{align}",
        r"\int_0^\infty e^{-x^2} dx",
        r"\left\{ \begin{aligned} a &= b \\ c &= d \end{aligned} \right.",
        r"\begin{array}{lcr} a & b & c \\ dd & e & f \end{array}",
    ] {
        let block = wing_math::render_block(src).unwrap_or_else(|| panic!("expected Some: {src}"));
        assert_rectangular(&block);
    }
}
