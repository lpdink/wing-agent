//! 字符级扫描原语（本项目自研，非上游代码）。
//!
//! 上游 [`crate::latex`] 的 parser 工作在 `Vec<char>` 上、且**不暴露位置**，所以凡是
//! "在看懂输入结构之后才能做的判断"（顶层 `&` / `\\`、环境配对、`\left`/`\right` 计数、
//! 多行环境切分）都要我们自己扫一遍。这里集中放这些扫描工具，避免每个模块各写一份
//! 花括号匹配。
//!
//! 所有下标都是 **`char` 下标**（不是字节下标），调用方一律先 `let chars: Vec<char> = ...`。

use std::ops::Range;

/// `\begin{env}` … `\end{env}` 的跨度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnvSpan {
    /// 环境名（原样，可能含 `*`）。
    pub name: String,
    /// `\begin` 的反斜杠所在下标。
    pub start: usize,
    /// `\begin{...}` 之后（body 起点）。
    pub body_start: usize,
    /// `\end{...}` 的反斜杠所在下标。
    pub body_end: usize,
    /// `\end{...}` 之后。
    pub end: usize,
}

/// 一次全量扫描得到的顶层结构信息。
///
/// "顶层" 的判据统一为：**花括号深度 0 且环境嵌套深度 0**（行/列分隔符单独统计，见
/// [`Structure::row_separator_count`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Structure {
    /// 顶层出现 `&`（上游 parser 会在这里 break，静默丢掉后半段）。
    pub top_level_column_sep: bool,
    /// 顶层出现 `\\`（同上，上游把它当行分隔符 break）。
    pub top_level_row_sep: bool,
    /// 花括号深度 0 的 `\\` 总数（含环境内部的）—— 每个至少产生一行。
    pub row_separator_count: usize,
    /// 花括号深度 0 的 `&` 总数（含环境内部的）—— 每个至少产生一列。
    pub column_separator_count: usize,
    /// 源码以孤立的反斜杠结尾（`x + \`）：上游会把裸 `\` 渲染出来。
    pub dangling_backslash: bool,
    /// 所有 `\begin{X}` 都有配对的、同名的 `\end{X}`。
    pub balanced_envs: bool,
    /// 顶层（非嵌套）环境跨度，按出现顺序。
    pub envs: Vec<EnvSpan>,
    /// `\left` 计数。
    pub left_count: usize,
    /// `\right` 计数。
    pub right_count: usize,
    /// 最大花括号嵌套深度。
    pub max_brace_depth: usize,
}

impl Default for Structure {
    fn default() -> Self {
        Self {
            top_level_column_sep: false,
            top_level_row_sep: false,
            row_separator_count: 0,
            column_separator_count: 0,
            dangling_backslash: false,
            // `balanced_envs` 是"没能证明不配对"的正面属性，默认视为成立，
            // 由扫描过程置 false。
            balanced_envs: true,
            envs: Vec::new(),
            left_count: 0,
            right_count: 0,
            max_brace_depth: 0,
        }
    }
}

/// 读取 `chars[i]`（要求是 `\`）处的命令名，返回 `(名字, 名字之后的第一个下标)`。
///
/// `\` 后面不是 ASCII 字母（`\{` `\,` `\ ` `\\` …）时返回 [`Option::None`]。
pub(crate) fn command_at(chars: &[char], i: usize) -> Option<(String, usize)> {
    if chars.get(i) != Some(&'\\') {
        return None;
    }
    let mut j = i + 1;
    if !chars.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut name = String::new();
    while let Some(&c) = chars.get(j) {
        if c.is_ascii_alphabetic() {
            name.push(c);
            j += 1;
        } else {
            break;
        }
    }
    Some((name, j))
}

/// 读取 `chars[i]`（要求是 `{`）处的分组内容，返回 `(内容, 配对 `}` 之后的下标)`。
pub(crate) fn brace_arg(chars: &[char], i: usize) -> Option<(String, usize)> {
    if chars.get(i) != Some(&'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut content = String::new();
    let mut j = i;
    while let Some(&c) = chars.get(j) {
        match c {
            '{' => {
                depth += 1;
                if depth > 1 {
                    content.push(c);
                }
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((content, j + 1));
                }
                content.push(c);
            }
            _ => content.push(c),
        }
        j += 1;
    }
    None
}

