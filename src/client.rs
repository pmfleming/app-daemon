use anyhow::Result;
use shelllist_daemon_core::DaemonEndpoint;
use shelllist_daemon_tokio::{
    BasicCorrelation, CallFailure, CancelMode, JsonlClientConfig, run_jsonl_client,
};

use crate::api::{self, BUS_NAME, INTERFACE, OBJECT_PATH};

const ENDPOINT: DaemonEndpoint =
    DaemonEndpoint::new("app-daemon", BUS_NAME, OBJECT_PATH, INTERFACE);

fn call_failure(_method: &str, _error: &anyhow::Error) -> CallFailure {
    CallFailure::Api(api::error(
        "daemon-unavailable",
        "app-daemon session service is unavailable".into(),
    ))
}

pub async fn run() -> Result<()> {
    run_jsonl_client(JsonlClientConfig {
        endpoint: ENDPOINT,
        correlation: BasicCorrelation,
        cancel_mode: CancelMode::Json,
        call_failure,
        pending_event_limit: 32,
        max_in_flight_requests: 64,
        shutdown_timeout: Some(std::time::Duration::from_secs(5)),
    })
    .await
}
