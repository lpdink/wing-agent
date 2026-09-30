//! 窄接口语义测试：把 `design.md → D2` 的 `None` 表格逐行钉住。
//!
//! 这些测试只用公开 API（`render_inline` / `render_display` / `render_block`）。任何一条
//! `None` 都是"上层应降级为源码字面量"的契约；任何一条 `Some` 都必须满足
//! "输出里不含 `\`" 这条硬保证。

use wing_math::{render_block, render_display, render_inline};

/// `Some` 的硬保证：渲染结果里绝不出现反斜杠（没有把没渲染的东西原样吐出来）。
fn assert_no_leak(text: &str) {
    assert!(
        !text.contains('\\'),
        "rendered output leaked a LaTeX command: {text:?}"
    );
}

// ── `Some` 侧：能渲染的必须真的能渲染 ────────────────────────────────

#[test]
fn inline_single_row_formulas() {
    for (src, expected) in [
        (r"x^2 + y^2", "x² + y²"),
        (r"\alpha + \beta = \gamma", "α + β = γ"),
        (r"a_n", "aₙ"),
        (r"\mathbb{R}^n", "ℝⁿ"),
        (r"\mathcal{L}", "ℒ"),
        (r"E = mc^2", "E = mc²"),
    ] {
        let got = render_inline(src).unwrap_or_else(|| panic!("expected Some for {src}"));
        assert_eq!(got, expected, "inline render mismatch for {src}");
        assert_no_leak(&got);
    }
}

#[test]
fn display_formulas_are_grids() {
    let m = render_display(r"\frac{a}{b}", 40).unwrap();
    assert_eq!(m.lines(), [" a", "───", " b"]);
    assert_eq!(m.height(), 3);
    assert_eq!(m.width(), 3);
    assert!(!m.to_plain_text().is_empty());

    let m = render_display(r"\sum_{i=1}^{n} i = \frac{n(n+1)}{2}", 40).unwrap();
    assert!(m.height() >= 3);
    assert!(m.to_plain_text().contains('∑'));
    assert_no_leak(&m.to_plain_text());
}

#[test]
fn display_width_is_display_width_not_char_count() {
    // 显示宽度与字符数在纯 ASCII 下一致
    let m = render_display(r"a + b", 40).unwrap();
    assert_eq!(m.width(), m.lines()[0].chars().count());
}

#[test]
fn block_is_raw_grid() {
    let b = render_block(r"\frac{a}{b}").unwrap();
    assert_eq!((b.width(), b.height()), (3, 3));
    assert_eq!(b.baseline(), 1);
}

// ── `None` 侧：D2 表格逐行 ──────────────────────────────────────────

#[test]
fn none_for_empty_input() {
    assert_eq!(render_inline(""), None);
    assert_eq!(render_inline("   "), None);
    assert_eq!(render_display("", 40), None);
    assert_eq!(render_display("\t\n ", 40), None);
    assert!(render_block("").is_none());
}

#[test]
fn none_for_top_level_row_separator() {
    // 上游会静默截断（只剩 `a`）——必须拒绝，绝不能返回半截内容
    assert_eq!(render_inline(r"a \\ b"), None);
    assert_eq!(render_display(r"a \\ b", 80), None);
}

#[test]
fn none_for_top_level_column_separator() {
    assert_eq!(render_inline(r"a & b"), None);
    assert_eq!(render_display(r"a & b", 80), None);
}

#[test]
fn none_for_unbalanced_environment() {
    assert_eq!(render_inline(r"\begin{matrix} a"), None);
    assert_eq!(render_inline(r"\end{matrix}"), None);
    assert_eq!(render_inline(r"\begin{cases} a & b"), None);
}

#[test]
fn none_for_unbalanced_delimiters() {
    assert_eq!(render_display(r"\left( x + y", 80), None);
    assert_eq!(render_display(r"x + y \right)", 80), None);
    // 半开区间（`\left\{ a \right.`）是合法的，左右计数一致 → 允许渲染
    let m = render_display(r"\left\{ a \right.", 80).unwrap();
    assert!(m.to_plain_text().contains('a'));
}

#[test]
fn none_for_multiple_top_level_environments() {
    assert_eq!(
        render_display(
            r"\begin{cases} a \end{cases} \begin{cases} b \end{cases}",
            80
        ),
        None
    );
}

#[test]
fn none_for_unsupported_commands() {
    // 上游会把没渲染的命令原样吐出来 → 必须整条降级
    for src in [
        r"\ce{2H2O}",
        r"\unknowncmd{1}",
        r"\substack{i \\ j}",
        r"a \pmod{n}",
        r"\widehat{abc}",
    ] {
        assert_eq!(render_inline(src), None, "should degrade: {src}");
    }
}

