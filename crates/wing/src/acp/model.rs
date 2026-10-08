//! 模型选择：`model` session config option 的构造、热切换与外部变更中继。
//!
//! ACP 的模型选择 = **session config option**（`category: "model"` 的 select）：客户端从
//! `session/new` / `session/load` / `session/resume` 响应的 `configOptions` 里渲染下拉，
//! 用户选中某一项 → `session/set_config_option`；本模块把值翻译成
//! `POST /api/session/update {model_id}`，成功后回**全量** options（协议语义）。
//!
//! # 值域
//!
//! 广告出去的值 id **就是 model_id**（全局唯一引用词，`providers[].models` 的 `id`）——
//! 不用 provider 前缀消歧：同名模型跨 provider 各自有独立 id，值原样回送即命中。
//! label = 声明的 `display_name`（缺省回落调用名），group = provider（展示分组）。
//!
//! `currentValue`（[`SessionModel::current_value_id`]）= 会话当前的 model_id；为空
//! （旧会话 metadata 无 id 且反查不中）时回落调用名，**不编任何前缀**。
//!
//! 未命中 id 的校验在**网关**（400，错误文案含 available ids 与 name 提示）——前端
//! 只做「非空值」的本地检查，不猜、不回落、不解析字符串结构。
//!
//! # 当前状态的数据源
//!
//! `GET /api/session/get` 的 `agent: AgentInfo`——`model_id` / `provider_name` /
//! `model_display_name` 都在这里（info 端点有 `model_id` + `provider_name`，但 agent
//! 才是本模块一直对账的那一份）。
//!
//! # 中继
//!
//! 其它前端（TUI / 编排 CLI）改模型时网关广播 `session_state_changed{model_id}`；本模块
//! 重新取权威状态构造全量 options，以 `config_option_update` 推给客户端。**触发点在
//! [`super::session::SessionHub::dispatch`] 而不是 prompt 轮次的事件循环**：该事件与
//! 「有没有在途 prompt」无关，空闲时会话事件通道没有消费者（分流即丢弃），写在轮次里
//! 几乎永不触发（design D7）。

use std::sync::Arc;

use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::ConfigOptionUpdate;
use agent_client_protocol::schema::v1::SessionConfigOption;
use agent_client_protocol::schema::v1::SessionConfigOptionCategory;
use agent_client_protocol::schema::v1::SessionConfigSelectGroup;
use agent_client_protocol::schema::v1::SessionConfigSelectOption;
use agent_client_protocol::schema::v1::SessionConfigSelectOptions;
use agent_client_protocol::schema::v1::SessionId;
use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::schema::v1::SessionUpdate;
use agent_client_protocol::schema::v1::SetSessionConfigOptionRequest;
use wing_api_client::ApiClientError;
use wing_api_client::models::ModelDetail;
use wing_api_client::models::ModelsResponse;
use wing_api_client::models::ProviderModels;
use wing_api_client::models::UpdateSessionRequest;

use crate::protocol::WingEvent;

use super::session::SessionHub;

/// `model` config option 的 id（客户端的 `configId`；也是我们自己唯一的 option）。
pub const CONFIG_ID: &str = "model";

/// option 的人读名字（客户端菜单里的标题）。
const CONFIG_NAME: &str = "Model";

// ============================================================
// 领域取值
// ============================================================

/// 会话当前的模型身份：引用词 + 运行期事实 + 展示名。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionModel {
    /// 当前模型的引用词（∈ 配置声明的 id 空间）；不可用时 None。
    /// `currentValue` 的来源——值域与网关的 update 请求同一个命名空间。
    pub model_id: Option<String>,
    /// 当前模型调用名（发给上游的值）；展示回落素材。
    pub model: String,
    /// 配置声明的展示名（展示层专用；未声明 = None）。
    pub display_name: Option<String>,
    /// 当前活跃 provider 名（运行期事实；降级路径可能缺席）。
    pub provider: Option<String>,
}

impl SessionModel {
    /// `currentValue`：model_id 非空 → 它本身；否则回落调用名（trim 后）。
    ///
    /// 空 id（旧会话 / 反查不中）不编前缀：客户端拿到的值必须能原样回到同一个 id。
    pub fn current_value_id(&self) -> String {
        non_blank(self.model_id.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| self.model.trim().to_string())
    }
}

