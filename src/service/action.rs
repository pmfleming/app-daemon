use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;

use serde::{Deserialize, Serialize};

use tokio::time;

use crate::{
    catalog::Catalog,
    hyprland::{self, Client, Snapshot},
    launch::{self, LaunchReceipt},
    model::{OperationResult, PlacementStatus, WorkspacePlacement},
};

use super::identity::{resolve_target, target_window};
use crate::ownership::Snapshot as OwnershipSnapshot;

mod close;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
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

// Preparation borrows a consistent settings view before admission. No launcher
// or compositor effects run until this value has been registered as an operation.
pub(super) struct PreparedAction {
    catalog: Arc<Catalog>,
    params: ExecuteParams,
}

impl PreparedAction {
    pub(super) fn new(
        catalog: Arc<Catalog>,
        mut params: ExecuteParams,
        preferred_workspace: Option<&str>,
    ) -> Self {
        if matches!(
            params.action,
            ApplicationAction::Activate
                | ApplicationAction::Launch
                | ApplicationAction::DesktopAction
        ) && let Some(workspace) = preferred_workspace
        {
            params.workspace_id = Some(workspace.into());
        }
        Self { catalog, params }
    }

    pub(super) fn target_id(&self) -> &str {
        &self.params.target_id
    }

    pub(super) fn accepted(&self) -> OperationResult {
        operation_result(
            format!("operation-{}", uuid::Uuid::new_v4()),
            &self.params,
            "accepted",
            "Operation accepted".into(),
            None,
        )
    }

    pub(super) async fn execute(
        &self,
        service: &super::ApplicationService,
        id: &str,
    ) -> anyhow::Result<ActionOutcome> {
        execute_action(&self.catalog, &self.params, service, id).await
    }
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
    message: String,
    launch: Option<LaunchReceipt>,
    close: Option<crate::model::CloseObservation>,
    status: &'static str,
}

pub(super) fn operation_result(
    id: String,
    params: &ExecuteParams,
    status: &str,
    message: String,
    launch: Option<&LaunchReceipt>,
) -> OperationResult {
    let (launch_backend, launch_scope, placement) = launch
        .map(|receipt| {
            (
                Some(receipt.backend.clone()),
                Some(receipt.scope.clone()),
                receipt.placement.clone(),
            )
        })
        .unwrap_or_default();
    OperationResult {
        close: None,
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
    pub(super) fn apply(self, result: &mut OperationResult) {
        result.status = self.status.into();
        result.message = self.message;
        result.close = self.close;
        if let Some(receipt) = self.launch {
            result.launch_backend = Some(receipt.backend);
            result.launch_scope = Some(receipt.scope);
            result.placement = receipt.placement;
        }
    }

    fn new(catalog: &Catalog, target_id: &str, verb: &str) -> Self {
        Self {
            message: format!("{verb} {}", display_name(catalog, target_id)),
            launch: None,
            close: None,
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
        self.service
            .remember_launch(&params.target_id, receipt)
            .await;
        let progress = operation_result(
            self.operation_id.into(),
            params,
            "running",
            "Launch handed off; waiting for window placement".into(),
            Some(receipt),
        );
        // Owned status reads retain the receipt if a subscriber misses the event.
        self.service.publish_running(progress).await;
    }
}

impl super::ApplicationService {
    async fn remember_launch(&self, target: &str, receipt: &LaunchReceipt) {
        let refresh = std::sync::Arc::clone(&self.ownership_refresh)
            .lock_owned()
            .await;
        self.ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remember(target, receipt.ownership_roots());
        self.update_ownership(false, refresh).await;
    }
}

async fn execute_action(
    catalog: &Catalog,
    params: &ExecuteParams,
    service: &super::ApplicationService,
    operation_id: &str,
) -> anyhow::Result<ActionOutcome> {
    // The caller holds the application's launch lock. Never use a cached window
    // baseline: a preceding launch or compositor event may not have reconciled yet.
    service.refresh_ownership(false).await;
    let ownership = service.ownership_snapshot();
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
            hyprland::focus(&selected_window(catalog, &windows, params, &ownership)?.address)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Focused"))
        }
        ApplicationAction::Close | ApplicationAction::CloseWindow => {
            close_application(catalog, &windows, params, &ownership, &progress).await
        }
        ApplicationAction::MoveToWorkspace => {
            move_to_workspace(catalog, &windows, params, &ownership)
                .await
                .map(|()| ActionOutcome::new(catalog, target_id, "Moved"))
        }
    }
}

