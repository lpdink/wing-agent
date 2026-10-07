//! 模型选择：`model` session config option 的构造、热切换与外部变更中继。
//!
//! ACP 的模型选择 = **session config option**（`category: "model"` 的 select）：客户端从
//! `session/new` / `session/load` / `session/resume` 响应的 `configOptions` 里渲染下拉，
//! 用户选中某一项 → `session/set_config_option`；本模块把值翻译成
//! `POST /api/session/update {model, provider}`，成功后回**全量** options（协议语义）。
//!
//! # 值域与解析
//!
//! 广告出去的值 id 一律 `"{provider}:{model}"`（provider 名逐字前缀 + `:`）——同名模型
//! 跨 provider 必须能消歧。收进来的值两种都收（[`parse_value_id`]）：
//!
//! - `provider:model`（Zed 与我们自己广告的形状，按「已知 provider 列表」**最长前缀**匹配）；
//! - 裸模型名（omnigent 的 `/model` 形状），provider 由 [`resolve_provider`] 从目录 / 当前
//!   会话归属里解析。
//!
//! `currentValue`（[`SessionModel::current_value_id`]）必须与我们自己的解析**往返**：
//! provider 已知 → `provider:model`；provider 缺席（旧网关）→ 裸模型名，不编前缀。
//!
//! # 当前状态的数据源
//!
//! `GET /api/session/get` 的 `agent: AgentInfo`——`provider_name` 只在这里
//! （`/api/session/info` 没有该字段，见 design D3）。
//!
//! # 中继
//!
//! 其它前端（TUI / 编排 CLI）改模型时网关广播 `session_state_changed{model}`；本模块
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
use wing_api_client::models::ModelsResponse;
use wing_api_client::models::ProviderModels;
use wing_api_client::models::UpdateSessionRequest;

use crate::protocol::WingEvent;

use super::session::SessionHub;

/// `model` config option 的 id（客户端的 `configId`；也是我们自己唯一的 option）。
pub const CONFIG_ID: &str = "model";

/// option 的人读名字（客户端菜单里的标题）。
const CONFIG_NAME: &str = "Model";

/// 值 id 里 provider 与模型的分隔符（`{provider}:{model}`）。
const VALUE_SEPARATOR: char = ':';

// ============================================================
// 领域取值
// ============================================================

/// 会话当前的模型身份（值 id 的两半 + 展示名）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionModel {
    /// 当前活跃 provider 名（旧网关 / 降级路径可能缺席）。
    pub provider: Option<String>,
    /// 当前模型调用名（身份）；空串 = 未知。
    pub model: String,
    /// 配置声明的展示名（展示层专用；未声明 = None）。
    pub display_name: Option<String>,
}

impl SessionModel {
    /// `currentValue`：provider 非空 → `"{provider}:{model}"`；否则裸模型名。
    ///
    /// 缺席就不编前缀——客户端拿到的值必须能回到同一个 (provider, model)。
    pub fn current_value_id(&self) -> String {
        match non_blank(self.provider.as_deref()) {
            Some(provider) => format!("{provider}{VALUE_SEPARATOR}{}", self.model),
            None => self.model.clone(),
        }
    }
}

/// 一次值解析的产物（`provider = None` = 裸模型形态，归属待 [`resolve_provider`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: Option<String>,
    pub model: String,
}

/// 目录里的 provider 名（解析值 id 的「已知 provider 列表」，保序）。
pub fn provider_names(catalog: &ModelsResponse) -> Vec<String> {
    catalog
        .providers
        .iter()
        .filter_map(|provider| non_blank(Some(provider.provider.as_str())))
        .map(str::to_string)
        .collect()
}

