//! Wing Gateway HTTP API client — 手写的、符合项目风格的 Rust 客户端。
//!
//! 唯一的入口类型是 [`GatewayClient`]，所有 API 调用都是其上的 async 方法。
//!
//! ```no_run
//! # use wing_api_client::GatewayClient;
//! # async fn example() -> Result<(), wing_api_client::ApiClientError> {
//! let client = GatewayClient::localhost(None)?;
//! let health = client.health().await?;
//! assert_eq!(health.status, "ok");
//! # Ok(())
//! # }
//! ```

mod client;
mod error;
pub mod models;

pub use client::{DEFAULT_PORT, GatewayClient};
pub use error::ApiClientError;
