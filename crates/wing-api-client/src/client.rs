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
        self.resume_session_with_override(session_id, None).await
    }

    /// 从磁盘恢复已有 session，并应用 resume 覆盖（`AgentOverride` 子集）。
    ///
    /// 覆盖只应用 `model_id` / `effort` / `tools`——见
    /// [`ResumeSessionRequest`] 的字段说明。`agent` 为 `None` 时与
    /// [`Self::resume_session`] 等价（保留这个变体方法而不是给
    /// `resume_session` 加参数：既有调用点零改动，合并面更小）。
    pub async fn resume_session_with_override(
        &self,
        session_id: &str,
        agent: Option<&AgentOverride>,
    ) -> Result<ResumeSessionResponse, ApiClientError> {
        let body = ResumeSessionRequest {
            session_id: session_id.to_owned(),
            agent: agent.cloned(),
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

    /// 读取或原子增删 session 标签。
    ///
    /// `add` / `remove` 皆 `None` = 纯读取（返回当前标签，不做变更）；
    /// 两者同时给出时服务端一次原子应用（幂等）。不水合已逐出会话。
    pub async fn tag_session(
        &self,
        session_id: &str,
        add: Option<Vec<String>>,
        remove: Option<Vec<String>>,
    ) -> Result<TagSessionResponse, ApiClientError> {
        let body = TagSessionRequest {
            session_id: session_id.to_owned(),
            add,
            remove,
        };
        self.post_json("/api/session/tag", &body).await
    }

    // ============================================================
    // Session 操作
    // ============================================================

    /// 压缩 session 上下文。
    ///
    /// `instruction` 为可选的压缩侧重指令，附加到压缩 prompt；
    /// `None` 使用默认压缩策略。
    pub async fn compact_session(
        &self,
        session_id: &str,
        instruction: Option<&str>,
    ) -> Result<CompactResponse, ApiClientError> {
        let body = CompactRequest {
            session_id: session_id.to_owned(),
            instruction: instruction.map(str::to_owned),
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

    /// 逐出（release）session 内存态：只回收内存，磁盘状态不动。
    ///
    /// 忽略空闲时长（不为 TTL 等待），但不忽略钉住条件——忙碌 / 有待处理输入 /
    /// 被订阅 / 非持久后端的会话由网关以 409 拒绝（`ApiClientError` 带出原因）。
    pub async fn release_session(
        &self,
        session_id: &str,
    ) -> Result<ReleaseResponse, ApiClientError> {
        let body = ReleaseRequest {
            session_id: session_id.to_owned(),
        };
        self.post_json("/api/session/release", &body).await
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
    // Settings（配置的设置目录 / 读写 / 预检）
    // ============================================================

    /// 取设置目录（catalog 树）：全量字段声明 + 约束 + 生效域。
    ///
    /// 纯静态，可长缓存；setup mode 下同样可用（它是修复配置的依据）。
    pub async fn settings_schema(&self) -> Result<SettingsSchemaResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/settings/schema"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// 读当前配置：稀疏文档（密文叶子 = `null`）+ 指纹 + 密文状态 + 问题。
    ///
    /// **回传契约**：密文的 `null` 必须原样带回 [`Self::settings_set`]（`null` = 保留磁盘现值；
    /// 丢掉键 = 清空密钥）。前端在 `values` 上做编辑，保存时发整份。
    pub async fn settings_get(&self) -> Result<SettingsGetResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/settings/get"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// 极简健康判定（启动路径的预检：`valid=false` ⇒ 网关处于 setup mode，走修复流程）。
    pub async fn settings_status(&self) -> Result<SettingsStatusResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/settings/status"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
    }

    /// 保存配置（全文档替换 + 乐观并发指纹）。
    ///
    /// **校验失败也是 `Ok`**：服务端返回 HTTP 200 + `ok=false` + `problems`（design.md D16），
    /// 因为"用户填的内容不合法"是业务结果而不是请求非法。`Err` 只对应协议级失败：
    /// 409 指纹不匹配（[`ApiClientError::is_conflict`]）/ 鉴权 / 写盘失败 / 网络。
    pub async fn settings_set(
        &self,
        req: &SettingsSetRequest,
    ) -> Result<SettingsSetResponse, ApiClientError> {
        self.post_json("/api/settings/set", req).await
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

    /// 获取全局工具列表（含内置 + 远程）。
    pub async fn list_tools(&self) -> Result<ToolsListResponse, ApiClientError> {
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, "/api/tools"))
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
    // 远程工具注册
    // ============================================================

    /// 注册远程工具。`client_id` 经 X-Client-Id header 传递，指向已持有 WS 的 tool host。
    pub async fn register_tools(
        &self,
        client_id: &str,
        tools: Vec<RemoteToolSpec>,
    ) -> Result<RegisterToolsResponse, ApiClientError> {
        let body = RegisterToolsRequest { tools };
        let resp = self
            .http
            .post(format!("{}{}", self.base_url, "/api/tools/register"))
            .header("X-Client-Id", client_id)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(extract_api_error(resp).await);
        }
        Ok(resp.json().await?)
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

    // ============================================================
    // Settings 端点 — 进程内假网关（手写 HTTP/1.1 应答，零新依赖）
    // ============================================================
    //
    // L1 的真网关此刻还没实现这些端点，这是离线条件下唯一能验证
    // "4 个端点路径 / 方法 / 认证头 / 请求体写对了"的手段。

    /// 假网关收到的一条请求。
    #[derive(Debug, Clone)]
    struct Recorded {
        method: String,
        path: String,
        authorization: Option<String>,
        body: String,
    }

    /// 进程内假网关：`127.0.0.1:0` 临时端口，按路径回放预置的 `(status, body)`，记录全部请求。
    struct MockGateway {
        url: String,
        recorded: std::sync::Arc<std::sync::Mutex<Vec<Recorded>>>,
    }

    impl MockGateway {
        async fn start(routes: Vec<(&'static str, u16, &'static str)>) -> Self {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let recorded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = recorded.clone();

            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let routes = routes.clone();
                    let sink = sink.clone();
                    tokio::spawn(async move {
                        // 我们发出的请求都很小：读到空行拿到头，再按 Content-Length 收 body。
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 4096];
                        let head_end = loop {
                            let Ok(n) = stream.read(&mut chunk).await else {
                                return;
                            };
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                            if let Some(pos) = find_head_end(&buf) {
                                break pos;
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                        let body_start = head_end + 4;
                        let content_length = header_value(&head, "content-length")
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(0);
                        let body_end = body_start + content_length;
                        while buf.len() < body_end {
                            let Ok(n) = stream.read(&mut chunk).await else {
                                return;
                            };
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }

                        let mut lines = head.split("\r\n");
                        let request_line = lines.next().unwrap_or_default();
                        let mut parts = request_line.split(' ');
                        let method = parts.next().unwrap_or_default().to_string();
                        let path = parts.next().unwrap_or_default().to_string();
                        sink.lock().unwrap().push(Recorded {
                            method,
                            path: path.clone(),
                            authorization: header_value(&head, "authorization"),
                            body: String::from_utf8_lossy(
                                &buf[body_start..body_end.min(buf.len())],
                            )
                            .to_string(),
                        });

                        let (status, body) = routes
                            .iter()
                            .find(|(route, _, _)| *route == path.as_str())
                            .map(|(_, status, body)| (*status, *body))
                            .unwrap_or((404, r#"{"error":"not_found"}"#));
                        let response = format!(
                            "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\n\
                             content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            status_text(status),
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.flush().await;
                        let _ = stream.shutdown().await;
                    });
                }
            });

            Self { url, recorded }
        }

        fn recorded(&self) -> Vec<Recorded> {
            self.recorded.lock().unwrap().clone()
        }
    }

    fn find_head_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn header_value(head: &str, name: &str) -> Option<String> {
        head.split("\r\n").skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }

    fn status_text(status: u16) -> &'static str {
        match status {
            200 => "OK",
            409 => "Conflict",
            503 => "Service Unavailable",
            _ => "Error",
        }
    }

    const SCHEMA_BODY: &str = r#"{"version":"0.4.1","config_path":"/tmp/config.yaml",
        "root":{"key":"config","path":"","title":"Wing","doc":"d","kind":"object"}}"#;
    const GET_BODY: &str = r#"{"values":{"gateway":{"port":32523}},"secrets":{},
        "fingerprint":"sha256:abc","problems":[],"setup_mode":false,
        "config_path":"/tmp/config.yaml"}"#;
    const STATUS_BODY: &str = r#"{"valid":false,"setup_mode":true,
        "problems":[{"path":"providers","kind":"empty_list","message":"providers 不得为空"}],
        "fingerprint":null}"#;
    const SET_REFUSED_BODY: &str = r#"{"ok":false,"fingerprint":"sha256:abc",
        "problems":[{"path":"providers[0].api_key","kind":"missing_required",
        "message":"必填","hint":"在面板里填一个密钥"}],"changed":[],"restart_required":[]}"#;
    const SET_OK_BODY: &str = r#"{"ok":true,"fingerprint":"sha256:fff",
        "setup_mode_exited":true,"backup_path":"/tmp/config.yaml.bak"}"#;

    #[tokio::test]
    async fn settings_endpoints_hit_the_frozen_paths_and_bodies() {
        let server = MockGateway::start(vec![
            ("/api/settings/schema", 200, SCHEMA_BODY),
            ("/api/settings/get", 200, GET_BODY),
            ("/api/settings/status", 200, STATUS_BODY),
            ("/api/settings/set", 200, SET_REFUSED_BODY),
        ])
        .await;
        let client = GatewayClient::new(server.url.clone(), Some("secret-key")).unwrap();

        let schema = client.settings_schema().await.unwrap();
        assert_eq!(schema.version, "0.4.1");
        assert_eq!(schema.root.key, "config");

        let get = client.settings_get().await.unwrap();
        assert_eq!(get.fingerprint, "sha256:abc");
        assert_eq!(get.values["gateway"]["port"], serde_json::json!(32523));

        let status = client.settings_status().await.unwrap();
        assert!(!status.valid && status.setup_mode);
        assert_eq!(status.problems.len(), 1);

        let set = client
            .settings_set(&SettingsSetRequest {
                base: Some("sha256:abc".into()),
                document: serde_json::json!({"gateway": {"port": 32523}}),
            })
            .await
            .unwrap();
        // D16：校验失败是 HTTP 200 + ok=false + problems（不是 4xx）
        assert!(!set.ok);
        assert_eq!(
            set.problems[0].path.as_deref(),
            Some("providers[0].api_key")
        );

        let recorded = server.recorded();
        let seen: Vec<(&str, &str)> = recorded
            .iter()
            .map(|req| (req.method.as_str(), req.path.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("GET", "/api/settings/schema"),
                ("GET", "/api/settings/get"),
                ("GET", "/api/settings/status"),
                ("POST", "/api/settings/set"),
            ]
        );
        assert_eq!(
            recorded[3].body,
            r#"{"base":"sha256:abc","document":{"gateway":{"port":32523}}}"#
        );
        for req in &recorded {
            assert_eq!(req.authorization.as_deref(), Some("Bearer secret-key"));
        }
    }

    #[tokio::test]
    async fn settings_set_serializes_base_null_explicitly() {
        let server = MockGateway::start(vec![("/api/settings/set", 200, SET_OK_BODY)]).await;
        let client = GatewayClient::new(server.url.clone(), None).unwrap();

        let resp = client
            .settings_set(&SettingsSetRequest {
                base: None,
                document: serde_json::json!({}),
            })
            .await
            .unwrap();
        assert!(resp.ok && resp.setup_mode_exited);
        assert_eq!(resp.backup_path.as_deref(), Some("/tmp/config.yaml.bak"));
        // §9 的 `base: str | None` 没有默认值：省键会被判 422，必须显式写 null
        assert_eq!(server.recorded()[0].body, r#"{"base":null,"document":{}}"#);
    }

    #[tokio::test]
    async fn setup_mode_and_conflict_are_recognized_off_the_wire() {
        // setup mode 下非 settings 端点被守门中间件挡成 503 + error=setup_mode
        // （断在 create_session 上是真实路径：TUI/stdio 的预检之外，run/wait 也会撞到）。
        let setup = MockGateway::start(vec![(
            "/api/session/create",
            503,
            r#"{"error":"setup_mode","detail":"providers 不得为空"}"#,
        )])
        .await;
        let client = GatewayClient::new(setup.url.clone(), None).unwrap();
        let err = client
            .create_session(&CreateSessionRequest::default())
            .await
            .unwrap_err();
        assert!(err.is_setup_mode(), "{err}");
        assert!(!err.is_conflict());
        assert!(err.to_string().contains("503"), "错误文案带状态码：{err}");

        // 指纹不匹配 = 409（协议级失败）
        let conflict = MockGateway::start(vec![(
            "/api/settings/set",
            409,
            r#"{"error":"fingerprint_mismatch","detail":"配置已被其它客户端修改"}"#,
        )])
        .await;
        let client = GatewayClient::new(conflict.url.clone(), None).unwrap();
        let err = client
            .settings_set(&SettingsSetRequest {
                base: Some("sha256:stale".into()),
                document: serde_json::json!({}),
            })
            .await
            .unwrap_err();
        assert!(err.is_conflict(), "{err}");
        assert!(!err.is_setup_mode());
    }
}
