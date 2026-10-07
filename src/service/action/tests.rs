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
fn workspace_confirmation_accepts_ids_and_named_workspaces_not_unrelated_state() {
    let mut window = windows().clients.remove(0);
    window.workspace.id = 3;
    window.workspace.name = "code".into();
    assert!(super::workspace_matches(&window, "3"));
    assert!(super::workspace_matches(&window, "name:code"));
    assert!(!super::workspace_matches(&window, "2"));
    assert!(!super::workspace_matches(&window, "name:media"));
    window.workspace.name = "special:scratchpad".into();
    assert!(super::workspace_matches(&window, "special:scratchpad"));
    window.workspace.name = "2".into();
    assert!(
        !super::workspace_matches(&window, "2"),
        "numeric selectors refer to IDs, not renamed workspaces"
    );
    assert!(super::workspace_matches(&window, "name:2"));
}

#[test]
fn placement_requires_one_new_owned_window() {
    let windows = windows();
    let previous = ["0x1"];
    assert_eq!(
        correlated_window(windows.clone(), &previous, |window| window.pid != 3)
            .map(|window| window.address)
            .as_deref(),
        Some("0x2")
    );
    assert!(correlated_window(windows.clone(), &previous, |_| false).is_none());
    assert!(
        correlated_window(windows, &previous, |_| true).is_none(),
        "ambiguous launch must not move any window"
    );
}

#[test]
fn post_handoff_errors_preserve_receipts_and_successful_placement() -> anyhow::Result<()> {
    use super::{ActionOutcome, Catalog, ExecuteParams, PlacementStatus, WorkspacePlacement};
    use crate::launch::{LaunchBackend, LaunchReceipt};
    use anyhow::Context;

    let params: ExecuteParams = serde_json::from_value(serde_json::json!({
        "target_id": "app.desktop", "action": "launch"
    }))?;
    for (initial, status, expected) in [
        (None, "failed", None),
        (
            Some(PlacementStatus::Pending),
            "completed",
            Some(PlacementStatus::Failed),
        ),
        (
            Some(PlacementStatus::Placed),
            "failed",
            Some(PlacementStatus::Placed),
        ),
    ] {
        let mut receipt = LaunchReceipt::from(LaunchBackend::Systemd);
        receipt.placement = initial.map(|status| WorkspacePlacement {
            workspace_id: "3".into(),
            status,
            reason: None,
        });
        let outcome = ActionOutcome::launched(
            &Catalog::default(),
            &params,
            receipt,
            Err(anyhow::anyhow!("compositor rejected request")),
            true,
        );
        assert_eq!(outcome.status, status);
        assert!(outcome.message.contains("already started"));
        let receipt = outcome.launch.context("successful handoff receipt")?;
        assert_eq!(receipt.backend, "systemd-run");
        assert_eq!(receipt.placement.as_ref().map(|p| p.status), expected);
        let reason = receipt.placement.as_ref().and_then(|p| p.reason.as_deref());
        assert_eq!(
            reason,
            (expected == Some(PlacementStatus::Failed)).then_some("compositor rejected request")
        );
    }
    Ok(())
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
    let mut receipt = LaunchReceipt::from(LaunchBackend::Systemd);
    receipt.unit = Some("private-launch.service".into());
    receipt.remember_process(std::process::id());
    receipt.placement = Some(super::WorkspacePlacement {
        workspace_id: "3".into(),
        status: super::PlacementStatus::Pending,
        reason: None,
    });
    progress.handed_off(&params, &receipt).await;
    let event = events.try_recv().unwrap();
    assert_eq!(event.placement, receipt.placement);
    assert!(receipt.owns_process(std::process::id()));
    let wire = serde_json::to_value(&event).unwrap();
    assert!(wire.get("unit").is_none() && wire.get("provenance").is_none());
    assert_eq!(event.status, "running");
    assert_eq!(event.launch_backend.as_deref(), Some("systemd-run"));
    let recovered = service
        .operation_status_owned("handoff", owner)
        .await
        .unwrap();
    assert_eq!(recovered, event);
    receipt.placement.as_mut().unwrap().status = super::PlacementStatus::Placed;
    assert_eq!(
        event.placement.unwrap().status,
        super::PlacementStatus::Pending
    );
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
