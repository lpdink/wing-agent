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

// ── review r1 的 B/S 回归 ───────────────────────────────────────────

#[test]
fn none_when_a_cell_fails_to_render() {
    // B1：单元格渲染失败绝不能退化成空格（内容静默丢失），必须整条降级
    for src in [
        r"\begin{align} a &= \ce{2H2O} \\ c &= d \end{align}",
        r"\begin{align} \unknowncmd{1} &= b \end{align}",
        r"\begin{align} a &= \widehat{xyz} \\ c &= d \end{align}",
        r"\begin{align} a &= b \substack{i} \end{align}",
        r"\begin{aligned} x &= \frac{a}{b} \\ y &= \pmod{n} \end{aligned}",
    ] {
        assert_eq!(render_display(src, 400), None, "should degrade: {src}");
        assert!(render_block(src).is_none(), "should degrade: {src}");
    }
}

#[test]
fn none_when_prefix_or_suffix_fails_to_render() {
    // B1：前后缀渲染失败同样必须整条降级（之前会静默丢掉前缀）
    for src in [
        r"\ce{2H2O} \begin{aligned} a &= b \end{aligned}",
        r"\begin{aligned} a &= b \end{aligned} \unknowncmd{x}",
        r"\widehat{xyz} \begin{array}{c} a \end{array}",
    ] {
        assert_eq!(render_display(src, 400), None, "should degrade: {src}");
    }
}

#[test]
fn none_when_environment_nesting_exceeds_the_budget() {
    // B1 的最恶性形态：超深嵌套原来会返回一个空行（公式整体消失）
    let deep = |levels: usize| {
        format!(
            "{}a &= b{}",
            r"\begin{aligned} ".repeat(levels),
            r" \end{aligned}".repeat(levels)
        )
    };
    assert_eq!(render_display(&deep(9), 400), None);
    assert_eq!(render_display(&deep(12), 400), None);
    // 预算之内的嵌套照常工作
    let m = render_display(&deep(3), 400).unwrap();
    assert!(m.to_plain_text().contains("a  = b"));
}

#[test]
fn control_characters_never_break_the_row_contract() {
    // B3：`\n` / `\t` 归一化成空格；单行与网格行契约必须成立
    assert_eq!(render_inline("a\nb").as_deref(), Some("a b"));
    assert_eq!(render_inline("x + \tb").as_deref(), Some("x + b"));
    assert_eq!(render_inline("a\r\nb").as_deref(), Some("a b"));

    for src in [
        "a\nb",
        "x + \tb",
        "x = \\frac{-b \\pm \\sqrt{b^2-4ac}}\n{2a}",
        "\\int_0^\\infty\ne^{-x^2} dx = \\frac{\\sqrt{\\pi}}{2}",
        "f(x) = \\begin{cases}\nx^2 & x > 0 \\\\\n0 & x \\le 0\n\\end{cases}",
        "$$\nx + 1\n$$",
    ] {
        // 行内结果必须单行、无控制字符
        if let Some(text) = render_inline(src) {
            assert!(
                !text.contains('\n'),
                "newline survived in {src:?} -> {text:?}"
            );
            assert!(!text.contains('\t'), "tab survived in {src:?} -> {text:?}");
            assert!(
                !text.chars().any(|c| c.is_control()),
                "control char survived in {src:?}"
            );
        }
        // 显示结果：每个网格行内部不得含控制字符，且行数账目自洽
        if let Some(m) = render_display(src, 400) {
            for line in m.lines() {
                assert!(
                    !line.chars().any(|c| c.is_control()),
                    "control char inside a grid row for {src:?}: {line:?}"
                );
            }
            assert_eq!(
                m.height(),
                m.lines().len(),
                "height must match the number of grid rows for {src:?}"
            );
            assert_eq!(
                m.to_plain_text().lines().count(),
                m.height(),
                "grid rows must be exactly the newline-joined lines for {src:?}"
            );
        }
    }

    // 换行折在 `\frac` 参数之间（多行公式的常态）也要正确渲染
    let m = render_display("x = \\frac{-b \\pm \\sqrt{b^2-4ac}}\n{2a}", 400).unwrap();
    assert!(m.to_plain_text().contains("2a"), "{}", m.to_plain_text());
}

