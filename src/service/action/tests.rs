use super::{Client, Snapshot, correlated_window};

fn windows() -> Snapshot {
    Snapshot {
        available: true,
        revision: 1,
        clients: [1, 2, 3]
            .into_iter()
            .map(|pid| Client {
                address: format!("0x{pid}"),
                class: "app".into(),
                initial_class: "app".into(),
                title: String::new(),
                pid,
                workspace: Default::default(),
                focus_rank: 0,
                mapped: true,
            })
            .collect(),
    }
}

#[test]
fn placement_requires_one_new_owned_window() {
    let windows = windows();
    let previous = vec!["0x1".into()];
    assert_eq!(
        correlated_window(&windows, &previous, |window| window.pid != 3).as_deref(),
        Some("0x2")
    );
    assert!(correlated_window(&windows, &previous, |_| false).is_none());
    assert!(
        correlated_window(&windows, &previous, |_| true).is_none(),
        "ambiguous launch must not move any window"
    );
}

#[tokio::test]
async fn checked_launch_receipt_is_recoverable_and_cannot_revive_cancelled_operations() {
    use super::super::{ApplicationService, ExecuteParams};
    use super::{ApplicationAction, LaunchProgress, operation_result};
    use crate::launch::{LaunchBackend, LaunchReceipt};

    let service = ApplicationService::new();
    let params = ExecuteParams {
        target_id: "app.desktop".into(),
        action: ApplicationAction::Launch,
        window_id: None,
        desktop_action_id: None,
        expected_revision: None,
        workspace_id: None,
    };
    let owner = Some(":test");
    let mut events = service.subscribe_operations();
    let task = tokio::spawn(std::future::pending::<()>());
    service
        .operations
        .lock()
        .await
        .insert(
            owner.map(str::to_owned),
            task.abort_handle(),
            operation_result(
                "handoff".into(),
                &params,
                "accepted",
                "Accepted".into(),
                None,
            ),
        )
        .unwrap();
    let progress = LaunchProgress {
        service: &service,
        operation_id: "handoff",
    };
    let receipt = LaunchReceipt::from(LaunchBackend::Systemd);
    progress.handed_off(&params, &receipt).await;
    let event = events.try_recv().unwrap();
    assert_eq!(event.status, "running");
    assert_eq!(event.launch_backend.as_deref(), Some("systemd-run"));
    let recovered = service
        .operation_status_owned("handoff", owner)
        .await
        .unwrap();
    assert_eq!(recovered.launch_scope, event.launch_scope);
    assert!(
        service
            .operation_status_owned("handoff", Some(":other"))
            .await
            .is_none()
    );
    service
        .cancel_operation_owned("handoff", owner)
        .await
        .unwrap();
    assert_eq!(events.try_recv().unwrap().status, "cancelled");
    progress.handed_off(&params, &receipt).await;
    assert!(events.try_recv().is_err());
    assert_eq!(
        service
            .operation_status_owned("handoff", owner)
            .await
            .unwrap()
            .status,
        "cancelled"
    );
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn launches_for_one_target_share_a_cancellation_safe_lock() {
    let service = crate::service::ApplicationService::new();
    let first = service.launch_lock("app.desktop");
    let second = service.launch_lock("app.desktop");
    let other = service.launch_lock("other.desktop");
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    let guard = first.lock().await;
    assert!(second.try_lock().is_err());
    assert!(other.try_lock().is_ok());
    drop(guard);
    assert!(second.try_lock().is_ok());
}
