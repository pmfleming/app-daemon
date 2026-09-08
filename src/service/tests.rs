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
async fn accepts_operations_before_reporting_their_result() -> anyhow::Result<()> {
    let service = ApplicationService::new();
    let mut events = service.subscribe_operations();
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

#[tokio::test]
async fn rejects_operations_for_stale_revisions() {
    let service = ApplicationService::new();
    assert!(
        service
            .execute(close_missing(Some(u64::MAX)))
            .await
            .is_err()
    );
}