/// 全量扫描：顶层结构信息 + 顶层环境跨度。
///
/// 不返回 `Result`：所有异常都体现在 [`Structure`] 的字段里（`balanced_envs` /
/// 计数不等），由 [`crate::guard`] 决定怎么拒绝。
pub(crate) fn scan(chars: &[char]) -> Structure {
    let mut st = Structure::default();
    let mut brace_depth = 0usize;
    // (环境名, `\begin` 下标, body 起点)
    let mut stack: Vec<(String, usize, usize)> = Vec::new();

    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '{' => {
                brace_depth += 1;
                st.max_brace_depth = st.max_brace_depth.max(brace_depth);
                i += 1;
            }
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                i += 1;
            }
            '&' => {
                if brace_depth == 0 {
                    st.column_separator_count += 1;
                    if stack.is_empty() {
                        st.top_level_column_sep = true;
                    }
                }
                i += 1;
            }
            '\\' => {
                // 行分隔符 `\\`：注意它也可能出现在 `\\[3pt]` 里，这里只关心是否存在。
                if chars.get(i + 1) == Some(&'\\') {
                    if brace_depth == 0 {
                        st.row_separator_count += 1;
                        if stack.is_empty() {
                            st.top_level_row_sep = true;
                        }
                    }
                    i += 2;
                    continue;
                }
                // 孤立的反斜杠（后面什么都没有）：上游会把它当未知命令兜底成裸 `\`
                if i + 1 >= chars.len() {
                    st.dangling_backslash = true;
                    i += 1;
                    continue;
                }
                match command_at(chars, i) {
                    Some((name, after)) => {
                        match name.as_str() {
                            "left" => st.left_count += 1,
                            "right" => st.right_count += 1,
                            "begin" => match brace_arg(chars, after) {
                                Some((env, after_arg)) => {
                                    stack.push((env, i, after_arg));
                                    i = after_arg;
                                    continue;
                                }
                                None => {
                                    st.balanced_envs = false;
                                    i = after;
                                    continue;
                                }
                            },
                            "end" => match brace_arg(chars, after) {
                                Some((env, after_arg)) => {
                                    match stack.pop() {
                                        Some((open, start, body_start)) if open == env => {
                                            // 只有最外层配对才登记成"顶层环境"
                                            if stack.is_empty() {
                                                st.envs.push(EnvSpan {
                                                    name: env,
                                                    start,
                                                    body_start,
                                                    body_end: i,
                                                    end: after_arg,
                                                });
                                            }
                                        }
                                        _ => st.balanced_envs = false,
                                    }
                                    i = after_arg;
                                    continue;
                                }
                                None => {
                                    st.balanced_envs = false;
                                    i = after;
                                    continue;
                                }
                            },
                            _ => {}
                        }
                        i = after;
                    }
                    // `\` 后面不是字母：连同被吃掉的那个字符一起跳过（`\{` `\,` `\ `）。
                    None => i += 2,
                }
            }
            _ => i += 1,
        }
    }

    if !stack.is_empty() {
        st.balanced_envs = false;
    }
    st
}

/// 在 `chars` 内按顶层分隔符切分，返回每段的下标区间。
///
/// - `row_sep == true`：按 `\\` 切分（并跳过 `\\[3pt]` 这类可选间距参数）；
/// - `row_sep == false`：按 `&` 切分。
///
/// "顶层" 同样是"花括号深度 0 且环境嵌套深度 0"，所以单元格里嵌
/// `\begin{cases} a & b \\ c & d \end{cases}` 不会被误切。
///
/// 返回值**至少一段**（可能为空段）；是否丢弃空段由调用方决定。
pub(crate) fn split_top_level(chars: &[char], row_sep: bool) -> Vec<Range<usize>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut brace_depth = 0usize;
    let mut env_depth = 0usize;

    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '{' => {
                brace_depth += 1;
                i += 1;
            }
            '}' => {
                brace_depth = brace_depth.saturating_sub(1);
                i += 1;
            }
            '&' if !row_sep && brace_depth == 0 && env_depth == 0 => {
                parts.push(start..i);
                start = i + 1;
                i += 1;
            }
            '\\' => {
                if chars.get(i + 1) == Some(&'\\') {
                    if row_sep && brace_depth == 0 && env_depth == 0 {
                        parts.push(start..i);
                        i += 2;
                        // `\\[3pt]`：跳过一个不含命令的短方括号参数
                        if chars.get(i) == Some(&'[')
                            && let Some(rel) = chars[i..].iter().position(|&c| c == ']')
                            && rel <= 24
                            && !chars[i..i + rel].contains(&'\\')
                        {
                            i += rel + 1;
                        }
                        start = i;
                        continue;
                    }
                    i += 2;
                    continue;
                }
                match command_at(chars, i) {
                    Some((name, after)) => {
                        match name.as_str() {
                            "begin" => env_depth += 1,
                            "end" => env_depth = env_depth.saturating_sub(1),
                            _ => {}
                        }
                        i = after;
                    }
                    None => i += 2,
                }
            }
            _ => i += 1,
        }
    }
    parts.push(start..chars.len());
    parts
}

