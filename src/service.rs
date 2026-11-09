use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, Notify, RwLock, Semaphore, broadcast, watch},
    time,
};

use crate::{
    catalog::Catalog,
    history::{HistoryStore, now_milliseconds},
    hyprland::Snapshot,
    model::{
        ApplicationEnergyOverview, ApplicationPage, ApplicationResourceHistory, OperationResult,
    },
    resources::{ResourceSampler, ResourceSnapshot},
    settings::{ApplicationSettings, SettingsStore},
};

mod action;
#[cfg(feature = "benchmarks")]
pub(crate) mod benchmarks;
mod catalog_watch;
mod energy_labels;
mod identity;
mod operations;
pub(crate) mod query;
mod software;
mod watcher;
mod workspaces;
use operations::OperationRegistry;

pub use action::{ApplicationAction, ExecuteParams};
use identity::{group_windows, resolve_target};
pub use query::QueryParams;
use query::{combined_revision, page, target_revision};

#[derive(Debug)]
pub(crate) struct StaleState;

impl std::fmt::Display for StaleState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("application state changed; refresh and retry")
    }
}

impl std::error::Error for StaleState {}

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
    resources: RwLock<Arc<ResourceSnapshot>>,
    // Bound non-cancellable query projection jobs, including abandoned callers.
    // The framework's request admission separately bounds asynchronous waiters.
    query_slots: Arc<Semaphore>,
    ownership: Arc<StdMutex<crate::ownership::Ownership>>,
    ownership_refresh: Arc<Mutex<()>>,
    history: Arc<Mutex<HistoryStore>>,
    // When both are needed, acquire settings before resources. Tokio's fair
    // RwLocks can deadlock even readers when writers queue between acquisitions.
    settings: Arc<RwLock<Arc<SettingsStore>>>,
    settings_updates: Arc<Mutex<()>>,
    software: Arc<Mutex<Option<crate::software::Cache>>>,
    software_revision: Arc<AtomicU64>,
    state_changes: broadcast::Sender<StateRevision>,
    operation_changes: broadcast::Sender<OperationResult>,
    operations: Mutex<OperationRegistry>,
    launch_journal: Mutex<workspaces::LaunchJournal>,
    launch_locks: StdMutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
    resource_sampling: StdMutex<ResourceSamplingPolicy>,
    resource_demand_changed: Notify,
    resume_events: watch::Receiver<u64>,
    stopping: AtomicBool,
    stop: watch::Sender<bool>,
    compositor_failure: watch::Sender<Option<shelllist_hyprland::IpcError>>,
    background_tasks: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
    shutdown_lock: Mutex<()>,
    request_gate: RwLock<()>,
}

