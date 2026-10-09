//! 帧测试夹具：手工构造的 catalog / 文档 / 面板，加一组按键驱动辅助。
//!
//! 构造器只填测试真正读的字段，其余走 [`node`] 的合理缺省 —— 每条用例都能一眼看清
//! 「这个目录长什么样」。**这些夹具是 ui/settings 自己的**（07 的 `test_support` 是
//! settings 包私有的），刻意多带了渲染要断言的东西：`notes` / `example` / 约束 /
//! 一条超长 `doc` / 一个 `value_hint = "color"` 的颜色字段。

use std::collections::HashMap;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use serde_json::Value;
use serde_json::json;
use wing_api_client::models::ApplyScope;
use wing_api_client::models::SecretPresence;
use wing_api_client::models::SecretState;
use wing_api_client::models::SettingChoice;
use wing_api_client::models::SettingKind;
use wing_api_client::models::SettingNode;
use wing_api_client::models::SettingsGetResponse;
use wing_api_client::models::SettingsSchemaResponse;

use crate::config::ThemePalette;
use crate::shared::panels::settings::InterfaceSource;
use crate::shared::panels::settings::RowMarkers;
use crate::shared::panels::settings::SettingsPanel;
use crate::shared::panels::settings::View;

/// 路径拼接（`join_path` 在 07 里是包私有的，夹具自带一份）。
fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

pub(crate) fn palette() -> ThemePalette {
    ThemePalette::default()
}

// ── 目录节点 ────────────────────────────────────────────────

/// 合理缺省：editable、hot、无约束、无默认。
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
    let mut node = node(key, SettingKind::Object);
    node.children = children;
    node
}

pub(crate) fn list(key: &str, element: SettingNode) -> SettingNode {
    let mut node = node(key, SettingKind::List);
    node.element = Some(Box::new(element));
    node
}

pub(crate) fn list_of(key: &str, variants: Vec<SettingNode>) -> SettingNode {
    let mut node = node(key, SettingKind::List);
    node.variants = Some(variants);
    node
}

pub(crate) fn element(inner: SettingNode) -> SettingNode {
    SettingNode {
        key: "[]".into(),
        ..inner
    }
}

pub(crate) fn str_field(key: &str) -> SettingNode {
    node(key, SettingKind::Str)
}

pub(crate) fn required_str(key: &str) -> SettingNode {
    let mut node = node(key, SettingKind::Str);
    node.required = true;
    node
}

pub(crate) fn int_field(key: &str, min: Option<f64>, max: Option<f64>) -> SettingNode {
    let mut node = node(key, SettingKind::Int);
    node.min = min;
    node.max = max;
    node.has_default = true;
    node.default = Some(json!(0));
    node
}

pub(crate) fn float_field(key: &str, default: f64) -> SettingNode {
    let mut node = node(key, SettingKind::Float);
    node.has_default = true;
    node.default = Some(json!(default));
    node
}

pub(crate) fn bool_field(key: &str, default: bool) -> SettingNode {
    let mut node = node(key, SettingKind::Bool);
    node.has_default = true;
    node.default = Some(json!(default));
    node
}

pub(crate) fn secret_field(key: &str) -> SettingNode {
    let mut node = node(key, SettingKind::Secret);
    node.secret = true;
    node
}

pub(crate) fn enum_field(key: &str, values: &[(&str, &str)]) -> SettingNode {
    let mut node = node(key, SettingKind::Enum);
    node.choices = values
        .iter()
        .map(|(value, doc)| SettingChoice {
            value: (*value).into(),
            doc: Some((*doc).into()),
        })
        .collect();
    node
}

pub(crate) fn map_field(key: &str) -> SettingNode {
    let mut node = node(key, SettingKind::Map);
    node.has_default = true;
    node.default = Some(json!({}));
    node
}

/// 递归重算全部 path（`[]` = 元素模板）。
pub(crate) fn with_paths(mut node: SettingNode, prefix: &str) -> SettingNode {
    node.path = if node.key == "[]" {
        format!("{prefix}[]")
    } else {
        join_path(prefix, &node.key)
    };
    let path = node.path.clone();
    if let Some(child) = node.element.take() {
        node.element = Some(Box::new(with_paths(*child, &path)));
    }
    if let Some(variants) = node.variants.take() {
        node.variants = Some(variants.into_iter().map(|v| with_paths(v, &path)).collect());
    }
    let children = std::mem::take(&mut node.children);
    node.children = children.into_iter().map(|c| with_paths(c, &path)).collect();
    node
}

