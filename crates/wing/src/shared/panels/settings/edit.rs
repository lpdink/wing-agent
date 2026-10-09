//! 内联编辑器：单行缓冲 + 本地校验（**模态**，见 design §20 D20）。
//!
//! 编辑器激活时只有字符 / `Backspace` / `Delete` / `←` `→` / `Home` `End` / `Ctrl+U` /
//! `Enter` / `Esc` 有效，`↑`/`↓`/`Tab` 与其余一切**忽略** —— 值有类型与约束，不允许
//! 「半截编辑」这种中间态。
//!
//! **密文缓冲没有任何公开出口**：`EditState` 的字段全私有，对外只有
//! [`EditState::visible_buffer`]（密文 = `•` × 长度）与 [`EditState::buffer_len`]。
//! 「缓冲区永不明文渲染」（design §13.2）由类型系统保证，而不是纪律。
//!
//! 本地校验是**提交前的 UX 糖**，权威永远是后端 `set` 的 `problems`（§7.3）：因此
//! pattern 只做尽力而为的子集匹配（仓库没有 regex 依赖，不为这一步加依赖），
//! 不支持的构造**跳过**而不是误报（design D12）。

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use serde_json::Value;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;

use super::doc::Root;

/// 编辑器认得的标量类型（bool 没有编辑器，enum 走内联选择项）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    Str,
    Int,
    Float,
    Secret,
    /// freeform map（`extra_body`，单行 JSON 编辑器；design §13.5）。
    Json,
}

/// 从声明读出的本地约束（范围 / 长度 / pattern / 可空）。
#[derive(Debug, Clone, PartialEq)]
pub struct Constraints {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub exclusive_min: bool,
    pub exclusive_max: bool,
    pub min_length: Option<i64>,
    pub pattern: Option<String>,
    pub nullable: bool,
}

impl Constraints {
    /// 从 catalog 节点抽约束。
    pub(crate) fn from_node(node: &SettingNode) -> Self {
        Self {
            min: node.min,
            max: node.max,
            exclusive_min: node.exclusive_min,
            exclusive_max: node.exclusive_max,
            min_length: node.min_length,
            pattern: node.pattern.clone(),
            nullable: node.nullable,
        }
    }
}

/// 一次按键对编辑器的影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditEvent {
    /// 键被吃掉，还在编辑。
    None,
    /// 取消（Esc；密文的空缓冲提交等价于取消）。
    Cancel,
    /// 请求提交 —— 调用方接着 [`EditState::validate`]。
    Submit,
}

/// 内联编辑器状态。
#[derive(Debug, Clone)]
pub struct EditState {
    root: Root,
    path: String,
    kind: ScalarKind,
    buffer: String,
    /// 光标位置（**字符**索引）。
    cursor: usize,
    error: Option<String>,
    required: bool,
    constraints: Constraints,
    /// 打开编辑器时缓冲所代表的值（校验通过才有）——AD3 的「无操作」判据。
    initial_value: Option<Value>,
}

impl EditState {
    /// 打开编辑器。密文的初值恒为空（design §13.2：缓冲区从空开始）。
    pub(crate) fn open(
        root: Root,
        path: String,
        kind: ScalarKind,
        required: bool,
        constraints: Constraints,
        initial: String,
    ) -> Self {
        let buffer = if kind == ScalarKind::Secret {
            String::new()
        } else {
            initial
        };
        let cursor = buffer.chars().count();
        // AD3：记住「打开时展示的值」。提交值与之相同 ⇒ 无操作（不写入、不标脏），
        // 于是「打开 → 不改 → Enter」不会把声明默认值物化成显式覆盖。
        let initial_value = validate_buffer(kind, &buffer, required, &constraints).ok();
        Self {
            root,
            path,
            kind,
            buffer,
            cursor,
            error: None,
            required,
            constraints,
            initial_value,
        }
    }

    /// AD3：这次提交与「打开时展示的值」等价吗（等价 = 该次提交视为无操作）。
    pub(crate) fn commits_unchanged(&self, committed: &Value) -> bool {
        self.initial_value.as_ref() == Some(committed)
    }

