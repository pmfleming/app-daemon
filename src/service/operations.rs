use crate::model::OperationResult;
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};
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

struct FinishedOperation {
    result: OperationResult,
    owner: Option<String>,
    finished: Instant,
}

#[derive(Default)]
pub(super) struct OperationRegistry {
    active: HashMap<String, ActiveOperation>,
    recent: VecDeque<FinishedOperation>,
}

impl OperationRegistry {
    pub fn admit(&self, owner: Option<&str>) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.active.len() < MAX_ACTIVE
                && self
                    .active
                    .values()
                    .filter(|active| active.owner.as_deref() == owner)
                    .count()
                    < MAX_ACTIVE_PER_OWNER,
            "too many active operations; retry after an operation finishes"
        );
        Ok(())
    }

    pub fn insert(&mut self, id: String, active: ActiveOperation) {
        self.active.insert(id, active);
    }

    pub fn running(&mut self, result: OperationResult) -> bool {
        let Some(active) = self.active.get_mut(&result.id) else {
            return false;
        };
        active.result = result;
        true
    }

    pub fn finish(&mut self, result: OperationResult) -> bool {
        let Some(active) = self.active.remove(&result.id) else {
            return false;
        };
        self.remember(result, active.owner);
        true
    }

    pub fn cancel(&mut self, id: &str, owner: Option<&str>) -> Option<OperationResult> {
        if self.active.get(id)?.owner.as_deref() != owner {
            return None;
        }
        let active = self.active.remove(id)?;
        active.abort.abort();
        let mut result = active.result;
        result.status = "cancelled".into();
        result.message = "Operation cancelled".into();
        self.remember(result.clone(), active.owner);
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
        self.prune(Instant::now());
        self.active
            .get(id)
            .filter(|active| active.owner.as_deref() == owner)
            .map(|active| active.result.clone())
            .or_else(|| {
                self.recent
                    .iter()
                    .find(|finished| finished.result.id == id && finished.owner.as_deref() == owner)
                    .map(|finished| finished.result.clone())
            })
    }

    fn remember(&mut self, result: OperationResult, owner: Option<String>) {
        let now = Instant::now();
        self.prune(now);
        self.recent.push_back(FinishedOperation {
            result,
            owner,
            finished: now,
        });
        while self.recent.len() > MAX_RECENT {
            self.recent.pop_front();
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .recent
            .front()
            .is_some_and(|entry| now.saturating_duration_since(entry.finished) >= RETENTION)
        {
            self.recent.pop_front();
        }
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
            registry.insert(
                format!("operation-{id}"),
                ActiveOperation {
                    abort: task.abort_handle(),
                    result: result(id, "accepted"),
                    owner: Some(":1.1".into()),
                },
            );
            assert!(registry.finish(result(id, "completed")));
        }
        assert!(registry.status("operation-0", Some(":1.1")).is_none());
        assert!(registry.status("operation-1", Some(":1.2")).is_none());
        assert_eq!(
            registry.status("operation-1", Some(":1.1")).unwrap().status,
            "completed"
        );
        registry.prune(Instant::now() + RETENTION);
        assert!(registry.recent.is_empty());
    }

    #[tokio::test]
    async fn cancellation_is_terminal_and_admission_is_bounded() {
        let mut registry = OperationRegistry::default();
        for id in 0..MAX_ACTIVE_PER_OWNER {
            let task = tokio::spawn(std::future::pending::<()>());
            registry.insert(
                format!("operation-{id}"),
                ActiveOperation {
                    abort: task.abort_handle(),
                    result: result(id, "accepted"),
                    owner: None,
                },
            );
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