pub(crate) fn root(children: Vec<SettingNode>) -> SettingNode {
    let mut node = node("config", SettingKind::Object);
    node.path = "config".into();
    node.title = "配置".into();
    node.children = children
        .into_iter()
        .map(|child| with_paths(child, ""))
        .collect();
    node
}

/// 示例目录（Gateway 根）：
///
/// ```text
/// config
///   providers: list[{ name(required) protocol(enum) base_url(required, example)
///                     api_key(secret) timeout_first_chunk(float, notes)
///                     models: list[str|ModelSpec](min_items 1) }](min_items 1)
///   gateway:   { port(int 1..65535), auth: { enabled(bool) } }
///   tools:     list[str]（min_items 1）
///   extra_body: map
///   log:       { level(enum, nullable) }
/// ```
pub(crate) fn sample_catalog() -> SettingNode {
    let model_spec = element(object(
        "ModelSpec",
        vec![str_field("id"), str_field("display_name")],
    ));
    let mut models = list_of("models", vec![element(str_field("str")), model_spec]);
    models.min_items = Some(1);
    models.summary_fields = vec!["id".into()];

    let mut timeout = float_field("timeout_first_chunk", 300.0);
    timeout.doc = "流式首块超时（秒）".into();
    timeout.notes = vec![
        "这是**响应头**超时。响应体停滞另有硬编码 120s 判定（provider/transport.py）。".into(),
        "第二行详解：给得很长的说明文字是为了让详情栏必须折行。".into(),
    ];

    let mut base_url = required_str("base_url");
    base_url.example = Some("https://api.example.com/v1".into());

    let mut provider = element(object(
        "ProviderConfig",
        vec![
            required_str("name"),
            enum_field(
                "protocol",
                &[
                    ("openai", "OpenAI 兼容协议（/chat/completions）"),
                    ("anthropic", "Anthropic Messages 协议"),
                ],
            ),
            base_url,
            secret_field("api_key"),
            timeout,
            models,
        ],
    ));
    provider.title = "Provider".into();

    let mut providers = list("providers", provider);
    providers.min_items = Some(1);
    providers.summary_fields = vec!["name".into(), "protocol".into()];

    let auth = object("auth", vec![bool_field("enabled", false)]);
    let gateway = object(
        "gateway",
        vec![int_field("port", Some(1.0), Some(65535.0)), auth],
    );

    let tools = list("tools", element(str_field("tool")));

    let mut level = enum_field(
        "level",
        &[("debug", "调试"), ("info", "信息"), ("warn", "警告")],
    );
    level.nullable = true;
    let log = object("log", vec![level]);

    root(vec![
        providers,
        gateway,
        tools,
        map_field("extra_body"),
        log,
    ])
}

/// Interface 根（TUI 自己的配置）：带一个 `value_hint = "color"` 的颜色字段。
pub(crate) fn interface_catalog() -> SettingNode {
    let mut accent = str_field("accent");
    accent.value_hint = Some("color".into());
    accent.doc = "强调色（命名色或 #RRGGBB）".into();
    let preset = enum_field("preset", &[("wing", "品牌配色"), ("terminal", "跟随终端")]);
    let colors = object("colors", vec![preset, accent]);
    let layout = object(
        "layout",
        vec![int_field("max_input_lines", Some(1.0), None)],
    );
    root(vec![colors, layout])
}

// ── 文档 / 面板 ─────────────────────────────────────────────

pub(crate) fn sample_values() -> Value {
    json!({
        "providers": [{
            "name": "default",
            "protocol": "openai",
            "base_url": "https://api.example.com",
            "api_key": null,
            "models": ["ds-flash"]
        }],
        "gateway": {"port": 32523, "auth": {"enabled": true}},
        "tools": ["Bash", "Read"],
        "extra_body": {"thinking": {"type": "enabled"}}
    })
}

/// 密文表：`api_key` 的 state = set，hint = `ab12`（渲染成 `•••••••• ab12`）。
pub(crate) fn sample_secrets() -> HashMap<String, SecretState> {
    let mut secrets = HashMap::new();
    secrets.insert(
        "providers[0].api_key".to_string(),
        SecretState {
            state: SecretPresence::Set,
            hint: Some("ab12".into()),
        },
    );
    secrets
}

pub(crate) fn schema(catalog: &SettingNode) -> SettingsSchemaResponse {
    SettingsSchemaResponse {
        version: "0.0.0-test".into(),
        root: catalog.clone(),
        config_path: "/home/u/.wing/core/config.yaml".into(),
    }
}

