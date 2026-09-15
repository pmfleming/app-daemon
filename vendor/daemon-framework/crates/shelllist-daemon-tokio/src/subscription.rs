use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, OnceLock, Weak},
};

use shelllist_daemon_core::IdSequence;
use tokio::{sync::oneshot, task::JoinHandle};

struct OwnedTask {
    owner: Option<String>,
    task: JoinHandle<()>,
    generation: Arc<()>,
}

#[derive(Default)]
struct State {
    closed: bool,
    tasks: HashMap<String, OwnedTask>,
}

/// One registry belongs to one serving D-Bus connection. No global bus state.
pub struct OwnedTaskRegistry {
    ids: IdSequence,
    state: Arc<Mutex<State>>,
    limits: TaskLimits,
    owners: OnceLock<crate::OwnerLossMonitor>,
}

#[derive(Debug, Clone, Copy)]
pub struct TaskLimits {
    pub total: usize,
    pub per_owner: usize,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            total: 256,
            per_owner: 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskAdmissionError {
    Closed,
    Full,
    DuplicateId,
}
impl std::fmt::Display for TaskAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Closed => "task registry is shutting down",
            Self::Full => "too many active tasks; retry after a task finishes",
            Self::DuplicateId => "task ID is already active",
        })
    }
}
impl std::error::Error for TaskAdmissionError {}

struct Registration {
    state: Weak<Mutex<State>>,
    id: String,
    generation: Arc<()>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            if state
                .tasks
                .get(&self.id)
                .is_some_and(|task| Arc::ptr_eq(&task.generation, &self.generation))
            {
                state.tasks.remove(&self.id);
            }
        }
    }
}

impl OwnedTaskRegistry {
    #[must_use]
    pub fn new(first_id: u64) -> Self {
        Self::with_limits(first_id, TaskLimits::default())
    }

    #[must_use]
    pub fn with_limits(first_id: u64, limits: TaskLimits) -> Self {
        Self {
            ids: IdSequence::new(first_id),
            state: Arc::new(Mutex::new(State::default())),
            limits,
            owners: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn next_id(&self, prefix: &str) -> String {
        self.ids.next(prefix)
    }

    /// Lazily binds this registry's monitor to its serving connection.
    pub fn owner_monitor(&self, connection: &zbus::Connection) -> crate::OwnerLossMonitor {
        self.owners
            .get_or_init(|| crate::OwnerLossMonitor::new(connection.clone()))
            .clone()
    }

    /// Registers before polling the worker. Cleans up on completion, panic,
    /// cancellation, owner loss, monitor failure, or registry destruction.
    pub fn spawn_for_owner(
        &self,
        id: String,
        owner: Option<String>,
        connection: &zbus::Connection,
        events: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), TaskAdmissionError> {
        let monitor = self.owner_monitor(connection);
        let watched_owner = owner.clone();
        self.spawn_until(id, owner, async move {
            match watched_owner {
                Some(owner) => {
                    if let Err(error) = monitor.wait(&owner).await {
                        tracing::warn!(%owner, %error, "ending owned task because owner monitoring failed");
                    }
                }
                None => std::future::pending().await,
            }
        }, events)
    }

    pub fn spawn_until(
        &self,
        id: String,
        owner: Option<String>,
        stop: impl Future<Output = ()> + Send + 'static,
        events: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), TaskAdmissionError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return Err(TaskAdmissionError::Closed);
        }
        if state.tasks.contains_key(&id) {
            return Err(TaskAdmissionError::DuplicateId);
        }
        if state.tasks.len() >= self.limits.total
            || state
                .tasks
                .values()
                .filter(|task| task.owner == owner)
                .count()
                >= self.limits.per_owner
        {
            return Err(TaskAdmissionError::Full);
        }
        let generation = Arc::new(());
        let registration = Registration {
            state: Arc::downgrade(&self.state),
            id: id.clone(),
            generation: generation.clone(),
        };
        let (start, ready) = oneshot::channel();
        let task = crate::spawn_named("owned-task", async move {
            let _registration = registration;
            if ready.await.is_err() {
                return;
            }
            tokio::select! { biased; () = stop => {}, () = events => {} }
        });
        state.tasks.insert(
            id,
            OwnedTask {
                owner,
                task,
                generation,
            },
        );
        drop(state);
        let _ = start.send(());
        Ok(())
    }