/// 值 id →（provider, model）：对已知 provider 名做**最长前缀**匹配。
///
/// - 命中（`"{p}:{rest}"`，`rest` 非空）→ `provider = Some(p)`、`model = rest`
///   （`model` 自身可以含 `:`，只剥一层前缀）；
/// - 未命中 → 整串当模型名、`provider = None`（未知前缀不猜——它可能就是某个含 `:` 的
///   模型名，例如 `qwen2.5:7b`）；
/// - 空串 / 纯空白 / 以分隔符结尾（`openai:` = 空模型名）→ `None`（无效值）。
pub fn parse_value_id(raw: &str, providers: &[String]) -> Option<ModelSelection> {
    let raw = raw.trim();
    // 空值，或空模型名（值以分隔符结尾，如 `openai:` / `a:b:`）——都不是合法值：
    // 前半段像是 provider、后半段却是空的，只有客户端拼错才会这样。
    if raw.is_empty() || raw.ends_with(VALUE_SEPARATOR) {
        return None;
    }
    let mut longest: Option<(&str, &str)> = None;
    for provider in providers {
        let Some(name) = non_blank(Some(provider.as_str())) else {
            continue;
        };
        let Some(rest) = raw
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(VALUE_SEPARATOR))
            .filter(|rest| !rest.trim().is_empty())
        else {
            continue;
        };
        if longest.is_none_or(|(previous, _)| name.len() > previous.len()) {
            longest = Some((name, rest.trim()));
        }
    }
    Some(match longest {
        Some((provider, model)) => ModelSelection {
            provider: Some(provider.to_string()),
            model: model.to_string(),
        },
        None => ModelSelection {
            provider: None,
            model: raw.to_string(),
        },
    })
}

/// 裸模型名的 provider 归属（只在 `parse_value_id` 给出 `provider = None` 时调用）。
///
/// 1. 当前 provider 的模型表里有它 → 当前 provider（同 provider 换名最贴近直觉，
///    也避免同名模型跨 provider 时被目录顺序带走）；
/// 2. 否则目录里第一个列出它的 provider（目录序 = 确定性）；
/// 3. 否则回落当前 provider——provider 的静态模型表**不是权威**（会话可以跑在表外模型上）；
/// 4. 都没有 → `None`（调用方报 invalid params：网关要求 model / provider 成对下发）。
pub fn resolve_provider(
    model: &str,
    catalog: &ModelsResponse,
    current: Option<&str>,
) -> Option<String> {
    let current = non_blank(current);
    if let Some(current) = current
        && catalog.providers.iter().any(|provider| {
            provider_name(provider) == Some(current) && lists_model(provider, model)
        })
    {
        return Some(current.to_string());
    }
    if let Some(found) = catalog
        .providers
        .iter()
        .find(|provider| provider_name(provider).is_some() && lists_model(provider, model))
    {
        return provider_name(found).map(str::to_string);
    }
    current.map(str::to_string)
}

/// `GET /api/models` 的目录 + 会话当前状态 → `model` option（空 vec = 没有可广告的值）。
///
/// 规则（design D4）：
///
/// - 每个 provider 一组（`group` = 组名 = provider 名，目录顺序）；provider 名空 / 模型表空
///   的组丢掉；组内空白项与重复值 id 丢掉；
/// - 显示名 `label_for`（声明的展示名，缺省回落调用名）；`description` 只在声明且非空时带；
/// - `currentValue` 见 [`SessionModel::current_value_id`]；
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
        for model in &provider.models {
            let Some(model) = non_blank(Some(model.as_str())) else {
                continue;
            };
            let value = format!("{name}{VALUE_SEPARATOR}{model}");
            if seen.contains(&value) {
                tracing::debug!(
                    value,
                    "acp: duplicate model value id in the catalog; skipped"
                );
                continue;
            }
            seen.push(value.clone());
            let mut option = SessionConfigSelectOption::new(value, provider.label_for(model));
            if let Some(description) = description_of(provider, model) {
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
        // provider 缺席：补进哪个组都是误导（值 id 也不带前缀），只记日志。
        None => tracing::debug!(
            model = %current.model,
            "acp: current model has no provider; not appended to the option values",
        ),
    }
}