pub(crate) fn state(values: Value) -> SettingsGetResponse {
    SettingsGetResponse {
        values,
        secrets: sample_secrets(),
        fingerprint: "fp-1".into(),
        problems: vec![],
        setup_mode: false,
        config_path: "/home/u/.wing/core/config.yaml".into(),
    }
}

/// Interface 根的注入内容（catalog + 稀疏文档）。
pub(crate) fn interface_source(catalog: SettingNode) -> InterfaceSource {
    InterfaceSource {
        catalog,
        doc: json!({"colors": {"accent": "cyan", "preset": "wing"}}),
    }
}

/// 只带后端问题的面板（目录是空根 → 没有本地问题，问题表就是传进来的那些）。
pub(crate) fn panel_from_problems(
    problems: Vec<wing_api_client::models::SettingProblem>,
) -> (SettingNode, SettingsPanel) {
    let catalog = root(vec![]);
    let mut get = state(json!({}));
    get.problems = problems;
    let panel = SettingsPanel::new(&schema(&catalog), get, None, View::Problems);
    (catalog, panel)
}

/// 一个 30 项（可参数化）的标量列表：窗口滚动 / 长列表测试用。
pub(crate) fn big_list(count: usize) -> (SettingNode, SettingsPanel) {
    let catalog = root(vec![list("items", element(str_field("item")))]);
    let values = json!({
        "items": (0..count).map(|i| format!("item{i:02}")).collect::<Vec<_>>()
    });
    let panel = SettingsPanel::new(&schema(&catalog), state(values), None, View::Tree);
    (catalog, panel)
}

/// 打开面板（Gateway 根、树视图、无 Interface）。
pub(crate) fn panel() -> SettingsPanel {
    let catalog = sample_catalog();
    SettingsPanel::new(&schema(&catalog), state(sample_values()), None, View::Tree)
}

/// 一把问题（`count` 条；文案够长，足以触发折行与窗口数学）。
pub(crate) fn panel_with_many_problems(count: usize) -> SettingsPanel {
    let catalog = sample_catalog();
    let mut get = state(json!({}));
    get.problems = (0..count)
        .map(|index| wing_api_client::models::SettingProblem {
            path: Some(format!("providers[{index}].base_url")),
            kind: "invalid_value".into(),
            message: format!("第 {index} 条问题：{}", "很长的问题描述".repeat(3)),
            hint: Some("很长很长的提示".repeat(2)),
        })
        .collect();
    SettingsPanel::new(&schema(&catalog), get, None, View::Problems)
}

pub(crate) fn setup_panel() -> (SettingNode, SettingsPanel) {
    let catalog = sample_catalog();
    let mut get = state(json!({}));
    get.setup_mode = true;
    get.problems = vec![
        wing_api_client::models::SettingProblem {
            path: Some("providers".into()),
            kind: "empty_list".into(),
            message: "providers 不得为空".into(),
            hint: Some("至少声明一个 provider".into()),
        },
        wing_api_client::models::SettingProblem {
            path: None,
            kind: "invalid_value".into(),
            message: "配置里出现了未知键".into(),
            hint: None,
        },
    ];
    let panel = SettingsPanel::new(&schema(&catalog), get, None, View::Problems);
    (catalog, panel)
}

// ── 按键驱动 ────────────────────────────────────────────────

pub(crate) fn press(panel: &mut SettingsPanel, code: KeyCode) {
    panel.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

pub(crate) fn press_with(panel: &mut SettingsPanel, code: KeyCode, modifiers: KeyModifiers) {
    panel.handle_key(KeyEvent::new(code, modifiers));
}

pub(crate) fn type_text(panel: &mut SettingsPanel, text: &str) {
    for ch in text.chars() {
        press(panel, KeyCode::Char(ch));
    }
}

/// 用搜索跳到含 `needle` 的第一行（祖先自动展开，Enter 退出搜索并保留光标）。
pub(crate) fn goto(panel: &mut SettingsPanel, needle: &str) {
    press(panel, KeyCode::Char('/'));
    type_text(panel, needle);
    press(panel, KeyCode::Enter);
}

/// 跳到某一行并打开它的内联编辑器。
pub(crate) fn open_editor(panel: &mut SettingsPanel, needle: &str) {
    goto(panel, needle);
    press(panel, KeyCode::Enter);
}

/// 行标记的合理缺省（tree.rs / detail.rs 的单元测试直接构造 `Row` 时用）。
pub(crate) fn markers() -> RowMarkers {
    RowMarkers {
        dirty: false,
        problem: false,
        required: false,
        secret: false,
        apply: ApplyScope::Hot,
    }
}
