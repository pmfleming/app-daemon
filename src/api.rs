use std::sync::Arc;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use shelllist_daemon_core::{ApiError as EnvelopeError, ApiIdentity, error as error_envelope};

use crate::{
    model::{
        ApplicationEnergyOverview, ApplicationPage, ApplicationResourceHistory, OperationResult,
    },
    protocol::{
        self,
        methods::{OperationStatusParams, Request},
        replies::{ErrorCode, Settings},
    },
    service::{
        ApplicationService, EnergyOverviewParams, ExecuteParams, QueryParams,
        ResourceHistoryParams, UpdateSettingsParams,
    },
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
pub const BUS_NAME: &str = "org.laufan.AppDaemon";
pub const OBJECT_PATH: &str = "/org/laufan/AppDaemon";
pub const INTERFACE: &str = "org.laufan.AppDaemon2";
const API: ApiIdentity = ApiIdentity::new(PROTOCOL, VERSION as u32);

pub struct ApiService {
    applications: Arc<ApplicationService>,
}

impl ApiService {
    pub fn new(applications: Arc<ApplicationService>) -> Self {
        Self { applications }
    }

    pub async fn dispatch_owned(
        &self,
        method: &str,
        params: Value,
        owner: Option<String>,
    ) -> Value {
        let _permit = match self.applications.request_permit().await {
            Ok(permit) => permit,
            Err(error) => return self::error(ErrorCode::DaemonUnavailable, error.to_string()),
        };
        tracing::debug!(%method, "app-api request started");
        let request = match Request::decode(method, params) {
            Ok(request) => request,
            Err(error) => return error_response(error),
        };
        request
            .dispatch(self, owner)
            .await
            .map_or_else(error_response, success)
    }

    pub(crate) async fn query(
        &self,
        params: QueryParams,
        _owner: Option<String>,
    ) -> Result<ApplicationPage, ApiError> {
        Ok(self.applications.query(params).await)
    }

    pub(crate) async fn revision(
        &self,
        _params: Value,
        _owner: Option<String>,
    ) -> Result<u64, ApiError> {
        Ok(self.applications.revision().await)
    }

    pub(crate) async fn history(
        &self,
        params: ResourceHistoryParams,
        _owner: Option<String>,
    ) -> Result<ApplicationResourceHistory, ApiError> {
        self.applications
            .resource_history(params)
            .await
            .map_err(|error| (ErrorCode::ValidationError, error.to_string()))
    }

    pub(crate) async fn energy_overview(
        &self,
        params: EnergyOverviewParams,
        _owner: Option<String>,
    ) -> Result<ApplicationEnergyOverview, ApiError> {
        Ok(self.applications.energy_overview(params).await)
    }

    pub(crate) async fn refresh(
        &self,
        params: QueryParams,
        _owner: Option<String>,
    ) -> Result<ApplicationPage, ApiError> {
        self.applications.refresh().await;
        Ok(self.applications.query(params).await)
    }

    pub(crate) async fn update_settings(
        &self,
        params: UpdateSettingsParams,
        _owner: Option<String>,
    ) -> Result<Settings, ApiError> {
        let target_id = params.target_id.clone();
        self.applications
            .update_settings(params)
            .await
            .map(|settings| Settings {
                target_id,
                category: settings.category,
                workspace_id: settings.workspace_id,
            })
            .map_err(|error| (ErrorCode::ValidationError, error.to_string()))
    }

    pub(crate) async fn operation_status(
        &self,
        params: OperationStatusParams,
        owner: Option<String>,
    ) -> Result<OperationResult, ApiError> {
        self.applications
            .operation_status_owned(&params.operation_id, owner.as_deref())
            .await
            .ok_or_else(|| {
                (
                    ErrorCode::RequestNotFound,
                    "Operation is unknown, expired, or belongs to another client".into(),
                )
            })
    }

    pub(crate) async fn execute(
        &self,
        params: ExecuteParams,
        owner: Option<String>,
    ) -> Result<OperationResult, ApiError> {
        self.applications
            .execute_owned(params, owner)
            .await
            .map_err(|error| (ErrorCode::OperationFailed, error.to_string()))
    }
}

pub(crate) type ApiError = (ErrorCode, String);

pub(crate) fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|error| (ErrorCode::ValidationError, error.to_string()))
}

pub fn success(data: impl Serialize) -> Value {
    shelllist_daemon_core::success(
        API,
        serde_json::to_value(data).expect("serializable app-api reply"),
    )
}

fn error_response((code, message): ApiError) -> Value {
    error_envelope(
        API,
        EnvelopeError::new(code.as_str(), message).with_retryable(false),
    )
}

pub fn error(code: ErrorCode, message: String) -> Value {
    error_response((code, message))
}
