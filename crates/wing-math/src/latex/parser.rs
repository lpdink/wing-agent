// ---------------------------------------------------------------------------
// 来源（vendored，逐字内联）
//   crate   : rust-latex-parser
//   version : 0.1.0
//   repo    : https://github.com/William-Selna/Rust-LaTeX-Parser
//             （发布时 .cargo_vcs_info.json git sha1 = 086d394561465b4154272951f5cd524f77855e99）
//   license : MIT，Copyright (c) 2026 William-Selna
//             （原文见 crate 根 LICENSE-MIT-rust-latex-parser；上游 crate 未随包附 LICENSE 文件，
//               本文件由仓库 LICENSE 取得）
//   原路径  : parser.rs
//
// 本地改动（相对上游）：
//   1. 模块路径：`crate::ast` -> `crate::latex::ast`，`rust_latex_parser::`
//      -> `crate::latex::`（内联后模块位置变化）。
//   2. doc 示例里的 use 路径同步改写为 `wing_math::latex::...`。
//   3. 4 处 `collapsible_if` 机械折叠（let-chains），语义不变 —— 本仓库
//      `cargo clippy -- -D warnings` 必须干净，改点均在原地标了「本地改动」注释：
//      `maybe_wrap_op_spacing` / `parse_sequence_until_ex`（两处）/ `parse_command`。
//   4. `is_spaced_operator` 加入 `·`（U+00B7）：`\cdot` 在 LaTeX 里是二元运算符，
//      上游漏了它，`a \cdot b` 被渲染成 `a ·b`。
//   5. 命名算子（`\log` `\sin` `\det` `\max` …）后面被吃掉的空格补回来：
//      上游读完命令名后无条件吃掉一个尾随空格（TeX 控制词惯例），但命名算子后面那个
//      空格在 LaTeX 里是有语义的，被吃掉后 `\log p(x)` 渲染成 `logp(x)`。
//   6. `parse_group_atom` 先跳过参数前的空白：LaTeX 允许 `\frac {a} {b}`，上游不跳
//      空格会把第二个参数解析成一个空格，`\frac{X} {Y}` 的分母因此消失、`{Y}` 被挤到
//      最后一行（多行公式折行经控制字符归一化后正好是这个形状）。
//   7. 递归深度闸 + `parse_equation_with_depth`（本项目新增）：`parse_atom` /
//      `parse_sequence_until_ex` 各包一层深度计数，越界即置位并立刻返回空节点
//      （`parse_atom` 越界时会吃掉一个字符，保证调用方的 `while pos < len` 前进）。
//      上游没有这个上界：brace-free 的深嵌套（`\left(\left(…`、`\hat \hat …`）
//      会让解析器与排版递归到爆栈（debug + 2 MiB 栈实测 abort）。
//      `parse_equation` 保持原语义（无上界），新入口供上层适配层做"排版前闸门"。
//   8. `parse_equation_with_depth` 额外返回**消费的字符数**（`parser.pos`）：
//      上游在 `&` / `\\` / `}` / `\right` / `\end` 处会直接 `break`，剩下的输入被静默
//      丢掉。上层用它做"输入必须被完整消费"的兜底判据（review r3 的 B1 收尾：
//      `\\` 被当成 `\frac` 的第二个参数时，`cases` 后面整段消失）。
//   9. 运行 `cargo fmt`（仓库门禁要求 `cargo fmt --check` 干净）。上游文件未经 rustfmt
//      处理，因此有纯空白差异；已用「先 rustfmt 上游文件、再与本文件逐行 diff」核对，
//      除上述改动外逐字一致（核对脚本见 crate 根 NOTICE 的「内联保真度」一节）。
//  10. 符号表与二元算子间距（步骤 02）：`latex_to_unicode` 补 `\top`（⊤ U+22A4，
//      转置记号；缺了它 `W^\top` 会走未知命令兜底、被 AST 泄漏自检判成"未渲染"）与
//      `\odot`（⊙ U+2299）；`is_spaced_operator` 加入带圈 / 星号算子
//      `⊙ ⊕ ⊗ ∘ ∗ ⋆`（与本地改动 4 的 `·` 同类：它们在 LaTeX 里是二元运算符，
//      上游漏了间距，`a ⊕ b` 被渲染成 `a ⊕b`）。`⊤` 是普通符号，**不**进间距集。
//  11. 符号命令的尾随空格退回输入流（步骤 02）：本地改动 5 只把命名算子的语义空格
//      补回来，符号命令（`\pi` `\top` …）走的是"读完命令名无条件吃掉一个空格"的上游
//      路径，于是 `\pi x` 渲染成 `πx`，而裸词 `pi x` 是 `π x`（同一式子两种写法两种
//      结果）。这里把吃掉的空格**退回输入流**（`self.pos -= 1`），空格由外层序列正常
//      产生 Space 节点；这样 `w^\top x` 的上标仍是干净的 `⊤`（→ `ᵀ`），空格落在
//      `wᵀ` 与 `x` 之间，而不是被吞进上标参数。
//   除以上十一点外与上游逐字一致（含文件内联测试）。
// ---------------------------------------------------------------------------

//! The parser. Turns LaTeX math strings into [`EqNode`] trees.
//!
//! The main entry point is [`parse_equation`]. For standalone symbol lookup,
//! use [`latex_to_unicode`].
//!
//! The parser is a hand-written recursive descent parser over a `Vec<char>`.
//! It never fails — malformed input produces a best-effort tree rather than
//! an error. This keeps live-preview editors responsive while the user types.

use crate::latex::ast::*;

/// Space (in points) inserted on each side of binary operators like `+`, `=`, `\leq`.
const OP_SPACE: f32 = 4.0;

pub const BIG_OP_COMMANDS: &[&str] = &["sum", "prod", "int", "iint", "iiint", "oint", "coprod"];

/// Characters that are relation or binary operators and should get OP_SPACE on each side.
fn is_spaced_operator(ch: char) -> bool {
    matches!(
        ch,
        '\u{2264}' | '\u{2265}' | '\u{2260}' | '\u{2248}' | '\u{2261}' | // ≤ ≥ ≠ ≈ ≡
        '\u{2208}' | '\u{2282}' | '\u{2283}' | '\u{2286}' | '\u{2287}' | // ∈ ⊂ ⊃ ⊆ ⊇
        '\u{2192}' | '\u{2190}' | '\u{2194}' | '\u{21D2}' | '\u{21D0}' | '\u{21D4}' | // → ← ↔ ⇒ ⇐ ⇔
        '\u{227A}' | '\u{227B}' | '\u{223C}' | '\u{2245}' | '\u{226A}' | '\u{226B}' | // ≺ ≻ ∼ ≅ ≪ ≫
        '\u{221D}' | // ∝
        // 本地改动（见文件头）：`·`（`\cdot`）与带圈 / 星号算子（`⊙ ⊕ ⊗ ∘ ∗ ⋆`）在
        // LaTeX 里都是二元运算符，两侧要有间距；上游漏了它们，`a \cdot b` / `a ⊕ b`
        // 被渲染成 `a ·b` / `a ⊕b`（缺左侧空格）。
        '\u{00B7}' | '\u{2299}' | '\u{2295}' | '\u{2297}' | // · ⊙ ⊕ ⊗
        '\u{2218}' | '\u{2217}' | '\u{22C6}' | // ∘ ∗ ⋆
        '\u{00B1}' | '\u{2213}' | '\u{00D7}' | '\u{00F7}' // ± ∓ × ÷
    )
}

