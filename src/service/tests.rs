use super::{ApplicationAction, ApplicationService, ExecuteParams};

#[tokio::test]
async fn query_waiting_for_settings_does_not_block_resource_publication() {
    use futures::{pin_mut, poll};

    let service = ApplicationService::build(false);
    let settings_update = service.settings.write().await;
    let query = service.query(serde_json::from_str("{}").unwrap());
    pin_mut!(query);
    assert!(poll!(&mut query).is_pending());

    // A query blocked on settings must not retain a resources guard. Otherwise
    // a queued sampler write and a revision reader holding settings can form
    // a cycle with the next settings update.
    let resource_publication = service
        .resources
        .try_write()
        .expect("query must acquire settings before resources, just like revision and execute");
    drop(resource_publication);
    drop(settings_update);
    tokio::time::timeout(std::time::Duration::from_secs(1), query)
        .await
        .expect("query should finish after the settings update");
}

pub(super) fn close_missing(expected_revision: Option<u64>) -> ExecuteParams {
    ExecuteParams {
        target_id: "missing-window-group".into(),
        action: ApplicationAction::Close,
        window_id: None,
        desktop_action_id: None,
        expected_revision,
        workspace_id: None,
    }
}
