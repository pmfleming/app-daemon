use anyhow::Result;
use serde_json::Value;
use shelllist_daemon_core::DaemonEndpoint;
use shelllist_daemon_tokio::{BasicCorrelation, CancelMode, JsonlClientConfig, run_jsonl_client};

use crate::api::{self, BUS_NAME, INTERFACE, OBJECT_PATH};

const ENDPOINT: DaemonEndpoint =
    DaemonEndpoint::new("app-daemon", BUS_NAME, OBJECT_PATH, INTERFACE);

fn call_failure(_method: &str, _error: &anyhow::Error) -> Value {
    api::error(
        "daemon-unavailable",
        "app-daemon session service is unavailable".into(),
    )
}

pub async fn run() -> Result<()> {
    run_jsonl_client(JsonlClientConfig {
        endpoint: ENDPOINT,
        correlation: BasicCorrelation,
        cancel_mode: CancelMode::Json,
        call_failure,
    })
    .await
}
