use anyhow::Result;
use serde_json::Value;
use shelllist_daemon_core::DaemonEndpoint;
use shelllist_daemon_tokio::{CorrelationPolicy, JsonlClientConfig, run_jsonl_client};

use crate::{
    api::{BUS_NAME, INTERFACE, OBJECT_PATH},
    protocol,
};

const ENDPOINT: DaemonEndpoint =
    DaemonEndpoint::new("app-daemon", BUS_NAME, OBJECT_PATH, INTERFACE);

#[derive(Debug, Clone, Copy)]
struct AppCorrelation;

impl CorrelationPolicy for AppCorrelation {
    fn operation_id<'a>(&self, response: &'a Value) -> Option<&'a str> {
        response
            .pointer("/data/operation/id")?
            .as_str()
            .filter(|_| {
                response
                    .pointer("/data/operation/status")
                    .and_then(Value::as_str)
                    == Some("accepted")
            })
    }

    fn event_id(&self, stream: &str, event: &Value) -> Option<String> {
        if stream == protocol::stream::OPERATION {
            event
                .pointer("/data/operation/id")
                .or_else(|| event.get("subscription_id"))
        } else {
            event.get("subscription_id")
        }
        .and_then(Value::as_str)
        .map(str::to_owned)
    }

    fn is_terminal(&self, stream: &str, event: &Value) -> bool {
        stream == protocol::stream::OPERATION
            && matches!(
                event.get("event").and_then(Value::as_str),
                Some("completed" | "failed" | "cancelled")
            )
    }
}

pub async fn run() -> Result<()> {
    run_jsonl_client(JsonlClientConfig {
        endpoint: ENDPOINT,
        correlation: AppCorrelation,
        pending_event_limit: 32,
        max_in_flight_requests: 64,
        shutdown_timeout: Some(std::time::Duration::from_secs(5)),
    })
    .await
}
