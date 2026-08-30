use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use shelllist_daemon_core::{ApiError as EnvelopeError, ApiIdentity, error as error_envelope};

use crate::{
    protocol,
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

impl ApiService {
    pub fn new(applications: Arc<ApplicationService>) -> Self {
        Self { applications }
    }

    pub async fn dispatch(&self, method: &str, params: Value) -> Value {
        self.dispatch_owned(method, params, None).await
    }

    pub async fn dispatch_owned(
        &self,
        method: &str,
        params: Value,
        owner: Option<String>,
    ) -> Value {
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
    ) -> Result<Value, ApiError> {
        match method {
            "applications.query" => self.query(params).await,
            "applications.history" => self.history(params).await,
            "applications.energyOverview" => self.energy_overview(params).await,
            "applications.refresh" => self.refresh(params).await,
            "applications.execute" => self.execute(params, owner).await,
            "applications.settings.update" => self.update_settings(params).await,
            _ => Err((
                "unsupported-method",
                format!("Unsupported app-api method: {method}"),
            )),
        }
    }

    async fn query(&self, params: Value) -> Result<Value, ApiError> {
        Ok(json!({ "applications": self.applications.query(decode(params)?).await }))
    }

    async fn history(&self, params: Value) -> Result<Value, ApiError> {
        let query: ResourceHistoryParams = decode(params)?;
        self.applications
            .resource_history(query)
            .await
            .map(|history| json!({ "history": history }))
            .map_err(|error| ("validation-error", error.to_string()))
    }

    async fn energy_overview(&self, params: Value) -> Result<Value, ApiError> {
        Ok(json!({
            "energy_overview": self
                .applications
                .energy_overview(decode::<EnergyOverviewParams>(params)?)
                .await
        }))
    }

    async fn refresh(&self, params: Value) -> Result<Value, ApiError> {
        let query = decode(params)?;
        self.applications.refresh().await;
        Ok(json!({ "applications": self.applications.query(query).await }))
    }

    async fn update_settings(&self, params: Value) -> Result<Value, ApiError> {
        let params: UpdateSettingsParams = decode(params)?;
        let target_id = params.target_id.clone();
        self.applications
            .update_settings(params)
            .await
            .map(|settings| {
                json!({ "settings": {
                    "target_id": target_id,
                    "category": settings.category,
                    "workspace_id": settings.workspace_id
                }})
            })
            .map_err(|error| ("validation-error", error.to_string()))
    }

    async fn execute(&self, params: Value, owner: Option<String>) -> Result<Value, ApiError> {
        self.applications
            .execute_owned(decode::<ExecuteParams>(params)?, owner)
            .await
            .map(|operation| json!({ "operation": operation }))
            .map_err(|error| ("operation-failed", error.to_string()))
    }
}

type ApiError = (&'static str, String);

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|error| ("validation-error", error.to_string()))
}

pub fn success(data: Value) -> Value {
    shelllist_daemon_core::success(API, data)
}

fn error_response((code, message): ApiError) -> Value {
    error_envelope(API, EnvelopeError::new(code, message).with_retryable(false))
}

pub fn error(code: &'static str, message: String) -> Value {
    error_response((code, message))
}