    pub async fn cancel_owned(&self, id: &str, owner: Option<&str>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .tasks
            .get(id)
            .is_none_or(|task| task.owner.as_deref() != owner)
        {
            return false;
        }
        if let Some(task) = state.tasks.remove(id) {
            task.task.abort();
            true
        } else {
            false
        }
    }

    pub async fn cancel_owner(&self, owner: &str) -> usize {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let before = state.tasks.len();
        state.tasks.retain(|_, task| {
            let keep = task.owner.as_deref() != Some(owner);
            if !keep {
                task.task.abort();
            }
            keep
        });
        before - state.tasks.len()
    }

    pub async fn cancel_all(&self) {
        self.drain(false).await;
    }
    pub async fn shutdown(&self) {
        self.drain(true).await;
    }

    async fn drain(&self, close: bool) {
        let tasks = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.closed |= close;
            state
                .tasks
                .drain()
                .map(|(_, task)| task.task)
                .collect::<Vec<_>>()
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}

impl Default for OwnedTaskRegistry {
    fn default() -> Self {
        Self::new(1)
    }
}
impl Drop for OwnedTaskRegistry {
    fn drop(&mut self) {
        for task in self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .tasks
            .values()
        {
            task.task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    #[tokio::test]
    async fn admission_cancellation_and_shutdown_are_owner_scoped() {
        let registry = OwnedTaskRegistry::with_limits(
            1,
            TaskLimits {
                total: 2,
                per_owner: 1,
            },
        );
        registry
            .spawn_until("one".into(), Some("a".into()), pending(), pending())
            .unwrap();
        assert_eq!(
            registry.spawn_until("two".into(), Some("a".into()), pending(), pending()),
            Err(TaskAdmissionError::Full)
        );
        registry
            .spawn_until("two".into(), Some("b".into()), pending(), pending())
            .unwrap();
        assert_eq!(
            registry.spawn_until("three".into(), Some("c".into()), pending(), pending()),
            Err(TaskAdmissionError::Full)
        );
        assert!(!registry.cancel_owned("one", Some("b")).await);
        assert_eq!(registry.cancel_owner("a").await, 1);
        assert!(registry.cancel_owned("two", Some("b")).await);
        registry.shutdown().await;
        assert_eq!(
            registry.spawn_until("late".into(), None, pending(), pending()),
            Err(TaskAdmissionError::Closed)
        );
    }

    #[tokio::test]
    async fn immediate_completion_panic_and_stop_remove_registration() {
        let registry = OwnedTaskRegistry::default();
        registry
            .spawn_until("complete".into(), None, pending(), async {})
            .unwrap();
        registry
            .spawn_until("panic".into(), None, pending(), async {
                panic!("injected")
            })
            .unwrap();
        registry
            .spawn_until("stop".into(), None, async {}, pending())
            .unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(registry.state.lock().unwrap().tasks.is_empty());
    }

    #[tokio::test]
    async fn old_cleanup_cannot_remove_reused_id() {
        let registry = OwnedTaskRegistry::default();
        registry
            .spawn_until("id".into(), None, pending(), pending())
            .unwrap();
        assert!(registry.cancel_owned("id", None).await);
        registry
            .spawn_until("id".into(), None, pending(), pending())
            .unwrap();
        tokio::task::yield_now().await;
        assert!(registry.cancel_owned("id", None).await);
    }

    #[tokio::test]
    async fn dropping_registry_releases_workers() {
        let registry = OwnedTaskRegistry::default();
        let marker = Arc::new(());
        let held = marker.clone();
        registry
            .spawn_until("id".into(), None, pending(), async move {
                let _held = held;
                pending::<()>().await;
            })
            .unwrap();
        drop(registry);
        tokio::task::yield_now().await;
        assert_eq!(Arc::strong_count(&marker), 1);
    }
}
