use std::{
    collections::HashMap,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use serde::Deserialize;
use tokio::{
    process::Command,
    sync::{Mutex, RwLock},
    time::{self, MissedTickBehavior},
};
use uuid::Uuid;

use crate::{
    catalog::{Catalog, CatalogEntry},
    history::{HistoryStore, now_milliseconds},
    hyprland::{self, Client, Snapshot},
    model::{
        ApplicationIdentity, ApplicationPage, ApplicationResourceHistory, ApplicationRuntime,
        ApplicationSummary, OperationResult, ResourceUsage, WindowSummary,
    },
    resources::{ResourceSampler, ResourceSnapshot},
};

pub struct ApplicationService {
    catalog: RwLock<Arc<Catalog>>,
    windows: RwLock<Arc<Snapshot>>,
    resources: RwLock<ResourceSnapshot>,
    history: Mutex<HistoryStore>,
}

impl ApplicationService {
    pub fn new() -> Arc<Self> {
        let service = Arc::new(Self {
            catalog: RwLock::new(Arc::new(Catalog::load())),
            windows: RwLock::new(Arc::new(Snapshot::default())),
            resources: RwLock::new(ResourceSnapshot::default()),
            history: Mutex::new(HistoryStore::load_default()),
        });
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(track_resources(Arc::downgrade(&service)));
        }
        service
    }

    pub async fn refresh(&self) {
        *self.catalog.write().await = Arc::new(Catalog::load());
    }

    pub async fn revisions(&self) -> (u64, u64) {
        self.refresh().await;
        let catalog = self.catalog.read().await.revision;
        let snapshot = Arc::new(Snapshot::load().await);
        let windows = snapshot.revision;
        *self.windows.write().await = snapshot;
        (catalog, windows)
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
    ) -> ApplicationResourceHistory {
        let (points, has_more) =
            self.history
                .lock()
                .await
                .query(&params.target_id, params.since_ms, params.limit);
        ApplicationResourceHistory {
            target_id: params.target_id,
            points,
            has_more,
        }
    }

    pub async fn save_history(&self) {
        if let Err(error) = self.history.lock().await.save() {
            tracing::warn!(%error, "resource history could not be saved");
        }
    }

    pub async fn execute(&self, params: ExecuteParams) -> anyhow::Result<OperationResult> {
        let windows = Arc::clone(&*self.windows.read().await);
        let catalog = Arc::clone(&*self.catalog.read().await);
        let message = execute_action(&catalog, &windows, &params).await?;
        Ok(OperationResult {
            id: format!("operation-{}", Uuid::new_v4()),
            action: params.action,
            target_id: params.target_id,
            status: "completed".into(),
            message,
        })
    }
}

const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const HISTORY_SAVE_INTERVAL: Duration = Duration::from_secs(60);

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
    let windows = Arc::new(Snapshot::load().await);
    let catalog = Arc::clone(&*service.catalog.read().await);
    let mut roots: HashMap<String, Vec<u32>> = HashMap::new();
    for window in &windows.clients {
        roots
            .entry(resolve_target(&catalog, window))
            .or_default()
            .push(window.pid);
    }
    let started = Instant::now();
    let snapshot = sampler.sample_for_roots(roots.values().flatten().copied());
    let sample_milliseconds = started.elapsed().as_millis();
    tracing::debug!(
        active_applications = roots.len(),
        sample_milliseconds,
        "application resources sampled"
    );
    let mut history = service.history.lock().await;
    for (target_id, pids) in roots {
        let usage = snapshot.usage_for_roots(pids);
        history.record(
            &target_id,
            now_milliseconds(),
            snapshot.interval_seconds(),
            &usage,
        );
    }
    drop(history);
    *service.windows.write().await = windows;
    *service.resources.write().await = snapshot;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplicationAction {
    Activate,
    Launch,
    FocusWindow,
    Close,
    CloseWindow,
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
            "desktop-action" => Ok(Self::DesktopAction),
            _ => anyhow::bail!("unsupported application action"),
        }
    }
}

