//! Bounded admission for non-cancellable blocking work.
use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneError {
    Full,
    Closed,
    Panicked,
}
impl std::fmt::Display for LaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "blocking queue is full",
            Self::Closed => "blocking lane is closed",
            Self::Panicked => "blocking job panicked",
        })
    }
}
impl std::error::Error for LaneError {}

#[derive(Debug)]
pub struct LaneShutdown {
    pub dispatcher_stopped: bool,
    /// Running blocking jobs cannot be aborted; a timeout reports rather than hides them.
    pub active_jobs: usize,
}

struct Activity {
    count: AtomicUsize,
    idle: Notify,
}
struct ActiveJob(Arc<Activity>);
impl Drop for ActiveJob {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::AcqRel);
        self.0.idle.notify_waiters();
    }
}

pub struct BlockingLane {
    sender: mpsc::Sender<Job>,
    name: &'static str,
    closed: Mutex<bool>,
    shutdown: watch::Sender<bool>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
    activity: Arc<Activity>,
}

impl BlockingLane {
    /// Capacity and concurrency must be nonzero.
    pub fn start(
        runtime: &tokio::runtime::Handle,
        name: &'static str,
        capacity: usize,
        concurrency: usize,
    ) -> Self {
        assert!(
            capacity > 0 && concurrency > 0,
            "blocking lane bounds must be nonzero"
        );
        let (sender, receiver) = mpsc::channel(capacity);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let activity = Arc::new(Activity {
            count: AtomicUsize::new(0),
            idle: Notify::new(),
        });
        let dispatcher = runtime.spawn(run(
            name,
            concurrency,
            receiver,
            shutdown_rx,
            activity.clone(),
        ));
        Self {
            sender,
            name,
            closed: Mutex::new(false),
            shutdown,
            dispatcher: Mutex::new(Some(dispatcher)),
            activity,
        }
    }

    pub fn try_submit(&self, job: impl FnOnce() + Send + 'static) -> Result<(), LaneError> {
        let closed = self.closed.lock().unwrap_or_else(|p| p.into_inner());
        if *closed {
            return Err(LaneError::Closed);
        }
        self.sender
            .try_send(Box::new(job))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => LaneError::Full,
                mpsc::error::TrySendError::Closed(_) => LaneError::Closed,
            })
    }

    /// For synchronous callers only; never block a Tokio worker waiting for a lane.
    pub fn call<T: Send + 'static>(
        &self,
        task: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, LaneError> {
        let (reply, receive) = std::sync::mpsc::sync_channel(1);
        self.try_submit(move || {
            let _ =
                reply.send(catch_unwind(AssertUnwindSafe(task)).map_err(|_| LaneError::Panicked));
        })?;
        receive.recv().map_err(|_| LaneError::Closed)?
    }

    /// Admission is synchronous, before allocating a Tokio blocking task. Dropping
    /// the returned future does not cancel an accepted job or release its capacity.
    pub fn call_async<T: Send + 'static>(
        &self,
        task: impl FnOnce() -> T + Send + 'static,
    ) -> impl Future<Output = Result<T, LaneError>> + Send + 'static {
        let (reply, receive) = oneshot::channel();
        let admission = self.try_submit(move || {
            let _ =
                reply.send(catch_unwind(AssertUnwindSafe(task)).map_err(|_| LaneError::Panicked));
        });
        async move {
            admission?;
            receive.await.map_err(|_| LaneError::Closed)?
        }
    }

    pub async fn shutdown(&self, timeout: Duration) -> LaneShutdown {
        *self.closed.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.shutdown.send_replace(true);
        let deadline = tokio::time::Instant::now() + timeout;
        let dispatcher = self
            .dispatcher
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let mut dispatcher_stopped = true;
        if let Some(mut dispatcher) = dispatcher {
            if tokio::time::timeout_at(deadline, &mut dispatcher)
                .await
                .is_err()
            {
                dispatcher_stopped = false;
                dispatcher.abort();
                let _ = dispatcher.await;
            }
        }
        let _ = tokio::time::timeout_at(deadline, self.wait_until_idle()).await;
        let active_jobs = self.activity.count.load(Ordering::Acquire);
        if !dispatcher_stopped || active_jobs != 0 {
            tracing::warn!(
                lane = self.name,
                active_jobs,
                dispatcher_stopped,
                "blocking lane shutdown timed out"
            );
        }
        LaneShutdown {
            dispatcher_stopped,
            active_jobs,
        }
    }

    async fn wait_until_idle(&self) {
        loop {
            let notified = self.activity.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.activity.count.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}
impl Drop for BlockingLane {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(task) = self
            .dispatcher
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            task.abort();
        }
    }
}