#[test]
fn none_for_multiline_inline_formula() {
    for src in [
        r"\frac{a}{b}",
        r"\sum_{i=1}^{n}",
        r"\sqrt{\frac{a}{b}}",
        r"\hat{x}",
        r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
        r"x^{\frac{1}{2}}",
    ] {
        assert_eq!(render_inline(src), None, "inline must be single-row: {src}");
    }
}

#[test]
fn none_when_wider_than_max_width() {
    let src = r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}";
    let m = render_display(src, 200).unwrap();
    let w = m.width();
    assert!(render_display(src, w).is_some(), "exact width must fit");
    assert_eq!(
        render_display(src, w - 1),
        None,
        "one column short must fail"
    );
    assert_eq!(render_display(src, 0), None);
}

#[test]
fn none_for_trailing_backslash() {
    assert_eq!(render_inline(r"x + \"), None);
}

// ── 归一化在接口上生效 ──────────────────────────────────────────────

#[test]
fn normalization_is_visible_through_the_api() {
    // N1：多余的外壳
    assert_eq!(render_inline("$$x^2$$").as_deref(), Some("x²"));
    // N2/N3/N4
    assert_eq!(render_inline(r"\displaystyle x").as_deref(), Some("x"));
    assert!(render_display(r"\int\limits_0^1 f", 40).is_some());
    assert_eq!(render_inline(r"\bigl( x \bigr)").as_deref(), Some("( x )"));
    // N5
    let a = render_inline(r"\dfrac{a}{b}");
    let b = render_inline(r"\frac{a}{b}");
    assert_eq!(a, b);
    // N7 / N8
    assert_eq!(
        render_inline(r"\operatorname{softmax}").as_deref(),
        Some("softmax")
    );
    assert_eq!(render_inline(r"f\colon A").as_deref(), Some("f: A"));
    // N9
    assert_eq!(render_inline(r"a \label{eq:1}").as_deref(), Some("a"));
}

#[test]
fn left_brace_set_is_renderable() {
    // 上游缺陷「`\left\{…\right\}` 不成对」在归一化后被修好：
    // 定界符会被拉伸成 ⎧⎨⎩ / ⎫⎬⎭ 并按内容高度对齐
    let m = render_display(r"\left\{ \frac{a}{b} \right\}", 40).unwrap();
    let text = m.to_plain_text();
    assert!(text.contains('⎧'), "missing stretched left brace: {text}");
    assert!(text.contains('⎩'), "missing stretched left brace: {text}");
    assert!(text.contains('⎫'), "missing stretched right brace: {text}");
    assert!(text.contains('⎭'), "missing stretched right brace: {text}");
    assert!(text.contains('─'), "missing fraction bar: {text}");
    assert_no_leak(&text);
}

#[test]
fn nth_root_is_renderable() {
    let text = render_display(r"\sqrt[3]{x}", 40)
        .unwrap()
        .into_lines()
        .join("\n");
    assert!(text.contains('³'), "missing cube index: {text}");
    assert!(text.contains('√'), "missing radical: {text}");
    assert_no_leak(&text);
}

// ── 内容不丢失（"绝不静默吞内容"的正面断言）────────────────────────────

#[test]
fn some_results_keep_the_intended_symbols() {
    fn visible(m: &wing_math::RenderedMath) -> String {
        m.to_plain_text()
    }

    let cases: &[(&str, &[&str])] = &[
        (r"\frac{a}{b}", &["a", "b", "─"]),
        (
            r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}",
            &["b", "√", "4ac", "2a"],
        ),
        (r"\int_0^\infty e^{-x^2} dx", &["e", "x", "d"]),
        (r"\sum_{i=1}^{n} i", &["∑", "i", "n"]),
        (
            r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
            &["a", "b", "c", "d"],
        ),
        (
            r"\begin{cases} x^2 & x>0 \\ 0 & x\le 0 \end{cases}",
            &["x", "0"],
        ),
        (
            r"\begin{align} a &= b \\ c &= d \end{align}",
            &["a", "b", "c", "d", "="],
        ),
        (r"\mathcal{L}(\theta)", &["ℒ", "θ"]),
        (r"\nabla \cdot \vec{E}", &["∇", "E"]),
    ];

    for (src, needles) in cases {
        let m = render_display(src, 200).unwrap_or_else(|| panic!("expected Some for {src}"));
        let text = visible(&m);
        for needle in *needles {
            assert!(
                text.contains(needle),
                "{src}: missing {needle:?} in\n{text}"
            );
        }
        assert_no_leak(&text);
    }
}
