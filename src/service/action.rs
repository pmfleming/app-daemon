use std::time::{Duration, Instant};

use anyhow::Context;

use serde::Deserialize;

use tokio::time;

use crate::{
    catalog::{Catalog, CatalogEntry},
    hyprland::{self, Client, Snapshot},
    launch::{self, LaunchReceipt},
    model::{OperationResult, PlacementStatus, WorkspacePlacement},
};

use super::identity::{resolve_target, resolve_target_with_cgroup, target_window};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApplicationAction {
    Activate,
    Launch,
    FocusWindow,
    Close,
    CloseWindow,
    MoveToWorkspace,
    DesktopAction,
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

impl ApplicationAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Launch => "launch",
            Self::FocusWindow => "focus-window",
            Self::Close => "close",
            Self::CloseWindow => "close-window",
            Self::MoveToWorkspace => "move-to-workspace",
            Self::DesktopAction => "desktop-action",
        }
    }
}

pub(super) struct ActionOutcome {
    pub(super) message: String,
    pub(super) launch: Option<LaunchReceipt>,
    pub(super) status: &'static str,
}

pub(super) fn operation_result(
    id: String,
    params: &ExecuteParams,
    status: &str,
    message: String,
    launch: Option<LaunchReceipt>,
) -> OperationResult {
    let (launch_backend, launch_scope, placement) = launch
        .map(|receipt| {
            (
                Some(receipt.backend),
                Some(receipt.scope),
                receipt.placement,
            )
        })
        .unwrap_or_default();
    OperationResult {
        id,
        action: params.action.as_str().into(),
        target_id: params.target_id.clone(),
        status: status.into(),
        message,
        launch_backend,
        launch_scope,
        placement,
    }
}

impl ActionOutcome {
    fn new(catalog: &Catalog, target_id: &str, verb: &str) -> Self {
        Self {
            message: format!("{verb} {}", display_name(catalog, target_id)),
            launch: None,
            status: "completed",
        }
    }
}

struct LaunchProgress<'a> {
    service: &'a super::ApplicationService,
    operation_id: &'a str,
}

impl LaunchProgress<'_> {
    async fn handed_off(&self, params: &ExecuteParams, receipt: &LaunchReceipt) {
        let progress = operation_result(
            self.operation_id.into(),
            params,
            "running",
            "Launch handed off; waiting for window placement".into(),
            Some(receipt.clone()),
        );
        // Owned status reads retain the receipt if a subscriber misses the event.
        if self
            .service
            .operations
            .lock()
            .await
            .running(progress.clone())
        {
            let _ = self.service.operation_changes.send(progress);
        }
    }
}

pub(super) async fn execute_action(
    catalog: &Catalog,
    params: &ExecuteParams,
    service: &super::ApplicationService,
    operation_id: &str,
) -> anyhow::Result<ActionOutcome> {
    // The caller holds the application's launch lock. Never use a cached window
    // baseline: a preceding launch or compositor event may not have reconciled yet.
    let windows = Snapshot::load().await;
    let progress = LaunchProgress {
        service,
        operation_id,
    };
    let target_id = &params.target_id;
    match params.action {
        ApplicationAction::Activate => activate(catalog, &windows, params, &progress).await,
        ApplicationAction::Launch | ApplicationAction::DesktopAction => {
            launch_on_workspace(catalog, &windows, params, &progress, false).await
        }
        ApplicationAction::FocusWindow => {
            hyprland::focus(target_address(catalog, &windows, params)?)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Focused"))
        }
        ApplicationAction::Close => close_application(catalog, &windows, target_id)
            .await
            .map(|()| ActionOutcome::new(catalog, target_id, "Close requested for")),
        ApplicationAction::CloseWindow => {
            hyprland::close(target_address(catalog, &windows, params)?)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Close requested for"))
        }
        ApplicationAction::MoveToWorkspace => move_to_workspace(catalog, &windows, params)
            .await
            .map(|()| ActionOutcome::new(catalog, target_id, "Moved")),
    }
}

async fn activate(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
    progress: &LaunchProgress<'_>,
) -> anyhow::Result<ActionOutcome> {
    let target_id = &params.target_id;
    if let Some(window) = target_window(catalog, windows, target_id) {
        hyprland::focus(&window.address).await?;
        return Ok(ActionOutcome::new(catalog, target_id, "Focused"));
    }
    launch_on_workspace(catalog, windows, params, progress, true).await
}

