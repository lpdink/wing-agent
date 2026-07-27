//! [`ToolHost`] — 远程工具宿主客户端。
//!
//! 通过 builder 模式声明工具，`build()` 建立 WS 连接并注册，
//! `run()` 进入调用服务循环。
//!
//! # Example
//!
//! ```no_run
//! use wing_api_client::tool_host::{ToolHost, ToolSpec};
//! use std::collections::HashMap;
//!
//! # async fn example() -> Result<(), wing_api_client::ApiClientError> {
//! let host = ToolHost::builder("my-host", "ws://127.0.0.1:32523/ws")
//!     .api_key(Some("secret"))
//!     .tool(
//!         ToolSpec::new("Bash", "Execute a shell command")
//!             .param("command", "string", "The command to execute.")
//!             .param_optional("timeout", "integer", "Max seconds.", 30),
//!         |args| Box::pin(async move {
//!             let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
//!             Ok(format!("executed: {cmd}"))
//!         }),
//!     )
//!     .build()
//!     .await?;
//!
//! host.run().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::client::GatewayClient;
use crate::error::ApiClientError;
use crate::models::{RemoteToolSpec, ToolParam, WsToolCallRequest, WsToolCallResult};

/// WS sink 类型别名（避免重复书写长泛型）。
type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// WS stream 类型别名。
type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

/// 工具调用 handler 类型。
pub type ToolHandler =
    fn(HashMap<String, Value>) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;

/// 工具规格 builder。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    name: String,
    description: String,
    llm_name: Option<String>,
    params: Vec<ToolParam>,
}

impl ToolSpec {
    /// 创建工具规格。
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            llm_name: None,
            params: Vec::new(),
        }
    }

    /// 设置 LLM 可见名（可选）。
    pub fn llm_name(mut self, name: impl Into<String>) -> Self {
        self.llm_name = Some(name.into());
        self
    }

    /// 添加必填参数。
    pub fn param(
        mut self,
        name: impl Into<String>,
        param_type: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        self.params.push(ToolParam {
            name: name.into(),
            param_type: param_type.into(),
            description: description.into(),
            default: None,
            items: None,
        });
        self
    }

    /// 添加可选参数（带默认值）。
    pub fn param_optional(
        mut self,
        name: impl Into<String>,
        param_type: impl Into<String>,
        description: impl Into<String>,
        default: impl Into<Value>,
    ) -> Self {
        self.params.push(ToolParam {
            name: name.into(),
            param_type: param_type.into(),
            description: description.into(),
            default: Some(default.into()),
            items: None,
        });
        self
    }

    /// 添加数组参数。
    pub fn param_array(
        mut self,
        name: impl Into<String>,
        items_type: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        self.params.push(ToolParam {
            name: name.into(),
            param_type: "array".to_owned(),
            description: description.into(),
            default: None,
            items: Some(Value::String(items_type.into())),
        });
        self
    }

    /// 转换为协议模型。
    pub fn to_remote_spec(&self) -> RemoteToolSpec {
        RemoteToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            llm_name: self.llm_name.clone(),
            params: self.params.clone(),
        }
    }
}

/// ToolHost builder。
pub struct ToolHostBuilder {
    client_id: String,
    ws_url: String,
    api_key: Option<String>,
    tools: Vec<(ToolSpec, ToolHandler)>,
}

impl ToolHostBuilder {
    /// 注册一个工具及其 handler。
    pub fn tool(mut self, spec: ToolSpec, handler: ToolHandler) -> Self {
        self.tools.push((spec, handler));
        self
    }

    /// 设置 API key。
    pub fn api_key(mut self, key: Option<&str>) -> Self {
        self.api_key = key.map(|k| k.to_owned());
        self
    }

    /// 构建 ToolHost：WS 连接 → 注册工具。失败时断连并返回错误。
    pub async fn build(self) -> Result<ToolHost, ApiClientError> {
        // 1. WS connect with client_id
        let url = format!("{}?client_id={}", self.ws_url, self.client_id);
        let mut request = url
            .into_client_request()
            .map_err(|e| ApiClientError::Connection(format!("invalid WS URL: {e}")))?;

        if let Some(key) = &self.api_key
            && !key.is_empty()
        {
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {key}")
                    .parse()
                    .map_err(|_| ApiClientError::Connection("invalid API key chars".into()))?,
            );
        }