impl ApplicationService {
    pub async fn new() -> anyhow::Result<Arc<Self>> {
        let history = tokio::task::spawn_blocking(HistoryStore::open_default).await??;
        let (cache, settings, catalog) = tokio::task::spawn_blocking(|| {
            let path = shelllist_daemon_core::resolve_xdg_path(
                shelllist_daemon_core::XdgRoot::State,
                "app-daemon",
                std::path::Path::new("software-catalog-turso-v1.db"),
            )
            .ok_or_else(|| anyhow::anyhow!("application catalog state directory unavailable"))?;
            let store = crate::software::store::Store::open(&path)?;
            let settings = store.settings()?;
            let observations = store.observations()?;
            let worker = std::env::var_os("APP_DAEMON_UPDATE_WORKER")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| "/run/current-system/sw/bin/update-worker".into());
            Ok::<_, anyhow::Error>((
                crate::software::Cache {
                    store,
                    observations,
                    attempted: None,
                    worker,
                },
                settings,
                Catalog::load(),
            ))
        })
        .await??;
        Ok(Self::build_with_catalog(
            true,
            history,
            Some(cache),
            settings,
            catalog,
        ))
    }

    #[cfg(test)]
    fn build(trackers: bool) -> Arc<Self> {
        Self::build_with_history(trackers, HistoryStore::load(None))
    }

    /// Frozen synthetic input for transport boundary qualification, without live trackers.
    #[cfg(test)]
    pub(crate) async fn with_fixture_catalog(catalog: Catalog) -> Arc<Self> {
        let service = Self::build(false);
        *service.catalog.write().await = Arc::new(catalog);
        service
    }

    #[cfg(test)]
    fn build_with_history(trackers: bool, history: HistoryStore) -> Arc<Self> {
        Self::build_with_catalog(
            trackers,
            history,
            None,
            SettingsStore::default(),
            Catalog::default(),
        )
    }
    fn build_with_catalog(
        trackers: bool,
        history: HistoryStore,
        software: Option<crate::software::Cache>,
        settings: SettingsStore,
        catalog: Catalog,
    ) -> Arc<Self> {
        let (state_changes, _) = broadcast::channel(32);
        let (operation_changes, _) = broadcast::channel(64);
        let (resume_sender, resume_events) = watch::channel(0);
        let (stop, _) = watch::channel(false);
        let service = Arc::new(Self {
            catalog: RwLock::new(Arc::new(catalog)),
            windows: RwLock::new(Arc::new(Snapshot::default())),
            resources: RwLock::new(Arc::new(ResourceSnapshot::default())),
            query_slots: Arc::new(Semaphore::new(2)),
            ownership: Arc::default(),
            ownership_refresh: Arc::default(),
            history: Arc::new(Mutex::new(history)),
            settings: Arc::new(RwLock::new(Arc::new(settings))),
            settings_updates: Arc::new(Mutex::new(())),
            software: Arc::new(Mutex::new(software)),
            software_revision: Arc::new(AtomicU64::new(0)),
            state_changes,
            operation_changes,
            operations: Mutex::new(OperationRegistry::default()),
            launch_journal: Mutex::new(workspaces::LaunchJournal::default()),
            launch_locks: StdMutex::new(HashMap::new()),
            resource_sampling: StdMutex::new(ResourceSamplingPolicy::default()),
            resource_demand_changed: Notify::new(),
            resume_events,
            stopping: AtomicBool::new(false),
            stop,
            compositor_failure: watch::channel(None).0,
            background_tasks: StdMutex::new(Vec::new()),
            shutdown_lock: Mutex::new(()),
            request_gate: RwLock::new(()),
        });
        if trackers && tokio::runtime::Handle::try_current().is_ok() {
            let tasks = vec![
                tokio::spawn(until_shutdown(
                    shelllist_daemon_tokio::monitor_resumes(resume_sender),
                    service.stop.subscribe(),
                )),
                tokio::spawn(until_shutdown(
                    watcher::track_state(Arc::downgrade(&service)),
                    service.stop.subscribe(),
                )),
                tokio::spawn(track_resources(Arc::downgrade(&service))),
                tokio::spawn(until_shutdown(
                    software::track(Arc::downgrade(&service)),
                    service.stop.subscribe(),
                )),
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

    pub(crate) fn compositor_failed(&self, error: shelllist_hyprland::IpcError) {
        self.compositor_failure.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(error);
                true
            }
        });
    }

    pub(crate) async fn compositor_failure(&self) -> anyhow::Error {
        let mut errors = self.compositor_failure.subscribe();
        loop {
            if let Some(error) = errors.borrow_and_update().clone() {
                return error.into();
            }
            if errors.changed().await.is_err() {
                return anyhow::anyhow!("compositor failure channel closed");
            }
        }
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
            catalog: self.catalog.read().await.revision
                ^ self.software_revision.load(Ordering::Acquire),
            windows: self.windows.read().await.revision,
            settings: self.settings.read().await.revision,
            runtime: self.resources.read().await.runtime_revision()
                ^ self.ownership_snapshot().revision(),
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
            self.resources.read().await.runtime_revision() ^ self.ownership_snapshot().revision(),
        ) ^ self.software_revision.load(Ordering::Acquire)
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

    fn ownership_snapshot(&self) -> Arc<crate::ownership::Snapshot> {
        self.ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }

    async fn refresh_ownership(&self, recover: bool) {
        let refresh = Arc::clone(&self.ownership_refresh).lock_owned().await;
        self.update_ownership(recover, refresh).await;
    }

    async fn update_ownership(&self, recover: bool, refresh: tokio::sync::OwnedMutexGuard<()>) {
        let catalog = Arc::clone(&*self.catalog.read().await);
        let recovered = if recover {
            crate::ownership::recover(&catalog).await
        } else {
            Vec::new()
        };
        let ownership = Arc::clone(&self.ownership);
        let changed = tokio::task::spawn_blocking(move || {
            // Retain serialization even if the async caller is cancelled while
            // procfs is being read: an older scan must not erase a newer claim.
            let _refresh = refresh;
            let processes = crate::ownership::processes();
            let mut ownership = ownership
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (target, identity) in recovered {
                ownership.remember(&target, [identity]);
            }
            ownership.reconcile(&catalog, &processes)
        })
        .await
        .unwrap_or(false);
        if changed {
            self.publish_state().await;
        }
    }

    pub async fn query(&self, params: QueryParams) -> anyhow::Result<ApplicationPage> {
        self.mark_resource_demand();
        // Keep identity/category fields and their persisted software record on
        // the same immutable view even if settings change during this query.
        let (software, catalog, settings) = self.software_view(false).await?;
        let slot = Arc::clone(&self.query_slots).acquire_owned().await?;
        let windows = Arc::clone(&*self.windows.read().await);
        let ownership = self.ownership_snapshot();
        let resources = Arc::clone(&*self.resources.read().await);
        // Procfs ownership fallback, ranking and resource projection can be costly.
        // Immutable snapshots release all state locks before entering the worker.
        Ok(tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let grouped = group_windows(&catalog, &windows, &ownership);
            let mut page = page(
                &catalog,
                &windows,
                &resources,
                &settings,
                &params,
                grouped,
                ownership.revision(),
            );
            page.revision ^= software.revision;
            for app in &mut page.applications {
                app.software = software
                    .entries
                    .iter()
                    .find(|r| r.id == app.identity.id)
                    .cloned();
            }
            page
        })
        .await?)
    }

    pub async fn update_settings(
        &self,
        params: UpdateSettingsParams,
    ) -> anyhow::Result<(ApplicationSettings, Option<bool>)> {
        if let Some(enabled) = params.autostart {
            anyhow::ensure!(
                params.category.is_none() && params.new_workspace.is_none(),
                "autostart must be updated separately from workspace preferences"
            );
            let update = Arc::clone(&self.settings_updates).lock_owned().await;
            let catalog = Arc::clone(&*self.catalog.read().await);
            let entry = catalog
                .by_id(&params.target_id)
                .filter(|entry| !entry.launch_only)
                .ok_or_else(|| anyhow::anyhow!("autostart requires a desktop application"))?;
            let source = entry.desktop_path().to_owned();
            let settings = self
                .settings
                .read()
                .await
                .for_application(&params.target_id)
                .cloned()
                .unwrap_or_default();
            tokio::task::spawn_blocking(move || {
                let _update = update;
                crate::autostart::set(&params.target_id, &source, enabled)
            })
            .await??;
            self.refresh_catalog().await;
            return Ok((settings, Some(enabled)));
        }
        if params.new_workspace.is_some() {
            anyhow::ensure!(
                self.catalog
                    .read()
                    .await
                    .by_id(&params.target_id)
                    .is_some_and(|entry| !entry.launch_only),
                "new workspace preference requires a desktop application"
            );
        }
        anyhow::ensure!(
            self.catalog.read().await.by_id(&params.target_id).is_some(),
            "application is no longer available"
        );
        let autostart = self
            .catalog
            .read()
            .await
            .by_id(&params.target_id)
            .and_then(|entry| entry.autostart);
        let update = Arc::clone(&self.settings_updates).lock_owned().await;
        let (next, settings) = self.settings.read().await.prepare_patch(
            params.target_id,
            params.category,
            params.new_workspace,
        )?;
        self.persist_software_settings(Arc::new(next), update)
            .await?;
        self.publish_state().await;
        Ok((settings, autostart))
    }

    pub async fn resource_history(
        &self,
        params: ResourceHistoryParams,
    ) -> anyhow::Result<ApplicationResourceHistory> {
        self.mark_resource_demand();
        let mut history = Arc::clone(&self.history).lock_owned().await;
        let (target_id, page) = tokio::task::spawn_blocking(move || {
            let page = history.query_window(
                &params.target_id,
                params.since_ms,
                params.until_ms,
                params.cursor.as_deref(),
                params.limit,
            )?;
            Ok::<_, anyhow::Error>((params.target_id, page))
        })
        .await??;
        Ok(ApplicationResourceHistory {
            target_id,
            summary: page.summary,
            points: page.points,
            has_more: page.has_more,
            next_cursor: page.next_cursor,
        })
    }

    pub async fn energy_overview(
        &self,
        params: EnergyOverviewParams,
    ) -> anyhow::Result<ApplicationEnergyOverview> {
        self.mark_resource_demand();
        let until_ms = now_milliseconds();
        let since_ms = params.since_ms.min(until_ms);
        let mut history = Arc::clone(&self.history).lock_owned().await;
        let totals = tokio::task::spawn_blocking(move || history.energy_totals(since_ms, until_ms))
            .await??;
        Ok(energy_labels::overview(
            totals,
            &*self.catalog.read().await,
            since_ms,
            until_ms,
            params.limit,
        ))
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
        // The worker owns serialization even if its async caller is cancelled.
        // Queries, sampling, maintenance and final saves share this one gate.
        let mut history = Arc::clone(&self.history).lock_owned().await;
        match tokio::task::spawn_blocking(move || history.flush(final_save)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "{message}"),
            Err(error) => tracing::warn!(%error, "resource history persistence task failed"),
        }
    }

    // Snapshot acquisition stays with the service's locks, not in action code.
    async fn prepare_action(
        &self,
        params: ExecuteParams,
    ) -> anyhow::Result<action::PreparedAction> {
        let windows = Arc::clone(&*self.windows.read().await);
        let catalog = Arc::clone(&*self.catalog.read().await);
        // Preserve settings-before-resources lock order during revision validation.
        let settings = self.settings.read().await;
        self.validate_action_revision(&params, &catalog, &windows, &settings)
            .await?;
        let preferences = settings.for_application(&params.target_id).cloned();
        drop(settings);
        let preferred_workspace = preferences
            .as_ref()
            .and_then(|value| value.workspace_id.as_deref());
        action::PreparedAction::new(catalog, params, preferred_workspace)
            .capture(preferences.as_ref())
            .await
    }

    async fn validate_action_revision(
        &self,
        params: &ExecuteParams,
        catalog: &Catalog,
        windows: &Snapshot,
        settings: &SettingsStore,
    ) -> anyhow::Result<()> {
        let Some(expected) = params.expected_revision else {
            return Ok(());
        };
        let ownership = self.ownership_snapshot();
        let grouped = group_windows(catalog, windows, &ownership);
        let target = target_revision(
            &params.target_id,
            catalog.by_id(&params.target_id),
            grouped.get(&params.target_id).map_or(&[], Vec::as_slice),
            settings.for_application(&params.target_id),
            windows.available,
        );
        // Legacy page-wide tokens remain valid; row tokens ignore unrelated activity.
        let global = combined_revision(
            catalog,
            windows,
            settings.revision,
            self.resources.read().await.runtime_revision() ^ ownership.revision(),
        ) ^ self.software_revision.load(Ordering::Acquire);
        if expected != target && expected != global {
            return Err(StaleState.into());
        }
        Ok(())
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
}

