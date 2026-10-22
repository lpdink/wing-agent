//! `wing config` 的人读渲染：列表行 / 值 / 密文 / 约束。
//!
//! 全是纯函数（注入 `ListRow` / `SettingNode` / 值），单测直接断言文本。

use serde::Serialize;
use serde_json::Value;
use wing_api_client::models::{SecretPresence, SecretState, SettingNode};

use super::catalog::ListRow;
use crate::cmd::common;

/// `list` 的人类可读输出：section 分组头 + `├─` / `└─` 树形（纯函数）。
pub(super) fn render_list_text(rows: &[ListRow]) -> String {
    if rows.is_empty() {
        return "（没有匹配的设置项）\n".to_string();
    }
    let mut out = String::new();
    let mut previous_section: Option<&str> = None;
    // 深度优先顺序下的祖先栈：`true` = 该祖先是自己兄弟里的最后一个（画空格而不是竖线）。
    let mut ancestors: Vec<bool> = Vec::new();
    for row in rows {
        let section = row.section.as_deref();
        if section != previous_section {
            if let Some(section) = section {
                out.push_str(&format!("── {section} ──\n"));
            }
            previous_section = section;
        }
        ancestors.truncate(row.depth);
        let indent = tree_indent(&ancestors, row.depth, row.last);
        ancestors.push(row.last);
        let bang = if row.problem.is_some() { "! " } else { "" };
        let mut line = format!("{indent}{bang}{}", row.path);
        if row.structural {
            line.push_str(&format!("  ({})", row.kind));
        } else {
            line.push_str(&format!(" = {}", render_row_value(row, row.overridden)));
        }
        if let Some(default) = row
            .default
            .as_ref()
            .filter(|_| row.has_default && !row.structural)
        {
            line.push_str(&format!("  默认 {}", render_json_value(default)));
        }
        line.push_str(&format!("  [{}]", apply_short(&row.apply)));
        if row.overridden {
            line.push_str("  已覆盖");
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// 行值列：密文掩码 / 文档值 / 回落默认值 / 未设置。
pub(super) fn render_row_value(row: &ListRow, overridden: bool) -> String {
    if let Some(secret) = &row.secret {
        return render_secret(secret);
    }
    if let Some(value) = &row.value {
        return render_json_value(value);
    }
    if let Some(default) = row
        .default
        .as_ref()
        .filter(|_| !overridden && row.has_default)
    {
        return render_json_value(default);
    }
    "(未设置)".to_string()
}

/// 树形前缀：祖先层画 `│  ` / `   `，本层画 `├─ ` / `└─ `（根层无连接符）。
pub(super) fn tree_indent(ancestors: &[bool], depth: usize, last: bool) -> String {
    if depth == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ancestor_last in ancestors {
        out.push_str(if *ancestor_last { "   " } else { "│  " });
    }
    out.push_str(if last { "└─ " } else { "├─ " });
    out
}

/// 生效域短标记（`next_session` → `session`）。
pub(super) fn apply_short(apply: &str) -> &str {
    match apply {
        "next_session" => "session",
        other => other,
    }
}

/// 密文渲染：只给掩码 + 末 4 位 hint（state 未知也保守掩码）。
pub(super) fn render_secret(secret: &SecretState) -> String {
    match &secret.state {
        SecretPresence::Set => match secret.hint.as_deref() {
            Some(hint) if !hint.is_empty() => format!("•••••••• {hint}"),
            _ => "••••••••".to_string(),
        },
        SecretPresence::Empty => "(空)".to_string(),
        SecretPresence::Absent => "(未设置)".to_string(),
        SecretPresence::Unknown(_) => match secret.hint.as_deref() {
            Some(hint) if !hint.is_empty() => format!("•••••••• {hint}"),
            _ => "••••••••".to_string(),
        },
    }
}

/// 值渲染：字符串不带引号，容器序列化成 JSON；一律截断（避免一行吃掉整个终端）。
pub(super) fn render_json_value(value: &Value) -> String {
    match value {
        Value::String(text) => common::truncate_chars(text, VALUE_MAX_CHARS),
        other => common::truncate_chars(&other.to_string(), VALUE_MAX_CHARS),
    }
}

/// 完整渲染（不截断）：只给单项 `get` 判断"是否被截断"用。
pub(super) fn render_json_value_full(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 值列的最大字符数（与 `cmd/query.rs` 的截断风格一致）。
pub(super) const VALUE_MAX_CHARS: usize = 60;

/// 约束列（只列存在的项）。
pub(super) fn render_constraints(node: &SettingNode) -> Vec<String> {
    let mut out = Vec::new();
    match (node.min, node.exclusive_min) {
        (Some(min), exclusive) if exclusive => out.push(format!("> {min}")),
        (Some(min), _) => out.push(format!("≥ {min}")),
        (None, _) => {}
    }
    match (node.max, node.exclusive_max) {
        (Some(max), exclusive) if exclusive => out.push(format!("< {max}")),
        (Some(max), _) => out.push(format!("≤ {max}")),
        (None, _) => {}
    }
    if let Some(min_length) = node.min_length {
        out.push(format!("长度 ≥ {min_length}"));
    }
    if let Some(pattern) = node.pattern.as_deref() {
        out.push(format!("匹配 {pattern}"));
    }
    if let Some(min_items) = node.min_items {
        out.push(format!("至少 {min_items} 项"));
    }
    if let Some(max_items) = node.max_items {
        out.push(format!("至多 {max_items} 项"));
    }
    if node.nullable {
        out.push("可置 null".to_string());
    }
    out
}

/// `--json` 序列化（本模块输出全是自己的 struct，序列化不会失败）。
pub(super) fn json_string<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::{SecretPresence, SecretState};

    #[test]
    fn render_secret_covers_all_three_states() {
        let state = |presence| SecretState {
            state: presence,
            hint: Some("abcd".to_string()),
        };
        assert_eq!(render_secret(&state(SecretPresence::Set)), "•••••••• abcd");
        assert_eq!(render_secret(&state(SecretPresence::Empty)), "(空)");
        assert_eq!(render_secret(&state(SecretPresence::Absent)), "(未设置)");
        // 未知状态（更新的网关）也必须保守掩码。
        let unknown = render_secret(&state(SecretPresence::Unknown("future".to_string())));
        assert!(unknown.starts_with("••••••••"), "{unknown}");
        // 无 hint 的 set 状态。
        let no_hint = render_secret(&SecretState {
            state: SecretPresence::Set,
            hint: None,
        });
        assert_eq!(no_hint, "••••••••");
    }
}
