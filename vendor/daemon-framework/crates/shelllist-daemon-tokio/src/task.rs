//! Ownership of background workers, independent of daemon policy.
use std::{future::Future, panic::AssertUnwindSafe, sync::Mutex};

use anyhow::{Result, anyhow};
use futures::FutureExt;
use tokio::task::{AbortHandle, JoinHandle};

/// Abort a child when its parent future is dropped, including cancellation.
pub struct AbortOnDrop(pub AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Owns one generation of workers. Closing the spawn gate precedes cancellation.
#[derive(Default)]
pub struct TaskGroup(Mutex<TaskGroupState>);

#[derive(Default)]
struct TaskGroupState {
    closed: bool,
    handles: Vec<JoinHandle<()>>,
}

impl TaskGroup {
    /// Returns false if shutdown has already closed the group.
    pub fn spawn(
        &self,
        name: &'static str,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return false;
        }
        // spawn_named reports panics even if the finished handle is pruned.
        state.handles.retain(|handle| !handle.is_finished());
        state.handles.push(spawn_named(name, future));
        true
    }

    pub fn abort(&self) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        for handle in &state.handles {
            handle.abort();
        }
    }

    /// Abort and join workers. Domain-specific graceful cleanup should run first.
    pub async fn shutdown(&self) {
        let handles = {
            let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
            state.closed = true;
            std::mem::take(&mut state.handles)
        };
        for handle in &handles {
            handle.abort();
        }
        for handle in handles {
            let _ = handle.await;
        }
    }
}

impl Drop for TaskGroup {
    fn drop(&mut self) {
        for handle in &self.0.get_mut().unwrap_or_else(|p| p.into_inner()).handles {
            handle.abort();
        }
    }
}

/// Opt-in panic conversion for a domain operation that can publish a failure.
pub async fn catch_task<T>(
    name: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|payload| {
            let panic = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic");
            anyhow!("{name} panicked: {panic}")
        })?
}

/// Reports failures without converting a panic into successful completion.
pub fn spawn_named(
    name: &'static str,
    future: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tracing::trace!(task = name, "background task started");
        match AssertUnwindSafe(future).catch_unwind().await {
            Ok(()) => tracing::trace!(task = name, "background task ended"),
            Err(payload) => {
                tracing::error!(task = name, "background task panicked");
                std::panic::resume_unwind(payload);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn shutdown_joins_and_rejects_late_spawns() {
        let tasks = TaskGroup::default();
        let marker = Arc::new(());
        let held = marker.clone();
        tasks.spawn("test", async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        tasks.shutdown().await;
        assert_eq!(Arc::strong_count(&marker), 1);
        let held = marker.clone();
        assert!(!tasks.spawn("closed", async move {
            let _held = held;
        }));
        assert_eq!(Arc::strong_count(&marker), 1);
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn abort_closes_gate_before_cancellation_cleanup() {
        struct ReconnectOnDrop(Arc<TaskGroup>, Arc<()>);
        impl Drop for ReconnectOnDrop {
            fn drop(&mut self) {
                let marker = self.1.clone();
                assert!(!self.0.spawn("late", async move {
                    let _marker = marker;
                    std::future::pending::<()>().await;
                }));
            }
        }
        let tasks = Arc::new(TaskGroup::default());
        let marker = Arc::new(());
        let guard = ReconnectOnDrop(tasks.clone(), marker.clone());
        let (ready, started) = tokio::sync::oneshot::channel();
        tasks.spawn("candidate", async move {
            let _guard = guard;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        tasks.abort();
        tasks.shutdown().await;
        assert_eq!(Arc::strong_count(&marker), 1);
        assert_eq!(Arc::strong_count(&tasks), 1);
    }

    #[tokio::test]
    async fn panic_remains_a_failed_join() {
        assert!(
            spawn_named("panic", async { panic!("injected") })
                .await
                .unwrap_err()
                .is_panic()
        );
        assert!(
            catch_task::<()>("operation", async { panic!("injected") })
                .await
                .is_err()
        );
    }
}
