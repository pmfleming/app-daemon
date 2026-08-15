use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use tokio::{
    sync::{Mutex, RwLock, broadcast, mpsc, oneshot},
    task::AbortHandle,
    time::{self, MissedTickBehavior},
};
use uuid::Uuid;

use crate::{
    catalog::{Catalog, CatalogEntry, default_catalog_paths},
    history::{HistoryStore, now_milliseconds},
    hyprland::{self, Client, Snapshot},
    launch::{self, LaunchReceipt},
    model::{
        ApplicationIdentity, ApplicationPage, ApplicationResourceHistory, ApplicationRuntime,
        ApplicationSummary, OperationResult, ResourceUsage, WindowSummary,
    },
    resources::{ResourceSampler, ResourceSnapshot},
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
        page(&catalog, (*windows).clone(), &resources, &params)
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
        params.action.parse::<ApplicationAction>()?;
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
    let sample_targets = roots.clone();
    let sampled = tokio::task::spawn_blocking(move || {
        let snapshot = owned_sampler.sample_for_targets(&sample_targets);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplicationAction {
    Activate,
    Launch,
    FocusWindow,
    Close,
    CloseWindow,
    MoveToWorkspace,
    DesktopAction,
}

impl std::str::FromStr for ApplicationAction {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "activate" => Ok(Self::Activate),
            "launch" => Ok(Self::Launch),
            "focus-window" => Ok(Self::FocusWindow),
            "close" => Ok(Self::Close),
            "close-window" => Ok(Self::CloseWindow),
            "move-to-workspace" => Ok(Self::MoveToWorkspace),
            "desktop-action" => Ok(Self::DesktopAction),
            _ => anyhow::bail!("unsupported application action"),
        }
    }
}

struct ActionOutcome {
    message: String,
    launch: Option<LaunchReceipt>,
}

fn operation_result(
    id: String,
    params: &ExecuteParams,
    status: &str,
    message: String,
    launch: Option<LaunchReceipt>,
) -> OperationResult {
    OperationResult {
        id,
        action: params.action.clone(),
        target_id: params.target_id.clone(),
        status: status.into(),
        message,
        launch_backend: launch.as_ref().map(|receipt| receipt.backend.clone()),
        launch_scope: launch.map(|receipt| receipt.scope),
    }
}

impl ActionOutcome {
    fn new(catalog: &Catalog, target_id: &str, verb: &str) -> Self {
        Self {
            message: format!("{verb} {}", display_name(catalog, target_id)),
            launch: None,
        }
    }

    fn launched(catalog: &Catalog, target_id: &str, verb: &str, launch: LaunchReceipt) -> Self {
        Self {
            message: format!("{verb} {}", display_name(catalog, target_id)),
            launch: Some(launch),
        }
    }
}

async fn execute_action(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<ActionOutcome> {
    let action: ApplicationAction = params.action.parse()?;
    action.execute(catalog, windows, params).await
}

impl ApplicationAction {
    async fn execute(
        self,
        catalog: &Catalog,
        windows: &Snapshot,
        params: &ExecuteParams,
    ) -> anyhow::Result<ActionOutcome> {
        let target_id = &params.target_id;
        match self {
            Self::Activate => activate(catalog, windows, target_id).await,
            Self::Launch => launch(catalog, target_id)
                .await
                .map(|launch| ActionOutcome::launched(catalog, target_id, "Launched", launch)),
            Self::FocusWindow => focus_window(catalog, windows, params)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Focused")),
            Self::Close => close_application(catalog, windows, target_id)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Closed")),
            Self::CloseWindow => close_window(catalog, windows, params)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Closed")),
            Self::MoveToWorkspace => move_to_workspace(catalog, windows, params)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Moved")),
            Self::DesktopAction => desktop_action(catalog, params)
                .await
                .map(|launch| ActionOutcome::launched(catalog, target_id, "Started", launch)),
        }
    }
}

async fn desktop_action(
    catalog: &Catalog,
    params: &ExecuteParams,
) -> anyhow::Result<LaunchReceipt> {
    let action = params
        .desktop_action_id
        .as_deref()
        .context("desktop_action_id is required")?;
    launch_action(catalog, &params.target_id, action).await
}

async fn activate(
    catalog: &Catalog,
    windows: &Snapshot,
    target_id: &str,
) -> anyhow::Result<ActionOutcome> {
    if let Some(window) = target_window(catalog, windows, target_id) {
        hyprland::focus(&window.address).await?;
        return Ok(ActionOutcome::new(catalog, target_id, "Focused"));
    }
    let launch = launch(catalog, target_id).await?;
    let verb = if focus_launched_window(catalog, target_id).await? {
        "Launched and focused"
    } else {
        "Launched"
    };
    Ok(ActionOutcome::launched(catalog, target_id, verb, launch))
}

async fn focus_launched_window(catalog: &Catalog, target_id: &str) -> anyhow::Result<bool> {
    const FOCUS_TIMEOUT: Duration = Duration::from_secs(8);
    const FOCUS_RETRY_INTERVAL: Duration = Duration::from_millis(100);

    let deadline = Instant::now() + FOCUS_TIMEOUT;
    loop {
        let windows = Snapshot::load().await;
        if let Some(window) = target_window(catalog, &windows, target_id) {
            hyprland::focus(&window.address).await?;
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        time::sleep(FOCUS_RETRY_INTERVAL).await;
    }
}

async fn focus_window(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<()> {
    let address = target_address(catalog, windows, params)?;
    hyprland::focus(address).await
}

async fn close_window(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<()> {
    let address = target_address(catalog, windows, params)?;
    hyprland::close(address).await
}

async fn move_to_workspace(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<()> {
    let address = target_address(catalog, windows, params)?;
    let workspace = params
        .workspace_id
        .as_deref()
        .context("workspace_id is required")?;
    hyprland::move_to_workspace(address, workspace).await
}

fn target_address<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<&'a str> {
    Ok(&target_instance(catalog, windows, params)?.address)
}

async fn close_application(
    catalog: &Catalog,
    windows: &Snapshot,
    target_id: &str,
) -> anyhow::Result<()> {
    let addresses = application_window_addresses(catalog, windows, target_id);
    anyhow::ensure!(!addresses.is_empty(), "application is no longer running");
    for address in addresses {
        hyprland::close(&address).await?;
    }
    Ok(())
}

fn application_window_addresses(
    catalog: &Catalog,
    windows: &Snapshot,
    target_id: &str,
) -> Vec<String> {
    windows
        .clients
        .iter()
        .filter(|window| resolve_target(catalog, window) == target_id)
        .map(|window| window.address.clone())
        .collect()
}

fn target_instance<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<&'a Client> {
    let id = params
        .window_id
        .as_deref()
        .context("window_id is required")?;
    let window = windows
        .by_window_id(id)
        .context("window is no longer available")?;
    anyhow::ensure!(
        resolve_target(catalog, window) == params.target_id,
        "window no longer belongs to the selected application"
    );
    Ok(window)
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
    pub action: String,
    #[serde(default)]
    pub window_id: Option<String>,
    #[serde(default)]
    pub desktop_action_id: Option<String>,
    #[serde(default)]
    pub expected_revision: Option<u64>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

fn combined_revision(catalog: &Catalog, windows: &Snapshot) -> u64 {
    catalog.revision.rotate_left(17) ^ windows.revision
}

fn page(
    catalog: &Catalog,
    windows: Snapshot,
    resources: &ResourceSnapshot,
    params: &QueryParams,
) -> ApplicationPage {
    let revision = combined_revision(catalog, &windows);
    let available = windows.available;
    let mut grouped: HashMap<String, Vec<Client>> = HashMap::new();
    for window in windows.clients {
        grouped
            .entry(resolve_target(catalog, &window))
            .or_default()
            .push(window);
    }

    let mut applications: Vec<ApplicationSummary> = catalog
        .entries
        .iter()
        .map(|entry| {
            summary_for_entry(
                entry,
                grouped.remove(&entry.id).unwrap_or_default(),
                resources,
                revision,
            )
        })
        .collect();
    applications.extend(
        grouped
            .into_iter()
            .map(|(id, clients)| summary_for_unmatched(id, clients, resources, revision)),
    );
    applications.retain(|application| matches_query(application, &params.query));
    applications.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                left.identity
                    .name
                    .to_lowercase()
                    .cmp(&right.identity.name.to_lowercase())
            })
            .then_with(|| left.identity.id.cmp(&right.identity.id))
    });
    let limit = params.limit.clamp(1, 1000);
    let has_more = applications.len() > limit;
    applications.truncate(limit);
    ApplicationPage {
        revision,
        generation: params.generation,
        applications,
        has_more,
        hyprland_available: available,
    }
}

fn resolve_target(catalog: &Catalog, window: &Client) -> String {
    window_classes(window)
        .find_map(|class| exact_target(catalog, class))
        .or_else(|| window_classes(window).find_map(|class| suffix_target(catalog, class)))
        .unwrap_or_else(|| {
            let class = if window.initial_class.is_empty() {
                &window.class
            } else {
                &window.initial_class
            };
            format!("window-group:{}", class.trim().to_ascii_lowercase())
        })
}

fn window_classes(window: &Client) -> impl Iterator<Item = &str> {
    [&window.class, &window.initial_class]
        .into_iter()
        .map(|class| class.trim().trim_end_matches(".desktop"))
}

fn exact_target(catalog: &Catalog, class: &str) -> Option<String> {
    catalog
        .entries
        .iter()
        .find(|entry| {
            entry
                .id
                .trim_end_matches(".desktop")
                .eq_ignore_ascii_case(class)
                || (!entry.startup_class.is_empty()
                    && entry.startup_class.eq_ignore_ascii_case(class))
        })
        .map(|entry| entry.id.clone())
}

fn suffix_target(catalog: &Catalog, class: &str) -> Option<String> {
    let suffix = class.rsplit('.').next().unwrap_or_default();
    let mut matches = catalog.entries.iter().filter(|entry| {
        entry
            .id
            .trim_end_matches(".desktop")
            .eq_ignore_ascii_case(suffix)
    });
    let target = matches.next()?;
    matches.next().is_none().then(|| target.id.clone())
}

fn target_window<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    target_id: &str,
) -> Option<&'a Client> {
    windows
        .clients
        .iter()
        .find(|window| resolve_target(catalog, window) == target_id)
}

