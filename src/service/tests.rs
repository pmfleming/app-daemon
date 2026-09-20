use super::{ApplicationAction, ApplicationService, ExecuteParams};

#[test]
fn healthy_streams_reduce_polling_and_reconnects_force_refresh() {
    use super::*;
    use shelllist_hyprland::Event;
    assert_eq!(
        reconciliation_intervals(true, true),
        (Duration::from_secs(30), Duration::from_secs(300))
    );
    assert_eq!(
        reconciliation_intervals(false, false),
        (Duration::from_secs(5), Duration::from_secs(30))
    );
    let mut connected = false;
    assert!(observe_window_event(Event::Connected, &mut connected));
    assert!(connected);
    assert!(!observe_window_event(
        Event::Message("openlayer>>osd".into()),
        &mut connected
    ));
    assert!(observe_window_event(Event::Disconnected, &mut connected));
    assert!(!connected);
    assert!(
        observe_window_event(Event::Connected, &mut connected),
        "reconnect must refresh even without a client event"
    );
}

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

#[tokio::test]
async fn rejects_stale_actions_and_reports_accepted_operation_outcomes() -> anyhow::Result<()> {
    let service = ApplicationService::new();
    let mut events = service.subscribe_operations();
    assert!(
        service
            .execute(close_missing(Some(u64::MAX)))
            .await
            .is_err()
    );
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let accepted = service.execute(close_missing(None)).await?;
    assert_eq!(accepted.status, "accepted");
    let running = events.recv().await?;
    let failed = events.recv().await?;
    assert_eq!(running.id, accepted.id);
    assert_eq!(running.status, "running");
    assert_eq!(failed.id, accepted.id);
    assert_eq!(failed.status, "failed");
    assert_eq!(
        service.operation_status_owned(&accepted.id, None).await,
        Some(failed)
    );
    assert!(
        service
            .operation_status_owned(&accepted.id, Some(":1.2"))
            .await
            .is_none()
    );
    Ok(())
}
