use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    protocol,
    service::{ApplicationService, ExecuteParams},
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;

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
                    .map_err(|error| ApiError::new("operation-failed", error.to_string()))
            }
            _ => Err(ApiError::new(
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

#[derive(Debug)]
pub struct ApiError {
    code: &'static str,
    message: String,
}
impl ApiError {
    fn new(code: &'static str, message: String) -> Self {
        Self { code, message }
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value)
        .map_err(|error| ApiError::new("validation-error", error.to_string()))
}

pub fn success(data: Value) -> Value {
    json!({ "protocol": PROTOCOL, "version": VERSION, "ok": true, "data": data })
}

fn error_response(error: ApiError) -> Value {
    json!({ "protocol": PROTOCOL, "version": VERSION, "ok": false, "error": { "code": error.code, "message": error.message, "retryable": false } })
}

pub fn error(code: &'static str, message: String) -> Value {
    error_response(ApiError::new(code, message))
}
