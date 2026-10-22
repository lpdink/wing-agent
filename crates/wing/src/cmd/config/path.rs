//! 路径代数：文法 → 规范化 → 目录寻址 → 按 kind 强转。
//!
//! 读路径（`list` / `get`）与写路径（`set` / `add` / …）共用同一套判据——各判一半正是
//! "目录寻址认这个前缀、文档操作不认"的来源。根前缀的唯一实现是
//! [`wing_api_client::models::SettingNode::strip_root_prefix`]。

use serde_json::{Value, json};
use wing_api_client::models::{PathStep, SettingKind, SettingNode, parse_path};

use crate::shared::doc_edit;

/// 写命令的路径：必须非空、文法合法、且不含元素模板 `[]`。
pub(super) fn write_steps(path: &str) -> Result<Vec<PathStep>, String> {
    let steps = parse_path(path).ok_or_else(|| {
        format!("路径文法非法：`{path}`（示例：gateway.port / providers[0].api_key）")
    })?;
    if steps.is_empty() {
        return Err("路径不能为空（写操作必须指向具体字段）".to_string());
    }
    if steps.contains(&PathStep::Element) {
        return Err(format!(
            "模板路径（[]）不能用于写操作：`{path}`（请用具体下标，如 providers[0]）"
        ));
    }
    Ok(steps)
}

/// 位置参数与 `--json-value` 互斥（两处共用：预检 + `incoming_value`）。
pub(super) fn ensure_exclusive(
    value: Option<&str>,
    json_value: Option<&str>,
) -> Result<(), String> {
    if value.is_some() && json_value.is_some() {
        return Err("位置参数 <value> 与 --json-value 不能同时给出".to_string());
    }
    Ok(())
}

/// 写命令的路径：先做文法校验（离线可判），再规范化。
pub(super) fn plan_steps(root: &SettingNode, path: &str) -> Result<Vec<PathStep>, String> {
    normalize_steps(root, write_steps(path)?)
}

/// 路径规范化：剥掉可选的 `config.` 根前缀。
///
/// 判据只有一份（[`SettingNode::strip_root_prefix`]，与 `node_at` 共用），两种拼写都要能寻址。
/// **只做这一件事**：不折叠大小写、不做别名。
/// 规范化后的路径是唯一用于文档操作、目录寻址、回显与 `--json` 的形态——否则写命令会在
/// 文档根下造出名为 `config` 的伪键并静默成功。
///
/// 剥完为空（用户只敲了根名）→ 用法错误，并列出可用顶层节名。
pub(super) fn normalize_steps(
    root: &SettingNode,
    steps: Vec<PathStep>,
) -> Result<Vec<PathStep>, String> {
    let stripped: Vec<PathStep> = root.strip_root_prefix(&steps).to_vec();
    if stripped.is_empty() {
        let sections: Vec<&str> = root
            .children
            .iter()
            .map(|child| child.display_label())
            .collect();
        return Err(format!(
            "路径为空：`{}` 是根名，请指向一个具体设置（可用顶层节：{}）",
            root.key,
            if sections.is_empty() {
                "(无)".to_string()
            } else {
                sections.join(" / ")
            }
        ));
    }
    Ok(stripped)
}

/// 按路径在目录里寻址；找不到就是用法错误（带统一指引）。路径必须是**已规范化**的。
pub(super) fn require_node<'a>(
    root: &'a SettingNode,
    document: &Value,
    steps: &[PathStep],
) -> Result<&'a SettingNode, String> {
    resolve_node(root, document, steps).ok_or_else(|| {
        format!(
            "路径不在设置目录中：`{}`（用 `wing config list` 查看全部路径）",
            doc_edit::format_path(steps)
        )
    })
}

/// `set` 到 union 列表元素（`providers[0].models[0]` 这种整项替换）时，元素形态按**新值**判定：
/// 新值看起来像 JSON 对象（`{` 开头 / `--json-value` 解析出对象）= 完整形态，其余 = 简单形态。
///
/// 理由：整项替换的"目标形态"只能由新值表达——当前文档里的旧值是什么形态，与新值无关。
/// `models: ["ds-flash"] → set providers[0].models[0] '{"id":"x"}'` 因此能就地变成对象形态。
/// 非 union 路径返回 `None`（调用方走常规寻址）。
pub(super) fn union_replacement_target<'a>(
    root: &'a SettingNode,
    document: &Value,
    steps: &[PathStep],
    incoming: Incoming<'_>,
) -> Option<&'a SettingNode> {
    let (PathStep::Index(_), parents) = steps.split_last()? else {
        return None;
    };
    let wants_object = match incoming {
        Incoming::Positional(raw) => raw.trim_start().starts_with('{'),
        Incoming::RawJson(raw) => serde_json::from_str::<Value>(raw)
            .map(|value| value.is_object())
            .unwrap_or(false),
    };
    let list = resolve_node(root, document, parents)?;
    list.variants
        .as_deref()?
        .iter()
        .find(|variant| matches!(variant.kind, SettingKind::Object) == wants_object)
}

/// `remove` 的目标元素节点（打印摘要前按目录脱敏）。路径形态不对就不脱敏：
/// 值本来就来自后端已掩码的文档，这里只是纵深防御。
pub(super) fn removal_element<'a>(
    root: &'a SettingNode,
    document: &Value,
    steps: &[PathStep],
) -> Option<&'a SettingNode> {
    let (PathStep::Index(_), parents) = steps.split_last()? else {
        return None;
    };
    let list = resolve_node(root, document, parents)?;
    element_target(list, doc_edit::get_path(document, steps))
}

