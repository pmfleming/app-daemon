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
        let result = match method {
            "applications.query" => Ok(
                json!({ "applications": self.applications.query(match decode(params) { Ok(value) => value, Err(error) => return error_response(error) }).await }),
            ),
            "applications.history" => {
                let query: ResourceHistoryParams = match decode(params) {
                    Ok(value) => value,
                    Err(error) => return error_response(error),
                };
                Ok(json!({ "history": self.applications.resource_history(query).await }))
            }
            "applications.refresh" => {
                let query = match decode(params) {
                    Ok(value) => value,
                    Err(error) => return error_response(error),
                };
                self.applications.refresh().await;
                Ok(json!({ "applications": self.applications.query(query).await }))
            }
            "applications.execute" => {
                let execute: ExecuteParams = match decode(params) {
                    Ok(value) => value,
                    Err(error) => return error_response(error),
                };
                self.applications
                    .execute(execute)
                    .await
                    .map(|operation| json!({ "operation": operation }))
                    .map_err(|error| ("operation-failed", error.to_string()))
            }
            _ => Err((
                "unsupported-method",
                format!("Unsupported app-api method: {method}"),
            )),
        };
        match result {
            Ok(data) => success(data),
            Err(error) => error_response(error),
        }
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
