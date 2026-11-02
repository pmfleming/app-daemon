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

    pub fn running(&mut self, mut result: OperationResult) -> Option<OperationResult> {
        let active = self.active.get_mut(&result.id)?;
        result.revision = active.result.revision + 1;
        active.result = result.clone();
        Some(result)
    }

    pub fn finish(&mut self, mut result: OperationResult) -> Option<OperationResult> {
        let active = self.active.claim(&result.id)?;
        result.revision = active.value.result.revision + 1;
        self.recent
            .record(result.id.clone(), active.owner, result.clone());
        Some(result)
    }

    pub fn cancel(&mut self, id: &str, owner: Option<&str>) -> Option<OperationResult> {
        let active = self.active.claim_owned(id, owner)?;
        Some(record_cancelled(&mut self.recent, active))
    }

    pub fn cancel_owner(&mut self, owner: &str) -> Vec<OperationResult> {
        let ids: Vec<_> = self
            .active
            .iter()
            .filter(|(_, operation)| operation.owner.as_deref() == Some(owner))
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.cancel(&id, Some(owner)))
            .collect()
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
        let request = serde_json::to_value(&params)?;
        let launchable = matches!(
            params.action,
            super::ApplicationAction::Activate
                | super::ApplicationAction::Launch
                | super::ApplicationAction::DesktopAction
        );
        let durable = launchable
            && self
                .catalog
                .read()
                .await
                .by_id(&params.target_id)
                .is_some_and(|entry| !entry.launch_only)
            && std::env::var("SHELLLIST_TEMPORARY_WORKSPACES").as_deref() != Ok("0");
        let key = params
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Some(key) = &params.request_id {
            anyhow::ensure!(
                key.len() <= 118 && shelllist_daemon_core::workspace_wire::valid_request_id(key),
                "invalid launch request key"
            );
        }
        let mut accepted = super::action::operation_result(
            format!("operation-{key}"),
            &params,
            "accepted",
            "Operation accepted".into(),
            None,
        );
        if launchable
            && params.request_id.is_some()
            && let Some(result) = self.launch_journal.lock().await.lookup(
                &accepted.id,
                owner.as_deref(),
                Some(&request),
            )?
        {
            return Ok(result);
        }
        if durable {
            let mut journal = self.launch_journal.lock().await;
            if let Some(result) = journal.lookup(&accepted.id, owner.as_deref(), Some(&request))? {
                return Ok(result);
            }
            journal
                .admit(owner.clone(), request, accepted.clone())
                .await?;
        }
        // The request key exists before asynchronous context/capability work.
        // Even a lost preparation error is recoverable without replaying launch.
        let action = match self.prepare_action(params).await {
            Ok(action) => action,
            Err(error) if durable => {
                accepted.status = "failed".into();
                accepted.message = error.to_string();
                accepted.revision += 1;
                self.launch_journal.lock().await.update(&accepted).await?;
                return Ok(accepted);
            }
            Err(error) => return Err(error),
        };
        let service = Arc::clone(self);
        let mut operations = self.operations.lock().await;
        anyhow::ensure!(
            !self.stopping.load(Ordering::Acquire),
            "application service is shutting down"
        );
        let result = operations.spawn(owner, accepted.clone(), move |result| async move {
            service.run_operation(action, result).await;
        });
        drop(operations);
        if let Err(error) = &result
            && durable
        {
            accepted.status = "failed".into();
            accepted.message = error.to_string();
            accepted.revision += 1;
            self.launch_journal.lock().await.update(&accepted).await?;
        }
        result
    }

    async fn run_operation(&self, action: PreparedAction, mut result: OperationResult) {
        result.status = "running".into();
        result.message = "Operation running".into();
        if !self.publish_running(result.clone()).await {
            result.status = "failed".into();
            result.message =
                "Launch progress could not be persisted; no launch was attempted".into();
            if let Some(result) = self.operations.lock().await.finish(result) {
                let _ = self.operation_changes.send(result);
            }
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
        let _ = self.launch_journal.lock().await.update(&result).await;
        if let Some(result) = self.operations.lock().await.finish(result) {
            let _ = self.operation_changes.send(result);
        }
    }

    pub(super) async fn publish_running(&self, result: OperationResult) -> bool {
        if let Err(error) = self.launch_journal.lock().await.update(&result).await {
            tracing::error!(%error, "cannot persist launch progress");
            return false;
        }
        let mut operations = self.operations.lock().await;
        let Some(result) = operations.running(result) else {
            return false;
        };
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
        let active = self.operations.lock().await.status(operation_id, owner);
        if active.is_some() {
            return active;
        }
        self.launch_journal
            .lock()
            .await
            .lookup(operation_id, owner, None)
            .ok()
            .flatten()
    }

    pub(crate) async fn recover_launches(&self) {
        if let Err(error) = self.launch_journal.lock().await.recover().await {
            tracing::warn!(%error, "launch recovery unavailable; no effects replayed");
        }
    }

    pub async fn cancel_owner_operations(&self, owner: &str) {
        for cancelled in self.operations.lock().await.cancel_owner(owner) {
            let _ = self.operation_changes.send(cancelled);
        }
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
    result.revision += 1;
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
                launch_target: Default::default(),
                request_id: None,
            },
            status,
            status.into(),
            None,
        )
    }
    #[tokio::test]
    async fn admission_precedes_execution_and_cancellation_is_terminal() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let runs = Arc::new(AtomicUsize::new(0));
        let mut registry = OperationRegistry::default();
        for id in 0..MAX_ACTIVE_PER_OWNER {
            let mut value = result(id, "accepted");
            value.close = Some(crate::model::CloseObservation {
                targeted_window_ids: vec!["window-a".into()],
                dispatched_window_ids: vec!["window-a".into()],
                status: "observing".into(),
                ..Default::default()
            });
            let runs = Arc::clone(&runs);
            let accepted = registry
                .spawn(None, value, move |_| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                })
                .unwrap();
            assert_eq!(registry.status(&accepted.id, None), Some(accepted));
        }
        for (owner, id) in [(Some(":1.2"), 0), (None, MAX_ACTIVE_PER_OWNER)] {
            assert_rejected(&mut registry, owner, id);
        }
        assert!(registry.status("operation-0", Some(":1.2")).is_none());
        assert!(registry.active.admit(Some(":1.2")).is_ok());
        assert!(registry.cancel("operation-0", Some(":1.2")).is_none());
        cancel_is_terminal(&mut registry);
        // Cancellation before the first poll must prevent effects, not just hide their result.
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), MAX_ACTIVE_PER_OWNER - 1);
        assert_eq!(registry.cancel_all().len(), MAX_ACTIVE_PER_OWNER - 1);
        registry.recent.prune(std::time::Instant::now() + RETENTION);
        assert!(registry.status("operation-0", None).is_none());
    }

    #[tokio::test]
    async fn revisions_are_atomic_with_status_and_owner_cancellation_is_local() {
        let mut registry = OperationRegistry::default();
        for (id, owner) in [(1, "a"), (2, "b")] {
            let accepted = registry
                .spawn(Some(owner.into()), result(id, "accepted"), |_| {
                    std::future::pending()
                })
                .unwrap();
            assert_eq!(accepted.revision, 1);
        }
        let before = registry.status("operation-1", Some("a")).unwrap();
        let running = registry.running(result(1, "running")).unwrap();
        assert_eq!(running.revision, before.revision + 1);
        assert_eq!(registry.status("operation-1", Some("a")), Some(running));
        let terminal = registry.cancel_owner("a").pop().unwrap();
        assert_eq!(terminal.revision, 3);
        assert_eq!(registry.status("operation-1", Some("a")), Some(terminal));
        assert!(registry.status("operation-1", Some("b")).is_none());
        assert_eq!(
            registry.status("operation-2", Some("b")).unwrap().status,
            "accepted"
        );
        assert!(registry.cancel_owner("a").is_empty());
        registry.cancel_all();
    }

    fn assert_rejected(registry: &mut OperationRegistry, owner: Option<&str>, id: usize) {
        let (started, mut receiver) = tokio::sync::oneshot::channel();
        assert!(
            registry
                .spawn(
                    owner.map(str::to_owned),
                    result(id, "accepted"),
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
    }

    fn cancel_is_terminal(registry: &mut OperationRegistry) {
        let cancelled = registry.cancel("operation-0", None).unwrap();
        assert_eq!(cancelled.status, "cancelled");
        let close = cancelled.close.unwrap();
        assert_eq!(close.status, "unknown");
        assert_eq!(close.dispatched_window_ids, ["window-a"]);
        assert!(cancelled.message.contains("may still take effect"));
        assert!(registry.running(result(0, "running")).is_none());
        assert!(registry.finish(result(0, "completed")).is_none());
        assert_eq!(
            registry.status("operation-0", None).unwrap().status,
            "cancelled"
        );
        assert!(registry.active.admit(None).is_ok());
    }
}
