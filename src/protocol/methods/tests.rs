use super::*;

#[test]
fn generated_registry_and_reply_keys_match_the_frozen_v1_contract() {
    // Read the frozen file independently: both public debug commands now use
    // the generated registry, so comparing those commands would be tautological.
    let baseline: Value =
        serde_json::from_str(include_str!("../../../test_support/app-api-v1.json")).unwrap();
    let registry = crate::protocol::registry().unwrap();
    assert_eq!(registry, baseline["registry"]);
    assert_eq!(crate::protocol::contract_fixture().unwrap(), baseline);
    shelllist_daemon_core::validate_unique_names(METHODS).unwrap();
    assert_eq!(Method::ALL.len(), METHODS.len());
    let replies = crate::protocol::reply_contract_fixture();
    for (method, name) in Method::ALL.iter().zip(METHODS) {
        assert_eq!(method.name(), *name);
        assert_eq!(Method::parse(name), Some(*method));
        let entry = method.registry_entry();
        assert!(
            Request::decode(name, entry["params_example"].clone()).is_ok(),
            "{name}"
        );
        let body = replies[name]["data"].as_object().unwrap();
        assert_eq!(body.len(), 1);
        assert!(body.contains_key(method.response_key()), "{name}");
    }
    assert_eq!(Method::Execute.policy(), ExecutionPolicy::AcceptedOperation);
    assert_eq!(Method::Refresh.policy(), ExecutionPolicy::Refresh);
    assert_eq!(
        Method::UpdateSettings.policy(),
        ExecutionPolicy::SettingsMutation
    );
    for method in [
        Method::Query,
        Method::Revision,
        Method::History,
        Method::EnergyOverview,
        Method::OperationStatus,
    ] {
        assert_eq!(method.policy(), ExecutionPolicy::Read);
    }
}

#[test]
fn malformed_requests_keep_the_original_serde_type_names_and_errors() {
    for (method, expected_type) in [
        (Method::Query, "QueryParams"),
        (Method::Refresh, "QueryParams"),
        (Method::History, "ResourceHistoryParams"),
        (Method::EnergyOverview, "EnergyOverviewParams"),
        (Method::Execute, "ExecuteParams"),
        (Method::OperationStatus, "OperationStatusParams"),
        (Method::UpdateSettings, "UpdateSettingsParams"),
    ] {
        let (code, message) = Request::decode(method.name(), Value::Null).err().unwrap();
        assert_eq!(code.as_str(), "validation-error");
        assert_eq!(
            message,
            format!("invalid type: null, expected struct {expected_type}")
        );
    }
    for name in [
        "unknown.method",
        "Applications.query",
        " applications.query",
        "subscribe",
    ] {
        assert!(Method::parse(name).is_none());
        let (code, message) = Request::decode(name, Value::Null).err().unwrap();
        assert_eq!(code.as_str(), "unsupported-method");
        assert_eq!(message, format!("Unsupported app-api method: {name}"));
    }
}

#[test]
fn permissive_revision_and_parameter_defaults_are_not_tightened() {
    for value in [
        Value::Null,
        json!(true),
        json!(17),
        json!("ignored"),
        json!([]),
        json!({"extra": [1, 2]}),
    ] {
        assert!(matches!(
            Request::decode(Method::Revision.name(), value),
            Ok(Request::Revision(_))
        ));
    }
    let Request::Query(query) =
        Request::decode(Method::Query.name(), json!({"extra":true})).unwrap()
    else {
        panic!("wrong decoder")
    };
    assert_eq!(
        (
            query.query.as_str(),
            query.category.as_str(),
            query.generation,
            query.limit
        ),
        ("", "", 0, 500)
    );
    let Request::History(history) = Request::decode(
        Method::History.name(),
        json!({"target_id":"example", "extra":true}),
    )
    .unwrap() else {
        panic!("wrong decoder")
    };
    assert_eq!(
        (
            history.since_ms,
            history.until_ms,
            history.cursor,
            history.limit
        ),
        (None, None, None, 1000)
    );
    let Request::EnergyOverview(energy) =
        Request::decode(Method::EnergyOverview.name(), json!({"since_ms":0})).unwrap()
    else {
        panic!("wrong decoder")
    };
    assert_eq!(energy.limit, 20);
    let Request::Execute(execute) = Request::decode(
        Method::Execute.name(),
        json!({"target_id":"example", "action":"activate", "extra":true}),
    )
    .unwrap() else {
        panic!("wrong decoder")
    };
    assert_eq!(execute.action, ApplicationAction::Activate);
    assert_eq!(
        (
            execute.window_id,
            execute.desktop_action_id,
            execute.expected_revision,
            execute.workspace_id
        ),
        (None, None, None, None)
    );
}
