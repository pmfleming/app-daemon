use crate::{
    model::{HistorySummary, OperationResult, ResourceHistoryPoint, ResourceUsage},
    protocol,
};
use serde_json::{Value, json};
#[test]
fn frontend_contract_owned_types_and_partial_launch_semantics() {
    let f = protocol::contract_fixture().unwrap();
    let operation: OperationResult =
        serde_json::from_value(f["operation_outcome"]["partial_success_example"].clone()).unwrap();
    assert_eq!(operation.status, "completed");
    assert_eq!(operation.launch_backend.as_deref(), Some("uwsm-app"));
    let placement = operation.placement.unwrap();
    assert_eq!(placement.status, crate::model::PlacementStatus::Unavailable);
    assert_eq!(placement.workspace_id, "3");
    assert_eq!(
        f["operation_outcome"]["placement_statuses"],
        json!(["pending", "placed", "unavailable", "failed"])
    );
    assert!(
        f["registry"]["methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "applications.history"
                && m["pagination"]["kind"] == "opaque-cursor")
    );
    assert!(
        f["registry"]["streams"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "applications.operation")
    );
    let resource = protocol::resource_contract_fixture().unwrap();
    let _: ResourceUsage = serde_json::from_value(resource["current"].clone()).unwrap();
    let _: ResourceHistoryPoint =
        serde_json::from_value(resource["history_point"].clone()).unwrap();
    let _: HistorySummary = serde_json::from_value(resource["summary"].clone()).unwrap();
    let mut bad = f["operation_outcome"]["partial_success_example"].clone();
    bad["placement"]["status"] = json!(false);
    assert!(serde_json::from_value::<OperationResult>(bad).is_err());
    let mut bad = resource["current"].clone();
    bad["cpu_percent"] = json!({});
    assert!(serde_json::from_value::<ResourceUsage>(bad).is_err());
}
#[test]
fn frontend_contract_fixture_is_checked_not_silently_repaired() {
    let checked: Value =
        serde_json::from_str(include_str!("../test_support/app-api-v1.json")).unwrap();
    assert_eq!(checked, protocol::contract_fixture().unwrap());
}