#[test]
fn hat_geometry_is_not_mistaken_for_a_leak() {
    // S1：`\hat` 一族画出来的反斜杠字形不能被泄漏自检误伤
    for (src, needle) in [
        (r"\hat{a}", "^"),
        (r"\hat{ab}", "/\\"),
        (r"\hat{abc}", "/\\"),
        (r"\hat{abcd}", "‾"),
        (r"\hat{\theta}", "^"),
        (r"\hat{H}\psi", "^"),
    ] {
        let m = render_display(src, 80).unwrap_or_else(|| panic!("expected Some for {src}"));
        assert!(
            m.to_plain_text().contains(needle),
            "{src}: missing {needle:?} in\n{}",
            m.to_plain_text()
        );
    }
}

#[test]
fn none_for_blank_results() {
    // N3：渲染出来只有空白等于没渲染
    for src in [r"\sqrt{}", r"\,", r"\!", r"\ ", r"{}"] {
        assert_eq!(render_display(src, 80), None, "should degrade: {src}");
    }
}

#[test]
fn named_operators_keep_the_space_before_their_argument() {
    // S3：`\log p` 不能粘成 `logp`
    for (src, expected) in [
        (r"\log p(x)", "log p(x)"),
        (r"\sin x", "sin x"),
        (r"\det A", "det A"),
        (r"\max f(x)", "max f(x)"),
        (r"\ln x", "ln x"),
    ] {
        assert_eq!(render_inline(src).as_deref(), Some(expected), "for {src}");
    }
    // 运算符两侧的间距
    assert_eq!(render_inline(r"a \cdot b").as_deref(), Some("a · b"));
    // 带下标的算子（`\log_2`）是"极限式"排版（下标另起一行），这里只验证内容不丢
    let m = render_display(r"\log_2 n", 80).unwrap();
    let text = m.to_plain_text();
    assert!(text.contains("log"), "{text}");
    assert!(text.contains('2'), "{text}");
    assert!(text.contains('n'), "{text}");
    // `\lim` 后面的空格来自源码，不能被重复补一次
    let m = render_display(r"\lim_{n \to \infty} \frac{1}{n}", 80).unwrap();
    assert_eq!(m.lines()[1], " lim  ───");
}

#[test]
fn binom_is_rendered_as_a_stacked_pair_inside_the_parentheses() {
    // S4：`\binom{n}{k}` 的括号要与内容同行
    let m = render_display(r"\binom{n}{k}", 80).unwrap();
    assert_eq!(m.lines(), ["⎛n⎞", "⎝k⎠"]);

    // B2：空参数不得 panic，也不得画出错位形状
    assert_eq!(render_display(r"\binom{}{}", 80), None);
    assert!(render_display(r"\binom{}{b}", 80).is_some());
    assert!(render_display(r"\binom{a}{}", 80).is_some());
    assert!(render_display(r"\binom{a}{b}", 80).is_some());
}

#[test]
fn wide_characters_in_scripts_do_not_break_column_alignment() {
    // N2：CJK 宽字符 + 上下标时，环境里各行的列要对齐
    let m = render_display(r"\begin{align} x^{中} &= b \\ c &= d \end{align}", 200).unwrap();
    let eq_columns: Vec<usize> = m.lines().iter().filter_map(|l| l.find('=')).collect();
    assert_eq!(eq_columns.len(), 2, "{}", m.to_plain_text());
    assert_eq!(
        eq_columns[0],
        eq_columns[1],
        "columns misaligned:\n{}",
        m.to_plain_text()
    );
}

// ── review r2 的 B1/B2 回归 ─────────────────────────────────────────