/// Wrap a symbol text node with operator spacing if the first character is a spaced operator.
fn maybe_wrap_op_spacing(symbol: String) -> EqNode {
    // 本地改动：折叠嵌套 if（`collapsible_if`），语义与上游一致
    if let Some(ch) = symbol.chars().next()
        && is_spaced_operator(ch)
    {
        return EqNode::Seq(vec![
            EqNode::Space(OP_SPACE),
            EqNode::Text(symbol),
            EqNode::Space(OP_SPACE),
        ]);
    }
    EqNode::Text(symbol)
}

pub fn is_big_op(name: &str) -> bool {
    BIG_OP_COMMANDS.contains(&name)
}

pub fn big_op_symbol(name: &str) -> &'static str {
    match name {
        "sum" => "\u{2211}",
        "prod" => "\u{220F}",
        "coprod" => "\u{2210}",
        "int" => "\u{222B}",
        "iint" => "\u{222C}",
        "iiint" => "\u{222D}",
        "oint" => "\u{222E}",
        _ => "\u{2211}",
    }
}

/// Parse a LaTeX math string into an [`EqNode`] tree.
///
/// Accepts standard LaTeX math-mode markup as well as bareword shortcuts
/// (`pi`, `sqrt(x)`, `int_0^1`, etc.). Never returns an error — malformed
/// input produces a best-effort tree.
///
/// # Examples
///
/// ```
/// use wing_math::latex::{parse_equation, EqNode};
///
/// // Standard LaTeX
/// let tree = parse_equation("\\frac{a}{b}");
/// assert!(matches!(tree, EqNode::Frac(_, _)));
///
/// // Bareword shortcuts
/// let tree = parse_equation("pi r^2");
///
/// // Complex expressions
/// let tree = parse_equation("\\int_0^\\infty e^{-x^2} dx = \\sqrt{\\pi}");
/// ```
pub fn parse_equation(input: &str) -> EqNode {
    parse_equation_with_depth(input, usize::MAX).0
}

/// 本地改动（见文件头）：带**递归深度上界**的解析。
///
/// 返回 `(AST, 是否超深, 消费的字符数)`。超深时 AST 是残缺的（超出部分的节点是空
/// `Text`），调用方
/// **必须**丢弃它——这是给上层适配层用的"排版前闸门"：brace-free 的深嵌套
/// （`\left(\left(…`、`\hat \hat …`、`\frac \frac …`）不产生任何花括号，
/// 光看括号深度拦不住，只有解析器自己知道递归到多深。
pub fn parse_equation_with_depth(input: &str, max_depth: usize) -> (EqNode, bool, usize) {
    let mut parser = EqParser::new(input);
    parser.max_depth = max_depth;
    let node = parser.parse_sequence();
    // 第三个返回值：解析器**消费掉的字符数**。上游在 `&` / `\\` / `}` / `\right` /
    // `\end` 处会直接 `break`，剩下的输入被静默丢掉；调用方用它做"输入被完整消费"
    // 的兜底判据（review r3 的 B1：`\begin{cases} 1 &\frac 2 \\ 3 & 4 \end{cases}`
    // 里 `\\` 被 `\frac` 当成了第二个参数，`& 4` 整段消失）。
    (node, parser.depth_exceeded, parser.pos)
}

pub struct EqParser {
    chars: Vec<char>,
    pos: usize,
    /// 本地改动（见文件头）：当前递归深度 / 上界 / 是否越界。
    depth: usize,
    max_depth: usize,
    depth_exceeded: bool,
}

impl EqParser {
    pub fn new(input: &str) -> Self {
        Self {
            chars: input.chars().collect(),
            pos: 0,
            depth: 0,
            max_depth: usize::MAX,
            depth_exceeded: false,
        }
    }

