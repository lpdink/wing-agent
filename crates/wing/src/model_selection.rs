//! 模型选择的共享策略：把客户端给的**裸模型名**解析成它的 provider 归属。
//!
//! 两个前端在同一个问题上相遇——"这个模型名该由哪个 provider 接"：
//!
//! - `acp/model.rs`：客户端（Zed / omnigent）提交的 value id 可能是裸模型名，
//!   而 `POST /api/session/update` 要求 model / provider **成对**下发；
//! - `stdio/stdin_handler.rs`：Claude 协议的 `set_model` 控制请求只给一个
//!   裸模型名（T3 Code 的 customModels 值），同样要补全 provider。
//!
//! 规则见 [`resolve_provider`]（design D2："已知集合"与归属解析）。

use wing_api_client::models::{ModelsResponse, ProviderModels};

/// 裸模型名的 provider 归属（只在值 id 没有 provider 前缀时使用）。
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

/// 空白视同缺席（`None` 或纯空白都算没有）。
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shared_catalog() -> ModelsResponse {
        serde_json::from_value(json!({
            "providers": [
                {"provider": "alpha", "models": ["shared", "alpha-only"]},
                {"provider": "beta", "models": ["shared", "beta-only"]}
            ]
        }))
        .expect("shared catalog fixture decodes")
    }

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
}