/// 事件是否是「模型变更」（中继判据）：`session_state_changed{model: Some(非空)}`。
///
/// 其它字段（title / thinking / yolo / …）不在本步映射范围。
pub fn is_model_change(event: &WingEvent) -> bool {
    matches!(
        event,
        WingEvent::SessionStateChanged { model: Some(model), .. } if !model.trim().is_empty()
    )
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

/// `session/set_config_option` 的全流程：校验 → 解析 → 更新网关 → 全量 options。
pub async fn set_config_option(
    hub: &SessionHub,
    request: &SetSessionConfigOptionRequest,
) -> Result<Vec<SessionConfigOption>, Error> {
    let session_id = request.session_id.to_string();
    if !hub.knows(&session_id) {
        return Err(Error::invalid_params().data(format!("unknown session: {session_id}")));
    }
    let raw = check_request(request.config_id.0.as_ref(), &request.value)?;

    // 值的解析与回执都要目录；拿不到 / 目录为空一律报错——把 `provider:model` 误当裸模型名
    // 会把整串（含前缀）下发给网关，静默选错模型比报错糟得多。
    let catalog = fetch_catalog(hub).await?;
    let providers = provider_names(&catalog);
    if providers.is_empty() {
        return Err(Error::internal_error().data(
            "the gateway lists no model providers; refusing to guess what a value id means",
        ));
    }
    let Some(selection) = parse_value_id(raw, &providers) else {
        return Err(Error::invalid_params().data("empty model value"));
    };
    let provider = match selection.provider {
        Some(provider) => provider,
        None => {
            let current = fetch_state(hub, &session_id).await?;
            resolve_provider(&selection.model, &catalog, current.provider.as_deref()).ok_or_else(
                || {
                    Error::invalid_params().data(format!(
                        "cannot resolve a provider for model '{}'",
                        selection.model
                    ))
                },
            )?
        }
    };

    let update = UpdateSessionRequest {
        session_id: session_id.clone(),
        model: Some(selection.model.clone()),
        provider: Some(provider.clone()),
        ..Default::default()
    };
    hub.http
        .update_session(&update)
        .await
        .map_err(update_error)?;
    tracing::info!(
        session_id,
        model = %selection.model,
        provider = %provider,
        "acp: model switched"
    );

    // 成功 → 全量 options。**重新取**会话状态：网关是权威，不拿请求参数当结果
    // （网关可能被别的更新覆盖；随后还有一次 `session_state_changed` 中继兜底）。
    //
    // 重取失败**不报错**：切换已经生效（上面的 update 成功了），此刻回 JSON-RPC error 会让
    // 客户端以为没切成——omnigent 还会据此把「本进程不支持模型切换」永久关掉。回显请求的
    // 值（下一步的中继会纠正成权威值）+ warn 是这里唯一不撒谎的收口。
    let current = match fetch_state(hub, &session_id).await {
        Ok(current) => current,
        Err(err) => {
            tracing::warn!(
                session_id,
                error = %err,
                "acp: could not re-read the session state after the switch; echoing the requested value"
            );
            SessionModel {
                provider: Some(provider.clone()),
                model: selection.model.clone(),
                display_name: None,
            }
        }
    };
    Ok(build_options(&catalog, &current))
}

/// 模型变更中继（`session_state_changed{model}`）：权威状态 → 全量 options → 通知。
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

/// 请求头的校验（不含要 HTTP 的步骤）：config id 必须是我们广告的 `model`，值必须是值 id。
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
    value
        .as_value_id()
        .map(|value| value.0.as_ref())
        .ok_or_else(|| {
            Error::invalid_params().data(format!(
                "config option '{CONFIG_ID}' is a select; a boolean value is not accepted"
            ))
        })
}

/// `GET /api/models` → 目录。
async fn fetch_catalog(hub: &SessionHub) -> Result<ModelsResponse, Error> {
    hub.http.get_models().await.map_err(|err| {
        tracing::warn!(error = %err, "acp: /api/models failed");
        Error::internal_error().data(format!("the gateway did not list its models: {err}"))
    })
}

/// `GET /api/session/get` → 会话当前的 (provider, model, 展示名)。
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
        provider: agent.provider_name,
        model: agent.model_name,
        display_name: agent.model_display_name,
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
/// 4xx = 客户端给的值有问题（未知 provider / 会话不存在）→ invalid params；5xx 与传输层
/// → internal error。错误文本一律带上，别让「为什么不生效」变成黑盒。
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

