use serde_json::Value;

mod fixtures;
pub mod methods;
pub mod replies;

pub use methods::METHODS;

pub const NAME: &str = "app-api";
pub const VERSION: u8 = 1;

pub mod stream {
    pub const APPLICATIONS: &str = "applications.changed";
    pub const WINDOWS: &str = "windows.changed";
    pub const OPERATION: &str = "applications.operation";
}

pub const STREAMS: &[&str] = &[stream::APPLICATIONS, stream::WINDOWS, stream::OPERATION];

pub fn contract_fixture() -> serde_json::Result<Value> {
    let mut fixture =
        shelllist_daemon_core::load_fixture(include_str!("../test_support/app-api-v1.json"))?;
    fixture["registry"] = registry()?;
    Ok(fixture)
}

/// Canonical serialized resource shapes consumed by Shelllist presentation.
pub fn resource_contract_fixture() -> serde_json::Result<Value> {
    shelllist_daemon_core::load_fixture(include_str!("../test_support/app-resource-v1.json"))
}

/// Offline replies built with the same typed bodies and envelopes as live dispatch.
pub fn reply_contract_fixture() -> Value {
    fixtures::replies()
}

/// Offline examples built with the same event constructors as live subscriptions.
pub fn event_contract_fixture() -> Value {
    let mut fixture = serde_json::Map::new();
    for status in ["completed", "failed", "cancelled"] {
        let operation = fixtures::operation(status);
        fixture.insert(
            status.into(),
            crate::daemon::events::operation("subscription-example", &operation).into_value(),
        );
    }
    fixture.insert(
        "gap".into(),
        crate::daemon::events::operation_gap("subscription-example", 4).into_value(),
    );
    for stream in STREAMS {
        fixture.insert(
            format!("{stream}.subscribed"),
            crate::daemon::events::subscribed(
                stream,
                "subscription-example",
                crate::service::StateRevision {
                    catalog: 1,
                    windows: 2,
                    settings: 3,
                    runtime: 4,
                },
            )
            .into_value(),
        );
    }
    for stream in [stream::APPLICATIONS, stream::WINDOWS] {
        fixture.insert(
            stream.into(),
            crate::daemon::events::changed(stream, "subscription-example", 42).into_value(),
        );
    }
    Value::Object(fixture)
}

pub fn registry() -> serde_json::Result<Value> {
    Ok(serde_json::json!({
        "methods": methods::Method::ALL.iter().map(|method| method.registry_entry()).collect::<Vec<_>>(),
        "streams": STREAMS.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>()
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::{METHODS, STREAMS, VERSION, contract_fixture, resource_contract_fixture};
    use crate::model::{
        HistoricalResourceUsage, ResourceAvailability, ResourceHistoryPoint, ResourceUsage,
    };

    fn leaf_paths(value: &Value, prefix: &str, paths: &mut BTreeSet<String>) {
        let Some(object) = value.as_object() else {
            paths.insert(prefix.to_owned());
            return;
        };
        for (key, child) in object {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            leaf_paths(child, &path, paths);
        }
    }

    fn paths(value: &Value) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        leaf_paths(value, "", &mut paths);
        paths
    }

    fn names<'a>(fixture: &'a Value, registry: &str) -> anyhow::Result<Vec<&'a str>> {
        shelllist_daemon_core::fixture_names(fixture, registry).map_err(anyhow::Error::msg)
    }

    #[test]
    fn v1_contract_matches_registry_and_serialized_resources() -> anyhow::Result<()> {
        let fixture = contract_fixture()?;
        assert_eq!(fixture["version"], VERSION);
        assert_eq!(names(&fixture, "methods")?, METHODS);
        assert_eq!(names(&fixture, "streams")?, STREAMS);
        let fixture = resource_contract_fixture()?;
        let current = serde_json::to_value(ResourceUsage::default())?;
        let history = serde_json::to_value(ResourceHistoryPoint {
            timestamp_ms: 0,
            duration_ms: 0,
            resources: HistoricalResourceUsage {
                availability: Some(ResourceAvailability::default()),
                ..Default::default()
            },
        })?;
        assert_eq!(paths(&fixture["current"]), paths(&current));
        assert_eq!(paths(&fixture["history_point"]), paths(&history));
        assert!(fixture["current"].is_object());
        Ok(())
    }
}
