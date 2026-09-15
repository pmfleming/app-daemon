use crate::model::OperationResult;
use shelllist_daemon_core::{OperationLimits, OwnedOperations, RecentResults};
use std::time::Duration;
use tokio::task::AbortHandle;

const MAX_ACTIVE: usize = 128;
const MAX_ACTIVE_PER_OWNER: usize = 32;
const MAX_RECENT: usize = 256;
const RETENTION: Duration = Duration::from_secs(15 * 60);

pub(super) struct ActiveOperation {
    pub abort: AbortHandle,
    pub result: OperationResult,
    pub owner: Option<String>,
}
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

    pub fn insert(&mut self, id: String, active: ActiveOperation) -> anyhow::Result<()> {
        let abort = active.abort.clone();
        self.active
            .insert(
                id,
                active.owner,
                OperationTask {
                    abort: active.abort,
                    result: active.result,
                },
            )
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
        active.value.abort.abort();
        let mut result = active.value.result;
        result.status = "cancelled".into();
        result.message = "Operation cancelled".into();
        self.recent
            .record(result.id.clone(), active.owner, result.clone());
        Some(result)
    }

    pub fn cancel_all(&mut self) -> Vec<OperationResult> {
        let active = self
            .active
            .iter()
            .map(|(id, active)| (id.clone(), active.owner.clone()))
            .collect::<Vec<_>>();
        active
            .into_iter()
            .filter_map(|(id, owner)| self.cancel(&id, owner.as_deref()))
            .collect()
    }

    pub fn status(&mut self, id: &str, owner: Option<&str>) -> Option<OperationResult> {
        self.active
            .get_owned(id, owner)
            .map(|active| active.result.clone())
            .or_else(|| self.recent.get_owned(id, owner).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn outcomes_are_owner_scoped_bounded_and_expire() {
        let mut registry = OperationRegistry::default();
        for id in 0..MAX_RECENT + 1 {
            let task = tokio::spawn(async {});
            registry
                .insert(
                    format!("operation-{id}"),
                    ActiveOperation {
                        abort: task.abort_handle(),
                        result: result(id, "accepted"),
                        owner: Some(":1.1".into()),
                    },
                )
                .unwrap();
            assert!(registry.finish(result(id, "completed")));
        }
        assert!(registry.status("operation-0", Some(":1.1")).is_none());
        assert!(registry.status("operation-1", Some(":1.2")).is_none());
        assert_eq!(
            registry.status("operation-1", Some(":1.1")).unwrap().status,
            "completed"
        );
        registry.recent.prune(std::time::Instant::now() + RETENTION);
        assert!(registry.recent.is_empty());
    }
    #[tokio::test]
    async fn cancellation_is_terminal_and_admission_is_bounded() {
        let mut registry = OperationRegistry::default();
        for id in 0..MAX_ACTIVE_PER_OWNER {
            let task = tokio::spawn(std::future::pending::<()>());
            registry
                .insert(
                    format!("operation-{id}"),
                    ActiveOperation {
                        abort: task.abort_handle(),
                        result: result(id, "accepted"),
                        owner: None,
                    },
                )
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
    }
}
