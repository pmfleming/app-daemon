use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Deserialize;
use tokio::{
    sync::{Mutex, Notify, RwLock, broadcast, mpsc, oneshot, watch},
    time,
};
use uuid::Uuid;

use crate::{
    catalog::Catalog,
    history::{HistoryStore, merged_labels, now_milliseconds, persist_snapshot},
    hyprland::{self, Snapshot},
    model::{
        ApplicationEnergyOverview, ApplicationEnergySummary, ApplicationPage,
        ApplicationResourceHistory, OperationResult,
    },
    resources::{ResourceSampler, ResourceSnapshot},
    settings::{ApplicationSettings, SettingsStore},
};

mod action;
mod catalog_watch;
use catalog_watch::CatalogEvent;
#[cfg(feature = "benchmarks")]
pub(crate) mod benchmarks;
mod identity;
mod operations;
pub(crate) mod query;
mod watcher;
use operations::OperationRegistry;

pub use action::{ApplicationAction, ExecuteParams};
use action::{execute_action, operation_result};
use identity::{group_windows, resolve_target};
pub use query::QueryParams;
use query::{combined_revision, page};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateRevision {
    pub catalog: u64,
    pub windows: u64,
    pub settings: u64,
    pub runtime: u64,
}

pub struct ApplicationService {
    catalog: RwLock<Arc<Catalog>>,
    windows: RwLock<Arc<Snapshot>>,
    resources: RwLock<ResourceSnapshot>,
    history: Mutex<HistoryStore>,
    // When both are needed, acquire settings before resources. Tokio's fair
    // RwLocks can deadlock even readers when writers queue between acquisitions.
    settings: RwLock<SettingsStore>,
    settings_updates: Mutex<()>,
    state_changes: broadcast::Sender<StateRevision>,
    operation_changes: broadcast::Sender<OperationResult>,
    operations: Mutex<OperationRegistry>,
    launch_locks: StdMutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
    resource_sampling: StdMutex<ResourceSamplingPolicy>,
    resource_demand_changed: Notify,
    resume_events: watch::Receiver<u64>,
    stopping: AtomicBool,
    stop: watch::Sender<bool>,
    background_tasks: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
    shutdown_lock: Mutex<()>,
    request_gate: RwLock<()>,
    history_saves: Arc<Mutex<()>>,
}

impl ApplicationService {
    pub fn new() -> Arc<Self> {
        Self::build(true)
    }

    fn build(trackers: bool) -> Arc<Self> {
        let (state_changes, _) = broadcast::channel(32);
        let (operation_changes, _) = broadcast::channel(64);
        let (resume_sender, resume_events) = watch::channel(0);
        let (stop, _) = watch::channel(false);
        let service = Arc::new(Self {
            catalog: RwLock::new(Arc::new(Catalog::default())),
            windows: RwLock::new(Arc::new(Snapshot::default())),
            resources: RwLock::new(ResourceSnapshot::default()),
            history: Mutex::new(HistoryStore::load_default()),
            settings: RwLock::new(SettingsStore::load_default()),
            settings_updates: Mutex::new(()),
            state_changes,
            operation_changes,
            operations: Mutex::new(OperationRegistry::default()),
            launch_locks: StdMutex::new(HashMap::new()),
            resource_sampling: StdMutex::new(ResourceSamplingPolicy::default()),
            resource_demand_changed: Notify::new(),
            resume_events,
            stopping: AtomicBool::new(false),
            stop,
            background_tasks: StdMutex::new(Vec::new()),
            shutdown_lock: Mutex::new(()),
            request_gate: RwLock::new(()),
            history_saves: Arc::new(Mutex::new(())),
        });
        if trackers && tokio::runtime::Handle::try_current().is_ok() {
            let tasks = vec![
                tokio::spawn(until_shutdown(
                    crate::resume::monitor(resume_sender),
                    service.stop.subscribe(),
                )),
                tokio::spawn(until_shutdown(
                    track_state(Arc::downgrade(&service)),
                    service.stop.subscribe(),
                )),
                tokio::spawn(track_resources(Arc::downgrade(&service))),
            ];
            *service
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = tasks;
        }
        service
    }

