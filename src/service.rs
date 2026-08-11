use std::{collections::HashMap, process::Stdio, sync::Arc};

use anyhow::Context;
use serde::Deserialize;
use tokio::{
    process::Command,
    sync::{Mutex, RwLock},
};
use uuid::Uuid;

use crate::{
    catalog::{Catalog, CatalogEntry},
    hyprland::{self, Client, Snapshot},
    model::{ApplicationPage, ApplicationSummary, OperationResult, WindowSummary},
    resources::{ResourceSampler, ResourceSnapshot, ResourceUsage},
};

pub struct ApplicationService {
    catalog: RwLock<Arc<Catalog>>,
    resources: Mutex<ResourceSampler>,
}

impl ApplicationService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            catalog: RwLock::new(Arc::new(Catalog::load())),
            resources: Mutex::new(ResourceSampler::default()),
        })
    }

    pub async fn refresh(&self) {
        *self.catalog.write().await = Arc::new(Catalog::load());
    }

    pub async fn revisions(&self) -> (u64, u64) {
        self.refresh().await;
        let catalog = self.catalog.read().await.revision;
        let windows = Snapshot::load().await.revision;
        (catalog, windows)
    }

    pub async fn query(&self, params: QueryParams) -> ApplicationPage {
        let windows = Snapshot::load().await;
        let catalog = Arc::clone(&*self.catalog.read().await);
        let resources = self.resources.lock().await.sample();
        page(&catalog, windows, &resources, &params)
    }

    pub async fn execute(&self, params: ExecuteParams) -> anyhow::Result<OperationResult> {
        let windows = Snapshot::load().await;
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

async fn execute_action(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<String> {
    let verb = match params.action.as_str() {
        "activate" => activate(catalog, windows, &params.target_id).await?,
        "launch" => {
            launch(catalog, &params.target_id).await?;
            "Launched"
        }
        "focus-window" => {
            focus_window(catalog, windows, params).await?;
            "Focused"
        }
        "desktop-action" => {
            let action = params
                .desktop_action_id
                .as_deref()
                .context("desktop_action_id is required")?;
            launch_action(catalog, &params.target_id, action).await?;
            "Started"
        }
        _ => anyhow::bail!("unsupported application action"),
    };
    Ok(format!(
        "{verb} {}",
        display_name(catalog, &params.target_id)
    ))
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
    hyprland::focus(&window.address).await
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
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.id.cmp(&right.id))
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
                cpu_percent: usage.cpu_percent,
                memory_bytes: usage.memory_bytes,
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
        id: entry.id.clone(),
        revision,
        kind: "desktop-application".into(),
        name: entry.name.clone(),
        generic_name: entry.generic_name.clone(),
        comment: entry.comment.clone(),
        icon: entry.icon.clone(),
        keywords: entry.keywords.clone(),
        categories: entry.categories.clone(),
        startup_class: entry.startup_class.clone(),
        running,
        focused,
        running_count: instances.len(),
        cpu_percent: usage.cpu_percent,
        memory_bytes: usage.memory_bytes,
        instances,
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
        id,
        revision,
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
        running: true,
        focused,
        running_count: instances.len(),
        cpu_percent: usage.cpu_percent,
        memory_bytes: usage.memory_bytes,
        instances,
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
        application.name.as_str(),
        application.generic_name.as_str(),
        application.comment.as_str(),
        application.id.as_str(),
        application.startup_class.as_str(),
    ]
    .into_iter()
    .chain(application.keywords.iter().map(String::as_str))
    .chain(application.categories.iter().map(String::as_str))
    .chain(
        application
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
    anyhow::ensure!(
        catalog.by_id(target_id).is_some(),
        "application is no longer available"
    );
    let first = run_gtk_launch(target_id).await;
    if first.is_ok() {
        return first;
    }
    run_gtk_launch(target_id.trim_end_matches(".desktop")).await
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

    use super::{resolve_target, running_score};

    #[test]
    fn focused_and_recent_windows_rank_first() {
        assert!(running_score(true, 0) > running_score(false, 1));
        assert!(running_score(false, 1) > running_score(false, 8));
        assert!(running_score(false, 8) > running_score(false, i64::MAX));
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
