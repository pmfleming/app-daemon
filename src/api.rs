use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    protocol,
    service::{ApplicationService, ExecuteParams, ResourceHistoryParams},
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
pub const BUS_NAME: &str = "org.laufan.AppDaemon";
pub const OBJECT_PATH: &str = "/org/laufan/AppDaemon";
pub const INTERFACE: &str = "org.laufan.AppDaemon1";

pub struct ApiService {
    applications: Arc<ApplicationService>,
}

impl ApiService {
    pub fn new(applications: Arc<ApplicationService>) -> Self {
        Self { applications }
    }

    pub async fn dispatch(&self, method: &str, params: Value) -> Value {
        tracing::debug!(%method, "app-api request started");
        self.request(method, params)
            .await
            .map_or_else(error_response, success)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "applications.query" => self.query(params).await,
            "applications.history" => self.history(params).await,
            "applications.refresh" => self.refresh(params).await,
            "applications.execute" => self.execute(params).await,
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
        Ok(json!({ "history": self.applications.resource_history(query).await }))
    }

    async fn refresh(&self, params: Value) -> Result<Value, ApiError> {
        let query = decode(params)?;
        self.applications.refresh().await;
        Ok(json!({ "applications": self.applications.query(query).await }))
    }

    async fn execute(&self, params: Value) -> Result<Value, ApiError> {
        self.applications
            .execute(decode::<ExecuteParams>(params)?)
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
    json!({ "protocol": PROTOCOL, "version": VERSION, "ok": true, "data": data })
}

fn error_response((code, message): ApiError) -> Value {
    json!({ "protocol": PROTOCOL, "version": VERSION, "ok": false, "error": { "code": code, "message": message, "retryable": false } })
}

pub fn error(code: &'static str, message: String) -> Value {
    error_response((code, message))
}
