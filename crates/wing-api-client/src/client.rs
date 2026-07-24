//! [`GatewayClient`] — Wing Gateway 的 HTTP API 客户端。

use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap};

use crate::error::{ApiClientError, extract_api_error};
use crate::models::*;

/// Gateway 默认端口。
pub const DEFAULT_PORT: u16 = 32523;

/// Wing Gateway HTTP API 客户端。
///
/// # Example
///
/// ```no_run
/// # use wing_api_client::models::CreateSessionRequest;
/// # use wing_api_client::GatewayClient;
/// # async fn example() -> Result<(), wing_api_client::ApiClientError> {
/// let client = GatewayClient::new("http://127.0.0.1:32523", None)?;
///
/// // 创建 session
/// let req = CreateSessionRequest::default();
/// let session = client.create_session(&req).await?;
/// println!("created session: {}", session.session_id);
///
/// // 健康检查
/// let health = client.health().await?;
/// println!("status: {}, version: {}", health.status, health.version);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct GatewayClient {
    http: Client,
    base_url: String,
}

impl GatewayClient {
    /// 创建新的 client。
    ///
    /// `base_url` 通常是 `"http://127.0.0.1:32523"`。
    /// `api_key` 为 `Some` 且非空时，所有请求自动携带
    /// `Authorization: Bearer <key>` header。
    pub fn new(base_url: impl Into<String>, api_key: Option<&str>) -> Result<Self, ApiClientError> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .default_headers(build_auth_headers(api_key)?)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into(),
        })
    }

    /// 使用默认地址 (`http://127.0.0.1:{DEFAULT_PORT}`) 创建 client。
    pub fn localhost(api_key: Option<&str>) -> Result<Self, ApiClientError> {
        Self::new(format!("http://127.0.0.1:{DEFAULT_PORT}"), api_key)
    }

    // ============================================================
    // Session 生命周期
    // ============================================================

    /// 创建新 session。
    ///
    /// 使用 [`CreateSessionRequest`] 构建请求体，支持 template_name、
    /// workspace 和 agent override 参数。
    pub async fn create_session(
        &self,
        req: &CreateSessionRequest,
    ) -> Result<CreateSessionResponse, ApiClientError> {
        self.post_json("/api/session/create", req).await
    }

    /// 从磁盘恢复已有 session。
    pub async fn resume_session(
        &self,
        session_id: &str,
    ) -> Result<ResumeSessionResponse, ApiClientError> {
        let body = ResumeSessionRequest {
            session_id: session_id.to_owned(),
        };
        self.post_json("/api/session/resume", &body).await
    }

    /// 从指定 session 的指定消息处分叉出新 session。
    pub async fn fork_session(
        &self,
        source_session_id: &str,
        target_uuid: &str,
    ) -> Result<ForkSessionResponse, ApiClientError> {
        let body = ForkSessionRequest {
            source_session_id: source_session_id.to_owned(),
            target_uuid: target_uuid.to_owned(),
        };
        self.post_json("/api/session/fork", &body).await
    }

    // ============================================================
    // 订阅管理
    // ============================================================

    /// 订阅 session 事件。`client_id` 来自 WS 连接。
    pub async fn subscribe(&self, session_id: &str, client_id: &str) -> Result<(), ApiClientError> {
        let body = SubscribeRequest {
            session_id: session_id.to_owned(),
        };
        self.post_json_with_client_id("/api/session/subscribe", &body, client_id)
            .await
    }

    /// 取消订阅 session 事件。
    pub async fn unsubscribe(
        &self,
        session_id: &str,
        client_id: &str,
    ) -> Result<(), ApiClientError> {
        let body = UnsubscribeRequest {
            session_id: session_id.to_owned(),
        };
        self.post_json_with_client_id("/api/session/unsubscribe", &body, client_id)
            .await
    }

    // ============================================================
    // 消息发送
    // ============================================================

    /// 向活跃 session 发送消息。
    ///
    /// `tool_call_id` — 回复 Ask 事件时传入其 tool_call_id，
    /// 定向 resolve 对应的 feedback waiter。
    pub async fn send_message(
        &self,
        session_id: &str,
        content: &str,
        tool_call_id: Option<String>,
    ) -> Result<SendMessageResponse, ApiClientError> {
        let body = SendMessageRequest {
            session_id: session_id.to_owned(),
            content: content.to_owned(),
            tool_call_id,
        };
        self.post_json("/api/session/send", &body).await
    }

    // ============================================================
    // 查询
    // ============================================================

    /// 列出所有活跃 session。
    pub async fn list_sessions(&self) -> Result<SessionListResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/session/list"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// 获取指定 session 的完整状态。
    pub async fn get_session(
        &self,
        session_id: &str,
    ) -> Result<SessionGetResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/session/get"))
            .query(&[("session_id", session_id)])
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    // ============================================================
    // Session 查询
    // ============================================================

    /// 获取 session 的运行时状态：模型、工具、token 用量等。
    pub async fn get_session_info(
        &self,
        session_id: &str,
    ) -> Result<SessionInfoResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/session/info"))
            .query(&[("session_id", session_id)])
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// 获取 session 的可回退/分叉消息节点列表。
    pub async fn get_branches(&self, session_id: &str) -> Result<BranchesResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/session/branches"))
            .query(&[("session_id", session_id)])
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    // ============================================================
    // Session 更新
    // ============================================================

    /// 更新 session 状态（模型切换、agent 切换、标题设置、thinking/yolo 开关）。
    pub async fn update_session(
        &self,
        req: &UpdateSessionRequest,
    ) -> Result<UpdateSessionResponse, ApiClientError> {
        self.post_json("/api/session/update", req).await
    }

    // ============================================================
    // Session 操作
    // ============================================================

    /// 压缩 session 上下文。
    pub async fn compact_session(
        &self,
        session_id: &str,
    ) -> Result<CompactResponse, ApiClientError> {
        let body = CompactRequest {
            session_id: session_id.to_owned(),
        };
        self.post_json("/api/session/compact", &body).await
    }

    /// 中断 session 当前任务。
    pub async fn interrupt_session(&self, session_id: &str) -> Result<OkResponse, ApiClientError> {
        let body = InterruptRequest {
            session_id: session_id.to_owned(),
        };
        self.post_json("/api/session/interrupt", &body).await
    }

    /// 回退 session 到指定消息节点。
    pub async fn rewind_session(
        &self,
        session_id: &str,
        target_uuid: &str,
    ) -> Result<RewindResponse, ApiClientError> {
        let body = RewindRequest {
            session_id: session_id.to_owned(),
            target_uuid: target_uuid.to_owned(),
        };
        self.post_json("/api/session/rewind", &body).await
    }

    // ============================================================
    // 系统级操作
    // ============================================================

    /// 热重载全局配置。
    pub async fn reload_system(&self) -> Result<ReloadResponse, ApiClientError> {
        self.post_empty("/api/system/reload").await
    }

    // ============================================================
    // 系统级查询
    // ============================================================

    /// 获取可用命令列表。
    pub async fn get_commands(&self) -> Result<CommandsResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/commands"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// 获取可用模型列表。
    pub async fn get_models(&self) -> Result<ModelsResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/models"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// 获取可用 agent 模板列表。
    pub async fn get_agents(&self) -> Result<AgentsResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/agents"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    // ============================================================
    // Health
    // ============================================================

    /// 健康检查。
    pub async fn health(&self) -> Result<HealthResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/health"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// 优雅关闭 Gateway。
    pub async fn shutdown(&self) -> Result<(), ApiClientError> {
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, "/api/shutdown"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(())
    }

    // ============================================================
    // 内部 helper
    // ============================================================

    /// POST without body → deserialize JSON response。
    async fn post_empty<Resp: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Resp, ApiClientError> {
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// POST JSON body → deserialize JSON response。
    async fn post_json<Req: serde::Serialize, Resp: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &Req,
    ) -> Result<Resp, ApiClientError> {
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .json(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// POST JSON body with X-Client-Id header → discard response body。
    async fn post_json_with_client_id<Req: serde::Serialize>(
        &self,
        path: &str,
        body: &Req,
        client_id: &str,
    ) -> Result<(), ApiClientError> {
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .header("X-Client-Id", client_id)
            .json(body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(())
    }
}

/// Build default headers with optional API key authentication.
///
/// Returns a `HeaderMap` containing `Authorization: Bearer <key>`
/// when `api_key` is `Some` and non-empty; otherwise an empty map.
fn build_auth_headers(api_key: Option<&str>) -> Result<HeaderMap, ApiClientError> {
    let mut headers = HeaderMap::new();
    if let Some(key) = api_key
        && !key.is_empty()
    {
        let value = format!("Bearer {key}").parse()?;
        headers.insert(AUTHORIZATION, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_auth_headers_with_key() {
        let headers = build_auth_headers(Some("my-secret")).unwrap();
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer my-secret"
        );
    }

    #[test]
    fn test_build_auth_headers_none() {
        let headers = build_auth_headers(None).unwrap();
        assert!(headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn test_build_auth_headers_empty() {
        let headers = build_auth_headers(Some("")).unwrap();
        assert!(headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn test_build_auth_headers_invalid_key() {
        // Control characters are invalid in HTTP header values.
        let result = build_auth_headers(Some("bad\nkey"));
        assert!(result.is_err());
    }

    #[test]
    fn test_client_new_with_key() {
        let client = GatewayClient::new("http://127.0.0.1:32523", Some("key"));
        assert!(client.is_ok());
    }

    #[test]
    fn test_client_new_without_key() {
        let client = GatewayClient::new("http://127.0.0.1:32523", None);
        assert!(client.is_ok());
    }

    #[test]
    fn test_client_localhost() {
        let client = GatewayClient::localhost(None);
        assert!(client.is_ok());
        assert_eq!(client.unwrap().base_url, "http://127.0.0.1:32523");
    }
}
