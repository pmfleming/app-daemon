use anyhow::Result;
use serde_json::Value;
use shelllist_daemon_core::DaemonEndpoint;
use shelllist_daemon_tokio::{
    CallFailure, CancelMode, CorrelationPolicy, JsonlClientConfig, TrackedId, TrackedKind,
    run_jsonl_client,
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
    fn response_id(&self, response: &Value) -> Option<TrackedId> {
        tracked(
            response.pointer("/data/operation/id"),
            TrackedKind::Operation,
        )
        .or_else(|| {
            tracked(
                response.pointer("/data/subscription/id"),
                TrackedKind::Subscription,
            )
        })
    }

    fn event_id(&self, stream: &str, event: &Value) -> Option<String> {
        if stream == protocol::stream::OPERATION {
            event.pointer("/operation/id")
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

fn tracked(value: Option<&Value>, kind: TrackedKind) -> Option<TrackedId> {
    value.and_then(Value::as_str).map(|id| TrackedId {
        id: id.to_owned(),
        kind,
    })
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
    use serde_json::json;
    use shelllist_daemon_tokio::{CorrelationPolicy, TrackedKind};

    use super::AppCorrelation;
    use crate::protocol;

    #[test]
    fn correlates_application_operations_and_subscriptions() {
        let policy = AppCorrelation;
        let operation = policy
            .response_id(&json!({ "data": { "operation": { "id": "operation-1" } } }))
            .unwrap();
        assert_eq!(operation.id, "operation-1");
        assert_eq!(operation.kind, TrackedKind::Operation);
        assert_eq!(
            policy.event_id(
                protocol::stream::OPERATION,
                &json!({ "operation": { "id": "operation-1" } })
            ),
            Some("operation-1".into())
        );
        assert!(policy.is_terminal(
            protocol::stream::OPERATION,
            &json!({ "event": "completed" })
        ));

        let subscription = policy
            .response_id(&json!({ "data": { "subscription": { "id": "sub-1" } } }))
            .unwrap();
        assert_eq!(subscription.kind, TrackedKind::Subscription);
    }
}
