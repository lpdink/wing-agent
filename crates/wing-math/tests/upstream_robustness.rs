// ---------------------------------------------------------------------------
// 来源（迁移的上游集成测试，断言一条未改）
//   crate   : rust-latex-parser
//   version : 0.1.0
//   repo    : https://github.com/William-Selna/Rust-LaTeX-Parser（发布 sha1 086d394561465b4154272951f5cd524f77855e99）
//   license : MIT，Copyright (c) 2026 William-Selna（原文见 crate 根 LICENSE-MIT-rust-latex-parser）
//   原路径  : tests/robustness.rs
//
// 本地改动（相对上游）：仅 `use rust_latex_parser::...` -> `use wing_math::latex::...`
// 与文件头出处注释；测试函数与断言逐字保留。另跑 `cargo fmt`（纯空白差异）。
// ---------------------------------------------------------------------------

//! Stress tests: adversarial, edge-case, and real-world inputs.

use wing_math::latex::{EqNode, latex_to_unicode, parse_equation};

// ─── Adversarial / malformed input ──────────────────────────────────

#[test]
fn deeply_nested_braces() {
    // 200 levels of nesting — should not stack overflow
    let input = "{".repeat(200) + "x" + &"}".repeat(200);
    let _node = parse_equation(&input); // must not panic
}

#[test]
fn deeply_nested_fractions() {
    // \frac{\frac{\frac{...}{1}}{1}}{1} — 100 levels
    let mut input = "x".to_string();
    for _ in 0..100 {
        input = format!("\\frac{{{}}}{{{}}}", input, "1");
    }
    let _node = parse_equation(&input);
}

#[test]
fn deeply_nested_sqrt() {
    // sqrt(sqrt(sqrt(...))) — 100 levels
    let mut input = "x".to_string();
    for _ in 0..100 {
        input = format!("\\sqrt{{{}}}", input);
    }
    let _node = parse_equation(&input);
}

#[test]
fn unmatched_open_brace() {
    let node = parse_equation("{{{x");
    // Should not panic, produces some tree
    let _ = format!("{:?}", node);
}

#[test]
fn unmatched_close_brace() {
    let node = parse_equation("x}}}");
    let _ = format!("{:?}", node);
}

#[test]
fn unmatched_parens() {
    let node = parse_equation("(((x");
    let _ = format!("{:?}", node);
}

#[test]
fn empty_frac() {
    let node = parse_equation("\\frac{}{}");
    assert!(matches!(node, EqNode::Frac(_, _)));
}

#[test]
fn empty_sqrt() {
    let node = parse_equation("\\sqrt{}");
    assert!(matches!(node, EqNode::Sqrt(_)));
}

#[test]
fn backslash_at_eof() {
    let node = parse_equation("x + \\");
    let _ = format!("{:?}", node);
}

#[test]
fn unknown_command() {
    let node = parse_equation("\\notarealcommand");
    // Should produce a Text node with the escaped command
    let _ = format!("{:?}", node);
}

#[test]
fn only_whitespace() {
    let node = parse_equation("   ");
    let _ = format!("{:?}", node);
}

#[test]
fn only_operators() {
    let node = parse_equation("+ = - * /");
    let _ = format!("{:?}", node);
}

#[test]
fn unicode_input() {
    // Direct Unicode math symbols (not LaTeX commands)
    let node = parse_equation("α + β = γ");
    let _ = format!("{:?}", node);
}

#[test]
fn emoji_input() {
    let node = parse_equation("🎉 + 🔥 = 💯");
    let _ = format!("{:?}", node); // should not panic
}

#[test]
fn cjk_input() {
    let node = parse_equation("数学 x^2");
    let _ = format!("{:?}", node);
}

#[test]
fn very_long_input() {
    let input = "x + ".repeat(10_000) + "y";
    let _node = parse_equation(&input); // must not take forever
}

#[test]
fn repeated_superscripts() {
    // x^2^3^4^5 — only first ^ should bind, rest are separate atoms
    let node = parse_equation("x^2^3^4^5");
    let _ = format!("{:?}", node);
}

