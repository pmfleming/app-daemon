//! Operation events shared by live subscriptions and offline contract fixtures.
use serde_json::{Value, json};
use shelllist_daemon_core::{ApiIdentity, Correlation, event_envelope};
use zbus::object_server::SignalEmitter;

use crate::{api, model::OperationResult, protocol};

pub(crate) fn operation(subscription_id: &str, operation: &OperationResult) -> Value {
    envelope(
        subscription_id,
        &operation.status,
        json!({ "operation": operation }),
    )
}

pub(crate) fn operation_gap(subscription_id: &str, dropped_events: u64) -> Value {
    envelope(
        subscription_id,
        "resync-required",
        json!({
            "dropped_events": dropped_events,
            "recovery_method": "applications.operation.status"
        }),
    )
}

fn envelope(subscription_id: &str, event: &str, fields: Value) -> Value {
    event_envelope(
        ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
        protocol::stream::OPERATION,
        event,
        Correlation::Subscription(subscription_id),
        fields,
    )
}

pub(super) async fn emit(emitter: &SignalEmitter<'_>, event: Value) {
    let stream = protocol::stream::OPERATION;
    if let Err(error) = emitter
        .emit(api::INTERFACE, "Event", &(stream, event.to_string()))
        .await
    {
        tracing::warn!(%stream, %error, "app-api event could not be emitted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_fixture_preserves_live_v1_envelopes() -> anyhow::Result<()> {
        let fixture = crate::protocol::event_contract_fixture();
        for status in ["completed", "failed", "cancelled"] {
            let event = &fixture[status];
            let operation: OperationResult = serde_json::from_value(event["operation"].clone())?;
            assert_eq!(operation.status, status);
            assert_eq!(event, &super::operation("subscription-example", &operation));
            assert_eq!(event["protocol"], protocol::NAME);
            assert_eq!(event["version"], protocol::VERSION);
            assert_eq!(event["stream"], protocol::stream::OPERATION);
            assert_eq!(event["subscription_id"], "subscription-example");
            assert_eq!(event["event"], status);
            assert!(event.get("data").is_none(), "v1 payloads are top-level");
        }
        let gap = &fixture["gap"];
        assert_eq!(gap, &operation_gap("subscription-example", 4));
        assert_eq!(gap["event"], "resync-required");
        assert_eq!(gap["dropped_events"], 4);
        assert_eq!(gap["recovery_method"], "applications.operation.status");
        assert!(gap.get("data").is_none());
        Ok(())
    }
}
