use serde_json::Value;

pub const NAME: &str = "app-api";
pub const VERSION: u8 = 1;

pub mod stream {
    pub const APPLICATIONS: &str = "applications.changed";
    pub const WINDOWS: &str = "windows.changed";
    pub const OPERATION: &str = "applications.operation";
}

pub const METHODS: &[&str] = &[
    "applications.query",
    "applications.revision",
    "applications.history",
    "applications.energyOverview",
    "applications.refresh",
    "applications.execute",
    "applications.settings.update",
];
pub const STREAMS: &[&str] = &[stream::APPLICATIONS, stream::WINDOWS, stream::OPERATION];

pub fn contract_fixture() -> serde_json::Result<Value> {
    shelllist_daemon_core::load_fixture(include_str!("../test_support/app-api-v1.json"))
}

/// Canonical serialized resource shapes consumed by Shelllist presentation.
pub fn resource_contract_fixture() -> serde_json::Result<Value> {
    shelllist_daemon_core::load_fixture(include_str!("../test_support/app-resource-v1.json"))
}

pub fn registry() -> serde_json::Result<Value> {
    Ok(contract_fixture()?["registry"].take())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::{METHODS, STREAMS, VERSION, contract_fixture, resource_contract_fixture};
    use crate::model::{HistoricalResourceUsage, ResourceHistoryPoint, ResourceUsage};

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

    fn names<'a>(fixture: &'a Value, registry: &str) -> Vec<&'a str> {
        shelllist_daemon_core::fixture_names(fixture, registry).expect("fixture registry")
    }

    #[test]
    fn fixture_matches_registry() -> serde_json::Result<()> {
        let fixture = contract_fixture()?;
        assert_eq!(fixture["version"], VERSION);
        assert_eq!(names(&fixture, "methods"), METHODS);
        assert_eq!(names(&fixture, "streams"), STREAMS);
        Ok(())
    }

    #[test]
    fn resource_fixture_matches_serialized_domain_shapes() -> serde_json::Result<()> {
        let fixture = resource_contract_fixture()?;
        let current = serde_json::to_value(ResourceUsage::default())?;
        let history = serde_json::to_value(ResourceHistoryPoint {
            timestamp_ms: 0,
            duration_ms: 0,
            resources: HistoricalResourceUsage::default(),
        })?;
        assert_eq!(paths(&fixture["current"]), paths(&current));
        assert_eq!(paths(&fixture["history_point"]), paths(&history));
        assert!(fixture["current"].is_object());
        Ok(())
    }
}