/// 位置参数与 `--json-value` 二选一的结论。
#[derive(Debug, Clone, Copy)]
pub(super) enum Incoming<'a> {
    /// 位置参数：按 catalog kind 强制转换。
    Positional(&'a str),
    /// `--json-value`：原始 JSON，不做任何 kind 检查（逃生舱）。
    RawJson(&'a str),
}

/// 位置参数与 `--json-value` 二选一。
pub(super) fn incoming_value<'a>(
    value: Option<&'a str>,
    json_value: Option<&'a str>,
) -> Result<Incoming<'a>, String> {
    ensure_exclusive(value, json_value)?;
    match (value, json_value) {
        (Some(value), None) => Ok(Incoming::Positional(value)),
        (None, Some(raw)) => Ok(Incoming::RawJson(raw)),
        (None, None) => Err("缺少 <value>（或 --json-value <raw>）".to_string()),
        (Some(_), Some(_)) => unreachable!("exclusivity checked above"),
    }
}

/// 输入 → 待写入的 JSON 值：位置参数按 kind 转换，`--json-value` 原样解析。
pub(super) fn incoming_json(node: &SettingNode, incoming: Incoming<'_>) -> Result<Value, String> {
    match incoming {
        Incoming::Positional(raw) => coerce_value(node, raw),
        Incoming::RawJson(raw) => parse_json(raw),
    }
}

// ============================================================
// 路径与目录
// ============================================================

/// 目录寻址：`Key` 走 `children`；`Index`/`Element` 走 `element`，
/// union 元素（`variants`）用**文档实际值** + [`SettingNode::select_variant`] 选形态。
///
/// 与 [`SettingNode::node_at`] 的分工：`node_at` 在 `variants` 场景按契约返回 `None`（catalog
/// 无法单独判定元素形态），而 CLI 手里有文档，所以这里补上"用值选形态"这一段；
/// 路径解析仍走 `parse_path`，选形态仍走 `select_variant`——没有第二份实现。
pub(super) fn resolve_node<'a>(
    root: &'a SettingNode,
    document: &Value,
    steps: &[PathStep],
) -> Option<&'a SettingNode> {
    // `steps` 必须是**已规范化**的（`normalize_steps`）：根前缀容忍是入口的唯一职责，
    // 这里不再各判一半——否则"目录寻址认这个前缀、文档操作不认"就会重现。
    let mut node = root;
    let mut value: Option<&Value> = Some(document);
    for step in steps {
        match step {
            PathStep::Key(name) => {
                node = node.children.iter().find(|child| &child.key == name)?;
                value = value.and_then(|current| current.get(name));
            }
            PathStep::Index(index) => {
                let item = value.and_then(|current| current.get(*index));
                node = element_target(node, item)?;
                value = item;
            }
            PathStep::Element => {
                node = element_target(node, None)?;
                value = None;
            }
        }
    }
    Some(node)
}

/// 列表节点 → 元素节点：单一元素类型直接给 `element`；union 元素要一个值来选形态。
pub(super) fn element_target<'a>(
    list: &'a SettingNode,
    item: Option<&Value>,
) -> Option<&'a SettingNode> {
    if let Some(element) = list.element.as_deref() {
        return Some(element);
    }
    list.select_variant(item?)
}

/// 值强制转换表：把命令行文本按 catalog `kind` 转成 JSON。
///
/// - `nullable` 先行：`null` / `~` / 空串 → JSON `null`（任何 kind）；
/// - 容器（`map` / `object` / `list`）收 JSON 文本；
/// - 未知 kind 拒绝：让调用方改用 `--json-value`（逃生舱），而不是猜一种转换。
pub(super) fn coerce_value(node: &SettingNode, raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if node.nullable && matches!(raw, "null" | "~" | "") {
        return Ok(Value::Null);
    }
    match &node.kind {
        SettingKind::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(json!(true)),
            "false" | "0" | "no" | "off" => Ok(json!(false)),
            _ => Err("需要一个布尔值（true/false）".to_string()),
        },
        SettingKind::Int => raw
            .parse::<i64>()
            .map(|number| json!(number))
            .map_err(|_| "需要一个整数".to_string()),
        SettingKind::Float => raw
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .map(|number| json!(number))
            .ok_or_else(|| "需要一个数字".to_string()),
        SettingKind::Str | SettingKind::Secret => Ok(json!(raw)),
        SettingKind::Enum => {
            if node.choices.iter().any(|choice| choice.value == raw) {
                Ok(json!(raw))
            } else {
                Err(format!("可选值：{}", render_choices(node)))
            }
        }
        SettingKind::Map | SettingKind::Object => match parse_json(raw)? {
            Value::Object(map) => Ok(Value::Object(map)),
            _ => Err("需要一个 JSON 对象".to_string()),
        },
        SettingKind::List => match parse_json(raw)? {
            Value::Array(array) => Ok(Value::Array(array)),
            _ => Err("需要一个 JSON 数组".to_string()),
        },
        SettingKind::Unknown(_) => Err(
            "无法把字符串转换成该类型（网关比 CLI 新？），请用 --json-value 指定原始 JSON"
                .to_string(),
        ),
    }
}

/// 枚举可选值的展示（`a` / `b`）。
pub(super) fn render_choices(node: &SettingNode) -> String {
    node.choices
        .iter()
        .map(|choice| choice.value.clone())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// `--json-value`：原始 JSON，不做任何 kind 检查（调用方负责形状；不符由后端 problem 报告）。
pub(super) fn parse_json(raw: &str) -> Result<Value, String> {
    serde_json::from_str(raw).map_err(|e| format!("不是合法 JSON：{e}"))
}