fn instances(
    target_id: &str,
    clients: &[Client],
    resources: &ResourceSnapshot,
) -> Vec<WindowSummary> {
    clients
        .iter()
        .map(|window| {
            let usage = resources.usage_for_target(target_id, [window.pid]);
            WindowSummary {
                id: hyprland::window_id(&window.address),
                title: window.title.clone(),
                class: window.class.clone(),
                workspace_id: window.workspace.id.to_string(),
                workspace_name: window.workspace.name.clone(),
                focused: window.focus_rank == 0,
                focus_rank: window.focus_rank,
                resources: usage,
            }
        })
        .collect()
}

fn instance_state(
    target_id: &str,
    clients: Vec<Client>,
    resources: &ResourceSnapshot,
) -> (Vec<WindowSummary>, bool, i64, ResourceUsage) {
    let usage = resources.usage_for_target(target_id, clients.iter().map(|window| window.pid));
    let instances = instances(target_id, &clients, resources);
    let focused = instances.iter().any(|window| window.focused);
    let best_rank = instances
        .iter()
        .map(|window| window.focus_rank)
        .min()
        .unwrap_or(i64::MAX);
    (instances, focused, best_rank, usage)
}

fn summary_for_entry(
    entry: &CatalogEntry,
    clients: Vec<Client>,
    resources: &ResourceSnapshot,
    revision: u64,
) -> ApplicationSummary {
    let (instances, focused, best_rank, usage) = instance_state(&entry.id, clients, resources);
    let running = !instances.is_empty();
    ApplicationSummary {
        identity: ApplicationIdentity {
            id: entry.id.clone(),
            kind: "desktop-application".into(),
            name: entry.name.clone(),
            generic_name: entry.generic_name.clone(),
            comment: entry.comment.clone(),
            icon: entry.icon.clone(),
            keywords: entry.keywords.clone(),
            categories: entry.categories.clone(),
            startup_class: entry.startup_class.clone(),
        },
        revision,
        runtime: ApplicationRuntime {
            running,
            focused,
            running_count: instances.len(),
            resources: usage,
            instances,
        },
        desktop_actions: entry.actions.clone(),
        score: running_score(focused, best_rank),
    }
}

