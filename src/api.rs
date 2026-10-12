use std::sync::Arc;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use shelllist_daemon_core::{ApiError as EnvelopeError, ApiIdentity, error as error_envelope};

use crate::{
    protocol::{
        self,
        replies::{ErrorCode, Reply, Settings},
    },
    service::{
        ApplicationService, EnergyOverviewParams, ExecuteParams, ResourceHistoryParams,
        UpdateSettingsParams,
    },
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
pub const BUS_NAME: &str = "org.laufan.AppDaemon";
pub const OBJECT_PATH: &str = "/org/laufan/AppDaemon";
pub const INTERFACE: &str = "org.laufan.AppDaemon1";
const API: ApiIdentity = ApiIdentity::new(PROTOCOL, VERSION as u32);

pub struct ApiService {
    applications: Arc<ApplicationService>,
}

#[derive(Deserialize)]
struct OperationStatusParams {
    operation_id: String,
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
        self.request(method, params, owner)
            .await
            .map_or_else(error_response, success)
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        owner: Option<String>,
    ) -> Result<Reply, ApiError> {
        match method {
            "applications.query" => self.query(params).await,
            "applications.revision" => self.revision().await,
            "applications.history" => self.history(params).await,
            "applications.energyOverview" => self.energy_overview(params).await,
            "applications.refresh" => self.refresh(params).await,
            "applications.execute" => self.execute(params, owner).await,
            "applications.operation.status" => {
                self.operation_status(params, owner.as_deref()).await
            }
            "applications.settings.update" => self.update_settings(params).await,
            _ => Err((
                ErrorCode::UnsupportedMethod,
                format!("Unsupported app-api method: {method}"),
            )),
        }
    }

    async fn query(&self, params: Value) -> Result<Reply, ApiError> {
        Ok(Reply::Applications {
            applications: self.applications.query(decode(params)?).await,
        })
    }

    async fn revision(&self) -> Result<Reply, ApiError> {
        Ok(Reply::Revision {
            revision: self.applications.revision().await,
        })
    }

    async fn history(&self, params: Value) -> Result<Reply, ApiError> {
        let query: ResourceHistoryParams = decode(params)?;
        self.applications
            .resource_history(query)
            .await
            .map(|history| Reply::History { history })
            .map_err(|error| (ErrorCode::ValidationError, error.to_string()))
    }

    async fn energy_overview(&self, params: Value) -> Result<Reply, ApiError> {
        Ok(Reply::EnergyOverview {
            energy_overview: self
                .applications
                .energy_overview(decode::<EnergyOverviewParams>(params)?)
                .await,
        })
    }

    async fn refresh(&self, params: Value) -> Result<Reply, ApiError> {
        let query = decode(params)?;
        self.applications.refresh().await;
        Ok(Reply::Applications {
            applications: self.applications.query(query).await,
        })
    }

    async fn update_settings(&self, params: Value) -> Result<Reply, ApiError> {
        let params: UpdateSettingsParams = decode(params)?;
        let target_id = params.target_id.clone();
        self.applications
            .update_settings(params)
            .await
            .map(|settings| Reply::Settings {
                settings: Settings {
                    target_id,
                    category: settings.category,
                    workspace_id: settings.workspace_id,
                },
            })
            .map_err(|error| (ErrorCode::ValidationError, error.to_string()))
    }

    async fn operation_status(
        &self,
        params: Value,
        owner: Option<&str>,
    ) -> Result<Reply, ApiError> {
        let params: OperationStatusParams = decode(params)?;
        self.applications
            .operation_status_owned(&params.operation_id, owner)
            .await
            .map(|operation_status| Reply::OperationStatus { operation_status })
            .ok_or_else(|| {
                (
                    ErrorCode::RequestNotFound,
                    "Operation is unknown, expired, or belongs to another client".into(),
                )
            })
    }

    async fn execute(&self, params: Value, owner: Option<String>) -> Result<Reply, ApiError> {
        self.applications
            .execute_owned(decode::<ExecuteParams>(params)?, owner)
            .await
            .map(|operation| Reply::Operation { operation })
            .map_err(|error| (ErrorCode::OperationFailed, error.to_string()))
    }
}

type ApiError = (ErrorCode, String);

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
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