        let (ws_stream, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ApiClientError::Connection(format!("WS connect failed: {e}")))?;

        let (ws_sink, mut ws_stream_rx) = ws_stream.split();

        // 2. Read ConnectResponse (first frame)
        let first_msg = ws_stream_rx
            .next()
            .await
            .ok_or_else(|| {
                ApiClientError::Connection("gateway closed before ConnectResponse".into())
            })?
            .map_err(|e| ApiClientError::Connection(format!("WS read error: {e}")))?;

        match first_msg {
            Message::Text(text) => {
                // Validate it's a ConnectResponse (has "type": "connected")
                let val: Value = serde_json::from_str(&text)
                    .map_err(|e| ApiClientError::Connection(format!("bad ConnectResponse: {e}")))?;
                if val.get("type").and_then(|t| t.as_str()) != Some("connected") {
                    return Err(ApiClientError::Connection(format!(
                        "unexpected first frame: {text}"
                    )));
                }
            }
            other => {
                return Err(ApiClientError::Connection(format!(
                    "expected ConnectResponse, got: {other:?}"
                )));
            }
        }

        // 3. HTTP register tools
        let http_base = self
            .ws_url
            .replace("ws://", "http://")
            .replace("wss://", "https://");
        // Strip /ws path suffix for HTTP base
        let http_base = http_base.trim_end_matches("/ws");
        let http_client = GatewayClient::new(http_base, self.api_key.as_deref())?;

        let specs: Vec<RemoteToolSpec> =
            self.tools.iter().map(|(s, _)| s.to_remote_spec()).collect();
        if let Err(e) = http_client.register_tools(&self.client_id, specs).await {
            // Rollback: close WS
            drop(ws_stream_rx);
            return Err(e);
        }

        // 4. Build handler map
        let mut handlers: HashMap<String, ToolHandler> = HashMap::new();
        for (spec, handler) in &self.tools {
            handlers.insert(spec.name.clone(), *handler);
        }

        tracing::info!(
            client_id = %self.client_id,
            tools = ?handlers.keys().collect::<Vec<_>>(),
            "tool host built and registered"
        );

        Ok(ToolHost {
            client_id: self.client_id,
            ws_sink: Arc::new(Mutex::new(ws_sink)),
            ws_stream: ws_stream_rx,
            handlers,
        })
    }
}

/// 远程工具宿主——持有 WS 连接，服务工具调用。
pub struct ToolHost {
    client_id: String,
    ws_sink: Arc<Mutex<WsSink>>,
    ws_stream: WsStream,
    handlers: HashMap<String, ToolHandler>,
}

impl ToolHost {
    /// 创建 builder。
    pub fn builder(client_id: impl Into<String>, ws_url: impl Into<String>) -> ToolHostBuilder {
        ToolHostBuilder {
            client_id: client_id.into(),
            ws_url: ws_url.into(),
            api_key: None,
            tools: Vec::new(),
        }
    }