/// `GET /api/models` 的目录 + 会话当前状态 → `model` option（空 vec = 没有可广告的值）。
///
/// 规则（design D1）：
///
/// - 每个 provider 一组（`group` = 组名 = provider 名，目录顺序）；provider 名空 / 模型表空
///   的组丢掉；组内空白 id 与重复值 id 丢掉；
/// - 每个 option 的 value = `detail.id`（引用词）、label = `display_name‖name`（缺省回落
///   调用名）；`description` 只在声明且非空时带；
/// - `currentValue` = [`SessionModel::current_value_id`]（= model_id，空则调用名）；
/// - 当前值不在目录里时**自补一项**（追加到它自己 provider 的组；组不存在就新建一组），
///   这样值列表里总有 `currentValue`——客户端才会显示真实模型而不是 `Unknown`。
pub fn build_options(catalog: &ModelsResponse, current: &SessionModel) -> Vec<SessionConfigOption> {
    let current_value = current.current_value_id();
    let mut seen: Vec<String> = Vec::new();
    let mut groups: Vec<SessionConfigSelectGroup> = Vec::new();
    for provider in &catalog.providers {
        let Some(name) = provider_name(provider) else {
            continue;
        };
        let mut options = Vec::new();
        for detail in &provider.models {
            let Some(id) = non_blank(Some(detail.id.as_str())) else {
                // 空 id 不是可发的值（配置层保证不出现；脏数据丢掉而不是发个死值）。
                continue;
            };
            if seen.iter().any(|value| value == id) {
                tracing::debug!(
                    value = id,
                    "acp: duplicate model id in the catalog; skipped"
                );
                continue;
            }
            seen.push(id.to_string());
            let mut option = SessionConfigSelectOption::new(id.to_string(), detail.display_label());
            if let Some(description) = description_of(detail) {
                option = option.description(description.to_string());
            }
            options.push(option);
        }
        if !options.is_empty() {
            groups.push(SessionConfigSelectGroup::new(
                name.to_string(),
                name.to_string(),
                options,
            ));
        }
    }

    if !seen.contains(&current_value) && !current.model.trim().is_empty() {
        append_current(&mut groups, current, &current_value);
    }
    if groups.is_empty() {
        return Vec::new();
    }
    vec![
        SessionConfigOption::select(
            CONFIG_ID,
            CONFIG_NAME,
            current_value,
            SessionConfigSelectOptions::Grouped(groups),
        )
        .category(SessionConfigOptionCategory::Model),
    ]
}

/// 把「目录里没有的当前值」补进值列表（[`build_options`] 的最后一步）。
///
/// 值 = 当前 model_id（空则调用名），显示名沿用声明的展示名 / 调用名；分组用会话的
/// provider——provider 缺席（降级路径）时补进哪个组都是误导，只记日志。
fn append_current(
    groups: &mut Vec<SessionConfigSelectGroup>,
    current: &SessionModel,
    current_value: &str,
) {
    let label = non_blank(current.display_name.as_deref()).unwrap_or(current.model.trim());
    let option = SessionConfigSelectOption::new(current_value.to_string(), label.to_string());
    match non_blank(current.provider.as_deref()) {
        Some(provider) => match groups
            .iter_mut()
            .find(|group| group.group.0.as_ref() == provider)
        {
            Some(group) => group.options.push(option),
            None => groups.push(SessionConfigSelectGroup::new(
                provider.to_string(),
                provider.to_string(),
                vec![option],
            )),
        },
        // provider 缺席：值本身仍可发（就是 currentValue），但没有诚实的组放它。
        None => tracing::debug!(
            model = %current.model,
            "acp: current model has no provider; not appended to the option values",
        ),
    }
}

/// 事件是否是「模型变更」（中继判据）：`session_state_changed{model_id|model: 非空}`。
///
/// id 是首选判据；旧会话（id 反查不中 → 事件不带 model_id）的变更仍要播报——中继会重取
/// 权威状态，`currentValue` 那时回落调用名。其它字段（title / thinking / yolo / …）不在
/// 本步映射范围。
pub fn is_model_change(event: &WingEvent) -> bool {
    match event {
        WingEvent::SessionStateChanged {
            model_id, model, ..
        } => {
            model_id.as_deref().is_some_and(|id| !id.trim().is_empty())
                || model.as_deref().is_some_and(|m| !m.trim().is_empty())
        }
        _ => false,
    }
}