    // ── 只读访问器（08 渲染） ─────────────────────────────────

    pub fn root(&self) -> Root {
        self.root
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn kind(&self) -> ScalarKind {
        self.kind
    }

    /// 光标位置（字符索引）。
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn required(&self) -> bool {
        self.required
    }

    pub fn constraints(&self) -> &Constraints {
        &self.constraints
    }

    /// 渲染用的缓冲区内容：密文 = `•` × 长度（原始缓冲在这里没有出口）。
    pub fn visible_buffer(&self) -> String {
        if self.is_secret() {
            "•".repeat(self.buffer.chars().count())
        } else {
            self.buffer.clone()
        }
    }

    /// 缓冲的字符数（掩码长度 / 光标块定位）。
    pub fn buffer_len(&self) -> usize {
        self.buffer.chars().count()
    }

    pub fn is_secret(&self) -> bool {
        self.kind == ScalarKind::Secret
    }

    // ── 按键 ──────────────────────────────────────────────────

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> EditEvent {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u' | 'U') if ctrl => {
                self.buffer.clear();
                self.cursor = 0;
                self.error = None;
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert(c);
                self.error = None;
            }
            KeyCode::Backspace => {
                self.backspace();
                self.error = None;
            }
            KeyCode::Delete => {
                self.delete();
                self.error = None;
            }
            KeyCode::Left => self.move_cursor(-1),
            KeyCode::Right => self.move_cursor(1),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.buffer.chars().count(),
            KeyCode::Enter => {
                // 密文的空提交 = 保留原值不变（§13.2），不是错误也不是编辑。
                if self.is_secret() && self.buffer.is_empty() {
                    return EditEvent::Cancel;
                }
                return EditEvent::Submit;
            }
            KeyCode::Esc => return EditEvent::Cancel,
            // ↑/↓/Tab/其余一切：模态忽略（D20）。
            _ => {}
        }
        EditEvent::None
    }

    /// 记录一次提交失败的本地原因（08 渲染在行下方）。
    pub(crate) fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    fn insert(&mut self, c: char) {
        let byte = char_to_byte(&self.buffer, self.cursor);
        self.buffer.insert(byte, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let from = char_to_byte(&self.buffer, self.cursor - 1);
        let to = char_to_byte(&self.buffer, self.cursor);
        self.buffer.replace_range(from..to, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        if self.cursor >= self.buffer.chars().count() {
            return;
        }
        let from = char_to_byte(&self.buffer, self.cursor);
        let to = char_to_byte(&self.buffer, self.cursor + 1);
        self.buffer.replace_range(from..to, "");
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.buffer.chars().count();
        self.cursor = (self.cursor as isize + delta).clamp(0, len as isize) as usize;
    }

    // ── 校验 ──────────────────────────────────────────────────

    /// 提交前的本地校验：`Ok` = 写进文档的 JSON 值，`Err` = 留在编辑器里的原因。
    pub(crate) fn validate(&self) -> Result<Value, String> {
        validate_buffer(self.kind, &self.buffer, self.required, &self.constraints)
    }
}

/// 本地校验的实体（`EditState::open` 的初始值判定与 `EditState::validate` 共用同一份）。
fn validate_buffer(
    kind: ScalarKind,
    buffer: &str,
    required: bool,
    constraints: &Constraints,
) -> Result<Value, String> {
    match kind {
        ScalarKind::Secret => Ok(Value::String(buffer.to_string())),
        ScalarKind::Json => {
            let parsed: Value =
                serde_json::from_str(buffer.trim()).map_err(|e| format!("JSON 解析失败：{e}"))?;
            if parsed.is_object() {
                Ok(parsed)
            } else {
                Err("需要一个 JSON 对象".into())
            }
        }
        ScalarKind::Int => {
            let number: i64 = buffer
                .trim()
                .parse()
                .map_err(|_| "需要一个整数".to_string())?;
            check_range(number as f64, constraints)?;
            Ok(Value::from(number))
        }
        ScalarKind::Float => {
            let number: f64 = buffer
                .trim()
                .parse()
                .map_err(|_| "需要一个数字".to_string())?;
            if !number.is_finite() {
                return Err("需要一个数字".into());
            }
            check_range(number, constraints)?;
            Ok(Value::from(number))
        }
        ScalarKind::Str => {
            let trimmed = buffer.trim();
            if constraints.nullable && matches!(trimmed, "" | "null" | "~") {
                return Ok(Value::Null);
            }
            if required && trimmed.is_empty() {
                return Err("必填".into());
            }
            if let Some(min_length) = constraints.min_length {
                let len = buffer.chars().count() as i64;
                if len < min_length {
                    return Err(format!("至少 {min_length} 个字符"));
                }
            }
            // 不支持的构造返回 None：跳过本地校验（后端是权威）。
            if let Some(pattern) = &constraints.pattern
                && pattern_matches(pattern, buffer) == Some(false)
            {
                return Err(format!("格式应为 {pattern}"));
            }
            Ok(Value::String(buffer.to_string()))
        }
    }
}

fn check_range(value: f64, constraints: &Constraints) -> Result<(), String> {
    let out_of_range = constraints.min.is_some_and(|min| {
        if constraints.exclusive_min {
            value <= min
        } else {
            value < min
        }
    }) || constraints.max.is_some_and(|max| {
        if constraints.exclusive_max {
            value >= max
        } else {
            value > max
        }
    });
    if out_of_range {
        Err(format!("取值范围 {}", range_label(constraints)))
    } else {
        Ok(())
    }
}

/// 开闭区间按 exclusive 标志渲染，缺界用 `∞`：`(0, ∞)`（无界的一侧恒为开）。
fn range_label(constraints: &Constraints) -> String {
    let low = constraints
        .min
        .map(fmt_bound)
        .unwrap_or_else(|| "-∞".into());
    let high = constraints.max.map(fmt_bound).unwrap_or_else(|| "∞".into());
    let left = if constraints.min.is_none() || constraints.exclusive_min {
        '('
    } else {
        '['
    };
    let right = if constraints.max.is_none() || constraints.exclusive_max {
        ')'
    } else {
        ']'
    };
    format!("{left}{low}, {high}{right}")
}

/// 标量 kind 的编辑器判据（bool / enum / object / list 没有单行编辑器）。
pub(crate) fn editor_kind(node: &SettingNode) -> Option<ScalarKind> {
    match node.kind {
        SettingKind::Str => Some(ScalarKind::Str),
        SettingKind::Int => Some(ScalarKind::Int),
        SettingKind::Float => Some(ScalarKind::Float),
        SettingKind::Secret => Some(ScalarKind::Secret),
        SettingKind::Map => Some(ScalarKind::Json),
        _ => None,
    }
}

/// 范围标签里的数字：整值不补小数（`取值范围 (0, ∞)`）。
fn fmt_bound(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// 浮点展示：整数值补一位小数（`300.0`），其余用 `{}` 的紧凑形式。
pub(crate) fn fmt_f64(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() && value.abs() < 1e15 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// 第 `char_idx` 个字符的字节偏移（越界钳到字符串末尾）。
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

// ── 迷你 pattern 匹配（正则子集） ────────────────────────────
//
// 支持：`^` / `$` 锚点、字面量、`.`、`[...]`（含 `^` 取反、`a-z` 范围、`\]` / `\\` 转义）、
// `\d \D \w \W \s \S`、`\x` 任意字面量转义、`*` `+` `?` `{n}` `{n,}` `{n,m}` 量词（回溯匹配）。
// 出现 `(` `)` `|` 或其它锚点写法 → `None`（调用方跳过本地校验）。

/// 模式匹配；`None` = 含不支持的构造，无法本地判定。
pub(crate) fn pattern_matches(pattern: &str, value: &str) -> Option<bool> {
    let (anchored_start, atoms, anchored_end) = parse_pattern(pattern)?;
    let chars: Vec<char> = value.chars().collect();
    if anchored_start {
        return Some(match_from(&atoms, &chars, 0, anchored_end));
    }
    Some((0..=chars.len()).any(|start| match_from(&atoms, &chars, start, anchored_end)))
}

#[derive(Debug, Clone)]
struct Atom {
    kind: AtomKind,
    min: usize,
    max: usize,
}

#[derive(Debug, Clone)]
enum AtomKind {
    Lit(char),
    Dot,
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

#[derive(Debug, Clone)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

impl Atom {
    fn matches(&self, c: char) -> bool {
        match &self.kind {
            AtomKind::Lit(expected) => *expected == c,
            AtomKind::Dot => true,
            AtomKind::Class { negated, items } => {
                let hit = items.iter().any(|item| match item {
                    ClassItem::Char(expected) => *expected == c,
                    ClassItem::Range(low, high) => (*low..=*high).contains(&c),
                    ClassItem::Digit(positive) => c.is_ascii_digit() == *positive,
                    ClassItem::Word(positive) => (c.is_alphanumeric() || c == '_') == *positive,
                    ClassItem::Space(positive) => c.is_whitespace() == *positive,
                });
                hit != *negated
            }
        }
    }
}

/// 从 `from` 起用 `atoms` 匹配；`require_end` 时要求恰好 consume 到结尾。
fn match_from(atoms: &[Atom], chars: &[char], from: usize, require_end: bool) -> bool {
    let Some((atom, rest)) = atoms.split_first() else {
        return !require_end || from == chars.len();
    };
    let mut ends = vec![from];
    let mut pos = from;
    while ends.len() - 1 < atom.max && pos < chars.len() && atom.matches(chars[pos]) {
        pos += 1;
        ends.push(pos);
    }
    if ends.len() - 1 < atom.min {
        return false;
    }
    // 贪心优先（量词默认贪心），失败再回溯。
    ends.iter()
        .rev()
        .any(|&end| match_from(rest, chars, end, require_end))
}

/// 解析：`(锚定起点, 原子序列, 锚定终点)`；不支持的构造 → `None`。
fn parse_pattern(pattern: &str) -> Option<(bool, Vec<Atom>, bool)> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let anchored_start = chars.first() == Some(&'^');
    if anchored_start {
        i += 1;
    }
    let mut anchored_end = false;
    let mut atoms = Vec::new();
    while i < chars.len() {
        let kind = match chars[i] {
            '$' if i + 1 == chars.len() => {
                anchored_end = true;
                i += 1;
                continue;
            }
            // 中缀 `$` / 中缀 `^` / 分组与选择：不支持。
            '$' | '^' | '(' | ')' | '|' => return None,
            // 量词出现在原子位置 = 语法错。
            '*' | '+' | '?' | '{' | '}' => return None,
            '[' => {
                let (kind, next) = parse_class(&chars, i + 1)?;
                i = next;
                kind
            }
            '\\' => {
                let (item, next) = parse_escape(&chars, i + 1)?;
                i = next;
                item.into_atom()
            }
            '.' => {
                i += 1;
                AtomKind::Dot
            }
            other => {
                i += 1;
                AtomKind::Lit(other)
            }
        };
        let (min, max) = match chars.get(i) {
            Some('*') => {
                i += 1;
                (0, usize::MAX)
            }
            Some('+') => {
                i += 1;
                (1, usize::MAX)
            }
            Some('?') => {
                i += 1;
                (0, 1)
            }
            Some('{') => {
                let (min, max, next) = parse_brace(&chars, i + 1)?;
                i = next;
                (min, max)
            }
            _ => (1, 1),
        };
        atoms.push(Atom { kind, min, max });
    }
    Some((anchored_start, atoms, anchored_end))
}

/// `{n}` / `{n,}` / `{n,m}`（`i` 指向 `{` 之后）。
fn parse_brace(chars: &[char], mut i: usize) -> Option<(usize, usize, usize)> {
    let mut min = String::new();
    while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
        min.push(chars[i]);
        i += 1;
    }
    if min.is_empty() {
        return None;
    }
    let min: usize = min.parse().ok()?;
    let max = match chars.get(i) {
        Some('}') => min,
        Some(',') => {
            i += 1;
            let mut max = String::new();
            while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                max.push(chars[i]);
                i += 1;
            }
            if max.is_empty() {
                usize::MAX
            } else {
                max.parse().ok()?
            }
        }
        _ => return None,
    };
    if chars.get(i) != Some(&'}') {
        return None;
    }
    if max < min {
        return None;
    }
    Some((min, max, i + 1))
}

/// `[...]` 的内容（`i` 指向 `[` 之后）。
fn parse_class(chars: &[char], mut i: usize) -> Option<(AtomKind, usize)> {
    let negated = chars.get(i) == Some(&'^');
    if negated {
        i += 1;
    }
    let mut items: Vec<ClassItem> = Vec::new();
    loop {
        let c = *chars.get(i)?;
        if c == ']' && !items.is_empty() {
            i += 1;
            break;
        }
        let item = if c == '\\' {
            let (escaped, next) = parse_escape(chars, i + 1)?;
            i = next;
            match escaped {
                Escape::Literal(c) => ClassItem::Char(c),
                Escape::Digit(positive) => ClassItem::Digit(positive),
                Escape::Word(positive) => ClassItem::Word(positive),
                Escape::Space(positive) => ClassItem::Space(positive),
            }
        } else {
            i += 1;
            ClassItem::Char(c)
        };
        // `a-z` 范围只对两个普通字符成立；`\d-x` 这种把 `-` 当字面量。
        let item = match (item, chars.get(i), chars.get(i + 1)) {
            (ClassItem::Char(low), Some('-'), Some(high)) if *high != ']' => {
                i += 2;
                if *high == '\\' {
                    return None; // 右端是转义的范围：不解析
                }
                ClassItem::Range(low, *high)
            }
            (item, _, _) => item,
        };
        items.push(item);
    }
    if items.is_empty() {
        return None;
    }
    Some((AtomKind::Class { negated, items }, i))
}

#[derive(Debug, Clone, Copy)]
enum Escape {
    Literal(char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

impl Escape {
    fn into_atom(self) -> AtomKind {
        match self {
            Escape::Literal(c) => AtomKind::Lit(c),
            Escape::Digit(positive) => AtomKind::Class {
                negated: false,
                items: vec![ClassItem::Digit(positive)],
            },
            Escape::Word(positive) => AtomKind::Class {
                negated: false,
                items: vec![ClassItem::Word(positive)],
            },
            Escape::Space(positive) => AtomKind::Class {
                negated: false,
                items: vec![ClassItem::Space(positive)],
            },
        }
    }
}

fn parse_escape(chars: &[char], i: usize) -> Option<(Escape, usize)> {
    let c = *chars.get(i)?;
    let escaped = match c {
        'd' => Escape::Digit(true),
        'D' => Escape::Digit(false),
        'w' => Escape::Word(true),
        'W' => Escape::Word(false),
        's' => Escape::Space(true),
        'S' => Escape::Space(false),
        other if other.is_alphanumeric() => return None, // `\b` `\A` … 不支持
        other => Escape::Literal(other),
    };
    Some((escaped, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::SettingChoice;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn constraints() -> Constraints {
        Constraints {
            min: None,
            max: None,
            exclusive_min: false,
            exclusive_max: false,
            min_length: None,
            pattern: None,
            nullable: false,
        }
    }

    fn editor(kind: ScalarKind, initial: &str) -> EditState {
        EditState::open(
            Root::Gateway,
            "gateway.port".into(),
            kind,
            false,
            constraints(),
            initial.into(),
        )
    }

    fn node(kind: SettingKind) -> SettingNode {
        SettingNode {
            key: "f".into(),
            path: "f".into(),
            title: "f".into(),
            doc: String::new(),
            notes: vec![],
            example: None,
            order: 0,
            kind,
            required: false,
            nullable: false,
            default: None,
            has_default: false,
            min: None,
            max: None,
            exclusive_min: false,
            exclusive_max: false,
            min_length: None,
            pattern: None,
            choices: vec![SettingChoice {
                value: "a".into(),
                doc: None,
            }],
            min_items: None,
            max_items: None,
            secret: false,
            apply: Default::default(),
            editable: true,
            deprecated: None,
            section: None,
            section_doc: None,
            children: vec![],
            element: None,
            variants: None,
            summary_fields: vec![],
            value_hint: None,
        }
    }

    // ── 缓冲 / 光标 ──────────────────────────────────────────

    #[test]
    fn typing_and_editing_operate_on_char_indices() {
        let mut e = editor(ScalarKind::Str, "");
        for c in ['a', 'b', 'c'] {
            e.handle_key(ch(c));
        }
        assert_eq!(e.buffer, "abc");
        e.handle_key(key(KeyCode::Left));
        e.handle_key(ch('X'));
        assert_eq!(e.buffer, "abXc");
        e.handle_key(key(KeyCode::Home));
        e.handle_key(key(KeyCode::Delete));
        assert_eq!(e.buffer, "bXc");
        e.handle_key(key(KeyCode::End));
        e.handle_key(key(KeyCode::Backspace));
        assert_eq!(e.buffer, "bX");
        assert_eq!(e.cursor, 2);
    }

    #[test]
    fn typing_clears_a_previous_error() {
        let mut e = editor(ScalarKind::Int, "12");
        e.set_error("x".into());
        e.handle_key(ch('3'));
        assert_eq!(e.error(), None);
        assert_eq!(e.buffer, "123");
    }

    #[test]
    fn secret_buffer_is_masked_and_never_rendered_plain() {
        let mut e = editor(ScalarKind::Secret, "should-be-ignored");
        assert_eq!(e.buffer_len(), 0, "密文初值恒为空");
        assert_eq!(e.visible_buffer(), "");
        for c in ['s', 'k', '1'] {
            e.handle_key(ch(c));
        }
        assert_eq!(e.visible_buffer(), "•••");
        assert_eq!(e.buffer_len(), 3);
        assert!(e.is_secret());
        // 原始缓冲只在模块内可见（类型系统保证没有公开出口）。
        assert_eq!(e.buffer, "sk1");
    }

    #[test]
    fn up_down_and_tab_are_ignored_while_editing() {
        let mut e = editor(ScalarKind::Str, "ab");
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Tab] {
            assert_eq!(e.handle_key(key(code)), EditEvent::None);
        }
        assert_eq!(e.buffer, "ab");
        assert_eq!(e.cursor, 2, "光标不动");
    }

    #[test]
    fn esc_cancels_and_enter_asks_for_submit() {
        let mut e = editor(ScalarKind::Str, "ab");
        assert_eq!(e.handle_key(key(KeyCode::Esc)), EditEvent::Cancel);
        assert_eq!(e.handle_key(key(KeyCode::Enter)), EditEvent::Submit);
    }

    #[test]
    fn ctrl_u_clears_the_buffer() {
        let mut e = editor(ScalarKind::Str, "hello");
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        e.handle_key(key);
        assert_eq!(e.buffer_len(), 0);
        assert_eq!(e.cursor(), 0);
    }

    // ── int / float 校验 ─────────────────────────────────────

    #[test]
    fn int_parses_and_rejects_garbage() {
        let e = editor(ScalarKind::Int, "42");
        assert_eq!(e.validate().unwrap(), Value::from(42));
        let e = editor(ScalarKind::Int, "  7 ");
        assert_eq!(e.validate().unwrap(), Value::from(7));
        let e = editor(ScalarKind::Int, "abc");
        assert_eq!(e.validate().unwrap_err(), "需要一个整数");
        let e = editor(ScalarKind::Int, "1.5");
        assert_eq!(e.validate().unwrap_err(), "需要一个整数");
    }

    #[test]
    fn float_accepts_decimals_and_rejects_nan() {
        let e = editor(ScalarKind::Float, "1.5");
        assert_eq!(e.validate().unwrap(), Value::from(1.5));
        let e = editor(ScalarKind::Float, "300");
        assert_eq!(e.validate().unwrap(), Value::from(300.0));
        for bad in ["nan", "inf", "-inf", "x"] {
            let e = editor(ScalarKind::Float, bad);
            assert_eq!(e.validate().unwrap_err(), "需要一个数字", "{bad}");
        }
    }

    #[test]
    fn exclusive_and_inclusive_bounds_are_enforced_with_bracket_labels() {
        let mut cs = constraints();
        cs.min = Some(0.0);
        cs.exclusive_min = true;
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Int,
            false,
            cs,
            "0".into(),
        );
        assert_eq!(e.validate().unwrap_err(), "取值范围 (0, ∞)");
        let mut cs2 = constraints();
        cs2.min = Some(1.0);
        cs2.max = Some(10.0);
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Int,
            false,
            cs2.clone(),
            "11".into(),
        );
        assert_eq!(e.validate().unwrap_err(), "取值范围 [1, 10]");
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Int,
            false,
            cs2,
            "10".into(),
        );
        assert_eq!(e.validate().unwrap(), Value::from(10), "闭区间上界可取");
        let mut cs3 = constraints();
        cs3.max = Some(10.0);
        cs3.exclusive_max = true;
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Int,
            false,
            cs3,
            "10".into(),
        );
        assert_eq!(e.validate().unwrap_err(), "取值范围 (-∞, 10)");
    }

    // ── str 校验 ─────────────────────────────────────────────

    #[test]
    fn required_string_rejects_blank_and_min_length_counts_chars() {
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Str,
            true,
            constraints(),
            "   ".into(),
        );
        assert_eq!(e.validate().unwrap_err(), "必填");
        let mut cs = constraints();
        cs.min_length = Some(3);
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Str,
            false,
            cs,
            "中文".into(),
        );
        assert_eq!(
            e.validate().unwrap_err(),
            "至少 3 个字符",
            "按字符数不是字节数"
        );
        let mut cs = constraints();
        cs.min_length = Some(2);
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Str,
            false,
            cs,
            "中文".into(),
        );
        assert_eq!(e.validate().unwrap(), Value::from("中文"));
    }

    #[test]
    fn nullable_accepts_null_tilde_and_empty_as_json_null() {
        let mut cs = constraints();
        cs.nullable = true;
        for input in ["null", "~", ""] {
            let e = EditState::open(
                Root::Gateway,
                "p".into(),
                ScalarKind::Str,
                false,
                cs.clone(),
                input.into(),
            );
            assert_eq!(e.validate().unwrap(), Value::Null, "{input:?}");
        }
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Str,
            false,
            cs,
            "cyan".into(),
        );
        assert_eq!(e.validate().unwrap(), Value::from("cyan"));
    }

    #[test]
    fn pattern_is_enforced_when_supported_and_skipped_otherwise() {
        let mut cs = constraints();
        cs.pattern = Some("^[a-zA-Z0-9_-]+$".into());
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Str,
            false,
            cs.clone(),
            "a-b_1".into(),
        );
        assert_eq!(e.validate().unwrap(), Value::from("a-b_1"));
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Str,
            false,
            cs,
            "a b".into(),
        );
        assert_eq!(e.validate().unwrap_err(), "格式应为 ^[a-zA-Z0-9_-]+$");
        let mut cs = constraints();
        cs.pattern = Some("^(cyan|magenta)$".into());
        let e = EditState::open(
            Root::Gateway,
            "p".into(),
            ScalarKind::Str,
            false,
            cs,
            "nope".into(),
        );
        assert_eq!(
            e.validate().unwrap(),
            Value::from("nope"),
            "不支持的构造跳过本地校验"
        );
    }

    // ── json 编辑器 ──────────────────────────────────────────

    #[test]
    fn json_editor_requires_a_parseable_object() {
        let e = editor(ScalarKind::Json, "{\"thinking\":{\"type\":\"enabled\"}}");
        assert!(e.validate().unwrap().is_object());
        let e = editor(ScalarKind::Json, "{oops}");
        assert!(e.validate().unwrap_err().starts_with("JSON 解析失败"));
        let e = editor(ScalarKind::Json, "[1,2]");
        assert_eq!(e.validate().unwrap_err(), "需要一个 JSON 对象");
        let e = editor(ScalarKind::Json, "\"str\"");
        assert_eq!(e.validate().unwrap_err(), "需要一个 JSON 对象");
    }

    // ── kind 映射 ────────────────────────────────────────────

    #[test]
    fn editor_kind_covers_scalars_and_leaves_others_to_the_tree() {
        assert_eq!(editor_kind(&node(SettingKind::Str)), Some(ScalarKind::Str));
        assert_eq!(editor_kind(&node(SettingKind::Int)), Some(ScalarKind::Int));
        assert_eq!(
            editor_kind(&node(SettingKind::Float)),
            Some(ScalarKind::Float)
        );
        assert_eq!(editor_kind(&node(SettingKind::Map)), Some(ScalarKind::Json));
        assert_eq!(editor_kind(&node(SettingKind::Bool)), None);
        assert_eq!(editor_kind(&node(SettingKind::Enum)), None);
        assert_eq!(editor_kind(&node(SettingKind::Object)), None);
        assert_eq!(editor_kind(&node(SettingKind::List)), None);
        assert_eq!(editor_kind(&node(SettingKind::Unknown("x".into()))), None);
    }

    #[test]
    fn secret_editor_kind_comes_from_the_node() {
        let mut n = node(SettingKind::Secret);
        n.secret = true;
        assert_eq!(editor_kind(&n), Some(ScalarKind::Secret));
    }

    // ── pattern 子集匹配器 ───────────────────────────────────

    #[test]
    fn pattern_matcher_handles_the_supported_subset() {
        assert_eq!(pattern_matches("^[a-z]+$", "abc"), Some(true));
        assert_eq!(pattern_matches("^[a-z]+$", "abc1"), Some(false));
        assert_eq!(pattern_matches("^[^0-9]+$", "abc"), Some(true));
        assert_eq!(pattern_matches("^[^0-9]+$", "a1"), Some(false));
        assert_eq!(pattern_matches(r"^\d{3}-\d{4}$", "123-4567"), Some(true));
        assert_eq!(pattern_matches(r"^\d{3}-\d{4}$", "12-4567"), Some(false));
        assert_eq!(pattern_matches(r"^\w+$", "a_1"), Some(true));
        assert_eq!(pattern_matches(r"^a\.b$", "a.b"), Some(true));
        assert_eq!(pattern_matches(r"^a\.b$", "axb"), Some(false));
        assert_eq!(
            pattern_matches("ab", "xxabyy"),
            Some(true),
            "无锚点 = 搜索语义"
        );
        assert_eq!(pattern_matches("ab$", "xxab"), Some(true));
        assert_eq!(pattern_matches("ab$", "abx"), Some(false));
        assert_eq!(pattern_matches("^a*bc$", "bc"), Some(true), "* 零次");
        assert_eq!(pattern_matches("^a*bc$", "aaabc"), Some(true));
        assert_eq!(pattern_matches("^(a|b)$", "a"), None, "选择不支持 → 跳过");
        assert_eq!(
            pattern_matches("^[a-z](x)$", "ax"),
            None,
            "分组不支持 → 跳过"
        );
        assert_eq!(pattern_matches(r"^\d{2,3}$", "12"), Some(true));
        assert_eq!(pattern_matches(r"^\d{2,3}$", "1"), Some(false));
        assert_eq!(pattern_matches(r"^\d{2,}$", "12345"), Some(true));
        assert_eq!(pattern_matches("", "anything"), Some(true));
        assert_eq!(pattern_matches("^$", ""), Some(true));
        assert_eq!(pattern_matches("^$", "x"), Some(false));
    }

    #[test]
    fn pattern_matcher_backtracks_across_quantifiers() {
        // 贪心失败后必须回溯：`a+` 先吃掉全部 a，`b` 才可能匹配。
        assert_eq!(pattern_matches("^a+b$", "aaab"), Some(true));
        assert_eq!(pattern_matches("^.*b$", "aab"), Some(true));
        assert_eq!(pattern_matches("^a?ab$", "ab"), Some(true));
        assert_eq!(pattern_matches(r"^\s*x\s*$", "  x "), Some(true));
    }

    #[test]
    fn fmt_f64_keeps_a_decimal_for_whole_numbers() {
        assert_eq!(fmt_f64(300.0), "300.0");
        assert_eq!(fmt_f64(0.5), "0.5");
        assert_eq!(fmt_f64(-2.25), "-2.25");
    }
}
