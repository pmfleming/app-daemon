use crate::{
    model::{HistorySummary, OperationResult, ResourceHistoryPoint, ResourceUsage},
    protocol,
};
use serde::Deserialize;
use serde_json::{Value, json};
#[test]
fn frontend_contract_owned_types_and_partial_launch_semantics() {
    let mut f = protocol::contract_fixture().unwrap();
    let checked: Value =
        serde_json::from_str(include_str!("../test_support/app-api-v1.json")).unwrap();
    assert_eq!(
        checked, f,
        "fixtures must be checked, not silently repaired"
    );
    let operation =
        OperationResult::deserialize(&f["operation_outcome"]["partial_success_example"]).unwrap();
    assert_eq!(operation.status, "completed");
    assert_eq!(operation.launch_backend.as_deref(), Some("uwsm-app"));
    let placement = operation.placement.unwrap();
    assert_eq!(placement.status, crate::model::PlacementStatus::Unavailable);
    assert_eq!(placement.workspace_id, "3");
    assert_eq!(
        f["operation_outcome"]["placement_statuses"],
        json!(["pending", "placed", "unavailable", "failed"])
    );
    let mut resource = protocol::resource_contract_fixture().unwrap();
    ResourceUsage::deserialize(&resource["current"]).unwrap();
    ResourceHistoryPoint::deserialize(&resource["history_point"]).unwrap();
    HistorySummary::deserialize(&resource["summary"]).unwrap();
    let mut bad = f["operation_outcome"]["partial_success_example"].take();
    bad["placement"]["status"] = json!(false);
    assert!(serde_json::from_value::<OperationResult>(bad).is_err());
    let mut bad = resource["current"].take();
    bad["cpu_percent"] = json!({});
    assert!(serde_json::from_value::<ResourceUsage>(bad).is_err());
    let replies = protocol::reply_contract_fixture();
    for kind in ["subscription", "operation"] {
        assert_eq!(replies[format!("cancel-{kind}")]["data"]["kind"], kind);
    }
    assert_eq!(
        replies["subscribe"]["data"]["subscription"]["id"],
        "subscription-example"
    );
    for code in protocol::replies::ErrorCode::ALL {
        let reply = &replies["errors"][code.as_str()];
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], code.as_str());
    }
}
