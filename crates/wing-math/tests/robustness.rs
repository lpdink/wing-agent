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

    // 渲染结果里不得混入控制字符（`\n` / `\t` 会破坏网格行契约，review r1 的 B3）
    if let Some(m) = &display {
        assert!(m.width() <= 80, "width budget broken for {src:?}");
        assert_eq!(m.height(), m.lines().len());
        assert!(m.baseline() < m.height());
        for line in m.lines() {
            assert!(
                !line.chars().any(|c| c.is_control()),
                "control char in a grid row for {src:?}: {line:?}"
            );
        }
    }
    if let Some(s) = &inline {
        assert!(!s.contains('\n'), "inline must be single-row: {src:?}");
        assert!(
            !s.chars().any(|c| c.is_control()),
            "control char in inline result: {src:?}"
        );
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

// ── review r1 的 B2 回归：命令 × 参数形态全矩阵不得 panic ──────────────

/// 覆盖上游 `parse_command` 里全部分支的命令名（含不存在命令与空参数陷阱）。
const COMMANDS: &[&str] = &[
    // 参数命令
    "frac",
    "dfrac",
    "tfrac",
    "cfrac",
    "sqrt",
    "binom",
    "hat",
    "bar",
    "overline",
    "dot",
    "ddot",
    "tilde",
    "vec",
    "text",
    "mathbf",
    "mathbb",
    "mathcal",
    "mathrm",
    "mathfrak",
    "mathsf",
    "mathtt",
    "overbrace",
    "underbrace",
    "overset",
    "underset",
    "stackrel",
    "operatorname",
    "label",
    "tag",
    "left",
    "right",
    "begin",
    "end",
    "substack",
    "pmod",
    "ce",
    "unknowncmd",
    // 无参数命令 / 符号
    "alpha",
    "Omega",
    "sum",
    "prod",
    "int",
    "iint",
    "oint",
    "lim",
    "limsup",
    "log",
    "ln",
    "sin",
    "cos",
    "tan",
    "det",
    "max",
    "min",
    "gcd",
    "deg",
    "quad",
    "qquad",
    "infty",
    "partial",
    "nabla",
    "cdot",
    "times",
    "pm",
    "leq",
    "geq",
    "neq",
    "approx",
    "in",
    "subset",
    "rightarrow",
    "to",
    "implies",
    "forall",
    "exists",
    "ldots",
    "cdots",
    "vdots",
    "ddots",
    "colon",
    "mid",
    "angle",
    "emptyset",
    "varnothing",
    "infty",
];

/// 参数形态矩阵：空参数、只给一半、括号参数、可选参数、上下标、星号变体、未闭合。
const ARG_SHAPES: &[&str] = &[
    "", "{}", "{a}", "{ }", "{}{}", "{a}{}", "{}{b}", "{a}{b}", "{}{}{}", "[3]{a}", "[n+1]{x}",
    "_(a)", "^{b}", "_{}^{}", "*(a)", "{", "}",
];

#[test]
fn command_argument_matrix_never_panics() {
    // review r1 的 B2：`\binom{}{}` 曾在 debug 下 `0 - 1` 下溢 panic。
    // 这里把"命令 × 参数形态"全矩阵跑一遍（含刻意畸形的），契约是"绝不 panic"。
    for cmd in COMMANDS {
        // 该命令若"没被渲染"，上游会把它原样兜底成 `\name`——这是可判定的泄漏形状
        let leak_token = format!("\\{cmd}");
        for shape in ARG_SHAPES {
            let src = format!("\\{cmd}{shape}");
            let variants = [
                src.clone(),
                format!("({src})"),
                format!("{src}^{{2}}"),
                format!("x + {src}"),
                format!("\\frac{{{src}}}{{2}}"),
                format!("\\left({src}\\right)"),
                format!("\\begin{{align}} {src} &= b \\end{{align}}"),
            ];
            for variant in &variants {
                exercise(variant);
                // 只要渲染出来了，就不允许把没渲染的命令原样吐进结果
                if let Some(m) = render_display(variant, 200) {
                    let text = m.to_plain_text();
                    assert!(
                        !text.contains(&leak_token),
                        "leaked command {leak_token:?} in {variant:?}:\n{text}"
                    );
                }
            }
        }
    }
}

#[test]
fn binom_with_empty_arguments_is_safe() {
    // B2 的直接回归（debug 构建下曾经 panic）
    for src in [
        r"\binom{}{}",
        r"\binom{}{b}",
        r"\binom{a}{}",
        r"\binom{}",
        r"\binom{}{}{}",
        r"\binom{\frac{}{}}{}",
    ] {
        exercise(src);
    }
    assert_eq!(render_display(r"\binom{}{}", 80), None);
    assert!(render_display(r"\binom{a}{b}", 80).is_some());
}

#[test]
fn control_characters_are_neutralised() {
    // review r1 的 B3：控制字符必须被归零，不能直通网格
    let alphabet = "a \\{}$^_&x\n\t\r\u{0b}\u{0c}\u{00a0}\u{200b}";
    for a in alphabet.chars() {
        for b in alphabet.chars() {
            let src = format!("x {a} y {b} z");
            exercise(&src);
            if let Some(inline) = render_inline(&src) {
                assert!(
                    !inline.chars().any(|c| c.is_control()),
                    "control char survived: {src:?} -> {inline:?}"
                );
            }
            if let Some(m) = render_display(&src, 80) {
                for line in m.lines() {
                    assert!(
                        !line.chars().any(|c| c.is_control()),
                        "control char inside a grid row: {src:?} -> {line:?}"
                    );
                }
            }
        }
    }
}
