use anyhow::Result;
use serde_json::Value;
use shelllist_daemon_core::DaemonEndpoint;
use shelllist_daemon_tokio::{
    CallFailure, CancelMode, CorrelationPolicy, JsonlClientConfig, run_jsonl_client,
};

use crate::{
    api::{self, BUS_NAME, INTERFACE, OBJECT_PATH},
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
                .pointer("/operation/id")
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

fn call_failure(_method: &str, _error: &anyhow::Error) -> CallFailure {
    CallFailure::Api(api::error(
        "daemon-unavailable",
        "app-daemon session service is unavailable".into(),
    ))
}

pub async fn run() -> Result<()> {
    run_jsonl_client(JsonlClientConfig {
        endpoint: ENDPOINT,
        correlation: AppCorrelation,
        cancel_mode: CancelMode::Json,
        call_failure,
        pending_event_limit: 32,
        max_in_flight_requests: 64,
        shutdown_timeout: Some(std::time::Duration::from_secs(5)),
    })
    .await
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use serde_json::json;
    use shelllist_daemon_tokio::{CorrelationPolicy, TrackedKind};

    use super::AppCorrelation;
    use crate::protocol;

    #[test]
    fn correlates_application_operations_and_subscriptions() -> anyhow::Result<()> {
        let policy = AppCorrelation;
        let operation = policy
            .response_id(
                &json!({ "data": { "operation": { "id": "operation-1", "status": "accepted" } } }),
            )
            .context("operation correlation")?;
        assert_eq!(operation.id, "operation-1");
        assert_eq!(operation.kind, TrackedKind::Operation);
        assert_eq!(
            policy.event_id(
                protocol::stream::OPERATION,
                &json!({ "operation": { "id": "operation-1" } })
            ),
            Some("operation-1".into())
        );
        assert_eq!(
            policy.event_id(
                protocol::stream::OPERATION,
                &json!({
                    "event": "resync-required", "subscription_id": "sub-1"
                })
            ),
            Some("sub-1".into())
        );
        assert!(policy.is_terminal(
            protocol::stream::OPERATION,
            &json!({ "event": "completed" })
        ));

        let subscription = policy
            .response_id(&json!({ "data": { "subscription": { "id": "sub-1" } } }))
            .context("subscription correlation")?;
        assert_eq!(subscription.kind, TrackedKind::Subscription);
        for status in ["running", "completed", "failed", "cancelled"] {
            let response = json!({"data": {"operation": {"id": "op", "status": status}}});
            assert!(policy.response_id(&response).is_none(), "{status}");
        }
        Ok(())
    }
}