const ACTIVE_RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const BACKGROUND_RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
const RESOURCE_DEMAND_WINDOW: Duration = Duration::from_secs(15);
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
    while let Some(service) = service
        .upgrade()
        .filter(|s| !s.stopping.load(Ordering::Acquire))
    {
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
        if last_save.elapsed() >= HISTORY_SAVE_INTERVAL && !service.stopping.load(Ordering::Acquire)
        {
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
    let deadline = last_sample.map_or(now, |last| last + service.resource_sample_interval(now));
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
    service.refresh_ownership(true).await;
    let ownership = service.ownership_snapshot();
    let windows = Arc::clone(&*service.windows.read().await);
    let catalog = Arc::clone(&*service.catalog.read().await);
    let mut roots: HashMap<String, Vec<u32>> = HashMap::new();
    for window in &windows.clients {
        roots
            .entry(resolve_target(&catalog, window, &ownership).into_owned())
            .or_default()
            .push(window.pid);
    }
    let started = Instant::now();
    let mut owned_sampler = std::mem::take(sampler);
    let sampled = tokio::task::spawn_blocking(move || {
        let snapshot = owned_sampler.sample_for_applications(&roots, &catalog, &ownership);
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
    let snapshot = Arc::new(snapshot);
    let history_snapshot = Arc::clone(&snapshot);
    let mut history = Arc::clone(&service.history).lock_owned().await;
    let recorded = tokio::task::spawn_blocking(move || {
        let samples: Vec<_> = history_snapshot
            .target_roots()
            .iter()
            .map(|(id, pids)| {
                (
                    id.as_str(),
                    history_snapshot.usage_for_target(id, pids.iter().copied()),
                )
            })
            .collect();
        history.record_batch(
            now_milliseconds(),
            history_snapshot.interval_seconds(),
            samples.iter().map(|(id, usage)| (*id, usage)),
        )
    })
    .await;
    match recorded {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "resource history sample could not be committed"),
        Err(error) => tracing::warn!(%error, "resource history worker failed"),
    }
    let mut resources = service.resources.write().await;
    let changed = resources.runtime_revision() != snapshot.runtime_revision();
    *resources = snapshot;
    drop(resources);
    if changed {
        service.publish_state().await;
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct UpdateSettingsParams {
    pub target_id: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "category_patch"
    )]
    pub category: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_workspace: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart: Option<bool>,
}

fn category_patch<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
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

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
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