/// 全量 options → `config_option_update` 帧；空 options → `None`（没有可播报的选择面）。
pub fn config_option_update(options: Vec<SessionConfigOption>) -> Option<SessionUpdate> {
    if options.is_empty() {
        return None;
    }
    Some(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
        options,
    )))
}

// ============================================================
// HTTP 编排
// ============================================================

/// 会话当前状态的 options（`session/new|load|resume` 的响应填充）。
pub async fn options_for(
    hub: &SessionHub,
    session_id: &str,
) -> Result<Vec<SessionConfigOption>, Error> {
    let current = fetch_state(hub, session_id).await?;
    let catalog = fetch_catalog(hub).await?;
    Ok(build_options(&catalog, &current))
}

/// `session/set_config_option` 的全流程：校验 → 更新网关 → 全量 options。
///
/// 值就是 model_id，直接下发——不解析、不补 provider、不回落。未知 id 由网关报 400
/// （错误文案含 available ids + name 提示），映射为 JSON-RPC invalid_params 原文回给客户端。
pub async fn set_config_option(
    hub: &SessionHub,
    request: &SetSessionConfigOptionRequest,
) -> Result<Vec<SessionConfigOption>, Error> {
    let session_id = request.session_id.to_string();
    if !hub.knows(&session_id) {
        return Err(Error::invalid_params().data(format!("unknown session: {session_id}")));
    }
    let model_id = check_request(request.config_id.0.as_ref(), &request.value)?.to_string();

    // 目录先取：它只用于构造返回的**值列表**（不含任何解析），失败就不进入变更——
    // 反向顺序（update 成功、目录失败）会让客户端以为没切成。
    let catalog = fetch_catalog(hub).await?;

    let update = UpdateSessionRequest {
        session_id: session_id.clone(),
        model_id: Some(model_id.clone()),
        ..Default::default()
    };
    hub.http
        .update_session(&update)
        .await
        .map_err(update_error)?;
    tracing::info!(session_id, model_id = %model_id, "acp: model switched");

    // 成功 → 全量 options。**重新取**会话状态：网关是权威，不拿请求参数当结果
    // （网关可能被别的更新覆盖；随后还有一次 `session_state_changed` 中继兜底）。
    //
    // 重取失败**不报错**：切换已经生效（上面的 update 成功了），此刻回 JSON-RPC error 会让
    // 客户端以为没切成——omnigent 还会据此把「本进程不支持模型切换」永久关掉。用请求值
    // 兜底（下一步的中继会纠正成权威值）+ warn 是这里唯一不撒谎的收口。
    let current = match fetch_state(hub, &session_id).await {
        Ok(current) => current,
        Err(err) => {
            tracing::warn!(
                session_id,
                error = %err,
                "acp: could not re-read the session state after the switch; echoing the requested value"
            );
            SessionModel {
                model_id: Some(model_id),
                // 调用名未知（重取失败）：留空让 append 守卫跳过值列表补项——
                // currentValue 仍是请求的 id，客户端照着原值回送即可再次命中。
                model: String::new(),
                display_name: None,
                provider: None,
            }
        }
    };
    Ok(build_options(&catalog, &current))
}

/// 模型变更中继（`session_state_changed{model_id}`）：权威状态 → 全量 options → 通知。
///
/// 合并协议（`SessionEntry::claim_model_relay` / [`SessionHub::finish_model_relay`]）：同一会话同时只有
/// 一次在途中继；期间到达的变更合并成「再跑一次」，而每次重跑都重取权威状态，
/// 所以合并语义 = **以最新状态为准**。
pub async fn relay_model_change(hub: &Arc<SessionHub>, session_id: &str) {
    loop {
        // 没有连接（`initialize` / `connect_with` 之前）时也**必须**走到收尾：否则在途标记
        // 会永远挂着，此后所有模型变更都只会被合并、再也不会播报。
        match hub.client_connection() {
            Some(client) => match options_for(hub, session_id).await {
                Ok(options) => match config_option_update(options) {
                    Some(update) => {
                        if let Err(err) = client.send_notification(SessionNotification::new(
                            SessionId::new(session_id),
                            update,
                        )) {
                            tracing::warn!(
                                session_id,
                                error = %err,
                                "acp: failed to relay the model change to the client"
                            );
                        }
                    }
                    None => tracing::debug!(
                        session_id,
                        "acp: no model options to relay (empty catalog / no current model)"
                    ),
                },
                Err(err) => tracing::warn!(
                    session_id,
                    error = %err,
                    "acp: model change relay could not read the session state; skipped"
                ),
            },
            None => tracing::warn!(
                session_id,
                "acp: model change received before the client connection was registered; not relayed"
            ),
        }
        if !hub.finish_model_relay(session_id) {
            return;
        }
    }
}