/// 把 `chars[a..b]` 取成 `String`。
pub(crate) fn slice(chars: &[char], range: &Range<usize>) -> String {
    chars[range.clone()].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn command_at_reads_alpha_name() {
        let c = chars(r"\frac{a}{b}");
        assert_eq!(command_at(&c, 0), Some(("frac".to_string(), 5)));
        // 非字母命令不算命令
        assert_eq!(command_at(&chars(r"\{"), 0), None);
        assert_eq!(command_at(&chars(r"\\"), 0), None);
    }

    #[test]
    fn brace_arg_handles_nesting() {
        let c = chars(r"{a{b}c}d");
        assert_eq!(brace_arg(&c, 0), Some(("a{b}c".to_string(), 7)));
        assert_eq!(brace_arg(&chars("x"), 0), None);
        // 未闭合
        assert_eq!(brace_arg(&chars("{a"), 0), None);
    }

    #[test]
    fn scan_flags_top_level_separators() {
        let st = scan(&chars(r"a & b"));
        assert!(st.top_level_column_sep);
        assert!(!st.top_level_row_sep);

        let st = scan(&chars(r"a \\ b"));
        assert!(st.top_level_row_sep);

        // 环境内部的 `&` / `\\` 不算顶层
        let st = scan(&chars(r"\begin{cases} a & b \\ c & d \end{cases}"));
        assert!(!st.top_level_column_sep);
        assert!(!st.top_level_row_sep);
        assert!(st.balanced_envs);
        assert_eq!(st.envs.len(), 1);
        assert_eq!(st.envs[0].name, "cases");
    }

    #[test]
    fn scan_records_only_outermost_envs() {
        let st = scan(&chars(
            r"\begin{align} a &= \begin{cases} x \end{cases} \end{align}",
        ));
        assert!(st.balanced_envs);
        assert_eq!(st.envs.len(), 1);
        assert_eq!(st.envs[0].name, "align");
    }

    #[test]
    fn scan_detects_unbalanced_env() {
        let st = scan(&chars(r"\begin{align} a &= b"));
        assert!(!st.balanced_envs);

        let st = scan(&chars(r"\end{align}"));
        assert!(!st.balanced_envs);

        let st = scan(&chars(r"\begin{align} a \end{gather}"));
        assert!(!st.balanced_envs);
    }

    #[test]
    fn scan_counts_delimiters() {
        let st = scan(&chars(r"\left( x \right)"));
        assert_eq!(st.left_count, 1);
        assert_eq!(st.right_count, 1);

        let st = scan(&chars(r"\left( x"));
        assert_eq!((st.left_count, st.right_count), (1, 0));
    }

    #[test]
    fn scan_tracks_brace_depth() {
        let st = scan(&chars(r"{{x}}"));
        assert_eq!(st.max_brace_depth, 2);
    }

    #[test]
    fn scan_counts_separators_outside_braces() {
        let st = scan(&chars(r"\begin{cases} a & b \\ c & d \end{cases}"));
        assert_eq!(st.column_separator_count, 2);
        assert_eq!(st.row_separator_count, 1);

        // 花括号内的分隔符不计入（它们是 `\text{...}` 之类的字面内容）
        let st = scan(&chars(r"\text{a & b}"));
        assert_eq!(st.column_separator_count, 0);
        assert!(!st.top_level_column_sep);
    }

    #[test]
    fn scan_detects_dangling_backslash() {
        assert!(scan(&chars(r"x + \")).dangling_backslash);
        assert!(!scan(&chars(r"x + \ ")).dangling_backslash);
        assert!(!scan(&chars(r"\frac{a}{b}")).dangling_backslash);
    }

    #[test]
    fn split_rows_skips_optional_spacing() {
        let c = chars(r"a \\[3pt] b \\ c");
        let parts = split_top_level(&c, true);
        assert_eq!(parts.len(), 3);
        assert_eq!(slice(&c, &parts[0]), "a ");
        assert_eq!(slice(&c, &parts[1]), " b ");
        assert_eq!(slice(&c, &parts[2]), " c");
    }

    #[test]
    fn split_cols_respects_nested_env() {
        let c = chars(r"a & \begin{cases} x & y \\ z & w \end{cases} & b");
        let parts = split_top_level(&c, false);
        assert_eq!(parts.len(), 3);
        assert_eq!(slice(&c, &parts[0]), "a ");
        assert_eq!(
            slice(&c, &parts[1]),
            r" \begin{cases} x & y \\ z & w \end{cases} "
        );
        assert_eq!(slice(&c, &parts[2]), " b");
    }

    #[test]
    fn split_rows_keeps_braces_intact() {
        let c = chars(r"\frac{a}{b} \\ c");
        let parts = split_top_level(&c, true);
        assert_eq!(parts.len(), 2);
        assert_eq!(slice(&c, &parts[0]), r"\frac{a}{b} ");
    }
}