fn summary_for_unmatched(
    id: String,
    clients: Vec<Client>,
    resources: &ResourceSnapshot,
    revision: u64,
) -> ApplicationSummary {
    let name = clients
        .first()
        .filter(|window| !window.class.is_empty())
        .map_or("Untitled", |window| &window.class)
        .to_owned();
    let (instances, focused, best_rank, usage) = instance_state(&id, clients, resources);
    ApplicationSummary {
        identity: ApplicationIdentity {
            id,
            kind: "window-group".into(),
            name,
            generic_name: "Running window".into(),
            comment: String::new(),
            icon: String::new(),
            keywords: instances
                .iter()
                .flat_map(|window| [window.title.clone(), window.class.clone()])
                .collect(),
            categories: Vec::new(),
            startup_class: String::new(),
        },
        revision,
        runtime: ApplicationRuntime {
            running: true,
            focused,
            running_count: instances.len(),
            resources: usage,
            instances,
        },
        desktop_actions: Vec::new(),
        score: running_score(focused, best_rank),
    }
}

fn running_score(focused: bool, focus_rank: i64) -> i64 {
    if focused {
        20_000
    } else if focus_rank != i64::MAX {
        10_000 + (1_000 - focus_rank).max(0)
    } else {
        0
    }
}

fn matches_query(application: &ApplicationSummary, query: &str) -> bool {
    let tokens: Vec<_> = query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if tokens.is_empty() {
        return true;
    }
    let searchable = [
        application.identity.name.as_str(),
        application.identity.generic_name.as_str(),
        application.identity.comment.as_str(),
        application.identity.id.as_str(),
        application.identity.startup_class.as_str(),
    ]
    .into_iter()
    .chain(application.identity.keywords.iter().map(String::as_str))
    .chain(application.identity.categories.iter().map(String::as_str))
    .chain(
        application
            .runtime
            .instances
            .iter()
            .flat_map(|window| [window.title.as_str(), window.class.as_str()]),
    )
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();
    tokens.iter().all(|token| searchable.contains(token))
}