/// 请求头的校验（不含要 HTTP 的步骤）：config id 必须是我们广告的 `model`，值必须非空。
///
/// 客户端发来的 `value` 有两种线上形状（ACP schema：无 `type` = 值 id；`type: "boolean"` =
/// 布尔）——我们只广告 select，收到布尔值只能是客户端用错了 id。
fn check_request<'a>(
    config_id: &str,
    value: &'a agent_client_protocol::schema::v1::SessionConfigOptionValue,
) -> Result<&'a str, Error> {
    if config_id != CONFIG_ID {
        return Err(Error::invalid_params().data(format!("unknown config option: {config_id}")));
    }
    let value = value
        .as_value_id()
        .map(|value| value.0.as_ref())
        .ok_or_else(|| {
            Error::invalid_params().data(format!(
                "config option '{CONFIG_ID}' is a select; a boolean value is not accepted"
            ))
        })?;
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::invalid_params().data("empty model value"));
    }
    Ok(value)
}

/// `GET /api/models` → 目录。
async fn fetch_catalog(hub: &SessionHub) -> Result<ModelsResponse, Error> {
    hub.http.get_models().await.map_err(|err| {
        tracing::warn!(error = %err, "acp: /api/models failed");
        Error::internal_error().data(format!("the gateway did not list its models: {err}"))
    })
}

/// `GET /api/session/get` → 会话当前的模型身份。
async fn fetch_state(hub: &SessionHub, session_id: &str) -> Result<SessionModel, Error> {
    let state = hub.http.get_session(session_id).await.map_err(|err| {
        tracing::warn!(session_id, error = %err, "acp: /api/session/get failed");
        state_error(session_id, err)
    })?;
    let Some(agent) = state.agent else {
        return Err(
            Error::internal_error().data(format!("session {session_id} carries no agent info"))
        );
    };
    Ok(SessionModel {
        model_id: agent.model_id,
        model: agent.model_name,
        display_name: agent.model_display_name,
        provider: agent.provider_name,
    })
}

/// 读会话状态失败 → JSON-RPC error（未知会话 = invalid params；其余 = internal error）。
fn state_error(session_id: &str, err: ApiClientError) -> Error {
    if err.is_not_found() {
        return Error::invalid_params().data(format!("unknown session: {session_id}"));
    }
    Error::internal_error().data(format!("could not read session {session_id}: {err}"))
}

/// `POST /api/session/update` 失败 → JSON-RPC error（带网关原文）。
///
/// 4xx = 客户端给的值有问题（未知 model id / 会话不存在）→ invalid params；5xx 与传输层
/// → internal error。错误文本一律带上，别让「为什么不生效」变成黑盒——网关的错误文案
/// （含 available ids + name 提示）是外部编排方唯一的自诊断素材。
fn update_error(err: ApiClientError) -> Error {
    if let ApiClientError::Api { status, .. } = &err
        && (400..500).contains(status)
    {
        return Error::invalid_params().data(err.to_string());
    }
    Error::internal_error().data(err.to_string())
}

// ============================================================
// 小工具
// ============================================================

/// `Some(trim 后非空)`；空白与 None 一律折叠成 None（目录 / 配置里可能有空白项）。
fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// provider 条目的名字（空白 → None）。
fn provider_name(provider: &ProviderModels) -> Option<&str> {
    non_blank(Some(provider.provider.as_str()))
}

