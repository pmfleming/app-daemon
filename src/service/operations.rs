use crate::model::OperationResult;
use shelllist_daemon_core::{OperationLimits, OwnedOperation, OwnedOperations, RecentResults};
use std::time::Duration;
use tokio::task::AbortHandle;

const MAX_ACTIVE: usize = 128;
const MAX_ACTIVE_PER_OWNER: usize = 32;
const MAX_RECENT: usize = 256;
const RETENTION: Duration = Duration::from_secs(15 * 60);

struct OperationTask {
    abort: AbortHandle,
    result: OperationResult,
}

pub(super) struct OperationRegistry {
    active: OwnedOperations<OperationTask>,
    recent: RecentResults<OperationResult>,
}
impl Default for OperationRegistry {
    fn default() -> Self {
        Self {
            active: OwnedOperations::new(OperationLimits {
                total: MAX_ACTIVE,
                per_owner: MAX_ACTIVE_PER_OWNER,
            }),
            recent: RecentResults::new(MAX_RECENT, Some(RETENTION)),
        }
    }
}
impl OperationRegistry {
    pub fn admit(&self, owner: Option<&str>) -> anyhow::Result<()> {
        self.active.admit(owner).map_err(Into::into)
    }

    pub fn insert(
        &mut self,
        owner: Option<String>,
        abort: AbortHandle,
        result: OperationResult,
    ) -> anyhow::Result<()> {
        let task = OperationTask {
            abort: abort.clone(),
            result,
        };
        self.active
            .insert(task.result.id.clone(), owner, task)
            .inspect_err(|_| abort.abort())
            .map_err(Into::into)
    }

    pub fn running(&mut self, result: OperationResult) -> bool {
        let Some(active) = self.active.get_mut(&result.id) else {
            return false;
        };
        active.result = result;
        true
    }

    pub fn finish(&mut self, result: OperationResult) -> bool {
        let Some(active) = self.active.claim(&result.id) else {
            return false;
        };
        self.recent.record(result.id.clone(), active.owner, result);
        true
    }

    pub fn cancel(&mut self, id: &str, owner: Option<&str>) -> Option<OperationResult> {
        let active = self.active.claim_owned(id, owner)?;
        Some(record_cancelled(&mut self.recent, active))
    }

    pub fn cancel_all(&mut self) -> Vec<OperationResult> {
        self.active
            .drain()
            .map(|active| record_cancelled(&mut self.recent, active))
            .collect()
    }

    pub fn status(&mut self, id: &str, owner: Option<&str>) -> Option<OperationResult> {
        self.active
            .get_owned(id, owner)
            .map(|active| active.result.clone())
            .or_else(|| self.recent.get_owned(id, owner).cloned())
    }
}

fn record_cancelled(
    recent: &mut RecentResults<OperationResult>,
    active: OwnedOperation<OperationTask>,
) -> OperationResult {
    active.value.abort.abort();
    let mut result = active.value.result;
    result.status = "cancelled".into();
    result.message = "Operation cancelled".into();
    if let Some(close) = &mut result.close {
        close.status = "unknown".into();
        result.message = "Close observation cancelled; dispatched requests may still take effect. Check windows before retrying.".into();
    }
    recent.record(result.id.clone(), active.owner, result.clone());
    result
}

#[cfg(test)]
mod tests {
    use super::{MAX_ACTIVE_PER_OWNER, OperationRegistry, OperationResult, RETENTION};
    use crate::service::{ApplicationAction, ExecuteParams, action::operation_result};
    fn result(id: usize, status: &str) -> OperationResult {
        operation_result(
            format!("operation-{id}"),
            &ExecuteParams {
                target_id: "app".into(),
                action: ApplicationAction::Launch,
                window_id: None,
                desktop_action_id: None,
                expected_revision: None,
                workspace_id: None,
            },
            status,
            status.into(),
            None,
        )
    }
    #[tokio::test]
    async fn close_cancellation_retains_targets_and_never_claims_rollback() {
        let mut registry = OperationRegistry::default();
        let task = tokio::spawn(std::future::pending::<()>());
        let mut value = result(0, "running");
        value.close = Some(crate::model::CloseObservation {
            targeted_window_ids: vec!["window-a".into()],
            dispatched_window_ids: vec!["window-a".into()],
            status: "observing".into(),
            ..Default::default()
        });
        registry.insert(None, task.abort_handle(), value).unwrap();
        let cancelled = registry.cancel("operation-0", None).unwrap();
        assert_eq!(cancelled.close.as_ref().unwrap().status, "unknown");
        assert_eq!(cancelled.close.unwrap().dispatched_window_ids, ["window-a"]);
        assert!(cancelled.message.contains("may still take effect"));
        assert!(!registry.finish(result(0, "completed")));
    }

    #[tokio::test]
    async fn cancellation_is_terminal_and_admission_is_bounded() {
        let mut registry = OperationRegistry::default();
        for id in 0..MAX_ACTIVE_PER_OWNER {
            let task = tokio::spawn(std::future::pending::<()>());
            registry
                .insert(None, task.abort_handle(), result(id, "accepted"))
                .unwrap();
        }
        assert!(registry.admit(None).is_err());
        assert!(registry.admit(Some(":1.2")).is_ok());
        assert!(registry.cancel("operation-0", Some(":1.2")).is_none());
        assert_eq!(
            registry.cancel("operation-0", None).unwrap().status,
            "cancelled"
        );
        assert!(!registry.running(result(0, "running")));
        assert!(!registry.finish(result(0, "completed")));
        assert_eq!(
            registry.status("operation-0", None).unwrap().status,
            "cancelled"
        );
        assert!(registry.admit(None).is_ok());
        assert_eq!(registry.cancel_all().len(), MAX_ACTIVE_PER_OWNER - 1);
        registry.recent.prune(std::time::Instant::now() + RETENTION);
        assert!(registry.status("operation-0", None).is_none());
    }
}
