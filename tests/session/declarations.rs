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
