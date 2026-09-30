//! LaTeX 子集归一化（本项目自研，非上游代码）。
//!
//! 上游 parser 只认一部分命令，其余会走"未知命令兜底"变成 `\name` 字面量漏到输出里。
//! 这个模块做一遍**词法级重写**：把科学写作里高频、但语义上不需要渲染细节的写法，改写成
//! 上游认识的等价形式。每条规则都在注释里给了实测理由，并且都有对应单测。
//!
//! 规则编号（`N1`–`N11`，与设计文档 `design.md → D6` 一致）：
//!
//! - `N1` 剥掉整体包裹的 `$$…$$` / `$…$` / `\[…\]` / `\(…\)`（防御性，正常路径由上层剥）；
//! - `N2`/`N3`/`N4` 删掉无渲染语义的命令（`\displaystyle` / `\limits` / `\bigl` …）；
//! - `N5` 分式别名（`\dfrac` `\tfrac` `\cfrac` → `\frac`）；
//! - `N6` 定界符命令 → 字面字符（`\left\{` → `\left{`，`\left\|` → `\left‖`）；
//! - `N7` `\operatorname{X}` → `\text{X}`；
//! - `N8` `\colon` → `:`；
//! - `N9` 删除无渲染语义的元数据（`\label{X}` / `\nonumber` / `\notag`）；
//! - `N10` `\tag{X}` → `(X)`；
//! - `N11` `\sqrt[n]{X}` → `{}^{n}\sqrt{X}`；
//! - `N12` 符号别名（`\mid` → `|`，`\lt` / `\gt` → `<` / `>`）。
//!
//! **不做**的事：不解析 AST、不改语义、不猜宽度。拿不准就原样保留，交给
//! [`crate::guard`] 的输出泄漏自检拒绝整条公式（宁可降级为字面量，也不显示半截）。

use crate::scan::{brace_arg, command_at};

/// `N2`/`N3`/`N4`：无渲染语义的命令，直接删除（含 `\bigl` 这类带 `l`/`r`/`m` 后缀的尺寸命令）。
const DROP: &[&str] = &[
    "displaystyle",
    "textstyle",
    "scriptstyle",
    "scriptscriptstyle",
    "limits",
    "nolimits",
    "big",
    "bigl",
    "bigr",
    "bigm",
    "Big",
    "Bigl",
    "Bigr",
    "Bigm",
    "bigg",
    "biggl",
    "biggr",
    "biggm",
    "Bigg",
    "Biggl",
    "Biggr",
    "Biggm",
    "nonumber",
    "notag",
];

/// `N9`：带一个 `{...}` 参数、且**整体删除**的命令（无渲染语义的元数据）。
const DROP_WITH_ARG: &[&str] = &["label"];

/// `N5`：分式变体 → `\frac`。
const FRAC_ALIASES: &[&str] = &["dfrac", "tfrac", "cfrac"];

/// `N6`：`\left` / `\right` 后面允许出现的定界符命令 → 字面字符。
///
/// `\left\{` 之所以必须重写：上游 `parse_left_right` 只 `advance()` **一个字符**当定界符，
/// 于是 `\left\{ x \right\}` 变成"定界符 = `\`"、`\right` 被当正文，输出 `\ x \right}`
/// （实测）。`\|` / `\Vert` 归一成 `‖` 是为了让上游 `build_delimiter` 命中双竖线分支
/// （若归一成 `|` 会把范数符号画成绝对值）。
const DELIMS: &[(&str, char)] = &[
    ("lbrace", '{'),
    ("rbrace", '}'),
    ("lvert", '|'),
    ("rvert", '|'),
    ("vert", '|'),
    ("lVert", '‖'),
    ("rVert", '‖'),
    ("Vert", '‖'),
    ("langle", '⟨'),
    ("rangle", '⟩'),
    ("lfloor", '⌊'),
    ("rfloor", '⌋'),
    ("lceil", '⌈'),
    ("rceil", '⌉'),
];

/// `N12`：上游符号表里缺、但科学写作高频的符号命令 → 字面字符。
///
/// 上游 `latex_to_unicode` 覆盖 130+ 符号，但 `\mid`（条件概率 `P(A \mid B)` 里的竖线）
/// 与 `\lt` / `\gt` 不在其中，会走未知命令兜底漏成字面量。这三个是纯别名，重写零风险。
///
/// `N13`（review r1 的 N1）：`\ldots` / `\dots` 上游一律映射成中线点 `⋯`，但 LaTeX 语义
/// 是"基线省略号" `…`（`\cdots` 才是中线点）。这里把前两者改写成 `…`。
const SYMBOL_ALIASES: &[(&str, char)] = &[
    ("mid", '|'),
    ("lt", '<'),
    ("gt", '>'),
    ("ldots", '…'),
    ("dots", '…'),
];

