use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use tokio::{
    sync::{Mutex, RwLock, broadcast, mpsc, oneshot},
    task::AbortHandle,
    time::{self, MissedTickBehavior},
};
use uuid::Uuid;

use crate::{
    catalog::{Catalog, default_catalog_paths},
    history::{HistoryStore, now_milliseconds},
    hyprland::{self, Snapshot},
    model::{ApplicationPage, ApplicationResourceHistory, OperationResult},
    resources::{ResourceSampler, ResourceSnapshot},
};

mod action;
mod query;

pub use action::ApplicationAction;
use action::{execute_action, operation_result};
use query::{combined_revision, page, resolve_target};
#[cfg(test)]
use {
    action::application_window_addresses,
    query::{resolve_target_with_cgroup, running_score},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateRevision {
    pub catalog: u64,
    pub windows: u64,
}

struct ActiveOperation {
    abort: AbortHandle,
    accepted: OperationResult,
}

pub struct ApplicationService {
    catalog: RwLock<Arc<Catalog>>,
    windows: RwLock<Arc<Snapshot>>,
    resources: RwLock<ResourceSnapshot>,
    history: Mutex<HistoryStore>,
    state_changes: broadcast::Sender<StateRevision>,
    operation_changes: broadcast::Sender<OperationResult>,
    operations: Mutex<HashMap<String, ActiveOperation>>,
}

impl ApplicationService {
    pub fn new() -> Arc<Self> {
        let (state_changes, _) = broadcast::channel(32);
        let (operation_changes, _) = broadcast::channel(64);
        let service = Arc::new(Self {
            catalog: RwLock::new(Arc::new(Catalog::load())),
            windows: RwLock::new(Arc::new(Snapshot::default())),
            resources: RwLock::new(ResourceSnapshot::default()),
            history: Mutex::new(HistoryStore::load_default()),
            state_changes,
            operation_changes,
            operations: Mutex::new(HashMap::new()),
        });
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(track_state(Arc::downgrade(&service)));
            tokio::spawn(track_resources(Arc::downgrade(&service)));
        }
        service
    }

    pub async fn refresh(&self) {
        self.refresh_catalog().await;
        self.refresh_windows().await;
    }

    pub async fn revisions(&self) -> StateRevision {
        StateRevision {
            catalog: self.catalog.read().await.revision,
            windows: self.windows.read().await.revision,
        }
    }

    pub fn subscribe_state(&self) -> broadcast::Receiver<StateRevision> {
        self.state_changes.subscribe()
    }

    pub fn subscribe_operations(&self) -> broadcast::Receiver<OperationResult> {
        self.operation_changes.subscribe()
    }

    async fn refresh_catalog(&self) {
        let next = Arc::new(Catalog::load());
        let changed = next.revision != self.catalog.read().await.revision;
        if changed {
            *self.catalog.write().await = next;
            self.publish_state().await;
        }
    }

    async fn refresh_windows(&self) {
        let next = Arc::new(Snapshot::load().await);
        let current = self.windows.read().await;
        let changed = next.available != current.available || next.revision != current.revision;
        drop(current);
        if changed {
            *self.windows.write().await = next;
            self.publish_state().await;
        }
    }

    async fn publish_state(&self) {
        let _ = self.state_changes.send(self.revisions().await);
    }

    pub async fn query(&self, params: QueryParams) -> ApplicationPage {
        let windows = Arc::clone(&*self.windows.read().await);
        let catalog = Arc::clone(&*self.catalog.read().await);
        let resources = self.resources.read().await;
        page(&catalog, &windows, &resources, &params)
    }

    pub async fn resource_history(
        &self,
        params: ResourceHistoryParams,
    ) -> anyhow::Result<ApplicationResourceHistory> {
        let page = self.history.lock().await.query(
            &params.target_id,
            params.since_ms,
            params.cursor.as_deref(),
            params.limit,
        )?;
        Ok(ApplicationResourceHistory {
            target_id: params.target_id,
            points: page.points,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        })
    }

    pub async fn save_history(&self) {
        if let Err(error) = self.history.lock().await.save() {
            tracing::warn!(%error, "resource history could not be saved");
        }
    }

    pub async fn save_history_final(&self) {
        if let Err(error) = self.history.lock().await.save_final() {
            tracing::warn!(%error, "final resource history could not be saved");
        }
    }

    pub async fn execute(
        self: &Arc<Self>,
        params: ExecuteParams,
    ) -> anyhow::Result<OperationResult> {
        let windows = Arc::clone(&*self.windows.read().await);
        let catalog = Arc::clone(&*self.catalog.read().await);
        if let Some(expected) = params.expected_revision {
            anyhow::ensure!(
                expected == combined_revision(&catalog, &windows),
                "application state changed; refresh and retry"
            );
        }

        let accepted = operation_result(
            format!("operation-{}", Uuid::new_v4()),
            &params,
            "accepted",
            "Operation accepted".into(),
            None,
        );
        let operation_id = accepted.id.clone();
        let service = Arc::clone(self);
        let (start_sender, start_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = start_receiver.await;
            let running = operation_result(
                operation_id.clone(),
                &params,
                "running",
                "Operation running".into(),
                None,
            );
            let _ = service.operation_changes.send(running);
            let result = execute_action(&catalog, &windows, &params).await;
            let completed = match result {
                Ok(outcome) => operation_result(
                    operation_id.clone(),
                    &params,
                    "completed",
                    outcome.message,
                    outcome.launch,
                ),
                Err(error) => operation_result(
                    operation_id.clone(),
                    &params,
                    "failed",
                    error.to_string(),
                    None,
                ),
            };
            if service
                .operations
                .lock()
                .await
                .remove(&operation_id)
                .is_some()
            {
                let _ = service.operation_changes.send(completed);
            }
        });
        self.operations.lock().await.insert(
            accepted.id.clone(),
            ActiveOperation {
                abort: task.abort_handle(),
                accepted: accepted.clone(),
            },
        );
        let _ = start_sender.send(());
        Ok(accepted)
    }

    pub async fn cancel_operation(&self, operation_id: &str) -> Option<OperationResult> {
        let active = self.operations.lock().await.remove(operation_id)?;
        active.abort.abort();
        let mut cancelled = active.accepted;
        cancelled.status = "cancelled".into();
        cancelled.message = "Operation cancelled".into();
        let _ = self.operation_changes.send(cancelled.clone());
        Some(cancelled)
    }
}

