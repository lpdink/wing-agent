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

/// What the caller can establish after a save request returns an error.
#[derive(Debug)]
pub enum SettingsSaveFailure {
    Conflict,
    Rejected(String),
    Unconfirmed(String),
}

impl From<&ApiClientError> for SettingsSaveFailure {
    fn from(error: &ApiClientError) -> Self {
        if error.is_conflict() {
            return Self::Conflict;
        }
        match error {
            ApiClientError::Api {
                status: 500,
                body: Some(body),
                ..
            } if body.error == "settings_write_failed" => Self::Rejected(
                body.detail
                    .as_deref()
                    .map_or_else(|| "无法写入配置文件".into(), super::common::single_line),
            ),
            ApiClientError::Api {
                status: status @ (400 | 401 | 403 | 404 | 405 | 413 | 422),
                ..
            } => Self::Rejected(format!("网关拒绝请求（HTTP {status}）")),
            ApiClientError::Api { status, .. } => Self::Unconfirmed(format!("HTTP {status}")),
            ApiClientError::Transport(error) if error.is_timeout() => {
                Self::Unconfirmed("请求超时".into())
            }
            _ => Self::Unconfirmed("未收到有效的保存回执".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::ErrorResponse;

    #[test]
    fn save_failure_classifies_confirmed_and_unknown_outcomes() {
        let http = |status, body| ApiClientError::Api {
            status,
            detail: "raw HTTP response".into(),
            body,
        };
        assert!(matches!(
            SettingsSaveFailure::from(&http(409, None)),
            SettingsSaveFailure::Conflict
        ));
        assert!(matches!(
            SettingsSaveFailure::from(&http(403, None)),
            SettingsSaveFailure::Rejected(_)
        ));
        assert!(matches!(
            SettingsSaveFailure::from(&http(502, None)),
            SettingsSaveFailure::Unconfirmed(_)
        ));
        assert!(matches!(
            SettingsSaveFailure::from(&http(504, None)),
            SettingsSaveFailure::Unconfirmed(_)
        ));
        assert!(
            matches!(SettingsSaveFailure::from(&http(500, Some(ErrorResponse {
            error: "settings_write_failed".into(),
            detail: Some("Read-only file system".into()),
            session_id: None,
            uuid: None,
        }))), SettingsSaveFailure::Rejected(reason) if reason == "Read-only file system")
        );
    }
}
