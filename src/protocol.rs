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
    "applications.refresh",
    "applications.execute",
];
pub const STREAMS: &[&str] = &[stream::APPLICATIONS, stream::WINDOWS, stream::OPERATION];

pub fn contract_fixture() -> Value {
    serde_json::from_str(include_str!("../test_support/app-api-v1.json"))
        .expect("app-api fixture is valid")
}

pub fn registry() -> Value {
    contract_fixture()["registry"].take()
}

#[cfg(test)]
mod tests {
    use super::{METHODS, STREAMS, VERSION, contract_fixture};

    #[test]
    fn fixture_matches_registry() {
        let fixture = contract_fixture();
        assert_eq!(fixture["version"], VERSION);
        let methods: Vec<_> = fixture["registry"]["methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect();
        let streams: Vec<_> = fixture["registry"]["streams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["name"].as_str().unwrap())
            .collect();
        assert_eq!(methods, METHODS);
        assert_eq!(streams, STREAMS);
    }
}
