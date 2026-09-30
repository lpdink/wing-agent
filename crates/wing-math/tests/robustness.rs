//! 鲁棒性硬要求：**任何输入都不得 panic**，且结果要么满足窄接口契约、要么是 `None`。
//!
//! 用例集合对齐步骤任务书点名的畸形/压力输入（空串、纯空白、未闭合 `\frac{a}{`、
//! `\left(` 不成对、`\begin{unknown}`、200+ 个 `^`、2000 字符参数、`\unknowncmd{1}`、
//! emoji + 零宽字符、`$`、`\\`），量级与实测一致。

use wing_math::{render_block, render_display, render_inline};

/// 三条接口都跑一遍，断言"不 panic + 契约自洽"。
///
/// 三条入口共享同一条管线，所以 `render_block` 是地基：另外两个只可能在它之上更严
/// （行内要求单行、显示要求宽度与预算过关）。
fn exercise(src: &str) {
    let block = render_block(src);
    let display = render_display(src, 80);
    let inline = render_inline(src);

    if display.is_some() {
        assert!(block.is_some(), "display rendered without block: {src:?}");
    }
    if inline.is_some() {
        assert!(block.is_some(), "inline rendered without block: {src:?}");
    }
    if let Some(b) = &block {
        let single_nonblank = b.height() == 1 && !b.cells()[0].concat().trim().is_empty();
        if single_nonblank {
            assert!(
                inline.is_some(),
                "single-row block must render inline: {src:?}"
            );
        }
    }

    // 渲染结果绝不泄漏 LaTeX 命令
    if let Some(m) = &display {
        assert!(!m.to_plain_text().contains('\\'), "leak in {src:?}");
        assert!(m.width() <= 80, "width budget broken for {src:?}");
        assert_eq!(m.height(), m.lines().len());
        assert!(
            m.baseline() < m.height(),
            "baseline {} out of range for height {} in {src:?}",
            m.baseline(),
            m.height()
        );
    }
    if let Some(s) = &inline {
        assert!(!s.contains('\\'), "leak in {src:?}");
        assert!(!s.contains('\n'), "inline must be single-row: {src:?}");
    }
}

#[test]
fn empty_and_whitespace() {
    for src in [
        "", " ", "   ", "\t", "\n", " \t \n ", "\u{00a0}", "\u{200b}",
    ] {
        exercise(src);
    }
    assert_eq!(render_inline(""), None);
    assert_eq!(render_inline("   "), None);
}

#[test]
fn unclosed_and_malformed_constructs() {
    for src in [
        r"\frac{a}{",
        r"\frac{}{}",
        r"\sqrt{",
        r"\left( x + y",
        r"x + y \right)",
        r"\begin{unknown} x \end{unknown}",
        r"\begin{align} a &= b",
        r"\end{matrix}",
        r"{{{x",
        r"x}}}",
        r"(((x",
        r"\hat{",
        r"\left\{",
        r"\left\{ x \right\} y \right)",
        r"\text{unclosed",
        r"\\",
        r"&",
        r"a \\ b",
        r"a & b",
        r"$",
        r"$$",
        r"\(x\)",
        r"\[x\]",
        r"$x$",
        r"$$x$$",
    ] {
        exercise(src);
    }
}

#[test]
fn unknown_commands_never_render_verbatim() {
    for src in [
        r"\unknowncmd{1}",
        r"\ce{2H2O}",
        r"\substack{i \\ j}",
        r"\weird",
        r"\mathbf{\unknown}",
        r"\frac{\unknown}{b}",
    ] {
        exercise(src);
        assert_eq!(render_inline(src), None, "should degrade: {src}");
    }
}