/// provider 的模型表里是否有该模型（逐字比对，两侧 trim）。
fn lists_model(provider: &ProviderModels, model: &str) -> bool {
    let model = model.trim();
    !model.is_empty()
        && provider
            .models
            .iter()
            .any(|candidate| candidate.trim() == model)
}

/// 模型声明的描述（空白 / 未声明 → None）。
fn description_of<'a>(provider: &'a ProviderModels, model: &str) -> Option<&'a str> {
    non_blank(
        provider
            .detail_for(model)
            .and_then(|detail| detail.description.as_deref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ErrorCode;
    use agent_client_protocol::schema::v1::SessionConfigKind;
    use serde_json::json;

    /// 目录 fixture（走 JSON，与 `/api/models` 同一形状）。
    fn catalog() -> ModelsResponse {
        serde_json::from_value(json!({
            "providers": [
                {
                    "provider": "dashscope",
                    "models": ["glm-4.6", "qwen3-max"],
                    "model_details": [{
                        "name": "glm-4.6",
                        "display_name": "GLM-4.6",
                        "description": "智谱旗舰"
                    }]
                },
                {
                    "provider": "openai",
                    "models": ["gpt-5"],
                    "model_details": [{"name": "gpt-5", "display_name": "GPT-5"}]
                }
            ]
        }))
        .expect("catalog fixture decodes")
    }

    /// 同名模型跨 provider 的目录（归属解析用）。
    fn shared_catalog() -> ModelsResponse {
        serde_json::from_value(json!({
            "providers": [
                {"provider": "alpha", "models": ["shared", "alpha-only"]},
                {"provider": "beta", "models": ["shared", "beta-only"]}
            ]
        }))
        .expect("shared catalog fixture decodes")
    }

    fn providers(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn current(provider: &str, model: &str, display_name: Option<&str>) -> SessionModel {
        SessionModel {
            provider: Some(provider.to_string()),
            model: model.to_string(),
            display_name: display_name.map(str::to_string),
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

    // ---- parse_value_id ----

    #[test]
    fn parse_value_id_matches_the_longest_known_provider_prefix() {
        let known = providers(&["openai", "dashscope"]);
        assert_eq!(
            parse_value_id("openai:gpt-5", &known),
            Some(ModelSelection {
                provider: Some("openai".into()),
                model: "gpt-5".into(),
            })
        );
        assert_eq!(
            parse_value_id("dashscope:qwen3-max", &known),
            Some(ModelSelection {
                provider: Some("dashscope".into()),
                model: "qwen3-max".into(),
            })
        );

        // 最长前缀：`a` 与 `a:b` 都在列表里时，`a:b:c` 归 `a:b`（与列表顺序无关）。
        for order in [vec!["a", "a:b"], vec!["a:b", "a"]] {
            assert_eq!(
                parse_value_id("a:b:c", &providers(&order)),
                Some(ModelSelection {
                    provider: Some("a:b".into()),
                    model: "c".into(),
                }),
                "providers = {order:?}"
            );
        }
        // 只认识 `a` 时按 `a` 切，剩下的整串（含 `:`）是模型名。
        assert_eq!(
            parse_value_id("a:b:c", &providers(&["a"])),
            Some(ModelSelection {
                provider: Some("a".into()),
                model: "b:c".into(),
            })
        );
    }

    #[test]
    fn parse_value_id_keeps_colons_inside_the_model_name() {
        // Ollama 风格的模型名自带 `:`：只剥一层前缀。
        assert_eq!(
            parse_value_id("ollama:qwen2.5:7b", &providers(&["ollama"])),
            Some(ModelSelection {
                provider: Some("ollama".into()),
                model: "qwen2.5:7b".into(),
            })
        );
        // 两侧空白按 trim 处理（目录里的名字也是 trim 后比对的）。
        assert_eq!(
            parse_value_id("  ollama:qwen2.5:7b ", &providers(&[" ollama "])),
            Some(ModelSelection {
                provider: Some("ollama".into()),
                model: "qwen2.5:7b".into(),
            })
        );
    }

    #[test]
    fn parse_value_id_falls_back_to_a_bare_model_name() {
        // 裸模型名（omnigent 的 `/model` 形状）：不认识 → 整串当模型名。
        assert_eq!(
            parse_value_id("gpt-5", &providers(&["openai"])),
            Some(ModelSelection {
                provider: None,
                model: "gpt-5".into(),
            })
        );
        // 未知前缀：整串当模型名（不猜 provider，也不丢前缀）。
        assert_eq!(
            parse_value_id("foo:bar", &providers(&["openai"])),
            Some(ModelSelection {
                provider: None,
                model: "foo:bar".into(),
            })
        );
        // 目录为空 / provider 名空白：同样退化成裸模型名。
        assert_eq!(
            parse_value_id("openai:gpt-5", &[]),
            Some(ModelSelection {
                provider: None,
                model: "openai:gpt-5".into(),
            })
        );
        assert_eq!(
            parse_value_id("openai:gpt-5", &providers(&["  "])),
            Some(ModelSelection {
                provider: None,
                model: "openai:gpt-5".into(),
            })
        );
    }

    #[test]
    fn parse_value_id_rejects_blank_values_and_empty_models() {
        assert!(parse_value_id("", &providers(&["openai"])).is_none());
        assert!(parse_value_id("   ", &providers(&["openai"])).is_none());
        // `provider:`（空模型名）不是合法值——不把它当成裸模型名 `openai:`。
        assert!(parse_value_id("openai:", &providers(&["openai"])).is_none());
        assert!(parse_value_id("openai:   ", &providers(&["openai"])).is_none());
        // 不认识的前缀同样按「空模型名」拒绝（`a:b:`）。
        assert!(parse_value_id("foo:bar:", &providers(&["openai"])).is_none());
    }

    // ---- currentValue 往返 ----

    #[test]
    fn current_value_id_round_trips_through_the_parser() {
        let known = providers(&["dashscope"]);
        let value = current("dashscope", "glm-4.6", Some("GLM-4.6"));
        assert_eq!(value.current_value_id(), "dashscope:glm-4.6");
        let parsed =
            parse_value_id(&value.current_value_id(), &known).expect("advertised value parses");
        assert_eq!(
            parsed,
            ModelSelection {
                provider: Some("dashscope".into()),
                model: "glm-4.6".into(),
            }
        );

        // provider 缺席（旧网关）→ 裸模型名：不编前缀，解析回来仍是（None, model）。
        let anonymous = SessionModel {
            provider: None,
            model: "glm-4.6".into(),
            display_name: None,
        };
        assert_eq!(anonymous.current_value_id(), "glm-4.6");
        assert_eq!(
            parse_value_id("glm-4.6", &known).expect("bare value parses"),
            ModelSelection {
                provider: None,
                model: "glm-4.6".into(),
            }
        );

        // 空白 provider 视同缺席（不产出 `:model` 这种畸形值）。
        let blank = SessionModel {
            provider: Some("   ".into()),
            model: "glm-4.6".into(),
            display_name: None,
        };
        assert_eq!(blank.current_value_id(), "glm-4.6");
    }

    #[test]
    fn every_advertised_value_parses_back_to_its_provider_and_model() {
        let catalog = catalog();
        let options = build_options(&catalog, &current("dashscope", "glm-4.6", None));
        let known = provider_names(&catalog);
        for group in groups_of(&options) {
            for option in &group.options {
                let value = option.value.0.to_string();
                let parsed = parse_value_id(&value, &known)
                    .unwrap_or_else(|| panic!("advertised value {value} must parse"));
                assert_eq!(parsed.provider.as_deref(), Some(group.group.0.as_ref()));
                assert_eq!(
                    format!("{}:{}", parsed.provider.as_deref().unwrap(), parsed.model),
                    value
                );
            }
        }
    }

    // ---- resolve_provider ----

    #[test]
    fn resolve_provider_prefers_the_current_provider_then_the_catalog_order() {
        let catalog = shared_catalog();
        // 同名模型跨 provider：当前 provider 优先（两个方向都成立）。
        assert_eq!(
            resolve_provider("shared", &catalog, Some("beta")).as_deref(),
            Some("beta")
        );
        assert_eq!(
            resolve_provider("shared", &catalog, Some("alpha")).as_deref(),
            Some("alpha")
        );
        // 当前 provider 的表里没有它 → 目录里第一个列出它的 provider。
        assert_eq!(
            resolve_provider("shared", &catalog, None).as_deref(),
            Some("alpha")
        );
        assert_eq!(
            resolve_provider("beta-only", &catalog, Some("alpha")).as_deref(),
            Some("beta")
        );
        // 目录里没有的模型名：回落当前 provider（静态表不是权威）。
        assert_eq!(
            resolve_provider("table-outside", &catalog, Some("alpha")).as_deref(),
            Some("alpha")
        );
        // 连当前 provider 都不知道 → None（调用方报 invalid params）。
        assert_eq!(resolve_provider("table-outside", &catalog, None), None);
        // 空白 provider 名视同缺席。
        assert_eq!(
            resolve_provider("table-outside", &catalog, Some("  ")),
            None
        );
    }

    // ---- build_options ----

    #[test]
    fn build_options_groups_by_provider_with_display_name_fallback() {
        let options = build_options(
            &catalog(),
            &current("dashscope", "glm-4.6", Some("GLM-4.6")),
        );
        let select = select_of(&options);
        assert_eq!(select.current_value.0.as_ref(), "dashscope:glm-4.6");

        let groups = groups_of(&options);
        assert_eq!(groups.len(), 2, "每个 provider 一组");
        assert_eq!(groups[0].group.0.as_ref(), "dashscope");
        assert_eq!(groups[0].name, "dashscope", "组名 = provider 名");
        assert_eq!(
            values(&groups[0]),
            ["dashscope:glm-4.6", "dashscope:qwen3-max"]
        );
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
        assert_eq!(values(&groups[1]), ["openai:gpt-5"]);
        assert_eq!(groups[1].options[0].name, "GPT-5");
    }

    #[test]
    fn build_options_appends_an_out_of_catalog_current_model() {
        // 表外模型（`--model foo` 覆盖 / 模板声明）：补进它自己 provider 的组，
        // 值列表里因此总有 currentValue（客户端才不会显示 Unknown）。
        let options = build_options(
            &catalog(),
            &current("dashscope", "glm-4.6-x", Some("GLM-4.6-X")),
        );
        let select = select_of(&options);
        assert_eq!(select.current_value.0.as_ref(), "dashscope:glm-4.6-x");
        let groups = groups_of(&options);
        assert_eq!(groups.len(), 2, "自补不新建组（该 provider 已在目录里）");
        assert_eq!(
            values(&groups[0]),
            [
                "dashscope:glm-4.6",
                "dashscope:qwen3-max",
                "dashscope:glm-4.6-x"
            ]
        );
        assert_eq!(groups[0].options[2].name, "GLM-4.6-X", "展示名优先");
        assert_eq!(groups[0].options[2].description, None);

        // provider 不在目录里 → 末尾新建一组。
        let options = build_options(&catalog(), &current("newprov", "m1", None));
        let groups = groups_of(&options);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[2].group.0.as_ref(), "newprov");
        assert_eq!(values(&groups[2]), ["newprov:m1"]);
        assert_eq!(groups[2].options[0].name, "m1", "无展示名 → 回落模型名");

        // provider 未知：value id 是裸模型名，补进任何组都是误导 → 不补（已知限制 6）。
        let options = build_options(
            &catalog(),
            &SessionModel {
                provider: None,
                model: "m2".into(),
                display_name: None,
            },
        );
        assert_eq!(select_of(&options).current_value.0.as_ref(), "m2");
        for group in groups_of(&options) {
            assert!(!values(group).contains(&"m2".to_string()));
        }
    }

    #[test]
    fn build_options_is_empty_without_usable_values() {
        // 空目录 + 没有当前模型 → 空 vec（调用方不广告 configOptions）。
        let empty: ModelsResponse =
            serde_json::from_value(json!({"providers": []})).expect("empty catalog decodes");
        assert!(build_options(&empty, &SessionModel::default()).is_empty());

        // provider 名空白 / 模型表全空白 → 组丢掉，同样不广告。
        let blank: ModelsResponse = serde_json::from_value(json!({
            "providers": [
                {"provider": "  ", "models": ["a"]},
                {"provider": "p", "models": ["", "   "]},
            ]
        }))
        .expect("blank catalog decodes");
        assert!(build_options(&blank, &SessionModel::default()).is_empty());

        // 目录里有重复值 id（同一模型列两次）→ 去重，不产出重复项。
        let dup: ModelsResponse = serde_json::from_value(json!({
            "providers": [{"provider": "p", "models": ["m", " m ", "n"]}]
        }))
        .expect("dup catalog decodes");
        let options = build_options(&dup, &SessionModel::default());
        assert_eq!(values(&groups_of(&options)[0]), ["p:m", "p:n"]);
    }

    #[test]
    fn build_options_matches_the_design_frame_sample() {
        // design.md「Frame samples ①」的逐键对账：文档与代码不许漂移。
        let options = build_options(
            &catalog(),
            &current("dashscope", "glm-4.6", Some("GLM-4.6")),
        );
        let value = serde_json::to_value(&options[0]).expect("option serializes");
        assert_eq!(
            value,
            json!({
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "dashscope:glm-4.6",
                "options": [
                    {"group": "dashscope", "name": "dashscope", "options": [
                        {"value": "dashscope:glm-4.6", "name": "GLM-4.6", "description": "智谱旗舰"},
                        {"value": "dashscope:qwen3-max", "name": "qwen3-max"}
                    ]},
                    {"group": "openai", "name": "openai", "options": [
                        {"value": "openai:gpt-5", "name": "GPT-5"}
                    ]}
                ]
            })
        );
    }

    // ---- 中继映射 ----

    #[test]
    fn is_model_change_only_fires_for_a_concrete_new_model() {
        assert!(is_model_change(&event(json!({
            "type": "session_state_changed",
            "model": "qwen3-max",
            "model_display_name": "Qwen3 Max",
            "created_at": "2026-01-01T00:00:00+00:00",
            "session_id": "s1",
            "request_id": "req-1",
        }))));

        // 只改了标题 / thinking / yolo：本步不映射（标题由 translate 的既有映射负责）。
        for other in [
            json!({"title": "新标题"}),
            json!({"thinking": true}),
            json!({"yolo": true}),
            json!({"model": null}),
            json!({"model": "   "}),
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

        let options = build_options(&catalog(), &current("dashscope", "qwen3-max", None));
        let update = config_option_update(options.clone()).expect("options → frame");
        let SessionUpdate::ConfigOptionUpdate(update) = update else {
            panic!("expected config_option_update");
        };
        assert_eq!(update.config_options, options);

        // 线上形状（design「Frame samples ③」）：session/update 信封 + 全量 options。
        let notification = SessionNotification::new(
            SessionId::new("s1"),
            SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options)),
        );
        let frame = serde_json::to_value(&notification).expect("notification serializes");
        assert_eq!(frame["sessionId"], "s1");
        assert_eq!(frame["update"]["sessionUpdate"], "config_option_update");
        assert_eq!(
            frame["update"]["configOptions"][0]["currentValue"],
            "dashscope:qwen3-max"
        );
        assert_eq!(
            frame["update"]["configOptions"][0]["options"][0]["group"],
            "dashscope"
        );
    }

    // ---- 请求解码 / 校验 ----

    #[test]
    fn set_config_option_accepts_the_value_id_wire_shape() {
        // Zed：值 id 形状（无 `type`）；omnigent：同一个形状，值是裸模型名。
        for value in ["dashscope:glm-4.6", "glm-4.6"] {
            let request: SetSessionConfigOptionRequest = serde_json::from_value(json!({
                "sessionId": "s1",
                "configId": "model",
                "value": value,
            }))
            .expect("request fixture decodes");
            assert_eq!(
                check_request(request.config_id.0.as_ref(), &request.value)
                    .expect("value id is accepted"),
                value
            );
        }
    }

    #[test]
    fn set_config_option_rejects_unknown_ids_and_boolean_values() {
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
            detail: "provider 'nope' not found".into(),
            body: None,
        };
        let error = update_error(bad_request);
        assert_eq!(error.code, ErrorCode::InvalidParams);
        assert!(
            error.to_string().contains("provider 'nope' not found"),
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