async fn launch_on_workspace(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
    progress: &LaunchProgress<'_>,
    focus: bool,
) -> anyhow::Result<ActionOutcome> {
    let existing = launch::Provenance::for_application(
        catalog,
        &params.target_id,
        windows
            .clients
            .iter()
            .filter(|window| resolve_target(catalog, window) == params.target_id)
            .map(|window| window.pid),
    );
    let mut launch = if params.action == ApplicationAction::DesktopAction {
        launch_action(catalog, params).await?
    } else {
        launch(catalog, &params.target_id).await?
    };
    launch.include_existing(existing);
    let launch_only = catalog
        .by_id(&params.target_id)
        .is_some_and(|entry| entry.launch_only);
    if !launch_only {
        launch.placement = params
            .workspace_id
            .as_ref()
            .map(|workspace| WorkspacePlacement {
                workspace_id: workspace.clone(),
                status: PlacementStatus::Pending,
                reason: None,
            });
    }
    progress.handed_off(params, &launch).await;
    let result = if launch_only || (!focus && launch.placement.is_none()) {
        Ok(true)
    } else {
        place_launched_window(catalog, windows, params, &mut launch, focus).await
    };
    Ok(ActionOutcome::launched(
        catalog,
        params,
        launch,
        result,
        windows.available,
    ))
}

impl ActionOutcome {
    fn launched(
        catalog: &Catalog,
        params: &ExecuteParams,
        mut launch: LaunchReceipt,
        result: anyhow::Result<bool>,
        snapshot_available: bool,
    ) -> Self {
        let mut outcome = Self::new(catalog, &params.target_id, "Launched");
        match result {
            Ok(true) => {}
            Ok(false) => {
                let reason = if snapshot_available {
                    "no unambiguous new window with verified launch ownership"
                } else {
                    "pre-launch compositor snapshot unavailable"
                };
                set_placement(
                    &mut launch,
                    PlacementStatus::Unavailable,
                    Some(reason.into()),
                );
                outcome.message.push_str(&format!(
                    "; placement/focus unavailable: {reason}. Do not relaunch automatically."
                ));
            }
            Err(error) => {
                // Preserve a successful launch receipt even if subsequent move,
                // verification or focus fails. Retrying must never replay the launch.
                match launch.placement.as_mut() {
                    Some(placement) if placement.status != PlacementStatus::Placed => {
                        placement.status = PlacementStatus::Failed;
                        placement.reason = Some(error.to_string());
                    }
                    _ => outcome.status = "failed", // No placement, or placement succeeded but focus failed.
                }
                outcome.message.push_str(&format!(
                    "; placement/focus failed: {error}. The app has already started."
                ));
            }
        }
        tracing::debug!(target_id = %params.target_id, backend = %launch.backend,
        unit = ?launch.unit, placement = ?launch.placement, "application launch outcome");
        outcome.launch = Some(launch);
        outcome
    }
}

fn set_placement(launch: &mut LaunchReceipt, status: PlacementStatus, reason: Option<String>) {
    if let Some(placement) = launch.placement.as_mut() {
        placement.status = status;
        placement.reason = reason;
    }
}

async fn place_launched_window(
    catalog: &Catalog,
    previous: &Snapshot,
    params: &ExecuteParams,
    launch: &mut LaunchReceipt,
    focus: bool,
) -> anyhow::Result<bool> {
    if !previous.available {
        return Ok(false);
    }
    let previous_addresses = previous
        .clients
        .iter()
        .map(|window| window.address.as_str())
        .collect::<Vec<_>>();
    let Some(window) =
        wait_for_new_window(catalog, &params.target_id, &previous_addresses, launch).await
    else {
        return Ok(false);
    };
    if let Some(workspace) = params.workspace_id.as_deref() {
        hyprland::move_to_workspace(&window.address, workspace).await?;
        verify_placement(&window, workspace, launch).await?;
        set_placement(launch, PlacementStatus::Placed, None);
    }
    if focus {
        hyprland::focus(&window.address).await?;
    }
    Ok(true)
}