#[test]
fn empty_matrix() {
    let node = parse_equation("\\begin{pmatrix}\\end{pmatrix}");
    assert!(matches!(node, EqNode::Matrix { .. }));
}

#[test]
fn matrix_single_cell() {
    let node = parse_equation("\\begin{bmatrix} 42 \\end{bmatrix}");
    assert!(matches!(node, EqNode::Matrix { .. }));
}

#[test]
fn cases_single_row() {
    let node = parse_equation("\\begin{cases} x \\end{cases}");
    assert!(matches!(node, EqNode::Cases { .. }));
}

#[test]
fn left_without_right() {
    let node = parse_equation("\\left( x + y");
    // Should handle gracefully
    let _ = format!("{:?}", node);
}

#[test]
fn right_without_left() {
    let node = parse_equation("x + y \\right)");
    let _ = format!("{:?}", node);
}

#[test]
fn nested_environments() {
    let node = parse_equation(
        "\\begin{pmatrix} \\begin{cases} a & b \\\\ c & d \\end{cases} & e \\\\ f & g \\end{pmatrix}",
    );
    assert!(matches!(node, EqNode::Matrix { .. }));
}

// ─── Real-world expressions ─────────────────────────────────────────

#[test]
fn quadratic_formula() {
    let node = parse_equation("x = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}");
    assert!(matches!(node, EqNode::Seq(_)));
}

#[test]
fn euler_identity() {
    let node = parse_equation("e^{i\\pi} + 1 = 0");
    let _ = format!("{:?}", node);
}

#[test]
fn gaussian_integral() {
    let node = parse_equation("\\int_{-\\infty}^{\\infty} e^{-x^2} dx = \\sqrt{\\pi}");
    let _ = format!("{:?}", node);
}

#[test]
fn taylor_series() {
    let node = parse_equation("f(x) = \\sum_{n=0}^{\\infty} \\frac{f^{(n)}(a)}{n!}(x-a)^n");
    let _ = format!("{:?}", node);
}

#[test]
fn matrix_determinant() {
    let node = parse_equation("\\det \\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix} = ad - bc");
    let _ = format!("{:?}", node);
}

#[test]
fn limit_definition() {
    let node = parse_equation("\\lim_{h \\to 0} \\frac{f(x+h) - f(x)}{h}");
    let _ = format!("{:?}", node);
}

#[test]
fn binomial_theorem() {
    let node = parse_equation("(x+y)^n = \\sum_{k=0}^{n} \\binom{n}{k} x^{n-k} y^k");
    let _ = format!("{:?}", node);
}

#[test]
fn maxwells_equation() {
    let node =
        parse_equation("\\nabla \\times \\vec{E} = -\\frac{\\partial \\vec{B}}{\\partial t}");
    let _ = format!("{:?}", node);
}

#[test]
fn schrodinger_equation() {
    let node = parse_equation("i\\hbar \\frac{\\partial}{\\partial t} \\Psi = \\hat{H} \\Psi");
    let _ = format!("{:?}", node);
}

#[test]
fn bareword_convenience() {
    // All barewords should parse without backslash
    let node = parse_equation("pi alpha omega infty sum int sin cos lim");
    let _ = format!("{:?}", node);
}

// ─── latex_to_unicode completeness ──────────────────────────────────

#[test]
fn all_greek_lowercase() {
    let letters = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        "lambda", "mu", "nu", "xi", "omicron", "pi", "rho", "sigma", "tau", "upsilon", "phi",
        "chi", "psi", "omega",
    ];
    for l in &letters {
        assert!(latex_to_unicode(l).is_some(), "Missing: {}", l);
    }
}

#[test]
fn all_greek_uppercase() {
    let letters = [
        "Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta", "Eta", "Theta", "Iota", "Kappa",
        "Lambda", "Mu", "Nu", "Xi", "Pi", "Rho", "Sigma", "Tau", "Upsilon", "Phi", "Chi", "Psi",
        "Omega",
    ];
    for l in &letters {
        assert!(latex_to_unicode(l).is_some(), "Missing: {}", l);
    }
}
