//! API client error types.

use crate::models::ErrorResponse;

/// Gateway HTTP client 的所有错误类型。
#[derive(Debug, thiserror::Error)]
pub enum ApiClientError {
    /// HTTP 请求失败（网络错误、超时等）。
    #[error("HTTP request failed: {0}")]
    Transport(#[from] reqwest::Error),

    /// 服务端返回了错误响应（4xx / 5xx）。
    #[error("API error ({status}): {detail}")]
    Api {
        status: u16,
        detail: String,
        /// 如果服务端返回了结构化的 ErrorResponse，保存在这里。
        body: Option<ErrorResponse>,
    },

    /// 响应体反序列化失败。
    #[error("failed to deserialize response: {0}")]
    Deserialize(#[from] serde_json::Error),

    /// API key 含非法 HTTP header 字符。
    #[error("invalid API key for HTTP header: {0}")]
    InvalidApiKey(#[from] reqwest::header::InvalidHeaderValue),
}

/// 从 reqwest::Response 中提取 ApiClientError::Api。
pub(crate) async fn extract_api_error(resp: reqwest::Response) -> ApiClientError {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();

    // 尝试反序列化为结构化的 ErrorResponse
    let body: Option<ErrorResponse> = serde_json::from_str(&text).ok();

    let detail = body
        .as_ref()
        .and_then(|e| e.detail.as_deref())
        .unwrap_or(&text)
        .to_string();

    ApiClientError::Api {
        status,
        detail,
        body,
    }
}