async fn execute_action(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<String> {
    let action: ApplicationAction = params.action.parse()?;
    let verb = action.execute(catalog, windows, params).await?;
    Ok(format!(
        "{verb} {}",
        display_name(catalog, &params.target_id)
    ))
}

impl ApplicationAction {
    async fn execute(
        self,
        catalog: &Catalog,
        windows: &Snapshot,
        params: &ExecuteParams,
    ) -> anyhow::Result<&'static str> {
        match self {
            Self::Activate => activate(catalog, windows, &params.target_id).await,
            Self::Launch => launch(catalog, &params.target_id)
                .await
                .map(|()| "Launched"),
            Self::FocusWindow => focus_window(catalog, windows, params)
                .await
                .map(|()| "Focused"),
            Self::Close => close_application(catalog, windows, &params.target_id)
                .await
                .map(|()| "Closed"),
            Self::CloseWindow => close_window(catalog, windows, params)
                .await
                .map(|()| "Closed"),
            Self::DesktopAction => desktop_action(catalog, params).await.map(|()| "Started"),
        }
    }
}

async fn desktop_action(catalog: &Catalog, params: &ExecuteParams) -> anyhow::Result<()> {
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
) -> anyhow::Result<&'static str> {
    if let Some(window) = target_window(catalog, windows, target_id) {
        hyprland::focus(&window.address).await?;
        Ok("Focused")
    } else {
        launch(catalog, target_id).await?;
        Ok("Launched")
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
    #[serde(default = "default_history_limit")]
    pub limit: usize,
}

const fn default_history_limit() -> usize {
    1_000
}

#[derive(Debug, Deserialize)]
pub struct ExecuteParams {
    pub target_id: String,
    pub action: String,
    #[serde(default)]
    pub window_id: Option<String>,
    #[serde(default)]
    pub desktop_action_id: Option<String>,
}

fn page(
    catalog: &Catalog,
    windows: Snapshot,
    resources: &ResourceSnapshot,
    params: &QueryParams,
) -> ApplicationPage {
    let revision = catalog.revision.rotate_left(17) ^ windows.revision;
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

fn instances(clients: &[Client], resources: &ResourceSnapshot) -> Vec<WindowSummary> {
    clients
        .iter()
        .map(|window| {
            let usage = resources.usage_for_roots([window.pid]);
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
    clients: Vec<Client>,
    resources: &ResourceSnapshot,
) -> (Vec<WindowSummary>, bool, i64, ResourceUsage) {
    let usage = resources.usage_for_roots(clients.iter().map(|window| window.pid));
    let instances = instances(&clients, resources);
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
    let (instances, focused, best_rank, usage) = instance_state(clients, resources);
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
    let (instances, focused, best_rank, usage) = instance_state(clients, resources);
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

async fn launch(catalog: &Catalog, target_id: &str) -> anyhow::Result<()> {
    let entry = catalog
        .by_id(target_id)
        .context("application is no longer available")?;
    if entry.requires_terminal() {
        return launch_in_terminal(entry.launch_command()?).await;
    }
    let first = run_gtk_launch(target_id).await;
    if first.is_ok() {
        return first;
    }
    run_gtk_launch(target_id.trim_end_matches(".desktop")).await
}

async fn launch_in_terminal(command: Vec<String>) -> anyhow::Result<()> {
    let (program, arguments) = command
        .split_first()
        .context("desktop application command is empty")?;
    Command::new("xdg-terminal-exec")
        .arg("--")
        .arg(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start application in the default terminal")?;
    Ok(())
}

async fn run_gtk_launch(id: &str) -> anyhow::Result<()> {
    let status = Command::new("gtk-launch")
        .arg(id)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await
        .context("start gtk-launch")?;
    anyhow::ensure!(status.success(), "desktop application launch failed");
    Ok(())
}

async fn launch_action(catalog: &Catalog, target_id: &str, action_id: &str) -> anyhow::Result<()> {
    let entry = catalog
        .by_id(target_id)
        .context("application is no longer available")?;
    let args = entry.parse_action(action_id)?;
    let (program, arguments) = args
        .split_first()
        .context("desktop action command is empty")?;
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start desktop action")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::{
        catalog::Catalog,
        hyprland::{Client, Workspace},
    };

    use super::{ApplicationAction, application_window_addresses, resolve_target, running_score};

    #[test]
    fn parses_application_actions() {
        assert!(matches!(
            "activate".parse::<ApplicationAction>(),
            Ok(ApplicationAction::Activate)
        ));
        assert!("unknown".parse::<ApplicationAction>().is_err());
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
