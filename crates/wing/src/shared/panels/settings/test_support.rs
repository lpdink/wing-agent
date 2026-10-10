//! 测试夹具：手工构造的 catalog（06 的 `SettingNode`）与稀疏文档（`serde_json::Value`）。
//!
//! 只在 `cfg(test)` 下编译。构造器只填测试真正读的字段，其余走 [`node`] 的合理缺省，
//! 让每条用例都能一眼看清「这个目录长什么样」。

use std::collections::HashMap;

use serde_json::Value;
use serde_json::json;
use wing_api_client::models::ApplyScope;
use wing_api_client::models::SecretPresence;
use wing_api_client::models::SecretState;
use wing_api_client::models::SettingChoice;
use wing_api_client::models::SettingGroup;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;

use super::Problem;
use super::doc::Root;
use super::doc::SettingsDoc;
use crate::shared::doc_edit::join_path;

/// 目录节点的合理缺省：editable、hot、无约束。
pub(crate) fn node(key: &str, kind: SettingKind) -> SettingNode {
    SettingNode {
        key: key.into(),
        path: key.into(),
        title: key.into(),
        doc: format!("{key} 的说明"),
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
        choices: vec![],
        min_items: None,
        max_items: None,
        secret: false,
        apply: ApplyScope::Hot,
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

pub(crate) fn object(key: &str, children: Vec<SettingNode>) -> SettingNode {
    let mut n = node(key, SettingKind::Object);
    n.children = children;
    n
}

pub(crate) fn list(key: &str, element: SettingNode) -> SettingNode {
    let mut n = node(key, SettingKind::List);
    n.element = Some(Box::new(element));
    n
}

/// union 元素列表（`models: list[str | ModelSpec]`）。
pub(crate) fn list_of(key: &str, variants: Vec<SettingNode>) -> SettingNode {
    let mut n = node(key, SettingKind::List);
    n.variants = Some(variants);
    n
}

pub(crate) fn str_field(key: &str) -> SettingNode {
    node(key, SettingKind::Str)
}

pub(crate) fn required_str(key: &str) -> SettingNode {
    let mut n = node(key, SettingKind::Str);
    n.required = true;
    n
}

pub(crate) fn int_field(key: &str, min: Option<f64>, max: Option<f64>) -> SettingNode {
    let mut n = node(key, SettingKind::Int);
    n.min = min;
    n.max = max;
    n.has_default = true;
    n.default = Some(json!(0));
    n
}

pub(crate) fn float_field(key: &str, default: f64) -> SettingNode {
    let mut n = node(key, SettingKind::Float);
    n.has_default = true;
    n.default = Some(json!(default));
    n
}

pub(crate) fn bool_field(key: &str, default: bool) -> SettingNode {
    let mut n = node(key, SettingKind::Bool);
    n.has_default = true;
    n.default = Some(json!(default));
    n
}

pub(crate) fn secret_field(key: &str) -> SettingNode {
    let mut n = node(key, SettingKind::Secret);
    n.secret = true;
    n
}

pub(crate) fn enum_field(key: &str, values: &[(&str, &str)]) -> SettingNode {
    let mut n = node(key, SettingKind::Enum);
    n.choices = values
        .iter()
        .map(|(value, doc)| SettingChoice {
            value: (*value).into(),
            doc: Some((*doc).into()),
        })
        .collect();
    n
}

pub(crate) fn map_field(key: &str) -> SettingNode {
    let mut n = node(key, SettingKind::Map);
    n.has_default = true;
    n.default = Some(json!({}));
    n
}

/// 元素模板的空壳（key 恒为 `[]`，路径由 [`with_paths`] 算）。
pub(crate) fn element(inner: SettingNode) -> SettingNode {
    SettingNode {
        key: "[]".into(),
        ..inner
    }
}

/// 递归重算全部 path（`[]` = 元素模板）。
pub(crate) fn with_paths(mut n: SettingNode, prefix: &str) -> SettingNode {
    n.path = if n.key == "[]" {
        format!("{prefix}[]")
    } else {
        join_path(prefix, &n.key)
    };
    let path = n.path.clone();
    if let Some(element) = n.element.take() {
        n.element = Some(Box::new(with_paths(*element, &path)));
    }
    if let Some(variants) = n.variants.take() {
        n.variants = Some(variants.into_iter().map(|v| with_paths(v, &path)).collect());
    }
    let children = std::mem::take(&mut n.children);
    n.children = children.into_iter().map(|c| with_paths(c, &path)).collect();
    n
}

/// `Config` 的根节点（key/path = "config"，P2）。
///
/// 顶层子节点的路径**不带根前缀**（`providers` 而不是 `config.providers`）——
/// 这正是面板与协议里使用的形状（`node_at` 才容忍可选前缀）。
pub(crate) fn root(children: Vec<SettingNode>) -> SettingNode {
    let mut n = node("config", SettingKind::Object);
    n.path = "config".into();
    n.children = children
        .into_iter()
        .map(|child| with_paths(child, ""))
        .collect();
    n
}

/// 一个够用的示例目录：
///
/// ```text
/// config
///   providers: list[{ name(required) protocol(enum) base_url(required) api_key(secret) models: list[str|ModelSpec] }]
///   gateway:   { port(int) auth: { enabled(bool) } }
///   tools:     list[str]（min_items 1）
///   extra_body: map
/// ```
pub(crate) fn sample_catalog() -> SettingNode {
    let model_spec = element(object(
        "ModelSpec",
        vec![str_field("id"), str_field("display_name")],
    ));
    let mut models = list_of("models", vec![element(str_field("str")), model_spec]);
    models.min_items = Some(1);
    models.summary_fields = vec!["id".into()];

    let provider = element(object(
        "ProviderConfig",
        vec![
            required_str("name"),
            enum_field(
                "protocol",
                &[
                    ("openai", "OpenAI 兼容协议"),
                    ("anthropic", "Anthropic Messages 协议"),
                ],
            ),
            required_str("base_url"),
            secret_field("api_key"),
            float_field("timeout_first_chunk", 300.0),
            models,
        ],
    ));

    let mut providers = list("providers", provider);
    providers.min_items = Some(1);
    providers.summary_fields = vec!["name".into(), "protocol".into()];

    let auth = object("auth", vec![bool_field("enabled", false)]);
    let gateway = object(
        "gateway",
        vec![int_field("port", Some(1.0), Some(65535.0)), auth],
    );

    let tools = list("tools", element(str_field("tool")));

    root(vec![providers, gateway, tools, map_field("extra_body")])
}

// ── 分组（左栏锚点）────────────────────────────────────────

/// 一个分组声明（夹具用；生产路径的两份声明见 `groups.rs` 的模块文档）。
pub(crate) fn group(id: &str, title: &str, members: &[&str]) -> SettingGroup {
    SettingGroup {
        id: id.to_string(),
        title: title.to_string(),
        doc: format!("{title} 的说明"),
        members: members.iter().map(|member| (*member).to_string()).collect(),
    }
}

/// [`sample_catalog`] 的分组表：四个顶层键 → 三个锚点。
pub(crate) fn sample_groups() -> Vec<SettingGroup> {
    vec![
        group("providers", "Providers", &["providers"]),
        group("net", "Net", &["gateway"]),
        group("misc", "Misc", &["tools", "extra_body"]),
    ]
}

/// 示例 Interface 根（两个顶层键）。
pub(crate) fn interface_catalog() -> SettingNode {
    let mut accent = str_field("accent");
    accent.value_hint = Some("color".into());
    root(vec![
        object("colors", vec![accent]),
        object(
            "layout",
            vec![int_field("max_input_lines", Some(1.0), None)],
        ),
    ])
}

/// 示例 Interface 根的分组表（一个锚点装下全部顶层键）。
pub(crate) fn interface_groups() -> Vec<SettingGroup> {
    vec![group("interface", "Interface", &["colors", "layout"])]
}

/// 示例文档：一个 provider（api_key 恒为 null —— 密文三态里的「保留」）。
pub(crate) fn sample_gateway_doc() -> Value {
    json!({
        "providers": [
            {
                "name": "default",
                "protocol": "openai",
                "base_url": "https://api.example.com",
                "api_key": null,
                "models": ["ds-flash"]
            }
        ],
        "gateway": {"port": 32523, "auth": {"enabled": true}},
        "tools": ["Bash", "Read"],
        "extra_body": {"thinking": {"type": "enabled"}}
    })
}

pub(crate) fn sample_doc() -> SettingsDoc {
    SettingsDoc::new(
        sample_gateway_doc(),
        json!({}),
        HashMap::new(),
        "fp-1".into(),
    )
}

/// 带密文表的文档（`api_key` 的 state = set，hint = ab12）。
pub(crate) fn doc_with_secret_state() -> SettingsDoc {
    let mut secrets = HashMap::new();
    secrets.insert(
        "providers[0].api_key".to_string(),
        SecretState {
            state: SecretPresence::Set,
            hint: Some("ab12".into()),
        },
    );
    SettingsDoc::new(sample_gateway_doc(), json!({}), secrets, "fp-1".into())
}

pub(crate) fn empty_doc() -> SettingsDoc {
    SettingsDoc::new(json!({}), json!({}), HashMap::new(), "absent".into())
}

pub(crate) fn problem(root: Root, path: Option<&str>, kind: &str, message: &str) -> Problem {
    Problem {
        root,
        path: path.map(str::to_string),
        kind: kind.into(),
        message: message.into(),
        hint: None,
    }
}
