use super::{ApplicationAction, ApplicationService, ExecuteParams};

fn close_missing(expected_revision: Option<u64>) -> ExecuteParams {
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
    Ok(())
}