    pub(crate) async fn request_permit(
        &self,
    ) -> anyhow::Result<tokio::sync::RwLockReadGuard<'_, ()>> {
        let permit = self.request_gate.read().await;
        anyhow::ensure!(
            !self.stopping.load(Ordering::Acquire),
            "application service is shutting down"
        );
        Ok(permit)
    }

    pub async fn shutdown(&self) {
        let _shutdown = self.shutdown_lock.lock().await;
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stop.send_replace(true);
        // Finish already admitted API calls before cancelling their operations.
        let _requests = self.request_gate.write().await;
        self.cancel_all_operations().await;
        let tasks = std::mem::take(
            &mut *self
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for task in tasks {
            if let Err(error) = task.await {
                tracing::warn!(%error, "background task failed during shutdown");
            }
        }
        self.save_history_final().await;
    }

    pub async fn refresh(&self) {
        self.refresh_catalog().await;
        self.refresh_windows().await;
    }

    pub async fn revisions(&self) -> StateRevision {
        StateRevision {
            catalog: self.catalog.read().await.revision,
            windows: self.windows.read().await.revision,
            settings: self.settings.read().await.revision,
            runtime: self.resources.read().await.runtime_revision(),
        }
    }

    pub async fn revision(&self) -> u64 {
        let catalog = self.catalog.read().await;
        let windows = self.windows.read().await;
        let settings = self.settings.read().await;
        combined_revision(
            &catalog,
            &windows,
            settings.revision,
            self.resources.read().await.runtime_revision(),
        )
    }

    pub fn subscribe_state(&self) -> broadcast::Receiver<StateRevision> {
        self.state_changes.subscribe()
    }

    pub fn subscribe_operations(&self) -> broadcast::Receiver<OperationResult> {
        self.operation_changes.subscribe()
    }

    async fn refresh_catalog(&self) {
        let next = Arc::new(
            tokio::task::spawn_blocking(Catalog::load)
                .await
                .unwrap_or_default(),
        );
        let changed = next.revision != self.catalog.read().await.revision;
        if changed {
            *self.catalog.write().await = next;
            self.publish_state().await;
        }
    }

    async fn refresh_windows(&self) -> bool {
        let next = Arc::new(Snapshot::load().await);
        let available = next.available;
        let current = self.windows.read().await;
        let changed = next.available != current.available || next.revision != current.revision;
        drop(current);
        if changed {
            *self.windows.write().await = next;
            self.publish_state().await;
        }
        available
    }

    async fn publish_state(&self) {
        let _ = self.state_changes.send(self.revisions().await);
    }

    pub async fn query(&self, params: QueryParams) -> ApplicationPage {
        self.mark_resource_demand();
        let windows = Arc::clone(&*self.windows.read().await);
        let catalog = Arc::clone(&*self.catalog.read().await);
        let grouped = group_windows(&catalog, &windows);
        let settings = self.settings.read().await;
        let resources = self.resources.read().await;
        page(&catalog, &windows, &resources, &settings, &params, grouped)
    }

    pub async fn update_settings(
        &self,
        params: UpdateSettingsParams,
    ) -> anyhow::Result<ApplicationSettings> {
        anyhow::ensure!(
            self.catalog.read().await.by_id(&params.target_id).is_some(),
            "application is no longer available"
        );
        let _update = self.settings_updates.lock().await;
        let (next, settings) = self
            .settings
            .read()
            .await
            .prepare_update(params.target_id, params.category)?;
        let next = tokio::task::spawn_blocking(move || {
            next.persist()?;
            Ok::<_, std::io::Error>(next)
        })
        .await??;
        *self.settings.write().await = next;
        self.publish_state().await;
        Ok(settings)
    }

    pub async fn resource_history(
        &self,
        params: ResourceHistoryParams,
    ) -> anyhow::Result<ApplicationResourceHistory> {
        self.mark_resource_demand();
        let page = self.history.lock().await.query_window(
            &params.target_id,
            params.since_ms,
            params.until_ms,
            params.cursor.as_deref(),
            params.limit,
        )?;
        Ok(ApplicationResourceHistory {
            target_id: params.target_id,
            summary: page.summary,
            points: page.points,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        })
    }

    pub async fn energy_overview(&self, params: EnergyOverviewParams) -> ApplicationEnergyOverview {
        self.mark_resource_demand();
        let until_ms = now_milliseconds();
        let since_ms = params.since_ms.min(until_ms);
        let mut totals = self.history.lock().await.energy_totals(since_ms, until_ms);
        totals.sort_by(|left, right| {
            right
                .energy_mwh
                .total_cmp(&left.energy_mwh)
                .then(left.target_id.cmp(&right.target_id))
        });
        let total_energy_mwh = totals.iter().map(|value| value.energy_mwh).sum::<f64>();
        let energy_source = merged_labels(totals.iter().map(|value| value.energy_source.as_str()));
        let energy_confidence =
            merged_labels(totals.iter().map(|value| value.energy_confidence.as_str()));
        let catalog = self.catalog.read().await;
        let applications = totals
            .into_iter()
            .take(params.limit.clamp(1, 100))
            .map(|value| {
                let identity = catalog.by_id(&value.target_id);
                ApplicationEnergySummary {
                    name: identity
                        .map(|entry| entry.name.clone())
                        .unwrap_or_else(|| value.target_id.clone()),
                    icon: identity.map(|entry| entry.icon.clone()).unwrap_or_default(),
                    share: if total_energy_mwh > 0.0 {
                        ((value.energy_mwh / total_energy_mwh) * 10_000.0).round() / 10_000.0
                    } else {
                        0.0
                    },
                    target_id: value.target_id,
                    energy_mwh: value.energy_mwh,
                }
            })
            .collect();
        ApplicationEnergyOverview {
            since_ms,
            until_ms,
            total_energy_mwh: (total_energy_mwh * 10_000.0).round() / 10_000.0,
            energy_source,
            energy_confidence,
            applications,
        }
    }

    fn mark_resource_demand(&self) {
        self.resource_sampling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mark_demand(Instant::now());
        self.resource_demand_changed.notify_one();
    }

    fn resource_sample_interval(&self, now: Instant) -> Duration {
        self.resource_sampling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .interval(now)
    }

    pub async fn save_history(&self) {
        self.persist_history(false, "resource history could not be saved")
            .await;
    }

    pub async fn save_history_final(&self) {
        self.persist_history(true, "final resource history could not be saved")
            .await;
    }

    async fn persist_history(&self, final_save: bool, message: &'static str) {
        // Transfer the guard to the writer: cancelling the awaiting future must
        // not release serialization while an older blocking write still runs.
        let save = Arc::clone(&self.history_saves).lock_owned().await;
        let snapshot = self.history.lock().await.snapshot(final_save);
        match tokio::task::spawn_blocking(move || {
            let _save = save;
            persist_snapshot(snapshot)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "{message}"),
            Err(error) => tracing::warn!(%error, "resource history persistence task failed"),
        }
    }

    fn launch_lock(&self, target_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .launch_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(target_id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(target_id.to_owned(), Arc::downgrade(&lock));
        lock
    }

    pub async fn execute_owned(
        self: &Arc<Self>,
        params: ExecuteParams,
        owner: Option<String>,
    ) -> anyhow::Result<OperationResult> {
        let windows = Arc::clone(&*self.windows.read().await);
        let catalog = Arc::clone(&*self.catalog.read().await);
        let settings = self.settings.read().await;
        if let Some(expected) = params.expected_revision {
            anyhow::ensure!(
                expected
                    == combined_revision(
                        &catalog,
                        &windows,
                        settings.revision,
                        self.resources.read().await.runtime_revision()
                    ),
                "application state changed; refresh and retry"
            );
        }

        let mut params = params;
        if matches!(
            params.action,
            ApplicationAction::Activate
                | ApplicationAction::Launch
                | ApplicationAction::DesktopAction
        ) && let Some(workspace) = settings
            .for_application(&params.target_id)
            .and_then(|value| value.workspace_id.clone())
        {
            params.workspace_id = Some(workspace);
        }
        drop(settings);

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
        let mut operations = self.operations.lock().await;
        anyhow::ensure!(
            !self.stopping.load(Ordering::Acquire),
            "application service is shutting down"
        );
        operations.admit(owner.as_deref())?;
        let task = tokio::spawn(async move {
            let _ = start_receiver.await;
            let running = operation_result(
                operation_id.clone(),
                &params,
                "running",
                "Operation running".into(),
                None,
            );
            {
                let mut operations = service.operations.lock().await;
                if !operations.running(running.clone()) {
                    return;
                }
                let _ = service.operation_changes.send(running);
            }
            let lock = service.launch_lock(&params.target_id);
            let _launch = lock.lock().await;
            let (status, message, launch) =
                match execute_action(&catalog, &params, &service, &operation_id).await {
                    Ok(outcome) => (outcome.status, outcome.message, outcome.launch),
                    Err(error) => ("failed", error.to_string(), None),
                };
            let completed = operation_result(operation_id, &params, status, message, launch);
            if service.operations.lock().await.finish(completed.clone()) {
                let _ = service.operation_changes.send(completed);
            }
        });
        operations.insert(owner, task.abort_handle(), accepted.clone())?;
        let _ = start_sender.send(());
        Ok(accepted)
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

const ACTIVE_RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const BACKGROUND_RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
const RESOURCE_DEMAND_WINDOW: Duration = Duration::from_secs(15);
const WINDOW_RECOVERY_INTERVAL: Duration = Duration::from_secs(5);
const CATALOG_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
const HEALTHY_WINDOW_INTERVAL: Duration = Duration::from_secs(30);
const HEALTHY_CATALOG_INTERVAL: Duration = Duration::from_secs(300);
const EVENT_DEBOUNCE: Duration = Duration::from_millis(75);
const HISTORY_SAVE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
struct ResourceSamplingPolicy {
    active_until: Option<Instant>,
}

impl ResourceSamplingPolicy {
    fn mark_demand(&mut self, now: Instant) {
        self.active_until = Some(now + RESOURCE_DEMAND_WINDOW);
    }

    fn interval(&self, now: Instant) -> Duration {
        if self.active_until.is_some_and(|deadline| now < deadline) {
            ACTIVE_RESOURCE_SAMPLE_INTERVAL
        } else {
            BACKGROUND_RESOURCE_SAMPLE_INTERVAL
        }
    }
}

async fn track_state(service: std::sync::Weak<ApplicationService>) {
    let (window_sender, mut window_events) = mpsc::channel(64);
    let events_task = tokio::spawn(hyprland::watch_window_events(window_sender));
    let _events = crate::platform::AbortOnDrop(events_task.abort_handle());
    let (catalog_sender, mut catalog_events) = mpsc::channel(64);
    let mut catalog_watch = watcher::WatchRecovery::new(Instant::now());
    let mut window_events_open = true;
    let mut catalog_events_open = true;
    let mut window_connected = false;
    let Some(initial) = service.upgrade() else {
        return;
    };
    let mut resumes = initial.resume_events.clone();
    let (_, mut window_available) =
        tokio::join!(initial.refresh_catalog(), initial.refresh_windows());
    drop(initial);
    let mut window_refreshed = Instant::now();
    let mut catalog_refreshed = Instant::now();

    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        let (window_interval, catalog_interval) = reconciliation_intervals(
            window_connected && window_available,
            catalog_watch.retry_deadline().is_none(),
        );
        tokio::select! {
            _ = resumes.changed(), if resumes.has_changed().is_ok() => {
                (_, window_available) = tokio::join!(service.refresh_catalog(), service.refresh_windows());
                window_refreshed = Instant::now();
                catalog_refreshed = window_refreshed;
                catalog_watch.rebuild(catalog_refreshed);
            }
            _ = wait_for_watcher_retry(catalog_watch.retry_deadline()) => {
                if let Err(error) = catalog_watch.attempt(Instant::now(), || catalog_watch::create(catalog_sender.clone())) {
                    tracing::warn!(%error, "catalog watcher unavailable; polling continues, retry scheduled");
                } else {
                    // Close the gap between the last snapshot and installing a watch.
                    service.refresh_catalog().await;
                    catalog_refreshed = Instant::now();
                }
            }
            event = window_events.recv(), if window_events_open => {
                let Some(event) = event else {
                    window_events_open = false;
                    window_connected = false;
                    tracing::warn!("window event stream ended; continuing reconciliation");
                    continue;
                };
                let mut refresh = observe_window_event(event, &mut window_connected);
                if !refresh { continue; }
                time::sleep(EVENT_DEBOUNCE).await;
                while let Ok(event) = window_events.try_recv() {
                    refresh |= observe_window_event(event, &mut window_connected);
                }
                if refresh {
                    window_available = service.refresh_windows().await;
                    window_refreshed = Instant::now();
                }
            }
            event = catalog_events.recv(), if catalog_events_open => {
                let Some(event) = event else { catalog_events_open = false; continue; };
                let mut failed = matches!(event, CatalogEvent::Failed);
                let mut rewatch = matches!(event, CatalogEvent::Rewatch);
                time::sleep(EVENT_DEBOUNCE).await;
                while let Ok(event) = catalog_events.try_recv() {
                    failed |= matches!(event, CatalogEvent::Failed);
                    rewatch |= matches!(event, CatalogEvent::Rewatch);
                }
                if failed { catalog_watch.failed(Instant::now()); }
                else if rewatch { catalog_watch.rebuild(Instant::now()); }
                service.refresh_catalog().await;
                catalog_refreshed = Instant::now();
            }
            _ = time::sleep_until(time::Instant::from_std(window_refreshed + window_interval)) => {
                window_available = service.refresh_windows().await;
                window_refreshed = Instant::now();
            }
            _ = time::sleep_until(time::Instant::from_std(catalog_refreshed + catalog_interval)) => {
                // Install the repaired watch before taking a single snapshot,
                // avoiding a second full catalog scan on every healthy timer.
                if catalog_watch.retry_deadline().is_none() {
                    catalog_watch.rebuild(Instant::now());
                    if let Err(error) = catalog_watch.attempt(Instant::now(), || catalog_watch::create(catalog_sender.clone())) {
                        tracing::warn!(%error, "catalog watch repair failed; polling continues");
                    }
                }
                service.refresh_catalog().await;
                catalog_refreshed = Instant::now();
            }
        }
    }
}

fn reconciliation_intervals(windows_healthy: bool, catalog_healthy: bool) -> (Duration, Duration) {
    (
        if windows_healthy {
            HEALTHY_WINDOW_INTERVAL
        } else {
            WINDOW_RECOVERY_INTERVAL
        },
        if catalog_healthy {
            HEALTHY_CATALOG_INTERVAL
        } else {
            CATALOG_RECOVERY_INTERVAL
        },
    )
}

fn observe_window_event(event: shelllist_hyprland::Event, connected: &mut bool) -> bool {
    match event {
        shelllist_hyprland::Event::Connected => {
            *connected = true;
            true
        }
        shelllist_hyprland::Event::Disconnected => {
            *connected = false;
            true
        }
        shelllist_hyprland::Event::Message(line) => hyprland::window_event_relevant(&line),
    }
}

async fn wait_for_watcher_retry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

async fn track_resources(service: std::sync::Weak<ApplicationService>) {
    let mut sampler = ResourceSampler::default();
    let mut last_sample = None;
    let mut last_save = Instant::now();
    let Some(initial) = service.upgrade() else {
        return;
    };
    let mut resumes = initial.resume_events.clone();
    let mut resume_generation = *resumes.borrow_and_update();
    drop(initial);
    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        if service.stopping.load(Ordering::Acquire) {
            return;
        }
        if !resource_sample_due(&service, last_sample, &mut resumes).await {
            continue;
        }
        let generation = *resumes.borrow_and_update();
        if generation != resume_generation {
            sampler.reset_after_resume();
            resume_generation = generation;
        }
        sample_resources(&service, &mut sampler).await;
        last_sample = Some(Instant::now());
        if service.stopping.load(Ordering::Acquire) {
            return;
        }
        if last_save.elapsed() >= HISTORY_SAVE_INTERVAL {
            service.save_history().await;
            last_save = Instant::now();
        }
    }
}

async fn resource_sample_due(
    service: &ApplicationService,
    last_sample: Option<Instant>,
    resumes: &mut watch::Receiver<u64>,
) -> bool {
    let now = Instant::now();
    let Some(last_sample) = last_sample else {
        return true;
    };
    let deadline = last_sample + service.resource_sample_interval(now);
    if now >= deadline {
        return true;
    }
    let mut stop = service.stop.subscribe();
    tokio::select! {
        () = wait_for_stop(&mut stop) => false,
        () = time::sleep_until(time::Instant::from_std(deadline)) => true,
        () = service.resource_demand_changed.notified() => false,
        _ = resumes.changed(), if resumes.has_changed().is_ok() => true,
    }
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    while !*stop.borrow_and_update() {
        if stop.changed().await.is_err() {
            return;
        }
    }
}

async fn until_shutdown(task: impl Future<Output = ()>, mut stop: watch::Receiver<bool>) {
    tokio::select! {
        () = task => {},
        () = wait_for_stop(&mut stop) => {},
    }
}

async fn sample_resources(service: &ApplicationService, sampler: &mut ResourceSampler) {
    let windows = Arc::clone(&*service.windows.read().await);
    let catalog = Arc::clone(&*service.catalog.read().await);
    let mut roots: HashMap<String, Vec<u32>> = HashMap::new();
    for window in &windows.clients {
        roots
            .entry(resolve_target(&catalog, window).into_owned())
            .or_default()
            .push(window.pid);
    }
    let started = Instant::now();
    let mut owned_sampler = std::mem::take(sampler);
    let sampled = tokio::task::spawn_blocking(move || {
        let snapshot = owned_sampler.sample_for_applications(&roots, &catalog);
        (owned_sampler, snapshot)
    })
    .await;
    let Ok((next_sampler, snapshot)) = sampled else {
        tracing::warn!("application resource sampler task failed");
        return;
    };
    *sampler = next_sampler;
    let sample_milliseconds = started.elapsed().as_millis();
    tracing::debug!(
        active_applications = snapshot.target_roots().len(),
        sample_milliseconds,
        "application resources sampled"
    );
    let mut history = service.history.lock().await;
    for (target_id, pids) in snapshot.target_roots() {
        let usage = snapshot.usage_for_target(target_id, pids.iter().copied());
        history.record(
            target_id,
            now_milliseconds(),
            snapshot.interval_seconds(),
            &usage,
        );
    }
    drop(history);
    let mut resources = service.resources.write().await;
    let changed = resources.runtime_revision() != snapshot.runtime_revision();
    *resources = snapshot;
    drop(resources);
    if changed {
        service.publish_state().await;
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateSettingsParams {
    pub target_id: String,
    pub category: String,
}

#[derive(Debug, Deserialize)]
pub struct ResourceHistoryParams {
    pub target_id: String,
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// Freeze the selected window across pages and its statistical summary.
    #[serde(default)]
    pub until_ms: Option<u64>,
    /// Opaque cursor returned as `next_cursor` by the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_history_limit")]
    pub limit: usize,
}

const fn default_history_limit() -> usize {
    1_000
}

#[derive(Debug, Deserialize)]
pub struct EnergyOverviewParams {
    pub since_ms: u64,
    #[serde(default = "default_energy_limit")]
    pub limit: usize,
}

const fn default_energy_limit() -> usize {
    20
}

#[cfg(test)]
mod shutdown_tests;
#[cfg(test)]
mod tests;
