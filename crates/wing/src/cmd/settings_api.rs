//! Setting API 门面：四个 `/api/settings/*` 操作的可注入窄接口。
//!
//! `wing config` 与首次运行向导（`cmd::setup`）是同一套端点上的两个界面，
//! 共用这一份 trait；各自按需再加自己的本地 I/O 方法（见 `setup::SetupBackend`）。
//!
//! trait 保持 `pub(super)`：`async fn in trait` 的 lint 只针对公开 trait，静态分发即可（不用 `dyn`）。

use wing_api_client::models::{
    SettingsGetResponse, SettingsSchemaResponse, SettingsSetRequest, SettingsSetResponse,
    SettingsStatusResponse,
};
use wing_api_client::{ApiClientError, GatewayClient as GatewayApiClient};

pub(super) trait SettingsApi {
    /// `GET /api/settings/schema`
    async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError>;
    /// `GET /api/settings/get`
    async fn get(&self) -> Result<SettingsGetResponse, ApiClientError>;
    /// `GET /api/settings/status`
    async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError>;
    /// `POST /api/settings/set`
    async fn set(&self, req: &SettingsSetRequest) -> Result<SettingsSetResponse, ApiClientError>;
}

impl SettingsApi for GatewayApiClient {
    async fn schema(&self) -> Result<SettingsSchemaResponse, ApiClientError> {
        self.settings_schema().await
    }

    async fn get(&self) -> Result<SettingsGetResponse, ApiClientError> {
        self.settings_get().await
    }

    async fn status(&self) -> Result<SettingsStatusResponse, ApiClientError> {
        self.settings_status().await
    }

    async fn set(&self, req: &SettingsSetRequest) -> Result<SettingsSetResponse, ApiClientError> {
        self.settings_set(req).await
    }
}