/// `N14`：源码里的**控制字符**（review r1 的 B3）。
///
/// `\n` / `\t` 之类会作为普通字符进入网格，破坏"行内结果单行 / 显示结果是字符网格"的契约，
/// 而 LLM 输出的多行公式是常态。这里统一折叠成**一个空格**（换行在数学表达式里等价于
/// 空白）。`\r\n` 折叠后也只留一个空格。
const CONTROL_TO_SPACE: &[char] = &[
    '\n', '\r', '\t', '\u{000b}', '\u{000c}', '\u{0085}', '\u{2028}', '\u{2029}', '\u{00a0}',
];

/// `N14` 的第二类：**零宽格式字符**，肉眼不可见、只会在宽度记账里制造噪声，直接删掉。
const ZERO_WIDTH_TO_DROP: &[char] = &[
    '\u{200b}', // ZERO WIDTH SPACE
    '\u{200c}', // ZERO WIDTH NON-JOINER
    '\u{200d}', // ZERO WIDTH JOINER
    '\u{feff}', // ZERO WIDTH NO-BREAK SPACE / BOM
    '\u{00ad}', // SOFT HYPHEN
];

/// 定界符命令名 → 字面字符（`lbrace` → `{`，`lVert` → `‖` …）。
pub(crate) fn delimiter_command_char(name: &str) -> Option<char> {
    DELIMS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// 归一化入口。输入是"公式正文"（不含 markdown 的 `$`），输出可交给
/// [`crate::latex::parse_equation`]。
pub(crate) fn normalize(src: &str) -> String {
    let chars: Vec<char> = sanitize(src).chars().collect();
    let (start, end) = strip_wrapping(&chars, 0, chars.len());
    let mut out = String::with_capacity(end - start);
    rewrite(&chars, start, end, &mut out);
    out.trim().to_string()
}

/// `N14`：控制字符 → 空格、零宽字符 → 删除。**必须在任何扫描之前做**，否则
/// `\n` 会以普通字符的身份进入 parser 与网格（review r1 的 B3）。
fn sanitize(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut pending_space = false;
    for ch in src.chars() {
        if ZERO_WIDTH_TO_DROP.contains(&ch) {
            continue;
        }
        if CONTROL_TO_SPACE.contains(&ch) || ch.is_control() {
            // 连续控制字符折叠成一个空格（`\r\n` 只留一个）
            if !pending_space && !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if pending_space {
        out.push(' ');
    }
    out
}

/// `N1`：剥掉完整包裹源码的 `$$…$$` / `$…$` / `\[…\]` / `\(…\)`（最多两层）。
fn strip_wrapping(chars: &[char], mut start: usize, mut end: usize) -> (usize, usize) {
    for _ in 0..2 {
        while start < end && chars[start].is_whitespace() {
            start += 1;
        }
        while end > start && chars[end - 1].is_whitespace() {
            end -= 1;
        }
        let len = end - start;
        if len >= 4
            && chars[start] == '$'
            && chars[start + 1] == '$'
            && chars[end - 2] == '$'
            && chars[end - 1] == '$'
        {
            start += 2;
            end -= 2;
            continue;
        }
        if len >= 2 && chars[start] == '$' && chars[end - 1] == '$' {
            start += 1;
            end -= 1;
            continue;
        }
        if len >= 4
            && chars[start] == '\\'
            && chars[start + 1] == '['
            && chars[end - 2] == '\\'
            && chars[end - 1] == ']'
        {
            start += 2;
            end -= 2;
            continue;
        }
        if len >= 4
            && chars[start] == '\\'
            && chars[start + 1] == '('
            && chars[end - 2] == '\\'
            && chars[end - 1] == ')'
        {
            start += 2;
            end -= 2;
            continue;
        }
        break;
    }
    (start, end)
}

/// 主重写循环。逐字符拷贝，遇到命令就查表。
fn rewrite(chars: &[char], start: usize, end: usize, out: &mut String) {
    let mut i = start;
    while i < end {
        if chars[i] != '\\' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        // 单字符命令（`\{` `\}` `\,` `\ ` `\\` …）：原样保留
        if i + 1 < end && !chars[i + 1].is_ascii_alphabetic() {
            out.push('\\');
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }

        let Some((name, after)) = command_at(chars, i) else {
            // 末尾孤立的反斜杠：原样保留，交给 guard 拒绝
            out.push('\\');
            i += 1;
            continue;
        };

        match name.as_str() {
            // N9：连同 `{...}` 参数一起删除
            n if DROP_WITH_ARG.contains(&n) => {
                i = skip_optional_brace_arg(chars, after, end);
            }
            // N2 / N3 / N4
            n if DROP.contains(&n) => {
                i = after;
            }
            // N5
            n if FRAC_ALIASES.contains(&n) => {
                out.push_str("\\frac");
                i = after;
            }
            // N8
            "colon" => {
                out.push(':');
                i = after;
            }
            // N12
            n if SYMBOL_ALIASES.iter().any(|(name, _)| *name == n) => {
                let ch = SYMBOL_ALIASES
                    .iter()
                    .find(|(name, _)| *name == n)
                    .map(|(_, ch)| *ch)
                    .unwrap_or('?');
                out.push(ch);
                i = after;
            }
            // N6：`\left` / `\right` 后面接定界符
            "left" | "right" => {
                let (text, next) = rewrite_delimiter(chars, after, end);
                out.push('\\');
                out.push_str(&name);
                out.push_str(&text);
                i = next;
            }
            // N7：`\operatorname{X}` → `\text{X}`
            "operatorname" => match brace_arg(chars, skip_star(chars, after, end)) {
                Some((content, next)) => {
                    out.push_str("\\text{");
                    out.push_str(&content);
                    out.push('}');
                    i = next;
                }
                None => {
                    out.push('\\');
                    out.push_str(&name);
                    i = after;
                }
            },
            // N10：`\tag{X}` → `(X)`
            "tag" => match brace_arg(chars, skip_star(chars, after, end)) {
                Some((content, next)) => {
                    out.push('(');
                    out.push_str(&content);
                    out.push(')');
                    i = next;
                }
                None => {
                    out.push('\\');
                    out.push_str(&name);
                    i = after;
                }
            },
            // N11：`\sqrt[n]{X}` → `{}^{n}\sqrt{X}`
            "sqrt" => match rewrite_nth_root(chars, after, end) {
                Some((rewritten, next)) => {
                    out.push_str(&rewritten);
                    i = next;
                }
                None => {
                    out.push_str("\\sqrt");
                    i = after;
                }
            },
            _ => {
                out.push('\\');
                out.push_str(&name);
                i = after;
            }
        }
    }
}

/// 跳过可选的一个 `*`（`\tag*` / `\operatorname*`）。
fn skip_star(chars: &[char], i: usize, end: usize) -> usize {
    if i < end && chars[i] == '*' { i + 1 } else { i }
}

/// 跳过 `{...}`（如果存在），返回其后的下标；没有 `{` 时原样返回。
fn skip_optional_brace_arg(chars: &[char], i: usize, end: usize) -> usize {
    let mut j = i;
    while j < end && chars[j] == ' ' {
        j += 1;
    }
    match brace_arg(chars, j) {
        Some((_, next)) if next <= end => next,
        _ => i,
    }
}

/// `N6`：读 `\left` / `\right` 后面的定界符，返回 `(要追加的文本, 之后的下标)`。
///
/// - 定界符命令（`\{` `\|` `\lbrace` …）→ 对应字面字符（多字符命令会被整体吃掉）；
/// - 其他单字符（`(` `)` `[` `]` `|` `.` …）→ 原样保留一个字符；
/// - 认不出来（例如 `\left\foo`）→ 返回空串并**不前进**，让主循环按普通命令处理它
///   （此时与上游行为一致：上游会把 `\` 当定界符）。
fn rewrite_delimiter(chars: &[char], i: usize, end: usize) -> (String, usize) {
    let mut j = i;
    // 上游 `parse_command` 会吃掉命令后的一个空格，这里同步跳过以便识别 `\left \{`
    if j < end && chars[j] == ' ' {
        j += 1;
    }
    if j >= end {
        return (String::new(), i);
    }
    let c = chars[j];

    if c == '\\' {
        if let Some((name, after)) = command_at(chars, j) {
            return match delimiter_command_char(&name) {
                Some(ch) => (ch.to_string(), after),
                None => (String::new(), i),
            };
        }
        // `\{` `\}` `\|`
        return match chars.get(j + 1) {
            Some('{') => ("{".to_string(), j + 2),
            Some('}') => ("}".to_string(), j + 2),
            Some('|') => ("‖".to_string(), j + 2),
            _ => (String::new(), i),
        };
    }

    (c.to_string(), j + 1)
}

/// `N11`：`\sqrt[n]{X}` → `{}^{n}\sqrt{X}`。
///
/// 上游把 `[` 当成根号体，`\sqrt[3]{x}` 会渲染成 `√[3]x`（实测）：内容没丢但语义错。
/// 改写成"上标 `n` + 普通根号"，`³√x` 与手写习惯一致。
///
/// 不是 nth-root 形态时返回 [`Option::None`]，走原路径。
fn rewrite_nth_root(chars: &[char], i: usize, end: usize) -> Option<(String, usize)> {
    let mut j = i;
    while j < end && chars[j] == ' ' {
        j += 1;
    }
    if chars.get(j) != Some(&'[') {
        return None;
    }
    let close = chars[j..end].iter().position(|&c| c == ']')?;
    if close == 0 || close > 24 {
        return None;
    }
    let index: String = chars[j + 1..j + close].iter().collect();
    if index.contains('\\') {
        return None;
    }
    let (body, next) = brace_arg(chars, j + close + 1)?;
    Some((format!("{{}}^{{{}}}\\sqrt{{{}}}", index, body), next))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        normalize(s)
    }

    #[test]
    fn n1_strips_wrapping_delimiters() {
        assert_eq!(n("$$x + 1$$"), "x + 1");
        assert_eq!(n("$x + 1$"), "x + 1");
        assert_eq!(n(r"\[x + 1\]"), "x + 1");
        assert_eq!(n(r"\(x + 1\)"), "x + 1");
        assert_eq!(n("  $$ x $$  "), "x");
        // 只剥完整包裹的那一层
        assert_eq!(n("$x + 1"), "$x + 1");
        assert_eq!(n(r"\[x + 1"), r"\[x + 1");
    }

    #[test]
    fn n2_n3_n4_drop_style_and_sizing_commands() {
        assert_eq!(n(r"\displaystyle \int_0^1 x dx"), r"\int_0^1 x dx");
        assert_eq!(n(r"\int\limits_0^1 x dx"), r"\int_0^1 x dx");
        assert_eq!(n(r"\bigl( x \bigr)"), "( x )");
        assert_eq!(n(r"\Bigg[ x \Bigg]"), "[ x ]");
        assert_eq!(n(r"\nonumber a = b"), "a = b");
    }

    #[test]
    fn n5_fraction_aliases() {
        assert_eq!(n(r"\dfrac{a}{b}"), r"\frac{a}{b}");
        assert_eq!(n(r"\tfrac{a}{b}"), r"\frac{a}{b}");
        assert_eq!(n(r"\cfrac{1}{1+x}"), r"\frac{1}{1+x}");
    }

    #[test]
    fn n6_delimiter_commands_become_literal_chars() {
        assert_eq!(n(r"\left\{ x \right\}"), r"\left{ x \right}");
        assert_eq!(n(r"\left\| x \right\|"), "\\left‖ x \\right‖");
        assert_eq!(n(r"\left\lbrace x \right\rbrace"), r"\left{ x \right}");
        assert_eq!(n(r"\left\lfloor x \right\rfloor"), "\\left⌊ x \\right⌋");
        assert_eq!(n(r"\left\langle x \right\rangle"), "\\left⟨ x \\right⟩");
        // 普通单字符定界符原样
        assert_eq!(n(r"\left( x \right)"), r"\left( x \right)");
        assert_eq!(n(r"\left. x \right."), r"\left. x \right.");
        // 命令后带空格
        assert_eq!(n(r"\left \{ x \right \}"), r"\left{ x \right}");
        // 认不出来的定界符：保持原样（交给 guard）
        assert_eq!(n(r"\left\weird x"), r"\left\weird x");
    }

    #[test]
    fn n7_operatorname_alias() {
        assert_eq!(n(r"\operatorname{softmax}(z)"), r"\text{softmax}(z)");
        assert_eq!(n(r"\operatorname*{argmax}"), r"\text{argmax}");
    }

    #[test]
    fn n8_colon_alias() {
        assert_eq!(n(r"f\colon A \to B"), r"f: A \to B");
    }

    #[test]
    fn n9_drops_metadata_commands() {
        assert_eq!(n(r"a = b \label{eq:1}"), "a = b");
        assert_eq!(n(r"a = b \label{eq:1} c"), "a = b  c");
    }

    #[test]
    fn n10_tag_becomes_text() {
        assert_eq!(n(r"a = b \tag{1}"), "a = b (1)");
        assert_eq!(n(r"a = b \tag*{2}"), "a = b (2)");
        // 没有参数时保持原样
        assert_eq!(n(r"a = b \tag"), r"a = b \tag");
    }

    #[test]
    fn n11_nth_root() {
        assert_eq!(n(r"\sqrt[3]{x}"), r"{}^{3}\sqrt{x}");
        assert_eq!(n(r"\sqrt[n+1]{\frac{a}{b}}"), r"{}^{n+1}\sqrt{\frac{a}{b}}");
        // 普通根号不动
        assert_eq!(n(r"\sqrt{x}"), r"\sqrt{x}");
        // 无 body 时不改写
        assert_eq!(n(r"\sqrt[3]"), r"\sqrt[3]");
        // 索引里有命令时不改写
        assert_eq!(n(r"\sqrt[\alpha]{x}"), r"\sqrt[\alpha]{x}");
    }

    #[test]
    fn n12_symbol_aliases() {
        assert_eq!(n(r"P(A \mid B)"), "P(A | B)");
        assert_eq!(n(r"a \lt b \gt c"), "a < b > c");
    }

    #[test]
    fn n13_ellipsis_aliases() {
        assert_eq!(n(r"a \ldots b"), "a … b");
        assert_eq!(n(r"a \dots b"), "a … b");
        // 中线点保持上游行为
        assert_eq!(n(r"a \cdots b"), r"a \cdots b");
    }

    #[test]
    fn n14_control_characters_become_spaces() {
        assert_eq!(n("a\nb"), "a b");
        assert_eq!(n("a\r\nb"), "a b");
        assert_eq!(n("a\tb"), "a b");
        assert_eq!(n("a\n\n\nb"), "a b");
        assert_eq!(n("x = 1\n"), "x = 1");
        assert_eq!(n("\n x = 1"), "x = 1");
        assert_eq!(n("a\u{000b}b\u{000c}c"), "a b c");
    }

    #[test]
    fn n14_zero_width_characters_are_dropped() {
        assert_eq!(n("a\u{200b}b"), "ab");
        assert_eq!(n("a\u{feff}b"), "ab");
        assert_eq!(n("a\u{00ad}b"), "ab");
        // 只剩零宽字符 → 空
        assert_eq!(n("\u{200b}"), "");
    }

    #[test]
    fn n14_never_leaves_control_characters_behind() {
        for src in [
            "a\nb",
            "a\tb",
            "a\rb",
            "\u{0}b",
            "a\u{1}b",
            "x = \\frac{1}{2}\n",
            "\n\n",
        ] {
            let out = n(src);
            assert!(
                !out.chars().any(|c| c.is_control()),
                "control char survived: {src:?} -> {out:?}"
            );
        }
    }

    #[test]
    fn leaves_unknown_commands_alone() {
        // 未知命令原样保留 —— 由 guard 的输出泄漏自检拒绝整条公式
        assert_eq!(n(r"\ce{2H2O}"), r"\ce{2H2O}");
        assert_eq!(n(r"\weirdcmd"), r"\weirdcmd");
    }

    #[test]
    fn leaves_known_commands_alone() {
        for s in [
            r"\frac{-b \pm \sqrt{b^2-4ac}}{2a}",
            r"\sum_{i=1}^{n} x_i \quad \alpha",
            r"\hat{H}\psi = E\psi",
            r"\text{ where }",
        ] {
            assert_eq!(n(s), s);
        }
    }

    #[test]
    fn handles_trailing_backslash() {
        assert_eq!(n(r"x + \"), r"x + \");
    }

    #[test]
    fn normalization_is_idempotent_on_known_inputs() {
        for s in [
            r"\frac{a}{b}",
            r"\left\{ \frac{a}{b} \right\}",
            r"\sqrt[3]{x}",
            r"\operatorname{softmax}(z)",
            r"\dfrac{a}{b}",
            r"a = b \tag{1}",
            r"x + 1",
        ] {
            let once = n(s);
            let twice = n(&once);
            assert_eq!(once, twice, "not idempotent for {s}");
        }
    }
}
