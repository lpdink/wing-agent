// ---------------------------------------------------------------------------
// 来源（vendored，逐字内联）
//   crate   : term-maths
//   version : 1.0.0
//   repo    : 包元数据未声明 repository（crates.io `term-maths`；发布时
//             `.cargo_vcs_info.json` git sha1 = 5a2de3b29f1d4cec72c8685b8d52ddfb53519676）
//   license : MIT OR Apache-2.0，Copyright (c) 2026 Jack Geraghty
//             （原文见 crate 根 LICENSE-MIT / LICENSE-APACHE）
//   原路径  : latex_renderer.rs
//
// 本地改动（相对上游）：
//   1. 模块路径：`rust_latex_parser::` -> `crate::latex::`，
//      `crate::xxx` -> `crate::grid::xxx`（内联后模块位置变化）。
//   2. 运行 `cargo fmt`（仓库门禁要求 `cargo fmt --check` 干净）。上游文件未经 rustfmt
//      处理，因此有纯空白差异；已用「先 rustfmt 上游文件、再与本文件逐行 diff」核对，
//      除上述改动外逐字一致（核对脚本见 crate 根 NOTICE 的「内联保真度」一节）。
//   除以上两点外与上游逐字一致（含文件内联测试）。
// ---------------------------------------------------------------------------

//! LaTeX renderer — serialises an `EqNode` AST back to a LaTeX string.

use crate::latex::{AccentKind, EqNode, MathFontKind, MatrixKind};

use crate::grid::renderer::MathRenderer;

/// Serialises an `EqNode` back to a LaTeX math string.
pub struct LatexRenderer;

impl MathRenderer for LatexRenderer {
    type Output = String;

    fn render(&self, node: &EqNode) -> String {
        node_to_latex(node)
    }
}

fn node_to_latex(node: &EqNode) -> String {
    match node {
        EqNode::Text(s) => latex_escape_text(s),
        EqNode::Space(pts) => space_to_latex(*pts),
        EqNode::Seq(children) => children.iter().map(node_to_latex).collect(),
        EqNode::Frac(num, den) => {
            format!(r"\frac{{{}}}{{{}}}", node_to_latex(num), node_to_latex(den))
        }
        EqNode::Sup(base, sup) => {
            format!("{}^{{{}}}", node_to_latex(base), node_to_latex(sup))
        }
        EqNode::Sub(base, sub) => {
            format!("{}_{{{}}} ", node_to_latex(base), node_to_latex(sub))
        }
        EqNode::SupSub(base, sup, sub) => {
            format!(
                "{}^{{{}}}_{{{}}}",
                node_to_latex(base),
                node_to_latex(sup),
                node_to_latex(sub)
            )
        }
        EqNode::Sqrt(body) => format!(r"\sqrt{{{}}}", node_to_latex(body)),
        EqNode::BigOp {
            symbol,
            lower,
            upper,
        } => {
            let sym = unicode_to_latex_op(symbol);
            let mut s = sym;
            if let Some(lo) = lower {
                s.push_str(&format!("_{{{}}}", node_to_latex(lo)));
            }
            if let Some(up) = upper {
                s.push_str(&format!("^{{{}}}", node_to_latex(up)));
            }
            s
        }
        EqNode::Accent(body, kind) => {
            let cmd = match kind {
                AccentKind::Hat => r"\hat",
                AccentKind::Bar => r"\overline",
                AccentKind::Dot => r"\dot",
                AccentKind::DoubleDot => r"\ddot",
                AccentKind::Tilde => r"\tilde",
                AccentKind::Vec => r"\vec",
            };
            format!("{}{{{}}}", cmd, node_to_latex(body))
        }
        EqNode::Limit { name, lower } => {
            let latex_name = format!(r"\{}", name);
            if let Some(lo) = lower {
                format!("{}_{{{}}}", latex_name, node_to_latex(lo))
            } else {
                latex_name
            }
        }
        EqNode::TextBlock(s) => format!(r"\text{{{}}}", s),
        EqNode::MathFont { kind, content } => {
            let cmd = match kind {
                MathFontKind::Bold => r"\mathbf",
                MathFontKind::Blackboard => r"\mathbb",
                MathFontKind::Calligraphic => r"\mathcal",
                MathFontKind::Roman => r"\mathrm",
                MathFontKind::Fraktur => r"\mathfrak",
                MathFontKind::SansSerif => r"\mathsf",
                MathFontKind::Monospace => r"\mathtt",
            };
            format!("{}{{{}}}", cmd, node_to_latex(content))
        }
        EqNode::Delimited {
            left,
            right,
            content,
        } => {
            format!(
                r"\left{} {} \right{}",
                latex_delim(left),
                node_to_latex(content),
                latex_delim(right)
            )
        }
        EqNode::Matrix { kind, rows } => {
            let env = match kind {
                MatrixKind::Plain => "matrix",
                MatrixKind::Paren => "pmatrix",
                MatrixKind::Bracket => "bmatrix",
                MatrixKind::Brace => "Bmatrix",
                MatrixKind::VBar => "vmatrix",
                MatrixKind::DoubleVBar => "Vmatrix",
            };
            let rows_str: Vec<String> = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(node_to_latex)
                        .collect::<Vec<_>>()
                        .join(" & ")
                })
                .collect();
            format!(
                r"\begin{{{}}} {} \end{{{}}}",
                env,
                rows_str.join(r" \\ "),
                env
            )
        }
        EqNode::Cases { rows } => {
            let rows_str: Vec<String> = rows
                .iter()
                .map(|(val, cond)| {
                    if let Some(c) = cond {
                        format!("{} & {}", node_to_latex(val), node_to_latex(c))
                    } else {
                        node_to_latex(val)
                    }
                })
                .collect();
            format!(r"\begin{{cases}} {} \end{{cases}}", rows_str.join(r" \\ "))
        }
        EqNode::Binom(top, bottom) => {
            format!(
                r"\binom{{{}}}{{{}}}",
                node_to_latex(top),
                node_to_latex(bottom)
            )
        }
        EqNode::Brace {
            content,
            label,
            over,
        } => {
            let cmd = if *over { r"\overbrace" } else { r"\underbrace" };
            let mut s = format!("{}{{{}}}", cmd, node_to_latex(content));
            if let Some(lbl) = label {
                if *over {
                    s.push_str(&format!("^{{{}}}", node_to_latex(lbl)));
                } else {
                    s.push_str(&format!("_{{{}}}", node_to_latex(lbl)));
                }
            }
            s
        }
        EqNode::StackRel {
            base,
            annotation,
            over,
        } => {
            let cmd = if *over { r"\overset" } else { r"\underset" };
            format!(
                "{}{{{}}}{{{}}}",
                cmd,
                node_to_latex(annotation),
                node_to_latex(base)
            )
        }
    }
}