#[test]
fn none_for_separators_outside_managed_regions_at_any_depth() {
    // B1（r2）：上游 parser 在**任意花括号深度**遇到 `&` / `\\` 都会 break 并丢掉
    // 后半段，所以判据不能只看"花括号深度 0"。下面每一条在修复前都会返回
    // "少了内容但看起来正常"的网格。
    for src in [
        r"x + { y & z }",
        r"x + { y \\ z }",
        r"{a & b}",
        r"{{a & b}}",
        "x + { y & z",
        r"\frac{a & b}{c}",
        r"f(x) = \left\{ x^2 & x > 0 \\ 0 & x \le 0 \right.",
        r"\begin{align} \frac{a}{ &= b \\ c &= d \end{align}",
        r"\begin{align} a &= \frac{a}{ \\ c &= d \end{align}",
        r"\left( \begin{align} a &= \frac{a}{ \\ c &= d \end{align} \right)",
    ] {
        assert_eq!(render_inline(src), None, "must degrade (inline): {src}");
        assert_eq!(render_display(src, 400), None, "must degrade: {src}");
        assert!(render_block(src).is_none(), "must degrade: {src}");
    }

    // 大范围丢失形态：输入 250 字符 → 修复前输出 5 字符
    let bulk = format!("x + {{ y & {}", "z + ".repeat(60));
    assert_eq!(render_display(&bulk, 400), None);

    // 未经配平的花括号同样拒绝（上游会把组外的内容当成组内一路吃掉）
    for src in [r"{a}", r"{{a}}", r"\frac{a}{b}"] {
        assert!(
            render_block(src).is_some(),
            "balanced braces must render: {src}"
        );
    }
    for src in [r"{a", r"a}", r"{{a}", r"\frac{a}{b"] {
        assert!(render_block(src).is_none(), "unbalanced braces: {src}");
    }
}

#[test]
fn managed_regions_still_accept_separators() {
    // B1 的反面：分隔符出现在"有主"的地方必须照常工作
    for (src, needles) in [
        (
            r"\begin{cases} a & b \\ c & d \end{cases}",
            &["a", "b", "c", "d"][..],
        ),
        (
            r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
            &["a", "b", "c", "d"],
        ),
        (
            r"\begin{array}{cc} a & b \\ c & d \end{array}",
            &["a", "b", "c", "d"],
        ),
        (
            r"\begin{align} a &= b \\ c &= d \end{align}",
            &["a", "b", "c", "d"],
        ),
    ] {
        let m = render_display(src, 200).unwrap_or_else(|| panic!("must render: {src}"));
        let text = m.to_plain_text();
        for needle in needles {
            assert!(text.contains(needle), "{src}: missing {needle}");
        }
    }

    // `\text{…}` 里的 `&` 是普通字符
    assert_eq!(
        render_inline(r"f(x) = \text{a & b}").as_deref(),
        Some("f(x) = a & b")
    );
    // 环境套在 `\left…\right` 里也照常
    assert!(render_display(r"\left\{ \begin{matrix} a & b \end{matrix} \right.", 200).is_some());
    // align 单元格里再嵌 cases
    assert!(
        render_display(
            r"\begin{align} f &= \begin{cases} 1 & x>0 \\ 0 & x\le 0 \end{cases} \end{align}",
            200
        )
        .is_some()
    );
}

#[test]
fn none_for_invalid_delimiter_after_left_or_right() {
    // B2（r2）：`\left` 后接非法定界符时上游会把裸 `\` 当定界符画出来，
    // 输出里于是出现没渲染的 LaTeX 命令。
    for src in [
        r"\left\right",
        r"\left\unknowncmd y \right)",
        r"\left\frac x \right)",
        r"\left\ce x \right)",
        r"\left\alpha x \right)",
        r"$\left\right$",
        r"\begin{align} a &= \left\unknowncmd b \right) \\ c &= d \end{align}",
    ] {
        assert_eq!(render_inline(src), None, "must degrade: {src}");
        assert_eq!(render_display(src, 400), None, "must degrade: {src}");
    }

    // 合法定界符（含 `\left\{` / `\left\|` 归一化后的形态）照常
    for src in [
        r"\left( x \right)",
        r"\left\{ x \right\}",
        r"\left\| x \right\|",
        r"\left. x \right|",
        r"\left[ x \right]",
    ] {
        assert!(
            render_display(src, 80).is_some(),
            "valid delimiters must render: {src}"
        );
    }
    // `\hat` 的几何字形仍被豁免（防止有人再改回"输出含 `\` 即拒"）
    assert_eq!(
        render_display(r"\hat{ab}", 80).unwrap().lines(),
        ["/\\", "ab"]
    );
}

