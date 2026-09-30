//! 质量样例（步骤任务书点名的那 10 条公式）的**内容断言**。
//!
//! 人肉可读的实际渲染效果见 `examples/render_samples.rs`（`cargo run -p wing-math
//! --example render_samples`）。这里钉的是"内容不丢失"这条硬线：
//! 只要返回 `Some`，原作者意图里的主体符号就必须出现；做不到就应该是 `None`。

use wing_math::{render_display, render_inline};

/// 步骤任务书点名的 10 条样例。
const SAMPLES: &[(&str, &[&str])] = &[
    (
        r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}",
        &["b", "±", "√", "4ac", "2a", "─"],
    ),
    (
        r"\int_0^\infty e^{-x^2} dx = \frac{\sqrt{\pi}}{2}",
        &["e", "x", "d", "π", "─"],
    ),
    (
        r"\sum_{i=1}^{n} i = \frac{n(n+1)}{2}",
        &["∑", "i", "n", "1", "2"],
    ),
    (
        r"\mathcal{L}(\theta) = \frac{1}{N}\sum_{i=1}^{N} \log p(x_i|\theta)",
        &["ℒ", "θ", "N", "log", "p", "x"],
    ),
    (
        r"\begin{align} a &= b + c \\ d &= e \end{align}",
        &["a", "b", "c", "d", "e", "="],
    ),
    (
        r"f(x) = \begin{cases} x^2 & x>0 \\ 0 & x\le 0 \end{cases}",
        &["f(x)", "x²", "0", "if"],
    ),
    (
        r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}^{-1}",
        &["a", "b", "c", "d", "⁻¹"],
    ),
    (
        r"\nabla \cdot \vec{E} = \frac{\rho}{\epsilon_0}",
        &["∇", "E", "ρ", "ε", "₀"],
    ),
    (r"\hat{H}\psi = E\psi", &["H", "ψ", "E", "^"]),
    (
        r"\bar{x} \pm \sigma, \quad p < 0.05",
        &["x", "±", "σ", "p", "0.05"],
    ),
];

#[test]
fn every_sample_renders_and_keeps_its_symbols() {
    for (src, needles) in SAMPLES {
        let m = render_display(src, 120).unwrap_or_else(|| panic!("sample must render: {src}"));
        let text = m.to_plain_text();
        for needle in *needles {
            assert!(
                text.contains(needle),
                "sample {src} lost {needle:?}; rendered:\n{text}"
            );
        }
        assert!(
            !text.contains('\\'),
            "sample {src} leaked a command:\n{text}"
        );
        assert!(m.width() <= 120, "sample {src} exceeded width budget");
        assert!(m.height() >= 1);
    }
}

#[test]
fn samples_are_not_silently_truncated() {
    // `align` 家族曾在实测里"只剩半行"：这里显式比对首尾符号
    let text = render_display(r"\begin{align} a &= b + c \\ d &= e \end{align}", 120)
        .unwrap()
        .to_plain_text();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.lines().next().unwrap().contains('a'));
    assert!(text.lines().next().unwrap().contains("b + c"));
    assert!(text.lines().nth(1).unwrap().contains('d'));
    assert!(text.lines().nth(1).unwrap().contains('e'));
}

#[test]
fn inline_samples_are_single_line() {
    for src in [
        r"x^2 + y^2 = z^2",
        r"E = mc^2",
        r"\alpha + \beta = \gamma",
        r"\mathbb{R}^n \subset \mathbb{C}",
        r"p < 0.05",
        r"\mathcal{O}(n \log n)",
    ] {
        let s = render_inline(src).unwrap_or_else(|| panic!("must render inline: {src}"));
        assert!(!s.contains('\n'));
        assert!(!s.contains('\\'));
    }
}

#[test]
fn samples_have_expected_shapes() {
    // 形状断言（不锁死具体排版，只锁"是几行、有没有该有的结构"）
    let frac = render_display(r"\frac{a}{b}", 40).unwrap();
    assert_eq!(frac.height(), 3, "分式应当 3 行（分子 / 横线 / 分母）");

    let sqrt = render_display(r"\sqrt{x}", 40).unwrap();
    assert_eq!(sqrt.height(), 2, "单行根号应当 2 行（上划线 + 根号体）");

    let sum = render_display(r"\sum_{i=1}^{n}", 40).unwrap();
    assert!(sum.height() >= 3, "带上下限的求和至少 3 行");

    let matrix = render_display(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}", 40).unwrap();
    assert_eq!(matrix.height(), 2, "2x2 矩阵应当 2 行");

    let cases = render_display(r"\begin{cases} x & x>0 \\ 0 & x\le 0 \end{cases}", 40).unwrap();
    assert_eq!(cases.height(), 2, "两行 cases 应当 2 行");

    let align = render_display(r"\begin{align} a &= b \\ c &= d \end{align}", 40).unwrap();
    assert_eq!(align.height(), 2, "两行 align 应当 2 行");
}
