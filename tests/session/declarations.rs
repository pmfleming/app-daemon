use anyhow::{Context, Result};
use app_daemon::protocol::{self, methods::Method};
use serde_json::{Value, json};

use super::support::{Session, raw_call};

#[tokio::test]
async fn every_declared_method_reaches_its_typed_handler_on_the_private_bus() -> Result<()> {
    let mut session = Session::start(true).await?;
    let proxy = session.proxy().await?;
    let registry = protocol::registry()?;
    let frozen: Value = serde_json::from_str(include_str!("../../test_support/app-api-v1.json"))?;
    assert_eq!(registry, frozen["registry"]);
    assert_eq!(protocol::contract_fixture()?, frozen);
    let events = protocol::event_contract_fixture();
    assert_eq!(events["gap"]["event"], "gap");
    assert_eq!(events["gap"]["stream"], protocol::stream::OPERATION);
    assert_eq!(events["gap"]["data"]["dropped_events"], "4");
    assert_eq!(events["gap"]["subscription_id"], "subscription-example");
    assert!(events["gap"].get("dropped_events").is_none());
    // The producer fixture feeds full-width hashes through the live constructors.
    // Every revision on the wire must survive a JavaScript Number round trip.
    for event in events.as_object().unwrap().values() {
        for (key, value) in event["data"].as_object().unwrap() {
            if key.ends_with("revision") {
                let revision = value.as_u64().context("numeric revision")?;
                assert!(revision < (1_u64 << 53), "{key}: {revision}");
                assert_eq!(revision as f64 as u64, revision);
            }
        }
    }
    shelllist_daemon_core::validate_unique_names(protocol::METHODS).map_err(anyhow::Error::msg)?;
    let mut operation_id = String::new();
    for entry in registry["methods"].as_array().context("method registry")? {
        let name = entry["name"].as_str().context("wire name")?;
        let method = Method::parse(name).context("declared method")?;
        let mut params = entry["params_example"].clone();
        if params.get("target_id").is_some() {
            params["target_id"] = json!("ok.desktop");
        }
        if method == Method::Execute {
            // Only the temporary fake launcher, never the desktop or placement effects.
            params["action"] = json!("launch");
            params["expected_revision"] = Value::Null;
            params["workspace_id"] = Value::Null;
        }
        if method == Method::OperationStatus {
            params["operation_id"] = json!(operation_id);
        }
        let response = raw_call(&proxy, name, &params.to_string()).await?;
        assert_eq!(response["protocol"], protocol::NAME);
        assert_eq!(response["version"], protocol::VERSION);
        assert_eq!(response["ok"], true, "{name}: {response}");
        let body = response["data"].as_object().context("reply body")?;
        assert_eq!(body.len(), 1);
        assert!(
            body.contains_key(method.response_key()),
            "{name}: {response}"
        );
        if method == Method::Execute {
            operation_id = body[method.response_key()]["id"]
                .as_str()
                .context("accepted operation")?
                .into();
        }
    }
    drop(proxy);
    session.shutdown().await
}