// ── review r3 的 B1 / N1–N3 回归 ─────────────────────────────────────

#[test]
fn none_when_an_environment_row_exceeds_its_capacity() {
    // B1（r3）：`cases` 是上游托管环境，每行只有 2 个槽位（value + condition）。
    // 第 3 个 `&` 之后的列没有消费者，上游直接 break → 后半段静默消失。
    for src in [
        r"\begin{cases} a & b & c \end{cases}",
        r"\begin{cases} a & b & c & d \end{cases}",
        r"\begin{cases} a & b \\ c & d & e \end{cases}",
        r"\begin{cases} 中 & 文 & 字 \end{cases}",
        // 真实语料形态：模型的"说明列"写法
        r"\begin{cases} 1 & x>0 & \text{正} \\ 0 & x\le 0 & \text{非正} \end{cases}",
        // 穿透到我们的托管层（整条必须降级，而不是少一列）
        r"\begin{align} x &= \begin{cases} a & b & c \end{cases} \end{align}",
        r"\begin{matrix} \begin{cases} a & b & c \end{cases} & d \end{matrix}",
        r"\left( \begin{cases} a & b & c \end{cases} \right)",
    ] {
        assert_eq!(render_inline(src), None, "must degrade (inline): {src}");
        assert_eq!(render_display(src, 400), None, "must degrade: {src}");
        assert!(render_block(src).is_none(), "must degrade: {src}");
    }

    // 正面：容量之内照常；`matrix` / `array` / 我们托管的 `align` 都**不受** 2 列限制
    let cases = render_display(r"\begin{cases} a & b \\ c & d \end{cases}", 200)
        .expect("two-column cases must render");
    let text = cases.to_plain_text();
    for needle in ["a", "b", "c", "d"] {
        assert!(text.contains(needle), "cases 丢了内容: {needle}");
    }
    for src in [
        r"\begin{pmatrix} a & b & c \\ d & e & f \end{pmatrix}",
        r"\begin{array}{ccc} a & b & c \\ d & e & f \end{array}",
        r"\begin{align} a &= b & c &= d \end{align}",
        r"\begin{cases} \text{a & b} & x>0 \end{cases}",
        r"\begin{cases} \begin{matrix} a & b \end{matrix} & x>0 \end{cases}",
    ] {
        assert!(
            render_display(src, 300).is_some(),
            "unlimited-column shape must render: {src}"
        );
    }
}