async fn wait_for_new_window(
    catalog: &Catalog,
    target_id: &str,
    previous_addresses: &[&str],
    launch: &mut LaunchReceipt,
) -> Option<Client> {
    const WINDOW_TIMEOUT: Duration = Duration::from_secs(8);
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    loop {
        launch.observe_processes();
        let windows = Snapshot::load().await;
        if let Some(window) = correlated_window(windows, previous_addresses, |window| {
            launch.owns_process(window.pid)
                && (resolve_target(catalog, window) == target_id
                    // Chromium-based applications may adopt a generic Chromium
                    // scope. Verified launch provenance plus their own window
                    // identity remains valid even if that helper is installed.
                    || resolve_target_with_cgroup(catalog, window, None) == target_id)
        }) {
            return Some(window);
        }
        if Instant::now() >= deadline {
            return None;
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

async fn verify_placement(
    window: &Client,
    workspace: &str,
    launch: &LaunchReceipt,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = Snapshot::load().await;
        let current = snapshot.clients.iter().find(|current| {
            current.address == window.address
                && current.pid == window.pid
                && launch.owns_process(current.pid)
        });
        if current.is_some_and(|current| workspace_matches(current, workspace)) {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "workspace move was not confirmed by the compositor"
        );
        time::sleep(Duration::from_millis(100)).await;
    }
}

fn workspace_matches(window: &Client, workspace: &str) -> bool {
    if let Ok(id) = workspace.parse::<i64>() {
        window.workspace.id == id
    } else {
        window.workspace.name == workspace.strip_prefix("name:").unwrap_or(workspace)
    }
}

fn correlated_window(
    windows: Snapshot,
    previous_addresses: &[&str],
    belongs: impl Fn(&Client) -> bool,
) -> Option<Client> {
    let mut matches = windows
        .clients
        .into_iter()
        .filter(|window| !previous_addresses.contains(&window.address.as_str()) && belongs(window));
    let window = matches.next()?;
    // A multi-window launch is ambiguous too; leave all windows untouched.
    matches.next().is_none().then_some(window)
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

async fn close_application(
    catalog: &Catalog,
    windows: &Snapshot,
    target_id: &str,
) -> anyhow::Result<()> {
    let windows = windows
        .clients
        .iter()
        .filter(|window| resolve_target(catalog, window) == target_id)
        .collect::<Vec<_>>();
    anyhow::ensure!(!windows.is_empty(), "application is no longer running");
    for window in windows {
        hyprland::close(&window.address).await?;
    }
    Ok(())
}

fn target_address<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    params: &ExecuteParams,
) -> anyhow::Result<&'a str> {
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
    Ok(&window.address)
}

fn display_name<'a>(catalog: &'a Catalog, target_id: &'a str) -> &'a str {
    catalog.by_id(target_id).map_or_else(
        || target_id.strip_prefix("window-group:").unwrap_or(target_id),
        |entry| entry.name.as_str(),
    )
}

async fn launch(catalog: &Catalog, target_id: &str) -> anyhow::Result<LaunchReceipt> {
    let entry = catalog
        .by_id(target_id)
        .context("application is no longer available")?;
    if let Some(receipt) = activate_dbus_entry(entry, None).await? {
        return Ok(receipt);
    }
    if entry.requires_terminal() && launch::LaunchBackend::detect() != launch::LaunchBackend::Uwsm {
        return launch_in_terminal(
            target_id,
            entry.launch_command()?,
            entry.working_directory(),
        )
        .await;
    }
    launch::launch_desktop(target_id).await
}

async fn activate_dbus_entry(
    entry: &CatalogEntry,
    action: Option<&str>,
) -> anyhow::Result<Option<LaunchReceipt>> {
    if !entry.dbus_activatable() {
        return Ok(None);
    }
    match launch::activate_dbus(&entry.id, action).await {
        Ok(receipt) => Ok(Some(receipt)),
        Err(error) => {
            let fallback = action.map_or_else(
                || entry.launch_command(),
                |action| entry.parse_action(action),
            );
            if fallback.is_err() {
                return Err(error.context("activate desktop entry without an Exec fallback"));
            }
            tracing::debug!(target_id = %entry.id, %error, "D-Bus activation failed; using desktop Exec fallback");
            Ok(None)
        }
    }
}

async fn launch_in_terminal(
    target_id: &str,
    command: Vec<String>,
    directory: Option<&str>,
) -> anyhow::Result<LaunchReceipt> {
    let (program, command_arguments) = command
        .split_first()
        .context("desktop application command is empty")?;
    let mut arguments = Vec::with_capacity(command_arguments.len() + 2);
    arguments.extend(["--", program.as_str()]);
    arguments.extend(command_arguments.iter().map(String::as_str));
    launch::spawn_for_application(target_id, "xdg-terminal-exec", arguments, directory)
        .await
        .context("start application in the default terminal")
}

async fn launch_action(catalog: &Catalog, params: &ExecuteParams) -> anyhow::Result<LaunchReceipt> {
    let target_id = &params.target_id;
    let action_id = params
        .desktop_action_id
        .as_deref()
        .context("desktop_action_id is required")?;
    let entry = catalog
        .by_id(target_id)
        .context("application is no longer available")?;
    anyhow::ensure!(
        entry.actions.iter().any(|action| action.id == action_id),
        "desktop action is unavailable"
    );
    if let Some(receipt) = activate_dbus_entry(entry, Some(action_id)).await? {
        return Ok(receipt);
    }
    let args = entry.parse_action(action_id)?;
    if launch::LaunchBackend::detect() == launch::LaunchBackend::Uwsm {
        return launch::launch_desktop_action(target_id, action_id).await;
    }
    let (program, arguments) = args
        .split_first()
        .context("desktop action command is empty")?;
    if entry.requires_terminal() {
        return launch_in_terminal(target_id, args, entry.working_directory()).await;
    }
    launch::spawn_for_application(target_id, program, arguments, entry.working_directory())
        .await
        .context("start desktop action")
}
