//! 目录与文档的共享基质：拉一次 schema+get、密文脱敏、问题排序，以及渲染用的行模型。
//!
//! doctor / read / write 三家都依赖这些；它们住在这里，那三个模块之间因此没有边。

use serde::Serialize;
use serde_json::Value;
use wing_api_client::models::{
    SecretState, SettingNode, SettingProblem, SettingsGetResponse, SettingsSchemaResponse,
};

use super::Outcome;
use super::SettingsApi;
use super::api_failure;
use super::path::element_target;

/// 读 schema + get（两条 GET）；失败已映射成（退出码 + stderr 文案）。
pub(super) async fn fetch_catalog<B: SettingsApi>(
    backend: &B,
    http_base: &str,
) -> Result<(SettingsSchemaResponse, SettingsGetResponse), Outcome> {
    let schema = match backend.schema().await {
        Ok(schema) => schema,
        Err(e) => {
            let (code, message) = api_failure(&e, http_base, "读取设置目录失败");
            return Err(Outcome::failure(code, format!("{message}\n")));
        }
    };
    let values = match backend.get().await {
        Ok(values) => values,
        Err(e) => {
            let (code, message) = api_failure(&e, http_base, "读取配置失败");
            return Err(Outcome::failure(code, format!("{message}\n")));
        }
    };
    Ok((schema, values))
}

/// 按目录把密文叶子替换成 `null`（纵深防御：上游若没掩码，值也不会出现在本模块的任何输出里）。
pub(super) fn redact_secrets(node: &SettingNode, value: &Value) -> Value {
    if node.secret {
        return Value::Null;
    }
    if !node.children.is_empty() {
        let Value::Object(map) = value else {
            return value.clone();
        };
        let mut redacted = serde_json::Map::new();
        for (key, item) in map {
            let item = match node.children.iter().find(|child| &child.key == key) {
                Some(child) => redact_secrets(child, item),
                None => item.clone(),
            };
            redacted.insert(key.clone(), item);
        }
        return Value::Object(redacted);
    }
    if let Value::Array(array) = value {
        let mut redacted = Vec::with_capacity(array.len());
        for item in array {
            redacted.push(match element_target(node, Some(item)) {
                Some(element) => redact_secrets(element, item),
                None => item.clone(),
            });
        }
        return Value::Array(redacted);
    }
    value.clone()
}

/// 问题排序（严重度序，未知种类排最后；同序按 path）。纯函数。
pub(super) fn sorted_problems(problems: &[SettingProblem]) -> Vec<&SettingProblem> {
    let mut sorted: Vec<&SettingProblem> = problems.iter().collect();
    sorted.sort_by(|a, b| {
        problem_rank(&a.kind)
            .cmp(&problem_rank(&b.kind))
            .then_with(|| {
                a.path
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.path.as_deref().unwrap_or(""))
            })
    });
    sorted
}

/// 已知种类的严重度序；`conflict` 与未知种类一律排在最后（P6：未知必须容忍）。
pub(super) fn problem_rank(kind: &str) -> usize {
    match kind {
        "missing_required" => 0,
        "unknown_reference" => 1,
        "duplicate" => 2,
        "empty_list" => 3,
        "invalid_value" => 4,
        "unknown_key" => 5,
        _ => 6,
    }
}

/// 一行（树被拍平；文本与 JSON 共用同一份构建结果）。
#[derive(Debug, Clone, Serialize)]
pub(super) struct ListRow {
    /// 规范路径（具体下标；列表元素逐个展开）。
    pub(super) path: String,
    /// catalog kind（`object` / `list` / `secret` / …）。
    pub(super) kind: String,
    /// 生效域（`hot` / `next_session` / `restart` / `readonly`）。
    pub(super) apply: String,
    /// 该路径是否显式写在稀疏文档里。
    pub(super) overridden: bool,
    /// 标量值（结构行为 `null`；密文恒为 `null`——真实值不出网关）。
    pub(super) value: Option<Value>,
    pub(super) has_default: bool,
    pub(super) default: Option<Value>,
    /// 密文状态（state + 末 4 位 hint）；非密文为 `null`。
    pub(super) secret: Option<SecretState>,
    /// 该路径上的第一条问题（按严重度排序后取首条）。
    pub(super) problem: Option<SettingProblem>,
    /// 结构节点（object / list）。
    pub(super) structural: bool,
    /// 顶层分组（声明序；未声明分组时为 `null`）。
    pub(super) section: Option<String>,
    /// 树深度（渲染缩进用；`rows` 顺序已是深度优先）。
    pub(super) depth: usize,
    /// 是否是其兄弟里的最后一个（渲染 `└─` / `├─` 连接符）。
    pub(super) last: bool,
}