    /// 进入一层递归：超过上界就置位并让调用方立刻返回空节点（不再向下递归）。
    fn enter(&mut self) -> bool {
        self.depth += 1;
        if self.depth > self.max_depth {
            self.depth_exceeded = true;
            return false;
        }
        true
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.chars.get(self.pos).copied();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    fn parse_sequence(&mut self) -> EqNode {
        self.parse_sequence_until(false)
    }

    fn parse_sequence_until(&mut self, stop_on_paren: bool) -> EqNode {
        self.parse_sequence_until_ex(stop_on_paren, false)
    }

    fn parse_sequence_until_ex(&mut self, stop_on_paren: bool, stop_on_right: bool) -> EqNode {
        // 本地改动（见文件头）：递归深度闸 —— 超深立刻返回空节点，不再向下递归。
        if !self.enter() {
            return EqNode::Text(String::new());
        }
        let node = self.parse_sequence_body(stop_on_paren, stop_on_right);
        self.leave();
        node
    }

    fn parse_sequence_body(&mut self, stop_on_paren: bool, stop_on_right: bool) -> EqNode {
        let mut nodes = Vec::new();
        while self.pos < self.chars.len() {
            // 本地改动（见文件头）：已越界就直接收工（结果会被上层的深度闸丢弃）
            if self.depth_exceeded {
                break;
            }
            let ch = match self.peek() {
                Some(c) => c,
                None => break,
            };
            // Stop on \right when parsing \left...\right content
            // 本地改动：折叠嵌套 if（`collapsible_if`），语义与上游一致
            if stop_on_right && ch == '\\' && self.lookahead_command("right") {
                break;
            }
            // Stop on \end when parsing environment bodies
            if ch == '\\' && self.lookahead_command("end") {
                break;
            }
            match ch {
                '}' => break,
                ')' if stop_on_paren => break,
                // Row separator \\ inside environments — stop to let caller handle
                '\\' if self.is_row_separator() => break,
                // Column separator & — stop to let caller handle
                '&' => break,
                '/' => {
                    self.advance();
                    // Turn last node into fraction numerator
                    let numer = if let Some(n) = nodes.pop() {
                        n
                    } else {
                        EqNode::Text(String::new())
                    };
                    let denom = self.parse_group_atom();
                    let denom = self.parse_postfix(denom);
                    nodes.push(EqNode::Frac(Box::new(numer), Box::new(denom)));
                }
                _ => {
                    let atom = self.parse_atom();
                    let node = self.parse_postfix(atom);
                    // Context-aware minus: add operator spacing only for binary minus
                    // 本地改动：折叠嵌套 if（`collapsible_if`），语义与上游一致
                    if let EqNode::Text(ref s) = node
                        && s == "-"
                        && !nodes.is_empty()
                    {
                        // Check if the previous node looks like an operand (not an operator/space)
                        let is_binary = match nodes.last() {
                            Some(EqNode::Space(_)) => false,
                            Some(_) => true,
                            None => false,
                        };
                        if is_binary {
                            nodes.push(EqNode::Space(OP_SPACE));
                            nodes.push(node);
                            nodes.push(EqNode::Space(OP_SPACE));
                            continue;
                        }
                    }
                    nodes.push(node);
                }
            }
        }
        match nodes.len() {
            0 => EqNode::Text(String::new()),
            1 => nodes.remove(0),
            _ => EqNode::Seq(nodes),
        }
    }

    /// Check if the next two chars are `\\` (row separator in matrices/cases).
    fn is_row_separator(&self) -> bool {
        self.pos + 1 < self.chars.len()
            && self.chars[self.pos] == '\\'
            && self.chars[self.pos + 1] == '\\'
    }

    /// Peek ahead to see if the next command after `\` is the given name,
    /// without consuming anything.
    fn lookahead_command(&self, name: &str) -> bool {
        if self.pos >= self.chars.len() || self.chars[self.pos] != '\\' {
            return false;
        }
        let name_chars: Vec<char> = name.chars().collect();
        let start = self.pos + 1; // skip the backslash
        if start + name_chars.len() > self.chars.len() {
            return false;
        }
        for (i, &nc) in name_chars.iter().enumerate() {
            if self.chars[start + i] != nc {
                return false;
            }
        }
        // Make sure it's not a prefix of a longer command
        let after = start + name_chars.len();
        if after < self.chars.len() && self.chars[after].is_ascii_alphabetic() {
            return false;
        }
        true
    }

    /// Parse an atom that treats `(...)` as invisible grouping (no parens shown).
    /// Used by sqrt, frac, and other constructs that take arguments.
    /// Tracks paren nesting so `((3x+3))` correctly distinguishes inner visible parens from grouping parens.
    fn parse_group_atom(&mut self) -> EqNode {
        // 本地改动（见文件头）：LaTeX 允许命令参数前有空白（`\frac {a} {b}` 合法），
        // 上游不跳空格，于是 `\frac{X} {Y}` 的分母会被解析成一个空格、`{Y}` 变成
        // 紧随其后的独立分组，渲染成品里分母消失、body 被挤到最后一行。
        // 多行公式折行（`\frac{X}\n{Y}`）经控制字符归一化后正好是这个形状，所以必须跳过。
        while self.peek() == Some(' ') {
            self.advance();
        }
        if self.peek() == Some('(') {
            self.advance();
            // Parse content, but track nested parens so we only stop
            // on the `)` that matches our opening `(`
            let mut nodes = Vec::new();
            let mut depth = 0i32;
            while self.pos < self.chars.len() {
                match self.peek() {
                    Some(')') if depth == 0 => break,
                    Some(')') => {
                        depth -= 1;
                        self.advance();
                        nodes.push(EqNode::Text(")".into()));
                    }
                    Some('(') => {
                        depth += 1;
                        self.advance();
                        nodes.push(EqNode::Text("(".into()));
                    }
                    Some('}') => break,
                    None => break,
                    _ => {
                        let atom = self.parse_atom();
                        let node = self.parse_postfix(atom);
                        nodes.push(node);
                    }
                }
            }
            if self.peek() == Some(')') {
                self.advance();
            }
            return match nodes.len() {
                0 => EqNode::Text(String::new()),
                1 => nodes.remove(0),
                _ => EqNode::Seq(nodes),
            };
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> EqNode {
        // 本地改动（见文件头）：递归深度闸（`\hat \hat …` 这类链只走 parse_atom）。
        // 越界时必须**吃掉一个字符**：调用方的 `while pos < len` 循环依赖"每个 atom
        // 至少前进一格"，原地返回会让它死循环。
        if !self.enter() {
            self.advance();
            return EqNode::Text(String::new());
        }
        let node = self.parse_atom_inner();
        self.leave();
        node
    }

    fn parse_atom_inner(&mut self) -> EqNode {
        match self.peek() {
            None => EqNode::Text(String::new()),
            Some('{') => {
                self.advance();
                let inner = self.parse_sequence();
                if self.peek() == Some('}') {
                    self.advance();
                }
                inner
            }
            Some('\\') => {
                self.advance();
                self.parse_command()
            }
            Some(ch) if ch.is_ascii_alphabetic() => {
                // Check for "sqrt" keyword
                if self.try_keyword("sqrt") {
                    let arg = self.parse_group_atom();
                    return EqNode::Sqrt(Box::new(arg));
                }
                // Try bare-word big operators (int, sum, prod, etc.)
                for &kw in BIG_OP_COMMANDS {
                    if self.try_keyword(kw) {
                        let symbol = big_op_symbol(kw).to_string();
                        let mut upper: Option<EqNode> = None;
                        let mut lower: Option<EqNode> = None;
                        loop {
                            match self.peek() {
                                Some('^') if upper.is_none() => {
                                    self.advance();
                                    upper = Some(self.parse_atom());
                                }
                                Some('_') if lower.is_none() => {
                                    self.advance();
                                    lower = Some(self.parse_atom());
                                }
                                _ => break,
                            }
                        }
                        let op = EqNode::BigOp {
                            symbol,
                            lower: lower.map(Box::new),
                            upper: upper.map(Box::new),
                        };
                        // If followed by (...), consume as the integrand/summand body
                        if self.peek() == Some('(') {
                            let body = self.parse_group_atom();
                            return EqNode::Seq(vec![op, body]);
                        }
                        return op;
                    }
                }
                // Try bare-word limit operators (lim, sin, cos, log, etc.)
                static BARE_LIMIT_OPS: &[&str] = &[
                    "liminf", "limsup", "lim", "arcsin", "arccos", "arctan", "sinh", "cosh",
                    "tanh", "sin", "cos", "tan", "cot", "sec", "csc", "log", "ln", "exp", "min",
                    "max", "det", "dim", "ker", "gcd", "arg",
                ];
                for &kw in BARE_LIMIT_OPS {
                    if self.try_keyword(kw) {
                        let mut lower: Option<EqNode> = None;
                        if self.peek() == Some('_') {
                            self.advance();
                            lower = Some(self.parse_group_atom());
                        }
                        return EqNode::Limit {
                            name: kw.to_string(),
                            lower: lower.map(Box::new),
                        };
                    }
                }
                // Try bare-word Greek letters / symbols (pi, alpha, theta, etc.)
                if let Some(node) = self.try_bareword_symbol() {
                    return node;
                }
                // Single letter
                self.advance();
                EqNode::Text(ch.to_string())
            }
            Some(ch) if ch.is_ascii_digit() || ch == '.' => {
                let mut s = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == '.' {
                        s.push(c);
                        self.advance();
                    } else {
                        break;
                    }
                }
                EqNode::Text(s)
            }
            Some(' ') => {
                self.advance();
                EqNode::Text(" ".into())
            }
            Some(ch @ ('+' | '=' | '<' | '>' | '*')) => {
                self.advance();
                let op = if ch == '*' {
                    "\u{00B7}".to_string()
                } else {
                    ch.to_string()
                };
                EqNode::Seq(vec![
                    EqNode::Space(OP_SPACE),
                    EqNode::Text(op),
                    EqNode::Space(OP_SPACE),
                ])
            }
            Some('-') => {
                // Handled in parse_sequence_until_ex for context-aware spacing
                self.advance();
                EqNode::Text("-".to_string())
            }
            Some(ch) => {
                self.advance();
                EqNode::Text(ch.to_string())
            }
        }
    }

    fn parse_postfix(&mut self, base: EqNode) -> EqNode {
        // Collect both ^ and _ in any order to combine into SupSub if both present.
        let mut sup: Option<EqNode> = None;
        let mut sub: Option<EqNode> = None;

        loop {
            match self.peek() {
                Some('^') if sup.is_none() => {
                    self.advance();
                    sup = Some(self.parse_atom());
                }
                Some('_') if sub.is_none() => {
                    self.advance();
                    sub = Some(self.parse_atom());
                }
                _ => break,
            }
        }

        match (sup, sub) {
            (Some(s), Some(b)) => EqNode::SupSub(Box::new(base), Box::new(s), Box::new(b)),
            (Some(s), None) => EqNode::Sup(Box::new(base), Box::new(s)),
            (None, Some(b)) => EqNode::Sub(Box::new(base), Box::new(b)),
            (None, None) => base,
        }
    }

    fn try_keyword(&mut self, kw: &str) -> bool {
        let kw_chars: Vec<char> = kw.chars().collect();
        if self.pos + kw_chars.len() > self.chars.len() {
            return false;
        }
        for (i, &kc) in kw_chars.iter().enumerate() {
            if self.chars[self.pos + i] != kc {
                return false;
            }
        }
        let after = self.pos + kw_chars.len();
        if after < self.chars.len() && self.chars[after].is_ascii_alphabetic() {
            return false;
        }
        self.pos += kw_chars.len();
        true
    }

    /// Try to match a bare-word Greek letter or common symbol at the current position.
    /// Tries longest match first to avoid ambiguity (e.g. "epsilon" before "eta").
    fn try_bareword_symbol(&mut self) -> Option<EqNode> {
        // Sorted longest-first so we match "epsilon" before "eta", "theta" before "the", etc.
        static BAREWORDS: &[&str] = &[
            "varepsilon",
            "rightarrow",
            "leftarrow",
            "Rightarrow",
            "Leftarrow",
            "epsilon",
            "upsilon",
            "omicron",
            "lambda",
            "Lambda",
            "implies",
            "partial",
            "emptyset",
            "alpha",
            "beta",
            "gamma",
            "delta",
            "theta",
            "kappa",
            "sigma",
            "omega",
            "Gamma",
            "Delta",
            "Theta",
            "Sigma",
            "Omega",
            "infty",
            "nabla",
            "forall",
            "exists",
            "approx",
            "equiv",
            "times",
            "cdot",
            "zeta",
            "iota",
            "pi",
            "rho",
            "tau",
            "phi",
            "chi",
            "psi",
            "eta",
            "nu",
            "xi",
            "mu",
            "Pi",
            "Phi",
            "Psi",
            "pm",
        ];
        for &word in BAREWORDS {
            if self.try_keyword(word) {
                let symbol = latex_to_unicode(word).unwrap_or_else(|| word.to_string());
                return Some(maybe_wrap_op_spacing(symbol));
            }
        }
        None
    }

    fn parse_command(&mut self) -> EqNode {
        // Handle single-char (non-alpha) spacing commands first:
        // \, \; \: \> \! and also \\ (row separator — should not reach here normally)
        // 本地改动：折叠嵌套 if（`collapsible_if`），语义与上游一致
        if let Some(ch) = self.peek()
            && !ch.is_ascii_alphabetic()
        {
            self.advance();
            return match ch {
                ',' => EqNode::Space(3.0),           // thin space
                ':' | '>' => EqNode::Space(4.0),     // medium space
                ';' => EqNode::Space(5.0),           // thick space
                '!' => EqNode::Space(-3.0),          // negative thin space
                '\\' => EqNode::Text(String::new()), // row separator \\, handled elsewhere
                '{' => EqNode::Text("{".to_string()),
                '}' => EqNode::Text("}".to_string()),
                ' ' => EqNode::Space(4.0), // backslash-space
                _ => EqNode::Text(ch.to_string()),
            };
        }

        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphabetic() {
                name.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        // Skip one trailing space if present
        // 本地改动（见文件头）：记下"是否吃掉了一个空格"，供命名算子把语义空格补回来。
        let had_trailing_space = self.peek() == Some(' ');
        if had_trailing_space {
            self.advance();
        }

        // Named spacing commands
        match name.as_str() {
            "quad" => return EqNode::Space(18.0),
            "qquad" => return EqNode::Space(36.0),
            _ => {}
        }

        // \text{...} — upright text block
        if name == "text" {
            return self.parse_text_block();
        }

        // \mathbf, \mathbb, \mathcal, \mathrm, \mathfrak, \mathsf, \mathtt
        if let Some(kind) = match name.as_str() {
            "mathbf" => Some(MathFontKind::Bold),
            "mathbb" => Some(MathFontKind::Blackboard),
            "mathcal" => Some(MathFontKind::Calligraphic),
            "mathrm" => Some(MathFontKind::Roman),
            "mathfrak" => Some(MathFontKind::Fraktur),
            "mathsf" => Some(MathFontKind::SansSerif),
            "mathtt" => Some(MathFontKind::Monospace),
            _ => None,
        } {
            let arg = self.parse_atom();
            return EqNode::MathFont {
                kind,
                content: Box::new(arg),
            };
        }

        // \left ... \right — stretchy delimiters
        if name == "left" {
            return self.parse_left_right();
        }

        // \begin{env} ... \end{env}
        if name == "begin" {
            return self.parse_begin_env();
        }

        // \binom{n}{k}
        if name == "binom" {
            let top = self.parse_group_atom();
            let bot = self.parse_group_atom();
            return EqNode::Binom(Box::new(top), Box::new(bot));
        }

        // \overbrace{...}^{label}
        if name == "overbrace" {
            let content = self.parse_atom();
            let mut label = None;
            if self.peek() == Some('^') {
                self.advance();
                label = Some(Box::new(self.parse_atom()));
            }
            return EqNode::Brace {
                content: Box::new(content),
                label,
                over: true,
            };
        }

        // \underbrace{...}_{label}
        if name == "underbrace" {
            let content = self.parse_atom();
            let mut label = None;
            if self.peek() == Some('_') {
                self.advance();
                label = Some(Box::new(self.parse_atom()));
            }
            return EqNode::Brace {
                content: Box::new(content),
                label,
                over: false,
            };
        }

        // \overset{annotation}{base} and \stackrel{annotation}{base}
        if name == "overset" || name == "stackrel" {
            let annotation = self.parse_group_atom();
            let base = self.parse_group_atom();
            return EqNode::StackRel {
                base: Box::new(base),
                annotation: Box::new(annotation),
                over: true,
            };
        }

        // \underset{annotation}{base}
        if name == "underset" {
            let annotation = self.parse_group_atom();
            let base = self.parse_group_atom();
            return EqNode::StackRel {
                base: Box::new(base),
                annotation: Box::new(annotation),
                over: false,
            };
        }

        // Fractions
        if name == "frac" {
            let numer = self.parse_group_atom();
            let denom = self.parse_group_atom();
            return EqNode::Frac(Box::new(numer), Box::new(denom));
        }

        // Square root
        if name == "sqrt" {
            let arg = self.parse_group_atom();
            return EqNode::Sqrt(Box::new(arg));
        }

        // Accents
        if let Some(kind) = match name.as_str() {
            "hat" => Some(AccentKind::Hat),
            "bar" | "overline" => Some(AccentKind::Bar),
            "dot" => Some(AccentKind::Dot),
            "ddot" => Some(AccentKind::DoubleDot),
            "tilde" => Some(AccentKind::Tilde),
            "vec" => Some(AccentKind::Vec),
            _ => None,
        } {
            let arg = self.parse_atom();
            return EqNode::Accent(Box::new(arg), kind);
        }

        // Named limit operators (lim, min, max, sup, inf, log, ln, sin, cos, tan, etc.)
        static LIMIT_OPS: &[&str] = &[
            "lim", "liminf", "limsup", "min", "max", "sup", "inf", "log", "ln", "exp", "sin",
            "cos", "tan", "cot", "sec", "csc", "arcsin", "arccos", "arctan", "sinh", "cosh",
            "tanh", "det", "dim", "ker", "deg", "gcd", "hom", "arg",
        ];
        if LIMIT_OPS.contains(&name.as_str()) {
            let mut lower: Option<EqNode> = None;
            if self.peek() == Some('_') {
                self.advance();
                lower = Some(self.parse_group_atom());
            }
            // 本地改动（见文件头）：上游在读完命令名后无条件吃掉一个尾随空格
            // （TeX 控制词的惯例），但命名算子（`\log` `\sin` `\det` …）后面那个空格
            // 在 LaTeX 里是有语义的：算子与它的参数之间必须有间隔。被吃掉后
            // `\log p(x)` 会渲染成 `logp(x)`（review r1 的 S3）。这里把吃掉的空格补回来。
            let node = EqNode::Limit {
                name: name.clone(),
                lower: lower.map(Box::new),
            };
            return if had_trailing_space {
                EqNode::Seq(vec![node, EqNode::Text(" ".to_string())])
            } else {
                node
            };
        }

        // Big operators -- parse as BigOp node, then collect limits via postfix
        if is_big_op(&name) {
            let symbol = big_op_symbol(&name).to_string();
            // Collect limits
            let mut upper: Option<EqNode> = None;
            let mut lower: Option<EqNode> = None;
            loop {
                match self.peek() {
                    Some('^') if upper.is_none() => {
                        self.advance();
                        upper = Some(self.parse_atom());
                    }
                    Some('_') if lower.is_none() => {
                        self.advance();
                        lower = Some(self.parse_atom());
                    }
                    _ => break,
                }
            }
            return EqNode::BigOp {
                symbol,
                lower: lower.map(Box::new),
                upper: upper.map(Box::new),
            };
        }

        // Greek letter / symbol lookup
        let symbol = latex_to_unicode(&name).unwrap_or_else(|| format!("\\{}", name));
        // 本地改动（见文件头）：符号命令后面那个空格在 LaTeX 里是**分隔两个对象**的
        // 语义空格（`\pi x` 该渲染成 `π x`；裸词路径 `pi x` 本来就是这个行为 —— 被吃
        // 掉之后同一个式子两种写法两种结果）。本地改动 5 对命名算子（`\log p(x)`）做过
        // 同样的事，这里推广到符号命令；做法是把吃掉的空格**退回输入流**，而不是塞进
        // 返回的节点里：空格留在输入里，外层序列会照常产生一个 Space 节点，于是
        // `w^\top x` 的上标仍然是干净的 `⊤`（→ `ᵀ`），空格落在 `wᵀ` 与 `x` 之间。
        if had_trailing_space {
            self.pos -= 1;
        }
        maybe_wrap_op_spacing(symbol)
    }

    /// Parse \text{...} — consume braces and return raw text (no math parsing).
    fn parse_text_block(&mut self) -> EqNode {
        if self.peek() == Some('{') {
            self.advance();
            let mut text = String::new();
            let mut depth = 1;
            while let Some(ch) = self.advance() {
                if ch == '{' {
                    depth += 1;
                    text.push(ch);
                } else if ch == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    text.push(ch);
                } else {
                    text.push(ch);
                }
            }
            EqNode::TextBlock(text)
        } else {
            EqNode::TextBlock(String::new())
        }
    }

    /// Parse \left<delim> ... \right<delim>.
    fn parse_left_right(&mut self) -> EqNode {
        // Consume the left delimiter character
        let left_delim = match self.advance() {
            Some('.') => String::new(), // invisible delimiter
            Some(ch) => ch.to_string(),
            None => String::new(),
        };

        // Parse content until \right
        let content = self.parse_sequence_until_ex(false, true);

        // Consume \right and its delimiter
        let right_delim = if self.lookahead_command("right") {
            self.advance(); // skip '\'
            // consume 'right'
            for _ in 0..5 {
                self.advance();
            }
            // skip trailing space
            if self.peek() == Some(' ') {
                self.advance();
            }
            match self.advance() {
                Some('.') => String::new(),
                Some(ch) => ch.to_string(),
                None => String::new(),
            }
        } else {
            String::new()
        };

        EqNode::Delimited {
            left: left_delim,
            right: right_delim,
            content: Box::new(content),
        }
    }

    /// Parse \begin{env_name} ... \end{env_name}.
    fn parse_begin_env(&mut self) -> EqNode {
        let env_name = self.parse_brace_arg();

        match env_name.as_str() {
            "matrix" | "pmatrix" | "bmatrix" | "vmatrix" | "Bmatrix" | "Vmatrix" => {
                let kind = match env_name.as_str() {
                    "matrix" => MatrixKind::Plain,
                    "pmatrix" => MatrixKind::Paren,
                    "bmatrix" => MatrixKind::Bracket,
                    "vmatrix" => MatrixKind::VBar,
                    "Vmatrix" => MatrixKind::DoubleVBar,
                    "Bmatrix" => MatrixKind::Brace,
                    _ => MatrixKind::Plain,
                };
                let rows = self.parse_matrix_body();
                self.consume_end_env(&env_name);
                EqNode::Matrix { kind, rows }
            }
            "cases" => {
                let rows = self.parse_cases_body();
                self.consume_end_env("cases");
                EqNode::Cases { rows }
            }
            _ => {
                // Unknown environment — just return empty text
                EqNode::Text(format!("\\begin{{{}}}", env_name))
            }
        }
    }

    /// Consume {text} and return the text inside braces.
    fn parse_brace_arg(&mut self) -> String {
        if self.peek() == Some('{') {
            self.advance();
            let mut s = String::new();
            while let Some(ch) = self.peek() {
                if ch == '}' {
                    self.advance();
                    break;
                }
                s.push(ch);
                self.advance();
            }
            s
        } else {
            String::new()
        }
    }

    /// Parse matrix body: rows separated by \\, columns by &. Stop at \end.
    fn parse_matrix_body(&mut self) -> Vec<Vec<EqNode>> {
        let mut rows = Vec::new();
        loop {
            let mut row = Vec::new();
            loop {
                let cell = self.parse_sequence_until_ex(false, false);
                row.push(cell);
                if self.peek() == Some('&') {
                    self.advance(); // consume &
                    continue;
                }
                break;
            }
            rows.push(row);
            // Check for \\ row separator
            if self.is_row_separator() {
                self.advance(); // skip first '\'
                self.advance(); // skip second '\'
                // Skip optional trailing space
                if self.peek() == Some(' ') {
                    self.advance();
                }
                // Check if we're about to hit \end
                if self.peek() == Some('\\') && self.lookahead_command("end") {
                    break;
                }
                continue;
            }
            break;
        }
        rows
    }

    /// Parse cases body: rows separated by \\, two columns by &.
    fn parse_cases_body(&mut self) -> Vec<(EqNode, Option<EqNode>)> {
        let mut rows = Vec::new();
        loop {
            let value = self.parse_sequence_until_ex(false, false);
            let condition = if self.peek() == Some('&') {
                self.advance();
                Some(self.parse_sequence_until_ex(false, false))
            } else {
                None
            };
            rows.push((value, condition));
            // Check for \\ row separator
            if self.is_row_separator() {
                self.advance();
                self.advance();
                if self.peek() == Some(' ') {
                    self.advance();
                }
                if self.peek() == Some('\\') && self.lookahead_command("end") {
                    break;
                }
                continue;
            }
            break;
        }
        rows
    }

    /// Consume \end{env_name} from the input.
    fn consume_end_env(&mut self, _expected: &str) {
        // We should be at \end{...}
        if self.peek() == Some('\\') {
            self.advance(); // skip '\'
            // consume 'end'
            for _ in 0..3 {
                self.advance();
            }
            // skip trailing space
            if self.peek() == Some(' ') {
                self.advance();
            }
            // consume {env_name}
            self.parse_brace_arg();
        }
    }
}

/// Look up the Unicode character for a LaTeX command name.
///
/// Covers 130+ symbols: Greek letters (upper and lowercase), operators,
/// relations, set theory, arrows, delimiters, and miscellaneous symbols.
/// Returns `None` for anything it doesn't recognize.
///
/// This is useful on its own if you just need symbol conversion without
/// parsing a full expression.
///
/// # Examples
///
/// ```
/// use wing_math::latex::latex_to_unicode;
///
/// assert_eq!(latex_to_unicode("alpha"), Some("α".to_string()));
/// assert_eq!(latex_to_unicode("infty"), Some("∞".to_string()));
/// assert_eq!(latex_to_unicode("rightarrow"), Some("→".to_string()));
/// assert_eq!(latex_to_unicode("notacommand"), None);
/// ```
pub fn latex_to_unicode(name: &str) -> Option<String> {
    let ch = match name {
        // Greek lowercase
        "alpha" => '\u{03B1}',
        "beta" => '\u{03B2}',
        "gamma" => '\u{03B3}',
        "delta" => '\u{03B4}',
        "epsilon" | "varepsilon" => '\u{03B5}',
        "zeta" => '\u{03B6}',
        "eta" => '\u{03B7}',
        "theta" | "vartheta" => '\u{03B8}',
        "iota" => '\u{03B9}',
        "kappa" => '\u{03BA}',
        "lambda" => '\u{03BB}',
        "mu" => '\u{03BC}',
        "nu" => '\u{03BD}',
        "xi" => '\u{03BE}',
        "omicron" => '\u{03BF}',
        "pi" | "varpi" => '\u{03C0}',
        "rho" | "varrho" => '\u{03C1}',
        "sigma" | "varsigma" => '\u{03C3}',
        "tau" => '\u{03C4}',
        "upsilon" => '\u{03C5}',
        "phi" | "varphi" => '\u{03C6}',
        "chi" => '\u{03C7}',
        "psi" => '\u{03C8}',
        "omega" => '\u{03C9}',
        // Greek uppercase
        "Alpha" => '\u{0391}',
        "Beta" => '\u{0392}',
        "Gamma" => '\u{0393}',
        "Delta" => '\u{0394}',
        "Epsilon" => '\u{0395}',
        "Zeta" => '\u{0396}',
        "Eta" => '\u{0397}',
        "Theta" => '\u{0398}',
        "Iota" => '\u{0399}',
        "Kappa" => '\u{039A}',
        "Lambda" => '\u{039B}',
        "Mu" => '\u{039C}',
        "Nu" => '\u{039D}',
        "Xi" => '\u{039E}',
        "Pi" => '\u{03A0}',
        "Rho" => '\u{03A1}',
        "Sigma" => '\u{03A3}',
        "Tau" => '\u{03A4}',
        "Upsilon" => '\u{03A5}',
        "Phi" => '\u{03A6}',
        "Chi" => '\u{03A7}',
        "Psi" => '\u{03A8}',
        "Omega" => '\u{03A9}',
        // Operators
        "pm" | "plusminus" => '\u{00B1}',
        "mp" | "minusplus" => '\u{2213}',
        "times" => '\u{00D7}',
        "div" => '\u{00F7}',
        "cdot" => '\u{00B7}',
        "ast" => '\u{2217}',
        "star" => '\u{22C6}',
        "circ" => '\u{2218}',
        "bullet" => '\u{2022}',
        "oplus" => '\u{2295}',
        "otimes" => '\u{2297}',
        "odot" => '\u{2299}', // 本地改动（见文件头）：`\odot`（⊙）上游缺失，ML 里的逐元素乘高频
        // Relations
        "leq" | "le" => '\u{2264}',
        "geq" | "ge" => '\u{2265}',
        "neq" | "ne" => '\u{2260}',
        "approx" => '\u{2248}',
        "equiv" => '\u{2261}',
        "sim" => '\u{223C}',
        "simeq" => '\u{2243}',
        "cong" => '\u{2245}',
        "propto" => '\u{221D}',
        "ll" => '\u{226A}',
        "gg" => '\u{226B}',
        "prec" => '\u{227A}',
        "succ" => '\u{227B}',
        "perp" => '\u{22A5}',
        "parallel" => '\u{2225}',
        // 本地改动（见文件头）：`\top`（U+22A4 DOWN TACK，转置记号）上游缺失，
        // `W^\top` / `A^\top` 会走未知命令兜底、被 AST 泄漏自检判成"没渲染的命令"。
        // 注意它是**普通符号**不是二元算子：不进 `is_spaced_operator`。
        "top" => '\u{22A4}',
        // Logic & sets
        "forall" => '\u{2200}',
        "exists" => '\u{2203}',
        "nexists" => '\u{2204}',
        "neg" | "lnot" => '\u{00AC}',
        "land" | "wedge" => '\u{2227}',
        "lor" | "vee" => '\u{2228}',
        "in" => '\u{2208}',
        "notin" => '\u{2209}',
        "ni" => '\u{220B}',
        "subset" => '\u{2282}',
        "supset" => '\u{2283}',
        "subseteq" => '\u{2286}',
        "supseteq" => '\u{2287}',
        "cup" => '\u{222A}',
        "cap" => '\u{2229}',
        "emptyset" | "varnothing" => '\u{2205}',
        "setminus" => '\u{2216}',
        // Arrows
        "rightarrow" | "to" => '\u{2192}',
        "leftarrow" | "gets" => '\u{2190}',
        "leftrightarrow" => '\u{2194}',
        "Rightarrow" | "implies" => '\u{21D2}',
        "Leftarrow" => '\u{21D0}',
        "Leftrightarrow" | "iff" => '\u{21D4}',
        "uparrow" => '\u{2191}',
        "downarrow" => '\u{2193}',
        "mapsto" => '\u{21A6}',
        "hookrightarrow" => '\u{21AA}',
        "hookleftarrow" => '\u{21A9}',
        // Misc
        "infty" | "inf" => '\u{221E}',
        "partial" => '\u{2202}',
        "nabla" => '\u{2207}',
        "hbar" => '\u{210F}',
        "ell" => '\u{2113}',
        "Re" => '\u{211C}',
        "Im" => '\u{2111}',
        "wp" => '\u{2118}',
        "aleph" => '\u{2135}',
        "angle" => '\u{2220}',
        "triangle" => '\u{25B3}',
        "degree" | "deg" => '\u{00B0}',
        "prime" => '\u{2032}',
        "dots" | "ldots" | "cdots" => '\u{22EF}',
        "vdots" => '\u{22EE}',
        "ddots" => '\u{22F1}',
        // Delimiters
        "langle" => '\u{27E8}',
        "rangle" => '\u{27E9}',
        "lceil" => '\u{2308}',
        "rceil" => '\u{2309}',
        "lfloor" => '\u{230A}',
        "rfloor" => '\u{230B}',
        _ => return None,
    };
    Some(ch.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Helper matchers ────────────────────────────────────────────

    fn is_text(node: &EqNode, expected: &str) -> bool {
        matches!(node, EqNode::Text(s) if s == expected)
    }

    /// Collect all Text contents from a tree (depth-first), ignoring spaces.
    fn collect_text(node: &EqNode) -> String {
        match node {
            EqNode::Text(s) => s.clone(),
            EqNode::Space(_) => String::new(),
            EqNode::Seq(nodes) => nodes.iter().map(collect_text).collect(),
            EqNode::Sup(base, sup) => format!("{}^{}", collect_text(base), collect_text(sup)),
            EqNode::Sub(base, sub) => format!("{}_{}", collect_text(base), collect_text(sub)),
            EqNode::SupSub(base, sup, sub) => format!(
                "{}^{}_{}",
                collect_text(base),
                collect_text(sup),
                collect_text(sub)
            ),
            EqNode::Frac(n, d) => format!("({})/({})", collect_text(n), collect_text(d)),
            EqNode::Sqrt(inner) => format!("sqrt({})", collect_text(inner)),
            EqNode::BigOp {
                symbol,
                lower,
                upper,
            } => {
                let mut s = symbol.clone();
                if let Some(l) = lower {
                    s += &format!("_{}", collect_text(l));
                }
                if let Some(u) = upper {
                    s += &format!("^{}", collect_text(u));
                }
                s
            }
            EqNode::Limit { name, lower } => {
                let mut s = name.clone();
                if let Some(l) = lower {
                    s += &format!("_{}", collect_text(l));
                }
                s
            }
            EqNode::Accent(inner, _) => collect_text(inner),
            EqNode::TextBlock(s) => s.clone(),
            EqNode::MathFont { content, .. } => collect_text(content),
            EqNode::Delimited { content, .. } => collect_text(content),
            EqNode::Matrix { rows, .. } => rows
                .iter()
                .map(|r| r.iter().map(collect_text).collect::<Vec<_>>().join("&"))
                .collect::<Vec<_>>()
                .join("\\\\"),
            EqNode::Cases { rows, .. } => rows
                .iter()
                .map(|(v, c)| {
                    let mut s = collect_text(v);
                    if let Some(cond) = c {
                        s += &format!("&{}", collect_text(cond));
                    }
                    s
                })
                .collect::<Vec<_>>()
                .join("\\\\"),
            EqNode::Binom(a, b) => format!("binom({},{})", collect_text(a), collect_text(b)),
            EqNode::Brace { content, .. } => collect_text(content),
            EqNode::StackRel {
                base, annotation, ..
            } => format!("stack({},{})", collect_text(base), collect_text(annotation)),
        }
    }

    // ─── Basic parsing ──────────────────────────────────────────────

    #[test]
    fn parse_single_letter() {
        let node = parse_equation("x");
        assert!(is_text(&node, "x"));
    }

    #[test]
    fn parse_number() {
        let node = parse_equation("42");
        assert!(is_text(&node, "42"));
    }

    #[test]
    fn parse_decimal_number() {
        let node = parse_equation("3.14");
        assert!(is_text(&node, "3.14"));
    }

    #[test]
    fn parse_empty_input() {
        let node = parse_equation("");
        assert!(is_text(&node, ""));
    }

    // ─── Superscripts & subscripts ──────────────────────────────────

    #[test]
    fn parse_superscript() {
        let node = parse_equation("x^2");
        assert!(matches!(node, EqNode::Sup(_, _)));
        assert_eq!(collect_text(&node), "x^2");
    }

    #[test]
    fn parse_subscript() {
        let node = parse_equation("x_1");
        assert!(matches!(node, EqNode::Sub(_, _)));
        assert_eq!(collect_text(&node), "x_1");
    }

    #[test]
    fn parse_combined_sup_sub() {
        let node = parse_equation("x^2_3");
        assert!(matches!(node, EqNode::SupSub(_, _, _)));
        assert_eq!(collect_text(&node), "x^2_3");
    }

    #[test]
    fn parse_combined_sub_sup_order() {
        // Sub then sup should also produce SupSub
        let node = parse_equation("x_3^2");
        assert!(matches!(node, EqNode::SupSub(_, _, _)));
    }

    #[test]
    fn parse_braced_superscript() {
        let node = parse_equation("x^{2n}");
        assert!(matches!(node, EqNode::Sup(_, _)));
        if let EqNode::Sup(_, sup) = &node {
            assert_eq!(collect_text(sup), "2n");
        }
    }

    // ─── Fractions ──────────────────────────────────────────────────

    #[test]
    fn parse_inline_fraction() {
        let node = parse_equation("a/b");
        assert!(matches!(node, EqNode::Frac(_, _)));
        assert_eq!(collect_text(&node), "(a)/(b)");
    }

    #[test]
    fn parse_frac_command() {
        let node = parse_equation("\\frac{a}{b}");
        assert!(matches!(node, EqNode::Frac(_, _)));
        assert_eq!(collect_text(&node), "(a)/(b)");
    }

    #[test]
    fn parse_frac_with_parens() {
        // (x+1)/(x-1) — parens are invisible grouping for /
        let node = parse_equation("(x+1)/(x-1)");
        // Should be a sequence ending with a Frac
        let text = collect_text(&node);
        assert!(text.contains("x+1"));
        assert!(text.contains("x-1"));
    }

    #[test]
    fn parse_nested_paren_fraction() {
        // 5/((3x+3)) — inner parens should be visible
        let node = parse_equation("5/((3x+3))");
        let text = collect_text(&node);
        assert!(text.contains("(3x+3)"));
    }

    // ─── Square root ────────────────────────────────────────────────

    #[test]
    fn parse_sqrt_bareword() {
        let node = parse_equation("sqrt(x)");
        assert!(matches!(node, EqNode::Sqrt(_)));
        assert_eq!(collect_text(&node), "sqrt(x)");
    }

    #[test]
    fn parse_sqrt_command() {
        let node = parse_equation("\\sqrt{x}");
        assert!(matches!(node, EqNode::Sqrt(_)));
    }

    #[test]
    fn parse_sqrt_no_visible_parens() {
        // sqrt(x^2) — parens are invisible grouping, not in the AST
        let node = parse_equation("sqrt(x^2)");
        assert!(matches!(node, EqNode::Sqrt(_)));
        // The inner content should be Sup(x, 2), not Seq with parens
        if let EqNode::Sqrt(inner) = &node {
            assert!(
                matches!(**inner, EqNode::Sup(_, _)),
                "Inner should be Sup, got: {:?}",
                inner
            );
        }
    }

    #[test]
    fn parse_sqrt_with_visible_parens() {
        // sqrt((x^2)) — double parens means inner parens are visible
        let node = parse_equation("sqrt((x^2))");
        let text = collect_text(&node);
        assert!(text.contains("("));
    }

    // ─── Greek letters ──────────────────────────────────────────────

    #[test]
    fn parse_greek_bareword() {
        let node = parse_equation("pi");
        assert!(is_text(&node, "\u{03C0}"));
    }

    #[test]
    fn parse_greek_backslash() {
        let node = parse_equation("\\alpha");
        assert!(is_text(&node, "\u{03B1}"));
    }

    #[test]
    fn parse_greek_uppercase() {
        let node = parse_equation("\\Omega");
        assert!(is_text(&node, "\u{03A9}"));
    }

    #[test]
    fn parse_greek_in_expression() {
        let node = parse_equation("pi r^2");
        let text = collect_text(&node);
        assert!(text.contains("\u{03C0}"));
        assert!(text.contains("r^2"));
    }

    // ─── Big operators ──────────────────────────────────────────────

    #[test]
    fn parse_sum_with_limits() {
        let node = parse_equation("\\sum_{i=0}^{n}");
        assert!(matches!(node, EqNode::BigOp { .. }));
        if let EqNode::BigOp { lower, upper, .. } = &node {
            assert!(lower.is_some());
            assert!(upper.is_some());
        }
    }

    #[test]
    fn parse_int_bareword() {
        let node = parse_equation("int_0^1");
        assert!(matches!(node, EqNode::BigOp { .. }));
    }

    #[test]
    fn parse_int_with_body_parens() {
        // int(x^2) — parens should be consumed as invisible grouping
        let tree = parse_equation("int(x^2)");
        let text = collect_text(&tree);
        // Should contain the integral symbol and x^2 but not parens
        assert!(text.contains("x^2"));
        assert!(!text.contains("("));
    }

    // ─── Limit operators ────────────────────────────────────────────

    #[test]
    fn parse_lim_bareword() {
        let node = parse_equation("lim_{x \\to 0}");
        assert!(matches!(node, EqNode::Limit { .. }));
        if let EqNode::Limit { name, lower } = &node {
            assert_eq!(name, "lim");
            assert!(lower.is_some());
        }
    }

    #[test]
    fn parse_sin_bareword() {
        let node = parse_equation("sin(x)");
        // sin is a Limit node, (x) is the argument as a separate Seq member
        let text = collect_text(&node);
        assert!(text.contains("sin"));
        assert!(text.contains("x"));
    }

    #[test]
    fn parse_log_with_subscript() {
        let node = parse_equation("log_2");
        assert!(matches!(node, EqNode::Limit { .. }));
        if let EqNode::Limit { name, lower } = &node {
            assert_eq!(name, "log");
            assert!(lower.is_some());
        }
    }

    // ─── Operator spacing ───────────────────────────────────────────

    #[test]
    fn parse_plus_gets_spacing() {
        let node = parse_equation("a + b");
        // Should produce a tree that contains Space nodes (may be nested)
        fn has_space(node: &EqNode) -> bool {
            match node {
                EqNode::Space(_) => true,
                EqNode::Seq(nodes) => nodes.iter().any(has_space),
                _ => false,
            }
        }
        assert!(has_space(&node), "Plus should have space nodes in tree");
    }

    #[test]
    fn parse_unary_minus_no_spacing() {
        let node = parse_equation("-x");
        // Leading minus should NOT have spacing (it's unary)
        let text = collect_text(&node);
        assert!(text.starts_with("-") || text.contains("-x"));
    }

    #[test]
    fn parse_binary_minus_gets_spacing() {
        let node = parse_equation("a - b");
        if let EqNode::Seq(nodes) = &node {
            // Should have Space nodes (binary minus gets spacing)
            let space_count = nodes
                .iter()
                .filter(|n| matches!(n, EqNode::Space(_)))
                .count();
            assert!(
                space_count >= 2,
                "Binary minus should have spaces: got {space_count}"
            );
        }
    }

    // ─── Relation symbol spacing ────────────────────────────────────

    #[test]
    fn parse_neq_gets_spacing() {
        let node = parse_equation("a \\neq b");
        fn has_space(node: &EqNode) -> bool {
            match node {
                EqNode::Space(_) => true,
                EqNode::Seq(nodes) => nodes.iter().any(has_space),
                _ => false,
            }
        }
        assert!(has_space(&node), "\\neq should get operator spacing");
    }

    #[test]
    fn parse_leq_gets_spacing() {
        let node = parse_equation("x \\leq y");
        fn has_space(node: &EqNode) -> bool {
            match node {
                EqNode::Space(_) => true,
                EqNode::Seq(nodes) => nodes.iter().any(has_space),
                _ => false,
            }
        }
        assert!(has_space(&node), "\\leq should get operator spacing");
    }

    // ─── Environments ───────────────────────────────────────────────

    #[test]
    fn parse_pmatrix() {
        let node = parse_equation("\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}");
        assert!(matches!(node, EqNode::Matrix { .. }));
        if let EqNode::Matrix { kind, rows } = &node {
            assert!(matches!(kind, MatrixKind::Paren));
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].len(), 2);
        }
    }

    #[test]
    fn parse_cases() {
        let node = parse_equation("\\begin{cases} x^2 & x > 0 \\\\ 0 & x = 0 \\end{cases}");
        assert!(matches!(node, EqNode::Cases { .. }));
        if let EqNode::Cases { rows } = &node {
            assert_eq!(rows.len(), 2);
        }
    }

    // ─── Special commands ───────────────────────────────────────────

    #[test]
    fn parse_text_block() {
        let node = parse_equation("\\text{hello world}");
        assert!(matches!(node, EqNode::TextBlock(_)));
        if let EqNode::TextBlock(s) = &node {
            assert_eq!(s, "hello world");
        }
    }

    #[test]
    fn parse_mathbb() {
        let node = parse_equation("\\mathbb{R}");
        assert!(matches!(node, EqNode::MathFont { .. }));
        if let EqNode::MathFont { kind, .. } = &node {
            assert!(matches!(kind, MathFontKind::Blackboard));
        }
    }

    #[test]
    fn parse_binom() {
        let node = parse_equation("\\binom{n}{k}");
        assert!(matches!(node, EqNode::Binom(_, _)));
    }

    #[test]
    fn parse_left_right_delimiters() {
        let node = parse_equation("\\left( x \\right)");
        assert!(matches!(node, EqNode::Delimited { .. }));
    }

    #[test]
    fn parse_overbrace() {
        let node = parse_equation("\\overbrace{a+b}^{n}");
        assert!(matches!(node, EqNode::Brace { over: true, .. }));
    }

    #[test]
    fn parse_overset() {
        let node = parse_equation("\\overset{def}{=}");
        assert!(matches!(node, EqNode::StackRel { over: true, .. }));
    }

    // ─── Spacing commands ───────────────────────────────────────────

    #[test]
    fn parse_quad_spacing() {
        let node = parse_equation("a \\quad b");
        if let EqNode::Seq(nodes) = &node {
            let has_wide_space = nodes
                .iter()
                .any(|n| matches!(n, EqNode::Space(w) if *w >= 18.0));
            assert!(has_wide_space, "\\quad should produce 18pt space");
        }
    }

    #[test]
    fn parse_thin_space() {
        let node = parse_equation("a\\,b");
        if let EqNode::Seq(nodes) = &node {
            let has_thin_space = nodes
                .iter()
                .any(|n| matches!(n, EqNode::Space(w) if *w > 0.0 && *w < 5.0));
            assert!(has_thin_space, "\\, should produce thin space");
        }
    }

    // ─── Accents ────────────────────────────────────────────────────

    #[test]
    fn parse_hat_accent() {
        let node = parse_equation("\\hat{x}");
        assert!(matches!(node, EqNode::Accent(_, AccentKind::Hat)));
    }

    #[test]
    fn parse_vec_accent() {
        let node = parse_equation("\\vec{v}");
        assert!(matches!(node, EqNode::Accent(_, AccentKind::Vec)));
    }

    // ─── latex_to_unicode ───────────────────────────────────────────

    #[test]
    fn unicode_greek_lowercase() {
        assert_eq!(latex_to_unicode("alpha"), Some("\u{03B1}".to_string()));
        assert_eq!(latex_to_unicode("omega"), Some("\u{03C9}".to_string()));
    }

    #[test]
    fn unicode_greek_uppercase() {
        assert_eq!(latex_to_unicode("Gamma"), Some("\u{0393}".to_string()));
        assert_eq!(latex_to_unicode("Sigma"), Some("\u{03A3}".to_string()));
    }

    #[test]
    fn unicode_operators() {
        assert_eq!(latex_to_unicode("leq"), Some("\u{2264}".to_string()));
        assert_eq!(latex_to_unicode("geq"), Some("\u{2265}".to_string()));
        assert_eq!(latex_to_unicode("neq"), Some("\u{2260}".to_string()));
        assert_eq!(latex_to_unicode("infty"), Some("\u{221E}".to_string()));
    }

    #[test]
    fn unicode_arrows() {
        assert_eq!(latex_to_unicode("rightarrow"), Some("\u{2192}".to_string()));
        assert_eq!(latex_to_unicode("implies"), Some("\u{21D2}".to_string()));
    }

    #[test]
    fn unicode_unknown_returns_none() {
        assert_eq!(latex_to_unicode("notacommand"), None);
    }

    // ─── Edge cases ─────────────────────────────────────────────────

    #[test]
    fn parse_nested_fractions() {
        let node = parse_equation("\\frac{\\frac{a}{b}}{c}");
        assert!(matches!(node, EqNode::Frac(_, _)));
        if let EqNode::Frac(numer, _) = &node {
            assert!(matches!(**numer, EqNode::Frac(_, _)));
        }
    }

    #[test]
    fn parse_nested_superscripts() {
        let node = parse_equation("x^{2^3}");
        assert!(matches!(node, EqNode::Sup(_, _)));
        if let EqNode::Sup(_, sup) = &node {
            assert!(matches!(**sup, EqNode::Sup(_, _)));
        }
    }

    #[test]
    fn parse_complex_expression() {
        // Should not panic
        let _node = parse_equation("\\frac{-b \\pm sqrt(b^2 - 4ac)}{2a}");
    }

    #[test]
    fn parse_complex_integral() {
        let _node = parse_equation("\\int_0^\\infty e^{-x^2} dx");
    }

    #[test]
    fn parse_complex_matrix() {
        let _node = parse_equation(
            "\\begin{pmatrix} \\cos\\theta & -\\sin\\theta \\\\ \\sin\\theta & \\cos\\theta \\end{pmatrix}",
        );
    }

    #[test]
    fn parse_euler_identity() {
        let _node = parse_equation("e^{i pi} + 1 = 0");
    }

    #[test]
    fn parse_epsilon_delta() {
        let _node = parse_equation(
            "\\forall \\epsilon > 0, \\exists \\delta > 0 : |x - a| < \\delta \\implies |f(x) - L| < \\epsilon",
        );
    }
}