#[test]
fn pathological_repetition() {
    let cases = [
        "^".repeat(200),
        "^".repeat(2000),
        "{".repeat(500),
        "}".repeat(500),
        "\\".repeat(200),
        "\\\\".repeat(200),
        "&".repeat(200),
        "$".repeat(200),
        "_".repeat(200),
        "a".repeat(2000),
        format!(r"\frac{{{}}}{{b}}", "a".repeat(2000)),
        format!(r"\sqrt{{{}}}", "x".repeat(2000)),
        format!(r"\text{{{}}}", "y".repeat(2000)),
        format!(r"\hat{{{}}}", "z".repeat(2000)),
        "x^".repeat(500),
        "\\left(".repeat(200),
        "\\begin{cases}".repeat(100),
    ];
    for src in &cases {
        exercise(src);
    }
}

#[test]
fn deeply_nested_structures_do_not_blow_the_stack() {
    // 任务书量级：200 层花括号 / 100 层 frac+sqrt
    let braces = format!("{}x{}", "{".repeat(200), "}".repeat(200));
    exercise(&braces);

    let mut nested_frac = "x".to_string();
    for _ in 0..100 {
        nested_frac = format!(r"\frac{{{}}}{{1}}", nested_frac);
    }
    exercise(&nested_frac);

    let mut nested_sqrt = "x".to_string();
    for _ in 0..100 {
        nested_sqrt = format!(r"\sqrt{{{}}}", nested_sqrt);
    }
    exercise(&nested_sqrt);
}

#[test]
fn unicode_and_control_characters() {
    for src in [
        "🎉 + 🔥 = 💯",
        "数学 x^2",
        "α + β = γ",
        "a\u{200b}b",
        "a\u{00ad}b",
        "x\u{0301}",
        "\u{fffd}",
        "\u{1f600}^2",
        "e\u{0302}",
    ] {
        exercise(src);
    }
}

#[test]
fn real_world_expressions_still_render() {
    // 这些必须能渲染（否则说明拒绝判据过严）
    for src in [
        r"x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}",
        r"e^{i\pi} + 1 = 0",
        r"\int_{-\infty}^{\infty} e^{-x^2} dx = \sqrt{\pi}",
        r"\sum_{n=0}^{\infty} \frac{f^{(n)}(a)}{n!}(x-a)^n",
        r"\det \begin{pmatrix} a & b \\ c & d \end{pmatrix} = ad - bc",
        r"\lim_{h \to 0} \frac{f(x+h) - f(x)}{h}",
        r"(x+y)^n = \sum_{k=0}^{n} \binom{n}{k} x^{n-k} y^k",
        r"\nabla \times \vec{E} = -\frac{\partial \vec{B}}{\partial t}",
        r"i\hbar \frac{\partial}{\partial t} \Psi = \hat{H} \Psi",
        r"P(A \mid B) = \frac{P(B \mid A) P(A)}{P(B)}",
        r"\mathcal{L}(\theta) = \frac{1}{N}\sum_{i=1}^{N} \log p(x_i \mid \theta)",
        r"f(x) = \begin{cases} x^2 & x > 0 \\ 0 & x \le 0 \end{cases}",
        r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}^{-1}",
        r"\bar{x} \pm \sigma, \quad p < 0.05",
        r"\underset{x}{\min} f(x)",
        r"\overbrace{a+b}^{n}",
        r"\left( \frac{a}{b} \right)^2",
        r"\left\{ x \in \mathbb{R} \mid x > 0 \right\}",
        r"\begin{align} a &= b + c \\ d &= e \end{align}",
    ] {
        let block = render_block(src);
        assert!(block.is_some(), "must be renderable: {src}");
        let m = render_display(src, 120).unwrap();
        assert!(!m.to_plain_text().contains('\\'), "leak in {src}");
    }
}

#[test]
fn no_panic_on_random_byte_soup() {
    // 确定性伪随机：拼接所有"危险"字符
    let alphabet: Vec<char> = "\\{}$^_&+-*/()[]|<>=.,;:!?'\"`~ abcxyz019αβ∑∫"
        .chars()
        .collect();
    let mut state: u64 = 0x9E3779B97F4A7C15;
    for _ in 0..400 {
        let mut src = String::new();
        for _ in 0..24 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            src.push(alphabet[(state >> 33) as usize % alphabet.len()]);
        }
        exercise(&src);
    }
}