#[test]
fn none_when_a_separator_is_swallowed_as_a_command_argument() {
    // r3 收尾时 fuzz 找到的同类漏网：`\\` 紧跟在"无花括号参数"的命令后面时，
    // 上游会把它当成那个命令的**参数**（不再当行分隔符），于是行/列计数与实际消费
    // 不一致、后面的整段消失（`\begin{cases} 1 &\frac 2 \\ 3 & 4 \end{cases}`
    // 的 `4` 就这样没了）。兜底判据是"解析器必须消费完输入"。
    // 真会丢内容的形态：`cases` 每行只有 2 个槽位，`\\` 被 `\frac` / `\sqrt` 当成
    // 参数之后，`& 4` 这一段既不是行分隔也不是合法条件 → 整段消失。
    for src in [
        r"\begin{cases} 1 &\frac 2 \\ 3 & 4 \end{cases}",
        r"\begin{cases} 1 & \sqrt \\ 2 & 3 \end{cases}",
        r"\begin{cases} 1 & \hat \\ 2 & 3 \end{cases}",
    ] {
        assert_eq!(render_inline(src), None, "must degrade (inline): {src}");
        assert_eq!(render_display(src, 400), None, "must degrade: {src}");
        assert!(render_block(src).is_none(), "must degrade: {src}");
    }

    // 同一形态在"列数无上限"的环境里没有内容丢失（`matrix` 会把多出来的格子照单全收），
    // 所以它们照常渲染 —— 这条同时钉住"兜底判据不能误伤合法输入"。
    for src in [
        r"\begin{pmatrix} 1 & \hat \\ 2 & 3 \end{pmatrix}",
        r"\begin{matrix} 1 & \hat \\ 2 & 3 \end{matrix}",
    ] {
        let m = render_display(src, 200).unwrap_or_else(|| panic!("must render: {src}"));
        let text = m.to_plain_text();
        for needle in ['1', '2', '3'] {
            assert!(text.contains(needle), "{src}: 丢了 {needle}");
        }
    }

    // 正常写法（参数带花括号）照常
    for src in [
        r"\begin{cases} 1 &\frac{2}{3} \\ 4 & 5 \end{cases}",
        r"\begin{align} 1 &= \frac{2}{3} \\ 4 &= 5 \end{align}",
    ] {
        assert!(
            render_display(src, 200).is_some(),
            "braced arguments must render: {src}"
        );
    }
}

#[test]
fn text_groups_may_nest_and_keep_their_separators() {
    // N1（r3）：`\text{…}` 里再嵌花括号时，上游是"原文照读 + 计深"，
    // 所以里面的 `&` 仍然是普通字符，不该被当成没人消费的分隔符。
    for src in [r"\text{a {b & c}}", r"\text{a {b} c}", r"\text{a & b}"] {
        assert!(
            render_inline(src).is_some(),
            "literal text must render: {src}"
        );
    }
    assert_eq!(
        render_inline(r"\text{a {b & c}}").as_deref(),
        Some("a {b & c}")
    );
}

#[test]
fn delimiters_must_be_real_delimiter_tokens() {
    // N3（r3）：上游把"紧跟 `\left`/`\right` 的任意单字符"当定界符逐行画出来，
    // `\right文` 会凭空多画一个 `文`。我们现在只认真正的定界符记号。
    for src in [
        r"\left( x \right中",
        r"\left中 x \right)",
        r"\left.中\ge中文\sqrt[3]\mid\right文字\label",
    ] {
        assert_eq!(render_display(src, 300), None, "must degrade: {src}");
    }
    // 合法定界符一律照常
    for src in [
        r"\left( x \right)",
        r"\left[ x \right]",
        r"\left| x \right|",
        r"\left\{ x \right\}",
        r"\left\| x \right\|",
        r"\left. x \right.",
        r"\left< x \right>",
        r"\left\langle x \right\rangle",
        r"\left\lfloor x \right\rfloor",
        r"\left\lceil x \right\rceil",
    ] {
        assert!(
            render_display(src, 80).is_some(),
            "valid delimiter must render: {src}"
        );
    }
}

#[test]
fn array_spec_must_be_understood_or_degrade() {
    // N2（r3）：`array` 的列格式串里出现我们看不懂的东西（`p{2cm}` / 嵌套组里的内容）
    // 时，忽略它等于静默丢内容 → 整条降级。
    for src in [
        r"\begin{array}{{cc} 中 }& 1 \\ x & y \end{array}",
        r"\begin{array}{p{2cm}} a \end{array}",
        r"\begin{array}{@{}c@{}} a \end{array}",
    ] {
        assert!(render_block(src).is_none(), "must degrade: {src}");
    }
    for src in [
        r"\begin{array}{cc} a & b \end{array}",
        r"\begin{array}{|c|c|} a & b \end{array}",
        r"\begin{array}{lcr} a & b & c \end{array}",
        r"\begin{array}{} a \end{array}",
    ] {
        assert!(render_block(src).is_some(), "must render: {src}");
    }
}
