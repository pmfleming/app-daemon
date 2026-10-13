//! Typed events shared by live subscriptions and offline contract fixtures.
use serde::Serialize;
use serde_json::Value;
use shelllist_daemon_core::{ApiIdentity, Correlation, event_envelope};
use zbus::object_server::SignalEmitter;

use crate::{api, model::OperationResult, protocol, service::StateRevision};

#[derive(Serialize)]
#[serde(untagged)]
enum Fields<'a> {
    Operation {
        operation: &'a OperationResult,
    },
    Gap {
        dropped_events: u64,
        recovery_method: &'static str,
    },
    Subscribed {
        catalog_revision: u64,
        window_revision: u64,
        settings_revision: u64,
        runtime_revision: u64,
    },
    Changed {
        revision: u64,
    },
}

/// Keep the D-Bus stream and the JSON stream inseparable at emission.
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
        Fields::Operation { operation },
    )
}

pub(crate) fn operation_gap(subscription_id: &str, dropped_events: u64) -> Event {
    envelope(
        protocol::stream::OPERATION,
        subscription_id,
        "resync-required",
        Fields::Gap {
            dropped_events,
            recovery_method: "applications.operation.status",
        },
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
        Fields::Subscribed {
            catalog_revision: revision.catalog,
            window_revision: revision.windows,
            settings_revision: revision.settings,
            runtime_revision: revision.runtime,
        },
    )
}

pub(crate) fn changed(stream: &'static str, subscription_id: &str, revision: u64) -> Event {
    envelope(
        stream,
        subscription_id,
        "changed",
        Fields::Changed { revision },
    )
}

fn envelope(stream: &'static str, subscription_id: &str, event: &str, fields: Fields<'_>) -> Event {
    Event {
        stream,
        value: event_envelope(
            ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
            stream,
            event,
            Correlation::Subscription(subscription_id),
            serde_json::to_value(fields).expect("serializable app-api event"),
        ),
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

#[cfg(test)]
mod tests {
    use super::{StateRevision, changed, operation_gap, protocol, subscribed};

    #[test]
    fn event_payloads_remain_top_level_and_emission_uses_the_envelope_stream() {
        let gap = operation_gap("sub", 4).into_value();
        assert_eq!(gap["event"], "resync-required");
        assert_eq!(gap["dropped_events"], 4);
        assert_eq!(gap["subscription_id"], "sub");
        assert_eq!(gap["recovery_method"], "applications.operation.status");
        assert!(gap.get("data").is_none());
        for stream in protocol::STREAMS {
            let subscribed = subscribed(
                stream,
                "sub",
                StateRevision {
                    catalog: 1,
                    windows: 2,
                    settings: 3,
                    runtime: 4,
                },
            );
            assert_eq!(subscribed.stream, subscribed.value["stream"]);
            assert_eq!(subscribed.value["catalog_revision"], 1);
            assert_eq!(subscribed.value["window_revision"], 2);
            assert_eq!(subscribed.value["settings_revision"], 3);
            assert_eq!(subscribed.value["runtime_revision"], 4);
            assert!(subscribed.value.get("data").is_none());
            let changed = changed(stream, "sub", 9);
            assert_eq!(changed.stream, changed.value["stream"]);
            assert_eq!(changed.value["revision"], 9);
            assert_eq!(changed.value["subscription_id"], "sub");
        }
    }
}