fn display_name(catalog: &Catalog, target_id: &str) -> String {
    catalog.by_id(target_id).map_or_else(
        || {
            target_id
                .strip_prefix("window-group:")
                .unwrap_or(target_id)
                .to_owned()
        },
        |entry| entry.name.clone(),
    )
}

async fn launch(catalog: &Catalog, target_id: &str) -> anyhow::Result<LaunchReceipt> {
    let entry = catalog
        .by_id(target_id)
        .context("application is no longer available")?;
    if entry.requires_terminal() {
        return launch_in_terminal(entry.launch_command()?);
    }
    let first = launch::launch_desktop(target_id).await;
    if first.is_ok() {
        return first;
    }
    launch::launch_desktop(target_id.trim_end_matches(".desktop")).await
}

fn launch_in_terminal(command: Vec<String>) -> anyhow::Result<LaunchReceipt> {
    let (program, command_arguments) = command
        .split_first()
        .context("desktop application command is empty")?;
    let mut arguments = Vec::with_capacity(command_arguments.len() + 2);
    arguments.extend(["--", program.as_str()]);
    arguments.extend(command_arguments.iter().map(String::as_str));
    launch::spawn("xdg-terminal-exec", arguments)
        .context("start application in the default terminal")
}

async fn launch_action(
    catalog: &Catalog,
    target_id: &str,
    action_id: &str,
) -> anyhow::Result<LaunchReceipt> {
    let entry = catalog
        .by_id(target_id)
        .context("application is no longer available")?;
    let args = entry.parse_action(action_id)?;
    let (program, arguments) = args
        .split_first()
        .context("desktop action command is empty")?;
    launch::spawn(program, arguments).context("start desktop action")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{
        catalog::Catalog,
        hyprland::{Client, Workspace},
    };

    use super::{
        ApplicationAction, ApplicationService, ExecuteParams, application_window_addresses,
        resolve_target, running_score,
    };

    #[test]
    fn parses_application_actions() {
        assert!(matches!(
            "activate".parse::<ApplicationAction>(),
            Ok(ApplicationAction::Activate)
        ));
        assert!(matches!(
            "move-to-workspace".parse::<ApplicationAction>(),
            Ok(ApplicationAction::MoveToWorkspace)
        ));
        assert!("unknown".parse::<ApplicationAction>().is_err());
    }

    #[tokio::test]
    async fn accepts_operations_before_reporting_their_result() -> anyhow::Result<()> {
        let service = ApplicationService::new();
        let mut events = service.subscribe_operations();
        let params = ExecuteParams {
            target_id: "missing-window-group".into(),
            action: "close".into(),
            window_id: None,
            desktop_action_id: None,
            expected_revision: None,
            workspace_id: None,
        };
        let accepted = service.execute(params).await?;
        assert_eq!(accepted.status, "accepted");
        let running = events.recv().await?;
        let failed = events.recv().await?;
        assert_eq!(running.id, accepted.id);
        assert_eq!(running.status, "running");
        assert_eq!(failed.id, accepted.id);
        assert_eq!(failed.status, "failed");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_operations_for_stale_revisions() {
        let service = ApplicationService::new();
        let params = ExecuteParams {
            target_id: "missing-window-group".into(),
            action: "close".into(),
            window_id: None,
            desktop_action_id: None,
            expected_revision: Some(u64::MAX),
            workspace_id: None,
        };
        assert!(service.execute(params).await.is_err());
    }

    #[test]
    fn focused_and_recent_windows_rank_first() {
        assert!(running_score(true, 0) > running_score(false, 1));
        assert!(running_score(false, 1) > running_score(false, 8));
        assert!(running_score(false, 8) > running_score(false, i64::MAX));
    }

    #[test]
    fn selects_all_application_windows_for_closing() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("example.desktop"),
            "[Desktop Entry]\nType=Application\nName=Example\nExec=true\nStartupWMClass=example\n",
        )?;
        let catalog = Catalog::from_paths(vec![directory.path().into()]);
        let client = |address: &str, class: &str| Client {
            address: address.into(),
            class: class.into(),
            initial_class: class.into(),
            title: class.into(),
            pid: 42,
            workspace: Workspace::default(),
            focus_rank: 0,
            mapped: true,
        };
        let windows = crate::hyprland::Snapshot {
            available: true,
            revision: 1,
            clients: vec![
                client("0x1", "example"),
                client("0x2", "example"),
                client("0x3", "other"),
            ],
        };

        assert_eq!(
            application_window_addresses(&catalog, &windows, "example.desktop"),
            ["0x1", "0x2"]
        );
        Ok(())
    }

    #[test]
    fn resolves_unique_reverse_dns_class_suffix() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("yazi.desktop"),
            "[Desktop Entry]\nType=Application\nName=Yazi\nExec=true\n",
        )?;
        let catalog = Catalog::from_paths(vec![directory.path().into()]);
        let window = Client {
            address: "0x1".into(),
            class: "com.laufan.yazi".into(),
            initial_class: "com.laufan.yazi".into(),
            title: "Yazi".into(),
            pid: 42,
            workspace: Workspace::default(),
            focus_rank: 0,
            mapped: true,
        };
        assert_eq!(resolve_target(&catalog, &window), "yazi.desktop");
        Ok(())
    }
}
