//! Typed payloads shared by live subscriptions, schemas and offline fixtures.
use crate::{api, model::OperationResult, protocol, service::StateRevision};
use serde::Serialize;
use serde_json::{Value, json};
use shelllist_daemon_core::{
    ApiIdentity, Correlation,
    wire::{Event as Envelope, Gap},
};
use zbus::object_server::SignalEmitter;

#[derive(Serialize, schemars::JsonSchema)]
struct OperationData<'a> {
    operation: &'a OperationResult,
}
#[derive(Serialize, schemars::JsonSchema)]
struct SubscribedData {
    catalog_revision: u64,
    window_revision: u64,
    settings_revision: u64,
    runtime_revision: u64,
}
#[derive(Serialize, schemars::JsonSchema)]
struct ChangedData {
    revision: u64,
}

pub(crate) struct Event {
    stream: &'static str,
    value: Value,
}
impl Event {
    pub(crate) fn into_value(self) -> Value {
        self.value
    }
}

pub(crate) fn operation(subscription_id: &str, operation: &OperationResult) -> Event {
    envelope(
        protocol::stream::OPERATION,
        subscription_id,
        &operation.status,
        OperationData { operation },
    )
}
pub(crate) fn operation_gap(subscription_id: &str, dropped_events: u64) -> Event {
    envelope(
        protocol::stream::OPERATION,
        subscription_id,
        "gap",
        Gap::lagged(dropped_events),
    )
}
pub(crate) fn subscribed(
    stream: &'static str,
    subscription_id: &str,
    revision: StateRevision,
) -> Event {
    envelope(
        stream,
        subscription_id,
        "subscribed",
        SubscribedData {
            catalog_revision: revision.catalog,
            window_revision: revision.windows,
            settings_revision: revision.settings,
            runtime_revision: revision.runtime,
        },
    )
}
pub(crate) fn changed(stream: &'static str, subscription_id: &str, revision: u64) -> Event {
    envelope(stream, subscription_id, "changed", ChangedData { revision })
}
fn envelope(
    stream: &'static str,
    subscription_id: &str,
    event: &str,
    data: impl Serialize,
) -> Event {
    Event {
        stream,
        value: Envelope::new(
            ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
            stream,
            event,
            Correlation::Subscription(subscription_id),
            data,
        )
        .into_value()
        .expect("serializable app event"),
    }
}
pub(super) async fn emit(emitter: &SignalEmitter<'_>, event: Event) {
    let stream = event.stream;
    if let Err(error) = emitter
        .emit(api::INTERFACE, "Event", &(stream, event.value.to_string()))
        .await
    {
        tracing::warn!(%stream, %error, "app-api event could not be emitted");
    }
}
fn schema<T: schemars::JsonSchema>() -> Value {
    shelllist_daemon_core::method_schemas::<(), T>()["result_schema"].take()
}
pub(crate) fn registry(stream: &str) -> Value {
    let mut events = vec![json!({"name":"subscribed", "data_schema":schema::<SubscribedData>()})];
    if stream == protocol::stream::OPERATION {
        for name in ["accepted", "running", "completed", "failed", "cancelled"] {
            events.push(json!({"name":name, "data_schema":schema::<OperationData<'_>>()}));
        }
        events.push(json!({"name":"gap", "data_schema":schema::<Gap>()}));
    } else {
        events.push(json!({"name":"changed", "data_schema":schema::<ChangedData>()}));
    }
    json!({"name":stream, "semantics":if stream == protocol::stream::OPERATION {"transitions"} else {"invalidation"}, "events":events})
}
#[cfg(test)]
mod tests {
    use super::{StateRevision, changed, operation_gap, protocol, subscribed};
    #[test]
    fn payload_and_gap_are_canonical_and_emission_uses_the_envelope_stream() {
        let gap = operation_gap("sub", 4).into_value();
        assert_eq!(gap["event"], "gap");
        assert_eq!(gap["data"]["dropped_events"], "4");
        assert_eq!(gap["subscription_id"], "sub");
        assert!(gap.get("dropped_events").is_none());
        assert!(gap["data"].get("recovery_method").is_none());
        for stream in protocol::STREAMS {
            let event = subscribed(
                stream,
                "sub",
                StateRevision {
                    catalog: 1,
                    windows: 2,
                    settings: 3,
                    runtime: 4,
                },
            );
            assert_eq!(event.stream, event.value["stream"]);
            assert_eq!(event.value["data"]["catalog_revision"], 1);
            assert!(event.value.get("catalog_revision").is_none());
            let changed = changed(stream, "sub", 9);
            assert_eq!(changed.value["data"]["revision"], 9);
        }
    }
}