async fn activate(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
    progress: &LaunchProgress<'_>,
) -> anyhow::Result<ActionOutcome> {
    let target_id = &params.target_id;
    if let Some(window) = target_window(
        catalog,
        windows,
        target_id,
        &progress.service.ownership_snapshot(),
    ) {
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
    let ownership = progress.service.ownership_snapshot();
    let existing = launch::Provenance::for_application(
        catalog,
        &params.target_id,
        windows
            .clients
            .iter()
            .filter(|window| resolve_target(catalog, window, &ownership) == params.target_id)
            .map(|window| window.pid),
    );
    let action = (params.action == ApplicationAction::DesktopAction)
        .then(|| {
            params
                .desktop_action_id
                .as_deref()
                .context("desktop_action_id is required")
        })
        .transpose()?;
    let entry = catalog
        .by_id(&params.target_id)
        .context("application is no longer available")?;
    let mut launch = launch::launch_entry(entry, action).await?;
    launch.include_existing(existing);
    let launch_only = entry.launch_only;
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
        place_launched_window(
            catalog,
            windows,
            params,
            &mut launch,
            focus,
            progress.service,
        )
        .await
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
    service: &super::ApplicationService,
) -> anyhow::Result<bool> {
    if !previous.available {
        return Ok(false);
    }
    let previous_addresses = previous
        .clients
        .iter()
        .map(|window| window.address.as_str())
        .collect::<Vec<_>>();
    let Some(window) = wait_for_new_window(
        catalog,
        &params.target_id,
        &previous_addresses,
        launch,
        service,
    )
    .await
    else {
        return Ok(false);
    };
    if let Some(workspace) = params.workspace_id.as_deref() {
        hyprland::move_to_workspace(&window.address, workspace).await?;
        verify_placement(&window, workspace, |current| {
            launch.owns_process(current.pid)
        })
        .await?;
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
    service: &super::ApplicationService,
) -> Option<Client> {
    const WINDOW_TIMEOUT: Duration = Duration::from_secs(8);
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    loop {
        launch.observe_processes();
        service.remember_launch(target_id, launch).await;
        let ownership = service.ownership_snapshot();
        let windows = Snapshot::load().await;
        if let Some(window) = correlated_window(windows, previous_addresses, |window| {
            launch.owns_process(window.pid)
                && resolve_target(catalog, window, &ownership) == target_id
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
    belongs: impl Fn(&Client) -> bool,
) -> anyhow::Result<()> {
    time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = Snapshot::load().await;
            if snapshot.available
                && snapshot.clients.iter().any(|current| {
                    current.address == window.address
                        && current.pid == window.pid
                        && belongs(current)
                        && workspace_matches(current, workspace)
                })
            {
                return;
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("workspace move was not confirmed by the compositor")
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
    ownership: &OwnershipSnapshot,
) -> anyhow::Result<()> {
    let window = selected_window(catalog, windows, params, ownership)?;
    let workspace = params
        .workspace_id
        .as_deref()
        .context("workspace_id is required")?;
    hyprland::move_to_workspace(&window.address, workspace).await?;
    verify_placement(window, workspace, |current| {
        resolve_target(catalog, current, ownership) == params.target_id
    })
    .await
}

async fn close_application(
    catalog: &Catalog,
    windows: &Snapshot,
    params: &ExecuteParams,
    ownership: &OwnershipSnapshot,
    progress: &LaunchProgress<'_>,
) -> anyhow::Result<ActionOutcome> {
    anyhow::ensure!(
        windows.available,
        "compositor window observation is unavailable"
    );
    let address = if params.action == ApplicationAction::CloseWindow {
        Some(&selected_window(catalog, windows, params, ownership)?.address)
    } else {
        None
    };
    let targets = windows
        .clients
        .iter()
        .filter(|window| {
            address.map_or_else(
                || resolve_target(catalog, window, ownership) == params.target_id,
                |address| &window.address == address,
            )
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(!targets.is_empty(), "application is no longer running");
    let close = close::observe(
        &close::Compositor,
        &targets,
        |close| async move {
            let mut result = operation_result(
                progress.operation_id.into(),
                params,
                "running",
                "Close requested; observing targeted windows".into(),
                None,
            );
            result.close = Some(close);
            progress.service.publish_running(result).await;
        },
        Duration::from_secs(5),
    )
    .await;
    let mut outcome = ActionOutcome::new(catalog, &params.target_id, "Close requested for");
    outcome.message = match close.status.as_str() {
        "closed" => "Requested windows closed".into(),
        "still-open" => format!(
            "Close requested; {} window(s) still open. Focus a window to check for a save prompt.",
            close.remaining_window_ids.len()
        ),
        _ => "Close outcome unknown; check windows before trying again".into(),
    };
    if let Some(error) = &close.dispatch_error {
        outcome.status = "failed";
        outcome.message.push_str(&format!(
            ". Some close requests could not be dispatched: {error}"
        ));
    }
    outcome.close = Some(close);
    Ok(outcome)
}

fn selected_window<'a>(
    catalog: &Catalog,
    windows: &'a Snapshot,
    params: &ExecuteParams,
    ownership: &OwnershipSnapshot,
) -> anyhow::Result<&'a Client> {
    let id = params
        .window_id
        .as_deref()
        .context("window_id is required")?;
    let window = windows
        .by_window_id(id)
        .context("window is no longer available")?;
    anyhow::ensure!(
        resolve_target(catalog, window, ownership) == params.target_id,
        "window no longer belongs to the selected application"
    );
    Ok(window)
}

fn display_name<'a>(catalog: &'a Catalog, target_id: &'a str) -> &'a str {
    catalog.by_id(target_id).map_or_else(
        || target_id.strip_prefix("window-group:").unwrap_or(target_id),
        |entry| entry.name.as_str(),
    )
}
