//! `wing config` 的测试夹具：一份覆盖关键形态的目录、一份对齐它的文档、一个可注入的假后端。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{Value, json};
use wing_api_client::ApiClientError;
use wing_api_client::models::{
    ApplyScope, PathStep, SettingChoice, SettingKind, SettingNode, SettingProblem,
    SettingsGetResponse, SettingsSchemaResponse, SettingsSetRequest, SettingsSetResponse,
    SettingsStatusResponse, parse_path,
};

use super::ConfigCommand;
use super::SettingsApi;
use super::execute;

// ------------------------------------------------------------
// fixture：目录
// ------------------------------------------------------------

/// 一个节点（全部约束先取缺省；各 fixture 自行覆盖需要的字段）。
pub(super) fn node(key: &str, path: &str, kind: SettingKind) -> SettingNode {
    SettingNode {
        key: key.to_string(),
        path: path.to_string(),
        title: String::new(),
        doc: format!("{key} 的一行说明"),
        notes: Vec::new(),
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
        choices: Vec::new(),
        min_items: None,
        max_items: None,
        secret: false,
        apply: ApplyScope::Hot,
        editable: true,
        deprecated: None,
        section: None,
        section_doc: None,
        children: Vec::new(),
        element: None,
        variants: None,
        summary_fields: Vec::new(),
        value_hint: None,
    }
}

pub(super) fn choice(value: &str, doc: &str) -> SettingChoice {
    SettingChoice {
        value: value.to_string(),
        doc: Some(doc.to_string()),
    }
}

/// 覆盖关键形态的目录：对象 / 嵌套对象 / 列表（单一元素）/ 列表（union）/ map /
/// secret / enum / 声明默认值 / section 分组 / restart 生效域。
pub(super) fn catalog() -> SettingNode {
    // providers[].*
    let mut name = node("name", "providers[].name", SettingKind::Str);
    name.required = true;
    name.pattern = Some("^[a-zA-Z0-9_-]+$".to_string());
    let mut protocol = node("protocol", "providers[].protocol", SettingKind::Enum);
    protocol.choices = vec![
        choice("openai", "OpenAI 兼容"),
        choice("anthropic", "Anthropic"),
    ];
    protocol.default = Some(json!("openai"));
    protocol.has_default = true;
    let mut base_url = node("base_url", "providers[].base_url", SettingKind::Str);
    base_url.required = true;
    let mut api_key = node("api_key", "providers[].api_key", SettingKind::Secret);
    api_key.required = true;
    api_key.secret = true;
    let model_id = node("id", "providers[].models[].id", SettingKind::Str);
    let mut model_spec = node("[]", "providers[].models[]", SettingKind::Object);
    model_spec.children = vec![model_id];
    let model_str = node("[]", "providers[].models[]", SettingKind::Str);
    let mut models = node("models", "providers[].models", SettingKind::List);
    models.variants = Some(vec![model_str, model_spec]);
    let extra_body = node("extra_body", "providers[].extra_body", SettingKind::Map);
    let mut provider = node("[]", "providers[]", SettingKind::Object);
    provider.children = vec![name, protocol, base_url, api_key, models, extra_body];
    let mut providers = node("providers", "providers", SettingKind::List);
    providers.element = Some(Box::new(provider));
    providers.min_items = Some(1);
    providers.section = Some("Providers".to_string());

    // agents[].*
    let mut agent_name = node("name", "agents[].name", SettingKind::Str);
    agent_name.required = true;
    let mut agent_model = node("model", "agents[].model", SettingKind::Str);
    agent_model.required = true;
    let mut agent = node("[]", "agents[]", SettingKind::Object);
    agent.children = vec![agent_name, agent_model];
    let mut agents = node("agents", "agents", SettingKind::List);
    agents.element = Some(Box::new(agent));
    agents.min_items = Some(1);
    agents.section = Some("Agents".to_string());

    // gateway.*（restart 生效域 + 嵌套对象）
    let mut host = node("host", "gateway.host", SettingKind::Str);
    host.default = Some(json!("127.0.0.1"));
    host.has_default = true;
    let mut port = node("port", "gateway.port", SettingKind::Int);
    port.default = Some(json!(32523));
    port.has_default = true;
    port.min = Some(1.0);
    let mut auth_enabled = node("enabled", "gateway.auth.enabled", SettingKind::Bool);
    auth_enabled.default = Some(json!(false));
    auth_enabled.has_default = true;
    let mut auth = node("auth", "gateway.auth", SettingKind::Object);
    auth.children = vec![auth_enabled];
    let mut gateway = node("gateway", "gateway", SettingKind::Object);
    gateway.children = vec![host, port, auth];
    gateway.section = Some("Gateway".to_string());
    gateway.apply = ApplyScope::Restart;

    // images.max_bytes
    let mut max_bytes = node("max_bytes", "images.max_bytes", SettingKind::Int);
    max_bytes.default = Some(json!(1048576));
    max_bytes.has_default = true;
    max_bytes.min = Some(1.0);
    max_bytes.section = Some("Images".to_string());
    let mut images = node("images", "images", SettingKind::Object);
    images.children = vec![max_bytes];
    images.section = Some("Images".to_string());

    // tools（标量列表 + 声明默认值：add 的物化路径）
    let mut tools = node("tools", "tools", SettingKind::List);
    tools.element = Some(Box::new(node("[]", "tools[]", SettingKind::Str)));
    tools.default = Some(json!(["bash"]));
    tools.has_default = true;

    // log.level（enum 默认值）
    let mut level = node("level", "log.level", SettingKind::Enum);
    level.choices = vec![choice("info", "常规"), choice("debug", "调试")];
    level.default = Some(json!("info"));
    level.has_default = true;
    let mut log = node("log", "log", SettingKind::Object);
    log.children = vec![level];

    let mut root = node("config", "config", SettingKind::Object);
    root.children = vec![providers, agents, gateway, images, tools, log];
    root
}