    /// 返回 client_id。
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// 进入调用服务循环。WS 断连时返回。
    ///
    /// 每收到一个 `tool_call_request` 帧，spawn 一个 task 执行 handler 并回传结果。
    /// 未知工具名以 is_error 回传。per-frame 解析错误不终止循环。
    pub async fn run(mut self) -> Result<(), ApiClientError> {
        let sink = self.ws_sink.clone();
        let handlers = Arc::new(self.handlers);
        let mut tasks = tokio::task::JoinSet::new();

        while let Some(msg_result) = self.ws_stream.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    let request: WsToolCallRequest =
                        match serde_json::from_str::<WsToolCallRequest>(&text) {
                            Ok(req) if req.frame_type == "tool_call_request" => req,
                            Ok(_) => continue, // 非工具调用帧，忽略
                            Err(e) => {
                                tracing::warn!("unparseable frame: {e}");
                                continue;
                            }
                        };

                    let call_id = request.call_id.clone();
                    let tool_name = request.name.clone();
                    let arguments = request.arguments.clone();
                    let sink = sink.clone();
                    let handlers = handlers.clone();

                    tasks.spawn(async move {
                        let result = if let Some(handler) = handlers.get(&tool_name) {
                            let args = match arguments {
                                Value::Object(map) => map.into_iter().collect(),
                                _ => HashMap::new(),
                            };
                            match handler(args).await {
                                Ok(output) => WsToolCallResult::success(&call_id, output),
                                Err(e) => WsToolCallResult::error(&call_id, e),
                            }
                        } else {
                            WsToolCallResult::error(&call_id, format!("unknown tool: {tool_name}"))
                        };

                        let json = match serde_json::to_string(&result) {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::error!("failed to serialize tool result: {e}");
                                return;
                            }
                        };
                        let mut sink_guard = sink.lock().await;
                        if let Err(e) = sink_guard.send(Message::Text(json.into())).await {
                            tracing::error!("failed to send tool result: {e}");
                        }
                    });
                }
                Ok(Message::Close(frame)) => {
                    tracing::info!("gateway sent close frame: {frame:?}");
                    break;
                }
                Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                    // tungstenite 内部处理
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(ApiClientError::Connection(format!("WS read error: {e}")));
                }
            }
        }

        // 等待在途 handler tasks 完成（优雅排水）
        while tasks.join_next().await.is_some() {}

        tracing::info!(client_id = %self.client_id, "tool host disconnected");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_spec_builder_serialization() {
        let spec = ToolSpec::new("Bash", "Execute a shell command")
            .param("command", "string", "The command to execute.")
            .param_optional("timeout", "integer", "Max seconds.", 30);

        let remote = spec.to_remote_spec();
        assert_eq!(remote.name, "Bash");
        assert_eq!(remote.description, "Execute a shell command");
        assert_eq!(remote.params.len(), 2);
        assert_eq!(remote.params[0].name, "command");
        assert_eq!(remote.params[0].param_type, "string");
        assert!(remote.params[0].default.is_none());
        assert_eq!(remote.params[1].name, "timeout");
        assert_eq!(remote.params[1].default, Some(serde_json::json!(30)));

        // JSON round-trip
        let json = serde_json::to_string(&remote).unwrap();
        assert!(json.contains("\"name\":\"Bash\""));
        assert!(json.contains("\"type\":\"string\""));
        // llm_name omitted when None
        assert!(!json.contains("llm_name"));
    }

    #[test]
    fn tool_spec_with_llm_name() {
        let spec = ToolSpec::new("exec", "Run stuff").llm_name("Bash");
        let remote = spec.to_remote_spec();
        assert_eq!(remote.llm_name, Some("Bash".to_owned()));
        let json = serde_json::to_string(&remote).unwrap();
        assert!(json.contains("\"llm_name\":\"Bash\""));
    }

    #[test]
    fn ws_tool_call_result_success() {
        let result = WsToolCallResult::success("call_1", "hello world".into());
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["type"], "tool_call_result");
        assert_eq!(json["call_id"], "call_1");
        assert_eq!(json["result"], "hello world");
        assert_eq!(json["is_error"], false);
    }

    #[test]
    fn ws_tool_call_result_error() {
        let result = WsToolCallResult::error("call_2", "not found".into());
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["is_error"], true);
        assert_eq!(json["result"], "not found");
    }

    #[test]
    fn ws_tool_call_request_deserialization() {
        let json = r#"{"type":"tool_call_request","call_id":"abc","name":"Bash","arguments":{"command":"ls"}}"#;
        let req: WsToolCallRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.frame_type, "tool_call_request");
        assert_eq!(req.call_id, "abc");
        assert_eq!(req.name, "Bash");
        assert_eq!(req.arguments["command"], "ls");
    }

    #[test]
    fn register_tools_request_serialization() {
        let spec = ToolSpec::new("Read", "Read a file").param("path", "string", "File path");
        let req = crate::models::RegisterToolsRequest {
            tools: vec![spec.to_remote_spec()],
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["tools"][0]["name"], "Read");
        assert_eq!(json["tools"][0]["params"][0]["name"], "path");
    }
}
