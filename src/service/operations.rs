use super::{ExecuteParams, action::PreparedAction};
use crate::model::OperationResult;
use shelllist_daemon_core::{OperationLimits, OwnedOperation, OwnedOperations, RecentResults};
use std::{
    future::Future,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::{sync::oneshot, task::AbortHandle};

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
    fn spawn<F: Future<Output = ()> + Send + 'static>(
        &mut self,
        owner: Option<String>,
        result: OperationResult,
        run: impl FnOnce(OperationResult) -> F + Send + 'static,
    ) -> anyhow::Result<OperationResult> {
        let (start, ready) = oneshot::channel();
        // The framework checks duplicate IDs and quotas before creating a task.
        // This synchronous transaction runs under the service's operation lock.
        self.active.insert_with(result.id.clone(), owner, |_| {
            let running = result.clone();
            let task = tokio::spawn(async move {
                if ready.await.is_ok() {
                    run(running).await;
                }
            });
            OperationTask {
                abort: task.abort_handle(),
                result: result.clone(),
            }
        })?;
        let _ = start.send(());
        Ok(result)
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

impl super::ApplicationService {
    pub async fn execute_owned(
        self: &Arc<Self>,
        params: ExecuteParams,
        owner: Option<String>,
    ) -> anyhow::Result<OperationResult> {
        let action = self.prepare_action(params).await?;
        let accepted = action.accepted();
        let service = Arc::clone(self);
        let mut operations = self.operations.lock().await;
        anyhow::ensure!(
            !self.stopping.load(Ordering::Acquire),
            "application service is shutting down"
        );
        operations.spawn(owner, accepted, move |result| async move {
            service.run_operation(action, result).await;
        })
    }

    async fn run_operation(&self, action: PreparedAction, mut result: OperationResult) {
        result.status = "running".into();
        result.message = "Operation running".into();
        if !self.publish_running(result.clone()).await {
            return;
        }
        let lock = self.launch_lock(action.target_id());
        let _launch = lock.lock().await;
        match action.execute(self, &result.id).await {
            Ok(outcome) => outcome.apply(&mut result),
            Err(error) => {
                result.status = "failed".into();
                result.message = error.to_string();
            }
        }
        if self.operations.lock().await.finish(result.clone()) {
            let _ = self.operation_changes.send(result);
        }
    }

    pub(super) async fn publish_running(&self, result: OperationResult) -> bool {
        let mut operations = self.operations.lock().await;
        if !operations.running(result.clone()) {
            return false;
        }
        let _ = self.operation_changes.send(result);
        true
    }

    pub async fn cancel_operation_owned(
        &self,
        operation_id: &str,
        owner: Option<&str>,
    ) -> Option<OperationResult> {
        let cancelled = self.operations.lock().await.cancel(operation_id, owner)?;
        let _ = self.operation_changes.send(cancelled.clone());
        Some(cancelled)
    }

    pub async fn operation_status_owned(
        &self,
        operation_id: &str,
        owner: Option<&str>,
    ) -> Option<OperationResult> {
        self.operations.lock().await.status(operation_id, owner)
    }

    pub async fn cancel_all_operations(&self) {
        for cancelled in self.operations.lock().await.cancel_all() {
            let _ = self.operation_changes.send(cancelled);
        }
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
    async fn cancellation_is_terminal_and_admission_is_bounded() {
        let mut registry = OperationRegistry::default();
        for id in 0..MAX_ACTIVE_PER_OWNER {
            let mut value = result(id, "accepted");
            value.close = Some(crate::model::CloseObservation {
                targeted_window_ids: vec!["window-a".into()],
                dispatched_window_ids: vec!["window-a".into()],
                status: "observing".into(),
                ..Default::default()
            });
            registry
                .spawn(None, value, |_| std::future::pending())
                .unwrap();
        }
        assert!(
            registry
                .spawn(None, result(MAX_ACTIVE_PER_OWNER, "accepted"), |_| async {
                    panic!("full registry must not start work")
                })
                .is_err()
        );
        assert!(registry.active.admit(Some(":1.2")).is_ok());
        assert!(registry.cancel("operation-0", Some(":1.2")).is_none());
        let cancelled = registry.cancel("operation-0", None).unwrap();
        assert_eq!(cancelled.status, "cancelled");
        let close = cancelled.close.unwrap();
        assert_eq!(close.status, "unknown");
        assert_eq!(close.dispatched_window_ids, ["window-a"]);
        assert!(cancelled.message.contains("may still take effect"));
        assert!(!registry.running(result(0, "running")));
        assert!(!registry.finish(result(0, "completed")));
        assert_eq!(
            registry.status("operation-0", None).unwrap().status,
            "cancelled"
        );
        assert!(registry.active.admit(None).is_ok());
        assert_eq!(registry.cancel_all().len(), MAX_ACTIVE_PER_OWNER - 1);
        registry.recent.prune(std::time::Instant::now() + RETENTION);
        assert!(registry.status("operation-0", None).is_none());
    }

    #[tokio::test]
    async fn registration_rejects_duplicates_before_starting_and_cancels_queued_work() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let runs = Arc::new(AtomicUsize::new(0));
        let work = || {
            let runs = Arc::clone(&runs);
            move |_| async move {
                runs.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
        };
        let mut registry = OperationRegistry::default();
        let accepted = registry
            .spawn(Some(":1.1".into()), result(0, "accepted"), work())
            .unwrap();
        assert_eq!(registry.status(&accepted.id, Some(":1.1")), Some(accepted));
        let (started, mut receiver) = tokio::sync::oneshot::channel();
        assert!(
            registry
                .spawn(
                    Some(":1.2".into()),
                    result(0, "accepted"),
                    move |_| async move {
                        let _ = started.send(());
                    }
                )
                .is_err()
        );
        assert_eq!(
            receiver.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        );
        registry.spawn(None, result(1, "accepted"), work()).unwrap();
        assert_eq!(
            registry.cancel("operation-1", None).unwrap().status,
            "cancelled"
        );
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(registry.status("operation-0", Some(":1.2")).is_none());
        assert!(!registry.finish(result(1, "completed")));
        assert!(!registry.running(result(1, "running")));
        assert_eq!(registry.cancel_all().len(), 1);
    }
}