/// Escape special LaTeX characters in text content.
fn latex_escape_text(s: &str) -> String {
    // Map common Unicode back to LaTeX commands
    let mut result = String::new();
    for ch in s.chars() {
        match ch {
            'α' => result.push_str(r"\alpha "),
            'β' => result.push_str(r"\beta "),
            'γ' => result.push_str(r"\gamma "),
            'δ' => result.push_str(r"\delta "),
            'ε' => result.push_str(r"\epsilon "),
            'ζ' => result.push_str(r"\zeta "),
            'η' => result.push_str(r"\eta "),
            'θ' => result.push_str(r"\theta "),
            'ι' => result.push_str(r"\iota "),
            'κ' => result.push_str(r"\kappa "),
            'λ' => result.push_str(r"\lambda "),
            'μ' => result.push_str(r"\mu "),
            'ν' => result.push_str(r"\nu "),
            'ξ' => result.push_str(r"\xi "),
            'π' => result.push_str(r"\pi "),
            'ρ' => result.push_str(r"\rho "),
            'σ' => result.push_str(r"\sigma "),
            'τ' => result.push_str(r"\tau "),
            'υ' => result.push_str(r"\upsilon "),
            'φ' => result.push_str(r"\phi "),
            'χ' => result.push_str(r"\chi "),
            'ψ' => result.push_str(r"\psi "),
            'ω' => result.push_str(r"\omega "),
            '∞' => result.push_str(r"\infty "),
            '∑' => result.push_str(r"\sum "),
            '∏' => result.push_str(r"\prod "),
            '∫' => result.push_str(r"\int "),
            '±' => result.push_str(r"\pm "),
            '·' => result.push_str(r"\cdot "),
            '→' => result.push_str(r"\rightarrow "),
            '←' => result.push_str(r"\leftarrow "),
            '≤' => result.push_str(r"\leq "),
            '≥' => result.push_str(r"\geq "),
            '≠' => result.push_str(r"\neq "),
            '∈' => result.push_str(r"\in "),
            '∀' => result.push_str(r"\forall "),
            '∃' => result.push_str(r"\exists "),
            '∂' => result.push_str(r"\partial "),
            '∇' => result.push_str(r"\nabla "),
            _ => result.push(ch),
        }
    }
    result
}

fn space_to_latex(pts: f32) -> String {
    if pts < 0.0 {
        r"\!".to_string()
    } else if pts < 3.0 {
        r"\,".to_string()
    } else if pts < 5.0 {
        r"\;".to_string()
    } else if pts >= 18.0 {
        r"\quad ".to_string()
    } else {
        " ".to_string()
    }
}

fn unicode_to_latex_op(symbol: &str) -> String {
    match symbol {
        "∑" => r"\sum".to_string(),
        "∏" => r"\prod".to_string(),
        "∫" => r"\int".to_string(),
        "∬" => r"\iint".to_string(),
        "∮" => r"\oint".to_string(),
        "⋃" => r"\bigcup".to_string(),
        "⋂" => r"\bigcap".to_string(),
        "⊕" => r"\bigoplus".to_string(),
        "⊗" => r"\bigotimes".to_string(),
        _ => symbol.to_string(),
    }
}

fn latex_delim(d: &str) -> String {
    match d {
        "." => ".".to_string(),
        "(" | ")" | "[" | "]" | "|" => d.to_string(),
        "{" => r"\{".to_string(),
        "}" => r"\}".to_string(),
        "‖" => r"\|".to_string(),
        _ => d.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::renderer::MathRenderer;
    use crate::latex::parse_equation;

    #[test]
    fn test_simple_fraction_roundtrip() {
        let renderer = LatexRenderer;
        let ast = parse_equation(r"\frac{a}{b}");
        let latex = renderer.render(&ast);
        assert!(latex.contains(r"\frac"));
        assert!(latex.contains('a'));
        assert!(latex.contains('b'));
    }

    #[test]
    fn test_superscript_roundtrip() {
        let renderer = LatexRenderer;
        let ast = parse_equation(r"x^2");
        let latex = renderer.render(&ast);
        assert!(latex.contains("x^"));
        assert!(latex.contains('2'));
    }

    #[test]
    fn test_matrix_roundtrip() {
        let renderer = LatexRenderer;
        let ast = parse_equation(r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}");
        let latex = renderer.render(&ast);
        assert!(latex.contains("pmatrix"));
        assert!(latex.contains('&'));
    }
}
