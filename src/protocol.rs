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

pub fn registry() -> serde_json::Result<Value> {
    Ok(contract_fixture()?["registry"].take())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{METHODS, STREAMS, VERSION, contract_fixture};

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
}