const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const WINDOW_RECOVERY_INTERVAL: Duration = Duration::from_secs(5);
const CATALOG_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
const EVENT_DEBOUNCE: Duration = Duration::from_millis(75);
const HISTORY_SAVE_INTERVAL: Duration = Duration::from_secs(60);

async fn track_state(service: std::sync::Weak<ApplicationService>) {
    let (window_sender, mut window_events) = mpsc::channel(64);
    tokio::spawn(hyprland::watch_events(window_sender));
    let (catalog_sender, mut catalog_events) = mpsc::channel(64);
    let _catalog_watcher = match catalog_watcher(catalog_sender) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            tracing::warn!(%error, "application catalog watcher could not start");
            None
        }
    };
    let mut window_poll = time::interval(WINDOW_RECOVERY_INTERVAL);
    let mut catalog_poll = time::interval(CATALOG_RECOVERY_INTERVAL);
    window_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    catalog_poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    window_poll.tick().await;
    catalog_poll.tick().await;

    let Some(initial) = service.upgrade() else {
        return;
    };
    initial.refresh_windows().await;
    drop(initial);

    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        tokio::select! {
            event = window_events.recv() => {
                if event.is_none() { return; }
                time::sleep(EVENT_DEBOUNCE).await;
                while window_events.try_recv().is_ok() {}
                service.refresh_windows().await;
            }
            event = catalog_events.recv() => {
                if event.is_none() { return; }
                time::sleep(EVENT_DEBOUNCE).await;
                while catalog_events.try_recv().is_ok() {}
                service.refresh_catalog().await;
            }
            _ = window_poll.tick() => service.refresh_windows().await,
            _ = catalog_poll.tick() => service.refresh_catalog().await,
        }
    }
}

fn catalog_watcher(sender: mpsc::Sender<()>) -> notify::Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = sender.try_send(());
        }
    })?;
    for path in default_catalog_paths()
        .into_iter()
        .filter(|path| path.exists())
    {
        if let Err(error) = watcher.watch(&path, RecursiveMode::Recursive) {
            tracing::warn!(%error, path = %path.display(), "application directory could not be watched");
        }
    }
    Ok(watcher)
}

async fn track_resources(service: std::sync::Weak<ApplicationService>) {
    let mut sampler = ResourceSampler::default();
    let mut interval = time::interval(RESOURCE_SAMPLE_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_save = Instant::now();
    loop {
        interval.tick().await;
        let Some(service) = service.upgrade() else {
            return;
        };
        sample_resources(&service, &mut sampler).await;
        if last_save.elapsed() >= HISTORY_SAVE_INTERVAL {
            service.save_history().await;
            last_save = Instant::now();
        }
    }
}

async fn sample_resources(service: &ApplicationService, sampler: &mut ResourceSampler) {
    let windows = Arc::clone(&*service.windows.read().await);
    let catalog = Arc::clone(&*service.catalog.read().await);
    let mut roots: HashMap<String, Vec<u32>> = HashMap::new();
    for window in &windows.clients {
        roots
            .entry(resolve_target(&catalog, window))
            .or_default()
            .push(window.pid);
    }
    let started = Instant::now();
    let mut owned_sampler = std::mem::take(sampler);
    let sampled = tokio::task::spawn_blocking(move || {
        let snapshot = owned_sampler.sample_for_targets(&roots);
        (owned_sampler, roots, snapshot)
    })
    .await;
    let Ok((next_sampler, roots, snapshot)) = sampled else {
        tracing::warn!("application resource sampler task failed");
        return;
    };
    *sampler = next_sampler;
    let sample_milliseconds = started.elapsed().as_millis();
    tracing::debug!(
        active_applications = roots.len(),
        sample_milliseconds,
        "application resources sampled"
    );
    let mut history = service.history.lock().await;
    for (target_id, pids) in roots {
        let usage = snapshot.usage_for_target(&target_id, pids);
        history.record(
            &target_id,
            now_milliseconds(),
            snapshot.interval_seconds(),
            &usage,
        );
    }
    drop(history);
    *service.resources.write().await = snapshot;
}

#[derive(Debug, Deserialize)]
pub struct QueryParams {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub generation: u64,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

const fn default_limit() -> usize {
    500
}

#[derive(Debug, Deserialize)]
pub struct ResourceHistoryParams {
    pub target_id: String,
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// Opaque cursor returned as `next_cursor` by the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_history_limit")]
    pub limit: usize,
}

const fn default_history_limit() -> usize {
    1_000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecuteParams {
    pub target_id: String,
    pub action: ApplicationAction,
    #[serde(default)]
    pub window_id: Option<String>,
    #[serde(default)]
    pub desktop_action_id: Option<String>,
    #[serde(default)]
    pub expected_revision: Option<u64>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

#[cfg(test)]
mod tests;
