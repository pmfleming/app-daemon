use serde_json::Value;

mod fixtures;
pub mod methods;
pub mod replies;

pub use methods::METHODS;

pub const NAME: &str = "app-api";
pub const VERSION: u8 = shelllist_daemon_core::CONTRACT_VERSION as u8;

pub mod stream {
    pub const APPLICATIONS: &str = "applications.changed";
    pub const WINDOWS: &str = "windows.changed";
    pub const OPERATION: &str = "applications.operation";
}

pub const STREAMS: &[&str] = &[stream::APPLICATIONS, stream::WINDOWS, stream::OPERATION];

pub fn contract_fixture() -> serde_json::Result<Value> {
    let mut fixture =
        shelllist_daemon_core::load_fixture(include_str!("../test_support/app-api-v1.json"))?;
    fixture["version"] = serde_json::json!(VERSION);
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
                    // Exercise real 64-bit hashes through the wire constructors,
                    // not tiny examples that hide JavaScript precision failures.
                    catalog: u64::MAX,
                    windows: 1_u64 << 63,
                    settings: (1_u64 << 53) + 1,
                    runtime: u64::MAX - 7,
                },
            )
            .into_value(),
        );
    }
    for stream in [stream::APPLICATIONS, stream::WINDOWS] {
        fixture.insert(
            stream.into(),
            crate::daemon::events::changed(stream, "subscription-example", u64::MAX - 42)
                .into_value(),
        );
    }
    Value::Object(fixture)
}

pub fn registry() -> serde_json::Result<Value> {
    Ok(serde_json::json!({
        "methods": methods::Method::ALL.iter().map(|method| method.registry_entry()).collect::<Vec<_>>(),
        "streams": STREAMS.iter().map(|name| crate::daemon::events::registry(name)).collect::<Vec<_>>()
    }))
}
