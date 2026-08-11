use std::{collections::HashMap, process::Stdio, sync::Arc};

use anyhow::Context;
use serde::Deserialize;
use tokio::{process::Command, sync::RwLock};
use uuid::Uuid;

use crate::{
    catalog::{Catalog, CatalogEntry},
    hyprland::{self, Client, Snapshot},
    model::{ApplicationPage, ApplicationSummary, OperationResult, WindowSummary},
};

pub struct ApplicationService {
    catalog: RwLock<Catalog>,
}

impl ApplicationService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            catalog: RwLock::new(Catalog::load()),
        })
    }

    pub async fn refresh(&self) {
        *self.catalog.write().await = Catalog::load();
    }

    pub async fn revisions(&self) -> (u64, u64) {
        self.refresh().await;
        let catalog = self.catalog.read().await.revision;
        let windows = Snapshot::load().await.revision;
        (catalog, windows)
    }

    pub async fn query(&self, params: QueryParams) -> ApplicationPage {
        let catalog = self.catalog.read().await.clone();
        let windows = Snapshot::load().await;
        page(&catalog, &windows, &params)
    }

    pub async fn execute(&self, params: ExecuteParams) -> anyhow::Result<OperationResult> {
        let catalog = self.catalog.read().await.clone();
        let windows = Snapshot::load().await;
        let operation_id = format!("operation-{}", Uuid::new_v4());
        let message = match params.action.as_str() {
            "activate" => {
                if let Some(window) = target_windows(&catalog, &windows, &params.target_id).first()
                {
                    hyprland::focus(&window.address).await?;
                    format!("Focused {}", display_name(&catalog, &params.target_id))
                } else {
                    launch(&catalog, &params.target_id).await?;
                    format!("Launched {}", display_name(&catalog, &params.target_id))
                }
            }
            "launch" => {
                launch(&catalog, &params.target_id).await?;
                format!("Launched {}", display_name(&catalog, &params.target_id))
            }
            "focus-window" => {
                let id = params
                    .window_id
                    .as_deref()
                    .context("window_id is required")?;
                let window = windows
                    .by_window_id(id)
                    .context("window is no longer available")?;
                anyhow::ensure!(
                    target_windows(&catalog, &windows, &params.target_id)
                        .iter()
                        .any(|candidate| candidate.address == window.address),
                    "window no longer belongs to the selected application"
                );
                hyprland::focus(&window.address).await?;
                format!("Focused {}", display_name(&catalog, &params.target_id))
            }
            "desktop-action" => {
                let action_id = params
                    .desktop_action_id
                    .as_deref()
                    .context("desktop_action_id is required")?;
                launch_action(&catalog, &params.target_id, action_id).await?;
                format!("Started {}", display_name(&catalog, &params.target_id))
            }
            _ => anyhow::bail!("unsupported application action"),
        };
        Ok(OperationResult {
            id: operation_id,
            action: params.action,
            target_id: params.target_id,
            status: "completed".into(),
            message,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
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

fn page(catalog: &Catalog, windows: &Snapshot, params: &QueryParams) -> ApplicationPage {
    let revision = catalog.revision.rotate_left(17) ^ windows.revision;
    let mut grouped: HashMap<String, Vec<&Client>> = HashMap::new();
    for window in &windows.clients {
        grouped
            .entry(resolve_target(catalog, window))
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
                revision,
            )
        })
        .collect();
    applications.extend(
        grouped
            .into_iter()
            .map(|(id, clients)| summary_for_unmatched(id, clients, revision)),
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
        hyprland_available: windows.available,
    }
}

fn resolve_target(catalog: &Catalog, window: &Client) -> String {
    for candidate in [&window.class, &window.initial_class] {
        let key = candidate.trim().trim_end_matches(".desktop");
        if let Some(entry) = catalog.entries.iter().find(|entry| {
            entry
                .id
                .trim_end_matches(".desktop")
                .eq_ignore_ascii_case(key)
                || (!entry.startup_class.is_empty()
                    && entry.startup_class.eq_ignore_ascii_case(key))
        }) {
            return entry.id.clone();
        }
    }
    for candidate in [&window.class, &window.initial_class] {
        let suffix = candidate
            .trim()
            .trim_end_matches(".desktop")
            .rsplit('.')
            .next()
            .unwrap_or("");
        let mut matches = catalog.entries.iter().filter(|entry| {
            entry
                .id
                .trim_end_matches(".desktop")
                .eq_ignore_ascii_case(suffix)
        });
        if let Some(entry) = matches.next()
            && matches.next().is_none()
        {
            return entry.id.clone();
        }
    }
    let class = if window.initial_class.is_empty() {
        &window.class
    } else {
        &window.initial_class
    };
    format!("window-group:{}", class.trim().to_ascii_lowercase())
}

fn target_windows<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    target_id: &str,
) -> Vec<&'a Client> {
    windows
        .clients
        .iter()
        .filter(|window| resolve_target(catalog, window) == target_id)
        .collect()
}

fn instances(clients: Vec<&Client>) -> Vec<WindowSummary> {
    clients
        .into_iter()
        .map(|window| WindowSummary {
            id: hyprland::window_id(&window.address),
            title: window.title.clone(),
            class: window.class.clone(),
            workspace_id: window.workspace.id.to_string(),
            workspace_name: window.workspace.name.clone(),
            focused: window.focus_rank == 0,
            focus_rank: window.focus_rank,
        })
        .collect()
}

fn summary_for_entry(
    entry: &CatalogEntry,
    clients: Vec<&Client>,
    revision: u64,
) -> ApplicationSummary {
    let instances = instances(clients);
    let focused = instances.iter().any(|window| window.focused);
    let best_rank = instances
        .iter()
        .map(|window| window.focus_rank)
        .min()
        .unwrap_or(i64::MAX);
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
        instances,
        desktop_actions: entry.actions.clone(),
        score: running_score(focused, best_rank),
    }
}

fn summary_for_unmatched(id: String, clients: Vec<&Client>, revision: u64) -> ApplicationSummary {
    let name = clients
        .first()
        .map(|window| {
            if window.class.is_empty() {
                "Untitled"
            } else {
                &window.class
            }
        })
        .unwrap_or("Untitled")
        .to_owned();
    let instances = instances(clients);
    let focused = instances.iter().any(|window| window.focused);
    let best_rank = instances
        .iter()
        .map(|window| window.focus_rank)
        .min()
        .unwrap_or(i64::MAX);
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
    catalog
        .by_id(target_id)
        .map(|entry| entry.name.clone())
        .unwrap_or_else(|| {
            target_id
                .strip_prefix("window-group:")
                .unwrap_or(target_id)
                .to_owned()
        })
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
    fn resolves_unique_reverse_dns_class_suffix() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("yazi.desktop"),
            "[Desktop Entry]\nType=Application\nName=Yazi\nExec=true\n",
        )
        .unwrap();
        let catalog = Catalog::from_paths(vec![directory.path().into()]);
        let window = Client {
            address: "0x1".into(),
            class: "com.laufan.yazi".into(),
            initial_class: "com.laufan.yazi".into(),
            title: "Yazi".into(),
            workspace: Workspace::default(),
            focus_rank: 0,
            mapped: true,
        };
        assert_eq!(resolve_target(&catalog, &window), "yazi.desktop");
    }
}
