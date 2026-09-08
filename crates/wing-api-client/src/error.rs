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

    /// WS 连接或握手错误（tool host 使用）。
    #[error("connection error: {0}")]
    Connection(String),
}

impl ApiClientError {
    /// 服务端明确回答"资源不存在"（404）。
    ///
    /// 与网络错误或 5xx 不同，404 不会因重试而好转——调用方应停止重试
    /// 并采取恢复动作（如为丢失的 session 新建一个）。
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Api { status: 404, .. })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn api_err(status: u16) -> ApiClientError {
        ApiClientError::Api {
            status,
            detail: "detail".to_string(),
            body: None,
        }
    }

    #[test]
    fn is_not_found_matches_404_only() {
        assert!(api_err(404).is_not_found());
        assert!(!api_err(400).is_not_found());
        assert!(!api_err(500).is_not_found());
        // 非服务端应答的错误（网络层/连接层）同样不是 404。
        assert!(!ApiClientError::Connection("reset".into()).is_not_found());
    }
}