/// 模型声明的描述（空白 / 未声明 → None）。
fn description_of(detail: &ModelDetail) -> Option<&str> {
    non_blank(detail.description.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ErrorCode;
    use agent_client_protocol::schema::v1::SessionConfigKind;
    use serde_json::json;

    /// 目录 fixture（走 JSON，与 `/api/models` 同一形状：对象数组 + id）。
    fn catalog() -> ModelsResponse {
        serde_json::from_value(json!({
            "providers": [
                {
                    "provider": "dashscope",
                    "models": [
                        {"id": "glm-4.6", "name": "glm-4.6", "display_name": "GLM-4.6",
                         "description": "智谱旗舰"},
                        {"id": "qwen3-max", "name": "qwen3-max"}
                    ]
                },
                {
                    "provider": "openai",
                    "models": [{"id": "gpt-5", "name": "gpt-5", "display_name": "GPT-5"}]
                }
            ]
        }))
        .expect("catalog fixture decodes")
    }

    fn current(model_id: Option<&str>, model: &str, display_name: Option<&str>) -> SessionModel {
        SessionModel {
            model_id: model_id.map(str::to_string),
            model: model.to_string(),
            display_name: display_name.map(str::to_string),
            provider: Some("dashscope".to_string()),
        }
    }

    fn values(group: &SessionConfigSelectGroup) -> Vec<String> {
        group
            .options
            .iter()
            .map(|option| option.value.0.to_string())
            .collect()
    }

    /// 取唯一 option 的 select 载荷。
    fn select_of(
        options: &[SessionConfigOption],
    ) -> &agent_client_protocol::schema::v1::SessionConfigSelect {
        assert_eq!(options.len(), 1, "本步只广告一个 option");
        let option = &options[0];
        assert_eq!(option.id.0.as_ref(), CONFIG_ID);
        assert_eq!(option.name, CONFIG_NAME);
        assert_eq!(option.category, Some(SessionConfigOptionCategory::Model));
        match &option.kind {
            SessionConfigKind::Select(select) => select,
            other => panic!("model option must be a select, got {other:?}"),
        }
    }

    fn groups_of(options: &[SessionConfigOption]) -> &[SessionConfigSelectGroup] {
        match &select_of(options).options {
            SessionConfigSelectOptions::Grouped(groups) => groups,
            other => panic!("options must be grouped, got {other:?}"),
        }
    }

    fn event(value: serde_json::Value) -> WingEvent {
        serde_json::from_value(value).expect("event fixture decodes")
    }

    // ---- currentValue ----

    #[test]
    fn current_value_is_the_model_id_and_never_a_prefix() {
        // 身份可用：currentValue = model_id（与调用名无关）。
        let value = current(Some("ds-flash"), "dfmodel-2026", Some("DeepSeek-Flash"));
        assert_eq!(value.current_value_id(), "ds-flash");

        // id 缺席（旧会话 / 反查不中）：回落调用名，不编任何前缀。
        let anonymous = SessionModel {
            model_id: None,
            model: "dfmodel-2026".into(),
            display_name: None,
            provider: Some("qoder".into()),
        };
        assert_eq!(anonymous.current_value_id(), "dfmodel-2026");

        // 空白 id 视同缺席（不产出空白值）。
        let blank = SessionModel {
            model_id: Some("   ".into()),
            model: "dfmodel".into(),
            display_name: None,
            provider: None,
        };
        assert_eq!(blank.current_value_id(), "dfmodel");
    }

    /// 每个广告值原样回送即命中它所在的组：**值就是 id**，不需要任何解析
    /// （旧世界的「往返不变量」在这里退化成静态事实：值 = `detail.id`）。
    #[test]
    fn every_advertised_value_is_the_id_of_its_group() {
        let catalog = catalog();
        for state in [
            current(Some("glm-4.6"), "glm-4.6", None),
            current(Some("gone-id"), "beta-1", None),
            current(None, "m2", None),
        ] {
            let options = build_options(&catalog, &state);
            assert_eq!(
                select_of(&options).current_value.0.as_ref(),
                state.current_value_id(),
                "{state:?}: currentValue 就是 current_value_id"
            );
            for group in groups_of(&options) {
                for option in &group.options {
                    let value = option.value.0.to_string();
                    let declared = catalog
                        .providers
                        .iter()
                        .find(|provider| provider.provider == group.group.0.as_ref())
                        .and_then(|provider| provider.find(&value));
                    if let Some(declared) = declared {
                        assert_eq!(declared.id, value, "产值 = 声明 id");
                        assert_eq!(
                            option.name,
                            declared.display_label(),
                            "label = display_name‖name"
                        );
                    } else {
                        // 自补项：值也必须是 identity（id 或调用名），不可能是拼串。
                        assert_eq!(
                            value,
                            state.current_value_id(),
                            "{state:?}: 目录外的值只能是自补的 currentValue"
                        );
                    }
                }
            }
        }
    }

    // ---- build_options ----

    #[test]
    fn build_options_groups_by_provider_with_ids_as_values() {
        let options = build_options(&catalog(), &current(Some("glm-4.6"), "glm-4.6", None));
        let select = select_of(&options);
        assert_eq!(select.current_value.0.as_ref(), "glm-4.6");

        let groups = groups_of(&options);
        assert_eq!(groups.len(), 2, "每个 provider 一组");
        assert_eq!(groups[0].group.0.as_ref(), "dashscope");
        assert_eq!(groups[0].name, "dashscope", "组名 = provider 名");
        assert_eq!(values(&groups[0]), ["glm-4.6", "qwen3-max"]);
        // 声明了展示名 / 描述 → 用它们。
        assert_eq!(groups[0].options[0].name, "GLM-4.6");
        assert_eq!(
            groups[0].options[0].description.as_deref(),
            Some("智谱旗舰")
        );
        // 未声明 → 显示名回落调用名，描述缺席（不发明内容）。
        assert_eq!(groups[0].options[1].name, "qwen3-max");
        assert_eq!(groups[0].options[1].description, None);
        // 目录顺序即分组顺序。
        assert_eq!(groups[1].group.0.as_ref(), "openai");
        assert_eq!(values(&groups[1]), ["gpt-5"]);
        assert_eq!(groups[1].options[0].name, "GPT-5");
    }

    #[test]
    fn build_options_labels_fall_back_to_the_call_name_when_id_differs() {
        // id ≠ name 且未声明展示名：label = 调用名（显示层素材），value = id。
        let catalog: ModelsResponse = serde_json::from_value(json!({
            "providers": [{"provider": "qoder", "models": [
                {"id": "ds-flash", "name": "dfmodel-2026", "display_name": null}
            ]}]
        }))
        .expect("catalog decodes");
        let options = build_options(&catalog, &current(Some("ds-flash"), "dfmodel-2026", None));
        let groups = groups_of(&options);
        assert_eq!(values(&groups[0]), ["ds-flash"]);
        assert_eq!(groups[0].options[0].name, "dfmodel-2026");
    }

    #[test]
    fn build_options_appends_an_out_of_catalog_current_model() {
        // 表外模型（模板声明 / 配置热重载）：补进它自己 provider 的组，
        // 值列表里因此总有 currentValue（客户端才不会显示 Unknown）。
        let options = build_options(
            &catalog(),
            &current(Some("glm-4.6-x"), "glm-4.6-x", Some("GLM-4.6-X")),
        );
        let select = select_of(&options);
        assert_eq!(select.current_value.0.as_ref(), "glm-4.6-x");
        let groups = groups_of(&options);
        assert_eq!(groups.len(), 2, "自补不新建组（该 provider 已在目录里）");
        assert_eq!(values(&groups[0]), ["glm-4.6", "qwen3-max", "glm-4.6-x"]);
        assert_eq!(groups[0].options[2].name, "GLM-4.6-X", "展示名优先");

        // provider 不在目录里 → 末尾新建一组。
        let mut gone = current(Some("beta-1"), "beta-1", None);
        gone.provider = Some("gone".into());
        let options = build_options(&catalog(), &gone);
        let groups = groups_of(&options);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[2].group.0.as_ref(), "gone");
        assert_eq!(values(&groups[2]), ["beta-1"]);
        assert_eq!(groups[2].options[0].name, "beta-1", "无展示名 → 回落调用名");

        // provider 未知：值仍可发（= currentValue），但没有诚实的组放它 → 不补（已知限制）。
        let mut orphan = current(Some("m2"), "m2", None);
        orphan.provider = None;
        let options = build_options(&catalog(), &orphan);
        assert_eq!(select_of(&options).current_value.0.as_ref(), "m2");
        for group in groups_of(&options) {
            assert!(!values(group).contains(&"m2".to_string()));
        }
    }

    #[test]
    fn build_options_appends_the_current_value_when_it_is_only_a_call_name() {
        // id 缺席（旧会话）：currentValue = 调用名；provider 已知 → 补进它的组。
        let anonymous = SessionModel {
            model_id: None,
            model: "legacy-model".into(),
            display_name: Some("Legacy".into()),
            provider: Some("openai".into()),
        };
        let options = build_options(&catalog(), &anonymous);
        assert_eq!(select_of(&options).current_value.0.as_ref(), "legacy-model");
        let groups = groups_of(&options);
        assert_eq!(values(&groups[1]), ["gpt-5", "legacy-model"]);
        assert_eq!(groups[1].options[1].name, "Legacy");
    }

    #[test]
    fn build_options_is_empty_without_usable_values() {
        // 空目录 + 没有当前模型 → 空 vec（调用方不广告 configOptions）。
        let empty: ModelsResponse =
            serde_json::from_value(json!({"providers": []})).expect("empty catalog decodes");
        assert!(build_options(&empty, &SessionModel::default()).is_empty());

        // provider 名空白 / 模型表全空白 id → 组丢掉，同样不广告。
        let blank: ModelsResponse = serde_json::from_value(json!({
            "providers": [
                {"provider": "  ", "models": [{"id": "a", "name": "a"}]},
                {"provider": "p", "models": [{"id": "", "name": ""}, {"id": "   ", "name": "x"}]},
            ]
        }))
        .expect("blank catalog decodes");
        assert!(build_options(&blank, &SessionModel::default()).is_empty());

        // 目录里有重复 id（同一模型列两次）→ 去重，不产出重复项。
        let dup: ModelsResponse = serde_json::from_value(json!({
            "providers": [{"provider": "p", "models": [
                {"id": "m", "name": "m"}, {"id": " m ", "name": "m"}, {"id": "n", "name": "n"}
            ]}]
        }))
        .expect("dup catalog decodes");
        let options = build_options(&dup, &SessionModel::default());
        assert_eq!(values(&groups_of(&options)[0]), ["m", "n"]);
    }

    #[test]
    fn build_options_matches_the_wire_frame_sample() {
        // 线上形状的逐键对账（值 = id、currentValue = id、组名 = provider）。
        let options = build_options(&catalog(), &current(Some("glm-4.6"), "glm-4.6", None));
        let value = serde_json::to_value(&options[0]).expect("option serializes");
        assert_eq!(
            value,
            json!({
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "glm-4.6",
                "options": [
                    {"group": "dashscope", "name": "dashscope", "options": [
                        {"value": "glm-4.6", "name": "GLM-4.6", "description": "智谱旗舰"},
                        {"value": "qwen3-max", "name": "qwen3-max"}
                    ]},
                    {"group": "openai", "name": "openai", "options": [
                        {"value": "gpt-5", "name": "GPT-5"}
                    ]}
                ]
            })
        );
    }

    // ---- 中继映射 ----

    #[test]
    fn is_model_change_fires_for_an_id_or_a_legacy_name() {
        assert!(is_model_change(&event(json!({
            "type": "session_state_changed",
            "model": "dfmodel-2026",
            "model_id": "ds-flash",
            "provider_name": "qoder",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-1",
        }))));
        // id 缺席（旧会话）但调用名变了：中继同样要播报（currentValue 回落调用名）。
        assert!(is_model_change(&event(json!({
            "type": "session_state_changed",
            "model": "legacy-model",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-1",
        }))));

        // 只改了标题 / thinking / yolo：本步不映射（标题由 translate 的既有映射负责）。
        for other in [
            json!({"title": "新标题"}),
            json!({"thinking": true}),
            json!({"yolo": true}),
            json!({"model": null, "model_id": null}),
            json!({"model": "   ", "model_id": "  "}),
        ] {
            let mut payload = other;
            payload["type"] = json!("session_state_changed");
            payload["created_at"] = json!("2026-01-01T00:00:00+00:00");
            payload["session_id"] = json!("s1");
            payload["request_id"] = json!("req-1");
            assert!(
                !is_model_change(&event(payload.clone())),
                "不该中继: {payload}"
            );
        }

        assert!(!is_model_change(&event(json!({
            "type": "text",
            "content": "hi",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-1",
        }))));
    }

    #[test]
    fn config_option_update_carries_the_full_option_list() {
        assert!(config_option_update(Vec::new()).is_none(), "没有值就不播报");

        let options = build_options(&catalog(), &current(Some("qwen3-max"), "qwen3-max", None));
        let update = config_option_update(options.clone()).expect("options → frame");
        let SessionUpdate::ConfigOptionUpdate(update) = update else {
            panic!("expected config_option_update");
        };
        assert_eq!(update.config_options, options);

        // 线上形状：session/update 信封 + 全量 options。
        let notification = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options)),
        );
        let frame = serde_json::to_value(&notification).expect("notification serializes");
        assert_eq!(frame["sessionId"], "s1");
        assert_eq!(frame["update"]["sessionUpdate"], "config_option_update");
        assert_eq!(
            frame["update"]["configOptions"][0]["currentValue"],
            "qwen3-max"
        );
        assert_eq!(
            frame["update"]["configOptions"][0]["options"][0]["group"],
            "dashscope"
        );
    }

    // ---- 请求解码 / 校验 ----

    #[test]
    fn set_config_option_accepts_the_model_id_wire_shape() {
        // 值 id 形状（无 `type`）：值就是 model_id（含 `:` / `/` 等可见字符也是合法 id）。
        for value in ["ds-flash", "qwen2.5:7b", "  ds-flash  "] {
            let request: SetSessionConfigOptionRequest = serde_json::from_value(json!({
                "sessionId": "s1",
                "configId": "model",
                "value": value,
            }))
            .expect("request fixture decodes");
            assert_eq!(
                check_request(request.config_id.0.as_ref(), &request.value)
                    .expect("value id is accepted"),
                value.trim()
            );
        }
    }

    #[test]
    fn set_config_option_rejects_unknown_ids_blank_values_and_booleans() {
        // 未知 config id（客户端拼错，或将来才有的 option）。
        let unknown: SetSessionConfigOptionRequest = serde_json::from_value(json!({
            "sessionId": "s1",
            "configId": "yolo",
            "value": "true",
        }))
        .expect("request fixture decodes");
        assert_eq!(
            check_request(unknown.config_id.0.as_ref(), &unknown.value)
                .expect_err("unknown config id is rejected")
                .code,
            ErrorCode::InvalidParams
        );

        // 空值 / 纯空白：没有可发的 id。
        for value in ["", "   "] {
            let blank: SetSessionConfigOptionRequest = serde_json::from_value(json!({
                "sessionId": "s1",
                "configId": "model",
                "value": value,
            }))
            .expect("request fixture decodes");
            assert_eq!(
                check_request(blank.config_id.0.as_ref(), &blank.value)
                    .expect_err("blank value is rejected")
                    .code,
                ErrorCode::InvalidParams
            );
        }

        // 布尔值：我们只广告 select，收到 `type:"boolean"` 只能是客户端用错了 id。
        let boolean: SetSessionConfigOptionRequest = serde_json::from_value(json!({
            "sessionId": "s1",
            "configId": "model",
            "type": "boolean",
            "value": true,
        }))
        .expect("boolean fixture decodes");
        assert!(boolean.value.as_value_id().is_none());
        assert_eq!(
            check_request(boolean.config_id.0.as_ref(), &boolean.value)
                .expect_err("boolean value is rejected")
                .code,
            ErrorCode::InvalidParams
        );
    }

    #[test]
    fn update_errors_map_4xx_to_invalid_params_and_keep_the_gateway_text() {
        let bad_request = ApiClientError::Api {
            status: 400,
            detail: "unknown model id 'nope'; available ids: ds-flash, ds-pro".into(),
            body: None,
        };
        let error = update_error(bad_request);
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert!(
            error.to_string().contains("unknown model id 'nope'"),
            "网关原文必须带上: {error}"
        );

        let not_found = ApiClientError::Api {
            status: 404,
            detail: "session not found".into(),
            body: None,
        };
        assert_eq!(update_error(not_found).code, ErrorCode::InvalidParams);

        let server_error = ApiClientError::Api {
            status: 500,
            detail: "boom".into(),
            body: None,
        };
        assert_eq!(update_error(server_error).code, ErrorCode::InternalError);
        assert_eq!(
            update_error(ApiClientError::Connection("reset".into())).code,
            ErrorCode::InternalError
        );
    }
}