/// 一份合法文档（与 [`catalog`] 对齐的最小形态）。
pub(super) fn document() -> Value {
    json!({
        "providers": [
            {
                "name": "qoder",
                "protocol": "openai",
                "base_url": "https://example.com/v1",
                "api_key": null,
                "models": ["ds-flash", {"id": "ds-pro"}]
            }
        ],
        "agents": [{"name": "default", "model": "ds-flash"}],
        "gateway": {"port": 32523},
        "tools": ["bash"]
    })
}

pub(super) fn get_response(values: Value) -> SettingsGetResponse {
    SettingsGetResponse {
        values,
        secrets: HashMap::new(),
        fingerprint: "fp-1".to_string(),
        problems: Vec::new(),
        setup_mode: false,
        config_path: "/tmp/wing/config.yaml".to_string(),
    }
}

pub(super) fn ok_set_response() -> SettingsSetResponse {
    SettingsSetResponse {
        warnings: Vec::new(),
        ok: true,
        fingerprint: "fp-2".to_string(),
        problems: Vec::new(),
        changed: vec!["gateway.port".to_string()],
        restart_required: Vec::new(),
        reload: None,
        setup_mode_exited: false,
        backup_path: Some("/tmp/wing/config.yaml.bak".to_string()),
    }
}

pub(super) fn problem(
    path: Option<&str>,
    kind: &str,
    message: &str,
    hint: Option<&str>,
) -> SettingProblem {
    SettingProblem {
        path: path.map(str::to_string),
        kind: kind.to_string(),
        message: message.to_string(),
        hint: hint.map(str::to_string),
    }
}

// ------------------------------------------------------------
// 假后端（可注入请求层）
// ------------------------------------------------------------

/// 假后端能制造的错误（`ApiClientError` 不可 Clone，所以存构造参数）。
#[derive(Debug, Clone, Copy)]
pub(super) enum FakeError {
    Unreachable,
    Conflict,
    NotFound,
    Server,
}

impl FakeError {
    fn to_api_error(self) -> ApiClientError {
        match self {
            FakeError::Unreachable => ApiClientError::Connection("connection refused".to_string()),
            FakeError::Conflict => ApiClientError::Api {
                status: 409,
                detail: "fingerprint mismatch".to_string(),
                body: None,
            },
            FakeError::NotFound => ApiClientError::Api {
                status: 404,
                detail: "Not Found".to_string(),
                body: None,
            },
            FakeError::Server => ApiClientError::Api {
                status: 500,
                detail: "boom".to_string(),
                body: None,
            },
        }
    }
}

/// 四路应答可独立指定的假后端；`set` 请求全部被捕获供断言。
pub(super) struct FakeBackend {
    pub(super) schema: Result<SettingsSchemaResponse, FakeError>,
    pub(super) get: Result<SettingsGetResponse, FakeError>,
    pub(super) status: Result<SettingsStatusResponse, FakeError>,
    pub(super) set: Result<SettingsSetResponse, FakeError>,
    pub(super) captured: Mutex<Vec<SettingsSetRequest>>,
}

impl FakeBackend {
    pub(super) fn healthy(root: SettingNode, values: Value) -> Self {
        Self {
            schema: Ok(SettingsSchemaResponse {
                version: "0.0.0".to_string(),
                root: root.clone(),
                config_path: "/tmp/wing/config.yaml".to_string(),
                // CLI 不消费分组；空表 = 面板自己按 section 兜底推导。
                groups: Vec::new(),
            }),
            get: Ok(get_response(values)),
            status: Ok(SettingsStatusResponse {
                valid: true,
                setup_mode: false,
                problems: Vec::new(),
                fingerprint: Some("fp-1".to_string()),
            }),
            set: Ok(ok_set_response()),
            captured: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn requests(&self) -> Vec<SettingsSetRequest> {
        self.captured.lock().unwrap().clone()
    }

    pub(super) fn last_document(&self) -> Value {
        let requests = self.requests();
        requests
            .last()
            .expect("expected one settings_set call")
            .document
            .clone()
    }
}

impl SettingsApi for FakeBackend {
    async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError> {
        self.schema.clone().map_err(FakeError::to_api_error)
    }

    async fn get(&self) -> Result<SettingsGetResponse, ApiClientError> {
        self.get.clone().map_err(FakeError::to_api_error)
    }

    async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError> {
        self.status.clone().map_err(FakeError::to_api_error)
    }

    async fn set(&self, req: &SettingsSetRequest) -> Result<SettingsSetResponse, ApiClientError> {
        self.captured.lock().unwrap().push(req.clone());
        self.set.clone().map_err(FakeError::to_api_error)
    }
}

/// 跑一条命令（测试入口）：返回（退出码, stdout, stderr）三元组。
pub(super) async fn run_cmd(
    backend: &FakeBackend,
    command: ConfigCommand,
    json: bool,
) -> (u8, String, String) {
    let outcome = execute(command, json, backend, "http://127.0.0.1:32523").await;
    (
        outcome.code,
        outcome.stdout.unwrap_or_default(),
        outcome.stderr.unwrap_or_default(),
    )
}

pub(super) fn steps(path: &str) -> Vec<PathStep> {
    parse_path(path).expect("fixture path must parse")
}