async fn run(
    name: &'static str,
    concurrency: usize,
    mut receiver: mpsc::Receiver<Job>,
    mut shutdown: watch::Receiver<bool>,
    activity: Arc<Activity>,
) {
    let permits = Arc::new(Semaphore::new(concurrency));
    loop {
        if *shutdown.borrow() {
            return;
        }
        let permit = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            permit = permits.clone().acquire_owned() => { let Ok(permit) = permit else { return; }; permit }
        };
        let job = tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            job = receiver.recv() => { let Some(job) = job else { return; }; job }
        };
        activity.count.fetch_add(1, Ordering::AcqRel);
        let active = ActiveJob(activity.clone());
        tokio::task::spawn_blocking(move || {
            let _active = active;
            let _permit = permit;
            if catch_unwind(AssertUnwindSafe(job)).is_err() {
                tracing::error!(lane = name, "blocking job panicked; lane remains available");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounds_admission_contains_panics_and_rejects_after_shutdown() {
        let lane = BlockingLane::start(&tokio::runtime::Handle::current(), "test", 1, 1);
        let (started, ready) = oneshot::channel();
        let (release, held) = std::sync::mpsc::sync_channel(1);
        lane.try_submit(move || {
            started.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(5)).unwrap();
        })
        .unwrap();
        ready.await.unwrap();
        let queued = lane.call_async(|| 42);
        assert_eq!(lane.try_submit(|| {}), Err(LaneError::Full));
        release.send(()).unwrap();
        assert_eq!(queued.await.unwrap(), 42);
        assert_eq!(
            lane.call_async(|| panic!("injected")).await,
            Err(LaneError::Panicked)
        );
        assert_eq!(lane.call_async(|| 7).await.unwrap(), 7);
        assert_eq!(lane.shutdown(Duration::from_secs(1)).await.active_jobs, 0);
        assert_eq!(lane.try_submit(|| {}), Err(LaneError::Closed));
    }
    #[tokio::test]
    async fn cancelled_waiters_keep_admission_and_control_lane_stays_available() {
        let runtime = tokio::runtime::Handle::current();
        let requests = BlockingLane::start(&runtime, "requests", 1, 1);
        let controls = BlockingLane::start(&runtime, "controls", 1, 1);
        let (started, ready) = oneshot::channel();
        let (release, held) = std::sync::mpsc::sync_channel(1);
        let accepted = requests.call_async(move || {
            started.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        ready.await.unwrap();
        drop(accepted);
        let queued = requests.call_async(|| 42);
        assert_eq!(requests.try_submit(|| {}), Err(LaneError::Full));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), controls.call_async(|| "cancelled"))
                .await
                .unwrap()
                .unwrap(),
            "cancelled"
        );
        release.send(()).unwrap();
        assert_eq!(queued.await.unwrap(), 42);
        requests.shutdown(Duration::from_secs(1)).await;
        controls.shutdown(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn timeout_reports_running_jobs_and_drops_queued_replies() {
        let lane = BlockingLane::start(&tokio::runtime::Handle::current(), "slow", 1, 1);
        let (started, ready) = oneshot::channel();
        let (release, held) = std::sync::mpsc::sync_channel(1);
        lane.try_submit(move || {
            started.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(5)).unwrap();
        })
        .unwrap();
        ready.await.unwrap();
        let queued = lane.call_async(|| 42);
        let report = lane.shutdown(Duration::from_millis(5)).await;
        assert_eq!(report.active_jobs, 1);
        assert_eq!(queued.await, Err(LaneError::Closed));
        release.send(()).unwrap();
        assert_eq!(lane.shutdown(Duration::from_secs(1)).await.active_jobs, 0);
    }
}
