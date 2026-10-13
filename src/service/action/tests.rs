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
